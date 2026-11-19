# jig cache and slot subcommands

`jig cache …` gives fetchers and build scripts direct access to jigd: plain blobs
(`get`/`put`) and whole directory trees stored as content-addressed file blobs plus a listing
(`get-dir`/`put-dir`). `jig slot <program> …` runs a program while holding a jigd job slot.
This document specifies the argv grammar, stdin/stdout/stderr bytes, exit codes, the
directory listing and `b/` blob formats, and the behavior of `get-dir` on a miss or a partial
tree, so that a Rust implementation can be a drop-in replacement. Value compression and the
socket protocol are in `behaviors/jig-cache-client.md`. Slot semantics on the daemon side are
in `behaviors/jigd-slots.md`. Store-hash masking (`MaskHashes`, `ResolveAll`) is in
`behaviors/jig-keys.md`.

Sources: `jig/src/main.cc` (`RunCacheMode`, `RunSlotMode`, `main`), `jig/src/dir_mode.h`,
`jig/src/dir_mode.cc`, `jig/src/cache_client.cc` (`Compress`, `HasMany`, `PutMany`,
`GetMany`), `jig/src/process.cc` (`Slot`, `Run`), `jig/src/store.cc` (`MaskHashes`,
`ResolveAll`), `jig/src/base.cc` (`HashOf`, `ReadFile`, `WriteFile`, `SplitWhitespace`,
`ParseUint`), `jigd/main.go` (request parsing).

Key words MUST, MUST NOT, SHOULD and MAY are as in RFC 2119. Current behavior is normative.
Line numbers refer to `jig/src/dir_mode.cc` unless another file is named.

---

## Dispatch (`main.cc:93-133`)

1. `socket_path = Env("JIG_SOCK", <StateDir>/jigd/socket)`. `StateDir` is the store dir with
   its last component replaced by `var/nix`, so `/nix/store` → `/nix/var/nix`. This is computed
   for every mode.
2. `mode` = basename of argv[0]. `args` = argv[1..].
3. If `mode == "jig"`:
   - no args → usage text (below) to **stderr**, exit **2**;
   - `args[0]` is `--help` or `-h` → usage to **stdout**, exit **0**;
   - otherwise `mode = args[0]`, `args = args[1..]`. If `mode` contains `/` → launcher mode.
4. The first match wins: `nix-store`, `cache`, `slot`, `gocacheprog`, `rustcwrap`,
   `reloc-fixup`. Anything else → cc mode with `argv0 = mode`.

`cache` and `slot` can therefore also be reached through symlinks of those names.

Usage text (exact; each line ends in `\n`):

```
usage: jig <mode> [args...]   or via a symlink named after the mode
       jig /path/to/<compiler> [args...]   launcher for gcc, clang, rustc or go

  cc c++ <args>         compiler driver + compile cache (any other name too)
  rustcwrap <args>      rustc through the cache
  gocacheprog           GOCACHEPROG server on stdin/stdout
  reloc-fixup <dir>     rewrite store references in an output tree
  nix-store <args>      realise paths over the builder RPC socket
  cache get|put <key> <file>, get-dir|put-dir <key> <dir>
  slot <program> <args> run holding a jigd job slot

  JIG_SOCK  jigd socket (default <store>/../var/nix/jigd/socket)
  JIG_LOG, JIG_LOG_ARGS  per-invocation log files
```

---

## `jig cache` (`RunCacheMode`, `main.cc:37-62`)

Grammar: `jig cache <verb> <key> <path>`. Exactly three arguments are required.

| Condition, checked in order | stdout | stderr | Exit |
|---|---|---|---|
| argument count ≠ 3 | — | — | **1** |
| verb `put-dir` | — | see §put-dir | `PutDir` |
| verb `get-dir` | — | see §get-dir | `GetDir` |
| `Connect(socket)` fails | — | — | **2** |
| verb `put`, file readable | — | — | **0** (whatever the daemon replied) |
| verb `put`, file unreadable | — | — | **1** |
| any other verb (treated as `get`), hit, file written | — | — | **0** |
| treated as `get`, miss, or write fails | — | — | **1** |

- `put`: `PUT <key>` with the file's bytes, unmodified (no masking). The daemon's reply is read
  and ignored.
- `get`: `GET <key>`, then `WriteFile(path, value)` (temp `<dir>/.jig<pid>.<name>` + rename,
  mode `0666 & ~umask`). `-` is a file name. stdout is never used.
- stdin is never read. Nothing is printed on any path.
- `<key>` is sent verbatim. No prefix is added, so these keys share the namespace with
  `m/`, `o/`, `b/`, `go/…` (Q-DIR-6).

---

## Directory trees

### Storage layout

| Key | Value (before client compression) |
|---|---|
| `<key>` (caller-chosen, verbatim) | the **listing** text (below) |
| `b/<hash>` | one regular file's **stored** content |

`<hash>` = `HashOf(stored).hex()`: 32 lowercase hex characters, the first 16 bytes of BLAKE3
over the stored bytes, **unframed** (`Hasher::Update`, not `Field`, `base.cc:287`).

**Stored content** (`Stored`, `:71-73`): if the file contains no NUL byte ("text"),
`Store::MaskHashes(data)`, so every `<storedir>/<32-char nix hash>-` becomes `<storedir>/*-`.
This happens in **both** identity modes (Q-DIR-1). Otherwise ("binary") the bytes are stored
unchanged.

**Compression**: neither format is compressed by `dir_mode` itself. Like every jig value, the
listing and each blob go on the wire as a 4-byte host-order uncompressed length followed by
one zstd frame produced by `ZSTD_compress(…, level 1)` (`cache_client.cc:285-301`). See
`jig-cache-client.md`. Keys and hashes are computed over the uncompressed bytes, so the exact
compressed bytes do not affect compatibility as long as they decompress to the same value.

### Listing format

UTF-8/bytes, one line per entry, each terminated by `\n`. Directories are not listed: they are
implied by paths. Empty directories, directory modes, and special files (fifos, sockets,
devices) are not represented.

| Kind | Line |
|---|---|
| regular file | `f` `0x20` `<mode>` `0x20` `<hash>` `0x09` `<relpath>` `0x0a` |
| symlink | `l` `0x20` `0` `0x20` `<target>` `0x09` `<relpath>` `0x0a` |

- `<mode>` = `st_mode & 0777` in octal, no leading zero (`644`, `755`, `0` for mode 000).
  setuid, setgid and sticky bits are dropped.
- `<target>` = `readlink` of the symlink, verbatim. Store hashes are **not** masked.
- `<relpath>` = `path.lexically_relative(root)`, with no leading `./`.
- No escaping of any field.

Example for a tree with `bin/tool` (0755, binary), `share/a.txt` (0644) and
`lib -> /nix/store/…-x/lib`:

```
f 755 0f3c…(32 hex)	bin/tool
f 644 9a1e…(32 hex)	share/a.txt
l 0 /nix/store/abc…-x/lib	lib
```

**Entry order** is the order of `std::filesystem::recursive_directory_iterator(root)`:
pre-order, with each directory's entries in `readdir` order. It is **not sorted** and depends
on the filesystem (Q-DIR-2). Symlinks to directories are listed as `l` and not descended into.

### `put-dir <key> <dir>` (`PutDir`, `:101-139`)

1. `Connect` fails → exit **2**, before the directory is touched.
2. Walk `recursive_directory_iterator(dir)`, using the throwing overloads (Q-DIR-4). For each
   entry:
   - symlink → append an `l` line;
   - not a regular file → skip;
   - `ReadFile` fails → skip silently (not listed);
   - otherwise `stat()` (return value ignored), `hash = HashOf(Stored(data))`, append an `f`
     line, queue `{b/<hash>, path}` and increment `files`.
   - When 256 entries are queued: Upload (below) and clear the queue.
3. Upload the remainder.
4. `PUT <key>` = listing. An empty tree gives an empty listing, which is still PUT.
5. stderr: `jig: put-dir <key>: <files> files, <uploaded> new\n`. Exit **0**, even if PUTs
   failed.

**Upload(batch)** (`:79-97`): one pipelined `HAS b/<hash>` per queued entry, and jigd answers
`1` or `0` per line. For each entry whose answer is not `1`, the file is **read again** and
`Stored()` is applied (Q-DIR-3). All of those are sent in one pipelined `PutMany`. Duplicate
hashes within one batch are uploaded twice and counted twice in `new`. Unreadable-on-second-
read files are skipped without affecting the listing.

### `get-dir <key> <dir>` (`GetDir`, `:141-194`)

1. `Connect` fails → exit **2**.
2. `GET <key>` misses → exit **1**. Nothing is printed and nothing is created.
3. **Parse** (`ParseListing`, `:45-61`): split the listing on `\n`, dropping empty lines. For
   each line: `tab` = first `\t`. `head = SplitWhitespace(line[0:tab])` (separators space,
   tab, `\n`). A line with no tab, or with a `head` of other than 3 fields, is **skipped**.
   Otherwise:
   - `kind` = the first byte of field 1;
   - `mode` = `ParseUint(field 2, octal)`, else `0644`;
   - `ref` = field 3;
   - `rel` = everything after the first tab.
4. **Symlinks first**: for every `l` entry in listing order: `create_directories(parent)`,
   `remove(dest)` (a file, symlink or empty directory), `create_symlink(ref, dest)`. All errors
   are ignored. `dest` is recorded as written, even if creation failed.
5. Entries whose kind is neither `l` nor `f` are discarded.
6. **Files** in batches of 256, in listing order: one pipelined `GetMany(b/<ref> …)`. For each
   entry: `create_directories(parent)`. If the blob is missing or `WriteFile(dest,
   Restored(blob))` fails, increment `missing`. Otherwise record `dest` and
   `chmod(dest, mode)`, ignoring the result. All 12 low bits are applied as parsed, not masked
   to 0777.
   **Restored** (`:74-76`): a blob with no NUL byte goes through `Store::ResolveAll`. In
   content mode that maps `<storedir>/*-name` back to this build's `$JIG_STORE_ROOTS`/`$out`.
   In path mode nothing is mapped back (Q-DIR-1). Binary blobs are restored unchanged.
7. stderr: `jig: get-dir <key>: <number of f entries> files, <missing> missing\n`. This is
   always printed once the listing was found.
8. If `missing != 0`: **remove every recorded path** (symlinks and files written by this run),
   then exit **1**. Directories created along the way are left behind. A pre-existing file that
   was overwritten is deleted, not restored. Files in `dir` that are not in the listing are
   never touched.
9. Otherwise exit **0**.

The rollback in step 8 exists because blobs are evicted independently of listings: a partial
tree must not pass for a warm one. A connection failure mid-way makes every remaining
`GetMany` answer a miss, so it also rolls back.

An empty listing creates nothing (not even `dir`), prints `… 0 files, 0 missing` and exits 0.

---

## `jig slot <program> [args…]` (`RunSlotMode`, `main.cc:66-74`)

| Condition | stderr | Exit |
|---|---|---|
| no program | `usage: jig slot <program> [args...]\n` | **2** |
| otherwise | the program's own | the program's status |

1. `Slot(cache, socket_path)`: no-op if `$JIG_SLOT` is non-empty. Otherwise connect (no daemon
   → run without a slot), send `SLOT <$NIX_BUILD_TOP or "-">`, block until `OK`, and set
   `JIG_SLOT=1` for the child.
2. `Run(program, args, kInherit)`: `fork` + `execvp` (PATH search), with stdin, stdout and
   stderr inherited. If exec fails the child exits **127 with no message**. Death by signal N
   gives `128+N`.
3. The Slot destructor sends `DONE <build>`, waits for the reply, and unsets `JIG_SLOT`.

---

## Quirks

### Q-DIR-1: Text blobs are always hash-masked, but only resolved in content mode

**Current behavior**: `Stored` calls `MaskHashes` unconditionally. `Restored` calls
`ResolveAll`, whose mapping table is empty unless `JIG_STORE_IDENTITY=content`.

**Impact**: in path mode, `put-dir`/`get-dir` of a text file containing store paths returns
`/nix/store/*-name/…`. The round trip is lossy. In content mode, a masked root the build lacks
also stays `*-name`. Detection of "text" (no NUL) also catches binary formats without NULs.

**Recommendation**: **ADOPTED (signed off 2026-11-17).** See `jig-keys.md` Q-KEY-1: in path mode `put-dir` stores text files unmasked and `get-dir` writes them back unchanged; content mode is unchanged.

### Q-DIR-2: Listing order is filesystem order

**Current behavior**: entries are listed in `readdir` order.

**Impact**: the same tree can produce different listing bytes on different filesystems or
runs. Nothing hashes the listing, so it only affects byte-for-byte comparison of stored
listings.

**Recommendation**: keep as-is for compatibility. A Rust port MAY sort, because listing bytes
are never keyed. That needs sign-off.

### Q-DIR-3: Blobs are read twice (TOCTOU)

**Current behavior**: the hash in the listing comes from the first read. The uploaded blob
comes from a second read in `Upload`.

**Impact**: a file modified during `put-dir` is stored under a hash that does not match its
content.

**Recommendation**: keep as-is. Inputs are expected to be quiescent.

### Q-DIR-4: Filesystem errors during `put-dir` abort the process

**Current behavior**: `recursive_directory_iterator(root)`, `read_symlink`, `is_symlink` and
`is_regular_file` use the throwing overloads in a `-fno-exceptions` build. A missing `<dir>`,
or an unreadable subdirectory, terminates the process (SIGABRT, with the C++ runtime's
"terminate called…" message) after it has connected and possibly uploaded blobs, but before
the listing is PUT.

**Impact**: the exit status is 134 from a shell. There is no jig message.

**Recommendation**: change, needs sign-off. Exit 1 with a message. A Rust port MUST at least
not PUT the listing in that case.

### Q-DIR-5: Listings are trusted, with no path sanitising

**Current behavior**: `rel` may be absolute or contain `..` (`root / rel` with an absolute
`rel` discards `root`). Symlinks are created before files, so an `f` entry under an `l` path
writes through the symlink.

**Impact**: anyone who can write jigd entries can make `get-dir` write anywhere the caller can.

**Recommendation**: change, needs sign-off. Reject absolute and `..` paths and refuse to write
through symlinks created by the same listing.

### Q-DIR-6: `jig cache` corner cases

**Current behavior**:
- a wrong argument count exits 1 with no message;
- any verb other than `put`, `put-dir` and `get-dir` (including typos) behaves as `get`;
- `put` exits 0 even if the daemon dropped the PUT;
- keys are not prefixed. jigd splits request lines with `strings.Fields`, so a key containing
  whitespace makes jigd hang up, which jig reports as a miss or an ignored put;
- symlink targets containing whitespace produce a listing line that `get-dir` skips.

**Impact**: silent failures, and possible collisions with internal key spaces.

**Recommendation**: keep as-is.

### Q-DIR-7: `jig slot` exec failure is silent

**Current behavior**: an unknown program exits 127 with nothing on stderr (`process.cc:522-523`).

**Impact**: confusing diagnostics.

**Recommendation**: keep as-is.

---

## Open questions

1. Which callers use `put-dir`/`get-dir` (the header mentions `nix/fetch.nix` goModules), and
   in which identity mode? This decides Q-DIR-1.
2. Should empty directories and directory modes be preserved? They are lost today.
3. Is the 2 GiB receive limit (`kMaxObjectSize`) and the 32-bit length header acceptable for
   single files in a tree? Larger files fail to round-trip.
