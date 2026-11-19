#!/bin/sh
# The P10 performance checks (implementation/12-testing.md §Performance checks):
#
# 1. hit latency: `jig` and the legacy jig compiling a trivial .c that is already cached, both
#    against ecd (so the Rust side pays the HELLO round trip). Gate: Rust <= legacy + 5%.
# 2. daemon GET/PUT round trips and IDS for 100 paths, ecd against the Go jigd.
#
#   scripts/bench.sh <ec> <ecd> <legacy-jig> <legacy-jigd> <cc>
#
# Needs hyperfine and python3. Exits 1 if the hit-latency gate fails.
set -eu
[ $# -eq 5 ] || { sed -n '2,10p' "$0" >&2; exit 2; }
jig=$1 jigd=$2 legacy_jig=$3 legacy_jigd=$4 cc=$5
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
pids=
cleanup() { for p in $pids; do kill "$p" 2>/dev/null || true; done; rm -rf "$work"; }
trap cleanup EXIT INT TERM

# start_jigd <binary> <name>: a daemon with its own cache and a store of 100 files
start_jigd() {
  d=$work/$2
  mkdir -p "$d/cache" "$d/store"
  i=0
  while [ $i -lt 100 ]; do head -c 65536 /dev/urandom >"$d/store/f$i"; i=$((i + 1)); done
  NIX_STORE_DIR=$d/store XDG_CACHE_HOME=$d/cache "$1" "$d/sock" 2>"$d/log" &
  pids="$pids $!"
  n=0
  while [ ! -S "$d/sock" ]; do
    n=$((n + 1))
    [ $n -lt 200 ] || { echo "$2 did not start:" >&2; cat "$d/log" >&2; exit 1; }
    sleep 0.05
  done
}
start_jigd "$jigd" rust
start_jigd "$legacy_jigd" go

# cc mode takes its language from argv[0]; JIG_CC is the compiler of a conf-less ec or jig
mkdir -p "$work/rust-bin" "$work/legacy-bin" "$work/src"
ln -s "$jig" "$work/rust-bin/cc"
ln -s "$legacy_jig" "$work/legacy-bin/cc"
printf 'int main(void) { return 0; }\n' >"$work/src/t.c"
export JIG_CC="$cc" JIG_SOCK="$work/rust/sock"
cd "$work/src"
for side in rust legacy; do "$work/$side-bin/cc" -c t.c -o "t-$side.o"; done

echo "== hit latency (ecd behind both)"
hyperfine --warmup 20 --runs 300 -N --export-json "$work/hit.json" \
  -n ec "$work/rust-bin/cc -c t.c -o t-rust.o" \
  -n legacy-jig "$work/legacy-bin/cc -c t.c -o t-legacy.o"
python3 -I - "$work/hit.json" <<'PY'
import json, sys
r = {x["command"]: x["mean"] for x in json.load(open(sys.argv[1]))["results"]}
rust, legacy = r["ec"], r["legacy-jig"]
ok = rust <= legacy * 1.05
print(f"gate: ec {rust * 1e3:.2f} ms, legacy {legacy * 1e3:.2f} ms, limit {legacy * 1.05e3:.2f} ms: {'pass' if ok else 'FAIL'}")
sys.exit(0 if ok else 1)
PY

for side in rust go; do
  echo "== daemon round trips: $side"
  python3 -I "$here/jigd_bench.py" "$work/$side/sock" "$work/$side/store"
done
