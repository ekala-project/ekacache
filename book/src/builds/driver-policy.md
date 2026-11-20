# Toolchain packages and ekacache.json

This chapter is for maintainers who package a C/C++ toolchain around `ec`. If you use launcher
mode or `EKACACHE_CC`, you can skip it.

A toolchain package can make `ec` a full compiler driver. Besides caching, `ec` then:

- injects the package set's flags;
- computes the RUNPATH of linked binaries;
- sets the dynamic linker;
- reserves space so that [`ec reloc-fixup`](../reference/ec-reloc-fixup.md) can later make
  the binaries relocatable.

## Where ec finds it

The package contains a real copy of `ec` (not a symlink to another package) at `<root>/bin/ec`,
compiler-named symlinks to it, and `<root>/etc/ekacache.json`. On Linux, `ec` resolves
`/proc/self/exe` and reads `<root>/etc/ekacache.json`. On macOS there is no
`/proc/self/exe`, so the file is never found and only `EKACACHE_CC` works.

If the file exists but has no `cc` key, `ec` does **not** fall back to `EKACACHE_CC`. It
reports `ec: no etc/ekacache.json next to the binary and EKACACHE_CC unset`, which is
misleading in this case.

## Format

A JSON object. Unknown keys are ignored, and so are values of the wrong type.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `cc` | string | | the real C/C++ compiler (required) |
| `fc` | string | | the real Fortran compiler |
| `binfmt` | string | `elf` | `elf`, `macho` or `coff` |
| `libc` | string | | libc root; `<libc>/lib` goes into RUNPATH and holds the dynamic linker |
| `interp` | string | `ld-linux-x86-64.so.2` | the dynamic linker's file name |
| `crt` | string | | an entry stub object, which makes the interpreter path relocatable |
| `runtimes` | string | | directory of libc++/libunwind, added to RUNPATH when a C++ runtime is linked |
| `flags` | string array | `[]` | prepended to C and C++ compiles |
| `cxxflags` | string array | `[]` | prepended to C++ compiles |
| `fflags` | string array | `[]` | for Fortran; `@/` expands to the package root |
| `prefix-map` | string array | `[]` | each becomes `-ffile-prefix-map=…` |

Errors exit with status 2:

```text
ekacache.json: not a JSON object
ekacache.json: binfmt <value> is none of elf, macho, coff
```

A Fortran compile with no `fc` fails with `ec: <name> called but etc/ekacache.json names no
fc` (status 1).

## Per-package settings

Two environment variables, usually set by the package set's setup hooks, add per-package
flags:

- `PKGS_CC`: JSON keyed by the toolchain root, `{"cflags": …, "cxxflags": …, "ldflags": …}`.
  ldflags are only added to links.
- `PKGS_PREFIX_MAP`: a `:`-separated list, each entry becoming `-ffile-prefix-map=…`.

## What the ELF link policy does

- Keeps `-Wl,-rpath` values from the build, minus host directories (`/usr`, `/lib`,
  `/opt` and anything starting with those).
- Adds store directories that provide linked `.so` files, the `runtimes` directory and
  `<libc>/lib`.
- Adds a padding entry, so that `ec reloc-fixup` can rewrite the RUNPATH in place.
- Drops any `-Wl,--dynamic-linker…` from the build and sets its own.
- Leaves `-static`, `-static-pie`, `-r` and `-nostartfiles` links alone.

Every injected argument is part of the cache key.
