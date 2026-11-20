// -*- mode: Bluespec; -*-
/**
 * The ec <-> ecd unix-socket protocol, one request at a time per connection
 * (jigd/main.go serve()). Byte formats are in behaviors/ecd-protocol.md; this module pins the
 * request/response discipline:
 *
 *  - requests on a connection are answered strictly in order, one answer per request, so a
 *    client may pipeline (jig/src/cache_client.cc GetMany/PutMany send everything first);
 *  - every malformed request, and every failure that leaves the byte stream position unknown,
 *    ends the connection without an answer; nothing is answered after that;
 *  - a PUT that was answered OK is visible to every GET/HAS that starts afterwards on any
 *    connection (the store is linearizable; eviction may later drop it, modelled by `evict`);
 *  - a connection blocked in SLOT answers nothing else until the token comes;
 *  - tokens die with the connection (detailed in slots.qnt).
 *
 * Abstraction: a reply is appended atomically. The real server can write part of a reply (an
 * IDS line, a GET header plus part of the body) and then hang up on a write error; the client
 * sees a short read and treats the connection as failed (behaviors/ecd-protocol.md).
 *
 * Run:
 *   quint run --invariant=safety --max-steps=40 --max-samples=5000 spec/ecd_protocol.qnt
 *   quint test spec/ecd_protocol.qnt
 */
module ecd_protocol {
  pure val CONNS = Set(1, 2)
  pure val KEYS = Set("k1", "k2")
  pure val VALS = Set(1, 2, 3)
  pure val BUILDS = Set("A", "B")
  pure val LIMIT = 1
  pure val MAX_IDS = 3          // stands for 1<<20

  // A request as the client sends it. Malformed variants stand for the byte-level failures:
  type Req =
    | Get(str)
    | Has(str)
    | Put((str, int))             // well-formed PUT key <len> + exactly len body bytes
    | PutBadLen(str)             // <len> not a base-10 int64            -> hang up
    | PutBadKey                  // store.Put rejects: len > 2 GiB, < 0, key too long -> hang up
    | PutShortBody(str)          // peer closes before len bytes arrive  -> hang up, nothing stored
    | Ids(int)                   // IDS n with 0 <= n <= 1<<20, then n path lines
    | IdsBadCount                // n not an int, < 0 or > 1<<20         -> hang up
    | Slot(str)
    | Done(str)
    | Stats
    | Unknown                    // unknown verb or wrong field count     -> hang up
    | TooLong                    // a line of 64 KiB or more without '\n' -> hang up

  type Resp =
    | ROk                        // "OK\n"  (PUT, SLOT, DONE)
    | RVal(int)                  // "OK <len>\n" + bytes
    | RMiss                      // "MISS\n"
    | RHas(bool)                 // "1\n" / "0\n"
    | RIds(int)                  // n lines, one identity ("" if unanswerable) each
    | RStats

  type CState = | Reading | InSlot(str) | Gone

  var inbox: int -> List[Req]          // pipelined, not yet read
  var sent: int -> List[Req]           // everything the client sent, in order
  var answered: int -> List[Resp]      // everything the server answered, in order
  var handled: int -> List[Req]        // requests the server finished (answered), in order
  var cstate: int -> CState
  var kv: str -> int                   // the store's current value per key
  var acked: str -> Set[int]           // values whose PUT was answered OK (ghost)
  var tokens: int -> (str -> int)      // per-connection held tokens
  var out: int

  pure def cnt(m: str -> int, b: str): int = if (m.keys().contains(b)) m.get(b) else 0
  pure def totalTokens(t: int -> (str -> int)): int =
    CONNS.fold(0, (acc, c) => acc + t.get(c).keys().fold(0, (a, b) => a + t.get(c).get(b)))

  // The answer prescribed for a request that was handled (None-like: well-formed only)
  pure def answerFor(r: Req, value: int, present: bool): Resp =
    match r {
      | Get(_k) => if (present) RVal(value) else RMiss
      | Has(_k) => RHas(present)
      | Put(_p) => ROk
      | Ids(n) => RIds(n)
      | Slot(_b) => ROk
      | Done(_b) => ROk
      | Stats => RStats
      | _ => RStats   // unreachable: malformed requests are never answered
    }

  pure def malformed(r: Req): bool =
    match r {
      | PutBadLen(_k) => true
      | PutBadKey => true
      | PutShortBody(_k) => true
      | IdsBadCount => true
      | Unknown => true
      | TooLong => true
      | _ => false
    }

  action init = all {
    inbox' = CONNS.mapBy(_c => List()),
    sent' = CONNS.mapBy(_c => List()),
    answered' = CONNS.mapBy(_c => List()),
    handled' = CONNS.mapBy(_c => List()),
    cstate' = CONNS.mapBy(_c => Reading),
    kv' = Map(),
    acked' = KEYS.mapBy(_k => Set()),
    tokens' = CONNS.mapBy(_c => Map()),
    out' = 0,
  }

  // the client writes one more request (pipelining: it need not wait for answers)
  action send(c: int, r: Req): bool = all {
    cstate.get(c) != Gone,
    inbox' = inbox.setBy(c, q => q.append(r)),
    sent' = sent.setBy(c, q => q.append(r)),
    answered' = answered, handled' = handled, cstate' = cstate, kv' = kv, acked' = acked,
    tokens' = tokens, out' = out,
  }

  // hang up: the deferred cleanup returns every token of this connection
  action hangUp(c: int): bool = all {
    cstate' = cstate.set(c, Gone),
    tokens' = tokens.set(c, Map()),
    out' = out - tokens.get(c).keys().fold(0, (a, b) => a + tokens.get(c).get(b)),
    inbox' = inbox.set(c, List()),
    sent' = sent, answered' = answered, handled' = handled, kv' = kv, acked' = acked,
  }

  // serve() reads the next request and answers it (or hangs up)
  action serveOne(c: int): bool = all {
    cstate.get(c) == Reading,
    inbox.get(c).length() > 0,
    handle(c, inbox.get(c)[0]),
  }

  action handle(c: int, r: Req): bool = {
    val rest = inbox.get(c).slice(1, inbox.get(c).length())
    if (malformed(r)) hangUp(c)
    else match r {
      | Slot(b) =>
        // Slots.Acquire: immediate grant only if a token is free AND nobody is queued
        if (out < LIMIT and CONNS.forall(d => blockedOn(cstate.get(d)) == "")) all {
          out' = out + 1,
          tokens' = tokens.setBy(c, m => m.put(b, cnt(m, b) + 1)),
          answered' = answered.setBy(c, q => q.append(ROk)),
          handled' = handled.setBy(c, q => q.append(r)),
          inbox' = inbox.set(c, rest),
          cstate' = cstate, kv' = kv, acked' = acked, sent' = sent,
        } else all {
          // blocked in Acquire: reads nothing more until granted (a second SLOT on the
          // connection that holds the only token waits forever: client rule EKACACHE_SLOT)
          cstate' = cstate.set(c, InSlot(b)),
          inbox' = inbox.set(c, rest),
          out' = out, tokens' = tokens, answered' = answered, handled' = handled,
          kv' = kv, acked' = acked, sent' = sent,
        }
      | Done(b) => all {
          // quirk: DONE for a build this connection holds no token for still answers OK
          out' = if (cnt(tokens.get(c), b) > 0) out - 1 else out,
          tokens' = if (cnt(tokens.get(c), b) > 0) tokens.setBy(c, m => m.put(b, cnt(m, b) - 1)) else tokens,
          answered' = answered.setBy(c, q => q.append(ROk)),
          handled' = handled.setBy(c, q => q.append(r)),
          inbox' = inbox.set(c, rest),
          cstate' = cstate, kv' = kv, acked' = acked, sent' = sent,
        }
      | Put(p) => all {
          kv' = kv.put(p._1, p._2),
          acked' = acked.setBy(p._1, s => s.union(Set(p._2))),
          answered' = answered.setBy(c, q => q.append(ROk)),
          handled' = handled.setBy(c, q => q.append(r)),
          inbox' = inbox.set(c, rest),
          cstate' = cstate, out' = out, tokens' = tokens, sent' = sent,
        }
      | Get(k) => all {
          answered' = answered.setBy(c, q => q.append(answerFor(r, if (kv.keys().contains(k)) kv.get(k) else 0, kv.keys().contains(k)))),
          handled' = handled.setBy(c, q => q.append(r)),
          inbox' = inbox.set(c, rest),
          cstate' = cstate, kv' = kv, acked' = acked, out' = out, tokens' = tokens, sent' = sent,
        }
      | Has(k) => all {
          answered' = answered.setBy(c, q => q.append(RHas(kv.keys().contains(k)))),
          handled' = handled.setBy(c, q => q.append(r)),
          inbox' = inbox.set(c, rest),
          cstate' = cstate, kv' = kv, acked' = acked, out' = out, tokens' = tokens, sent' = sent,
        }
      | _ => all {
          answered' = answered.setBy(c, q => q.append(answerFor(r, 0, false))),
          handled' = handled.setBy(c, q => q.append(r)),
          inbox' = inbox.set(c, rest),
          cstate' = cstate, kv' = kv, acked' = acked, out' = out, tokens' = tokens, sent' = sent,
        }
    }
  }

  // a token came back elsewhere: the blocked SLOT is granted and answered. Which waiter goes
  // first (fewest-held build, FIFO among equals) is left nondeterministic here; slots.qnt pins it.
  pure def blockedOn(s: CState): str = match s { | InSlot(b) => b | _ => "" }

  action granted(c: int): bool = {
    val b = blockedOn(cstate.get(c))
    all {
      out < LIMIT,
      b != "",
      out' = out + 1,
      tokens' = tokens.setBy(c, m => m.put(b, cnt(m, b) + 1)),
      answered' = answered.setBy(c, q => q.append(ROk)),
      handled' = handled.setBy(c, q => q.append(Slot(b))),
      cstate' = cstate.set(c, Reading),
      inbox' = inbox, sent' = sent, kv' = kv, acked' = acked,
    }
  }

  // the peer closes (or the daemon hangs up on a write error)
  action disconnect(c: int): bool = all {
    cstate.get(c) != Gone,
    hangUp(c),
  }

  // eviction may drop any key (store.qnt has the real policy)
  action evict(k: str): bool = all {
    kv.keys().contains(k),
    kv' = kv.keys().exclude(Set(k)).mapBy(x => kv.get(x)),
    inbox' = inbox, sent' = sent, answered' = answered, handled' = handled, cstate' = cstate,
    acked' = acked, tokens' = tokens, out' = out,
  }

  action step = {
    nondet c = CONNS.oneOf()
    nondet k = KEYS.oneOf()
    nondet v = VALS.oneOf()
    nondet b = BUILDS.oneOf()
    nondet n = range(0, MAX_IDS + 1).foldl(Set(), (s, i) => s.union(Set(i))).oneOf()
    nondet r = Set(Get(k), Has(k), Put((k, v)), PutBadLen(k), PutBadKey, PutShortBody(k), Ids(n),
                   IdsBadCount, Slot(b), Done(b), Stats, Unknown, TooLong).oneOf()
    any { send(c, r), serveOne(c), granted(c), disconnect(c), evict(k) }
  }

  // ---------------------------------------------------------------------------------------
  // Invariants
  // ---------------------------------------------------------------------------------------
  // one answer per handled request, and handled requests are a prefix of what was sent
  // (in order, none skipped except by hanging up)
  val inOrder = CONNS.forall(c => and {
    answered.get(c).length() == handled.get(c).length(),
    handled.get(c).length() <= sent.get(c).length(),
    range(0, handled.get(c).length()).foldl(true, (ok, i) => ok and handled.get(c)[i] == sent.get(c)[i]),
  })
  // each answer has the shape its request demands (IDS n -> exactly n lines, ...)
  val answerShapes = CONNS.forall(c => range(0, handled.get(c).length()).foldl(true, (ok, i) => ok and
    match handled.get(c)[i] {
      | Get(_k) => match answered.get(c)[i] { | RVal(_v) => true | RMiss => true | _ => false }
      | Has(_k) => match answered.get(c)[i] { | RHas(_x) => true | _ => false }
      | Put(_p) => answered.get(c)[i] == ROk
      | Ids(n) => answered.get(c)[i] == RIds(n)
      | Slot(_b) => answered.get(c)[i] == ROk
      | Done(_b) => answered.get(c)[i] == ROk
      | Stats => answered.get(c)[i] == RStats
      | _ => false
    }))
  // a malformed request is never answered
  val malformedNeverAnswered = CONNS.forall(c => handled.get(c).foldl(true, (ok, r) => ok and not(malformed(r))))
  // a GET hit only returns a value some PUT for that key was acknowledged with
  val hitsWereAcked = CONNS.forall(c => range(0, handled.get(c).length()).foldl(true, (ok, i) => ok and
    match handled.get(c)[i] {
      | Get(k) => match answered.get(c)[i] { | RVal(v) => acked.get(k).contains(v) | _ => true }
      | _ => true
    }))
  // a gone connection holds no tokens; tokens add up
  val tokensAccounted = and {
    out == totalTokens(tokens),
    0 <= out, out <= LIMIT,
    CONNS.forall(c => cstate.get(c) == Gone implies tokens.get(c).keys().forall(b => tokens.get(c).get(b) == 0)),
  }

  val safety = and { inOrder, answerShapes, malformedNeverAnswered, hitsWereAcked, tokensAccounted }

  // ---------------------------------------------------------------------------------------
  // Scenario tests
  // ---------------------------------------------------------------------------------------
  action stutter = all {
    inbox' = inbox, sent' = sent, answered' = answered, handled' = handled, cstate' = cstate,
    kv' = kv, acked' = acked, tokens' = tokens, out' = out,
  }

  // serve_test.go TestServeIdsAndPipelinedGets: PUT, IDS, GET miss, GET hit sent back to back, answered in order
  run pipelinedTest =
    init
      .then(send(1, Put(("k1", 3))))
      .then(send(1, Ids(2)))
      .then(send(1, Get("k2")))
      .then(send(1, Get("k1")))
      .then(serveOne(1)).then(serveOne(1)).then(serveOne(1)).then(serveOne(1))
      .then(all {
        assert(answered.get(1) == [ROk, RIds(2), RMiss, RVal(3)]),
        assert(safety),
        stutter,
      })

  // a malformed line ends the connection; requests pipelined behind it are never answered
  run hangUpOnGarbageTest =
    init
      .then(send(1, Unknown))
      .then(send(1, Get("k1")))
      .then(serveOne(1))
      .then(all { assert(cstate.get(1) == Gone), assert(answered.get(1) == []), serveOne(1).fail() })

  // serve_test.go: a hang-up frees the slot for the next client
  run hangUpFreesSlotTest =
    init
      .then(send(1, Slot("A")))
      .then(serveOne(1))
      .then(send(2, Slot("B")))
      .then(serveOne(2))
      .then(all { assert(cstate.get(2) == InSlot("B")), disconnect(1) })
      .then(granted(2))
      .then(all { assert(answered.get(2) == [ROk]), assert(safety), stutter })

  // a PUT whose body is cut short stores nothing and gets no answer
  run shortBodyTest =
    init
      .then(send(1, PutShortBody("k1")))
      .then(serveOne(1))
      .then(send(2, Get("k1")))
      .then(serveOne(2))
      .then(all { assert(answered.get(2) == [RMiss]), assert(cstate.get(1) == Gone), stutter })
}
