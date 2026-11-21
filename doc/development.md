# Development

## Environment

Use `nix-shell` ([`shell.nix`](../shell.nix)). It provides the pinned rustc (MSRV, see
`rust-toolchain.toml`), clippy, rustfmt and zstd, plus go and the testing tools (cargo-deny,
cargo-fuzz, hyperfine, strace). `EKACACHE_STORE_DIR` is read at
**build time** (default `/nix/store`), because keys and the default socket path derive from it.

```console
$ nix-shell --run 'cargo build'
$ nix-build -A ekacache          # bin/ec, libexec/ekacache/<mode> symlinks
$ nix-build -A ekacached         # bin/ecd
```

## Before committing

```console
$ nix-shell --run 'cargo fmt --all'
$ nix-shell --run 'cargo clippy --workspace --all-targets -- -D warnings'   # pedantic, see Cargo.toml [lints]
$ nix-shell --run scripts/check-macos.sh    # if you touched anything cfg-dependent
$ nix-shell --run 'cargo test --workspace'
```

## Test layers

The full description is in [12-testing.md](../implementation/12-testing.md).

| Layer | What | Where |
|---|---|---|
| L1 vectors | test vectors from the behaviour docs | `tests/vectors/*.toml`, `scripts/check-vectors.sh` |
| L2 ported tests | the C++/Go unit tests of jig and jigd, ported (the originals went with `legacy/`, last at `86ea083`) | `legacy_port.rs`, `legacy_store.rs`, `serve.rs`, `identity.rs` |
| L4 Quint MBT | replay ITF traces from `spec/` against the code | `*_itf.rs`, `nix_store_quint.rs`, `gocache_mbt.rs`, `scripts/gen-itf.sh` |
| L5 fuzzing | 12 targets, no panics and oracle checks | `fuzz/`, `scripts/fuzz.sh` |
| perf | hit latency and daemon round trips | `scripts/bench.sh` |

A plain `cargo test` covers L1, L2, unit tests and fake-daemon transcripts. Tests that need
real binaries **skip and print a message** unless these are set:

| Variable | Points at |
|---|---|
| `EC_RUST_ECD` | `target/debug/ecd`; build it first with `cargo build -p ekacached` |
| `EC_RUST_EC` | `ec`, for the fuzz scripts |
| `EKACACHE_TEST_GCC`, `EKACACHE_TEST_CLANG` | real compilers for the cc-mode end-to-end tests |

The full matrix:

```console
$ nix-shell --run 'cargo build -p ekacached && EC_RUST_ECD=$PWD/target/debug/ecd \
    EKACACHE_TEST_GCC=… EKACACHE_TEST_CLANG=… \
    cargo test --workspace --no-fail-fast'
```

ITF traces are generated rather than committed (`tests/itf/*/` is ignored). Run
`scripts/gen-itf.sh` before the replay tests, or they pass with a "no traces" message.

## CI

[`.github/workflows/ci.yml`](../.github/workflows/ci.yml) runs these jobs:

- **check:** fmt, clippy, cargo-deny, `check-vectors` and `check-macos`.
- **test:** x86_64 and aarch64.
- **quint:** the model tests, invariants and trace replay.
- **e2e:** both architectures, with ecd and real compilers (GCC 15, clang 20).
- **macOS:** macos-15 and macos-15-intel, the workspace tests with ecd.
- **nightly:** bench and fuzz.

To read a failed job's log:

```console
$ gh run view <run> --json jobs --jq '.jobs[]|"\(.databaseId) \(.name) \(.conclusion)"'
$ gh api repos/<owner>/<repo>/actions/jobs/<job-id>/logs
```

## The user docs book

[`book/`](../book/) is the user-facing documentation (mdBook). The `book` workflow builds it on
pull requests and publishes it to GitHub Pages from `main`. To preview it:

```console
$ nix-shell -p mdbook --run 'mdbook serve book'    # http://localhost:3000
```

Update the book when a change is visible to users: messages, exit codes, environment
variables, what is cached, or the setup steps.

## Debugging tips

- `EKACACHE_LOG=<file>` makes ec log one line per decision (`compiled`, `cached`, `linked`,
  `query`, …). Many tests assert on these lines.
- A test that hangs usually has a daemon that never exits. The ecd test helper
  (`crates/ekacached/tests/common`) kills a child after 60 s and names it.
- macOS differs from Linux in errno values, strerror text, `sun_path` length (104 bytes rather
  than 108), SIGPIPE versus EPIPE, the lack of abstract sockets, Mach-O sections, and APFS
  refusing non-UTF-8 names. Write tests with `cfg!` branches that cite the behaviour doc.
