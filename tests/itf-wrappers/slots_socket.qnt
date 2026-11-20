// -*- mode: Bluespec; -*-
/**
 * spec/slots.qnt (module `slots`) for the socket replay in crates/ekacached/tests/slots_itf.rs.
 *
 * ecd drains a closed connection in one go: its deferred cleanup releases every token before
 * anything else on that connection happens, while the model's `drainOne` steps may interleave
 * with other connections' requests. `socketStep` takes the same actions, but while a
 * connection is draining only `drainOne` of a draining connection may run. Every `socketStep`
 * is a `step`, so these traces are behaviours of the unchanged model.
 *
 *   quint run tests/itf-wrappers/slots_socket.qnt --step=socketStep --mbt \
 *     --out-itf='tests/itf/slots_socket/t{seq}.itf.json' --n-traces=200 --max-steps=60 \
 *     --max-samples=200 --seed=1 --invariant=safety
 */
module slots_socket {
  import slots.* from "../../spec/slots"

  action socketStep = {
    nondet c = CONNS.oneOf()
    nondet b = BUILDS.oneOf()
    val draining = CONNS.filter(d => conn.get(d) == Draining)
    if (draining.size() > 0) {
      nondet d = draining.oneOf()
      drainOne(d)
    } else any { requestSlot(c, b), done(c, b), peerCloses(c), reconnect(c) }
  }
}
