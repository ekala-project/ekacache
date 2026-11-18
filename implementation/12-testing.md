# 12: Testing and verification

Validation decision (2026-11-17): **differential tests + Quint model-based tests + ported
legacy tests**. Real-package canary builds on **ekapkgs** are a TODO (see
[13-rollout.md §TODO](13-rollout.md#todo)).

Five layers, from cheapest to most expensive. Each phase's exit criterion (README) names which
layers must be green.

| Layer | What it proves | Where | Runs |
|---|---|---|---|
| L1 spec vectors | the documented byte-level examples hold | `tests/vectors/`, unit tests | every push |
| L2 ported legacy tests | everything the old suites pinned still holds | `crates/*/tests/legacy_*.rs` | every push |
| L3 differential | Rust and legacy produce the same observable bytes on the same inputs | `tests/difftest/`, `jig-testkit` | every push (from P1) |
| L4 Quint MBT | the concurrent parts satisfy the models' invariants on model-generated schedules | `tests/itf/`, `jig-testkit::itf` | every push |
| L5 fuzzing | no panics; parity on malformed input | `fuzz/` | nightly |

## L1: spec vectors

Each behaviour doc's *Test vectors* section, plus the inline vectors (`jig-keys.md` digests,
keys, composites and manifests; the `jig-cache-client.md` frame; `jigd-identities.md`; the
`jig-driver-policy.md` tables; `jig-reloc-fixup.md`; `jig-nix-store.md`), is **extracted** into
data files under `tests/vectors/<doc>.toml` by hand, once per doc, with the doc section named
in each entry. A CI script checks that every vector heading in `behaviors/` has a matching
file, so a vector added to a doc later cannot be forgotten.

## L2: ported legacy tests

| Legacy suite | Lines | Port to | Notes |
|---|---|---|---|
| `legacy/jig/src/jig_test.cc` | 722 | `crates/jig/tests/legacy_jig.rs` + `jig-core` unit tests | segfaults under GCC 14 (known). Port the assertions; run the C++ binary under clang if a cross-check is needed |
| `legacy/jigd/store_test.go` | 340 | `crates/jigd/tests/legacy_store.rs` | virtual clock → `Clock` trait |
| `legacy/jigd/serve_test.go` | 132 | `crates/jigd/tests/legacy_serve.rs` | in-process server on a temp socket |
| `legacy/jigd/slots_test.go` | 29 | `crates/jigd/src/slots.rs` tests | |
| `legacy/jigd/identity_test.go` | 55 | `crates/jigd/tests/legacy_identity.rs` | |

Rules: same test names in snake case; a header comment cites the original file and line; an
expectation changed by an adopted row says `// ADOPTED Q-…` and asserts the new result. A
port is complete when every Go/C++ test function has a counterpart. A script greps the names
on both sides and lists the missing ones.

## L3: differential testing

### Harness (`crates/jig-testkit`)

- **Builds:** CI builds `legacy` (meson + go, from `legacy/`, via the Nix expression from 01)
  and the Rust workspace. Binaries: `legacy-jig`, `legacy-jigd`, `jig`, `jigd`.
- **Scenario:** a TOML file listing: the files to create, the env (an explicit allow-list
  dict; the harness starts from an **empty** environment, never the CI runner's, which also
  keeps tokens out of logs), the commands to run, and which outputs to compare.
- **Isolation:** paths enter keys, so both implementations must run at the **same absolute
  paths**. The runner executes legacy, snapshots the outputs, wipes the work dir, recreates it
  at the same path, runs Rust, and compares. Optionally, each run gets its own mount namespace
  (`unshare -rm` + bind mount) so that runs can proceed in parallel.
- **Recording proxy jigd** (`testkit::proxy`): listens on a socket, forwards to a real jigd
  (Go or Rust), and records every byte per connection in both directions. Comparisons work on
  the recorded transcripts. It normalises the **HELLO** exchange (adopted, Rust-only) out of
  the transcript, and records it separately so a test can assert it happened.
- **Fake tools:** `fake-cc` (writes a deterministic `.o`/`.d` from its argv, can be told to
  fail, crash, print given stderr, or read files to emulate depfile inputs); `fake-rustc`;
  `fake-nix-daemon` (09). The fakes make tests fast and reproducible. A smaller set of
  scenarios uses real GCC, clang, rustc and go.
- **Comparator:** per output kind: exact bytes (stdout, stderr, files, wire transcripts, pack
  files after a deterministic sequence), exit status or signal, and log lines with timestamps
  stripped (`jigd` log; `JIG_LOG` with the `<ms>` column stripped).

### Allow-list for intended differences

The adopted changes are the only places where Rust is *meant* to differ. Each gets a named
allow-list rule in `tests/difftest/allow.toml`, with the adopted row it implements:

| Rule | Adopted row | What may differ |
|---|---|---|
| `hello` | HELLO | the HELLO exchange in transcripts (normalised away) |
| `le-header` | Q-CLIENT-1 | nothing on x86-64/aarch64 (rule exists to document it) |
| `hint-order` | Q-STORE-1 | hint file entry order (compare as sorted sets) |
| `put-reply-done-order` | Q-CLIENT-2, Q-CC-2 | position of `DONE` relative to `PUT`s in the transcript |
| `path-mode-unmask` | Q-KEY-1, Q-CC-7, Q-DIR-1 | replayed depfile bytes and get-dir text files in path mode; `b/` hashes of path-mode text blobs |
| `transient-fail` | Q-CC-5 | `compiled-error` → `failed`, and the absence of `r/`/`e/` PUTs |
| `rs-schema-8` | Q-RS-3 | rustc k1s (compare preimages minus field 1), `$env:` manifest lines |
| `crate-type-split` | Q-RS-5 | comma `--crate-type` invocations: uncached in Rust |
| `unreadable-input` | Q-KEY-3 | no PUTs when an input is unreadable |
| `slot-cancel` | Q-SLOT-1 | the timing of `waiting=` in STATS after an abandoned waiter |
| `empty-packs`, `kick-at-start` | Q-STORE-2, -5 | empty pack files absent; demotion log lines at startup |
| `fsync` | Q-STORE-7 | nothing observable in bytes (strace-only check) |
| `cc-uncacheable-A..D` | Q-CC-12..16 | no PUTs, and the `uncacheable` outcome, for matching invocations |
| `jigd-size-strict` | Q-DAEMON-1 | exit 2 and a message for invalid sizes; budget log line |
| `gocache-json` | Q-GO-1, -3, -4 | `Err` replies / exit 1 for malformed lines; escaped DiskPath |

Any difference not covered by a rule fails the test, and the failure prints the first
differing byte with context. **Reviewing a new allow-list entry requires pointing at a row in
`behaviors/README.md §Adopted changes`.** That is the mechanical guard against silent drift.

### Mixed-fleet matrix

Each jig scenario runs in four configurations: {legacy-jig, jig} × {legacy-jigd, jigd}, and the
cross pairs must interoperate. Cache entries written in one configuration must hit in the
other (except where `rs-schema-8` / adopted uncacheables say otherwise).

### Scenario inventory (initial)

- jigd: every hang-up H1–H11, every command, IDS edge cases, PUT spool threshold, store
  restart, tiers and demotion, eviction order, torn tail, Go↔Rust store reopen.
- jig dispatch: usage, `--help`, symlink names, launcher classification, exec failures.
- cc: each invocation kind, depfile variants (`-MD`, `-MMD`, `-MF`, `-Wp,-MD`), `-E` to stdout,
  PCH, assembler, links (ELF), uncacheable flags, cached failures, response files, driver conf
  variants.
- rustc, gocacheprog, cache commands, nix-store, reloc-fixup: as listed in their docs' Tests
  sections.

## L4: Quint model-based testing

The `spec/` models stay as they are, and are the reference. They are not rewritten for MBT.

1. **Trace generation:** `quint run spec/<m>.qnt --main=<module> --mbt --out-itf=tests/itf/<m>/trace{seq}.itf.json
   --n-traces=N --max-steps=K` per model, in CI (`--mbt` records the action taken and its
   nondeterministic picks in each state; check the exact flag spellings against Quint 0.33.1
   when wiring it up). Adopted-change flags (`CANCEL_ABANDONED`, `DROP_EMPTY_PACKS`,
   `KICK_AT_START`, `FSYNC_ON_SEAL`) are set to `true` through a thin wrapper module per
   model, so the generated traces follow the Rust semantics.
2. **Replay driver** (`jig-testkit::itf`, using the `itf` crate): for each state transition,
   read `mbt::actionTaken` + `mbt::nondetPicks`, apply the matching operation to the real
   implementation, then compare the projected real state with the model state.

| Model | Real system under test | Projection compared |
|---|---|---|
| `slots.qnt` / `slotNesting` | `jigd::slots` directly, plus a socket-level variant on the real server | `out`, `held`, queue contents, granted set; invariants incl. `noDeadWaiters` |
| `store.qnt` | `jigd::store::Store` with a test clock, tiny `packLimit`/budgets; crash = drop + reopen | index contents, `live`/`totals`, packs per tier; `indexSound`, `liveAccounting`, `tierTotals` |
| `jigd_protocol.qnt` | real jigd over a socket | replies, hang-ups |
| `twokey.qnt` | `jig::twokey` over a fake filesystem | hit/miss decision, stored entries; soundness invariant |
| `gocache.qnt` | gocacheprog processes + Rust jigd | `hitIsConsistent`, `noTornBodies`, `pointersAnnounced` |
| `builder_rpc.qnt` | `jig nix-store` against the scriptable fake daemon | ops sent, exit |

3. **Coverage:** each witness in the models (for example `witnessAbandonedWaiterGranted`)
   must appear in at least one replayed trace. With adopted flags that witness must be
   *unreachable*, and its absence is asserted over N traces.

## L5: fuzzing

`cargo fuzz` targets (libFuzzer), 60 s each nightly. Corpora are seeded from L1/L3 inputs:

| Target | Property |
|---|---|
| `wire_fields`, `wire_request` | no panic; `fields` agrees with a Go-generated oracle table on corpus entries |
| `frame_decode` | no panic, no allocation > input-bound |
| `store_load` | random pack/hint bytes → open never panics; `indexSound` |
| `gocache_line` | no panic; reply is valid JSON or the documented exit |
| `nix_drv_json` | Rust vs legacy outcome parity (legacy binary as an oracle, offline mode) |
| `elf_fixup`, `macho_fixup` | Rust vs legacy outcome parity on mutated binaries |
| `depfile_parse`, `rsp_expand`, `listing_parse` | no panic; parity with legacy where cheap |

## Performance checks

Not correctness, but they gate P10: `hyperfine` hit latency (`jig` vs `legacy-jig` on a
trivial `.c`, Rust jigd behind both), jigd GET/PUT throughput, and IDS for 100 paths. The
target is Rust ≤ legacy + 5% on hit latency, which includes the HELLO round trip (05).
