# Development

## Environment

Use `nix-shell` ([`shell.nix`](../shell.nix)). It provides the pinned rustc (MSRV, see
`rust-toolchain.toml`), clippy, rustfmt and zstd, plus the tools for the legacy build (meson,
go) and for testing (cargo-deny, cargo-fuzz, hyperfine, strace). `JIG_STORE_DIR` is read at
**build time** (default `/nix/store`), because keys and the default socket path derive from it.

```console
$ nix-shell --run 'cargo build'
$ nix-build -A ekacache          # bin/ec, libexec/ekacache/<mode> symlinks
$ nix-build -A ekacached         # bin/ecd
$ nix-build -A legacy.jig        # C++ jig, for differential tests
$ nix-build -A legacy.jigd       # Go jigd
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
| L2 ported tests | every legacy C++/Go test has a Rust counterpart | `scripts/check-legacy-ports.sh` |
| L3 differential | Rust vs legacy, on identical inputs | `*difftest*.rs`, `legacy_jig.rs`, allow-list `tests/difftest/allow.toml` |
| L4 Quint MBT | replay ITF traces from `spec/` against the code | `*_itf.rs`, `nix_store_quint.rs`, `scripts/gen-itf.sh` |
| L5 fuzzing | 12 targets, run against legacy | `fuzz/`, `scripts/fuzz.sh` |
| perf | hit latency and daemon round trips | `scripts/bench.sh` |

A plain `cargo test` covers L1, L2, unit tests and fake-daemon transcripts. Tests that need
real binaries **skip and print a message** unless these are set:

| Variable | Points at |
|---|---|
| `JIG_LEGACY_JIG` | the C++ jig (`nix-build -A legacy.jig`) |
| `JIG_LEGACY_JIGD` | the Go jigd (`nix-build -A legacy.jigd`) |
| `EC_RUST_ECD` | `target/debug/ecd`; build it first with `cargo build -p ekacached` |
| `EC_RUST_EC` | `ec`, for the fuzz scripts |
| `JIG_TEST_GCC`, `JIG_TEST_CLANG` | real compilers for the cc-mode end-to-end tests |

The full matrix:

```console
$ nix-shell --run 'cargo build -p ekacached && EC_RUST_ECD=$PWD/target/debug/ecd \
    JIG_LEGACY_JIG=… JIG_LEGACY_JIGD=… JIG_TEST_GCC=… JIG_TEST_CLANG=… \
    cargo test --workspace --no-fail-fast'
```

ITF traces are generated rather than committed (`tests/itf/*/` is ignored). Run
`scripts/gen-itf.sh` before the replay tests, or they pass with a "no traces" message.

## CI

[`.github/workflows/ci.yml`](../.github/workflows/ci.yml) runs these jobs:

- **check:** fmt, clippy, cargo-deny, the spec cross-checks (`check-legacy-ports`,
  `check-allow-list`, `check-vectors`) and `check-macos`.
- **test:** x86_64 and aarch64.
- **quint:** the model tests, invariants and trace replay.
- **difftest:** both architectures, against legacy and real compilers.
- **macOS:** macos-15 and macos-15-intel, against the Go jigd only, because the C++ jig does
  not build on macOS.
- **nightly:** bench and fuzz.

To read a failed job's log:

```console
$ gh run view <run> --json jobs --jq '.jobs[]|"\(.databaseId) \(.name) \(.conclusion)"'
$ gh api repos/<owner>/<repo>/actions/jobs/<job-id>/logs
```

## Debugging tips

- `JIG_LOG=<file>` makes ec log one line per decision (`compiled`, `cached`, `linked`,
  `query`, …). Many tests assert on these lines.
- A test that hangs usually has a daemon that never exits. The ecd test helper
  (`crates/ekacached/tests/common`) kills a child after 60 s and names it.
- macOS differs from Linux in errno values, strerror text, `sun_path` length (104 bytes rather
  than 108), SIGPIPE versus EPIPE, the lack of abstract sockets, Mach-O sections, and APFS
  refusing non-UTF-8 names. Write tests with `cfg!` branches that cite the behaviour doc.
