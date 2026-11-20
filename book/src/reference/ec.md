# ec

```text
usage: ec <mode> [args...]   or via a symlink named after the mode
       ec /path/to/<compiler> [args...]   launcher for gcc, clang, rustc or go

  cc c++ <args>         compiler driver + compile cache (any other name too)
  rustcwrap <args>      rustc through the cache
  gocacheprog           GOCACHEPROG server on stdin/stdout
  reloc-fixup <dir>     rewrite store references in an output tree
  nix-store <args>      realise paths over the builder RPC socket
  cache get|put <key> <file>, get-dir|put-dir <key> <dir>
  slot <program> <args> run holding an ecd job slot

  EKACACHE_SOCK  ecd socket (default <store>/../var/nix/ekacache/socket)
  EKACACHE_LOG, EKACACHE_LOG_ARGS  per-invocation log files
```

`ec` with no arguments prints this to stderr and exits 2. `ec --help` or `ec -h`, as the
first argument, prints it to stdout and exits 0.

## Modes

| Mode | Reached by | Page |
|---|---|---|
| C/C++ compiler | `ec cc …`, a symlink named `cc`, `gcc`, `clang++`, …, or launcher mode | [C and C++](../builds/c-cpp.md) |
| launcher | `ec /path/to/program …` | [Hooking ec into a build](../builds/integration.md#option-1-launcher-mode) |
| `rustcwrap` | `RUSTC_WRAPPER` | [Rust](../builds/rust.md) |
| `gocacheprog` | `GOCACHEPROG` | [Go](../builds/go.md) |
| `cache` | `ec cache …` | [ec cache](ec-cache.md) |
| `slot` | `ec slot …` | [ec slot](ec-slot.md) |
| `nix-store` | `ec nix-store …` | [ec nix-store](ec-nix-store.md) |
| `reloc-fixup` | `ec reloc-fixup …` | [ec reloc-fixup](ec-reloc-fixup.md) |

Every mode can be reached either as `ec <mode>` or through a symlink of that name to `ec`.
The mode comes from the invoked name only when it is exactly `ec`. Under any other name
that is not a mode, `ec` is a C/C++ compiler.

## Exit status

**Compiler and program exit codes pass through.** When `ec` runs a compiler or a program:

| Child | `ec` exits with |
|---|---|
| exits with status N | N |
| is killed by signal N | 128 + N |
| cannot be started | 127, with no message (C/C++ mode and `ec slot`) |
| cannot be started in launcher mode for `go` or other programs | 127, after `ec: cannot execute <program>: <error>` |

A cached failure replays its stored status.

**`ec`'s own errors** depend on the mode. Generally, 2 means a usage error and 1 means an
operation failed. Each mode's page has the details.

**The cache never changes a compile's exit status.** A missing daemon, a refused connection
or a failed store all lead to an ordinary compile. Only the `ec cache` commands report a
missing daemon (exit 2).
