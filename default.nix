# Nix packaging (implementation/01-workspace.md §Nix packaging, 13-rollout.md §Packaging).
#
#   nix-build -A jig          # bin/jig and the mode symlink farm in libexec/jig; jigd in .jigd
#   nix-build -A jigd         # bin/jigd
#   nix-build -A legacy.jig   # the C++ jig, for rollback and the difftest
#   nix-build -A legacy.jigd  # the Go jigd
#
# JIG_STORE_DIR is a build-time constant of jig (keys and the default socket derive from it), so
# it is the store this package is built for: builtins.storeDir.
{
  pkgs ? import <nixpkgs> { },
}:
let
  inherit (pkgs) lib;
  fs = lib.fileset;

  # names jig answers to through argv[0] (behaviors/jig-dispatch.md §Mode selection): the
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

  jig = pkgs.rustPlatform.buildRustPackage {
    pname = "jig";
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
      "jigd"
    ];

    nativeBuildInputs = [ pkgs.pkg-config ];
    buildInputs = [ pkgs.zstd ];

    env.JIG_STORE_DIR = builtins.storeDir;

    cargoBuildFlags = [
      "-p"
      "jig"
      "-p"
      "jigd"
    ];
    # the whole workspace; the differential tests skip themselves without JIG_LEGACY_* and the
    # real-compiler tests without JIG_TEST_GCC/JIG_TEST_CLANG
    cargoTestFlags = [ "--workspace" ];

    postInstall = ''
      moveToOutput bin/jigd "$jigd"
      mkdir -p "$out/libexec/jig"
      for name in ${lib.escapeShellArgs (modes ++ compilers)}; do
        ln -s ../../bin/jig "$out/libexec/jig/$name"
      done
    '';

    passthru = {
      inherit modes compilers;
    };

    meta = {
      description = "Compile cache for Nix builds (jig) and its per-machine daemon (jigd)";
      license = lib.licenses.mit;
      platforms = lib.platforms.linux ++ lib.platforms.darwin;
      mainProgram = "jig";
    };
  };

  # legacy/ exactly as it builds today (legacy/README.md §Building); it stays until legacy removal
  # (13-rollout.md §Legacy removal step 1)
  legacy = {
    jig = pkgs.stdenv.mkDerivation {
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
      # jig_test segfaults in TestNixStore (a std::map compare under DerivationToATerm) with
      # GCC/libstdc++; the Rust port of those vectors (nix_store::tests::test_nix_store) passes
      doCheck = false;
      meta.mainProgram = "jig";
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
  inherit jig legacy;
  jigd = jig.jigd;
  default = jig;
}
