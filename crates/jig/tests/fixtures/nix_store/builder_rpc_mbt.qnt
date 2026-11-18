// -*- mode: Bluespec; -*-
/**
 * Trace generation for `crates/jig/tests/nix_store_quint.rs`: the `spec/builder_rpc.qnt`
 * client against a mostly well-behaved daemon. Uniformly random picks almost never get past
 * the handshake; here five steps in six behave, so traces also reach the op replies and the
 * second fetchurls op. The transitions are the spec's own `stepClient`; only the distribution
 * of the picks differs.
 *
 * The fixtures `itf/trace*.itf.json` were made with (from this directory):
 *   quint run --main=builder_rpc_mbt --step=mostlyGood --mbt --invariant=safety \
 *     --max-steps=40 --max-samples=4000 --n-traces=4000 --seed=0x5eed \
 *     --out-itf=/tmp/itf/trace{seq}.itf.json builder_rpc_mbt.qnt
 *   python3 select_traces.py /tmp/itf itf
 * which keeps a small set covering every (state, daemon behaviour) pair, trimmed after the
 * first `Exited` state.
 */
module builder_rpc_mbt {
  import builder_rpc.* from "../../../../../spec/builder_rpc"

  action mostlyGood = {
    nondet g = 0.to(5).oneOf()
    nondet i = Set(Good, Wrong, Eof).oneOf()
    nondet m = Set(Last, Next, Activity, Error, Unknown, MsgEof).oneOf()
    nondet w = Set(true, false).oneOf()
    if (g == 0) stepClient(i, m, w)
    else stepClient(Good, if (g == 1) Next else if (g == 2) Activity else Last, true)
  }
}
