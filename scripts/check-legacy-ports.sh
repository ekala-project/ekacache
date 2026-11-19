#!/bin/sh
# Lists legacy test functions without a Rust counterpart (implementation/12-testing.md §L2).
#
# A C++ `void TestFooBar()` in legacy/jig/src/jig_test.cc and a Go `func TestFooBar(t *testing.T)`
# in legacy/jigd/*_test.go are ported as `fn test_foo_bar` (or `fn foo_bar`) somewhere under
# crates/. Exits 1 when one is missing.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

snake() {
  # FooBarID -> foo_bar_i_d is not wanted: split before an upper-case letter that follows a
  # lower-case letter or digit, or that starts a new word after an acronym.
  printf '%s\n' "$1" | sed -E 's/([a-z0-9])([A-Z])/\1_\2/g; s/([A-Z])([A-Z][a-z])/\1_\2/g' | tr 'A-Z' 'a-z'
}

missing=0
check() {
  origin=$1
  name=$2
  want="test_$(snake "$name")"
  if ! grep -rqE "fn (test_)?$(snake "$name")\(" crates/; then
    echo "missing: ${origin} Test${name} -> fn ${want}"
    missing=1
  fi
}

for name in $(sed -nE 's/^(static )?void Test([A-Za-z0-9_]+)\(.*/\2/p' legacy/jig/src/jig_test.cc); do
  check legacy/jig/src/jig_test.cc "$name"
done
for file in legacy/jigd/*_test.go; do
  for name in $(sed -nE 's/^func Test([A-Za-z0-9_]+)\(t \*testing\.T\).*/\1/p' "$file"); do
    check "$file" "$name"
  done
done
if [ "$missing" = 0 ]; then
  echo "every legacy test has a counterpart"
fi
exit "$missing"
