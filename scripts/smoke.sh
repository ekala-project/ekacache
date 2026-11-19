#!/bin/sh
# End-to-end smoke test on the host: builds ec and ecd, starts ecd on a socket in a temporary
# directory (with its own pack directory), and compiles one file twice through `ec <compiler>`.
# The first run must compile, the second must be a cache hit with the same object. Prints
# ec's JIG_LOG rows, ecd's STATS and its log. Works on Linux and macOS.
#
#   nix-shell --run scripts/smoke.sh                       # cc on PATH
#   nix-shell --run 'scripts/smoke.sh "$(command -v clang)"'
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
cc=${1:-$(command -v cc)}

cargo build -q --locked -p ekacache -p ekacached --manifest-path "$root/Cargo.toml"
bin=$root/target/debug

dir=$(mktemp -d)
pid=
cleanup() {
    [ -n "$pid" ] && kill "$pid" 2>/dev/null && wait "$pid" 2>/dev/null
    rm -rf "$dir"
}
trap cleanup EXIT INT TERM
cd "$dir"

XDG_CACHE_HOME=$dir/cache "$bin/ecd" "$dir/sock" 2>"$dir/ecd.log" &
pid=$!
i=0
while [ ! -S "$dir/sock" ]; do
    i=$((i + 1))
    if [ "$i" -gt 50 ]; then
        echo "ecd did not start:" >&2
        cat "$dir/ecd.log" >&2
        exit 1
    fi
    sleep 0.1
done

printf 'int f(int x) { return x * 2; }\n' >a.c
for run in 1 2; do
    rm -f a.o
    JIG_SOCK=$dir/sock JIG_LOG=$dir/jig.log "$bin/ec" "$cc" -O2 -c a.c -o a.o
    cksum <a.o >"obj$run"
done

echo '--- JIG_LOG'
cat jig.log
echo '--- STATS'
printf 'STATS\n' | nc -U -w1 "$dir/sock" || true
echo '--- ecd log'
cat ecd.log

status=0
if ! cmp -s obj1 obj2; then
    echo "FAIL: the cached object differs from the compiled one" >&2
    status=1
fi
if ! sed -n 1p jig.log | grep -q '	compiled	'; then
    echo "FAIL: the first run did not compile" >&2
    status=1
fi
if ! sed -n 2p jig.log | grep -q '	cached	'; then
    echo "FAIL: the second run was not a cache hit" >&2
    status=1
fi
[ "$status" -eq 0 ] && echo 'ok: miss, then hit with the same object'
exit "$status"
