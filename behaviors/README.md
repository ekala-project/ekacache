# jig / jigd behaviour specification

These documents specify what jig (C++, `jig/src`) and jigd (Go, `jigd/`) do, precisely enough
to rewrite both in Rust as drop-in replacements. Interleavings and safety arguments are modelled
in [`../spec/`](../spec/README.md) (Quint).

## Compatibility contract

Unless a change below is explicitly signed off, the Rust implementation MUST be byte-compatible
with the current one:

- **Wire:** the jig↔jigd line protocol, including value headers and zstd framing, the
  GOCACHEPROG JSON stream, and the builder-rpc-v0 / worker-protocol bytes;
- **Keys:** every k1/k2/blob key bit for bit, so existing caches stay valid and mixed C++/Rust
  fleets share hits;
- **On disk:** jigd pack and hint files, directory layout and file names, so a Rust jigd opens an
  existing store;
- **Observable:** stdout/stderr bytes, exit codes and environment variables.

Current behaviour is normative, quirks included. Each doc ends with a **Quirks** section. Every
quirk is `### Q-<AREA>-<n>`, with **Current behavior** / **Impact** / **Recommendation**. A
recommendation of "change, needs sign-off" is a proposal, not part of the contract, until it is
adopted below.

Conventions: MUST/SHOULD/MAY follow RFC 2119; `file:line` cites the C++/Go source at commit
`4cfc796`; byte layouts are tables.

## Documents

| Doc | Covers | Quirks |
|---|---|---|
| **jigd** | | |
| [jigd-daemon.md](jigd-daemon.md) | CLI, socket creation, peer authentication, signals, logging, slot limit | Q-DAEMON-1..13 |
| [jigd-protocol.md](jigd-protocol.md) | line protocol: GET HAS PUT IDS SLOT DONE STATS, framing, hang-up rules | Q-PROTO-1..12 |
| [jigd-store.md](jigd-store.md) | bitcask packs and hints, index, tiers, budgets, demotion, eviction, recovery | Q-STORE-1..15 |
| [jigd-identities.md](jigd-identities.md) | IDS: store-file identities and the memo | Q-ID-1..5 |
| [jigd-slots.md](jigd-slots.md) | host-wide compile slots, fairness, hang-up release | Q-SLOT-1..5 |
| **jig** | | |
| [jig-dispatch.md](jig-dispatch.md) | mode selection (argv[0]/argv[1], launcher), usage, process spawning, exit status | Q-DISPATCH-1..8 |
| [jig-keys.md](jig-keys.md) | Hasher framing, key prefixes, store masking, identities, manifests, test vectors | Q-KEY-1..12 |
| [jig-cache-client.md](jig-cache-client.md) | the client side of the jigd protocol, value encoding, slots | Q-CLIENT-1..8 |
| [jig-cc-mode.md](jig-cc-mode.md) | C/C++ compile and link caching | Q-CC-1..16 |
| [jig-rustc-mode.md](jig-rustc-mode.md) | rustc caching (`rustcwrap`, launcher) | Q-RS-1..8 |
| [jig-gocacheprog.md](jig-gocacheprog.md) | GOCACHEPROG over jigd | Q-GO-1..8 |
| [jig-cache-commands.md](jig-cache-commands.md) | `jig cache put/get/put-dir/get-dir`, `jig slot`, directory blobs | Q-DIR-1..7 |
| [jig-nix-store.md](jig-nix-store.md) | `jig nix-store`: builder-rpc-v0 client, offline store, derivations | Q-NIX-1..17 |
| [jig-driver-policy.md](jig-driver-policy.md) | the compiler driver policy (`jig.json`, rpaths, runtimes) | Q-DRV-1..17 |
| [jig-reloc-fixup.md](jig-reloc-fixup.md) | `reloc-fixup`: ELF/Mach-O RUNPATH and install-name rewriting | Q-FIX-1..18 |
| [environment.md](environment.md) | every environment variable read by jig or jigd | Q-ENV-1..8 |
| [ecosystems.md](ecosystems.md) | informative: other toolchains (JVM, Fortran, CUDA, GHC, OCaml, cgo, …) and how they interact with jig | — |

## Adopted changes (signed off 2026-11-17)

The Rust port MUST implement these instead of the current behaviour:

| Change | Specified in | Compatibility |
|---|---|---|
| Value header is defined as little-endian u32 (Q-CLIENT-1) | jig-cache-client.md | byte-identical on all supported hosts |
| Hint entries sorted by record offset; readers accept any order (Q-STORE-1) | jigd-store.md | Go and Rust read each other's files |
| `HELLO <maxver>` / `OK <ver>` version negotiation; version 0 = today's protocol (new) | jigd-protocol.md §HELLO, jig-cache-client.md §Version negotiation | old clients unaffected; a new client falls back on an old daemon |
| Real JSON parser and serializer, strict base64, for gocacheprog (Q-GO-1, -3, -4) | jig-gocacheprog.md §Adopted: JSON handling | identical for Go's output |
| PUT replies checked; DONE sent after the PUTs, in cc and rustc modes (Q-CLIENT-2, Q-CC-2) | jig-cache-client.md, jig-cc-mode.md | jigd unchanged |
| Path mode hands back unmasked text: replayed depfiles and get-dir files (Q-KEY-1, Q-CC-7, Q-DIR-1) | jig-keys.md | cc keys unchanged; path-mode `b/` blobs with store paths miss once |
| Resource-exhaustion compile failures are not cached (Q-CC-5) | jig-cc-mode.md | keys unchanged |
| rustc tracks `# env-dep` in the manifest, `rs-schema=8` (Q-RS-3) | jig-rustc-mode.md | rustc entries start cold |
| rustc splits `--crate-type` on `,` for the link check (Q-RS-5) | jig-rustc-mode.md | keys unchanged |
| An unreadable input makes the invocation uncached (Q-KEY-3) | jig-keys.md | k2 unchanged for stored entries |
| A queued SLOT waiter whose peer hangs up is dequeued (Q-SLOT-1) | jigd-slots.md; `spec/slots.qnt` `CANCEL_ABANDONED` | wire unchanged |
| Empty packs deleted at open and close; demoter kicked at startup (Q-STORE-2, Q-STORE-5) | jigd-store.md; `spec/store.qnt` `DROP_EMPTY_PACKS`, `KICK_AT_START` | file formats unchanged |
| fsync on seal and on demotion, not per PUT (Q-STORE-7) | jigd-store.md; `spec/store.qnt` `FSYNC_ON_SEAL` | file formats unchanged |
| Uncacheable: side-output flags (`--coverage`, `-gsplit-dwarf`, `-MJ`, …), hidden-input flags (`-fprofile-use`, `-fplugin=`, `-specs=`), C++20 module flags, `.incbin` in assembler sources, other toolchains' temp-dir compiles (Q-CC-12..16) | jig-cc-mode.md §Additional uncacheable cases | fewer hits; no key changes |
| Strict `JIGD_SIZE` / `JIGD_COLD_SIZE` with M/G/T suffixes; invalid values exit 2; budgets logged (Q-DAEMON-1, Q-ENV-3) | jigd-daemon.md | `+N`, negatives and junk now refused |

## Proposed changes awaiting sign-off

These quirks recommend a change. Until a change is moved to the table above, the Rust port keeps
the current behaviour.

| Area | Quirks | Change | Compatibility cost |
|---|---|---|---|
| store | Q-STORE-3, -4, -9, -11 | strict pack-name parsing, ids > 999999, load-failure state | none |
| store | Q-STORE-6 | record checksums | pack format change |
| daemon | Q-PROTO-2, -9, Q-DAEMON-5, Q-ID-2 | correct `puts`, timeouts and connection limits, accept-error handling, streaming hash | none |
| keys | Q-KEY-6, Q-CC-3 | absent tracking inside the store and for GCC | fewer hits, more soundness |
| keys | Q-RS-2, -4 | rustc: MaskOut, empty blobs | rustc keys change |
| keys | Q-CC-6 | narrower build-system depfile detection | cc behaviour change |
| parsing | Q-DIR-4, -5, Q-DISPATCH-3..5 | path sanitising for get-dir; errors instead of aborts; exec/fork diagnostics | none for valid input |
| nix | Q-NIX-4..9 | see jig-nix-store.md | per item |
| driver/fixup | Q-DRV-1, -9, -10, Q-FIX-3, -4, -7, -10, -11, -17 | see those docs | per item |

## Known issue outside this spec

`jig_test` (jig/src/jig_test.cc) segfaults when built with GCC 14. This is a bug in the C++ test
build. It is not specified here and the Rust port need not reproduce it.
