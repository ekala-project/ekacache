# ec rustc caching (`rustcwrap` and launcher mode)

rustc mode (`rustcwrap`, cargo's `RUSTC_WRAPPER` convention, and launcher mode for a program
named `rustc`) caches library crate compilations in ecd with the same two-key scheme as cc
mode. k1 is computed under the `rs/` prefix. The crate root is the primary source, rustc's
dep-info provides the manifest inputs, and every output rustc names in the dep-info is stored
as one packed blob. This document specifies which invocations are cacheable, the k1 field
list, dep-info parsing, the blob format, and the lookup/store sequence, so that a Rust
implementation can be a byte-compatible replacement. Hasher framing, `Store::Key`, the
manifest format and k2 are in `behaviors/ec-keys.md`. The socket protocol is in
`behaviors/ec-cache-client.md`.

Sources: `jig/src/rustc_mode.h`, `jig/src/rustc_mode.cc`, `jig/src/main.cc` (dispatch),
`jig/src/launcher.cc`, `jig/src/manifest.cc`, `jig/src/process.cc`, `jig/src/keys.h`,
`jig/src/base.cc` (`ReplaceAll`, `ParseUint`, `WriteFile`), `jig/src/jig_test.cc`
(`TestDepInfo`, `TestRustInvocation`, `TestManifest`).

Key words MUST, MUST NOT, SHOULD and MAY are as in RFC 2119. Current behavior is normative.
Line numbers refer to `jig/src/rustc_mode.cc` unless another file is named.

---

## Invocation

| Entry | `args` given to `RunRustcMode` |
|---|---|
| `rustcwrap <rustc> <rustc args…>` (argv[0] basename `rustcwrap`, or `ec rustcwrap …`) | `[<rustc>, <rustc args…>]` |
| launcher `ec /path/to/rustc <args…>` (basename `rustc` after stripping `-<digits.dots>`) | `[/path/to/rustc, <args…>]` (`launcher.cc:113-117`) |

If `args` is empty, rustc mode prints `rustcwrap: usage: rustcwrap <rustc> args...\n` to stderr
and exits 1 (`:279-282`).

## Argument classification (`ParseRustInvocation`, `:209-236`)

`inv.args` = the rustc arguments, passed to rustc unchanged in every path. For each index,
the first matching rule applies:

1. **`TakeNamedOption`** (`:127-161`). Each flag is accepted as `--flag value` (when a next
   token exists) or `--flag=value`:

   | Flag | Effect | `key_args` entry |
   |---|---|---|
   | `--out-dir` | `out_dir = value` | none |
   | `--sysroot` | `sysroot = value` | `--sysroot=<value>` |
   | `--crate-name` | `crate_name = value` | `--crate-name=<value>` |
   | `--extern` | if `value` contains `=`: append the text after the first `=` to `externs` | `--extern=<value>` |
   | `--emit` | `has_dep_info \|= value contains "dep-info"` | `--emit=<value>` |
   | `-L` (only `-L x` or `-L=x`) | dropped | none |

2. `--print`, `--print=…`, `-`, `-vV` or `--version` → `query = true`.
3. `-o` → `cacheable = false`. Its value token is then classified on its own.
4. Does not start with `-` and ends with `.rs` → `source = arg`, `++sources`.
5. Anything else → **`TakeCodegenOption`** (`:165-182`):
   - `full = arg`. If `arg` is one of the two-token options
     `-C --cfg --check-cfg --cap-lints --edition --target --crate-type -Z -W -A -D -F --error-format --json --diagnostic-width --remap-path-prefix -l`
     and a next token exists: `full = arg + "=" + next` (consumed). Else, if `arg` starts with
     `-C` but not `-C=`: insert `=` after `-C` (`-Copt-level=3` → `-C=opt-level=3`).
   - `full` starts with `-C=incremental=` → `cacheable = false`. Not added to the key.
   - `full` starts with `-C=extra-filename=` → `extra_filename` = the rest. Not added to the key.
   - `full` starts with `-C=metadata=` → dropped.
   - otherwise (Rust port: per comma-separated element, adopted Q-RS-5): `links |= full ∈ {--crate-type=bin, --crate-type=proc-macro,
     --crate-type=cdylib, --crate-type=dylib}`, `has_crate_type |= full starts with
     "--crate-type="`, and `full` is appended to `key_args`.

After the scan: `links |= !has_crate_type` (no `--crate-type` means bin). The invocation is
**cacheable** only if all of these hold:

- not `query`;
- not `links`;
- exactly one `.rs` source;
- `--out-dir` given;
- some `--emit` value contains `dep-info`;
- no `-o`;
- no `-C incremental=…`.

So `lib`, `rlib` and `staticlib` crates compiled by cargo are cached. Binaries, build
scripts, tests (`--test`, no `--crate-type`), proc-macros, cdylibs and dylibs are not.

Test vectors: `jig_test.cc` `TestRustInvocation`.

## Uncached runs (`:185-205`)

The invocation runs uncached if it is not cacheable, the source is unreadable, or
`Connect(socket)` fails. ec holds a `Slot(cache, socket)` if `source` is non-empty, runs rustc
with stdio inherited, and logs the first outcome that applies: `query`, then `linked` (if
`links`) or `uncacheable` when not cacheable, then `uncacheable-nosrc`, else `no-daemon`. The
EKACACHE_LOG tool name is `rustc`. `EKACACHE_LOG_ARGS` is **not** written in rustc mode. It returns
rustc's status.

## Request key (k1) (`:297-318`)

Fields, in order (framing in `ec-keys.md`):

| # | Field |
|---|---|
| 1 | `rs-schema=7` (C++). The Rust port MUST use `rs-schema=8` (adopted Q-RS-3) |
| 2 | `"rustc=" + ToolId(OnPath(rustc))` |
| 3 | `"stdlib=" + StdlibIds(cache, rustc, sysroot)` |
| 4 | `"cwd=" + Key(current_path())` |
| 5 | `"src=" + Key(source)` (as spelled on the command line) |
| 6… | `Key(a)` for each `a` in `key_args`, in argv order |
| … | `"extern:" + (InputId(path) or "?")` for each path in `externs`, in order |
| … | `CARGO_PKG_NAME=<v>`, `CARGO_PKG_VERSION=<v>`, `CARGO_CFG_TARGET_FEATURE=<v>`, `RUSTFLAGS=<v>`, `CARGO_ENCODED_RUSTFLAGS=<v>`: five fields, `<v>` = `Env(var)`, empty when unset |
| last | the source file's raw bytes |

k1 text = `"rs/" + hex` (`keys.h`, `TestManifest`). The manifest key is `m/rs/<hex>`. Unlike cc
mode, no field passes through `MaskOut`.

`StdlibIds` (`:243-273`): `real = canonical(OnPath(rustc))`. If that fails, the result is
`?`. Roots are `real/../..` plus `sysroot` if one was given. For each root, for each entry
`T` of `root/lib/rustlib`, for each entry of `T/lib` whose name starts with `libstd-` or
`libcore-` and ends with `.rlib`, collect `weakly_canonical(entry)`. Sort bytewise (duplicates
are kept), run one `PrefetchIdentities`, and concatenate `(InputId or "?") + ","`.

## Lookup (`:320-330`)

1. `FindResult(cache, k1)`: manifest lookup and validation (`ec-keys.md`).
2. If there is a k2: `GET o/<k2>`. If present and `UnpackFiles(blob, out_dir, extra_filename)`
   succeeds: `GET e/<k2>` (a separate round trip), write its value (or nothing) to stderr
   verbatim, log `cached`, and **return 0**. No slot is taken.
3. Otherwise fall through to the compile. Files written by a partial `UnpackFiles` are left
   in place and overwritten by rustc.

The miss subject in EKACACHE_LOG is `<label> <why>`. `label` = `crate_name + extra_filename`, or
`source` if there is no crate name. `why` = the FindResult error, or `object-gone` when the
manifest validated but the blob was missing or failed to unpack.

## Compile and store (`:332-357`)

1. Within a scope: `Slot(cache, "")`, using the existing connection. Then
   `Run(rustc, args, kCapture)`. The C++ client releases the slot (**DONE**) before anything is
   stored. The Rust port MUST release it after the PUTs instead (adopted change, cc mode Q-CC-2).
2. Write the captured stderr to stderr verbatim.
3. **Failure** (`status != 0`): log `failed` and return the status. **Failures are never
   cached** (Q-RS-1).
4. `dep_text = ReadFile(out_dir/<crate_name><extra_filename>.d)`. If unreadable: log
   `compiled-unstored`, return 0.
5. `info = ParseDepInfo(dep_text)`.
6. `manifest = BuildManifest(cache, k1, info.inputs, absolute(source).lexically_normal())`,
   with no absent list.
7. Sequential PUTs, each waiting for `OK`:
   1. `m/<k1>` = manifest text
   2. `o/<k2>` = `PackFiles(info.outputs, extra_filename)`
   3. `e/<k2>` = captured stderr, **raw: no `MaskOut`** (Q-RS-2)
8. Log `compiled`, return 0.

rustc mode never writes `r/` or `d/`.

## Dep-info parsing (`ParseDepInfo`, `:58-78`)

`Split(text, '\n')` drops empty lines. For each line:

- starts with `#` → skipped. This covers `# env-dep:NAME=value` and `# checksum:…`.
  **`env!()`/`option_env!()` dependencies are not tracked** (Q-RS-3; the Rust port tracks them, adopted). The comment at `:350`
  says so. Only the `CARGO_PKG_*`, `CARGO_CFG_TARGET_FEATURE` and `RUSTFLAGS` variables are in
  k1.
- no `": "` (colon followed by a space) in the line → skipped. These are the phony `input:`
  rules.
- otherwise → `outputs += line[0 : first ": "]`, verbatim with no unescaping. If `inputs` is
  still empty, `inputs = [absolute(p).lexically_normal() for p in ParseDepfile(line)]`. Inputs
  therefore come from the **first** rule only.

Vector (`TestDepInfo`):
`out/t.d: src/lib.rs src/m.rs\n\nout/libt.rlib: src/lib.rs src/m.rs\n\nsrc/lib.rs:\nsrc/m.rs:\n# env-dep:X\n`
→ outputs `[out/t.d, out/libt.rlib]`, inputs `[<cwd>/src/lib.rs, <cwd>/src/m.rs]`.

The outputs include the dep-info file itself, because rustc writes a rule for it.

## Output blob (`o/<k2>`)

`PackFiles(paths, stem)` (`:83-96`) concatenates one record per output path, in dep-info
order. Paths are read relative to the cwd. An unreadable path is skipped silently.

| Part | Bytes |
|---|---|
| name | `ReplaceAll(basename(path), stem, "@")`: every occurrence of `extra_filename` replaced by `@`. Unchanged when `stem` is empty |
| `0x20` | |
| mode | `permissions & 0777` in octal, no leading zero, for example `644` |
| `0x20` | |
| size | the byte count in decimal |
| `0x0a` | |
| data | the file's bytes, unmodified (no `MaskOut`) |

Example: `libfoo-abcd.rlib` with stem `-abcd`, mode 0644, 5 bytes is `libfoo@.rlib 644 5\n`
followed by the 5 bytes.

An empty blob (no readable outputs) is still stored (Q-RS-4).

`UnpackFiles(blob, dir, stem)` (`:98-123`): while the blob is non-empty, read up to `\n`
(missing → fail), split the header on single spaces (keeping no empty fields), and require
exactly 3 fields, an octal mode, a decimal size and `size ≤ remaining`. Write
`dir / ReplaceAll(name, "@", stem)` with `WriteFile` (temp + rename), then set its
permissions to `mode` exactly (replace), ignoring errors. Any failure returns false. An empty
blob returns true. Files are always restored into `--out-dir` under their basename, wherever
rustc originally wrote them.

**Adopted change (2026-11-21, Q-RS-11): restore is confined to `--out-dir`.** Before writing a
record, the Rust port MUST compute `ReplaceAll(name, "@", stem)` and treat the record as
malformed (UnpackFiles returns false, exactly like a bad header) when the result is empty,
`.`, `..`, or contains `/` or NUL. Records before it stay written, and the lookup falls
through to the compile with `object-gone` (§Lookup). The permissions set are `mode & 0777`,
not `mode & 07777`: setuid, setgid and sticky bits are dropped. `PackFiles` only ever writes
a basename and `permissions & 0777`, so no blob ec stored is affected; only a blob some other
client planted in `o/<k2>` is.

The restored `.d` file is byte-identical to the stored one. It is not rewritten for the
current `out_dir` or stem.

---

## Quirks

### Q-RS-1: rustc failures are never cached

**Current behavior**: any non-zero status returns immediately without storing (`:338-341`).
There is no `r/` slot.

**Impact**: a crate that fails deterministically recompiles on every build. This is the
opposite of cc mode.

**Recommendation**: keep as-is.

### Q-RS-2: No `MaskOut` on the key or on stored bytes

**Current behavior**: neither the k1 fields, nor the packed outputs, nor stderr pass through
`Store::MaskOut`/`UnmaskOut`. cc mode masks all three.

**Impact**: an rlib or diagnostic that embeds the package's own `$out` path caches under, and
replays, the old hash. In practice library crates rarely embed `$out`.

**Recommendation**: keep as-is. Applying masking would change keys (bump `rs-schema`).

### Q-RS-3: `# env-dep` lines are not tracked

**Current behavior**: `env!("FOO")` dependencies recorded by rustc as `# env-dep:FOO=…` are
ignored (`:61`, `:350`).

**Impact**: a crate whose output depends on an environment variable other than the five
hashed ones replays a stale result when that variable changes.

**Recommendation**: **ADOPTED (signed off 2026-11-17).** rustc's `env!`/`option_env!` dependencies MUST be
part of the manifest:

- **Parsing.** A dep-info line starting with `# env-dep:` names a variable: `NAME` is the text
  after the prefix up to the first `=` (or the end of the line). The rest is ignored.
- **Manifest line.** After the input lines and before any `!` lines, emit one line per distinct
  `NAME`, in first-seen order: `$env:<NAME>\t<id>`. `<id>` is `unset` if `NAME` is not in ec's
  environment, and otherwise the lowercase hex digest of `Hasher` over `Field("set")`,
  `Field(<value>)`. ec's environment is the one rustc inherited, so it is the value rustc
  saw. The line goes into k2 like every other line (`Field(line)`).
- **Validation.** In a manifest whose k1 text starts with `rs/`, a piece starting with `$env:`
  is an env entry: `NAME` = text up to the first `\t`. Recompute `<id>` from ec's current
  environment and compare it as a byte string. A mismatch fails with `env-changed:<NAME>`.
  These lines are not interpreted in cc manifests (cc keys are unchanged). rustc inputs are
  absolute paths, so `$env:` cannot collide with one.
- **Schema.** k1 field 1 becomes `rs-schema=8`. Entries written by the C++ jig (`rs-schema=7`)
  are not read by the Rust ec, and the reverse.

### Q-RS-4: An empty output blob is stored and replayed as a hit

**Current behavior**: if no dep-info output was readable (relative paths against a different
cwd, or an unusual `--emit=…=path`), `PackFiles` returns `""`, which is stored. A later
`UnpackFiles("")` succeeds, so the hit writes nothing and returns 0.

**Impact**: cargo then finds no rlib. This is unlikely with cargo, which passes absolute
`--out-dir`.

**Recommendation**: change, needs sign-off. Do not store an empty blob.

### Q-RS-5: A comma-separated `--crate-type` evades the link check

**Current behavior**: `links` compares whole values. `--crate-type=lib,cdylib` sets
`has_crate_type` but not `links`, so the invocation is cached.

**Impact**: a cdylib, which embeds absolute store paths, can be replayed into another closure.

**Recommendation**: **ADOPTED (signed off 2026-11-17).** The value of `--crate-type=<v>` (and of the separate
`--crate-type <v>` form, if it is classified the same way) MUST be split on `,`. `links` is set if
any element is `bin`, `proc-macro`, `cdylib` or `dylib`. `--crate-type=lib,cdylib` is therefore
not cached. Keys of cacheable invocations are unchanged.

### Q-RS-6: Glued `-L` stays in the key

**Current behavior**: only `-L x` and `-L=x` are dropped. `-Ldependency=/b/deps` goes through
`TakeCodegenOption` and enters `key_args`.

**Impact**: extra misses when search paths differ. No unsoundness.

**Recommendation**: keep as-is.

### Q-RS-7: `--out-dir`, `-C metadata` and `-C extra-filename` are not in the key

**Current behavior**: by design. Outputs are renamed through the `@` stem, but restored file
contents, including the `.d` and the rlib's embedded crate metadata, are those of the run that
stored them.

**Impact**: the restored `.d` names the original out-dir and stem. The rlib carries the
original `-C metadata` (StableCrateId/symbol hashes). Two dependency graphs that compile an
identical crate with different metadata share an entry.

**Recommendation**: keep as-is. The maintainer should confirm that the metadata hash can never
differ for identical k1 inputs in a way that matters.

### Q-RS-8: An `@` in an output name is rewritten on restore

**Current behavior**: `UnpackFiles` replaces every `@` with the stem, including an `@` that was
in the original filename. With an empty stem, `@` is removed.

**Impact**: none for cargo's names.

**Recommendation**: keep as-is.

### Q-RS-9: Toolchains behind a rustup proxy share keys

**Current behavior**: with rustup, the `rustc` that ec resolves (`OnPath`, usually
`~/.cargo/bin/rustc`) is a symlink to the `rustup` proxy, which picks the real toolchain at
run time from `RUSTUP_TOOLCHAIN` (`cargo +nightly` sets it), a `rust-toolchain.toml`, a
directory override or the default. k1 sees only the proxy:

- `rustc=` is `ToolId(~/.cargo/bin/rustc)`, a path, so it is the same for every toolchain;
- `stdlib=`: `canonical(rustc)` is `~/.cargo/bin/rustup`, so the root is `~/.cargo`, which has
  no `lib/rustlib`. Without `--sysroot` (cargo passes none) no rlib is found and the field is
  empty;
- `RUSTUP_TOOLCHAIN` is not one of the hashed variables.

So `cargo +stable build` and `cargo +nightly build` of the same crate compute the same k1, and
so does a build before and after `rustup update`.

**Impact**: an rlib built by one toolchain is replayed into a build with another. rustc then
refuses it (E0514, "compiled by an incompatible version of rustc"), and the user has to clear
the cache or `cargo clean`. Builds inside Nix are not affected: there `rustc` is a store path
whose `lib/rustlib` holds the stdlib rlibs.

**Recommendation**: change, needs sign-off. Add two k1 fields after `stdlib=`:
`RUSTUP_TOOLCHAIN=<value>` (empty when unset), and `rustc-vV=` followed by the stdout of
`rustc -vV`, run with the invocation's environment and cwd (it names the release, commit hash,
host and LLVM version). The `-vV` result MAY be memoised by the identity of the real tool, but
not by the proxy path alone, since the proxy's answer depends on the toolchain selection.
Running `rustup which rustc` and using its path for `ToolId` and `StdlibIds` is an
alternative. Compatibility: rustc keys change, so `rs-schema` is bumped and existing rustc
entries start cold. cc keys are unchanged. Costs one extra process per cacheable rustc
invocation, unless memoised.

### Q-RS-10: stdout is not stored, and `@argfile` contents are not keyed

**Current behavior**:

- **stdout.** The cached compile runs rustc with stdout inherited and only stderr captured
  (§Compile and store). Nothing written to stdout is stored, and a hit writes only the `e/`
  value to stderr. Flags that make rustc print results to stdout, such as
  `-Zprint-type-sizes`, `-Zunpretty=…` and `-Zprint-mono-items`, are ordinary `key_args`, so a
  cargo library build that passes them is cacheable.
- **`@argfile`.** rustc reads arguments from `@path` (and `@shell:path`), but `ParseRustInvocation` does not expand
  it. `@path` falls through to `TakeCodegenOption` and enters `key_args` as the text `@path`;
  the file's contents are not hashed. When the argfile holds the source, `--out-dir` or
  `--emit`, the invocation is not cacheable (those are not seen), but when they are on the
  command line and the argfile holds only other flags (`--cfg`, `-C`, `--extern`), it is
  cached with a key that ignores those flags.

**Impact**: on a hit, the `-Z` printing flags produce empty output, where a miss printed the
report. With an argfile, changing its contents (another `--cfg`, other `-C opt-level`) still
hits and replays the output of the old flags. cargo writes an argfile only when the command
line is too long, and then it holds every argument, so this rarely matters with cargo.

**Recommendation**: change, needs sign-off. Make an invocation uncacheable if any argument
starts with `@`, or if it carries a `-Z` flag that prints to stdout (at least
`-Zprint-type-sizes`, `-Zunpretty`, `-Zprint-mono-items`), in either the glued or the
`-Z <opt>` spelling. Expanding
argfiles as cc mode does and classifying the expanded arguments is an alternative, at the
cost of another quoting format to specify. Compatibility: fewer hits; keys of the invocations
that stay cacheable are unchanged.

### Q-RS-11: Restore writes wherever the blob says

**Current behavior**: `UnpackFiles` joins the stored name onto `out_dir` as it is. An absolute
name replaces `out_dir`, and a name with `/` or `..` leaves it. The mode is applied as
`mode & 07777`, setuid included.

**Impact**: any client of the shared ecd can `PUT o/<k2>`. A planted blob can then write any
file the building user can write, with any permission bits, on the next hit.

**Recommendation**: **ADOPTED (signed off 2026-11-21).** A name that is not one path
component after the `@` rewriting is a malformed record, and the mode is masked to `0777`
(§Output blob).

---

## Open questions

1. Is the `.d` path `out_dir/<crate_name><extra_filename>.d` correct for every cargo version,
   including `--emit=dep-info=<path>`?
2. Should `staticlib` be cached? It is today (Q-RS-5 aside).
3. Does any consumer rely on stderr being replayed byte-exactly when cargo uses
   `--error-format=json` (paths inside the JSON are not masked)?
