#!/bin/sh
# The P10 performance checks (implementation/12-testing.md §Performance checks):
#
# 1. hit latency: `ec` compiling a trivial .c that is already cached, against ecd.
# 2. daemon GET/PUT round trips and IDS for 100 paths.
#
#   scripts/bench.sh <ec> <ecd> <cc>
#
# Needs hyperfine and python3. Reports numbers; there is no gate.
set -eu
[ $# -eq 3 ] || { sed -n '2,9p' "$0" >&2; exit 2; }
ec=$1 ecd=$2 cc=$3
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
pids=
cleanup() { for p in $pids; do kill "$p" 2>/dev/null || true; done; rm -rf "$work"; }
trap cleanup EXIT INT TERM

# a daemon with its own cache and a store of 100 files
d=$work/ecd
mkdir -p "$d/cache" "$d/store"
i=0
while [ $i -lt 100 ]; do head -c 65536 /dev/urandom >"$d/store/f$i"; i=$((i + 1)); done
NIX_STORE_DIR=$d/store XDG_CACHE_HOME=$d/cache "$ecd" "$d/sock" 2>"$d/log" &
pids="$pids $!"
n=0
while [ ! -S "$d/sock" ]; do
  n=$((n + 1))
  [ $n -lt 200 ] || { echo "ecd did not start:" >&2; cat "$d/log" >&2; exit 1; }
  sleep 0.05
done

# cc mode takes its language from argv[0]; EKACACHE_CC is the compiler of a conf-less ec
mkdir -p "$work/bin" "$work/src"
ln -s "$ec" "$work/bin/cc"
printf 'int main(void) { return 0; }\n' >"$work/src/t.c"
export EKACACHE_CC="$cc" EKACACHE_SOCK="$d/sock"
cd "$work/src"
"$work/bin/cc" -c t.c -o t.o

echo "== hit latency"
hyperfine --warmup 20 --runs 300 -N -n ec "$work/bin/cc -c t.c -o t.o"

echo "== daemon round trips"
python3 -I "$here/ecd_bench.py" "$d/sock" "$d/store"
