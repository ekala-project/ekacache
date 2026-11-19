# 07: rustc mode (phase P6)

Specified by: `jig-rustc-mode.md` (all), `jig-keys.md` (k1 framing, manifest), `environment.md`
(rustc rows).
Adopted here: Q-RS-3 (`# env-dep` tracked, `rs-schema=8`), Q-RS-5 (`--crate-type` split on
`,`), Q-CC-2 / Q-CLIENT-2 (DONE after checked PUTs), Q-KEY-3 (unreadable input → not stored).

## Module (`crates/ekacache/src/rustc/`)

| File | Contents | Spec section |
|---|---|---|
| `parse.rs` | `parse_rust_invocation`: crate name, extra-filename, out-dir, sysroot, externs, `key_args`, `links`, source | §Argument classification |
| `key.rs` | the k1 field table: `rs-schema=8`, `rustc=ToolId(OnPath(rustc))`, `stdlib=StdlibIds`, `cwd=`, `src=`, `key_args`, `extern:`, the five env fields, source bytes | §Request key |
| `depinfo.rs` | `parse_dep_info` → `{outputs, inputs, env_deps}` | §Dep-info parsing + adopted Q-RS-3 |
| `pack.rs` | `pack_files` / `unpack_files` | §Output blob |
| `mod.rs` | `run(args, socket)`: usage, uncached path, lookup, compile and store | §Invocation, §Lookup, §Compile and store |

## Implementation notes

- **`OnPath(rustc)`**: PATH lookup for a bare name, following the legacy algorithm (check
  `rustc_mode.cc`), not `which`. `ToolId` comes from `ekacache_core::store_path::Store::tool_id`.
- **`StdlibIds`**: `canonical(OnPath(rustc))` (failure → `?`), roots `real/../..` plus the
  sysroot. Scan `lib/rustlib/*/lib/` for `libstd-*.rlib` / `libcore-*.rlib`, apply
  `weakly_canonical` (port libstdc++'s semantics: canonicalise the longest existing prefix,
  then append the rest lexically normalised), sort bytewise keeping duplicates, run one
  `IDS` prefetch, then concatenate `id or "?"` + `,`. `read_dir` order does not matter because
  the result is sorted.
- **`absolute(p).lexically_normal()`**: port `std::filesystem::path::lexically_normal`
  exactly (it differs from Rust's `Path::components` normalisation on trailing `/`, on `..`
  past the root, and on empty components). It lives in `ekacache-core::bytes` (or `path.rs`),
  because cc mode uses it too, with a table test.
- **Env-dep (adopted Q-RS-3)**:
  - `parse_dep_info` keeps lines starting with `# env-dep:` and extracts `NAME` up to the
    first `=` (or the end of the line), deduplicated in first-seen order.
  - `twokey::build_manifest` takes an `env_lines` argument (empty for cc). It emits
    `$env:<NAME>\t<id>` after the input lines and before the `!` lines. `<id>` = `unset`, or
    `hex(Hasher().field("set").field(value))`.
  - `twokey::validate_manifest` interprets `$env:` pieces only when the k1 text starts with
    `rs/`, and fails with `env-changed:<NAME>`.
  - k1 field 1 = `rs-schema=8`. A test checks that a manifest written by the C++ jig (schema
    7) is never even fetched: different k1.
- **Crate-type split (adopted Q-RS-5)**: `links` is true if any `,`-separated element of any
  `--crate-type` value is `bin`, `proc-macro`, `cdylib` or `dylib`. Also apply the split to
  the separate `--crate-type <v>` form if legacy classifies that form at all (check
  `ParseRustInvocation`; the doc says "if it is classified the same way"). Resolve this while
  implementing, and if the legacy code ignores the separate form, **ask** before extending
  the check.
- **Slot scope (adopted)**: the slot is held across `Run` and the three PUTs (`m/`, `o/`,
  `e/`), then DONE. stderr is forwarded right after `Run`, as before.
- **Kept quirks**: failures never cached (Q-RS-1); no `MaskOut` on key or bytes (Q-RS-2); empty
  blob stored and replayed (Q-RS-4); glued `-L` in the key (Q-RS-6); `--out-dir`/metadata not
  in the key (Q-RS-7); `@` rewriting on restore (Q-RS-8). Each one gets a `// Q-RS-n` comment
  and a test that pins it.

## Tests

1. Ported `jig_test.cc` rustc tests (`TestDepInfo`, `TestManifest` for the `rs/` prefix, and
   the parse tests).
2. Pack/unpack round trips, including `@` substitution, modes, and malformed headers (each
   failure branch of `UnpackFiles`).
3. Env-dep adopted tests: build a crate using `env!("JIG_T")`; change `JIG_T` → miss with
   `env-changed:JIG_T`; unset it → `unset` id; `option_env!` of an unset variable.
4. Difftest key parity: same rustc argv corpus (captured from `cargo build -v` of `ripgrep`,
   `serde`, `tokio`), both jigs against a recording proxy. All key fields must match except
   field 1 (`rs-schema=7` vs `=8`), so the comparison masks that field and compares the rest
   of the framed input. To make this possible, both implementations' k1 *preimages* are
   compared, not the hashes: `ekacache-testkit` has a debug env var `JIG_DEBUG_KEYS` (Rust only)
   that dumps preimages. For legacy, the preimage is reconstructed by the harness from the
   same inputs. If that turns out too fiddly, fall back to comparing manifests and blobs
   only.
5. End-to-end: `cargo build` of a mid-size workspace with `RUSTC_WRAPPER="jig rustcwrap"`, run
   twice. The second run is all hits (`JIG_LOG` shows only `cached`), and the binaries are
   byte-identical to an uncached build.
