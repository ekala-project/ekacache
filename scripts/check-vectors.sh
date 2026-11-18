#!/bin/sh
# Checks the L1 spec-vector files (implementation/12-testing.md §L1: spec vectors):
#
#   - every behaviors/*.md with a heading matching /vector/i has tests/vectors/<doc>.toml;
#   - every such heading is named by at least one entry's `section`;
#   - every entry's `section` is a real heading path of its doc, and doc-only entries say why.
#
# The checks live in crates/jig-testkit/tests/vector_files.rs; this runs them. Set CARGO to
# run cargo through a wrapper.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
exec ${CARGO:-cargo} test -q -p jig-testkit --test vector_files
