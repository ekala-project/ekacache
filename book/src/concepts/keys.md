# Cache keys

`ec` looks up every compile in two steps.

```text
k1 = H(tool, cwd, normalised args, primary source bytes)
        └─► manifest: one "path<TAB>identity" line per file the compile read

k2 = H(k1, manifest)
        └─► results: output file, stderr, depfile, exit status
```

`H` is BLAKE3, truncated to 128 bits.

## Why two keys

A compile's real inputs are not known until it has run: `foo.c` might include fifty headers,
and which ones depends on the flags and on what exists on disk. So:

1. **k1** covers what is known before compiling: the compiler, working directory, arguments
   and the source file's bytes. It names a *manifest*.
2. The **manifest** lists every file the previous compile read, taken from the compiler's
   own dependency file (or the linker's `--dependency-file`, or rustc's dep-info). For each
   file it gives an *identity*.
3. `ec` checks each manifest line against the filesystem now. If any identity differs, the
   lookup is a miss.
4. If all of them match, **k2** names the stored results of that exact set of inputs.

## Identities

| File | Identity | Cost |
|---|---|---|
| in the Nix store (for example a header of a dependency) | its path | free: store paths are immutable and the hash is in the path |
| in the build tree | a hash of its contents | read and hashed |

Using paths for store files is the default, *path mode*. A new version of a dependency has
a new store path, so it gives new keys.

### Content mode

With `EKACACHE_STORE_IDENTITY=content`, store hashes are masked out of keys and store files
are identified by their contents instead (`ecd` computes and remembers those hashes). Two
builds whose dependencies changed store path but not content can then share entries.
`EKACACHE_STORE_ROOTS` (a space-separated list of store paths) lets `ec` map masked paths
back to real ones when it replays a result. Only the exact value `content` selects content
mode; any other value means path mode.

## What is part of the key

For C and C++:

- the compiler binary and, for clang, the LLVM libraries next to it;
- the working directory;
- the arguments, normalised: `-o`, depfile options and the source path are taken out,
  paths are cleaned up lexically, and CMake's random `TryCompile-…`/`cmTC_…` names are
  masked;
- any flags `ec` itself injects (see [ekacache.json](../builds/driver-policy.md));
- `CPATH`, `C_INCLUDE_PATH`, `CPLUS_INCLUDE_PATH`, `OBJC_INCLUDE_PATH`, `GCC_EXEC_PREFIX`
  and `COMPILER_PATH`;
- for links, also `LIBRARY_PATH` and the `NIX_*` variables (except volatile ones such as
  `NIX_BUILD_TOP` and `NIX_BUILD_CORES`), since the bintools wrapper reads them;
- the source file's bytes.

For Rust, see [Rust](../builds/rust.md#what-is-in-the-key).

## What is not part of the key

- file modification times;
- the output path and the depfile's name, so the same compile with a different `-o` hits;
- other environment variables, including `SOURCE_DATE_EPOCH`. Sources that use
  `__DATE__`, `__TIME__` or `__TIMESTAMP__` are never stored for this reason.

## Forcing a miss

There is no switch for it. Change something in the key, for example add a `-D` flag that
nothing uses, or bypass the cache with `EKACACHE_SOCK=` (empty).
