# ekacache

A compile cache for Nix builds, packaged for use with
[ekapkgs](https://github.com/ekala-project) (core-pkgs). The commands are `ec` (the compiler
launcher) and `ecd` (the per-machine daemon).

This is the Rust rewrite of jig and jigd. It does not interoperate with jig deployments: the
environment (`EKACACHE_*`, `EKACACHED_*`), the socket (`/nix/var/nix/ekacache/socket`), the pack
directory (`$XDG_CACHE_HOME/ekacache/packs`), the conf file (`etc/ekacache.json`) and the
messages (`ec: …`, `ecd: …`) all use the new names, and there is no `jig` command. The
behaviour, quirks included, is specified in [`behaviors/`](behaviors/), with the changes made
along the way listed in [`behaviors/README.md` §Adopted changes](behaviors/README.md).

## Origin

jig and jigd were written by Jörg Thalheim ([@Mic92](https://github.com/Mic92)) and the repkgs
contributors as part of [repkgs](https://github.com/Mic92/repkgs). The original sources lived
under `legacy/` until commit `86ea083`. The original MIT
license and copyright are kept, see [LICENSE](LICENSE).

## Overview

- **ec** (`crates/ekacache`): sits in front of a compiler. It caches C/C++ objects, configure
  probes, links and replayable failures, rustc library crates, and Go build actions
  (`GOCACHEPROG`). It also provides `cache`, `slot`, `reloc-fixup` and `nix-store` modes.
- **ecd** (`crates/ekacached`): the per-machine daemon behind the cache socket. It holds an
  append-only pack store and hands out compiler slots, so N sandboxes × `make -jN` don't
  oversubscribe the machine.
- **ekacache-core** (`crates/ekacache-core`): what both sides share: hashing, keys, value
  framing and the wire codec.

Derivations never mention the cache. If the socket is mapped into the sandbox, ec asks ecd
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
$ nix-build -A ekacache       # bin/ec, libexec/ekacache/<mode> symlinks; ecd in the .ekacached output
$ nix-build -A ekacached     # bin/ecd
```

Or with cargo, inside `nix-shell`:

```console
$ cargo build --release
$ cargo test --workspace
```

`EKACACHE_STORE_DIR` (default `/nix/store`) is read at build time: ec's keys and its default socket
path derive from it.

## Testing with ecd and real compilers

The plain `cargo test` run covers the unit tests, the vectors from `behaviors/`, the ported
jig/jigd tests and the fake-daemon transcripts. The end-to-end tests skip themselves unless
these variables point at binaries:

| Variable | What |
|---|---|
| `EC_RUST_ECD` | `ecd` (`target/debug/ecd`) |
| `EKACACHE_TEST_GCC`, `EKACACHE_TEST_CLANG` | real compilers for the cc-mode end-to-end tests |

`scripts/check-macos.sh` type-checks and lints the workspace for `aarch64-apple-darwin` and
`x86_64-apple-darwin` from a Linux host (std is rebuilt from nixpkgs' `rust-src`, and nothing is
linked). It shows that every macOS-only code path compiles cleanly. The macOS tests still need a
Mac to run.

## Running

```console
$ ecd /nix/var/nix/ekacache/socket     # EKACACHED_SIZE (GiB, default 50), EKACACHED_SLOTS (default: CPUs)
```

```
# nix.conf
extra-sandbox-paths = /nix/var/nix/ekacache
```

ecd only serves its own user, root, and Nix build users. **Any build that can reach the socket
can write entries that other builds will replay**, so run it only where you trust everyone who
can start a build.

## License

MIT, see [LICENSE](LICENSE).
