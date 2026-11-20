# Correctness and limits

## Why outputs match an uncached build

- **Derivations don't change.** The cache is not an input to any derivation, so store
  paths are the same with or without it.
- **Every hit is checked.** A result is replayed only if every file the original compile read
  still has the same identity.
- **A hit is a real compile.** The replayed bytes were produced by the same compiler with the
  same arguments and inputs.
- **Failures fall back.** Any error talking to `ecd` turns into an ordinary compile.

The two-key scheme is modelled formally in the repository (`spec/twokey.qnt`). The model
shows it is sound under two assumptions, which point at the limits below: that the compiler
reports every file it **looked for and did not find**, and that inputs do not change while a
compile runs.

## Known limits

**New headers that shadow old ones.** Suppose a compile included `/a/foo.h`, and a later
build adds `/b/foo.h` earlier on the include path. The compile should now pick up the new
file. Spotting that requires the compiler to report lookups that failed, and stock GCC and
clang do not. `ec` will replay the old result. In a Nix sandbox this needs an include
directory to gain a file between builds while the compile's own inputs and flags stay
the same. That is unusual, but possible with headers generated into the build tree.

**Inputs edited during a compile.** `ec` hashes the inputs after the compile finishes. A
file changed while the compiler ran is recorded with its new contents.

**Paths through symlinked directories.** Arguments are normalised lexically, so `a/../b`
becomes `b` even when `a` is a symlink to somewhere else.

**Stale depfiles.** Passing `-MF`, `-MT`, `-MQ` or `-MP` without `-MD`/`-MMD` makes `ec`
treat an existing `.d` file at that path as the compiler's output. A stale one from an
earlier build can then become the manifest.

**Side outputs `ec` does not recognise** are not replayed. The known case is the
separate-argument linker form `-Xlinker -Map -Xlinker file`.

**File modes.** Replayed outputs get mode `0666 & ~umask`, plus executable bits for links,
whatever mode the compiler gave them.

**Replayed failures.** Deterministic compile failures are cached. A failure caused by
something other than the inputs replays until an input changes (see
[Troubleshooting](../operating/troubleshooting.md#a-failure-keeps-being-replayed)).

**Trust.** All of the above assumes the entries were written by honest builds. See
[Security and trust](../setup/security.md).

## Reporting a problem

If you suspect a wrong hit:

1. Rebuild with `EKACACHE_SOCK=` (empty) and compare the outputs.
2. Rebuild with `EKACACHE_LOG` and `EKACACHE_LOG_ARGS` set, and include the lines for the
   affected files.
3. Open an issue with both.
