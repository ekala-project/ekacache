# Rust implementation plan

This directory plans the rewrite of jig (C++, `legacy/jig/`) and jigd (Go, `legacy/jigd/`) in
Rust. The **what** is fixed elsewhere:

- [`../behaviors/`](../behaviors/README.md): normative behaviour, quirks, and the adopted-changes
  table. When this plan and a behaviour doc disagree, the behaviour doc wins.
- [`../spec/`](../spec/README.md): Quint models of the concurrent parts (slots, store, protocol,
  two-key lookup, gocacheprog, builder RPC).

This plan covers only the **how**: crate layout, ordering, internal design, and how each step is
shown to be correct.

## Naming

The project was renamed after the port to **ekacache**. The plan's prose keeps the old names:
read **jig** as `ec` (crate `ekacache`, `crates/ekacache`) and **jigd** as `ecd` (crate
`ekacached`, `crates/ekacached`) where they mean the Rust code, and as the C++ jig and Go jigd
where they mean `legacy/`. `jig-core` is `ekacache-core` and `jig-testkit` is
`ekacache-testkit`. The package attributes are `ekacache` and `ekacached`, and `legacy.jig` and
`legacy.jigd` are unchanged. Code paths and crate names in these docs use the new names.

The names the outside world sees are kept, because the behaviour docs fix them and mixed fleets
rely on them: the `JIG_*`/`JIGD_*` environment, the socket path `/nix/var/nix/jigd/socket`,
`jig.json`, the store layout, the wire protocol, and the usage and error text. `ec` also
answers to `jig` as its `argv[0]` name. The Rust-side test gates are `EC_RUST_EC` and
`EC_RUST_ECD`. The legacy and compiler gates (`JIG_LEGACY_*`, `JIG_TEST_*`) are unchanged.

## Decisions (2026-11-17)

| Topic | Decision | Consequence |
|---|---|---|
| Platforms | **Linux first, macOS later** | Phases 0–8 target Linux hosts (x86-64 and aarch64). macOS host behaviour (peer auth, conf-less `binfmt`, `/proc` fallbacks) and the Mach-O half of reloc-fixup come in [phase 9](11-macos.md). **Correction:** the Mach-O fixup turns out to be host-independent byte rewriting that legacy also runs on Linux. Until P9 the Rust reloc-fixup refuses trees with Mach-O files rather than silently doing less (see [10](10-jig-reloc-fixup.md#a-correction-to-the-platform-split)). |
| jigd concurrency | **tokio** | jigd is async: one task per connection, `tokio::sync` primitives for slots, `select!` on peer hang-up while a SLOT waits. Store file I/O runs off the reactor (see [04-jigd-store.md](04-jigd-store.md)). **jig stays synchronous.** It is a short-lived process on the compile hot path, so it gets no runtime (see below). |
| Dependencies | **Small vetted set** | See [01-workspace.md §Dependencies](01-workspace.md#dependencies). No CLI framework: argv is parsed by hand, byte-exactly, as the docs require. |
| Validation | **Differential + Quint MBT + ported legacy tests**; real-build canary on **ekapkgs** is a TODO | See [12-testing.md](12-testing.md) and [13-rollout.md §TODO](13-rollout.md#todo). |

Choices made in this plan that were not asked about (each is easy to revisit):

- **jig gets no async runtime.** The decision said tokio, and it is applied to jigd. jig talks
  to jigd over one blocking socket per invocation, and a tokio runtime would add startup time
  and binary size to every compile for no gain.
- **One workspace at the repository root**, with crates under `crates/`. `legacy/` stays
  buildable on its own (meson/go) for differential testing until [phase 10](13-rollout.md).
- **The store dir is a compile-time constant**, as in legacy (`meson -Dstore_dir`). Here it is
  the `JIG_STORE_DIR` build-time env var, default `/nix/store` (see
  [01-workspace.md](01-workspace.md#build-configuration)).
- **Edition 2024, MSRV = the rustc in the pinned nixpkgs/ekapkgs**, recorded in
  `rust-toolchain.toml` and `Cargo.toml` `rust-version`.

## Phases

```
P0 workspace, core, test harness ─┬─> P1 jigd store ─> P2 jigd server ─────────────┐
                                  │                                                  │
                                  └─> P3 jig foundation ─┬─> P4 cache cmds + slot    ├─> P10 rollout
                                      (dispatch, process,├─> P5 driver + cc mode     │
                                       client, manifest) ├─> P6 rustc mode           │
                                                         ├─> P7 gocacheprog          │
                                                         ├─> P8a reloc-fixup (ELF)   │
                                                         └─> P8b nix-store ──────────┤
                                                              P9 macOS ──────────────┘
```

P1–P2 (jigd) and P3–P8 (jig) are independent tracks once P0 is done: the Rust jigd is tested
against the C++ jig, and the Rust jig against the Go jigd, before the two Rust halves meet.

| Phase | Doc | Ships | Exit criterion |
|---|---|---|---|
| P0 | [01-workspace.md](01-workspace.md), [02-core.md](02-core.md) | workspace, `ekacache-core` (hasher, keys, framing, wire codec), difftest harness skeleton | every digest/key/framing vector in the behaviour docs passes; CI green |
| P1 | [04-jigd-store.md](04-jigd-store.md) | bitcask store, tiers, demotion, recovery | ported `store_test.go`; opens a Go-written store; Quint store traces replay |
| P2 | [03-jigd-server.md](03-jigd-server.md) | jigd binary: protocol, slots, identities, daemon | ported `serve_test.go`, `slots_test.go`, `identity_test.go`; wire transcripts identical to Go jigd; the C++ jig runs a nixpkgs-style build against it |
| P3 | [05-jig-foundation.md](05-jig-foundation.md) | jig binary skeleton: dispatch, launcher, process, logging, client, manifest/two-key engine | dispatch/usage/exit-code differential tests; client talks to Go jigd |
| P4 | [08-jig-gocache-and-cache-cmds.md](08-jig-gocache-and-cache-cmds.md) §cache | `cache put/get/put-dir/get-dir`, `slot` | blobs interchangeable with C++ jig |
| P5 | [06-jig-cc.md](06-jig-cc.md) | driver policy (`jig.json`, `BuildDriverArgs`) + cc mode | ported `jig_test.cc` parse/key tests; identical k1/k2 to C++ jig on a corpus; adopted rules Q-CC-12..16 |
| P6 | [07-jig-rustc.md](07-jig-rustc.md) | rustcwrap + launcher rustc, `rs-schema=8` | cargo builds hit; env-dep tracked |
| P7 | [08-jig-gocache-and-cache-cmds.md](08-jig-gocache-and-cache-cmds.md) §gocacheprog | GOCACHEPROG server | `go build` twice → all hits; Go-cache entries shared with C++ jig |
| P8a | [10-jig-reloc-fixup.md](10-jig-reloc-fixup.md) | ELF reloc-fixup | byte-identical rewritten trees vs C++ jig |
| P8b | [09-jig-nix-store.md](09-jig-nix-store.md) | `nix-store` mode | worker-protocol transcripts identical; vectors pass |
| P9 | [11-macos.md](11-macos.md) | Mach-O fixup, macOS peer auth, macOS CI | differential suite green on macOS |
| P10 | [13-rollout.md](13-rollout.md) | packaging, mixed-fleet soak, cutover, legacy removal | see that doc |

## Cross-cutting rules for every phase

1. **Cite the spec.** Every module starts with a doc comment naming the behaviour doc sections it
   implements. Each quirk that is reproduced gets a `// Q-XXX-n` comment at the line that
   reproduces it, so a later sign-off can find it with `grep`.
2. **Adopted changes are not optional.** The table in `behaviors/README.md §Adopted changes`
   lists what the Rust port does *instead of* legacy. Each adopted row has a named test and a
   matching rule in the differential harness's allow-list (see
   [12-testing.md §Allow-list](12-testing.md#allow-list-for-intended-differences)), because
   those are the only places where Rust and legacy are *meant* to differ.
3. **Proposed (unsigned) changes are not implemented.** If implementing a quirk faithfully turns
   out to be unreasonable, stop and ask. Do not quietly fix it.
4. **Byte strings, not `String`.** Paths, argv, env values, keys and manifest lines are
   `OsString`/`Vec<u8>`/`&[u8]` end to end. Legacy never validates UTF-8, so neither can we.
   `bstr` is not used; the helpers needed are small (see [02-core.md](02-core.md#bytes-byte-string-helpers)).
5. **No panics on input.** Malformed input produces the documented message and exit code.
   `unwrap()` is allowed only on invariants; CI runs `clippy::unwrap_used` as a warning in
   non-test code.
6. **Commit per milestone.** Each phase is a series of reviewable commits (`area: summary`).
   Nothing is pushed or merged without the user's go-ahead.

## Open questions carried from the behaviour docs

The plan does not resolve these. Each blocks only the item named:

- `#embed` in GCC 15 / clang 19 depfiles (`jig-cc-mode.md` OQ5): blocks nothing, but a test is
  queued in P5.
- Completeness of the Q-CC-16 temp-dir patterns: P5 ships the adopted set. The ekapkgs canary
  (TODO) is how we find more.
- `JIG_ABSENT_LOG` format, use of path mode in practice, the `IDS 0` reply: see each doc's
  *Open questions*. Reproduce current behaviour until answered.

## Document index

| Doc | Contents |
|---|---|
| [01-workspace.md](01-workspace.md) | crates, dependencies, build config, toolchain, Nix packaging, CI |
| [02-core.md](02-core.md) | `ekacache-core`: hasher, key prefixes, store-path masking, value framing, line-protocol codec, HELLO |
| [03-jigd-server.md](03-jigd-server.md) | jigd: tokio architecture, protocol server, slots, identities, peer auth, signals, logging |
| [04-jigd-store.md](04-jigd-store.md) | jigd: packs, hints, index, tiers, demotion, eviction, recovery, durability |
| [05-jig-foundation.md](05-jig-foundation.md) | jig: dispatch, launcher, process, logging, cache client, manifest and two-key engine |
| [06-jig-cc.md](06-jig-cc.md) | driver policy and cc mode |
| [07-jig-rustc.md](07-jig-rustc.md) | rustc mode |
| [08-jig-gocache-and-cache-cmds.md](08-jig-gocache-and-cache-cmds.md) | gocacheprog, `cache` subcommands, `slot` |
| [09-jig-nix-store.md](09-jig-nix-store.md) | `nix-store`: builder-rpc-v0, offline store, derivations |
| [10-jig-reloc-fixup.md](10-jig-reloc-fixup.md) | reloc-fixup (ELF in P8a; Mach-O in P9) |
| [11-macos.md](11-macos.md) | the macOS phase |
| [12-testing.md](12-testing.md) | test layers: vectors, ported tests, differential, Quint MBT, fuzzing |
| [13-rollout.md](13-rollout.md) | packaging, mixed fleets, cutover, legacy removal, TODO list |
| [STATUS.md](STATUS.md) | what is done per phase, test and performance results, decisions awaiting sign-off |
