# ekacache

ekacache is a compile cache for [Nix](https://nixos.org) builds. It speeds up rebuilds by
replaying compiler results the machine has already produced, across derivations and across
sandboxes. The build outputs are the same as those of an uncached build.

It has two programs:

- **`ec`**, the compiler launcher. It sits in front of `cc`, `c++`, `gcc`, `clang`, `rustc`
  and `go`. One short-lived `ec` process runs for each compiler invocation.
- **`ecd`**, the per-machine daemon. It owns the cache store on disk and hands out compile
  *slots*, so that many sandboxes each running `make -jN` do not oversubscribe the machine.

`ec` talks to `ecd` over a Unix socket, by default `/nix/var/nix/ekacache/socket`, which is
mapped into the Nix sandbox.

## What gets cached

| Toolchain | Cached | Notes |
|---|---|---|
| C, C++, Objective-C, assembly | compiles (`-c`, `-S`, `-E`), configure probes, links, deterministic compile failures | see [C and C++](builds/c-cpp.md) |
| Rust | `lib`, `rlib` and `staticlib` crates | binaries, proc macros and failing crates are not cached; see [Rust](builds/rust.md) |
| Go | build actions, through `GOCACHEPROG` | needs Go 1.24 or newer; see [Go](builds/go.md) |
| Anything else | files and directory trees, with `ec cache` | see [`ec cache`](reference/ec-cache.md) |

## Design principles

- **Derivations never mention the cache.** Nothing in a derivation changes when ekacache is
  in use, so turning it on or off changes no store path.
- **No daemon means no cache, not a broken build.** If the socket is not mapped into the
  sandbox, or `ecd` is not running, `ec` runs the compiler normally. Outputs and exit codes
  are the same either way.
- **The compiler reports its own inputs.** `ec` learns what a compile read from the
  compiler's dependency file, and checks every input before it replays a result. See
  [Cache keys](concepts/keys.md).

## Who this book is for

- **Machine operators** who run `ecd` on a build host. Start with
  [Installing](setup/installing.md) and [Running ecd](setup/running-ecd.md).
- **Package-set and toolchain maintainers** who wire `ec` into compilers. See
  [Hooking ec into a build](builds/integration.md).
- **Anyone debugging a build** that uses the cache. See
  [Troubleshooting](operating/troubleshooting.md).

> **Status.** ekacache is the Rust rewrite of jig and jigd. It is tested on Linux (x86_64
> and aarch64). The macOS code compiles but has not yet run on a Mac, so treat macOS as
> experimental. If you are moving from jig, read [Coming from jig](jig.md).
