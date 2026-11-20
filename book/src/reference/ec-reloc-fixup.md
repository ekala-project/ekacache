# ec reloc-fixup

Rewrite an output tree's binaries so that they keep working when the tree is copied
somewhere else.

```text
usage: reloc-fixup <prefix> [--dest <store path>] [--sdk <dir>] [--deny <store path>]...
```

Invoke it as `ec reloc-fixup …` or through the `reloc-fixup` symlink.

## What it does

- **ELF** (64-bit little-endian):
  - makes RUNPATH entries relative to `$ORIGIN`;
  - turns NEEDED entries into direct `$ORIGIN/…` paths where they fit;
  - rewrites the interpreter path to one that the entry stub resolves relative to the
    binary.
- **Mach-O** (64-bit little-endian): rewrites dylib, rpath and id load commands relative to
  `@loader_path`, and refreshes the ad-hoc code signature.
- **Every file:** reports references to *denied* store paths, for example build-platform
  paths that must not end up in a cross-compiled output.

`reloc-fixup` never makes a file bigger. It relies on room that `ec`'s link driver reserves
(see [ekacache.json](../builds/driver-policy.md)): a padding RUNPATH entry, a padded
interpreter path, and `-headerpad_max_install_names` on Mach-O. On binaries linked any
other way, rewrites that don't fit are errors.

## Arguments

| Argument | Meaning |
|---|---|
| `<prefix>` | where the tree is now; always the first argument |
| `--dest <path>` | where the tree will finally live; must be under the store directory. Default: `<prefix>` |
| `--sdk <dir>` | macOS SDK root; system libraries count as present only if the SDK has a `.tbd` for them |
| `--deny <path>` | repeatable; any file containing the hash part of this store path is an error |

Flags take their value as the next argument; `--flag=value` is not accepted. `etc/ekacache.json`,
`etc/roots` and `*.debug` files are exempt from `--deny`.

Pass `--deny` paths without a trailing slash. `--deny /nix/store/<hash>-x/` matches every
file.

## Examples

```console
$ ec reloc-fixup "$out" --deny "$buildPlatformLibc"
$ ec reloc-fixup /build/stage --dest /nix/store/<hash>-foo-1.0
```

## Output

stdout has one line per rewritten file, in the form
`<file>  RUNPATH <new runpath>  NEEDED <target>…` for ELF, and warnings such as
`  WARN absolute store ref in <file> @0x<offset>: <text>`. Errors go to stderr. Every
file is processed even after an error.

## Exit status

| Exit | Meaning |
|---|---|
| 0 | no errors. **Also when `<prefix>` does not exist**, so check the path |
| 1 | at least one file had an error |
| 2 | usage error, unknown argument, or `--dest` not under the store (`reloc-fixup: <dest> is not under /nix/store`) |

A malformed binary can abort the whole run.

## Common errors

| Message | Meaning |
|---|---|
| `reloc-fixup: <file> refers to build-platform <path>` | a `--deny` hit |
| `<file>: RUNPATH does not fit (…)` | the binary was not linked with `ec`'s padding |
| `<file>: interp does not fit: …` | the same, for the interpreter path |
| `<file>: NEEDED <soname> not found in RUNPATH […] nor under <prefix>` | a library dependency cannot be found |
| `reloc-fixup: <file>: load commands need <n> bytes, the header has room for <m> (-headerpad)` | Mach-O header too small |
| `reloc-fixup: <file>: links <path>, which neither the package, a dependency nor the SDK has` | Mach-O dependency missing |

## Side effects

Files are replaced with a temporary file and a rename. That breaks hard links, gives the
file to the running user, and drops extended attributes and ACLs.

Only glibc's dynamic loader is recognised.
