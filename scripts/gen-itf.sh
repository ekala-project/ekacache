#!/bin/sh
# Generates the ITF traces that the Quint replay tests read (implementation/12-testing.md §L4).
# The traces are not committed (tests/itf/*/ is in .gitignore). Each command is also in the
# header of the test that replays it:
#
#   store          crates/ekacached/tests/store_itf.rs
#   slots          crates/ekacached/tests/slots_itf.rs (in-process)
#   slots_socket   crates/ekacached/tests/slots_itf.rs (over ecd's socket)
#   ecd_protocol  crates/ekacached/tests/protocol_itf.rs (and its long-trace wrapper)
#   twokey         crates/ekacache/tests/twokey_itf.rs
#
#   QUINT=/path/to/quint scripts/gen-itf.sh     # quint 0.33.1; nixpkgs may carry an older one
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
quint=${QUINT:-quint}

gen() {
  dir=$1
  shift
  rm -rf "tests/itf/$dir"
  mkdir -p "tests/itf/$dir"
  echo "== $dir"
  "$quint" run "$@" --mbt --out-itf="tests/itf/$dir/t{seq}.itf.json" --seed=1 >/dev/null
  n=$(ls "tests/itf/$dir" | wc -l)
  [ "$n" -gt 0 ] || { echo "$dir: quint wrote no traces" >&2; exit 1; }
  echo "$n traces"
}

gen store spec/store.qnt --n-traces=100 --max-steps=60 --max-samples=100
gen slots --main=slots spec/slots.qnt --invariant=safety \
  --n-traces=200 --max-steps=60 --max-samples=200
gen slots_socket tests/itf-wrappers/slots_socket.qnt --step=socketStep --invariant=safety \
  --n-traces=200 --max-steps=60 --max-samples=200
gen ecd_protocol spec/ecd_protocol.qnt --invariant=safety \
  --n-traces=300 --max-steps=40 --max-samples=300
gen ecd_protocol_long tests/itf-wrappers/ecd_protocol_long.qnt --step=longStep --invariant=safety \
  --n-traces=200 --max-steps=60 --max-samples=200
gen twokey spec/twokey.qnt --invariant=sound \
  --n-traces=300 --max-steps=30 --max-samples=300
