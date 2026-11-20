# ec cache

Store and fetch files and directory trees in `ecd` under keys you choose. This is for build
scripts and fetchers that want to cache a step `ec` does not handle itself.

```text
ec cache put     <key> <file>
ec cache get     <key> <file>
ec cache put-dir <key> <dir>
ec cache get-dir <key> <dir>
```

All four take exactly two arguments after the verb. `ec cache` prints no usage message.

## Keys

The key is sent as given. Choose keys that:

- contain no whitespace (a key with whitespace silently fails);
- include a prefix of your own, such as `myproject/…`, so they cannot collide with the keys
  `ec` uses internally (`m/…`, `o/…`, `b/…`, `go/…`, …);
- include a hash of everything the cached content depends on. `ec cache` does no input
  checking of its own.

## put and get

`put` stores the file's bytes. `get` writes them to `<file>` through a temporary file and a
rename, with mode `0666 & ~umask`. `-` is an ordinary file name, not stdin or stdout.

| Case | Output | Exit |
|---|---|---|
| success | nothing | 0 |
| `get`: not in the cache, or the file cannot be written | nothing | 1 |
| `put`: the file cannot be read | nothing | 1 |
| `put`: `ecd` refused the value (for example, over 2 GiB or a full disk) | `ec: cache put <key> failed` | 1 |
| no daemon | nothing | 2 |
| wrong number of arguments | nothing | 1 |

Any verb other than `put`, `put-dir` and `get-dir` acts as `get`, so `ec cache gte k f`
fetches.

## put-dir

Stores a directory tree. Each file is stored once as a content-addressed blob, so trees that
share files share storage, and only blobs `ecd` does not already have are uploaded. A
listing of the tree is stored last, under `<key>`.

```text
ec: put-dir <key>: <files> files, <new> new
```

What is kept:

- regular files, with their permission bits (`0777` only: setuid, setgid and sticky are
  dropped);
- symlinks, with their target as written.

What is not: empty directories, directory modes, special files, and files that cannot be
read (skipped silently).

`put-dir` exits 0 even when some uploads fail; it prints `ec: cache put … failed` for the
first one. With no daemon it exits 2. A missing `<dir>` aborts `ec` (status 134 in a shell).

Don't change the tree while `put-dir` runs.

## get-dir

Restores a tree stored with `put-dir` into `<dir>`, creating directories as needed:

```text
ec: get-dir <key>: <n> files, <missing> missing
```

| Case | Exit |
|---|---|
| complete | 0 |
| listing not found | 1, nothing written |
| some files were evicted | 1, every file and symlink it wrote is removed again |
| no daemon | 2 |

When `get-dir` fails partway, it removes what it wrote, so a partial tree is never left
behind looking complete. Directories it created stay, and a file it overwrote is deleted
rather than restored. Files not in the listing are not touched.

> **Trust.** A listing is used as stored. A crafted listing can write outside `<dir>`. Only
> use `get-dir` with a cache you trust (see [Security and trust](../setup/security.md)).

## Store paths in text files

In the default path mode, files are stored and restored byte for byte. In content mode
(`EKACACHE_STORE_IDENTITY=content`), `put-dir` replaces `/nix/store/<hash>-` in text files
with a placeholder, and `get-dir` maps it back using `$EKACACHE_STORE_ROOTS` and `$out`.
Files containing a NUL byte are always stored unchanged.

## Example

```sh
key="mytool/vendor-$(sha256sum go.sum | cut -c1-32)"
if ! ec cache get-dir "$key" vendor; then
  go mod vendor
  ec cache put-dir "$key" vendor || true
fi
```

`get-dir` exits non-zero for a miss, a partial tree and a missing daemon alike, so the
fallback branch covers all three.
