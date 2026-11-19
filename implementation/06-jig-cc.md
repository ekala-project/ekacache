# 06: Driver policy and cc mode (phase P5)

The biggest and most-used mode. Driver policy comes first because cc mode cannot run without a
conf except in launcher mode.

Specified by: `jig-driver-policy.md` (all), `jig-cc-mode.md` (all), `jig-keys.md §k1 field
sequences`, `ecosystems.md` (informative).
Adopted here: Q-CC-2 (DONE after PUTs), Q-CC-5 (transient failures not stored), Q-CC-7 /
Q-KEY-1 (path-mode replay unmasked), Q-KEY-3 (unreadable input → not stored), Q-CC-12..16
(rules A–D).

## Part 1: driver policy (`crates/ekacache/src/driver/`)

| File | Contents |
|---|---|
| `conf.rs` | `DriverConf`, `load_driver_conf()`, `parse_driver_conf(text, root)`, `conf_less(compiler)`, `$PKGS_CC`, `$PKGS_PREFIX_MAP`, `$JIG_CC` |
| `json.rs` | nlohmann-compatible parse front: BOM strip, then `serde_json::from_slice::<Value>` |
| `args.rs` | `build_driver_args` (A2), `scan_user_args` (A3), FORTIFY (A4), link tail (A5) |
| `elf.rs` | `elf_link` (A6), `runpath_list` (A7), `render` + slack constants (A8), `add_runpath_entries` (A9), `is_shared_lib_name` (A10) |
| `macho.rs`, `coff.rs` | A11, A12. The COFF policy is pure argument rewriting and ships in P5. Mach-O argument policy also ships in P5 (it is platform-independent text), but is only exercised on macOS in P9 |
| `search_dirs.rs` | `with_package_lib_dirs` (A13) |

The API mirrors the table in `jig-driver-policy.md §Interface to cc mode`, since the doc says
the Rust port MUST mirror it for its unit tests: `load_driver_conf`, `conf_less`,
`parse_driver_conf`, `is_shared_lib_name`, `with_package_lib_dirs`, `build_driver_args`,
`RUNPATH_SLACK = 48`, `NEEDED_SLACK = 80`, `INTERP_SLACK = 12`.

### JSON parity with nlohmann

`serde_json` is close but not identical. Each difference gets an explicit shim and a test:

| nlohmann behaviour (normative) | serde_json | Shim |
|---|---|---|
| leading UTF-8 BOM skipped | error | strip `EF BB BF` first |
| duplicate keys: last wins | last wins (also with `preserve_order`) | none; test it |
| invalid UTF-8 → error | error | none; test it |
| `\u0000` → embedded NUL | same | none; test it |
| lone surrogate escape → error | error | none; test it |
| trailing non-whitespace → error | error | none |
| big integers / floats | irrelevant: numbers are only ever "wrong type" | none |
| non-string array element → **abort** (Q-DRV-9) | a typed error | `std::process::abort()` at the same point in the read order |

Read order matters (an abort from a bad array happens before a bad `binfmt` message), so
`parse_driver_conf` reads the keys from the `Value` in the documented order instead of
deriving `Deserialize`.

### Filesystem probe aborts (Q-DRV-10)

Legacy dies with libstdc++'s `terminate called after throwing an instance of
'std::filesystem::__cxx11::filesystem_error'` + `what(): …` text and SIGABRT when `exists()`
hits EACCES/ELOOP/ENAMETOOLONG. The Rust port reproduces the **abort** (exit by SIGABRT).
Printing libstdc++'s text byte for byte would mean imitating another runtime's internal
message. **Flag for the user:** proposal: abort with no text, or with a short
`jig: <path>: <strerror>` line. Either way the exit-by-SIGABRT is kept. This is the only place
in P5 where the stderr bytes would deliberately differ without an adopted change, so it needs
a yes/no before P5 ships.

### Locating `etc/jig.json` (Q-DRV-2)

Only via `readlink("/proc/self/exe")` → `<exe>/../../etc/jig.json`, exactly as legacy. On
macOS (P9) the legacy code has a different path. See 11.

### Tests

- Every vector under `jig-driver-policy.md §Test vectors` (§From the unit tests and §Derived
  vectors) becomes a table-driven test. The `_×N` notation is expanded by a helper, and `META`
  is a constant.
- Difftest: a corpus of `(jig.json, env, argv)` triples. Run `legacy-jig` with a fake compiler
  that prints its argv NUL-separated, then the Rust jig, and compare argv bytes. The corpus is
  seeded from the vectors and extended with a proptest generator over flag vocabularies
  (`-l`, `-L`, `-Wl,-rpath`, `-static`, `-shared`, `-nostdlib`, FORTIFY variants, …).

## Part 2: cc mode (`crates/ekacache/src/cc/`)

| File | Contents | Spec |
|---|---|---|
| `rsp.rs` | `expand_response_files`, GNU tokenisation | §Pipeline step 1 |
| `parse.rs` | suffix predicates, `parse_invocation` (the scan), after-scan rules, cacheability summary | §Argument classification |
| `uncacheable.rs` | **adopted rules A–D** | §Additional uncacheable cases |
| `primary.rs` | `primary_identity` | §Primary identity |
| `key.rs` | k1 fields, `key_environment`, `toolchain_ids`, `key_args` | §Request key, `jig-keys.md §k1 field sequences` |
| `lookup.rs` | `lookup` (manifest → k2 → pipelined `r/ o/ d/ e/`), replay | §Lookup and replay |
| `store.rs` | `compile_and_store`: `RunObserved`, build-time check, failure path, success path | §CompileAndStore |
| `uncached.rs` | `run_uncached` | §Uncached runs |
| `mod.rs` | `run(argv0, raw_args, socket, launched)`: the 10-step pipeline | §Pipeline |

### Design points

- **`Invocation`** is a plain struct with the same fields as the C++ one (`compile_only`,
  `link`, `link_one`, `query`, `source`, `output`, `to_stdout`, `wants_depfile`, `depfile`,
  `args`, `key_args`, `cacheable`, …), so the six `jig_test.cc` parse tests port one to one.
- **Byte-string args.** `Vec<OsString>` for argv. Predicates work on `as_bytes()`.
- **GCC vs clang** detection by compiler basename containing `clang` (§GCC versus clang),
  plus the wrapper handling from commit `0f1e25a` (§"Wrapper" handling).
- **Temp paths** `out_dir/.jig<pid>{.d,.link.d,.absent}` are kept bit for bit, because they end
  up in compiler argv and therefore in stderr text that may be cached.
- **`temp_directory_path()`** for the link-depfile filter: `$TMPDIR`, else `/tmp`, following
  libstdc++'s order (`TMPDIR`, `TMP`, `TEMP`, `TEMPDIR`, then `/tmp`). Port that order exactly,
  not `std::env::temp_dir()`, which only checks `TMPDIR`.

### Adopted changes, concretely

| Change | Where | Behaviour |
|---|---|---|
| **Q-CC-2 / Q-CLIENT-2**: DONE after PUTs | `store.rs` | The `SlotGuard` scope covers `Run` *and* the PUTs. stderr is forwarded right after `Run`, as before. The PUTs go out while the slot is still held, and DONE is sent last. Each PUT reply is checked. If the manifest PUT fails, the artifact PUTs are skipped and the outcome is `compiled-unstored`. |
| **Q-CC-5**: transient failures | `process::deterministic` gains the nine resource-error strings | Failed compiles with them are `failed`, not `compiled-error`. |
| **Q-KEY-1 / Q-CC-7**: path-mode replay | `lookup.rs` replay uses `store.resolve_all`, which is the identity in path mode | Replayed depfiles keep real store paths. |
| **Q-KEY-3**: unreadable input | `twokey::build_manifest` → `Unstorable` | Outcome `compiled-unstored`, nothing stored. |
| **Q-CC-12..16**: rules A–D | `uncacheable.rs`, called in step 4 for A, B, D and C-before-lookup; C-on-hit in `lookup.rs` (subject `<source> incbin`, treated as a miss, and the following compile is not stored); C-before-store in `store.rs` | Each rule is a pure function over `(args, primary_source, manifest inputs)`, tested by table. Rule D's component regexes are hand-written matchers (no `regex` crate): `go-build` + 1+ digits, `ghc` + digits + `_` + digits, prefix `tmpxft_`, prefix `camlasm`. |

### Outcome and log parity

Every path through `run` ends in exactly one `LogOutcome` call, with the outcome names of
`jig-dispatch.md §Logging` and the subject strings of `jig-cc-mode.md` (`<source> new-key`,
`… inputs-changed:<path>`, `… object-gone`, …). A test enumerates the outcomes and drives each
one through a scenario.

## Tests

1. **Ported `jig_test.cc`**: `TestParseJoinedOutput`, `TestParseLink`, `TestParseInvocation`,
   `TestParsePch`, `TestParsePreprocess`, `TestParseGuards`, `TestKeyEnvironment`, and the
   driver tests. (The C++ suite segfaults under GCC 14. Port the assertions, not the harness.)
2. **Key parity corpus (difftest)**: for a corpus of real compile command lines (captured
   from building `zlib`, `openssl`, `sqlite`, `curl`, a C++ project with PCH, and an
   assembler-heavy one), run both jigs against a recording proxy jigd (12) and compare the
   `GET m/<k1>` keys, the manifest texts PUT, and the k2-keyed artifact keys. They must be
   identical except where an adopted rule makes the Rust jig not store (allow-listed by rule
   letter).
3. **Cross-implementation hits**: compile with the C++ jig, then the same with the Rust jig, and
   the Rust jig must hit (and vice versa). This is the mixed-fleet guarantee from
   `behaviors/README.md §Compatibility contract`.
4. **Adopted-rule tests**: one per row of rules A–D, plus `.incbin` on hit (an entry written by
   the C++ jig must be refused by the Rust jig), Q-CC-5 strings, and the Q-CC-2 ordering
   (a recording jigd asserts that `DONE` arrives after the last `PUT` on the connection).
5. **`#embed` probe** (`jig-cc-mode.md` OQ5): a test that runs only when GCC ≥ 15 or clang ≥ 19
   is on `PATH`, and records whether the embedded file appears in the depfile. The result
   goes back into the behaviour doc. It does not gate.
