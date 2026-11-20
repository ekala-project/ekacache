# Installing

## Platforms

| Platform | Status |
|---|---|
| Linux x86_64, aarch64 | supported and tested |
| macOS aarch64, x86_64 | builds, but has not yet run on a Mac; experimental. On macOS, `ecd` does no peer authentication (see [Security and trust](security.md)). |

## With Nix

The repository's `default.nix` has two attributes:

```console
$ nix-build -A ekacache     # ec, plus ecd in a second output
$ nix-build -A ekacached    # ecd only
```

The `ekacache` package has two outputs:

| Output | Contents |
|---|---|
| `out` | `bin/ec`, and `libexec/ekacache/<name>` symlinks to `../../bin/ec` |
| `ekacached` | `bin/ecd` |

The `libexec/ekacache/` directory holds one symlink per name `ec` answers to:

- the modes: `cache`, `slot`, `rustcwrap`, `gocacheprog`, `reloc-fixup`, `nix-store`;
- the compiler names: `cc`, `c++`, `gcc`, `g++`, `clang`, `clang++`, `gfortran`.

The symlinks are in `libexec` rather than `bin` on purpose. Installing the package into a
profile puts only `ec` on `PATH`, not a second `cc` or `gcc`.

## With cargo

Inside the repository's `nix-shell`:

```console
$ cargo build --release
$ ls target/release/{ec,ecd}
```

A cargo build gives you no `libexec` symlink farm. Use launcher mode
(`ec /path/to/gcc …`) or `ec <mode> …` instead.

## The store directory is fixed at build time

`ec` reads `EKACACHE_STORE_DIR` **when it is compiled**, not when it runs. The default is
`/nix/store`, and `default.nix` sets it to `builtins.storeDir`. Two things derive from it:

- the cache keys, which mask store paths under that directory;
- `ec`'s default socket path, `<dirname of the store dir>/var/nix/ekacache/socket`, which
  is `/nix/var/nix/ekacache/socket` for `/nix/store`.

If your Nix store is somewhere other than `/nix/store`, build `ec` against that store. `ecd`
does not use this constant. It reads `NIX_STORE_DIR` at runtime (see
[Configuring ecd](configuring-ecd.md)).
