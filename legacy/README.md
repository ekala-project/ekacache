# jig

A compile cache for Nix builds, packaged for use with
[ekapkgs](https://github.com/ekala-project) (core-pkgs).

## Origin

jig and jigd were written by Jörg Thalheim ([@Mic92](https://github.com/Mic92)) and the repkgs
contributors as part of [repkgs](https://github.com/Mic92/repkgs), where they live under
`pkgs/ji/jig` and `pkgs/ji/jigd`. This repository was extracted from repkgs
(at `e92484a`) with `git filter-repo`, so their full commit history and authorship are
preserved here. Changes made after the extraction:

- a standalone Meson build for jig, with jigd as a plain Go module
- builds with GCC and libstdc++ as well as clang and libc++
- launcher mode (`jig /path/to/<compiler> <args>`), so jig can sit under nixpkgs-style
  cc-wrapper and rustc-wrapper, and in front of `go`
- GCC as the compiler being cached
- cache keys that account for the environment read by nixpkgs-style wrappers

The original MIT license and copyright are kept, see [LICENSE](LICENSE).

## Overview

- **jig** (`jig/`, C++26): sits in front of a compiler. It caches C/C++ objects, configure
  probes, links and replayable failures, rustc library crates, and Go build actions
  (`GOCACHEPROG`).
- **jigd** (`jigd/`, Go): the per-machine daemon behind the cache socket. It holds an
  append-only pack store and hands out compiler slots so N sandboxes × `make -jN` don't
  oversubscribe the machine.

Derivations never mention the cache. If the socket is mapped into the sandbox, jig asks jigd
first. Otherwise it compiles as usual, so outputs are the same either way.

## How keys work

```
k1 = H(tool, cwd, normalised args, primary source bytes)  -> manifest: "input\tid" per file read
k2 = H(k1, manifest)                                       -> object, stderr, depfile, status
```

The inputs come from the compiler's own depfile (`-MD`) and, for links, the linker's
`--dependency-file`. A manifest is valid when every input still has the same identity. Store
files are identified by path, build-tree files by content.

## Building

```console
$ nix-shell
$ meson setup build -Dstore_dir=/nix/store && meson test -C build
$ (cd jigd && go test ./...)
```

## Running

```console
$ jigd /nix/var/nix/jigd/socket    # JIGD_SIZE (GiB, default 50), JIGD_SLOTS (default: CPUs)
```

```
# nix.conf
extra-sandbox-paths = /nix/var/nix/jigd
```

jigd only serves its own user, root, and Nix build users. **Any build that can reach the socket
can write entries that other builds will replay**, so run it only where you trust everyone who
can start a build.

## License

MIT, see [LICENSE](LICENSE).
