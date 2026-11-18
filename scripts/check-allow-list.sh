#!/bin/sh
# Checks tests/difftest/allow.toml against behaviors/README.md §Adopted changes
# (implementation/12-testing.md §Allow-list):
#
#   - every row of the adopted-changes table is quoted by some rule's `row`;
#   - every rule's `row` is text from one of those rows, and its `adopted` ids appear in it
#     (HELLO excepted: that row names no quirk id);
#   - every `file::function` in a rule's `tests` exists.
#
# The allow-list uses only `[[rule]]`, `key = "string"` and string arrays, so awk reads it.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
allow=tests/difftest/allow.toml
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# the adopted-changes table rows, one per line
awk '/^## Adopted changes/ { on = 1; next } on && /^## / { exit }
     on && /^\| / && !/^\| Change / && !/^\|---/ { print }' behaviors/README.md >"$tmp/rows"
[ -s "$tmp/rows" ] || { echo "no adopted-changes table in behaviors/README.md" >&2; exit 1; }

# one line per rule: name <TAB> row <TAB> adopted ids (space-separated) <TAB> tests (space-separated)
awk '
function flush() { if (name != "") printf "%s\t%s\t%s\t%s\n", name, row, ids, tests }
function strs(s,   out) { out = ""; while (match(s, /"[^"]*"/)) { out = out (out == "" ? "" : " ") substr(s, RSTART + 1, RLENGTH - 2); s = substr(s, RSTART + RLENGTH) } return out }
/^\[\[rule\]\]/ { flush(); name = row = ids = tests = ""; key = ""; next }
/^[a-z]+ = / { key = $1; v = $0; sub(/^[a-z]+ = /, "", v)
  if (key == "name") name = strs(v); else if (key == "row") row = strs(v)
  else if (key == "adopted") ids = strs(v); else if (key == "tests") tests = strs(v)
  if (v ~ /^\[/ && v !~ /\]$/) arr = key; else arr = ""; next }
arr != "" && /^\]/ { arr = ""; next }
arr == "tests" { tests = tests (tests == "" ? "" : " ") strs($0) }
END { flush() }' "$allow" >"$tmp/rules"

fail=0
err() { echo "allow.toml: $*" >&2; fail=1; }

# every rule points at a row, its ids are in that row, its tests exist
tab=$(printf '\t')
while IFS="$tab" read -r name row ids tests; do
  [ -n "$row" ] || { err "$name: no row"; continue; }
  line=$(grep -F -- "$row" "$tmp/rows" || true)
  if [ -z "$line" ]; then
    err "$name: row text not in behaviors/README.md §Adopted changes: $row"
  else
    [ "$(printf '%s\n' "$line" | wc -l)" -eq 1 ] || err "$name: row text matches more than one row: $row"
    for id in $ids; do
      [ "$id" = HELLO ] && continue
      case "$id" in
        Q-CC-1[2-6]) printf '%s\n' "$line" | grep -q -- "Q-CC-12..16" && continue ;;
      esac
      base=${id%-*}
      printf '%s\n' "$line" | grep -q -e "$id\b" -e "$base-[0-9, -]*${id##*-}\b" || err "$name: $id is not in its row"
    done
  fi
  for t in $tests; do
    file=${t%%::*} fn=${t#*::}
    if [ ! -f "$file" ]; then err "$name: no file $file"
    elif ! grep -qE "fn $fn\b" "$file"; then err "$name: no fn $fn in $file"; fi
  done
done <"$tmp/rules"

# every row is quoted by some rule
n=0
while IFS= read -r line; do
  n=$((n + 1))
  hit=0
  while IFS="$tab" read -r name row ids tests; do
    case "$line" in *"$row"*) hit=1; break ;; esac
  done <"$tmp/rules"
  [ "$hit" = 1 ] || err "no rule for adopted row $n: $(printf '%s' "$line" | cut -c1-80)"
done <"$tmp/rows"

[ "$fail" = 0 ] || exit 1
echo "allow.toml: $(wc -l <"$tmp/rules") rules cover $n adopted rows"
