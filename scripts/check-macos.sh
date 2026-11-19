#!/bin/sh
# Type-checks and lints the workspace for macOS from a Linux host (implementation/11-macos.md
# §Work items 1-2), without a darwin std or an Apple SDK:
#
# - std is built from nixpkgs' rust-src for the pinned rustc (`-Zbuild-std`, allowed on the
#   stable toolchain by RUSTC_BOOTSTRAP);
# - nothing is linked, so zstd-sys may take the host's pkg-config answer, and blake3 is told
#   its `pure` feature is on, so its build script compiles no C or assembly for the target.
#
# This proves that every cfg(target_vendor = "apple") branch compiles and is lint-clean. It runs
# nothing: the tests still need a Mac.
#
#   scripts/check-macos.sh                 # aarch64 and x86_64
#   scripts/check-macos.sh x86_64-apple-darwin
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

src=${RUST_LIB_SRC:-$(nix-build '<nixpkgs>' -A rustPlatform.rustLibSrc --no-out-link)}
targets=${*:-aarch64-apple-darwin x86_64-apple-darwin}

export RUSTC_BOOTSTRAP=1
export __CARGO_TESTS_ONLY_SRC_ROOT="$src"
export PKG_CONFIG_ALLOW_CROSS=1
export CARGO_FEATURE_PURE=1
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target/macos-check}"

for target in $targets; do
  echo "== $target"
  cargo clippy -Zbuild-std=std,panic_abort --target "$target" --workspace --all-targets -- -D warnings
done
