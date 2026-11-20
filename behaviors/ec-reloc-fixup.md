# ec reloc-fixup mode

`reloc-fixup` rewrites every ELF64 little-endian executable or shared object, and every 64-bit little-endian Mach-O image, under an output tree in place, so the tree still loads after it is copied anywhere. On ELF it makes RUNPATH entries `$ORIGIN`-relative, turns NEEDED entries into direct `$ORIGIN/...` paths when they fit, and hands PT_INTERP to a stub linked in by the driver. On Mach-O it respells dylib/rpath load commands `@loader_path`-relative and refreshes an ad-hoc code signature. Every regular file in the tree is also checked for forbidden ("denied") store hashes. It never grows a file: everything it writes goes into slack that the link driver set aside on purpose.

Sources: jig/src/fixup_mode.h, jig/src/fixup_mode.cc, jig/src/elf_fixup.cc, jig/src/macho_fixup.cc, jig/src/main.cc, jig/src/store.h, jig/src/store.cc, jig/src/base.cc, jig/src/driver.h, jig/src/driver.cc, jig/src/jig_test.cc

## Overview

- The mode runs when the binary is invoked under the basename `reloc-fixup`, or as `ec reloc-fixup ...` (see behaviors/ec-dispatch.md).
- It walks `<prefix>` once and collects every regular, non-symlink file. It then processes the files one at a time, in walk order: deny check, then ELF fixup, and the Mach-O fixup only if the file is not ELF64-LE.
- `<prefix>` is where the tree is now, on disk. `--dest` is where the tree will finally live, and it MUST be a store path. All relative spellings are computed from the *final* locations (the `Final` mapping); all existence checks look at the *on-disk* locations (the `OnDisk` mapping).
- Results:
  - **stdout:** one summary line for each file actually rewritten, plus `WARN` lines for ELF files that still contain an absolute store path.
  - **stderr:** diagnostics.
  - **Exit status:** 0 if no file had an error, 1 otherwise, 2 for invocation errors.
- No environment variable affects this mode. The `Store` singleton is built in `main`, but only its compile-time store directory is used here.

### Interaction with the link driver (informative; the driver is specified in behaviors/ec-driver-policy.md)

The driver (`jig/src/driver.cc`) shapes ELF and Mach-O links so that this mode can rewrite them in place:

| Driver convention | Constant / flag | What reloc-fixup does with it |
|---|---|---|
| RUNPATH = build-system rpaths verbatim, then each driver store dir spelled `<dir>/.`, then one pad entry `/` + `_` × (max(stores,1)×48 + libs×80 − 1) | `kRunpathSlack = 48` per store entry, `kNeededSlack = 80` per `-l` | The pad entry is not a store path, so A5.4 drops it. Its bytes then hold the shorter `$ORIGIN` spellings and the direct NEEDED strings (A5.5). The `/.` suffix normalizes to a trailing `/` (see Q-FIX-1). |
| `-Wl,--enable-new-dtags` | — | Produces DT_RUNPATH. DT_RPATH is handled identically. |
| Executables with a configured `crt`: the crt object linked in, `-Wl,--dynamic-linker=<libc>/lib/` + `./` × 12 + `<interp>`, `-Wl,--export-dynamic-symbol=__reloc_start` | `kInterpSlack = 12` | `__reloc_start` lands in `.dynsym`, which is where A5.8 looks for it. The 24 bytes of `./` pairs are slack for the prefix-relative interpreter path written by A5.9. |
| Executables without `crt`: `-Wl,--dynamic-linker=<libc>/lib/<interp>` | — | No stub, so PT_INTERP is kept (`kept (no stub)`) and the `WARN` line normally fires. |
| Entry stub installed by an external `formatelf --set-entry-stub` (reloc_stub.bin) | 16-byte header beginning `RELOCSTB` directly before `e_entry` | A5.8 accepts `e_entry` itself as the stub entry. |
| Mach-O links: `-Wl,-headerpad_max_install_names` | — | Gives the header room that A6.2/A6.5 grow load commands into. |
| RUNPATH always contains `<libc>/lib` | — | A5.5 "libc detection": a dir containing `libc.so.6` keeps serving its NEEDED by soname. |

### Conventions used in this document

**Store.** `S` is the compile-time store directory (`EKACACHE_STORE_DIR`, e.g. `/nix/store`; see behaviors/environment.md). `IsStorePath(p)` is true iff `len(p) > len(S)`, `p` starts with `S`, and `p[len(S)] == '/'`. So `S + "/"` itself counts as a store path. `kStoreHashLength = 32`.

**Integers.** ELF fields and Mach-O header/load-command fields are read and written **little-endian**. Code-signature blob fields are **big-endian**. All offset arithmetic is unsigned 64-bit and wraps modulo 2^64. Where a u32 is subtracted from a 64-bit size, the u32 is widened first.

**Image primitives.** The file is held in memory as a byte string `B` of length `L`.

- `Read<T>(off)` fails iff `off > L` or `L − off < sizeof(T)`. Otherwise it returns the bytes as T.
- `Write<T>(off, v)` follows the same rule and returns false on failure. Every caller below ignores that result unless stated otherwise.
- `CString(off)`: `""` if `off ≥ L`; otherwise the bytes from `off` up to, but not including, the first NUL, or to the end of `B`.
- `WritePadded(off, cap, text)` fails iff `len(text)+1 > cap`, or `off > L`, or `L − off < cap`. Otherwise it sets `B[off .. off+cap)` = `text` followed by `cap − len(text)` NUL bytes.
- `Overwrite(off, data)` fails iff `off > L` or `L − off < len(data)`. Otherwise it replaces `B[off .. off+len(data))`.

**Path operations (POSIX, byte strings; these reproduce C++ `std::filesystem` lexical semantics exactly).**

- *Elements* of a non-empty path: `"/"` if it begins with `/`; then each maximal run of non-`/` bytes; then a final empty element `""` if the path ends with `/` and has at least one non-root element. `"."` and `".."` are ordinary elements here.
- `join(a, b)`:
  1. If `b` begins with `/`, the result is `b`.
  2. Otherwise, if `a` is non-empty and does not end with `/`, the result is `a + "/" + b`.
  3. Otherwise the result is `a + b`.
  4. Consequence: `join("a", "")` = `"a/"`.
- `normalize(p)` (C++ `lexically_normal`):
  1. If `p` is `""`, return `""`.
  2. Set `abs = p begins with "/"`, `stack = []`, `trail = false`.
  3. For each run of non-`/` bytes `c` in order, and finally once more with `c = ""` if `p` ends with `/`:
     - `c == "."` or `c == ""`: set `trail = true`.
     - `c == ".."` and `stack` is non-empty with top ≠ `".."`: pop, `trail = true`.
     - `c == ".."` and `stack` is empty and `abs`: ignore.
     - `c == ".."` otherwise: push `".."`, `trail = false`.
     - anything else: push `c`, `trail = false`.
  4. If `stack` is non-empty and its top is `".."`, set `trail = false`.
  5. Result = (`"/"` if `abs`) + `stack` joined with `/` + (`"/"` if `trail` and `stack` non-empty). If that is empty, the result is `"."`.
  6. Examples: `/a/b/..` → `/a/`, `/x/lib/.` → `/x/lib/`, `a/..` → `.`, `a/../../b` → `../b`, `//a//b//` → `/a/b/`.
- `relative(p, base)` (C++ `lexically_relative`; neither argument is normalized):
  1. If exactly one of the two begins with `/`, return `""`.
  2. Compare the element lists and let `i` be the first index where they differ.
  3. If both lists end at `i`, return `"."`.
  4. Let `n` = Σ over base elements from `i` on: `".."` → −1, `""` or `"."` → 0, anything else → +1.
  5. If `n == 0` and (p has no element at `i`, or that element is `""`), return `"."`.
  6. If `n < 0`, return `""`.
  7. Otherwise start from `""` and `join` `".."` `n` times, then each remaining element of `p` from `i` on. The trailing `""` element adds a trailing `/`.
  8. Examples: `relative("/a/b/c/", "/a/x")` = `../b/c/`, `relative("/a/b", "/a/./b")` = `../b`, `relative("a", "/a")` = `""`.
- `filename(p)`: the last element if it is a non-root, non-empty element; otherwise `""`.
- `parent_path(p)`: `p` without its last element and the separator run before it (`/a` → `/`, `a` → `""`).
- *Path comparison* (used for sorting and for `!=`): lexicographic over element lists, comparing elements bytewise (unsigned); a proper prefix sorts first. So `/a/b/c` < `/a/b-c`, `/a//b` equals `/a/b`, and `/a/b` ≠ `/a/b/`.
- `rel(p)`: `p` relative to `<prefix>`, after both are resolved to canonical absolute form (C++ `fs::relative`, i.e. `weakly_canonical` on both, symlinks resolved). For files produced by the walk this is the walk-relative path, e.g. `bin/foo`. It is used in every "shown" name.
- `Final(p)`:
  1. `r = relative(normalize(p), prefix)`.
  2. If `r == ""`, or `r` starts with the two bytes `..` (string prefix test), return `p` **verbatim**, not normalized.
  3. Otherwise return `normalize(join(dest, r))`.
- `OnDisk(p)`: the same as `Final` with `dest` and `prefix` swapped.
- `RelativeTo(dir, target, anchor)`:
  1. `r = relative(normalize(target), dir)`. `dir` is not normalized.
  2. If `anchor == ""`, return `r`.
  3. Otherwise, if `r == "."`, return `anchor`.
  4. Otherwise return `anchor + "/" + r`. This includes `r == ""`, which yields `anchor + "/"`.

## Invocation

```
reloc-fixup <prefix> [--dest <store path>] [--sdk <dir>] [--deny <store path>]...
ec reloc-fixup <prefix> [...]
```

`args` are the arguments after the mode word.

1. If `args` is empty, write the following line to stderr and exit **2**:
   ```
   usage: reloc-fixup <prefix> [--dest <store path>] [--sdk <dir>] [--deny <store path>]...
   ```
2. `prefix = normalize(args[0])` and `dest = prefix`. `args[0]` is always the prefix, even if it looks like a flag.
3. Set `i = 1`. While `i + 1 < len(args)`: let `a = args[i]` and `v = args[i+1]`.
   - `a == "--dest"`: `dest = normalize(v)`. The last occurrence wins.
   - `a == "--sdk"`: `sdk = v` verbatim, not normalized. The last occurrence wins.
   - `a == "--deny"`: append `filename(v)[0:32]` (the first 32 bytes, or fewer) to `denied`. This accumulates in order.
   - anything else: write `reloc-fixup: unknown argument <a>` to stderr and exit **2**.
   - Then `i += 2`.
   - A final unpaired argument is silently ignored (Q-FIX-2).
4. If `!IsStorePath(dest)`, write `reloc-fixup: <dest> is not under <S>` to stderr and exit **2**. `<dest>` is the normalized dest.
5. Run A1. Exit **0** if the error counter is 0, otherwise **1**.

Exit codes:

| Code | Meaning |
|---|---|
| 0 | Every file processed without error. This includes a prefix that does not exist or is not a directory (Q-FIX-17). |
| 1 | At least one error was counted (any message marked "error" in *Errors and messages*). |
| 2 | Usage error, unknown argument, or dest not under the store. Nothing was walked. |
| abnormal (SIGABRT) | Malformed input or filesystem errors that hit an uncaught C++ exception (Q-FIX-10). |

Flags are only recognized at odd positions 1, 3, 5, … A flag spelled `--dest=x` is an unknown argument (if it has a successor).

## Algorithms

### A1. Top level

1. `errors = 0`.
2. Walk the tree (A2), giving `files` (in walk order) and `own_lib_dirs`.
3. For each file in `files`, in order, run A3.
4. Return `errors`.

### A2. Tree walk and own_lib_dirs

1. Walk `prefix` recursively, depth-first pre-order: when a directory entry is yielded, its contents are visited before its later siblings.
   - Within a directory, entries come in `readdir(3)` order; `.` and `..` are skipped.
   - Directory symlinks are not followed.
   - Directories that cannot be opened with EACCES are skipped silently.
   - If `prefix` itself cannot be opened (missing, not a directory, …), the walk yields nothing, without an error.
   - Each entry's path is `join(<parent path>, <name>)`, starting from `prefix` as normalized in Invocation step 2.
2. For each entry: skip it if it is a symlink (judged by lstat) or not a regular file. Otherwise:
   1. Append the path to `files`.
   2. Let `name = filename(path)`. If (`name` contains the substring `.so` anywhere, or `name` ends with `.dylib`) **and** the string `parent_path(path)` does not contain the substring `/debug` anywhere, insert `parent_path(path)` into the set `lib_dirs`.
3. `own_lib_dirs` = `lib_dirs`, deduplicated and sorted by *path comparison*, not plain string order.
4. The file list is complete before any file is modified; temporary files created later are never visited.

### A3. Per-file processing

For a file at `path`:

1. If the extension of `filename(path)` is exactly `.debug`, skip the file entirely: no read, no deny check.
   - The extension is the part from the last `.`, but a name whose only dot is its first byte (e.g. `.debug`) has no extension.
2. Read the whole file. If reading fails, skip it silently.
3. If `rel(path)` is neither `etc/ekacache.json` nor `etc/roots` (path comparison), run the deny check (A4) on the original bytes.
4. Run the ELF fixup (A5). If it returns "not mine", run the Mach-O fixup (A6).
   - A5 returns "not mine" when the file is not ELF64-LE, and also after a failed write-back. In the second case A6 then rejects the ELF magic and does nothing.

### A4. Deny check

For each `h` in `denied`, in order:

1. Find the **first** occurrence of the bytes `h` in the file. If there is none, continue with the next `h`.
2. `shown` = up to 100 bytes starting at that occurrence, cut before the first control byte (0x00–0x1F or 0x7F; the C locale `iscntrl`; bytes ≥ 0x80 are not control bytes).
3. Write `reloc-fixup: <rel(path)> refers to build-platform <shown>` to stderr. `errors += 1`.

One message per denied entry that occurs (duplicated `--deny` values give duplicated messages). An empty `h`, from a `--deny` ending in `/`, matches at offset 0 of every file (Q-FIX-3). The check runs on every readable non-`.debug` file, not only binaries, and before any rewrite.

### A5. ELF fixup

#### A5.1 Acceptance

1. The file is ELF64-LE iff it starts with `7F 45 4C 46`, `L > 5`, `B[4] == 2` (ELFCLASS64), and `B[5] == 1` (ELFDATA2LSB). Otherwise return "not mine".
2. Read the 64-byte `Elf64_Ehdr` at 0. If the read fails, or `e_type` is neither 2 (ET_EXEC) nor 3 (ET_DYN), return "mine" with nothing done: no output, no WARN.
   - `e_machine` is not checked.
3. Read the section table (A5.2). `log = [rel(path)]`, `dirty = false`.
4. Run A5.3–A5.7 (RUNPATH). If that fails, return "mine" immediately: nothing is written and there is no WARN.
5. Run A5.9 (INTERP). If that fails, return "mine" immediately: nothing is written, the RUNPATH edits are discarded, and there is no WARN.
6. Run A5.10.

#### A5.2 Section table

For `i` in `0 .. e_shnum`:

1. Read a 64-byte `Elf64_Shdr` at `e_shoff + i × e_shentsize`.
2. Stop at the first failed read.
3. Keep `sh_type`, `sh_offset`, `sh_size`, `sh_link`, `sh_entsize`.

`sections.len` is the number actually read. Extended section numbering (`e_shnum == 0`) is not supported, so it yields no sections.

`SectionEnd(s)` = 0 if `s.offset ≥ L`, else `s.offset + min(s.size, L − s.offset)`.

#### A5.3 Dynamic information

1. `dyn` = the first section with `sh_type == 6` (SHT_DYNAMIC). If there is none, or `dyn.link ≥ sections.len`, or `sections[dyn.link].sh_type == 8` (SHT_NOBITS), there is no RUNPATH; A5 continues at A5.9.
2. `dynstr = sections[dyn.link].offset`.
3. For `off = dyn.offset`, while `off < SectionEnd(dyn)`, step 16: read `Elf64_Dyn`. Stop on a failed read or `d_tag == 0` (DT_NULL). Then:
   - `d_tag == 1` (DT_NEEDED): append `Needed{name = CString(dynstr + d_val), val_offset = off + 8, vn_files = []}`.
   - `d_tag == 29` (DT_RUNPATH) or `d_tag == 15` (DT_RPATH): `runpath_offset = dynstr + d_val`. The last such entry wins; the tag itself is never changed.
4. `vn` = the first section with `sh_type == 0x6ffffffe` (SHT_GNU_verneed). Walk it with `off = vn.offset`, or 0 if `vn` is absent; `off == 0` ends the walk. While `off != 0`:
   1. Read `Elf64_Verneed` at `off`; stop on a failed read.
   2. `file = CString(dynstr + vn_file)`. The string table is the dynamic section's, not the verneed section's `sh_link`.
   3. For every `Needed` whose `name == file`, append `off + 4` to its `vn_files`.
   4. `off = (vn_next == 0) ? 0 : off + vn_next`.
   - `sh_size`, `sh_info` and DT_VERNEEDNUM are ignored.
5. If no RUNPATH was found, skip A5.4–A5.7. The NEEDED entries are then neither checked nor changed.

#### A5.4 RUNPATH relativization

1. `old = CString(runpath_offset)`.
2. `here = parent_path(path)` and `final_here = Final(here)`.
3. `runpath = []` (elements `{entry, dir, keep}`).
4. For each `e` in `old` split on `:`, with empty pieces dropped:
   1. `d = Final(e)`.
   2. If `e == "$ORIGIN"` or `e` starts with `$ORIGIN/`: `d = normalize(join(final_here, e[min(len(e), 8):]))`.
      - Because of `join`, an absolute remainder replaces `final_here` (e.g. `$ORIGIN//x` → `/x`).
   3. If `IsStorePath(d)`: append `{entry = RelativeTo(final_here, d, "$ORIGIN"), dir = OnDisk(d), keep = true}`. Otherwise the entry is dropped. This drops pad entries, build-tree and host dirs, `${ORIGIN}`, `$LIB`, `$PLATFORM`, and relative entries.
5. For each `own` in `own_lib_dirs`, in sorted order: append `{entry = RelativeTo(final_here, Final(own), "$ORIGIN"), dir = own, keep = false}`.
6. `Render(runpath)`: the `entry` of every element with `keep == true`, in order, keeping only the first of identical strings, joined with `:`. The empty list renders as `""`.

#### A5.5 NEEDED: direct paths, libc detection, blob

1. `direct = []`.
2. For each `Needed n` in DT order:
   1. If `n.name` contains `/`, or starts with `ld-linux` or `linux-vdso`, skip it.
   2. Find the **first** element `r` of `runpath` (vector order, duplicates included) for which `exists(join(r.dir, n.name))` holds (stat, symlinks followed).
   3. If there is none: write `<path>: NEEDED <n.name> not found in RUNPATH [<old>] nor under <prefix>` to stderr, `errors += 1`, and continue.
   4. **libc detection:** if `exists(join(r.dir, "libc.so.6"))`, do nothing. The NEEDED stays a soname and `r.keep` is unchanged: true for RUNPATH elements, false for own lib dirs (Q-FIX-13).
   5. Otherwise set `r.keep = false` and append `(n, r)` to `direct`.
3. `target(n, r) = r.entry + "/" + n.name`.
4. `neu = Render(runpath)`. `blob = neu`, then for each `(n, r)` in `direct`, append `"\0" + target(n, r)`.
5. **Fallback:** if `len(blob) > len(old)`:
   1. Set `r.keep = true` for every `(n, r)` in `direct`.
   2. Clear `direct`.
   3. `neu = blob = Render(runpath)`.
   - No message is written for the fallback.
6. If `blob == old`, go to A5.7 step 4.

#### A5.6 Suffix-merged .dynstr refusal

Run this only when `blob != old`:

1. `sym` = the first section with `sh_type == 11` (SHT_DYNSYM). If there is none, or `sym.entsize < 24`, or `sym.link ≥ sections.len`, there is no check.
2. `strtab = sections[sym.link].offset`.
3. For `off = sym.offset`, while `off < SectionEnd(sym)`, step `sym.entsize`:
   1. Read `Elf64_Sym`. A failed read skips this entry; it does not stop the loop.
   2. If `runpath_offset < strtab + st_name < runpath_offset + len(old)` (strict on both ends): write `<path>: symbol '<CString(strtab + st_name)>' shares bytes with RUNPATH (suffix-merged .dynstr, not lld?)` to stderr, `errors += 1`, and **fail** the file. Only the first such symbol is reported.

#### A5.7 Writing RUNPATH and NEEDED

1. `WritePadded(runpath_offset, len(old) + 1, blob)`. The slot is the old string plus its NUL.
   - On failure: write `<path>: RUNPATH does not fit (<len(blob)> > <len(old)>): <neu>` to stderr, `errors += 1`, and **fail** the file. Lengths are decimal.
2. `str = runpath_offset + len(neu) + 1 − dynstr`.
3. For each `(n, r)` in `direct`, in order:
   1. Write u64 `str` at `n.val_offset` (the whole 8-byte `d_un`).
   2. For each `v` in `n.vn_files`, write u32 `str mod 2^32` at `v`.
   3. `str += len(target(n, r)) + 1`.
4. `dirty = true`, but only if `blob != old`.
5. Always: append `RUNPATH <neu>` to `log` (this yields `RUNPATH ` with a trailing space when `neu` is empty). Then append `NEEDED <target(n, r)>` for each element of `direct`.

#### A5.8 Stub entry detection

`FileOffset(vaddr)`:

1. For `i` in `0 .. e_phnum`, read a 56-byte `Elf64_Phdr` at `e_phoff + i × e_phentsize`. A failed read skips that header.
2. The first header with `p_type == 1` (PT_LOAD), `vaddr ≥ p_vaddr` and `vaddr − p_vaddr < p_filesz` gives `p_offset + (vaddr − p_vaddr)`.
3. Otherwise there is no offset.

`stub`:

1. If `e_entry ≥ 16` and `o = FileOffset(e_entry − 16)` exists, and `B[o .. o+8)` (clipped at L) equals `RELOCSTB`, then `stub = e_entry`.
   - If `o > L`, the current implementation aborts (Q-FIX-10).
2. Otherwise:
   1. Take the first SHT_DYNSYM section, with the same validity conditions as A5.6. If it is not valid, there is no stub.
   2. Walk its entries with step `entsize`, **stopping** at the first failed read.
   3. The first symbol whose name `CString(sections[link].offset + st_name)` equals `__reloc_start` gives `stub = st_value`.
   - The symbol's type, binding and section index are not checked.
3. If nothing matched, there is no stub.

#### A5.9 PT_INTERP

For `i` in `0 .. e_phnum`:

1. Read the `Elf64_Phdr` at `ph_off = e_phoff + i × e_phentsize`. **Stop** at the first failed read. Skip the header unless `p_type == 3` (PT_INTERP).
2. `old = CString(p_offset)`.
3. If there is no stub: append `INTERP <old> kept (no stub)` to `log` and continue. `dirty` is unchanged.
4. Otherwise:
   1. `neu = old`.
   2. If `IsStorePath(old)`: `neu = RelativeTo(Final(parent_path(path)), old, "")`, which is plain relative, e.g. `../../<hash>-glibc/lib/ld-linux-x86-64.so.2`.
   3. In that case also `WritePadded(p_offset, p_filesz, neu)`. On failure: write `<path>: interp does not fit: <neu>` to stderr, `errors += 1`, and **fail** the file.
5. Set this header's `p_type = 0` (PT_NULL) and write the 56-byte header back at `ph_off`.
6. Write the 64-byte ELF header read in A5.1, with `e_entry = stub`, at offset 0.
7. `dirty = true`. Append `INTERP <neu> (PT_NULL, entry=__reloc_start)` to `log`.

A non-store interpreter path (e.g. `/lib64/ld-linux-x86-64.so.2`) is not rewritten, but the segment is still nulled and the entry still redirected. Every PT_INTERP header is handled in turn.

#### A5.10 Commit, log, WARN

1. If `dirty`:
   1. Write back (A7). On failure, `errors += 1` and return "not mine". There is no WARN.
   2. On success, write `log` joined with two spaces (`"  "`) plus a newline to stdout.
2. Find the first occurrence of `S + "/"` in the (possibly modified) image. If there is one at offset `k`, write the following line to stdout:
   ```
     WARN absolute store ref in <rel(path)> @0x<k in lowercase hex>: <first 100 bytes of CString(k)>
   ```
   - The line has two leading spaces. `k` uses C++ `{:#x}` (e.g. `0x318`; zero is `0x0`).
   - It is not an error and is printed whether or not the file was dirty.
3. Return "mine".

ELF summary line grammar:

```
<rel> ["  RUNPATH " <neu> ("  NEEDED " <target>)*] ("  INTERP " <old> " kept (no stub)" | "  INTERP " <neu> " (PT_NULL, entry=__reloc_start)")*
```

### A6. Mach-O fixup

#### A6.1 Acceptance

Read the 32-byte `mach_header_64` at 0. If the read fails or `magic` (LE u32) ≠ `0xfeedfacf`, return "not mine". This rejects 32-bit (`0xfeedface`), big-endian, and fat/universal (`0xcafebabe`) files.

`filetype` is not checked. `self = Final(path)`, `here = parent_path(self)`, `shown = rel(path)`.

#### A6.2 Load commands and header room

1. `room = L − 32`. `off = 32`.
2. For `i` in `0 .. ncmds`:
   1. Read `{cmd, cmdsize}` (two u32) at `off`. **Stop** if the read fails, `cmdsize < 8`, or `off + cmdsize > L`.
   2. `bytes = B[off .. off+cmdsize)`.
   3. If `cmd` is a path command (table in *Formats*): `po = u32 at off+8` (0 if unreadable). `path_off = po < cmdsize ? po : 0`. Otherwise `path_off = 0`.
   4. If `cmd == 0x19` (LC_SEGMENT_64): `fileoff = u64 at off+40` and `filesize = u64 at off+48` (0 if unreadable).
      - If `filesize ≠ 0` and `fileoff == 0`: `nsects = u32 at off+64`. For `s` in `0 .. nsects`, read `so = u32 at off + 72 + s×80 + 48` (0 if unreadable). If `so ≠ 0`, `room = min(room, so − 32)` (wrapping).
      - Else if `filesize ≠ 0`: `room = min(room, fileoff − 32)` (wrapping).
   5. Keep the command and set `off += cmdsize`.
3. Commands after a stop are dropped from the list (Q-FIX-11).

#### A6.3 New spellings

`log = [shown]`. For each kept command with `path_off ≠ 0`, in order:

1. `p` = the bytes of `bytes` from `path_off` up to the first NUL, or to the end of the command.
2. Compute `t` by command type:
   - **LC_ID_DYLIB (0xd):** `t = self`, the file's own final absolute path. It is never `@rpath` (Q-FIX-8).
   - **LC_RPATH (0x8000001c):** `f = Final(p)`. If `IsStorePath(f)`, `t = RelativeTo(here, f, "@loader_path")`. Otherwise leave the command as it is.
   - **Load commands (0xc, 0x80000018, 0x8000001f, 0x80000023):** `t = Resolve(p)` (below). If it gives no target, write `reloc-fixup: <shown>: links <p>, which neither the package, a dependency nor the SDK has` to stderr, `errors += 1`, and leave the command. Otherwise `t = p` if `IsSystemPath(p)`, else `RelativeTo(here, target, "@loader_path")`.
3. If `t` exists and `t ≠ p`: `SetPath(t)` and append `t` to `log`.

`IsSystemPath(p)`: `p` starts with `/usr/lib/` or `/System/`.

`Resolve(p)`:

1. If `IsSystemPath(p)`:
   1. `stem` = `p` without a trailing `.dylib`, if it has one.
   2. The path is known iff `sdk == ""`, or `exists(join(sdk, stem[1:] + ".tbd"))`. Errors count as not existing.
   3. Return `p` if known, otherwise no target.
2. If `p` starts with `@rpath/`: `rest = p[7:]`. For each `d` in `own_lib_dirs`, in sorted order, the first with `exists(join(d, rest))` gives `target = Final(join(d, rest))`. LC_RPATH entries are not consulted.
3. Else if `p` starts with `@loader_path/`: `target = normalize(join(here, p[13:]))`. `here` is the **final** directory.
4. Else if `IsStorePath(Final(p))`: `target = Final(p)`.
5. Else there is no target. This covers `@executable_path/…`, a bare `@loader_path`, and non-store absolute paths.
6. If there is a target and `OnDisk(target) ≠ target` (path comparison) and `!exists(OnDisk(target))`, there is no target. Only targets inside dest are existence-checked; when `prefix == dest` none are (Q-FIX-7).

`SetPath(t)`:

1. Truncate `bytes` to `path_off` bytes and append `t`.
2. Pad with NUL to `((len + 8) / 8) × 8` (integer division). There is always at least one NUL and at most 8.
3. Write the new length as u32 at offset 4 of the command (`cmdsize`).
- Bytes before `path_off` are preserved, e.g. the timestamp and versions of `dylib_command`.

#### A6.4 No change

If `log` has only one element, return "mine". Nothing is written and nothing goes to stdout, even if A6.3 reported errors.

#### A6.5 Rebuilding the header

1. `cmds` = the concatenation of all kept commands' `bytes`.
2. If `len(cmds) > room`: write `reloc-fixup: <shown>: load commands need <len(cmds)> bytes, the header has room for <room> (-headerpad)` to stderr (decimal numbers), `errors += 1`, and return "mine" without writing.
3. Write u32 `len(cmds)` at offset 20 (`sizeofcmds`). `ncmds` is **not** changed.
4. `Overwrite(32, cmds + NUL × (room − len(cmds)))`. Everything from the end of the commands up to the room bound is zeroed.

#### A6.6 Ad-hoc re-signing

1. Re-read the header and walk `ncmds` commands from offset 32, stopping if a read fails or `cmdsize < 8`. Commands are not bounds-checked against L here.
2. For each `cmd == 0x1d` (LC_CODE_SIGNATURE), `sig = u32 at off+8` (`dataoff`). The last one wins; an unreadable value means "none".
3. If there is no `sig`, the file is unsigned and the step succeeds (the x86_64 default).
4. If BE u32 at `sig` ≠ `0xfade0cc0` (embedded-signature SuperBlob): write `reloc-fixup: <shown>: LC_CODE_SIGNATURE is no embedded signature blob` to stderr and **fail**.
5. `count` = BE u32 at `sig+8` (0 if unreadable).
6. For `j` in `0 .. count`:
   1. `cd = sig + (BE u32 at sig + 12 + 8j + 4, or 0)`.
   2. Skip the blob unless BE u32 at `cd` equals `0xfade0c02` (CodeDirectory). **Every** CodeDirectory is processed, alternates included.
   3. Read the fields: `hashOffset` = BE u32 at `cd+16`, `nCodeSlots` = BE u32 at `cd+28`, `codeLimit` = BE u32 at `cd+32`, `hashSize` = u8 at `cd+36`, `hashType` = u8 at `cd+37`, `page = 1 << (u8 at cd+39)`. Unreadable values are 0.
   4. If `hashType ≠ 2` (SHA-256) or `hashSize ≠ 32`: write `reloc-fixup: <shown>: code signature hash type <hashType as decimal integer> is not SHA-256` to stderr and **fail**.
   5. For `slot` in `0 .. nCodeSlots`:
      1. `begin = slot × page` and `end = min(begin + page, codeLimit)`.
      2. `digest` = SHA-256 of the **current** image bytes `B[begin .. begin + (end − begin))`, with the length wrapping and then clipped at L.
         - If `begin > L` the current implementation aborts (Q-FIX-10).
      3. `Overwrite(cd + hashOffset + slot × hashSize, digest)`. On failure: write `reloc-fixup: <shown>: code signature slot <slot> lies outside the file` to stderr and **fail**.
   - Special (negative) slots, `codeLimit64`, and every other blob are untouched.
7. Succeed.

If re-signing fails: `errors += 1` and return "mine" without writing.

#### A6.7 Commit

1. Write back (A7). On failure, `errors += 1` and return "mine".
2. On success, write `log` joined with two spaces plus a newline to stdout.

Mach-O has no WARN scan and no symbol check.

Mach-O summary line grammar:

```
<rel> ("  " <new spelling>)+
```

The new spellings appear in load-command order.

### A7. Write-back

1. `stat(path)`. If it succeeds, `chmod(path, st_mode | S_IWUSR)`.
2. Write the image to `join(parent_path(path), ".ekacache<pid>." + filename(path))`.
   - The file is created or truncated with the default mode (0666 & ~umask).
   - If writing or closing fails, remove the temp file and fail.
3. `rename(temp, path)`. If that fails, fail and leave the temp file behind.
4. If the stat succeeded, `chmod(path, st_mode)`. This now applies to the new file and restores the original permission bits.
5. On failure, write `<path>: cannot write back: <strerror(errno)>` to stderr.
   - `errno` is read **after** step 2's cleanup, so after an open failure it is usually ENOENT, which prints "No such file or directory" (Q-FIX-4).
   - The caller counts the error.

The result is a new inode: hard links are broken, owner and xattrs are not preserved, and the modification time is the current time.

## Formats

### ELF64 structures read or written (little-endian)

| Structure (size) | Field @offset: type |
|---|---|
| `Elf64_Ehdr` (64) | `e_ident` @0 [16]; `e_type` @16 u16; `e_machine` @18 u16; `e_version` @20 u32; **`e_entry` @24 u64**; `e_phoff` @32 u64; `e_shoff` @40 u64; `e_flags` @48 u32; `e_ehsize` @52 u16; `e_phentsize` @54 u16; `e_phnum` @56 u16; `e_shentsize` @58 u16; `e_shnum` @60 u16; `e_shstrndx` @62 u16 |
| `Elf64_Shdr` (64) | `sh_name` @0 u32; `sh_type` @4 u32; `sh_flags` @8 u64; `sh_addr` @16 u64; `sh_offset` @24 u64; `sh_size` @32 u64; `sh_link` @40 u32; `sh_info` @44 u32; `sh_addralign` @48 u64; `sh_entsize` @56 u64 |
| `Elf64_Phdr` (56) | **`p_type` @0 u32**; `p_flags` @4 u32; `p_offset` @8 u64; `p_vaddr` @16 u64; `p_paddr` @24 u64; `p_filesz` @32 u64; `p_memsz` @40 u64; `p_align` @48 u64 |
| `Elf64_Dyn` (16) | `d_tag` @0 i64; **`d_val` @8 u64** |
| `Elf64_Sym` (24) | `st_name` @0 u32; `st_info` @4 u8; `st_other` @5 u8; `st_shndx` @6 u16; `st_value` @8 u64; `st_size` @16 u64 |
| `Elf64_Verneed` (16) | `vn_version` @0 u16; `vn_cnt` @2 u16; **`vn_file` @4 u32**; `vn_aux` @8 u32; `vn_next` @12 u32 |

Fields in bold are the only ones ever written: `e_entry` (by rewriting the whole Ehdr), `p_type` (by rewriting the whole Phdr), `d_val` of DT_NEEDED, and `vn_file`. String bytes are written only inside the old RUNPATH slot and the PT_INTERP segment.

| Constant | Value |
|---|---|
| ET_EXEC / ET_DYN | 2 / 3 |
| SHT_DYNAMIC / SHT_NOBITS / SHT_DYNSYM / SHT_GNU_verneed | 6 / 8 / 11 / 0x6ffffffe |
| DT_NULL / DT_NEEDED / DT_RPATH / DT_RUNPATH | 0 / 1 / 15 / 29 |
| PT_NULL / PT_LOAD / PT_INTERP | 0 / 1 / 3 |
| Stub header magic / size | `RELOCSTB` (8 bytes) / 16 bytes before `e_entry` |

### RUNPATH slot after rewrite

Capacity = `len(old) + 1` bytes starting at `runpath_offset`:

| Bytes | Content |
|---|---|
| `len(neu)` | rendered RUNPATH (`:`-joined) |
| 1 | NUL |
| for each direct NEEDED, in DT order: `len(target)` + 1 | `<entry>/<soname>` then NUL; its offset relative to `dynstr` is stored in the DT_NEEDED `d_val` (u64) and in each matching `vn_file` (u32) |
| rest | NUL padding up to the capacity |

### PT_INTERP segment after rewrite

`p_filesz` bytes at `p_offset`: the relative interpreter path, then NUL padding to `p_filesz`. The interpreter string is rewritten only if it was a store path. The header's `p_type` becomes 0.

### Mach-O structures (little-endian)

| Structure (size) | Fields used |
|---|---|
| `mach_header_64` (32) | `magic` @0 = 0xfeedfacf; `ncmds` @16; **`sizeofcmds` @20** |
| `load_command` | `cmd` @0 u32; **`cmdsize` @4 u32** |
| `dylib_command` (24 + string) | `name.offset` @8 u32 (normally 24); `timestamp` @12, `current_version` @16 and `compatibility_version` @20 are preserved |
| `rpath_command` (12 + string) | `path.offset` @8 u32 (normally 12) |
| `segment_command_64` (72) | `fileoff` @40 u64; `filesize` @48 u64; `nsects` @64 u32; `section_64` records follow at 72 |
| `section_64` (80) | `offset` @48 u32 |
| `linkedit_data_command` | `dataoff` @8 u32 |

| Load command | Value | Treatment |
|---|---|---|
| LC_SEGMENT_64 | 0x19 | bounds `room` |
| LC_LOAD_DYLIB | 0xc | path command, resolved |
| LC_ID_DYLIB | 0xd | path command, becomes the final absolute path |
| LC_CODE_SIGNATURE | 0x1d | locates the signature |
| LC_LOAD_WEAK_DYLIB | 0x80000018 | path command, resolved |
| LC_RPATH | 0x8000001c | path command, store paths `@loader_path`-relative |
| LC_REEXPORT_DYLIB | 0x8000001f | path command, resolved |
| LC_LOAD_UPWARD_DYLIB | 0x80000023 | path command, resolved |
| anything else (e.g. LC_LAZY_LOAD_DYLIB 0x20, LC_LOAD_DYLINKER 0xe) | — | copied verbatim |

### Code signature (big-endian)

| Structure | Field @offset |
|---|---|
| SuperBlob | `magic` @0 = 0xfade0cc0; `length` @4; `count` @8; `index[j]` @12+8j = {`type` u32, `offset` u32 (relative to the SuperBlob)} |
| CodeDirectory | `magic` @0 = 0xfade0c02; `hashOffset` @16 u32; `nCodeSlots` @28 u32; `codeLimit` @32 u32; `hashSize` @36 u8 (must be 32); `hashType` @37 u8 (must be 2 = SHA-256); `pageSize` @39 u8 (log2) |

Code slot `k` lives at `cd + hashOffset + 32k` and holds `SHA-256(B[k·2^pageSize .. min((k+1)·2^pageSize, codeLimit)))`.

## Errors and messages

Every message is one line ending in `\n`. Abbreviations: `<path>` is the walked path (prefix-joined, e.g. `/build/out/bin/foo`); `<rel>` is `rel(path)`; `<prefix>`/`<dest>` are the normalized arguments. "Error" means `errors += 1`, which leads to exit 1.

| # | Stream | Text | When | Effect |
|---|---|---|---|---|
| 1 | stderr | `usage: reloc-fixup <prefix> [--dest <store path>] [--sdk <dir>] [--deny <store path>]...` | no arguments | exit 2 |
| 2 | stderr | `reloc-fixup: unknown argument <arg>` | unknown flag at an odd position with a successor | exit 2 |
| 3 | stderr | `reloc-fixup: <dest> is not under <S>` | dest is not a store path | exit 2 |
| 4 | stderr | `reloc-fixup: <rel> refers to build-platform <shown>` | deny hit (A4) | error; processing continues |
| 5 | stderr | `<path>: cannot write back: <strerror>` | A7 failed | error (counted by caller) |
| 6 | stderr | `<path>: NEEDED <soname> not found in RUNPATH [<old RUNPATH>] nor under <prefix>` | A5.5 | error; the file is still rewritten |
| 7 | stderr | `<path>: symbol '<name>' shares bytes with RUNPATH (suffix-merged .dynstr, not lld?)` | A5.6 | error; file left unmodified, no WARN |
| 8 | stderr | `<path>: RUNPATH does not fit (<len blob> > <len old>): <neu>` | A5.7 | error; file left unmodified, no WARN |
| 9 | stderr | `<path>: interp does not fit: <neu>` | A5.9 | error; file left unmodified (RUNPATH edits discarded), no WARN |
| 10 | stderr | `reloc-fixup: <rel>: links <load path>, which neither the package, a dependency nor the SDK has` | A6.3 | error; that command is kept, the others are still rewritten |
| 11 | stderr | `reloc-fixup: <rel>: load commands need <n> bytes, the header has room for <room> (-headerpad)` | A6.5 | error; file left unmodified |
| 12 | stderr | `reloc-fixup: <rel>: LC_CODE_SIGNATURE is no embedded signature blob` | A6.6 | error; file left unmodified |
| 13 | stderr | `reloc-fixup: <rel>: code signature hash type <n> is not SHA-256` | A6.6 | error; file left unmodified |
| 14 | stderr | `reloc-fixup: <rel>: code signature slot <n> lies outside the file` | A6.6 | error; file left unmodified |
| 15 | stdout | ELF summary line (A5.10 grammar) | ELF file rewritten | — |
| 16 | stdout | `  WARN absolute store ref in <rel> @0x<hex>: <text>` | ELF64 EXEC/DYN still containing `S/` | not an error |
| 17 | stdout | Mach-O summary line (A6.7 grammar) | Mach-O file rewritten | — |

The order of lines within each stream follows processing order (walk order; within a file, message order as in the algorithms). How stdout and stderr interleave is unspecified: C++ stdout is block-buffered when not a terminal. An implementation SHOULD flush stdout before exit.

## Test vectors

Unless stated otherwise, these vectors assume `S = /nix/store`. `OUT` = `/nix/store/9123456789abcdfghijklmnpqrsvwxyz-openssl`, `ZLIB` = `/nix/store/7123456789abcdfghijklmnpqrsvwxyz-zlib/lib/libz.1.dylib`.

### From `TestBinaryImage` (jig/src/jig_test.cc)

Start from a 64-byte zero image with `7F 'E' 'L' 'F' 02 01` at 0:

- `IsElf64LittleEndian` → true.
- `Write<u32>(16, 0xdeadbeef)` → true, and `Read<u32>(16)` → `0xdeadbeef`.
- `Read<u64>(60)` → none (it would run past the end). `Read<u16>(1000)` → none. `Write<u64>(57, 1)` → false.
- `WritePadded(32, 8, "abc")` → true, and then `CString(32)` = `abc`.
- `WritePadded(32, 3, "abc")` → false (no room for the NUL). `WritePadded(60, 8, "abc")` → false.
- `CString(5000)` = `""`.
- An image of the 5 bytes `short` is not ELF64-LE.

### From `TestMachOFixup` (jig/src/jig_test.cc)

The test file is a 2048-byte image:

- One LC_SEGMENT_64 (cmdsize 152, fileoff 0, filesize 2048, 1 section with offset 1024, so `room` = 992).
- LC_ID_DYLIB `@rpath/libssl.3.dylib`.
- LC_LOAD_DYLIB `@rpath/libcrypto.3.dylib`, LC_LOAD_DYLIB `ZLIB`, and LC_LOAD_DYLIB `/usr/lib/libSystem.B.dylib`.
- Each dylib command has `name.offset = 24` and cmdsize `((24+len+8)/8)×8`.

The context is `prefix=/tmp/jigtest-macho-<pid>`, `dest=OUT`, `own_lib_dirs=[prefix/lib]`. The file is `prefix/lib/libssl.3.dylib`, and an empty `prefix/lib/libcrypto.3.dylib` exists.

Expected result:

1. The function returns "mine" with `errors == 0`, and the size stays 2048.
2. The file contains `OUT/lib/libssl.3.dylib`, `@loader_path/libcrypto.3.dylib`, `@loader_path/../../7123456789abcdfghijklmnpqrsvwxyz-zlib/lib/libz.1.dylib` and `/usr/lib/libSystem.B.dylib`.
3. Any remaining `…-zlib/` occurs only as `../../…-zlib/`.
4. Derived and confirmed: the new cmdsizes are 96, 56, 104 and 56, `sizeofcmds` = 464, and stdout is:
   ```
   lib/libssl.3.dylib  /nix/store/9123456789abcdfghijklmnpqrsvwxyz-openssl/lib/libssl.3.dylib  @loader_path/libcrypto.3.dylib  @loader_path/../../7123456789abcdfghijklmnpqrsvwxyz-zlib/lib/libz.1.dylib
   ```

Unresolvable loads, with `sdk = prefix/sdk` and an existing `sdk/usr/lib/libSystem.B.tbd`. Each case is a fresh image holding the segment plus one LC_LOAD_DYLIB:

| load | errors | stderr (confirmed) |
|---|---|---|
| `@rpath/libgone.dylib` | 1 | `reloc-fixup: lib/libssl.3.dylib: links @rpath/libgone.dylib, which neither the package, a dependency nor the SDK has` |
| `@loader_path/libgone.dylib` | 1 | `reloc-fixup: lib/libssl.3.dylib: links @loader_path/libgone.dylib, which neither the package, a dependency nor the SDK has` |
| `/usr/lib/libgone.dylib` | 1 | `reloc-fixup: lib/libssl.3.dylib: links /usr/lib/libgone.dylib, which neither the package, a dependency nor the SDK has` |
| `/usr/lib/libSystem.B.dylib` | 0 | (none; unchanged, no write, no stdout) |

Tight header: the segment's section offset is 32 + 72 + 80 + 96 = 280 (`room` 248), and there is one LC_LOAD_DYLIB `ZLIB` (cmdsize 96). The result is one error and the file is byte-identical on disk. Derived and confirmed stderr:

```
reloc-fixup: lib/libssl.3.dylib: load commands need 256 bytes, the header has room for 248 (-headerpad)
```

### Derived and confirmed: CLI (run against the built binary)

| argv after `ec` | stderr | exit |
|---|---|---|
| `reloc-fixup` | usage line | 2 |
| `reloc-fixup /tmp/x` | `reloc-fixup: /tmp/x is not under /nix/store` | 2 |
| `reloc-fixup /tmp/x --bogus 1` | `reloc-fixup: unknown argument --bogus` | 2 |
| `reloc-fixup /tmp/x --bogus` | `reloc-fixup: /tmp/x is not under /nix/store` (the unpaired flag is ignored) | 2 |
| `reloc-fixup /tmp/nonexistent --dest /nix/store/abc` | (none) | 0 |
| `reloc-fixup <dir> --dest <store> --deny` | (unpaired `--deny` ignored) | 0 |

### Derived and confirmed: deny check

For a tree containing `a.txt` = `abc\ndef` and `sub/b.bin` = `\x01xyz`:

- `--deny /nix/store/foo/` gives an empty needle. Stderr is `reloc-fixup: a.txt refers to build-platform abc` and then `reloc-fixup: sub/b.bin refers to build-platform ` (with a trailing space), in walk order. Exit 1.
- `--deny x/bc` gives the needle `bc`. Stderr is `reloc-fixup: a.txt refers to build-platform bc`. Exit 1.

A hit in an ELF RUNPATH shows exactly 100 bytes when no control byte intervenes, e.g. `<hash>-glibc-2.40-224/lib/.:/` followed by `_` up to the 100th byte. Files named `*.debug` and `etc/ekacache.json` are not reported.

### Derived and confirmed: path operations

`final_here` = `/nix/store/y-foo/bin`:

| call | result |
|---|---|
| `RelativeTo(final_here, "/nix/store/x-zlib/lib/.", "$ORIGIN")` | `$ORIGIN/../../x-zlib/lib/` |
| `RelativeTo(final_here, "/nix/store/x-zlib/lib", "$ORIGIN")` | `$ORIGIN/../../x-zlib/lib` |
| `RelativeTo(final_here, "/nix/store/y-foo/bin/", "$ORIGIN")` | `$ORIGIN` |
| `RelativeTo(final_here, "/nix/store/y-foo/bin/..", "$ORIGIN")` | `$ORIGIN/../` |
| `RelativeTo(final_here, "/nix/store/g-glibc/lib/./././ld-linux-x86-64.so.2", "")` | `../../g-glibc/lib/ld-linux-x86-64.so.2` |
| `RelativeTo(final_here, "relative/x", "$ORIGIN")` | `$ORIGIN/` |
| `RelativeTo("/nix/store/y-foo/lib", "/nix/store/y-foo/lib/libz.dylib", "@loader_path")` | `@loader_path/libz.dylib` |

With `prefix=/tmp/out` and `dest=/nix/store/y-foo`:

| call | result |
|---|---|
| `Final("/tmp/out/lib")` | `/nix/store/y-foo/lib` |
| `Final("/tmp/out")` | `/nix/store/y-foo/` |
| `Final("/tmp/out/lib/.")` | `/nix/store/y-foo/lib/` |
| `Final("/tmp/outx/lib")` | `/tmp/outx/lib` |
| `Final("/nix/store/x/lib/.")` | `/nix/store/x/lib/.` (verbatim) |
| `Final("$ORIGIN/../lib")` | `$ORIGIN/../lib` |
| `Final("/tmp//out//lib")` | `/nix/store/y-foo/lib` |
| `OnDisk("/nix/store/y-foo")` | `/tmp/out/` |
| `OnDisk("/nix/store/x/lib")` | `/nix/store/x/lib` |

### Derived and confirmed: ELF end to end (GNU ld output, gcc 14)

Setup:

- `prefix=/tmp/fxe2e/out2`, `--dest /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-foo`. `<G>` stands for a glibc store name.
- `lib/libbar.so` was linked with RUNPATH `/nix/store/<G>/lib/.:/` + 223×`_` + `:<three more store lib dirs>`.
- `bin/foo` was linked against `-lbar`, with RUNPATH `/tmp/fxe2e/out2/lib/.:/nix/store/<G>/lib/.:/` + 223×`_` + `:<the same three dirs>`, a dynsym-exported `__reloc_start`, and the interpreter `/nix/store/<G>/lib/` + `./`×12 + `ld-linux-x86-64.so.2`.

Expected stdout (exit 0):

```
lib/libbar.so  RUNPATH $ORIGIN/../../<G>/lib/:$ORIGIN/../../<D1>/lib:$ORIGIN/../../<G>/lib:$ORIGIN/../../<D2>/lib
bin/foo  RUNPATH $ORIGIN/../../<G>/lib/:$ORIGIN/../../<D1>/lib:$ORIGIN/../../<G>/lib:$ORIGIN/../../<D2>/lib  NEEDED $ORIGIN/../lib//libbar.so  INTERP ../../<G>/lib/ld-linux-x86-64.so.2 (PT_NULL, entry=__reloc_start)
```

The order of the two lines is walk order. Points to note:

- `<G>/lib/` and `<G>/lib` both appear (Q-FIX-1, Q-FIX-16), and libc.so.6 stays a soname.
- In `bin/foo`, DT_NEEDED is `$ORIGIN/../lib//libbar.so` with a double slash, because the own dir came from the `/.`-spelled RUNPATH entry.
- PT_INTERP becomes PT_NULL, and `e_entry` = `st_value(__reloc_start)` (0x1160 here).

A second run over the result prints nothing and exits 0, so the mode is idempotent.

The same binaries copied to a different directory, `run2`, give the same lines except `NEEDED $ORIGIN/../lib/libbar.so` with a single slash. The RUNPATH entry `/tmp/fxe2e/out2/lib/.` is outside that prefix and is dropped, and libbar.so is then found through `own_lib_dirs` (`run2/lib`), whose entry has no trailing slash.

Without a stub and without interpreter padding (`bin/foo` linked normally), the line ends `  INTERP /nix/store/<G>/lib/ld-linux-x86-64.so.2 kept (no stub)`, followed by `  WARN absolute store ref in bin/foo @0x318: /nix/store/<G>/lib/ld-linux-x86-64.so.2`.

A wrapper-linked `lib/libbar.so` without padding (only the three store dirs, no RUNPATH slack, no NEEDED to rewrite) gives stderr `<prefix>/lib/libbar.so: RUNPATH does not fit (192 > 183): $ORIGIN/../../<D1>/lib:$ORIGIN/../../<G>/lib:$ORIGIN/../../<D2>/lib` and exit 1. The `$ORIGIN` spelling is 9 bytes longer than the absolute one it replaces.

A rewrite needed in a read-only `bin/` directory gives stderr `/tmp/fxe2e/run3/bin/foo: cannot write back: No such file or directory` (sic, Q-FIX-4) and exit 1. A prefix and dest given with trailing `/` produced the same paths without doubled slashes.

### Derived and confirmed: Mach-O re-signing

The `TestMachOFixup` image has two extra commands: LC_RPATH `OUT/lib` and LC_RPATH `@loader_path/../lib` (path offset 12). It also has an LC_CODE_SIGNATURE (`dataoff` 1536) whose SuperBlob holds one CodeDirectory at +20, with hashOffset 88, nCodeSlots 3, codeLimit 1536, hashSize 32, hashType 2 and pageSize 9.

Expected stdout:

```
lib/libssl.3.dylib  /nix/store/9123456789abcdfghijklmnpqrsvwxyz-openssl/lib/libssl.3.dylib  @loader_path/libcrypto.3.dylib  @loader_path/../../7123456789abcdfghijklmnpqrsvwxyz-zlib/lib/libz.1.dylib  @loader_path
```

- The LC_RPATH cmdsizes become 32 (`@loader_path`) and 32 (unchanged). `sizeofcmds` = 544 and `ncmds` = 8.
- Each of the 3 slots equals SHA-256 of the corresponding 512-byte page of the output.

With hashType 1, stderr is `reloc-fixup: lib/libssl.3.dylib: code signature hash type 1 is not SHA-256`, `errors` = 1, and the file on disk is unchanged.

## Quirks

### Q-FIX-1: `/.`-spelled RUNPATH entries become entries with a trailing slash, and direct NEEDED get `//`

**Current behavior:** `normalize("<dir>/.")` keeps a trailing `/`, so the driver's `<store dir>/.` entries come out as `$ORIGIN/…/lib/`. A NEEDED made direct through such an entry is spelled `<entry>/<soname>`, e.g. `$ORIGIN/../lib//libbar.so`. Deduplication is by exact string, so `…/lib/` and `…/lib` are both kept. The driver's comment claims fixup "normalises the spelling away"; it does not.
**Impact:** Harmless to ld.so, but it costs one byte per entry, plus one per direct NEEDED, of the slack, and the RUNPATH can carry duplicates. The bytes are part of the output, which matters for reproducibility and drop-in compatibility.
**Recommendation:** keep

### Q-FIX-2: Lenient argument parsing

**Current behavior:** Flags are only examined in pairs from position 1. A trailing unpaired argument (`--dest` with no value, or a stray word) is silently ignored. An unknown word is only reported when something follows it. `args[0]` is always the prefix. `--flag=value` forms are unknown.
**Impact:** Typos can go unnoticed.
**Recommendation:** document only

### Q-FIX-3: `--deny` uses only the last path component; a trailing slash matches everything

**Current behavior:** The needle is `filename(v)[0:32]`. For `/nix/store/<hash>-x/` the filename is empty, so every file is reported, with up to 100 bytes from offset 0. For `/nix/store/<hash>-x/bin/y` the needle is `y`.
**Impact:** Large numbers of false errors for a cosmetically different argument.
**Recommendation:** change in Rust — needs maintainer sign-off (take the store-path hash component, or reject empty needles)

### Q-FIX-4: Write-back failure reports a clobbered errno

**Current behavior:** After the temp-file write fails, the cleanup `unlink` of the never-created temp file sets errno to ENOENT. The message then reads `cannot write back: No such file or directory` even when the real cause was EACCES or ENOSPC.
**Impact:** Misleading diagnostics.
**Recommendation:** change in Rust — needs maintainer sign-off (report the errno of the failing operation)

### Q-FIX-5: Inconsistent message prefixes and path forms

**Current behavior:** ELF diagnostics (#5–#9) use the full walked path and no `reloc-fixup: ` prefix. Deny and Mach-O diagnostics use `reloc-fixup: ` and `rel(path)`.
**Impact:** Cosmetic. Log scrapers must handle both forms.
**Recommendation:** document only

### Q-FIX-6: own_lib_dirs heuristic is substring-based

**Current behavior:** Any regular file whose name contains `.so` anywhere (`x.sock`, `a.sol`, `foo.so.conf`) or ends in `.dylib` marks its directory as a library dir. Dirs whose parent path string contains `/debug` anywhere are excluded, including when the prefix itself is under e.g. `/tmp/debug-build/`. In that case no own lib dir exists.
**Impact:** Extra entries can be appended to RUNPATH in the fallback, or searched for `@rpath`, or own lib dirs can go missing so that NEEDED/`@rpath` errors appear.
**Recommendation:** document only

### Q-FIX-7: No existence check for in-package Mach-O targets when prefix == dest

**Current behavior:** Resolve only checks `exists(OnDisk(target))` when `OnDisk(target) ≠ target`. Without `--dest`, these are equal for every in-package target, so a dangling `@loader_path/…` or absolute in-package load passes silently.
**Impact:** Broken references go unreported unless `--dest` differs from the prefix.
**Recommendation:** change in Rust — needs maintainer sign-off (check whenever the target lies under dest)

### Q-FIX-8: Header comment disagrees with the code about LC_ID_DYLIB

**Current behavior:** `fixup_mode.h` says LC_ID_DYLIB becomes `@rpath/<name>`. The code, confirmed by `TestMachOFixup`, sets it to the file's own final absolute path (`Final(path)`).
**Impact:** None at runtime. The code is normative.
**Recommendation:** document only

### Q-FIX-9: The "outside prefix" test is a string prefix `..`

**Current behavior:** `Final`/`OnDisk` treat a relative result starting with the bytes `..` as outside, so a top-level entry named e.g. `..data` is not mapped. A relative `<prefix>` never matches absolute RUNPATH or load paths, because `relative()` of absolute against relative is `""`.
**Impact:** Rare mis-mapping. With a relative prefix, absolute in-tree RUNPATH entries are dropped; only `own_lib_dirs` covers them.
**Recommendation:** document only

### Q-FIX-10: Malformed input or filesystem errors abort the process

**Current behavior:** The process terminates with SIGABRT through an uncaught C++ exception, and pending stdout is lost, in these cases:

- `string_view::substr` with a start past the end: the RELOCSTB check when a PT_LOAD maps past EOF, and a signature slot whose `begin > L`.
- Throwing `std::filesystem` calls: walk increment errors other than EACCES, `fs::exists` without an error code on EACCES/ELOOP (A5.5), and `fs::relative`.

Some cases are undefined behavior or merely slow:

- `pageSize ≥ 64` in a CodeDirectory is undefined behavior.
- A huge `nsects` makes the room loop run up to 2^32 iterations.
- `end < begin` in a slot hashes the rest of the file.

**Impact:** One corrupt file stops the whole run.
**Recommendation:** change in Rust — needs maintainer sign-off (treat as no match or skip the file, and report an error)

### Q-FIX-11: Truncated Mach-O command parse can corrupt the header

**Current behavior:** If the command walk stops early (bad cmdsize), the later commands are dropped from `cmds`. If any rewrite happens, `sizeofcmds` shrinks while `ncmds` stays the same. With no bounding segment at all, `room` is the whole file after the header, which is then zero-filled past the commands.
**Impact:** A corrupt output for already-malformed or unusual inputs.
**Recommendation:** change in Rust — needs maintainer sign-off (refuse the file with an error)

### Q-FIX-12: Signature handling is narrow

**Current behavior:** Every CodeDirectory in the SuperBlob is rehashed. Any one that is not SHA-256 with hashSize 32 (e.g. a SHA-1 alternate CD) fails the whole file. `codeLimit64` and special slots are ignored, and a non-SuperBlob at `dataoff` is an error.
**Impact:** Correct for ld64/lld ad-hoc arm64 output, but it fails on dual-hash signatures.
**Recommendation:** document only

### Q-FIX-13: libc detection and the skip list are glibc-specific

**Current behavior:** A directory is "libc's" iff it contains `libc.so.6`. NEEDED found there stay sonames. Only names starting `ld-linux` or `linux-vdso` (and names containing `/`) are exempt from resolution. musl (`libc.so`) or other loaders such as `ld64.so.2` and `ld-musl-*` are not recognized, so their NEEDED must be found or they raise errors. A NEEDED found in a libc dir that is only an own lib dir is neither made direct nor added to RUNPATH.
**Impact:** Non-glibc targets would misbehave.
**Recommendation:** document only

### Q-FIX-14: Several RUNPATH spellings are silently dropped

**Current behavior:** `${ORIGIN}`, `$LIB`, `$PLATFORM`, relative entries and empty entries are dropped. `$ORIGIN//abs` resolves to `/abs`, because `join` with an absolute right-hand side replaces the left. Binaries without any DT_RUNPATH/DT_RPATH get no NEEDED check at all.
**Impact:** Entries a binary relied on can vanish without a message.
**Recommendation:** document only

### Q-FIX-15: The symbol-overlap check is partial

**Current behavior:** A5.6 only checks `.dynsym` names, with strict bounds, so a name starting exactly at the RUNPATH start is not detected. Strings referenced by DT_SONAME, verneed auxiliaries or other dynamic tags are not checked.
**Impact:** A GNU-ld suffix-merged string other than a symbol name could in theory be corrupted.
**Recommendation:** document only

### Q-FIX-16: Duplicate RUNPATH dirs keep the directory on RUNPATH

**Current behavior:** NEEDED resolution marks only the **first** matching element `keep = false`. A second element for the same directory (another spelling, e.g. `…/lib/` and `…/lib`) stays. When direct NEEDED do not fit, the fallback is silent: no message, only the `RUNPATH` log entry.
**Impact:** Wasted slack. A silent loss of direct NEEDED.
**Recommendation:** document only

### Q-FIX-17: A missing or non-directory prefix succeeds silently

**Current behavior:** If `<prefix>` cannot be walked, no file is processed, nothing is printed, and the exit status is 0, provided dest is valid.
**Impact:** A typo in the prefix passes a build step.
**Recommendation:** change in Rust — needs maintainer sign-off (error if the prefix is not a readable directory)

### Q-FIX-18: Write-back replaces the inode

**Current behavior:** temp file + rename. This breaks hard links, gives the file the running user's ownership, drops xattrs and ACLs, and leaves `.ekacache<pid>.<name>` behind if rename fails. Permission bits are restored from the original `stat`.
**Impact:** Usually fine inside a build sandbox.
**Recommendation:** keep

## Open questions

1. `crt_interp.c` (the source of `__reloc_start`), `reloc_stub.bin` and `formatelf --set-entry-stub` are not in this repository. The only contract this mode relies on is the 8-byte `RELOCSTB` magic at `e_entry − 16` and the dynsym symbol name. How the stub interprets the relative PT_INTERP string (relative to the executable's directory, judging by `Final(parent_path(path))`) cannot be confirmed from this repo.
2. Q-FIX-10/11: should the Rust port reproduce aborts on malformed input? An abort cannot be reproduced meaningfully byte for byte. This document assumes "treat as an error" pending sign-off.
3. Walk order is `readdir` order, which is filesystem-dependent. Drop-in output equality holds only under the same order. Should the Rust port sort entries instead? That would change stdout line order and needs sign-off.
