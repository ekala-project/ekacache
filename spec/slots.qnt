// -*- mode: Bluespec; -*-
/**
 * ecd host-wide compiler slot admission.
 *
 * Sources: jigd/slots.go (Slots.Acquire/Release/grant) and the SLOT/DONE/hang-up handling
 * in jigd/main.go serve(), plus the client rule in jig/src/process.cc (Slot RAII + EKACACHE_SLOT).
 *
 * Module `slots` models the daemon: LIMIT tokens shared by all connections, a FIFO wait
 * queue served "fewest tokens held by the waiter's build first, FIFO among equals", and the
 * per-connection token bookkeeping that returns every token when the connection goes away.
 *
 * Module `slotNesting` models why an ec process that holds a token exports EKACACHE_SLOT=1 and
 * never asks for a second one: a nested request would wait on a token its own ancestor holds.
 * (Its queue is plain FIFO, a simplification: with LIMIT = 1 the order cannot matter.)
 *
 * Run:
 *   quint run --main=slots --invariant=safety --max-steps=60 --max-samples=5000 spec/slots.qnt
 *   quint run --main=slotNesting --invariant=noNestedWait --max-steps=20 spec/slots.qnt
 *   quint test --main=slots spec/slots.qnt
 */
module slots {
  // ---------------------------------------------------------------------------------------
  // Parameters (small, for simulation). EKACACHED_SLOTS sets LIMIT, default runtime.NumCPU().
  // ---------------------------------------------------------------------------------------
  pure val LIMIT = 2
  pure val CONNS = Set(1, 2, 3, 4)
  pure val BUILDS = Set("A", "B", "C")
  // Adopted change Q-SLOT-1 (signed off 2026-11-17): a waiter whose peer hangs up is removed
  // from the queue at once. false = the Go daemon (the waiter is still granted, then drained).
  pure val CANCEL_ABANDONED = true

  // What serve() is doing for a connection.
  type ConnState =
    | Idle              // reading the next request line
    | Blocked(str)      // inside Slots.Acquire(build): queued (Go: cannot notice the peer leaving)
    | Draining          // peer gone: the deferred loop is calling Release once per held token
    | Closed            // goroutine returned, holds nothing

  type Waiter = { conn: int, build: str }

  // --- Slots (slots.go) ---
  var out: int                  // tokens handed out
  var held: str -> int          // tokens per build; a key is deleted when its count drops to <= 0
  var waiting: List[Waiter]     // arrival order
  // --- serve() (main.go) ---
  var conn: int -> ConnState
  var peerUp: int -> bool       // false once the client closed its end
  var connHeld: int -> (str -> int)  // serve()'s `held` map: tokens this connection owns
  // --- ghost: the last grant from the wait queue, to check the service order ---
  var lastGrant: { valid: bool, index: int, queue: List[Waiter], heldBefore: str -> int }

  // ---------------------------------------------------------------------------------------
  // Helpers
  // ---------------------------------------------------------------------------------------
  pure def countOf(m: str -> int, b: str): int = if (m.keys().contains(b)) m.get(b) else 0
  pure def inc(m: str -> int, b: str): str -> int = m.put(b, countOf(m, b) + 1)
  pure def without(m: str -> int, b: str): str -> int = m.keys().exclude(Set(b)).mapBy(k => m.get(k))
  // Release's bookkeeping: decrement, delete the key at <= 0
  pure def dec(m: str -> int, b: str): str -> int =
    if (countOf(m, b) - 1 <= 0) without(m, b) else m.put(b, countOf(m, b) - 1)
  pure def total(m: str -> int): int = m.keys().fold(0, (acc, k) => acc + m.get(k))
  pure def removeAt(l: List[Waiter], i: int): List[Waiter] = l.slice(0, i).concat(l.slice(i + 1, l.length()))

  // grant(): the waiter whose build holds the fewest tokens; strict `<` keeps the earliest
  // among equals (FIFO). Mirrors the Go loop exactly: scan left to right, replace on `<`.
  pure def bestIndex(w: List[Waiter], h: str -> int): int =
    range(0, w.length()).foldl(0, (best, i) => if (countOf(h, w[i].build) < countOf(h, w[best].build)) i else best)

  // The declarative service rule bestIndex must implement.
  pure def isFairChoice(w: List[Waiter], h: str -> int, i: int): bool = and {
    0 <= i, i < w.length(),
    range(0, w.length()).foldl(true, (ok, j) => ok and countOf(h, w[i].build) <= countOf(h, w[j].build)),
    range(0, i).foldl(true, (ok, j) => ok and countOf(h, w[j].build) > countOf(h, w[i].build)),
  }

  type SlotState = { out: int, held: str -> int, waiting: List[Waiter], woken: List[Waiter],
                     g: { valid: bool, index: int, queue: List[Waiter], heldBefore: str -> int } }

  // One iteration of grant()'s loop (a no-op when it would not run).
  pure def grantOne(s: SlotState): SlotState =
    if (s.out < LIMIT and s.waiting.length() > 0) {
      val i = bestIndex(s.waiting, s.held)
      val w = s.waiting[i]
      { out: s.out + 1, held: inc(s.held, w.build), waiting: removeAt(s.waiting, i),
        woken: s.woken.append(w),
        g: { valid: true, index: i, queue: s.waiting, heldBefore: s.held } }
    } else s

  // grant() runs at most LIMIT iterations (each one takes a token).
  pure def grantAll(s: SlotState): SlotState = range(0, LIMIT).foldl(s, (st, _i) => grantOne(st))

  // Release(build) = out--, held[build]-- (delete at <= 0), grant()
  pure def release(s: SlotState, b: str): SlotState =
    grantAll(s.with("out", s.out - 1).with("held", dec(s.held, b)).with("woken", List()))

  // A woken waiter's goroutine returns from Acquire: held[build]++ in serve(), reply OK.
  // A dead peer makes the reply's Flush fail, so serve() returns and drains.
  pure def wake(cs: int -> ConnState, ch: int -> (str -> int), up: int -> bool, woken: List[Waiter])
      : (int -> ConnState, int -> (str -> int)) =
    woken.foldl((cs, ch), (acc, w) =>
      (acc._1.set(w.conn, if (up.get(w.conn)) Idle else Draining),
       acc._2.set(w.conn, inc(acc._2.get(w.conn), w.build))))

  // ---------------------------------------------------------------------------------------
  // Actions
  // ---------------------------------------------------------------------------------------
  action init = all {
    out' = 0,
    held' = Map(),
    waiting' = List(),
    conn' = CONNS.mapBy(_c => Idle),
    peerUp' = CONNS.mapBy(_c => true),
    connHeld' = CONNS.mapBy(_c => Map()),
    lastGrant' = { valid: false, index: 0, queue: List(), heldBefore: Map() },
  }

  // `SLOT <build>`: immediate grant iff a token is free AND nobody is queued, else enqueue.
  action requestSlot(c: int, b: str): bool = all {
    conn.get(c) == Idle,
    peerUp.get(c),
    if (out < LIMIT and waiting.length() == 0) all {
      out' = out + 1,
      held' = inc(held, b),
      waiting' = waiting,
      conn' = conn,
      connHeld' = connHeld.set(c, inc(connHeld.get(c), b)),
      lastGrant' = lastGrant.with("valid", false),
    } else all {
      out' = out,
      held' = held,
      waiting' = waiting.append({ conn: c, build: b }),
      conn' = conn.set(c, Blocked(b)),
      connHeld' = connHeld,
      lastGrant' = lastGrant,
    },
    peerUp' = peerUp,
  }

  // `DONE <build>`: release only if this connection holds a token for build; reply OK either way.
  action done(c: int, b: str): bool = all {
    conn.get(c) == Idle,
    peerUp.get(c),
    if (countOf(connHeld.get(c), b) > 0) {
      val s = release({ out: out, held: held, waiting: waiting, woken: List(), g: lastGrant }, b)
      val woke = wake(conn, connHeld.set(c, dec(connHeld.get(c), b)), peerUp, s.woken)
      all {
        out' = s.out, held' = s.held, waiting' = s.waiting, lastGrant' = s.g,
        conn' = woke._1, connHeld' = woke._2,
      }
    } else all {
      // quirk: an unheld DONE is answered OK and changes nothing
      out' = out, held' = held, waiting' = waiting, conn' = conn, connHeld' = connHeld,
      lastGrant' = lastGrant,
    },
    peerUp' = peerUp,
  }

  // The client closes its end (process exit, kill -9, ...).
  action peerCloses(c: int): bool = all {
    peerUp.get(c),
    peerUp' = peerUp.set(c, false),
    // an Idle reader sees EOF and starts draining. A Blocked one: Go keeps it queued; with
    // CANCEL_ABANDONED it is dequeued and drains whatever earlier SLOTs it still holds.
    val blocked = match conn.get(c) { | Blocked(_b) => true | _ => false }
    all {
      conn' = if (conn.get(c) == Idle or (blocked and CANCEL_ABANDONED)) conn.set(c, Draining) else conn,
      waiting' = if (blocked and CANCEL_ABANDONED) waiting.select(w => w.conn != c) else waiting,
      out' = out, held' = held, connHeld' = connHeld, lastGrant' = lastGrant,
    },
  }

  // serve()'s deferred loop: one Release per held token (Go map order: any build first).
  action drainOne(c: int): bool = all {
    conn.get(c) == Draining,
    if (connHeld.get(c).keys().size() == 0) all {
      conn' = conn.set(c, Closed),
      out' = out, held' = held, waiting' = waiting, connHeld' = connHeld, lastGrant' = lastGrant,
    } else {
      nondet b = connHeld.get(c).keys().oneOf()
      val s = release({ out: out, held: held, waiting: waiting, woken: List(), g: lastGrant }, b)
      val woke = wake(conn, connHeld.set(c, dec(connHeld.get(c), b)), peerUp, s.woken)
      all {
        out' = s.out, held' = s.held, waiting' = s.waiting, lastGrant' = s.g,
        conn' = woke._1, connHeld' = woke._2,
      }
    },
    peerUp' = peerUp,
  }

  // A new client reuses a closed slot in the model (fresh connection, nothing held).
  action reconnect(c: int): bool = all {
    conn.get(c) == Closed,
    conn' = conn.set(c, Idle),
    peerUp' = peerUp.set(c, true),
    connHeld' = connHeld.set(c, Map()),
    out' = out, held' = held, waiting' = waiting, lastGrant' = lastGrant,
  }

  action step = {
    nondet c = CONNS.oneOf()
    nondet b = BUILDS.oneOf()
    any { requestSlot(c, b), done(c, b), peerCloses(c), drainOne(c), reconnect(c) }
  }

  // ---------------------------------------------------------------------------------------
  // Invariants
  // ---------------------------------------------------------------------------------------
  val withinLimit = 0 <= out and out <= LIMIT
  // every handed-out token is counted once per build ...
  val outMatchesBuilds = out == total(held)
  // ... and once per owning connection, including connections that are draining
  val outMatchesConnections = out == CONNS.fold(0, (acc, c) => acc + total(connHeld.get(c)))
  // per build, the connections' tokens add up to the daemon's count
  val buildsMatchConnections = BUILDS.forall(b => countOf(held, b) == CONNS.fold(0, (acc, c) => acc + countOf(connHeld.get(c), b)))
  // no zero or negative entries survive Release
  val heldPositive = held.keys().forall(b => held.get(b) > 0)
  // nobody waits while a token is free: grant() drains the queue whenever out < LIMIT
  val waitOnlyWhenFull = waiting.length() > 0 implies out == LIMIT
  // a connection is Blocked exactly when it has an entry in the queue, and only one
  val blockedIffQueued = CONNS.forall(c =>
    match conn.get(c) {
      | Blocked(b) => range(0, waiting.length()).foldl(0, (n, i) => if (waiting[i].conn == c) n + 1 else n) == 1
                      and range(0, waiting.length()).foldl(false, (f, i) => f or (waiting[i].conn == c and waiting[i].build == b))
      | _ => range(0, waiting.length()).foldl(true, (ok, i) => ok and waiting[i].conn != c)
    })
  // a closed connection owns nothing (hang-up returns every token)
  val closedHoldsNothing = CONNS.forall(c => conn.get(c) == Closed implies connHeld.get(c).keys().size() == 0)
  // the queue was served by the stated rule: fewest held first, FIFO among equals
  val fairService = lastGrant.valid implies isFairChoice(lastGrant.queue, lastGrant.heldBefore, lastGrant.index)

  val safety = and {
    withinLimit, outMatchesBuilds, outMatchesConnections, buildsMatchConnections, heldPositive,
    waitOnlyWhenFull, blockedIffQueued, closedHoldsNothing, fairService, noDeadWaiters,
  }

  // Witnesses: `quint run --invariant=<name>` is expected to FAIL on these, proving reachability.
  val witnessQueueNonEmpty = waiting.length() < 2
  // Go behaviour only: a queued waiter whose peer has gone (it will be granted, then drained)
  val witnessAbandonedWaiterGranted = not(waiting.foldl(false, (e, w) => e or not(peerUp.get(w.conn))))
  // adopted Q-SLOT-1: the queue never holds a waiter whose peer has gone
  val noDeadWaiters = CANCEL_ABANDONED implies waiting.foldl(true, (ok, w) => ok and peerUp.get(w.conn))

  // ---------------------------------------------------------------------------------------
  // Scenario tests (quint test --main=slots spec/slots.qnt)
  // ---------------------------------------------------------------------------------------
  action stutter = all {
    out' = out, held' = held, waiting' = waiting, conn' = conn, peerUp' = peerUp,
    connHeld' = connHeld, lastGrant' = lastGrant,
  }

  // A holds both tokens and has a third request queued; B queues behind it. When one A token
  // comes back, B (holding 0) is served before A (holding 1) although A queued first.
  run fewestHeldFirstTest =
    init
      .then(requestSlot(1, "A"))
      .then(requestSlot(2, "A"))
      .then(requestSlot(3, "A"))
      .then(requestSlot(4, "B"))
      .then(done(1, "A"))
      .then(all {
        assert(conn.get(4) == Idle),
        assert(conn.get(3) == Blocked("A")),
        assert(countOf(held, "B") == 1),
        assert(safety),
        stutter,
      })

  // Two waiters of the same build: served in arrival order.
  run fifoAmongEqualsTest =
    init
      .then(requestSlot(1, "A"))
      .then(requestSlot(2, "B"))
      .then(requestSlot(3, "C"))
      .then(requestSlot(4, "C"))
      .then(done(1, "A"))
      .then(all {
        assert(conn.get(3) == Idle),
        assert(conn.get(4) == Blocked("C")),
        done(2, "B"),
      })
      .then(all { assert(conn.get(4) == Idle), assert(countOf(held, "C") == 2), assert(safety), stutter })

  // A connection may hold several tokens; hang-up returns all of them, one Release each.
  run hangUpReturnsTokensTest =
    init
      .then(requestSlot(1, "A"))
      .then(requestSlot(1, "A"))
      .then(requestSlot(2, "B"))
      .then(peerCloses(1))
      .then(drainOne(1))
      .then(all { assert(conn.get(2) == Idle), assert(out == 2), drainOne(1) })
      .then(all { assert(out == 1), drainOne(1) })
      .then(all { assert(conn.get(1) == Closed), assert(out == 1), assert(safety), stutter })

  // Q-SLOT-1 (adopted): a client that gives up while queued leaves the queue at once and is
  // never granted. (With CANCEL_ABANDONED = false the Go behaviour: it is granted when its turn
  // comes, fails to send OK and returns the token -- witnessAbandonedWaiterGranted.)
  run abandonedWaiterTest =
    init
      .then(requestSlot(1, "A"))
      .then(requestSlot(2, "A"))
      .then(requestSlot(3, "B"))
      .then(peerCloses(3))
      .then(all { assert(conn.get(3) == Draining), assert(waiting.length() == 0), done(1, "A") })
      .then(all { assert(out == 1), assert(waiting.length() == 0), drainOne(3) })
      .then(all { assert(conn.get(3) == Closed), assert(out == 1), assert(safety), stutter })

  // Quirk Q-SLOT-2: DONE for a build the connection does not hold still answers OK.
  run unheldDoneTest =
    init
      .then(done(1, "A"))
      .then(all { assert(out == 0), assert(conn.get(1) == Idle), stutter })
}

/**
 * Why a process holding a token must not ask for another: the client side of slots.
 *
 * Processes form a tree (a `ec slot make` whose make runs compilers through ec). A process
 * takes a token only if EKACACHE_SLOT is unset in its environment (jig/src/process.cc Slot ctor), and
 * exports EKACACHE_SLOT=1 to its children while it holds one. A parent cannot finish (and so cannot
 * release) before its child finishes.
 */
module slotNesting {
  pure val LIMIT = 1
  pure val PROCS = Set(1, 2, 3)
  pure val PARENT = Map(1 -> 0, 2 -> 1, 3 -> 1)   // 0 = no parent: 2 and 3 are children of 1
  // Set to false to watch noNestedWait fail (the deadlock ec avoids).
  pure val HONOR_EKACACHE_SLOT = true

  type PState = | NotStarted | Waiting | Running | Finished

  var st: int -> PState
  var holds: int -> bool
  var envSlot: int -> bool      // EKACACHE_SLOT in the process's environment
  var out: int
  var queue: List[int]

  // depth 2 is enough for PARENT above
  pure def ancestors(p: int): Set[int] = {
    val a1 = PARENT.get(p)
    if (a1 == 0) Set() else if (PARENT.get(a1) == 0) Set(a1) else Set(a1, PARENT.get(a1))
  }

  action init = all {
    st' = PROCS.mapBy(_p => NotStarted),
    holds' = PROCS.mapBy(_p => false),
    envSlot' = PROCS.mapBy(_p => false),
    out' = 0,
    queue' = List(),
  }

  // a process starts once its parent runs; it inherits the parent's environment
  action start(p: int): bool = {
    val inherited = if (PARENT.get(p) == 0) false else envSlot.get(PARENT.get(p))
    val wants = not(HONOR_EKACACHE_SLOT and inherited)
    all {
    st.get(p) == NotStarted,
    PARENT.get(p) == 0 or st.get(PARENT.get(p)) == Running,
    if (not(wants)) all {
      st' = st.set(p, Running), holds' = holds, envSlot' = envSlot.set(p, inherited),
      out' = out, queue' = queue,
    } else if (out < LIMIT and queue.length() == 0) all {
      st' = st.set(p, Running), holds' = holds.set(p, true), envSlot' = envSlot.set(p, true),
      out' = out + 1, queue' = queue,
    } else all {
      st' = st.set(p, Waiting), holds' = holds, envSlot' = envSlot.set(p, inherited),
      out' = out, queue' = queue.append(p),
    },
    }
  }

  // a running process finishes once all its children finished, returning its token
  action finish(p: int): bool = all {
    st.get(p) == Running,
    PROCS.forall(q => PARENT.get(q) == p implies st.get(q) == Finished or st.get(q) == NotStarted),
    if (holds.get(p) and queue.length() > 0) {
      // hand the token straight to the head of the queue (LIMIT = 1)
      val nxt = queue[0]
      all {
        st' = st.set(p, Finished).set(nxt, Running),
        holds' = holds.set(p, false).set(nxt, true),
        envSlot' = envSlot.set(nxt, true),
        out' = out, queue' = queue.slice(1, queue.length()),
      }
    } else all {
      st' = st.set(p, Finished), holds' = holds.set(p, false), envSlot' = envSlot,
      out' = if (holds.get(p)) out - 1 else out, queue' = queue,
    },
  }

  action step = {
    nondet p = PROCS.oneOf()
    any { start(p), finish(p) }
  }

  // A process waiting for a token while an ancestor holds one can only be served after that
  // ancestor finishes, which waits for this process: a deadlock once all tokens are nested.
  val noNestedWait = PROCS.forall(p => st.get(p) == Waiting implies ancestors(p).forall(a => not(holds.get(a))))
  val tokensAccounted = out == PROCS.filter(p => holds.get(p)).size() and out <= LIMIT
}
