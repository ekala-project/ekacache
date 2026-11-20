# Development shell for the Rust workspace.
#
#   nix-shell --run 'cargo test --workspace'
#
# quint 0.33.1 (spec/ trace generation) is expected on PATH; nixpkgs may carry an older one.
{
  pkgs ? import <nixpkgs> { },
}:
pkgs.mkShell {
  packages = [
    # Rust (MSRV = this rustc; see rust-toolchain.toml)
    pkgs.rustc
    pkgs.cargo
    pkgs.clippy
    pkgs.rustfmt
    pkgs.pkg-config
    pkgs.zstd
    # gocache_e2e and the Go generators under crates/*/gen
    pkgs.go
    # test tools
    pkgs.cargo-deny # check job (deny.toml)
    pkgs.cargo-fuzz # scripts/fuzz.sh
    pkgs.hyperfine # scripts/bench.sh
    pkgs.python3 # scripts/bench.sh
  ]
  ++ pkgs.lib.optional pkgs.stdenv.isLinux pkgs.strace;
  # the ec store dir is a build-time constant; keep the default explicit
  JIG_STORE_DIR = builtins.storeDir;
}
