# Nix packaging (implementation/01-workspace.md §Nix packaging, 13-rollout.md §Packaging).
#
#   nix-build -A ekacache     # bin/ec and the mode symlink farm in libexec/ekacache; ecd in .ekacached
#   nix-build -A ekacached    # bin/ecd
#   nix-build -A legacy.jig   # the C++ jig, for rollback and the difftest
#   nix-build -A legacy.jigd  # the Go jigd
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
    # the whole workspace; the differential tests skip themselves without JIG_LEGACY_* and the
    # real-compiler tests without JIG_TEST_GCC/JIG_TEST_CLANG
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

  # The C++ jig needs P2718 (lifetime extension in range-for initialisers, GCC 15 / Clang 19).
  # Without it, add-drv, fetchurls and jig_test's TestNixStore crash on a dangling temporary in
  # DerivationToATerm (behaviors Q-NIX-1).
  legacyStdenv =
    if pkgs.stdenv.cc.isGNU && lib.versionOlder pkgs.stdenv.cc.version "15" then
      pkgs.gcc15Stdenv
    else
      pkgs.stdenv;

  # legacy/ exactly as it builds today (legacy/README.md §Building); it stays until legacy removal
  # (13-rollout.md §Legacy removal step 1)
  legacy = {
    jig = legacyStdenv.mkDerivation {
      pname = "jig-legacy";
      version = "0.1.0";
      src = fs.toSource {
        root = ./legacy;
        fileset = fs.unions [
          ./legacy/meson.build
          ./legacy/meson.options
          ./legacy/jig
        ];
      };
      nativeBuildInputs = [
        pkgs.meson
        pkgs.ninja
        pkgs.pkg-config
      ];
      buildInputs = [
        pkgs.libblake3
        pkgs.zstd
        pkgs.nlohmann_json
      ];
      mesonFlags = [ "-Dstore_dir=${builtins.storeDir}" ];
      doCheck = true;
      meta = {
        mainProgram = "jig";
        # SOCK_CLOEXEC, environ without a declaration: the C++ jig builds only on Linux
        platforms = lib.platforms.linux;
      };
    };

    jigd = pkgs.buildGoModule {
      pname = "jigd-legacy";
      version = "0.1.0";
      src = fs.toSource {
        root = ./legacy/jigd;
        fileset = ./legacy/jigd;
      };
      vendorHash = "sha256-3TfQeKs/DJTm/h0QASqCPrpfGTfeTpnjaLak9eSWjz8=";
      meta.mainProgram = "jigd";
    };
  };
in
{
  inherit ekacache legacy;
  ekacached = ekacache.ekacached;
  default = ekacache;
}
