# Nix packaging (implementation/01-workspace.md §Nix packaging, 13-rollout.md §Packaging).
#
#   nix-build -A ekacache     # bin/ec and the mode symlink farm in libexec/ekacache; ecd in .ekacached
#   nix-build -A ekacached    # bin/ecd
#
# JIG_STORE_DIR is a build-time constant of ec (keys and the default socket derive from it), so
# it is the store this package is built for: builtins.storeDir.
{
  pkgs ? import <nixpkgs> { },
}:
let
  inherit (pkgs) lib;
  fs = lib.fileset;

  # names ec answers to through argv[0] (behaviors/jig-dispatch.md §Mode selection): the
  # non-cc modes, and the compiler names cc mode takes its language from
  modes = [
    "cache"
    "slot"
    "rustcwrap"
    "gocacheprog"
    "reloc-fixup"
    "nix-store"
  ];
  compilers = [
    "cc"
    "c++"
    "gcc"
    "g++"
    "clang"
    "clang++"
    "gfortran"
  ];

  ekacache = pkgs.rustPlatform.buildRustPackage {
    pname = "ekacache";
    version = "0.2.0";

    src = fs.toSource {
      root = ./.;
      # behaviors/, spec/ and tests/ hold vectors and traces that the test suite reads
      fileset = fs.unions [
        ./Cargo.toml
        ./Cargo.lock
        ./clippy.toml
        ./crates
        ./behaviors
        ./spec
        ./tests
      ];
    };
    cargoLock.lockFile = ./Cargo.lock;

    outputs = [
      "out"
      "ekacached"
    ];

    nativeBuildInputs = [ pkgs.pkg-config ];
    buildInputs = [ pkgs.zstd ];

    env.JIG_STORE_DIR = builtins.storeDir;

    cargoBuildFlags = [
      "-p"
      "ekacache"
      "-p"
      "ekacached"
    ];
    # the whole workspace; the real-compiler tests skip themselves without
    # JIG_TEST_GCC/JIG_TEST_CLANG
    cargoTestFlags = [ "--workspace" ];

    postInstall = ''
      moveToOutput bin/ecd "$ekacached"
      mkdir -p "$out/libexec/ekacache"
      for name in ${lib.escapeShellArgs (modes ++ compilers)}; do
        ln -s ../../bin/ec "$out/libexec/ekacache/$name"
      done
    '';

    passthru = {
      inherit modes compilers;
    };

    meta = {
      description = "Compile cache for Nix builds (ec) and its per-machine daemon (ecd)";
      license = lib.licenses.mit;
      platforms = lib.platforms.linux ++ lib.platforms.darwin;
      mainProgram = "ec";
    };
  };

in
{
  inherit ekacache;
  ekacached = ekacache.ekacached;
  default = ekacache;
}
