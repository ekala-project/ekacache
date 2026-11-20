# Hooking ec into a build

A derivation never mentions the cache. Instead, the **package set** puts `ec` between the
build and the real compiler. This chapter covers the ways to do that. It is mostly for
package-set and toolchain maintainers.

## How ec decides what to do

`ec` picks a mode from the name it was invoked by (the basename of `argv[0]`):

| Invoked as | Mode |
|---|---|
| `ec <mode> …` | the mode named by the first argument (only when the binary is called exactly `ec`) |
| `ec /path/to/prog …` | **launcher mode**: the first argument contains a `/` |
| `cache`, `slot`, `rustcwrap`, `gocacheprog`, `reloc-fixup`, `nix-store` | that mode |
| anything else (`cc`, `gcc`, `clang++`, `x86_64-linux-gnu-gcc`, …) | C/C++ compiler mode |

Mode names are exact and case-sensitive. Any name that is not a mode is treated as a
compiler name. So `ec cahce get k f` runs C compiler mode with a compiler called `cahce`,
and a copy of the binary renamed to `ec.real` becomes a C compiler driver.

## Option 1: launcher mode

Prefix the real compiler with `ec`:

```console
$ ec /nix/store/…-gcc-15/bin/gcc -c foo.c -o foo.o
$ ec /nix/store/…-rustc/bin/rustc --crate-type lib …
$ ec /nix/store/…-go/bin/go build ./...
```

The program path must contain a `/`, because `ec` does no `PATH` lookup in this form. `ec`
classifies the program by its basename, after stripping a version suffix such as `-15` or
`-21.1`:

| Basename | Action |
|---|---|
| `cc`, `c++`, `gcc`, `g++`, `clang`, `clang++`, or a name ending in `-` plus one of those (`x86_64-unknown-linux-gnu-gcc`, `mpi-cc`) | C/C++ mode, with the program as the real compiler |
| `rustc` | Rust mode |
| `go` | sets `GOCACHEPROG` (see [Go](go.md)) and runs `go` |
| anything else (`mpicc`, `distcc`, `gfortran`, `clang-cpp`, …) | runs the program unchanged, with no caching and no slot |

Launcher mode injects no flags and reads no `etc/ekacache.json`. It is the simplest way to
add caching to an existing toolchain.

**Where to put it.** Put `ec` *under* nixpkgs' `cc-wrapper`, between the wrapper and the
real driver, so that `ec` sees the final arguments the wrapper settled on. In practice that
means the wrapper's call to the unwrapped compiler becomes `ec /path/to/unwrapped/gcc …`.

## Option 2: a compiler-named symlink

`libexec/ekacache/` in the `ekacache` package holds symlinks such as `cc`, `gcc` and
`clang++` that point at `ec`. Invoked through one of them, `ec` runs in C/C++ mode and has to
find the real compiler itself:

1. It resolves `/proc/self/exe` (so symlinks are followed to the real `bin/ec`) and looks
   for `etc/ekacache.json` two levels up, next to `bin/`.
2. If there is no such file, or `/proc/self/exe` is unavailable (for example on macOS), it
   uses `$EKACACHE_CC` as the compiler.
3. If neither exists, it fails:

   ```text
   ec: no etc/ekacache.json next to the binary and EKACACHE_CC unset
   ```

   and exits with status 1.

The `ekacache` package itself ships no `etc/ekacache.json`, so its `libexec` symlinks need
`EKACACHE_CC`:

```console
$ export EKACACHE_CC=/nix/store/…-gcc-15/bin/gcc
$ $ekacache/libexec/ekacache/gcc -c foo.c -o foo.o
```

A toolchain package that wants `ec` to inject flags, RUNPATHs and so on ships its own copy of
`ec` in `bin/` with an `etc/ekacache.json` beside it. See
[Toolchain packages and ekacache.json](driver-policy.md).

## Option 3: the language's own hook

- **Rust:** set `RUSTC_WRAPPER` to the `rustcwrap` symlink. See [Rust](rust.md).
- **Go:** set `GOCACHEPROG` to the `gocacheprog` symlink. See [Go](go.md).

## Language from the name

In C/C++ mode, the invoked name sets the language:

- contains `++`: C++;
- otherwise contains `fortran` or `flang`: Fortran (passed through, never cached);
- otherwise: C.

## What every build needs

- The socket mapped into the sandbox (`extra-sandbox-paths`, see
  [Running ecd](../setup/running-ecd.md)).
- Nothing else. `ec` uses the default socket, and if it is missing, compiles normally.

To turn the cache off for one build without changing the toolchain, set `EKACACHE_SOCK` to
an empty string. An empty socket path never connects.
