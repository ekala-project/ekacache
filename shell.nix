# Development shell for the Rust workspace and the legacy C++/Go build (differential tests).
#
#   nix-shell --run 'cargo test --workspace'
#   nix-shell --run 'meson setup build legacy && ninja -C build'   # legacy jig
#   nix-shell --run '(cd legacy/jigd && go build -o ../../build/jigd .)'
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
    # legacy build inputs (difftest)
    pkgs.meson
    pkgs.ninja
    pkgs.go
    pkgs.libblake3
    pkgs.nlohmann_json
    # test tools
    pkgs.strace
  ];
  # the jig store dir is a build-time constant; keep the default explicit
  JIG_STORE_DIR = builtins.storeDir;
}
