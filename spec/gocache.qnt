// -*- mode: Bluespec; -*-
/**
 * `jig gocacheprog` (jig/src/gocache_mode.cc): Go's GOCACHEPROG protocol backed by jigd.
 *
 *   put(ActionID, OutputID, body):  PUT go/o/<hex OutputID> = body, then PUT go/a/<hex ActionID> = OutputID
 *   get(ActionID):                  GET go/a/<hex ActionID> -> OutputID, GET go/o/<hex OutputID> -> body
 *
 * Go computes OutputID as the hash of the body, so go/o/ is content addressed; go/a/ is a
 * mutable pointer with last-writer-wins. Several `go build`s on the host run concurrently and
 * their puts and gets interleave at the granularity of single daemon requests; the daemon may
 * evict any key at any time.
 *
 * Checked:
 *  - hitIsConsistent: every hit returns an OutputID that some put announced for that ActionID
 *    and a body whose hash is that OutputID;
 *  - noTornBodies: a body whose decoded size differs from BodySize (a killed go command leaves a
 *    torn stream) is answered with Err and never stored -- storing it would poison the action
 *    for every later build.
 *
 * Run:
 *   quint run --invariant=safety --max-steps=30 --max-samples=10000 spec/gocache.qnt
 */
module gocache {
  pure val ACTIONS = Set("a1", "a2")
  pure val BODIES = Set(1, 2, 3)         // a body; its OutputID is oid(body)
  pure val PROCS = Set(1, 2)

  pure def oid(body: int): int = 100 + body

  type PutReq = { act: str, out: int, body: int, torn: bool }
  type PState = | Idle | PutBodySent(PutReq) | GetAwaitBody({ act: str, out: int })

  var kv: str -> int                     // go/a/<action> -> OutputID
  var objs: int -> int                   // go/o/<OutputID> -> body
  var announced: str -> Set[int]         // OutputIDs a completed or partial put named per action (ghost)
  var proc: int -> PState
  var lastHit: { valid: bool, act: str, out: int, body: int }
  var lastErr: bool

  action init = all {
    kv' = Map(), objs' = Map(), announced' = ACTIONS.mapBy(_a => Set()),
    proc' = PROCS.mapBy(_p => Idle),
    lastHit' = { valid: false, act: "", out: 0, body: 0 },
    lastErr' = false,
  }

  // put, first request: torn bodies are refused before anything is sent
  action putStart(p: int, a: str, b: int, torn: bool): bool = all {
    proc.get(p) == Idle,
    if (torn) all {
      // {"ID":n,"Err":"body: got X of Y bytes"}
      lastErr' = true,
      objs' = objs, proc' = proc, announced' = announced,
    } else all {
      objs' = objs.put(oid(b), b),
      proc' = proc.set(p, PutBodySent({ act: a, out: oid(b), body: b, torn: false })),
      announced' = announced.setBy(a, s => s.union(Set(oid(b)))),
      lastErr' = false,
    },
    kv' = kv, lastHit' = lastHit,
  }

  pure def putOf(s: PState): PutReq = match s { | PutBodySent(x) => x | _ => { act: "", out: 0, body: 0, torn: false } }
  pure def getOf(s: PState): { act: str, out: int } = match s { | GetAwaitBody(x) => x | _ => { act: "", out: 0 } }

  // put, second request: point the action at the output
  action putFinish(p: int): bool = {
    val x = putOf(proc.get(p))
    all {
      x.act != "",
      kv' = kv.put(x.act, x.out),
      proc' = proc.set(p, Idle),
      objs' = objs, announced' = announced, lastHit' = lastHit, lastErr' = lastErr,
    }
  }

  // get, first request
  action getStart(p: int, a: str): bool = all {
    proc.get(p) == Idle,
    if (kv.keys().contains(a)) all {
      proc' = proc.set(p, GetAwaitBody({ act: a, out: kv.get(a) })),
      lastHit' = lastHit,
    } else all {
      proc' = proc,                     // {"ID":n,"Miss":true}
      lastHit' = lastHit.with("valid", false),
    },
    kv' = kv, objs' = objs, announced' = announced, lastErr' = lastErr,
  }

  // get, second request: the body may have been evicted in between -> Miss
  action getFinish(p: int): bool = {
    val g = getOf(proc.get(p))
    all {
      g.act != "",
      proc' = proc.set(p, Idle),
      lastHit' = if (objs.keys().contains(g.out)) { valid: true, act: g.act, out: g.out, body: objs.get(g.out) }
                 else lastHit.with("valid", false),
      kv' = kv, objs' = objs, announced' = announced, lastErr' = lastErr,
    }
  }

  action evictAction(a: str): bool = all {
    kv.keys().contains(a),
    kv' = kv.keys().exclude(Set(a)).mapBy(k => kv.get(k)),
    objs' = objs, announced' = announced, proc' = proc, lastHit' = lastHit, lastErr' = lastErr,
  }
  action evictObject(o: int): bool = all {
    objs.keys().contains(o),
    objs' = objs.keys().exclude(Set(o)).mapBy(k => objs.get(k)),
    kv' = kv, announced' = announced, proc' = proc, lastHit' = lastHit, lastErr' = lastErr,
  }

  action step = {
    nondet p = PROCS.oneOf()
    nondet a = ACTIONS.oneOf()
    nondet b = BODIES.oneOf()
    nondet torn = Set(false, true).oneOf()
    any {
      putStart(p, a, b, torn), putFinish(p), getStart(p, a), getFinish(p),
      evictAction(a), evictObject(oid(b)),
    }
  }

  val hitIsConsistent = lastHit.valid implies and {
    announced.get(lastHit.act).contains(lastHit.out),
    oid(lastHit.body) == lastHit.out,
  }
  // every stored body is the one its OutputID names (nothing torn got in)
  val noTornBodies = objs.keys().forall(o => oid(objs.get(o)) == o)
  // every action pointer names an OutputID some put announced for it
  val pointersAnnounced = kv.keys().forall(a => announced.get(a).contains(kv.get(a)))

  val safety = and { hitIsConsistent, noTornBodies, pointersAnnounced }

  val witnessHit = not(lastHit.valid)
}
