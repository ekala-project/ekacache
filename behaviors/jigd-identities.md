# jigd file identities (IDS)

The `IDS` command returns the content identity of files under the Nix store. jig uses these
identities to revalidate cache manifests. A cache hit checks about 100 headers, and hashing
them in every compiler process cost about 4 ms per hit. Store files are immutable once their
build has finished, so the daemon memoises identities by path and guards each entry with
(inode, size, mtime) for paths that are garbage-collected and rebuilt. Any sandboxed build can
reach the socket, so the daemon answers only for regular files that really lie inside the
store. Otherwise `IDS` would be a way to read host files. This document specifies what an
identity is, which paths get one, and how the memo behaves. Wire framing is in
[jigd-protocol.md §IDS](jigd-protocol.md#ids).

Sources: `jigd/identity.go` (`NewIdentities`, `stampOf`, `Identities.Of`, `Identities.Len`),
`jigd/peer_linux.go:fdUnder`, `jigd/peer_other.go:fdUnder`, `jigd/main.go` (`ids`, `NIX_STORE_DIR`,
STATS), `jigd/identity_test.go` (`TestIdentityMatchesJigAndTracksRewrites`,
`TestIdentityOnlyUnderStore`), `jigd/serve_test.go:TestServeIdsAndPipelinedGets`.
There is no dedicated Quint model. IDS framing is part of
[`spec/jigd_protocol.qnt`](../spec/jigd_protocol.qnt) (`Ids(n)` gives `RIds(n)`).

The key words MUST, MUST NOT, SHOULD, SHOULD NOT and MAY are used as in RFC 2119. Where this
document says "current behavior", that behavior is normative. The Rust implementation MUST
reproduce it unless the maintainer accepts the recommendation in the matching Quirk entry.

---

## The identity value

```
identity = "C:" + lowercase_hex(BLAKE3-256(file contents)[0..16])
```

- The hash is BLAKE3 in its default (unkeyed) hash mode, over the exact file bytes, truncated
  to the first 16 bytes (128 bits). That gives 32 lowercase hex digits, so an identity is
  always 34 bytes (`identity.go:83-84`, `lukechampine.com/blake3`).
- jig computes the same form itself (`"C:" + Digest(HashOf(...)).hex()`), so daemon and client
  identities can be compared directly.
- When there is no identity, the result is the empty string `""`, which goes on the wire as a
  bare `\n`.

Test vectors:

| Contents | Identity | Source |
|---|---|---|
| `abc` | `C:6437b3ac38465133ffb63b75273a8db5` | `identity_test.go:16`, `serve_test.go:64` |
| empty file | `C:af1349b9f5f9a1a6a0404dea36dcc949` | BLAKE3 of the empty input (derived) |

## Configuration

`NewIdentities(storeDir)` (`identity.go:38-44`) is called once at startup with `NIX_STORE_DIR`,
or with `/nix/store` if that is unset or empty (`main.go:210-214`). It computes:

- `store = Clean(storeDir) + "/"`, the prefix as configured;
- `realStore = Clean(EvalSymlinks(storeDir)) + "/"`, the prefix with symlinks resolved. If
  `EvalSymlinks` fails (for example, because the directory does not exist), the daemon uses
  `storeDir` unresolved. It resolves this **only once**: later changes to the symlinks are not
  seen.

## Algorithm: `Of(path)`

`path` is the raw IDS line minus its `\n`, with no trimming (`identity.go:52-89`):

```
1. if not path.starts_with(store) or Clean(path) != path: return ""
2. real = EvalSymlinks(path)                       # error -> return ""
   if not real.starts_with(realStore): return ""
3. f = open(real, O_RDONLY)                        # error -> return ""
4. info = fstat(f)                                 # error -> return ""
   if not info.is_regular_file: return ""
   if not fdUnder(f, realStore): return ""         # readlink(/proc/self/fd/N) must start with realStore
5. stamp = (st_ino, st_size, mtime in unix ns)     # taken from the fstat of the opened fd
6. if memo[path] exists and memo[path].stamp == stamp: return memo[path].id
7. data = read exactly st_size bytes from f        # short read / error -> return ""
8. id = "C:" + hex(blake3(data)[:16])
9. memo[path] = (stamp, id)                        # overwrites any older entry for this path
10. return id
```

Details a reimplementation MUST keep:

- **Lexical check first** (step 1). Go's `filepath.Clean` rejects `//`, `/./`, `/../`, a
  trailing `/` and the store directory itself (`/nix/store/` cleans to `/nix/store`). A path
  must not be relative, and it must begin with the configured prefix, including its trailing
  `/`, so `/nix/storex/...` is rejected (`TestIdentityOnlyUnderStore`).
- **Symlinks are followed** (step 2). A link inside the store that points to another file inside
  the store is answered: `inside -> h.h` gets the identity of `h.h`. A link that points out
  of the store, or through a directory symlink that leaves the store, gets `""`
  (`TestIdentityOnlyUnderStore`: `link -> ~/id_ed25519`, `dir/id_ed25519`).
- **TOCTOU guard** (steps 3 and 4). The daemon checks type and location on the **opened
  descriptor**: `fstat` and `/proc/self/fd/N`. A build that swaps a directory for a symlink
  between the path lookups cannot redirect the read. On non-Linux builds `fdUnder` always
  returns true (`peer_other.go:13`).
- **Memo key** is the path as requested, not the resolved path. Two paths that resolve to the
  same file get two entries.
- **Stamp** is (inode, size, mtime in ns) of the file actually opened. Device number, ctime
  and mode are not part of it (Q-ID-3).
- **Read size** is the size from `fstat`. If the file grew after the stat, only the first
  `st_size` bytes are hashed. If it shrank, the read is short and the result is `""`.
- **Failures are not memoised**: every request for an unanswerable path repeats steps 1 to 4.
- **Errors give `""` and are never logged.**
- jig asks only about paths outside its own `$out` (`identity.go:5`), but the daemon does not
  enforce that. Any store path that passes the checks is answered.

## The memo

- It is a `map[string]{stamp, id}` behind an RWMutex. Lookups take the read lock and inserts
  the write lock. The file is hashed with no lock held.
- **It is never pruned.** Entries are only ever added or overwritten (for the same path).
  Nothing removes the entries of paths that were garbage-collected (Q-ID-1).
- `Len()` returns the number of entries, and **STATS reports it as `ids=`**
  (`main.go:149`). `TestIdentityMatchesJigAndTracksRewrites` pins that a repeated lookup
  does not add an entry (`ids.Len() == 1`) and that rewriting the file is noticed (a new size
  gives a new stamp, so the file is hashed again and the entry overwritten).
- When two connections miss on the same path at the same time, both hash the file, and the
  last write wins (Q-ID-4).

---

## Quirks

### Q-ID-1: The identity memo is never pruned

**Current behavior**: Every distinct path that was ever answered stays in memory (path, about
24 bytes of stamp, and a 34-byte id) until the daemon exits. Garbage-collected store paths
are never removed. `ids=` in STATS grows without bound.

**Impact**: Memory grows slowly with the number of distinct headers seen. On a long-running
build host this can reach millions of entries, a few hundred MB.

**Recommendation**: keep as-is. A bound (LRU, or pruning entries whose path no longer
exists) changes `ids=` semantics and needs sign-off.

### Q-ID-2: The whole file is read into memory

**Current behavior**: On a miss the daemon allocates `st_size` bytes and reads the whole file
before hashing it. Nothing limits the size.

**Impact**: Any sandboxed build can ask for the identity of a multi-GB store file and make
the daemon allocate that much, many times over in parallel.

**Recommendation**: change, needs sign-off: hash in streaming fashion. The identity is the
same, so this changes no visible bytes and could arguably be done silently. It is listed
here because it changes memory behavior.

### Q-ID-3: The stamp ignores the device and ctime

**Current behavior**: The memo stays valid while (inode, size, mtime ns) is unchanged.
A file replaced by another with the same inode number (on a different device, or an inode
reused after GC), the same size and the same mtime gets the stale identity. Nix normalises
store mtimes to 1 (epoch + 1 s), so the mtime rarely tells files apart. Inode reuse after GC
plus an equal size is enough.

**Impact**: Possible stale identity after a store path is collected and a different file is
rebuilt at the same path. Store paths are input-addressed or content-addressed, so the same
path usually means the same content.

**Recommendation**: keep as-is.

### Q-ID-4: Concurrent misses hash twice

**Current behavior**: A miss computes the hash with no lock held. Concurrent misses for one
path each hash the file, and the last write wins.

**Impact**: Wasted CPU only.

**Recommendation**: keep as-is.

### Q-ID-5: Edge cases in the store directory

**Current behavior**: `NIX_STORE_DIR=/` gives the prefix `//`, which no cleaned path can start
with, so every IDS answer is `""`. A relative `NIX_STORE_DIR` gives a relative prefix, so only
relative request paths can match. The real path is resolved once at startup.

**Impact**: Configuration surprises only.

**Recommendation**: keep as-is.

## Open questions

1. Should identities be answered for paths inside the asking build's own `$out`? The daemon
   cannot know which build is asking, and the comment says jig never asks. Nothing enforces
   it.
2. Is the 128-bit truncation part of the manifest format (normative), or an internal choice?
   It must match jig's own `Digest`, which is outside this document's scope.
