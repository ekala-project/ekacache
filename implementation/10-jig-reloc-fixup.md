# 10: reloc-fixup (phase P8a: ELF; Mach-O in P9)

Specified by: `jig-reloc-fixup.md` (all), `jig-driver-policy.md` (the link conventions that
leave the slack this mode fills).
Adopted here: nothing. Q-FIX-3, -4, -7, -10, -11, -17 are proposals awaiting sign-off, so the
current behaviour is reproduced.

## A correction to the platform split

The decision "Linux first, macOS later" put Mach-O fixup in P9. Reading the spec closely: the
Mach-O fixup is **not** host-specific. It is pure byte rewriting (load commands plus an ad-hoc
SHA-256 code-signature refresh). Legacy runs it on Linux hosts for every file that is not
ELF64-LE. A Linux reloc-fixup without it is therefore **not** drop-in for trees that contain
Mach-O images (for example darwin cross-builds on a Linux builder). The genuinely
host-specific code is only `driver.cc:439` (`#ifdef __APPLE__`) and jigd's peer
authentication.

Plan: ELF ships in P8a. Mach-O (A6, about 315 lines of C++) ships in P9 as decided, and until
then the Rust reloc-fixup **refuses to run** on a tree containing a 64-bit LE Mach-O file
(it prints an explicit "Mach-O fixup not yet implemented" error and exits 1), so it never
silently does less than legacy. **Proposal:** if darwin cross-builds on Linux matter, move A6
into P8a. It costs about a week and needs no macOS machine, since the tests are byte-level.

## Module (`crates/jig/src/fixup/`)

| File | Contents | Spec |
|---|---|---|
| `mod.rs` | argument parsing (odd positions, last-wins, silently ignored unpaired tail Q-FIX-2), A1 top level, error counter, exit codes | §Invocation, A1 |
| `walk.rs` | A2 tree walk (regular non-symlink files, walk order = `recursive_directory_iterator` order, shared with `dir.rs` from 08), `own_lib_dirs` | A2 |
| `deny.rs` | A4 deny check | A4 |
| `elf.rs` | A5: acceptance, dynamic section, RUNPATH rewrite, NEEDED direct paths, libc detection, `__reloc_start` / RELOCSTB stub check, PT_INTERP rewrite, WARN lines | A5, §ELF64 structures |
| `macho.rs` | A6 (P9) | A6, §Mach-O structures, §Code signature |
| `image.rs` | `Image { bytes: Vec<u8> }` with `read::<T>`, `write::<T>`, `cstring`, `write_padded`, `overwrite`, using exactly the documented failure rules (`off > L` or `L − off < size`) | §Image primitives |
| `writeback.rs` | A7: write-back that replaces the inode (Q-FIX-18 kept) | A7 |

The path operations in §Conventions (`join`, `normalize`, `relative`, `filename`,
`parent_path`, element-wise comparison, `Final`, `OnDisk`, `RelativeTo`) go into
`jig-core::fspath`, because cc and rustc modes need the same `lexically_normal` semantics (07).
Every example in the doc becomes a test row, plus a proptest against the **legacy** binary's
behaviour, which is only reachable through whole-tree runs, so in practice the doc examples
plus the derived-and-confirmed path table are the oracle.

### Why not the `object` crate's writer

`object` gives the ELF and Mach-O type definitions and endian helpers (`object::elf::*`,
`object::macho::*`, `U32<LittleEndian>` …), and those are used for layout. But the fixup
must: never grow the file; ignore failed writes exactly where legacy ignores them; accept
truncated or overlapping structures the same way; and wrap offsets modulo 2⁶⁴. A general
writer does none of that, so all reads and writes go through `image.rs` with explicit
offsets, mirroring the spec's primitives one to one.

### Arithmetic

All offset arithmetic is `u64::wrapping_*`, as the spec says. A `u32` is widened before it is
subtracted from a 64-bit size. Clippy's `arithmetic_side_effects` lint is enabled for
`fixup/`, so any non-wrapping operator needs an explicit allow with a reason.

### Abort and UB cases (Q-FIX-10 kept)

| Legacy | Rust |
|---|---|
| `substr` out of range → SIGABRT (RELOCSTB check past EOF; signature slot `begin > L`) | `std::process::abort()` at the same point. Pending stdout is lost in legacy (stdio buffer not flushed on abort), so the Rust code also must **not** flush stdout before aborting. Use a `BufWriter` on stdout that is flushed only at normal exit, and test that the stdout bytes are empty. |
| throwing `std::filesystem` calls → SIGABRT | abort at the same points (walk increment errors other than EACCES, `exists` on EACCES/ELOOP, `fs::relative`). Message text: same flag as Q-DRV-10 in 06. |
| `pageSize ≥ 64` in a CodeDirectory: **undefined behaviour** in C++ | there is no behaviour to copy. Proposal: treat it like the hash-type error (message 13 style: error, file left unmodified). **Needs a yes/no** in P9; Mach-O only. |
| huge `nsects` → up to 2³² loop iterations | reproduce the loop's result. Iterating is cheap in Rust, and the result is the same. |
| `end < begin` in a slot → hashes the rest of the file | reproduce (with the wrapping length clipped at L, as specified). |

## Tests

1. All vectors under §Test vectors: `TestBinaryImage`, the CLI table, the deny check, path
   operations, and the ELF end-to-end case (GNU ld output from GCC 14).
2. **Golden-tree difftest:** link a matrix of executables and shared objects with the legacy
   driver (GNU ld and lld; with and without `crt`; with RUNPATH pads; libc and non-libc
   NEEDED; denied hashes), copy the tree twice, run legacy reloc-fixup on one copy and Rust
   on the other, then compare `diff -r` of the bytes, stdout, stderr and the exit status.
   This is the main gate.
3. **Load test:** run the fixed-up executables from a relocated copy of the tree (they must
   start), on the Linux CI hosts.
4. **Fuzz:** mutate fixed-up ELF files and compare Rust vs legacy outcomes
   (bytes/stdout/stderr/exit/signal) on each mutant (nightly). This is where Q-FIX-10/-11
   parity gets exercised.
5. P9 adds `TestMachOFixup`, the Mach-O re-signing vectors, and a Mach-O golden-tree matrix
   built with `ld64`/`lld -flavor darwin` (that can run on Linux with a cross toolchain).
