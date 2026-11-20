# ec cache keys

This document specifies everything that goes into an ec cache key: the `Hasher` framing and
digest form, the field sequence of the request key k1 for the C (`cc`) and rustc front ends,
the result key k2 derived from a manifest, the server-side key prefixes, the store-path
handling (`EKACACHE_STORE_IDENTITY`, hash masking, `$out` placeholder), lexical path normalisation
(`Store::Key`), file identities (`Store::InputId`), and the manifest format with its
validation rules. ec MUST produce these keys and manifests bit for bit, so that existing ecd
stores stay valid.

Sources: `jig/src/base.cc`, `jig/src/base.h` (`Hasher`, `HashOf`, `HexEncode`), `jig/src/keys.h`
(`Digest`, `RequestKey`, `ResultKey`, `slot::`), `jig/src/store.h`, `jig/src/store.cc`,
`jig/src/manifest.h`, `jig/src/manifest.cc`, `jig/src/cc_mode.cc` (`ComputeRequestKey`,
`KeyEnvironment`, `ToolchainIds`, `PrimaryIdentity`, `CompileAndStore`, `Lookup`, `Replay`),
`jig/src/rustc_mode.cc` (`RunRustcMode`, `StdlibIds`), `jig/src/dir_mode.cc` (`BlobKey`),
`jig/src/gocache_mode.cc`, `jigd/identity.go`, `jig/src/jig_test.cc`, `spec/twokey.qnt`,
`meson.build` / `meson.options` (`EKACACHE_STORE_DIR`).

Key words MUST, MUST NOT, SHOULD, SHOULD NOT and MAY are as in RFC 2119. Where this document
says "current behavior", that behavior is normative: the Rust implementation MUST reproduce it
unless a Quirk entry's recommendation is accepted by the maintainer.

Related documents: [`ec-cc-mode.md`](ec-cc-mode.md) (which arguments become `key_args`, when
an invocation is cacheable, replay), [`ec-rustc-mode.md`](ec-rustc-mode.md) (rustc argument
parsing, externs, artifact packing), [`ec-cache-client.md`](ec-cache-client.md) (how keys
and values travel to ecd), [`environment.md`](environment.md). The soundness argument for the
two-key scheme is modelled in [`../spec/twokey.qnt`](../spec/twokey.qnt): it holds as long as
the compiler reports missed lookups (`ABSENT_LOG`, see Q-KEY-5) and inputs do not change
between being read and being hashed (`MUTATE_DURING_COMPILE`, see Q-KEY-4).

---

## Overview

```
k1 = H(tool-specific fields ..., primary bytes)          m/<k1>          -> manifest text
k2 = H(k1-text, manifest line 1, manifest line 2, ...)   o/ e/ d/ r/<k2> -> artifacts
```

1. A front end computes k1 (the *request key*) from what the build system asked for.
2. `GET m/<k1>` returns the manifest written by an earlier compile with the same k1.
3. The manifest is validated against the current file system. On success k2 is recomputed
   from the stored lines and the artifacts are fetched under k2.
4. On a miss the compiler runs. Afterwards ec builds a manifest from what the compiler read
   (depfile) and missed (`$EKACACHE_ABSENT_LOG`), computes k2, and stores the manifest under k1
   and the artifacts under k2.

There is exactly one manifest per k1. A later compile with the same k1 overwrites it.

---

## The Hasher

### Digest

`Hasher` wraps one unkeyed BLAKE3 hasher (default mode, no key, no derive-key context)
(`base.cc:258-287`). `Finish()` finalises it into **16 bytes** of output, which are the first
16 bytes of the standard BLAKE3 hash, and returns them as **32 lowercase hex characters**
(`HexEncode`, `"{:02x}"` per byte). `Finish()` does not consume the state.

| Operation | Bytes fed to BLAKE3 |
|---|---|
| `Update(d)` | `d` |
| `Field(d)` | ASCII decimal of `len(d)` in bytes (no sign, no leading zeros, `"0"` for empty), then `":"`, then `d` |

There is no terminator after a field and no length or count for the sequence as a whole. A
k1 or k2 is therefore `hex(BLAKE3(F(f1) ‖ F(f2) ‖ … ‖ F(fn))[0..16])` where
`F(d) = itoa(len(d)) ‖ ":" ‖ d`. Every key in this document is built with `Field` only.

`HashOf(d)` = `Hasher().Update(d).Finish()`: plain BLAKE3-128 of `d` with **no** framing. It is
used for file identities (`C:` ids), dir-mode blob hashes and the ecd `IDS` answer.

### Digest test vectors

Computed with the C++ `libjig.a` and cross-checked with `b3sum -l 16`.

| Input | Digest |
|---|---|
| `Hasher()` (nothing) | `af1349b9f5f9a1a6a0404dea36dcc949` (also asserted in `jig_test.cc:63`) |
| `Field("")` = bytes `0:` | `6dbf06cfd24126fcb3b1e039350b8ead` |
| `Field("a")` = bytes `1:a` | `ceb3ce7a40070f320aa78d363dd9da34` |
| `Field("ab").Field("c")` = bytes `2:ab1:c` | `eafe26c443108bf8d097e6f3af8473a6` |
| `Field("a").Field("bc")` = bytes `1:a2:bc` | `a41516ff4f6e8e7b4f0534c112fc79e3` |
| `Field("x" × 1000)` = bytes `1000:xxx…` | `3f38432ebb6f115e1b974b087518b603` |
| `HashOf("hello")` (no framing) | `ea8f163db38682925e4491c5e58d4bb3` |
| `HashOf("int a;\n")` | `0d937065826369d51c16019b838d8932` |

---

## Key types and prefixes

`RequestKey(tool, digest).text()` is `digest.hex()` for `Tool::kCc` and `"rs/" + digest.hex()`
for `Tool::kRustc` (`keys.h:35`). `ResultKey(digest).text()` is always the bare hex: k2 has no
tool prefix, but because the k1 text (prefix included) is the first field of k2, C and rustc
k2 values cannot coincide except by hash collision.

Server-side keys (the only place the prefixes are written, `keys.h:53-62`, and `dir_mode.cc:36`):

| Key | Written by | Value (before compression, see `ec-cache-client.md`) |
|---|---|---|
| `m/<k1-text>` | cc, rustc | manifest text (§Manifest). For rustc this is `m/rs/<hex>` |
| `o/<k2>` | cc (success) | `MaskOut(object bytes)`: the `-o` output, or the `-E` text |
| `o/<k2>` | rustc (success) | `PackFiles(outputs, extra_filename)`, see `ec-rustc-mode.md` |
| `e/<k2>` | cc | `MaskOut(captured stderr)` (success and cached failure) |
| `e/<k2>` | rustc | captured stderr, unmasked |
| `d/<k2>` | cc, success, only when the build asked for a depfile | `MaskOut(MaskForReplay(depfile text))` |
| `r/<k2>` | cc, deterministic compile failure only | decimal exit status, e.g. `1` |
| `b/<hex>` | `ec cache put-dir` | one file: `Stored(bytes)`; `<hex>` = `HashOf(Stored(bytes))` |
| `go/a/<hex>` | gocacheprog | raw OutputID bytes; `<hex>` = lowercase hex of the base64-decoded Go ActionID |
| `go/o/<hex>` | gocacheprog | the object body; `<hex>` = lowercase hex of the OutputID |
| *verbatim* | `ec cache put/put-dir` | user-chosen key, no prefix added |

Write order on a cc miss (`cc_mode.cc:407-428`): `m/` first, then `r/` + `e/` (failure) or
`o/`, `e/`, `d/` (success). A reader that sees the new manifest before `o/` lands gets
`object-gone` and recompiles. A Rust implementation SHOULD keep this order.

On lookup the cc front end fetches `r/ o/ d/ e/` for k2 in one pipelined `GetMany`, in that
order (`cc_mode.cc:208-214`).

---

## Store paths

### The store directory

`STORE` is the compile-time constant `EKACACHE_STORE_DIR` (meson option `store_dir`, default
`/nix/store`). It is **not** read from the environment. A Rust build MUST make it a
build-time constant with the same default. `IsStorePath(p)` ⇔ `len(p) > len(STORE)` ∧ `p`
starts with `STORE` ∧ `p[len(STORE)] == '/'` (`store.cc:47-49`). `kStoreHashLength = 32`.
`StateDir()` = `STORE` up to its last `/`, + `/var/nix` (`/nix/store` → `/nix/var/nix`).

### Identity modes

`EKACACHE_STORE_IDENTITY` is read once, when the `Store` singleton is first used (in practice at
the start of `main`). Exactly the string `content` selects **content mode**. Anything else,
including unset, empty or `Content`, selects **path mode** (the default).

| | path mode (default) | content mode |
|---|---|---|
| store hashes in keys | verbatim | masked to `*` (`MaskHashes`) |
| store file identity | `S:<path>` (never read) | `C:<blake3-128>` (daemon `IDS` first) |
| `$out` hash in keys and stored bytes | verbatim (`MaskOut` is the identity) | replaced by `eeee…e` (32 × `e`) |
| `$EKACACHE_STORE_ROOTS`, `$out` used for resolving | no | yes |

### MaskHashes

`MaskHashes(text)` (`store.cc:61-72`): let `P = STORE + "/"`. Scan left to right for `P`. At
each occurrence with `h` = the index just after `P`: if `len(text) > h + 32`, `text[h+32] ==
'-'`, and all of `text[h..h+32]` are in the nix base-32 alphabet `0123456789abcdfghijklmnpqrsvwxyz`
(no `e o u t`), replace those 32 bytes with the single byte `*`. Continue scanning at `h + 1`
(in the modified text). Every occurrence is masked, anywhere in the text (inside `-I`, `=`
values, quotes, and so on).

| Input | `MaskHashes` |
|---|---|
| `/nix/store/0123456789abcdfghijklmnpqrsvwxyz-zlib-1.3/include/zlib.h` | `/nix/store/*-zlib-1.3/include/zlib.h` |
| `/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-x` | unchanged (`e` is not base-32) |
| `/nix/store/short-name` | unchanged |
| an already masked `/nix/store/*-name/…` | unchanged |

`MaskForReplay(text)` = `MaskHashes(text)` in content mode, `text` in path mode.

### The `$out` placeholder (MaskOut / UnmaskOut)

At `Store` construction, in content mode only (`store.cc:29-36`): if `$out` is a store path,
longer than `len(STORE) + 1 + 32`, with `-` at index `len(STORE) + 1 + 32`, then `out_hash` =
the 32 bytes after `STORE/`. No alphabet check. Otherwise `out_hash` is empty.

`MaskOut(bytes)` replaces every non-overlapping occurrence (left to right) of
`STORE/<out_hash>-` with `STORE/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-`. `UnmaskOut` does the reverse.
Both are the identity when `out_hash` is empty, which includes all of path mode. The width
is unchanged, so object files stay valid.

### Roots and Resolve

In content mode the constructor builds a map *masked root → real root* from
`split($EKACACHE_STORE_ROOTS, ' ')` (empty pieces dropped) followed by `$out`. Each root is first
cut at the first `/` after index `len(STORE) + 1` (so `STORE/<hash>-name/sub` → `STORE/<hash>-name`).
A root is entered only if `MaskHashes(root) != root`. On duplicate masked names the **first**
one wins (`emplace`). In path mode the map is empty.

`Resolve(p)` (`store.cc:158-168`): if `p` does not start with `STORE + "/*-"`, return `p`
unchanged. Otherwise take `p` up to (not including) the first `/` at or after index
`len(STORE) + 3`, look it up, and return the real root + the rest of `p`, or *none* if the
root is unknown.

`ResolveAll(text)` (`store.cc:170-185`) [C++; the Rust port is the identity in path mode, adopted Q-KEY-1]: `text = MaskHashes(text)` (**in both modes**), then for
every map entry replace each occurrence of the masked root that is followed by end of text,
`/`, or one of `` \t\n\\:"';,)]}>`` with the real root. Used on replayed depfiles and on
`get-dir` text files. In path mode this leaves every store path masked (Q-KEY-1).

### ToolId

`ToolId(path)` (`store.cc:187-192`) = `Key(canonical(dirname(path)) + "/" + basename(path))`;
if `canonical` fails (including an empty dirname for a bare name like `gcc`), `Key(path)`.
The directory is resolved, the last component is not, so `bin/rustc` symlinks in two packages
stay distinct by directory and `rust-bootstrap/bin/rustc` vs `rust/bin/rustc` by name.

---

## Key: lexical normalisation

`Store::Key(arg)` (`store.cc:152-156`) = `MaskForReplay(MaskRandomNames(NormalizePathArg(arg)))`.
It never touches the file system.

### NormalizePathArg

1. If `arg` starts with `-D` or `-U`: return `arg` unchanged.
2. Determine `start`, the offset of the value:
   - `arg` starts with `--`: if it has no `=`, return `arg`; else `start` = index of the first `=` + 1.
   - `arg` starts with `-`, `len(arg) > 2`, `arg[1] != '-'`: `start = 2`. If `arg` contains `=`
     and the part before the first `=` contains no `/`, `start` = index of that `=` + 1.
   - `arg` starts with `-` otherwise (`-`, `-I`, `-c`): return `arg`.
   - otherwise `start = 0`.
3. `value = arg[start..]`. If `value` contains no `/` and does not start with `.`, return `arg`.
4. `n = lexically_normal(value)` with C++ `std::filesystem` POSIX semantics: collapse runs of
   `/` (a leading `//` is **not** special), drop `.` components, drop each `name/..` pair,
   drop `..` directly after the root, keep a trailing `/` only when the original ended in a
   separator or `/.`; an empty result becomes `.`.
5. If `len(n) > 1` and `n` ends with `/`, drop that one `/`. If `n` is empty, `n = "."`.
6. Return `arg[0..start] + n`.

The two-character prefix rule means glued long single-dash options are split after two
bytes: `-iquote./a/../b` normalises the value `quote./a/../b` (Q-KEY-9).

### MaskRandomNames

For each pattern, in this order: (`TryCompile-`, 6), (`cmTC_`, 5). Scan for the prefix. If
fewer than `width` bytes follow, stop scanning for that pattern. If all `width` following bytes
are ASCII alphanumeric, replace each with `#`. Continue after the prefix.

### Key test vectors (path mode unless noted)

| `arg` | `Key(arg)` |
|---|---|
| `-I./include//sub/../` | `-Iinclude` |
| `./src/../src/a.c` | `src/a.c` |
| `--sysroot=/a/b/../c/` | `--sysroot=/a/c` |
| `-DFOO=./a//b` | `-DFOO=./a//b` |
| `-Ufoo/../bar` | `-Ufoo/../bar` |
| `-O2`, `-I`, `-D`, `--param=x`, `foo.c`, `.hidden` | unchanged |
| `.` / `./` / `a/` / `/` | `.` / `.` / `a` / `/` |
| `//x` | `/x` |
| `x/..` / `a/b/../..` | `.` / `.` |
| `../..` / `a/../../b` / `/..` / `/../a` | `../..` / `../b` / `/` / `/a` |
| `-I=/x/../y` | `-I=/y` |
| `-Ia/b=c/../d` | `-Ia/d` |
| `-mfoo=a/b` | `-mfoo=a/b` |
| `--x=` / `--x=./` | `--x=` / `--x=.` |
| `-L./lib` / `-l./x` / `-o./a/../b.o` | `-Llib` / `-lx` / `-ob.o` |
| `-fprofile-use=./p/../q` | `-fprofile-use=q` |
| `-Wl,-rpath,/a/../b` | `-Wl,-rpath,/b` |
| `-Wp,-MD,./x.d` | unchanged |
| `-include./x.h` | unchanged |
| `-iquote./a/../b` | `-iquote./b` |
| `@./rsp` | unchanged |
| `cwd=/a/../b` | `cwd=/b` |
| `/b/TryCompile-teSZ4z/src.c` | `/b/TryCompile-######/src.c` |
| `CMakeFiles/cmTC_7c7e5.dir/x.c.o` | `CMakeFiles/cmTC_#####.dir/x.c.o` |
| `TryCompile-abcdefgh` | `TryCompile-######gh` |
| `cmTC_abcde_cmTC_12345` | `cmTC_#####_cmTC_#####` |
| `-I/nix/store/0123456789abcdfghijklmnpqrsvwxyz-zlib-1.3/include` | path mode: unchanged. Content mode: `-I/nix/store/*-zlib-1.3/include` |

---

## InputId: file identity

`InputId(path)` (`store.cc:204-217`) returns an identity string or *none*:

1. Path mode and `IsStorePath(path)`: `"S:" + path`. The file is not opened, so this
   succeeds even if the path does not exist.
2. If the daemon answered for `path` earlier in this process (`RememberIdentity`, filled by
   `PrefetchIdentities`), that answer.
3. `ReadFile(path)`. On failure (missing, unreadable, a directory) → *none*.
4. `"C:" + HashOf(IsStorePath(path) ? bytes : MaskOut(bytes))`.

`PrefetchIdentities(paths)` (`manifest.cc:94-108`) selects the paths for which
`DaemonMayIdentify` holds: content mode ∧ `IsStorePath(p)` ∧ `p` is not `$out` and not under
`$out/` (when `$out` is non-empty). It sends them in one `IDS` request (none if the list is
empty) and remembers each non-empty answer. An empty answer, or any I/O error, leaves the path
to be hashed locally. ecd answers `"C:" + hex(BLAKE3(file)[0..16])` over the symlink-resolved
file (`jigd/identity.go:83-84`), the same value as step 4 for a store file, or `""` for a path
that is not clean (`filepath.Clean(p) != p`), not under its store, or not a regular file.

| Path (test setup) | Mode | `InputId` |
|---|---|---|
| build-tree file containing `int a;\n` | either | `C:0d937065826369d51c16019b838d8932` |
| `/nix/store/0123…xyz-zlib-1.3/include/zlib.h` (absent) | path | `S:/nix/store/0123456789abcdfghijklmnpqrsvwxyz-zlib-1.3/include/zlib.h` |
| same | content | *none* (file missing) |

---

## k1 field sequences

Each row is one `Field` call, in order. `Key`, `MaskOut`, `MaskForReplay` and `InputId` are as
defined above. Which invocations are cacheable at all, and the exact contents of `key_args`,
are specified in `ec-cc-mode.md` and `ec-rustc-mode.md`.

### cc (`ComputeRequestKey`, `cc_mode.cc:98-128`)

`links` = link-one (configure-style compile-and-link of one source) or link (objects only).

| # | Field | When |
|---|---|---|
| 1 | `"cc=" + ToolId(compiler)` | always. `compiler` = `ekacache.json`'s `cc`, `$EKACACHE_CC`, or the launcher's program path, as given (not resolved on `PATH`) |
| 2 | `"cwd=" + Key(current_path())` | always |
| 3 | `"src=" + Key(MaskOut(source))` | not a pure link. `source` as spelled on the command line |
| 4 | `"mode=compile"` \| `"mode=link-one"` \| `"mode=link"` | always |
| 5 | `"LIBRARY_PATH=" + MaskForReplay($LIBRARY_PATH)` | `links` |
| 6 | `"env=" + MaskForReplay(MaskOut(KeyEnvironment(environ, links)))` | always |
| 7… | for each `a` in `key_args`: `links ∧ (a contains "dynamic-linker" ∨ a contains "rpath")` ? `MaskOut(a)` : `Key(MaskOut(a))` | one field per argument |
| last | `MaskOut(primary)` | always |

`KeyEnvironment(entries, links)` (`cc_mode.cc:556-580`): from the process environment, as
`NAME=value` strings in `environ` order (name = up to the first `=`), keep an entry if its
name is one of `CPATH`, `C_INCLUDE_PATH`, `CPLUS_INCLUDE_PATH`, `OBJC_INCLUDE_PATH`,
`GCC_EXEC_PREFIX`, `COMPILER_PATH`, or if `links` and the name starts with `NIX_` and is not
one of `NIX_BUILD_TOP`, `NIX_BUILD_CORES`, `NIX_LOG_FD`, `NIX_ATTRS_JSON_FILE`,
`NIX_ATTRS_SH_FILE`, `NIX_SSL_CERT_FILE`, `NIX_REMOTE`, `NIX_PATH`, `NIX_PROFILES`,
`NIX_USER_PROFILE_DIR`. Sort the kept **whole entries** bytewise and join them, each followed
by `\n`. Because whole entries are compared, `NIX_LDFLAGS=…` sorts before `NIX_LDFLAGS_X=…`
(`=` is 0x3D, `_` is 0x5F) and `CPATH=` before `C_INCLUDE_PATH=`. With none kept the field is
`env=`.

`primary` (`PrimaryIdentity`, `cc_mode.cc:167-190`, then `cc_mode.cc:697`):

- compile / link-one: the source file's bytes, then for each `-include-pch` file
  `"\npch=" + InputId(pch)`, then `"\ntoolchain=" + T`.
- link: for each object/archive input in command-line order `input + "=" + InputId(input) + "\n"`
  (input spelled as given), then `"\ntoolchain=" + T`.

`T` (`ToolchainIds`, `cc_mode.cc:131-164`): the set {`canonical(OnPath(compiler))`, and for
`links` `canonical(v)` for every `--ld-path=v` argument}, each extended with every regular,
non-symlink file in `dirname(dirname(canonical tool))/lib` whose name starts with `libLLVM`,
`libclang-cpp` or `liblld`. Tools whose `canonical` fails are skipped. Sort bytewise, remove
duplicates, then concatenate `InputId(f)` (or `?` when *none*) + `,` for each. An empty set
gives `""`.

There is no schema/version field in cc keys (Q-KEY-10).

### rustc (`RunRustcMode`, `rustc_mode.cc:298-318`)

| # | Field |
|---|---|
| 1 | `"rs-schema=7"` |
| 2 | `"rustc=" + ToolId(OnPath(rustc))` |
| 3 | `"stdlib=" + StdlibIds` (see `ec-rustc-mode.md`: `InputId` + `,` of every `libstd-*.rlib` / `libcore-*.rlib` under the rustc root's and `--sysroot`'s `lib/rustlib/*/lib`, sorted; `?` if rustc cannot be resolved) |
| 4 | `"cwd=" + Key(current_path())` |
| 5 | `"src=" + Key(source)` |
| 6… | `Key(a)` for each `a` in `key_args` |
| … | `"extern:" + (InputId(rlib) or "?")` for each `--extern` rlib, in order |
| … | `"CARGO_PKG_NAME=" + $CARGO_PKG_NAME`, then `CARGO_PKG_VERSION`, `CARGO_CFG_TARGET_FEATURE`, `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`, same form (unset = empty) |
| last | the crate root's bytes (no `MaskOut`) |

The digest is wrapped as `"rs/" + hex`. Unlike cc, rustc applies no `MaskOut` to fields or
stored stderr.

### Composite test vector

`Field` sequence `cc=/nix/store/*-gcc-14.3.0/bin/gcc`, `cwd=/build/source`, `src=a.c`,
`mode=compile`, `env=`, `-c`, `-O2`, `int main(){}\n\ntoolchain=C:00000000000000000000000000000000,`
→ k1 = `544ade1ae7d1ab515b12eaba8c82aca5`. The same fields wrapped as rustc would print
`rs/<that hex>`. Example of the prefix: digest `e670e1a0104f0d5abffd4c4fe35ada39` gives the cc
text `e670e1a0104f0d5abffd4c4fe35ada39`, the rustc text `rs/e670e1a0104f0d5abffd4c4fe35ada39`,
and the manifest key `m/rs/e670e1a0104f0d5abffd4c4fe35ada39`.

---

## Manifest

### Format

A manifest is UTF-8-agnostic bytes, a sequence of lines each terminated by `\n`:

| Line | Meaning |
|---|---|
| `<Key(path)>\t<InputId>` | an input the compiler read, with its identity at manifest time |
| `!<Key(path)>` | a path the compiler looked up and did not find |

All input lines come first, then all `!` lines. `Key` masks store hashes in content mode, so
the stored paths are build-independent there.

### BuildManifest (`manifest.cc:110-140`)

Inputs: k1, the `inputs` list (cc: prerequisites of the first depfile rule, then for links
the `--dependency-file` entries minus the driver's temp objects; rustc: dep-info inputs), the
`primary_source` string, and the `absent` list (lines of `$EKACACHE_ABSENT_LOG`).

1. `PrefetchIdentities(inputs)`.
2. Start a `Hasher` with `Field(k1-text)` (the `rs/` prefix included for rustc).
3. For each `path` in `inputs`, in order: skip it if `path == primary_source` (exact string
   compare); if `InputId(path)` is *none*, skip it silently (Q-KEY-3; the Rust port instead marks the manifest unstorable, adopted); else emit
   `Key(path) + "\t" + id`. Inputs are **not** deduplicated.
4. For each `path` in `absent`, in order: skip it if empty, if `IsStorePath(path)`, or if it
   exists now (`std::filesystem::exists`, relative to the cwd; errors count as absent). Emit
   `"!" + Key(path)` unless that exact line was already emitted in this step.
5. Emitting a line appends `line + "\n"` to the text and calls `Field(line)` (without `\n`).
6. k2 = `Finish()`.

### ValidateManifest (`manifest.cc:142-170`)

1. Split the stored text on `\n`, dropping empty pieces. A piece starting with `!` is an absent
   entry, with `path = Resolve(piece[1..])` or `""` if unresolved. Otherwise, a piece with a
   `\t` is an input entry, with `path = Resolve(piece[..first tab])` or `""`. Other pieces are
   ignored, also for k2.
2. `PrefetchIdentities` of all input entries' paths.
3. `Field(k1-text)`, then for each entry in order:
   - absent: if `exists(path)`, fail with `appeared:` + the stored path (`piece[1..]`).
     An unresolved path is `""`, which never exists, so it passes.
   - input: if `InputId(path)` is *none* or differs from `piece[first tab + 1..]` as a byte
     string, fail with `inputs-changed:` + the stored path (`piece[..first tab]`). An
     unresolved path gives `InputId("")` = *none* in content mode, so it fails.
   - `Field(piece)`.
4. Return k2 = `Finish()`. The first failure ends validation.

`FindResult` (`manifest.cc:172-178`): `GET m/<k1>`. No value (miss, I/O error, bad frame) →
`new-key`. Otherwise `ValidateManifest`. The cc and rustc front ends put the error string in
their `$EKACACHE_LOG` subject (`<source> new-key`, `<source> inputs-changed:<path>`, …). The C++
cc front end adds `object-gone` when the manifest validates but an artifact is missing.

k2 is recomputed from the **stored** lines, so the paths hashed are the masked ones and the
identities are the stored ones: two content-mode builds with different store hashes compute
the same k2.

### Manifest test vectors

Setup (content of `/tmp/jigprobe/t`): `a.h` = `int a;\n`, `b.h` = `int b;\n`, `src.c` exists,
`missing.h` and `early/` do not. k1 text = `HashOf("k1")` = `ffb4332755be52674d663ce9586917c9`.
`inputs` = [`t/src.c`, `t/a.h`, `t/./b.h`, `t/missing.h`, `t/a.h`] (all prefixed
`/tmp/jigprobe/`), `primary_source` = `/tmp/jigprobe/t/src.c`, `absent` =
[`t/early/a.h`, `""`, `t/early/a.h`, `t/a.h`, `/nix/store/1123456789abcdfghijklmnpqrsvwxyz-glibc-2.40/include/x.h`, `rel/q.h`].
Both modes give:

```
/tmp/jigprobe/t/a.h\tC:0d937065826369d51c16019b838d8932\n
/tmp/jigprobe/t/b.h\tC:709be0706c9c4004a3ae4d9c20050e55\n
/tmp/jigprobe/t/a.h\tC:0d937065826369d51c16019b838d8932\n
!/tmp/jigprobe/t/early/a.h\n
!rel/q.h\n
```

k2 = `ca06f7cce6259fa171f39edb0e29b4a8`. Validation of the same text right away returns the
same k2; with a trailing `junk line\n\n` appended still the same k2; after creating
`t/early/a.h`: `appeared:/tmp/jigprobe/t/early/a.h`; after editing `b.h`:
`inputs-changed:/tmp/jigprobe/t/b.h`. In content mode with no root for `*-other`:
`!/nix/store/*-other/y.h\n` alone validates (k2 = `7eb18b0a88c25e4e28595f120f4ba2da`), and
`/nix/store/*-other/x.h\tC:00\n` fails with `inputs-changed:/nix/store/*-other/x.h`.

Pure `Hasher` vectors with k1 text `0123456789abcdef0123456789abcdef`:

| Lines | k2 |
|---|---|
| none | `5573430e23fe9fc1644dd647cc911c2f` |
| `include/a.h\tC:0d937065826369d51c16019b838d8932`, `!early/a.h` | `f445848e6f30d9745cdb88c0744c7859` |
| same lines, k1 text `rs/0123456789abcdef0123456789abcdef` | `11c6c31720976272d3ac6ab556847c54` |

---

## Quirks

### Q-KEY-1: Path mode masks store paths in replayed depfiles and `get-dir` text

**Current behavior**: `ResolveAll` calls `MaskHashes` unconditionally, but the root map is
populated only in content mode. In path mode (the default) a replayed depfile
(`cc_mode.cc:276`) therefore names `/nix/store/*-glibc-2.40/include/stdio.h` instead of the real
path, although the stored `d/` value was not masked. Verified with the C++ library:
`ResolveAll("x.o: x.c /nix/store/0123…xyz-zlib-1.3/include/zlib.h")` returns the `*-` form in
path mode. `ec cache get-dir` has the same effect on text files (`dir_mode.cc:71-76`, where
`Stored` also masks unconditionally).

**Impact**: on a hit, make sees prerequisites that do not exist. Without `-MP` the next make run
stops with "No rule to make target"; with `-MP` or ninja the object is considered out of date
and recompiled (a cache hit again). Since the system headers enter the depfile on purpose
(`-sys-header-deps`, `-MMD`→`-MD`), almost every replayed depfile is affected.

**Recommendation**: **ADOPTED (signed off 2026-11-17).** In **path mode** (`EKACACHE_STORE_IDENTITY` unset or
`path`) the Rust port MUST NOT mask text that is handed back to the build or stored as a blob:
`ResolveAll(text)` returns `text` unchanged, and `Stored(bytes)` (put-dir) returns `bytes`
unchanged. **Content mode** is unchanged (mask on store, resolve on replay). Consequences: cc
`d/` values were never masked in path mode, so cc keys and stored bytes do not change; only the
replayed depfile does (it now names real paths). Path-mode `put-dir` blob hashes (`b/<hash>`)
change for text files that contain store paths, so those blobs miss once after the switch.

### Q-KEY-2: Path-mode store identities are existence-blind

**Current behavior**: in path mode `InputId` returns `S:<path>` for any store path without
opening it, also during validation.

**Impact**: a manifest validates even if a store input is absent in this build's sandbox. In
practice the k1 arguments pin the same store paths, so this needs an unusual closure.

**Recommendation**: keep as-is.

### Q-KEY-3: Unreadable inputs are silently omitted from the manifest

**Current behavior**: `BuildManifest` drops an input whose `InputId` is *none* (deleted after
the compile, unreadable, a directory) without logging or refusing to store.

**Impact**: the entry is under-constrained: a later build with any content at that path, or none,
can hit.

**Recommendation**: **ADOPTED (signed off 2026-11-17).** If any input in step 3 of BuildManifest has `InputId`
*none*, the manifest is **unstorable**: the front end (cc or rustc) MUST still forward the
compiler's result unchanged, MUST NOT PUT the manifest or any artifact for this invocation,
and logs the subject `<source> not-cached:unreadable:<Key(path)>` (first such path) in its
`$EKACACHE_LOG` format. The primary source and absent paths are unaffected. Storable manifests are
byte-identical to the C++ ones, so k2 does not change.

### Q-KEY-4: TOCTOU, input identities are hashed after the compile

**Current behavior**: identities are computed in `BuildManifest`, after the compiler exited.
The `__DATE__`/`__TIME__` scan of build-tree inputs also happens afterwards.

**Impact**: an input edited between the compiler reading it and ec hashing it is recorded with
the new identity next to the old output (`twokey.qnt`, `MUTATE_DURING_COMPILE = true` violates
`sound`).

**Recommendation**: keep as-is (closing it would need the compiler to report identities).
Document it for users.

### Q-KEY-5: Absent tracking only with the patched clang

**Current behavior**: `!` lines come only from `$EKACACHE_ABSENT_LOG`, which only the project's
patched clang writes. ec sets the variable for every compiler, but stock GCC and stock clang
ignore it, so their manifests have no `!` lines.

**Impact**: with stock compilers a header that later appears earlier on the search path (for
example a generated `config.h` that shadows a system one) is not noticed, and the stale object
is replayed (`twokey.qnt`, `ABSENT_LOG = false` violates `sound`). This is a soundness caveat
for the GCC and launcher paths.

**Recommendation**: keep as-is, but document it. Consider refusing to cache when no absent log
was produced (needs sign-off).

### Q-KEY-6: Absent lookups inside the store are dropped, also in content mode

**Current behavior**: `BuildManifest` skips absent paths under the store ("store paths stay
absent").

**Impact**: sound in path mode, where k1 pins exact store paths. In content mode a store root
is identified by name only, so a rebuilt `*-zlib-1.3` that gained a header earlier on the search
path is not noticed.

**Recommendation**: change, needs sign-off (record masked store absences in content mode).

### Q-KEY-7: Manifest paths are normalised lexically and masked

**Current behavior**: manifest lines store `Key(path)`. Lexical `..` removal is wrong across
symlinked directories, so validation may read another file than the compiler did. The
`TryCompile-`/`cmTC_` masking turns paths into names that do not exist: such an input line never
validates (`inputs-changed:`), and such a `!` line always passes. A path that starts with `-`
is treated as an option by `NormalizePathArg`.

**Impact**: rare false hits through symlinked `..` paths, and permanent misses for headers inside
cmake scratch directories.

**Recommendation**: keep as-is (k2 depends on it).

### Q-KEY-8: Duplicates, separators and junk in manifests

**Current behavior**: input lines are not deduplicated (an input listed twice produces two
identical lines, both hashed). Absent lines are deduplicated. A path containing `\t` or `\n` is
split wrongly on validation. Lines that are neither `!…` nor contain a tab are skipped, also
for k2.

**Impact**: none in practice. A port that deduplicates inputs computes a different k2.

**Recommendation**: keep as-is.

### Q-KEY-9: Glued single-dash options split after two bytes

**Current behavior**: `-iquote./a/../b` keys as `-iquote./b`, and `-include./x.h` is not
normalised at all, because the value is taken to start at byte 2.

**Impact**: two spellings of one option can produce different keys (extra misses only).

**Recommendation**: keep as-is.

### Q-KEY-10: cc keys carry no schema version

**Current behavior**: rustc keys start with `rs-schema=7`, cc keys have no version field.

**Impact**: any change to cc key rules can make the new code look up entries written under
the old rules, if the field strings happen to coincide.

**Recommendation**: keep as-is for the drop-in port. Add a `cc-schema=` field only together
with a deliberate cache flush (needs sign-off).

### Q-KEY-11: `ToolId` of a bare compiler name is not resolved

**Current behavior**: with `EKACACHE_CC=gcc`, `ToolId("gcc")` is `gcc`, because `canonical("")`
fails. rustc resolves via `OnPath` first. The `toolchain=` part of the primary field does use
`OnPath`, so two different `gcc`s still get different keys through their `InputId`.

**Impact**: none for correctness. The `cc=` field is less informative.

**Recommendation**: keep as-is.

### Q-KEY-12: Duplicate masked roots: first wins

**Current behavior**: if `$EKACACHE_STORE_ROOTS` holds two roots with the same name and different
hashes, `Resolve` maps the masked name to the first one listed.

**Impact**: validation and depfile replay use the first one.

**Recommendation**: keep as-is.

---

## Open questions

1. **Absent-log format.** The patched clang is not in this repository. ec reads the file as
   `\n`-separated paths (`cc_mode.cc:358-359`). Are the paths absolute, or relative to the
   cwd as looked up? Is there any escaping?
2. **Q-KEY-1 intent.** Is the path-mode depfile masking a known trade-off or a bug? The answer
   decides whether the Rust port reproduces it.
3. **rustc primary exclusion.** `BuildManifest` drops the crate root only when the dep-info
   spelling equals `absolute(source).lexically_normal()`. This relies on rustc writing absolute
   paths. Confirm in `ec-rustc-mode.md`.
4. **Daemon/client store-dir mismatch.** ecd's `NIX_STORE_DIR` and ec's compile-time
   `EKACACHE_STORE_DIR` are configured separately. A mismatch silently turns every `IDS` answer
   into `""`, which costs only speed. Should either side check it?
