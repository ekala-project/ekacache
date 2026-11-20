# 13: Rollout, cutover, legacy removal (phase P10)

## Compatibility at cutover

What users see when a host switches from legacy to Rust, from `behaviors/README.md`
§Adopted changes:

| Area | Effect |
|---|---|
| ecd store | opened in place. Go and Rust read each other's packs and hints (sorted hints are readable by Go). No migration. |
| cc cache entries | **stay warm**: keys are unchanged. Fewer new entries are written (adopted uncacheables, transient failures, unreadable inputs). |
| rustc cache entries | **go cold once**: `rs-schema=8`. C++ and Rust ec processes do not share rustc entries during a mixed period. |
| go cache entries | stay warm (keys unchanged) |
| `put-dir` blobs | path-mode text blobs containing store paths miss once (adopted Q-DIR-1) |
| ecd env | `EKACACHED_SIZE=50G` and similar values that legacy silently treated as the default now mean what they say. Junk values now **fail startup** (exit 2). **Audit host configs before switching ecd.** |
| wire | Rust ec sends HELLO first. It falls back on a Go jigd at the cost of one reconnect per process (05). Mixed fleets work in all four combinations. |

## Rollout order

1. **ecd first** (after P2). Run the Rust ecd under the existing C++ jig on one builder.
   It is the smaller, self-contained half, and the C++ jig exercises it through the existing
   protocol (no HELLO). Watch the `STATS` hit rate, the log, RSS and file descriptors for a
   week. Rollback = restart the Go jigd on the same store.
2. **ec, cache commands and gocacheprog next** (after P4/P7) on the same builder.
3. **ec cc/rustc** (after P5/P6). Compare hit rates before and after from `EKACACHE_LOG` (outcome
   counts per day). Expect cc steady or slightly lower (adopted uncacheables), and rustc cold
   for a day and then steady.
4. **reloc-fixup and nix-store** (after P8). These change build outputs, so gate them on the
   golden-tree difftest (10) and the fake-daemon difftest (09). Then roll out with an
   output-hash comparison on the canary set (see TODO).
5. **All builders**, then remove legacy from the packages.

Each step is reversible by switching the package back. The store format is shared, so no data
needs converting in either direction.

## Packaging

- One derivation builds `ec` and `ecd` (01 §Nix packaging), with the symlink farm for ec
  modes. Binary names, symlink names and the default socket path are unchanged, so the
  consumer side (`sandbox-paths`, `RUSTC_WRAPPER`, launcher wrappers) needs no edits.
- The symlink farm is in `libexec/ekacache/` (signed off 2026-11-18), so installing the package
  does not put `cc`, `gcc` and the other mode names on `PATH`. **Check before cutover** that
  the ekapkgs wrappers point at `libexec/ekacache/<name>`, not at a farm of their own.
- `legacy` stays as a separate attribute until step 5, for rollback and for the difftest CI.

## Legacy removal

When all of these hold:

- every phase's exit criterion is met, and the difftest suite has been green for 30 days;
- two weeks on all builders with no rollback;
- the open P9 questions are answered, or macOS was explicitly descoped.

Then:

1. delete `legacy/`, the legacy derivation, and the legacy jobs in CI;
2. **freeze** the difftest transcripts as golden files (`tests/golden/`), so the parity
   guarantee survives without the old binaries;
3. update `behaviors/README.md`: `file:line` citations then point at a commit that no longer
   has the sources. Pin them to the last commit that had `legacy/`, by hash.

## TODO

Items deliberately left for later:

- [ ] **Real-package canary on ekapkgs** (validation option 4, deferred by decision). Pick a
  set of ekapkgs packages covering C (`zlib`, `openssl`, `curl`), C++ (`boost` subset, `fmt`,
  a cmake + PCH project), Rust (`ripgrep`, `fd`), Go (`hugo` or similar), plus one package
  per Q-CC-16 producer (a cgo package, a GHC package with C stubs, `ocaml` with native code,
  a CUDA package if feasible). Build each twice (cold, then warm) with (a) legacy and (b) Rust,
  sharing one ecd per arm. Compare: output NAR hashes between arms (must be identical except
  where reloc-fixup legitimately differs, which should be never), warm-build hit rates, and
  total build time. Also collect every compile that rule D (temp-dir patterns) catches and
  every compile that *never* hits, to look for further temp-dir patterns
  (`behaviors/ec-cc-mode.md` OQ on pattern completeness).
- [ ] Answer the P9 decision list (11).
- [ ] Flags raised in this plan that need a yes/no before their phase ships:
  - 04: cap the loader's key allocation at the remaining file size (avoids an OOM; same result).
  - 03: SIGQUIT exit status (Go exits 2; Rust core-dumps by default).
  - 03: confirm `CGO_ENABLED` of the legacy jigd package (decides the nixbld lookup).
  - 06/08/10: abort text for the libstdc++ `terminate` cases (Q-DRV-10, Q-DIR-4, Q-FIX-10):
    keep the abort, but which message?
  - 08: report a refused `ec cache put` (Q-DIR-6 vs adopted Q-CLIENT-2).
  - 10: move Mach-O fixup from P9 into P8a for darwin cross-builds on Linux.
  - 05: HELLO round-trip cost, if measurable.
- [ ] Proposed changes still awaiting sign-off (`behaviors/README.md §Proposed changes`).
  None is implemented. Revisit after P10, when the difftest suite makes each one cheap to
  evaluate.
- [ ] Confirm the C23 `#embed` depfile behaviour (P5 probe), and update `ec-cc-mode.md`.
