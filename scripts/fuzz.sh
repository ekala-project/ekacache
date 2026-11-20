#!/bin/sh
# The L5 fuzz targets (implementation/12-testing.md §L5): seed each corpus, then run every
# target for <seconds> (default 60, the nightly budget). Stops at the first target that finds a
# new crash; the input is left in fuzz/artifacts/<target>/.
#
#   EC_RUST_EC=target/release/ec [CC=gcc] scripts/fuzz.sh [seconds] [target...]
#
# Needs cargo-fuzz (or CARGO_FUZZ=/path/to/cargo-fuzz). libFuzzer is used without a sanitizer (nixpkgs' rustc ships none), with a
# 256 MiB allocation limit so a header that reserves gigabytes counts as a crash.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
secs=${1:-60}
[ $# -gt 0 ] && shift
targets=${*:-wire_fields wire_request frame_decode store_load gocache_line nix_drv_json elf_fixup macho_fixup depfile_parse rsp_expand listing_parse driver_json}
: "${EC_RUST_EC:?set EC_RUST_EC to a release ec}"
export RUSTC_BOOTSTRAP=1 EC_RUST_EC
fuzz=${CARGO_FUZZ:-cargo fuzz}
cd "$root/fuzz"
c=corpus
mkdir -p $c

# seed <target> <name> <bytes...> (printf format)
seed() { t=$1 n=$2; shift 2; mkdir -p "$c/$t"; printf "$@" >"$c/$t/$n"; }

# the Go oracle inputs, hex-decoded
python3 -I - "$root/tests/vectors" "$c" <<'PY'
import json, os, sys
vec, out = sys.argv[1], sys.argv[2]
for target, name in (("wire_fields", "go_fields.json"), ("wire_request", "go_parseint.json")):
    os.makedirs(f"{out}/{target}", exist_ok=True)
    for i, row in enumerate(json.load(open(f"{vec}/{name}"))):
        open(f"{out}/{target}/go{i}", "wb").write(bytes.fromhex(row["in"]))
PY
seed wire_request get 'GET k\n'
seed wire_request put 'PUT k 5\n'
seed wire_request ids 'IDS 2\n'
seed wire_request hello 'HELLO 1\n'
# a frame of "hello": the LE size, then zstd's raw-block encoding of it
seed frame_decode hello '\005\000\000\000\050\265\057\375\040\005\051\000\000hello'
seed gocache_line get '{"ID":1,"Command":"get","ActionID":"AAAA"}\n'
seed gocache_line put '{"ID":2,"Command":"put","ActionID":"AAAA","OutputID":"BBBB","BodySize":2}\n"aGk="\n'
seed gocache_line close '{"ID":3,"Command":"close"}\n'
seed nix_drv_json drv '{"system":"s","builder":"/b","args":["a"],"env":{"a":"1"},"inputSrcs":["/s"],"inputDrvs":{"/d.drv":["out"]},"outputs":{"out":{"hashAlgo":"r:sha256"}}}'
seed depfile_parse d 'a.o: a.c \\\n /nix/store/x-h/include/a.h b\\ c.h\n'
seed rsp_expand r -- '-I"a b" '"'"'c d'"'"' e\\ f\n@g\n'
seed listing_parse l 'f 644 abc\tdir/file\nl 777 ../x\tdir/link\n'
seed driver_json conf '{"cc":"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-gcc/bin/gcc","cxx":"g++","ldflags":["-L/x"]}'
# a minimal arm64 dylib header after the magic the target prepends
seed macho_fixup header '\014\000\000\001\000\000\000\000\006\000\000\000\000\000\000\000\000\000\000\000\000\000\000\000\000\000\000\000'
# a small dynamically linked ELF, minus the 6 magic bytes the target prepends
if command -v "${CC:-gcc}" >/dev/null 2>&1; then
  mkdir -p "$c/elf_fixup"
  tmp=$(mktemp -d)
  printf 'int main(void) { return 0; }\n' >"$tmp/t.c"
  "${CC:-gcc}" -O1 -s -o "$tmp/t" "$tmp/t.c"
  tail -c +7 "$tmp/t" >"$c/elf_fixup/t"
  rm -rf "$tmp"
fi

$fuzz build --sanitizer none --release
for t in $targets; do
  echo "== $t (${secs}s)"
  mkdir -p "$c/$t" "artifacts/$t"
  before=$(ls "artifacts/$t" | wc -l)
  $fuzz run --sanitizer none --release "$t" "$c/$t" -- \
    -max_total_time="$secs" -malloc_limit_mb=256 -rss_limit_mb=2048 -max_len=65536 \
    -print_final_stats=1 >"artifacts/$t.log" 2>&1 || true
  grep -E '^stat::number_of_executed_units' "artifacts/$t.log" || true
  # inputs left by earlier runs are not new findings
  if [ "$(ls "artifacts/$t" | wc -l)" -gt "$before" ]; then
    tail -n 40 "artifacts/$t.log" >&2
    echo "$t found a crash: fuzz/artifacts/$t/ (log: fuzz/artifacts/$t.log)" >&2
    exit 1
  fi
done
