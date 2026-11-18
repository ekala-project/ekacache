# jig

A compile cache for Nix builds, packaged for use with
[ekapkgs](https://github.com/ekala-project) (core-pkgs).

This is the Rust implementation. The C++ jig and Go jigd it replaces live in [`legacy/`](legacy/)
until the cutover is complete ([`implementation/13-rollout.md`](implementation/13-rollout.md)).
Their behaviour, quirks included, is specified in [`behaviors/`](behaviors/). The Rust port
reproduces it, except for the changes listed in
[`behaviors/README.md` §Adopted changes](behaviors/README.md).

## Origin

jig and jigd were written by Jörg Thalheim ([@Mic92](https://github.com/Mic92)) and the repkgs
contributors as part of [repkgs](https://github.com/Mic92/repkgs). See
[`legacy/README.md`](legacy/README.md) for the history of this repository. The original MIT
license and copyright are kept, see [LICENSE](LICENSE).

## Overview

- **jig** (`crates/jig`): sits in front of a compiler. It caches C/C++ objects, configure
  probes, links and replayable failures, rustc library crates, and Go build actions
  (`GOCACHEPROG`). It also provides `cache`, `slot`, `reloc-fixup` and `nix-store` modes.
- **jigd** (`crates/jigd`): the per-machine daemon behind the cache socket. It holds an
  append-only pack store and hands out compiler slots, so N sandboxes × `make -jN` don't
  oversubscribe the machine.
- **jig-core** (`crates/jig-core`): what both sides share: hashing, keys, value framing and the
  wire codec.

Derivations never mention the cache. If the socket is mapped into the sandbox, jig asks jigd
first. Otherwise it compiles as usual, so outputs are the same either way.

## How keys work

```
k1 = H(tool, cwd, normalised args, primary source bytes)  -> manifest: "input\tid" per file read
k2 = H(k1, manifest)                                       -> object, stderr, depfile, status
```

The inputs come from the compiler's own depfile (`-MD`) and, for links, the linker's
`--dependency-file`. A manifest is valid when every input still has the same identity. Store
files are identified by path, build-tree files by content.

## Building

```console
$ nix-build -A jig            # bin/jig, libexec/jig/<mode> symlinks; jigd in the .jigd output
$ nix-build -A jigd
$ nix-build -A legacy.jig     # the C++ jig, kept for rollback and the differential tests
```

Or with cargo, inside `nix-shell`:

```console
$ cargo build --release
$ cargo test --workspace
```

`JIG_STORE_DIR` (default `/nix/store`) is read at build time: jig's keys and its default socket
path derive from it.

## Testing against legacy and real compilers

The plain `cargo test` run covers the unit tests, the vectors from `behaviors/`, the ported
legacy tests and the fake-daemon transcripts. The differential and end-to-end tests skip
themselves unless these variables point at binaries:

| Variable | What |
|---|---|
| `JIG_LEGACY_JIG` | the C++ jig (`nix-build -A legacy.jig`, or `meson` in `legacy/`) |
| `JIG_LEGACY_JIGD` | the Go jigd |
| `JIG_RUST_JIGD` | a Rust jigd. When both jigds are set, tests that need one daemon use this one. |
| `JIG_TEST_GCC`, `JIG_TEST_CLANG` | real compilers for the cc-mode end-to-end tests |

`scripts/check-legacy-ports.sh` checks that every test in `legacy/jig/src/jig_test.cc` and
`legacy/jigd/*_test.go` has a Rust counterpart.

`scripts/check-macos.sh` type-checks and lints the workspace for `aarch64-apple-darwin` and
`x86_64-apple-darwin` from a Linux host (std is rebuilt from nixpkgs' `rust-src`, and nothing is
linked). It shows that every macOS-only code path compiles cleanly. The macOS tests still need a
Mac to run.

## Running

```console
$ jigd /nix/var/nix/jigd/socket    # JIGD_SIZE (GiB, default 50), JIGD_SLOTS (default: CPUs)
```

```
# nix.conf
extra-sandbox-paths = /nix/var/nix/jigd
```

The Rust jigd opens a store that the Go jigd wrote, and the Go jigd opens one that the Rust jigd
wrote. Any mix of C++/Rust jig and Go/Rust jigd works. **Before switching a host's jigd, audit its
`JIGD_*` settings:** values that legacy silently treated as the default now fail startup
(`implementation/13-rollout.md` §Compatibility at cutover).

jigd only serves its own user, root, and Nix build users. **Any build that can reach the socket
can write entries that other builds will replay**, so run it only where you trust everyone who
can start a build.

## License

MIT, see [LICENSE](LICENSE).
