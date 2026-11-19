// -*- mode: Bluespec; -*-
/**
 * Longer runs of spec/jigd_protocol.qnt for the socket replay (crates/ekacached/tests/protocol_itf.rs).
 *
 * The model's own `step` picks a malformed request 6 times in 13 and a disconnect once in 5, so
 * its traces end after about five steps, when both connections are gone. `longStep` takes the
 * same actions with the same arguments, but sends a malformed request one time in 8 and lets a
 * connection disconnect only after it has sent 8 requests. Every `longStep` is a `step`, so
 * these traces are behaviours of the unchanged model.
 *
 *   quint run tests/itf-wrappers/jigd_protocol_long.qnt --step=longStep --mbt \
 *     --out-itf='tests/itf/jigd_protocol_long/t{seq}.itf.json' --n-traces=200 --max-steps=60 \
 *     --max-samples=200 --seed=1 --invariant=safety
 */
module jigd_protocol_long {
  import jigd_protocol.* from "../../spec/jigd_protocol"

  action longStep = {
    nondet c = CONNS.oneOf()
    nondet k = KEYS.oneOf()
    nondet v = VALS.oneOf()
    nondet b = BUILDS.oneOf()
    nondet n = range(0, MAX_IDS + 1).foldl(Set(), (s, i) => s.union(Set(i))).oneOf()
    nondet bad = 1.to(8).oneOf()
    nondet good = Set(Get(k), Has(k), Put((k, v)), Ids(n), Slot(b), Done(b), Stats).oneOf()
    nondet malf = Set(PutBadLen(k), PutBadKey, PutShortBody(k), IdsBadCount, Unknown, TooLong).oneOf()
    any {
      send(c, if (bad == 1) malf else good),
      serveOne(c),
      granted(c),
      all { sent.get(c).length() >= 8, disconnect(c) },
      evict(k),
    }
  }
}
