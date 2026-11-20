# ec / ecd behaviour specification

These documents specify what ec and ecd do. They were written from the C++ jig (`jig/src`) and
Go jigd (`jigd/`) that ec and ecd were ported from, and the Rust port reproduces that behaviour
except for the adopted changes below. Interleavings and safety arguments are modelled in
[`../spec/`](../spec/README.md) (Quint).

## Compatibility contract

Interoperability with jig deployments is not a goal any more (2026-11-19): the external names
changed (see the first adopted row), and `legacy/` was removed. Within ekacache, unless a change
below is explicitly signed off, the behaviour MUST stay as specified:

- **Wire:** the ec↔ecd line protocol, including value headers and zstd framing, the
  GOCACHEPROG JSON stream, and the builder-rpc-v0 / worker-protocol bytes;
- **Keys:** every k1/k2/blob key bit for bit, so existing caches stay valid;
- **On disk:** ecd pack and hint files, directory layout and file names, so ecd opens an
  existing store;
- **Observable:** stdout/stderr bytes, exit codes and environment variables.

Current behaviour is normative, quirks included. Each doc ends with a **Quirks** section. Every
quirk is `### Q-<AREA>-<n>`, with **Current behavior** / **Impact** / **Recommendation**. A
recommendation of "change, needs sign-off" is a proposal, not part of the contract, until it is
adopted below.

Conventions: MUST/SHOULD/MAY follow RFC 2119; `file:line` cites the C++/Go source at commit
`4cfc796` (`jig/src/x.cc` and `jigd/x.go`; the last commit with them under `legacy/` is
`86ea083`); byte layouts are tables. Where a "Current behavior" names jig or jigd, it describes
that source; ec and ecd behave the same.

## Documents

| Doc | Covers | Quirks |
|---|---|---|
| **ecd** | | |
| [ecd-daemon.md](ecd-daemon.md) | CLI, socket creation, peer authentication, signals, logging, slot limit | Q-DAEMON-1..13 |
| [ecd-protocol.md](ecd-protocol.md) | line protocol: GET HAS PUT IDS SLOT DONE STATS, framing, hang-up rules | Q-PROTO-1..12 |
| [ecd-store.md](ecd-store.md) | bitcask packs and hints, index, tiers, budgets, demotion, eviction, recovery | Q-STORE-1..15 |
| [ecd-identities.md](ecd-identities.md) | IDS: store-file identities and the memo | Q-ID-1..5 |
| [ecd-slots.md](ecd-slots.md) | host-wide compile slots, fairness, hang-up release | Q-SLOT-1..5 |
| **ec** | | |
| [ec-dispatch.md](ec-dispatch.md) | mode selection (argv[0]/argv[1], launcher), usage, process spawning, exit status | Q-DISPATCH-1..8 |
| [ec-keys.md](ec-keys.md) | Hasher framing, key prefixes, store masking, identities, manifests, test vectors | Q-KEY-1..12 |
| [ec-cache-client.md](ec-cache-client.md) | the client side of the ecd protocol, value encoding, slots | Q-CLIENT-1..8 |
| [ec-cc-mode.md](ec-cc-mode.md) | C/C++ compile and link caching | Q-CC-1..16 |
| [ec-rustc-mode.md](ec-rustc-mode.md) | rustc caching (`rustcwrap`, launcher) | Q-RS-1..8 |
| [ec-gocacheprog.md](ec-gocacheprog.md) | GOCACHEPROG over ecd | Q-GO-1..8 |
| [ec-cache-commands.md](ec-cache-commands.md) | `ec cache put/get/put-dir/get-dir`, `ec slot`, directory blobs | Q-DIR-1..7 |
| [ec-nix-store.md](ec-nix-store.md) | `ec nix-store`: builder-rpc-v0 client, offline store, derivations | Q-NIX-1..17 |
| [ec-driver-policy.md](ec-driver-policy.md) | the compiler driver policy (`ekacache.json`, rpaths, runtimes) | Q-DRV-1..17 |
| [ec-reloc-fixup.md](ec-reloc-fixup.md) | `reloc-fixup`: ELF/Mach-O RUNPATH and install-name rewriting | Q-FIX-1..18 |
| [environment.md](environment.md) | every environment variable read by ec or ecd | Q-ENV-1..8 |
| [ecosystems.md](ecosystems.md) | informative: other toolchains (JVM, Fortran, CUDA, GHC, OCaml, cgo, …) and how they interact with ec | — |

## Adopted changes (signed off 2026-11-17)

The Rust port MUST implement these instead of the current behaviour:

| Change | Specified in | Compatibility |
|---|---|---|
| ekacache names (2026-11-19): `EKACACHE_*`/`EKACACHED_*` for `JIG_*`/`JIGD_*` (`EKACACHE_STORE_DIR` at build time too), socket `/nix/var/nix/ekacache/socket`, packs `$XDG_CACHE_HOME/ekacache/packs`, `etc/ekacache.json`, temp names `.ekacache<pid>…` and `ekacache<pid>.i`, messages and usage `ec: …`/`ec <mode>: …`/`ecd: …`; `ec` is the only self name (no `jig`) | every doc | none with jig: a jig fleet, store, socket or patched clang (`EKACACHE_ABSENT_LOG`) must be moved over |
| Value header is defined as little-endian u32 (Q-CLIENT-1) | ec-cache-client.md | byte-identical on all supported hosts |
| Hint entries sorted by record offset; readers accept any order (Q-STORE-1) | ecd-store.md | Go and Rust read each other's files |
| `HELLO <maxver>` / `OK <ver>` version negotiation; version 0 = today's protocol (new) | ecd-protocol.md §HELLO, ec-cache-client.md §Version negotiation | old clients unaffected; a new client falls back on an old daemon |
| Real JSON parser and serializer, strict base64, for gocacheprog (Q-GO-1, -3, -4) | ec-gocacheprog.md §Adopted: JSON handling | identical for Go's output |
| PUT replies checked; DONE sent after the PUTs, in cc and rustc modes (Q-CLIENT-2, Q-CC-2) | ec-cache-client.md, ec-cc-mode.md | ecd unchanged |
| Path mode hands back unmasked text: replayed depfiles and get-dir files (Q-KEY-1, Q-CC-7, Q-DIR-1) | ec-keys.md | cc keys unchanged; path-mode `b/` blobs with store paths miss once |
| Resource-exhaustion compile failures are not cached (Q-CC-5) | ec-cc-mode.md | keys unchanged |
| rustc tracks `# env-dep` in the manifest, `rs-schema=8` (Q-RS-3) | ec-rustc-mode.md | rustc entries start cold |
| rustc splits `--crate-type` on `,` for the link check (Q-RS-5) | ec-rustc-mode.md | keys unchanged |
| An unreadable input makes the invocation uncached (Q-KEY-3) | ec-keys.md | k2 unchanged for stored entries |
| A queued SLOT waiter whose peer hangs up is dequeued (Q-SLOT-1) | ecd-slots.md; `spec/slots.qnt` `CANCEL_ABANDONED` | wire unchanged |
| Empty packs deleted at open and close; demoter kicked at startup (Q-STORE-2, Q-STORE-5) | ecd-store.md; `spec/store.qnt` `DROP_EMPTY_PACKS`, `KICK_AT_START` | file formats unchanged |
| fsync on seal and on demotion, not per PUT (Q-STORE-7) | ecd-store.md; `spec/store.qnt` `FSYNC_ON_SEAL` | file formats unchanged |
| Uncacheable: side-output flags (`--coverage`, `-gsplit-dwarf`, `-MJ`, …), hidden-input flags (`-fprofile-use`, `-fplugin=`, `-specs=`), C++20 module flags, `.incbin` in assembler sources, other toolchains' temp-dir compiles (Q-CC-12..16) | ec-cc-mode.md §Additional uncacheable cases | fewer hits; no key changes |
| Strict `EKACACHED_SIZE` / `EKACACHED_COLD_SIZE` with M/G/T suffixes; invalid values exit 2; budgets logged (Q-DAEMON-1, Q-ENV-3) | ecd-daemon.md | `+N`, negatives and junk now refused |

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
| nix | Q-NIX-4..9 | see ec-nix-store.md | per item |
| driver/fixup | Q-DRV-1, -9, -10, Q-FIX-3, -4, -7, -10, -11, -17 | see those docs | per item |

## Known issue outside this spec

`jig_test` (jig/src/jig_test.cc) segfaults when built with GCC 14. This is a bug in the C++ test
build. It is not specified here and the Rust port need not reproduce it.
