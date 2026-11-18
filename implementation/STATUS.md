# Implementation status

As of 2026-11-18, branch `rust-port-spec`. The code for P0–P9 is written, and P10's packaging is
done. The rest of P10 is operational and time-gated, and has not started: rolling out on
builders, 30 days of green difftests, then legacy removal (13 §Rollout order, §Legacy removal).
`legacy/` and the `legacy` attribute are still in place.

Each phase lists its exit criterion from [README.md §Phases](README.md#phases), and what met it
or what is still missing.

| Phase | Commits | Exit criterion |
|---|---|---|
| P0 | `0769b87` | Vectors pass (`jig-core` unit tests, `go_vectors`). **No CI config exists**, so "CI green" is not met (see §Not built). |
| P1 | `3f849ec` | Met: `store_test.go` ported (`store`, `legacy_store`); each implementation opens the other's store (`difftest_store_compat`); Quint store traces replay (`store_itf`). |
| P2 | `c9f5a59`, `2176bdb` | Ported serve/slots/identity tests pass; wire transcripts match the Go jigd (`difftest_wire`); slots traces replay (`slots_itf`). The C++ jig's difftests and a compile smoke test pass against the Rust jigd. **No nixpkgs-style package build** was run with the C++ jig. |
| P3 | `81a8005` | Met: dispatch/usage/exit-code difftests (`binary`, `legacy_jig`); the client talks to the Go jigd, falling back to v0 (`client_jigd`). |
| P4 | `6044c3f` | Met: blobs interchangeable with the C++ jig (`cache_cmd` difftests). |
| P5 | `d451990`, `e03b0e3` | Met: `jig_test.cc` parse/key tests ported (`legacy_jig`, `2d5de7b`); k1/k2 identical to the C++ jig with real GCC 15.3 and clang 20.1 (`cc_mode`). The corpus is the test corpus, not a package set. |
| P6 | `01722c1`, `c94213e` | Met for a test workspace: `cargo build` twice, every library crate hits (`rustc_mode::cargo_workspace_twice`); env deps tracked. No real-crate corpus. |
| P7 | `517830a` | Met: `go build` twice gives only hits (`gocache_e2e`); entries are shared with the C++ jig (`gocache_difftest`). |
| P8a | `df4caec` | Met: rewritten trees byte-identical to the C++ jig's (`fixup_difftest`). |
| P8b | `16bcd17` | Met: worker-protocol transcripts identical (`nix_store_rpc`, `nix_store_quint`); vectors pass (`nix_store_offline`). |
| P9 | `9a95a39`, `54ec5c7`, `b648a62`, `bb12667` | **Not met:** "differential suite green on macOS" needs a Mac. Done so far: the Mach-O fixup, which is host-independent and difftested on Linux (`fixup_macho`); every macOS `cfg` branch; and `scripts/check-macos.sh`, which runs clippy with `-D warnings` for both darwin targets, clean. |
| P10 | `33862e0`, `2ab5f67`, `e35fe48`, `8ee93c5`, `04033fb` | Packaging done: `default.nix` builds `jig`, `jigd`, `legacy.jig` and `legacy.jigd`, and runs the test suites in the sandbox. Mixed-fleet compatibility tested locally (§Test results). Rollout, soak and legacy removal: not started. |

## Test results

All on Linux x86_64, at `04033fb`. The legacy jig is the GCC 15 build from `nix-build -A
legacy.jig`, the Go jigd is `legacy/jigd` built with Go 1.25.9, and the real compilers are GCC
15.3 and clang 20.1.

| Combination | Result |
|---|---|
| Rust jig + Rust jigd, every gate set (`cargo test --workspace`) | 448 passed, 0 failed, 1 ignored (the HELLO benchmark, by design) |
| Rust jig + Go jigd (`cargo test -p jig`, no `JIG_RUST_JIGD`) | 310 passed, 0 failed, 1 ignored |
| C++ jig + Rust jigd | difftests and compile smoke pass |
| Go jigd ↔ Rust jigd store files | each opens the other's store (`difftest_store_compat`) |
| Nix sandbox: `nix-build -A jig` | 448 passed; the gated tests skip themselves |
| Nix sandbox: `nix-build -A legacy.jig` | `jig_test` passes (it segfaulted under GCC 14, see below) |
| macOS (aarch64, x86_64) | type-check and clippy only, clean |

## Performance (12 §Performance checks)

| Check | Rust | Legacy |
|---|---|---|
| Hit latency on a trivial `.c`, Rust jigd behind both; **gate: Rust ≤ legacy + 5%** | 1.1 ms | 2.2 ms |
| jigd PUT, 4 KiB, one at a time | 11.5–16.4 µs | 14.2–16.3 µs |
| jigd GET hit, 4 KiB, one at a time | 9.0–9.2 µs | 11.4–14.5 µs |
| jigd GET hit, 4 KiB, pipelined | 3.3–4.1 µs | 5.2–7.1 µs |
| `IDS` for 100 paths, first (hashing) | ~3.5 ms | ~8.5 ms |
| `IDS` for 100 paths, memoised | 0.5–0.6 ms | ~1.0 ms |

- The HELLO round trip costs about 33 µs per jig process, and is included in the hit latency.
- Before `2ab5f67`, jigd ran its store calls through `spawn_blocking`, as 03 §Runtime shape
  says. That made it about 1.5× slower than Go per GET or PUT (22 µs). See decision 6.

## Decisions awaiting sign-off

These are defaults I chose. Each can be reversed.

1. **SIGQUIT** (03 §Runtime shape asks for this before P10). The Rust jigd installs no handler,
   so SIGQUIT terminates it with a core dump. Go dumps its goroutines and exits 2.
2. **fsync on macOS.** The store calls `File::sync_all`, which is `F_FULLFSYNC` on Apple, the
   same as Go's `File.Sync`. Plain `fsync` would be faster but weaker, and is a one-line change.
3. **`etc/jig.json` on macOS** is not found: legacy has no current-executable lookup there, and
   the port reproduces that. A conf-less jig on macOS uses Mach-O as its binfmt.
4. **No peer authentication on macOS** (Q-DAEMON-8 kept). Every peer is served, as with Go's
   `peer_other.go`. The Nix build-user lookup exists only on Linux.
5. **reloc-fixup: a page-size shift of 64 or more is an error.** Legacy masks the shift with
   `& 63`, and aborts on a shift of 255.
6. **jigd's store calls use `block_in_place`, not the plan's `spawn_blocking`** (`2ab5f67`).
   A slow call still holds up no other connection, and the thread cap is the same.
7. **`jig cache put-dir` exits 0 when a PUT is refused.** Adopted Q-CLIENT-2 makes `jig cache
   put` exit 1 and says nothing about put-dir, so put-dir keeps legacy's exit 0.
8. **The store loader caps its key allocation** (04 §open_store). This is the loader's one
   deliberate deviation.
9. **The nixbld group lookup goes through `getgrnam_r` (NSS)**, as the cgo-built Go jigd does.
10. **The packaging puts the mode symlinks in `libexec/jig/`.** Legacy's symlink farm comes from
    the ekapkgs packaging, not this repository. Check that ekapkgs uses the same place.
11. **Aborts with no message are kept from legacy**: Q-DRV-10, Q-DIR-4, Q-FIX-10, and a
    `getcwd` failure in rustc mode. Where libstdc++ printed a `terminate` message, the port
    aborts with no text or a short one.
12. **SIGTERM before the handlers exist** (Q-DAEMON-11). The window is reproduced: a SIGTERM
    that arrives in it kills jigd without sealing.

## Known differences from legacy

### jigd

- It logs extra lines about the tiers at startup.
- While a SLOT waits, request bytes pipelined after it are read from the socket. Go reads
  nothing until the grant.
- `IDS` hashes the path lines already in the buffer before writing their answers. The bytes on
  the wire are the same, but the timing differs.
- `IDS` hashes files as a stream (Q-ID-2). The result is the same, with less memory.
- GET reads the first chunk of the value together with the lookup.
- No `SHUT_WR` when a connection ends.
- fsync error texts differ.

These quirks are kept, though they look unreasonable:

- A FIFO in the store blocks `IDS`.
- Any accept error, including EMFILE, shuts the daemon down with exit 0 (Q-DAEMON-5).
- The identity memo is never pruned (Q-ID-1).

### jig

- **Dispatch, client and keys:**
  - The process does not remember a version-0 HELLO result across connections. That is
    allowed, and every mode connects at most once.
  - Children are started with `fork` and `execvp`, not `std::process::Command`. Command
    folds a failed fork and a failed exec into one error, and legacy reports the two
    differently (Q-DISPATCH-4).
  - A refused PUT logs `log::put_failed` with the mode's usual subject, in every mode.
- **Driver:** `jig.json` and `$PKGS_CC` are read by a hand-written parser. The plan proposed
  `serde_json`, but probing legacy found nlohmann behaviour that `serde_json` cannot match: a
  NUL byte ends the input, nesting is unlimited, and a number that overflows is an error
  (`driver/json.rs`).
- **cc:** when rule C (Q-CC-12) skips the store, the log subject is `<source> incbin`.
- **rustc:**
  - Q-RS-5 needs no case for the separate `--crate-type <v>` form. It is joined into
    `--crate-type=<v>` before the check, in legacy too.
  - The Q-KEY-3 log subject is the crate label.
  - `env-dep` values are recorded while the slot is held, so a recorded `JIG_SLOT` is `1`.
  - `$env:` lines without a tab are ignored.
  - A `getcwd` failure aborts with SIGABRT, as in legacy.
- **gocacheprog:**
  - Duplicate JSON keys get an `Err` reply.
  - A quoted or absent integer `ID` is fatal, and so is a whitespace-only line.
  - A body without `BodySize` is fatal.
  - Negative IDs are echoed back.
  - The `Err` texts are the port's own.
  - A DiskPath failure (Q-GO-5) does not abort: an `EACCES` from `exists` counts as absent.
- **reloc-fixup:**
  - No `terminate` text on aborts.
  - No `ENAMETOOLONG` fallback for canonical paths.
  - glibc's stdout buffering is emulated, and so are legacy's early loop exits.
  - The output follows readdir order.
  - The Mach-O half was written alongside P8a (`9a95a39`), because it is host-independent.
- **nix-store:**
  - Q-NIX-1 (a C++-only dangling temporary) is not reproduced.
  - Deep JSON nesting is handled, where legacy overflows its stack.
  - A NUL byte ends the JSON input.
  - `dump()` aborts on a path or `JIG_STORE_DIR` that is not UTF-8.
  - A failed stdout write exits 0.

### Corrections to the behaviour docs

- Q-CC-8: with `TMPDIR=""`, the `-E` temporary is the relative `jig<pid>.i`, in legacy as in the
  port, not `/jig<pid>.i` as the doc says.
- Mach-O `dataoff` wraps around at 2^32 (a `u32`, reproduced), not at 2^64 as the spec says.

## Not built, or not run

- **CI.** There is no CI config. The jobs in 01 §CI, including the difftest and macOS jobs, need
  writing.
- **Testing infrastructure from 12 that was not built as specified:**
  - the `jig-testkit` harness (TOML scenarios, recording proxy, fakes) is a stub; each phase
    has its own Rust difftest harness instead, and `jig-testkit` holds only the ITF reader;
  - `tests/difftest/allow.toml`;
  - the extraction of `tests/vectors/<doc>.toml` and its CI check;
  - the L5 fuzz targets;
  - a socket-level replay of `spec/jigd_protocol.qnt`.

  The jigd peer refusal is tested only in-process.
- **The macOS tests have not run on a Mac**, only the cross type-check and lint.
- **The ekapkgs real-package canary** (13 §TODO) was deferred, as planned.
- **Possible cleanups:**
  - the reloc-fixup byte helpers and `nix_store/hash.rs` overlap with `jig-core`;
  - `connect_unix` could return a distinct error for a path that is too long.
