# 01: Workspace, dependencies, build, CI (phase P0)

## Layout

```
Cargo.toml                 workspace: members, shared [workspace.dependencies], lints, profiles
rust-toolchain.toml        pinned channel (matches the Nix-provided rustc)
crates/
  jig-core/                library, no I/O beyond std::io traits
    src/hasher.rs          length-framed BLAKE3, 128-bit hex digests          (02-core.md)
    src/keys.rs            key prefixes, k1/k2 builders, KeyEnvironment
    src/store_path.rs      STORE_DIR, IsStorePath, masking/unmasking
    src/frame.rs           value header (LE u32) + zstd
    src/wire.rs            jigd line protocol: requests, replies, number parsing, HELLO
    src/manifest.rs        manifest line format (shared by cc/rustc and tests)
    src/bytes.rs           byte-string helpers
  jigd/                    binary + lib (lib exists for tests)
    src/main.rs            CLI, env, startup order, signals                  (03-jigd-server.md)
    src/server.rs          accept loop, per-connection task, command dispatch
    src/slots.rs           slot table
    src/identity.rs        IDS memo
    src/peer.rs            peer authentication (linux.rs / other.rs)
    src/store/             packs, hints, index, tiers, demoter               (04-jigd-store.md)
  jig/                     binary + lib
    src/main.rs            argv[0]/argv[1] dispatch                          (05-jig-foundation.md)
    src/launcher.rs, process.rs, log.rs, client.rs, twokey.rs
    src/cc/                parse.rs, key.rs, run.rs, uncacheable.rs           (06-jig-cc.md)
    src/driver/            conf.rs, args.rs                                  (06-jig-cc.md)
    src/rustc.rs                                                             (07-jig-rustc.md)
    src/gocache.rs, src/dir.rs, src/slot.rs                                  (08-…)
    src/nix_store/         rpc.rs, offline.rs, drv.rs, nix32.rs              (09-jig-nix-store.md)
    src/fixup/             mod.rs, elf.rs, macho.rs (P9)                     (10-jig-reloc-fixup.md)
  jig-testkit/             dev-only: difftest harness, fake/proxy jigd, ITF trace replay
tests/
  vectors/                 test vectors extracted from behaviors/*.md (TOML/JSON)
  difftest/                differential scenarios (see 12-testing.md)
  itf/                     generated Quint traces (gitignored; regenerated in CI)
legacy/                    untouched until P10
spec/, behaviors/          unchanged; the source of truth
implementation/            this plan
flake.nix or default.nix   packaging (see below)
```

Why `jig` and `jigd` each have a lib target: integration tests and `jig-testkit` call internal
functions (for example `parse_invocation`, `Store::open`) that the doc tables say "the Rust port
MUST mirror for its own unit tests" (for example `jig-driver-policy.md §Interface to cc mode`).

`jig-core` is shared by both binaries because keys, framing and the wire codec must agree
between them. jigd uses only `wire` and `hasher` (identities). It never decompresses values.

## Dependencies

Each one is chosen for a reason, and adding another needs a note in this table.

| Crate | Used by | Why | Notes |
|---|---|---|---|
| `blake3` | core | the hasher, and jigd identities | pure Rust with SIMD. Same digest as `libblake3` and `lukechampine.com/blake3`; vectors in `jig-keys.md` prove it. Disable the `rayon` feature. |
| `zstd` (`zstd-sys`) | jig | value framing, level 1 | Build with the `pkg-config` feature so Nix links the system libzstd. Compressed bytes are **not** required to match legacy (they are opaque values), but the `jig-cache-client.md` vector pins libzstd 1.5.7, so that test runs only when the linked version matches. |
| `serde`, `serde_json` | jig | gocacheprog JSON (adopted real parser), `jig.json`, derivation JSON in nix-store | default features, **without** `preserve_order`: nlohmann objects iterate in sorted key order, and serde_json's default `BTreeMap` matches that (see 09). Parity shims against nlohmann (BOM, abort-on-type-error) are listed in 06 and 09. |
| `base64` | jig | strict RFC 4648 (adopted, Q-GO-4) | `STANDARD` engine with strict padding |
| `sha2` | jig | nix-store store-path computation | replaces legacy's own SHA-256 |
| `rustix` | jig, jigd | `SO_PEERCRED`, `fstat` fields (dev/ino/mtime ns), `fsync`/`fdatasync`, `execv`, `waitpid` status, unlinkat, `/proc/self/exe`, socket options | safe wrappers. Avoids raw `libc` in most places. |
| `object` | jig | ELF/Mach-O **type definitions and endian handling only** (`object::elf`, `object::macho`, `object::endian`), with `default-features = false` | the reloc-fixup writer is ours, because it must reproduce legacy byte for byte (see 10) |
| `libc` | jig, jigd | the few calls rustix lacks: `strerror_r` (glibc message text), `localtime_r` (jigd log timestamps), `sigaction` for SIGPIPE (05), `getgrnam_r` if jigd must match a cgo-built Go (03) | |
| `tokio` | jigd | features `rt-multi-thread, net, io-util, sync, signal, macros, time, fs` | not a dependency of jig |
| dev: `tempfile`, `proptest`, `itf` (Informal Systems' ITF deserialiser), `serde_json` | tests | | not in release binaries |

Rejected: `clap` (argv must be byte-exact and dispatch is by argv[0]); `anyhow`/`thiserror`
(errors are small enums whose `Display` produces the documented messages); `tracing` (logging
formats are specified byte for byte; a hand-written logger is about 50 lines); `nix` (the crate;
`rustix` covers it); `memmap2` (packs are read with `pread`; see 04).

### Startup budget for jig

jig runs once per compiler invocation. Targets, checked in CI with `hyperfine` against the
legacy binary on a cache hit of a trivial `.c` file:

- no allocation-heavy statics; nothing initialised before dispatch;
- release profile: `opt-level = 3`, `lto = "thin"`, `codegen-units = 1`, `panic = "abort"`
  (legacy is `-fno-exceptions`; abort matches its failure mode), `strip = "debuginfo"`;
- **target: hit latency ≤ legacy + 5%.** A regression fails the benchmark job (it is
  informational until P10).

## Build configuration

- `JIG_STORE_DIR`: read in `jig-core/build.rs` through `option_env!`, default `/nix/store`,
  exported as `pub const STORE_DIR: &[u8]`. It is used for masking, `IsStorePath`, the default
  socket `<store>/../var/nix/jigd/socket` and nix-store paths. `build.rs` declares
  `cargo:rerun-if-env-changed=JIG_STORE_DIR`. The value must be absolute, contain no trailing
  `/`, and not be `/`; `build.rs` fails otherwise.
- A test checks that a non-default store dir gives the key vectors that `jig-keys.md` prescribes
  for masking (masking depends on STORE_DIR).
- jigd takes no build-time configuration.

## Toolchain and lints

- `rust-toolchain.toml` pins the channel to the rustc in the pinned package set.
- Workspace lints: `unsafe_code = "deny"` in `jig-core`. In `jig`/`jigd` it is `warn`, and each
  `unsafe` gets a `// SAFETY:` comment. Expected uses: none, because rustix covers the syscalls.
  Clippy pedantic is on, with a short allow-list.
- `cargo deny` (licenses: MIT-compatible; no duplicate major versions of tokio/serde).

## Nix packaging

- `default.nix` / `flake.nix` at the root builds both binaries with `rustPlatform.buildRustPackage`
  (`cargoLock.lockFile`), and passes `JIG_STORE_DIR = builtins.storeDir`.
- Outputs: `jig` (binary plus the symlink farm: `cc c++ gcc g++ … cache slot rustcwrap
  gocacheprog reloc-fixup nix-store`, the same names that legacy's packaging installs) and `jigd`.
- `shell.nix` provides the Rust toolchain, `pkg-config`, `zstd`, `quint` (0.33.1), and the
  legacy build inputs (meson, ninja, GCC/clang, Go, libblake3, nlohmann_json), so one shell can
  build both implementations for differential tests.
- The legacy package is exposed as `legacy` for the harness, built from `legacy/` exactly as
  today.

## CI

| Job | Runs | When |
|---|---|---|
| `check` | `cargo fmt --check`, `clippy -D warnings`, `cargo deny` | every push |
| `test` | `cargo test --workspace` (unit, vectors, ported legacy tests) | every push |
| `quint` | `quint verify`/`quint test` on `spec/*.qnt` (unchanged models), then `quint run --out-itf` trace generation and the Rust replay tests | every push |
| `difftest` | build `legacy` + Rust, then run `tests/difftest` | every push from P1 on |
| `bench` | hyperfine startup/hit latency vs legacy | nightly; gating from P10 |
| `fuzz` | `cargo fuzz` targets for 60 s each (see 12) | nightly |
| `macos` | `test` + `difftest` on macOS | from P9 |

Linux CI covers x86-64 and aarch64 (the protocol and on-disk formats are little-endian
everywhere, but the aarch64 job proves that the LE header is no longer an accident).
