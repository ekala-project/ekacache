# jig dispatch, launcher and process execution

This document specifies how the single `jig` binary decides what to do: mode selection from
`argv[0]` or the first argument, launcher mode (`jig /path/to/<compiler> <args>`) and how it
recognises compiler drivers (including target-prefixed and versioned names behind nixpkgs-style
wrappers), the usage text, the small `cache` and `slot` subcommands, and every exit code
produced at this level. It also specifies `process.cc`'s `Run`, which spawns the real compiler
and maps its wait status to an exit code. The modes themselves are specified elsewhere:
[`jig-driver-policy.md`](jig-driver-policy.md) (cc mode's driver configuration),
[`jig-nix-store.md`](jig-nix-store.md), [`jig-reloc-fixup.md`](jig-reloc-fixup.md),
[`jig-cc-mode.md`](jig-cc-mode.md), [`jig-rustc-mode.md`](jig-rustc-mode.md), and the job-slot
guard in [`jig-cache-client.md`](jig-cache-client.md).

Sources: `jig/src/main.cc`, `jig/src/launcher.h`, `jig/src/launcher.cc`, `jig/src/process.h`,
`jig/src/process.cc`, `jig/src/dir_mode.cc` (`PutDir`, `GetDir`), `jig/src/cc_mode.cc`
(`RunCcMode` entry checks), `jig/src/rustc_mode.cc` (`RunRustcMode` entry check),
`jig/src/gocache_mode.cc` (`RunGoCacheProg`), `jig/src/fixup_mode.cc` (usage), `jig/src/base.cc`
(`LogOutcome`), `jig/src/keys.h` (`Outcome`).

Key words MUST, MUST NOT, SHOULD, SHOULD NOT and MAY are as in RFC 2119. Where this document
says "current behavior", that behavior is normative: the Rust implementation MUST reproduce it
unless a Quirk entry's recommendation is accepted by the maintainer.

---

## Before dispatch

`main` (`main.cc:93-133`) always does the following first, whatever the mode:

1. Copies `argv` into strings (`all`).
2. Computes `socket_path = Env("JIG_SOCK", StateDir() + "/jigd/socket")`. Evaluating the default
   constructs the `Store` singleton, which reads `JIG_STORE_IDENTITY`, `out` and (in content
   mode) `JIG_STORE_ROOTS`. See [`environment.md`](environment.md).
3. `mode` = the last path component of `argv[0]` (`std::filesystem::path::filename`), or `""`
   when `argc == 0`. `args` = `argv[1..]`.

## Mode selection

```
if mode == "jig":
    if args empty:                  print usage to stderr, exit 2
    if args[0] in {"--help", "-h"}: print usage to stdout, exit 0
    mode = args[0]; args = args[1..]
    if mode contains '/':           launcher mode (below)
match mode:
    "nix-store"   -> RunNixStoreMode(args)                  jig-nix-store.md
    "cache"       -> cache subcommands (below)
    "slot"        -> slot subcommand (below)
    "gocacheprog" -> RunGoCacheProg(socket_path)            args ignored
    "rustcwrap"   -> RunRustcMode(args, socket_path)        args = [rustc, rustc-args...]
    "reloc-fixup" -> RunFixupMode(args)                     jig-reloc-fixup.md
    anything else -> RunCcMode(mode, args, socket_path)     cc mode, jig-cc-mode.md
```

Comparisons are exact and case-sensitive. The same table applies whether the name came from
`argv[0]` (a symlink named `cache` works) or from `jig <mode>`. `jig --help` is recognised only
as the first argument. `jig cc --help` reaches cc mode, which passes it to the compiler.

The cc mode is the fallback for every other name (`cc`, `c++`, `gcc`, `clang++`,
`x86_64-…-gcc`, `gfortran`, even `jig` reached as `jig jig …`). Inside cc mode, the language
comes from that name: contains `++` → C++, else contains `fortran` or `flang` → Fortran, else
C (`cc_mode.cc:656-662`). Its driver configuration (`etc/jig.json` next to the binary, else
`$JIG_CC`) is specified in `jig-driver-policy.md`.

There is no mode named `rustc`, `driver` or `fixup`. A symlink named `rustc` goes to cc mode.
rustc caching is `rustcwrap` (cargo's `RUSTC_WRAPPER` convention), or launcher mode with a
program whose name is `rustc`.

### Usage text

Printed byte for byte (`main.cc:76-89`), to stderr with exit 2 for a bare `jig`, to stdout with
exit 0 for `jig --help` / `jig -h`:

```
usage: jig <mode> [args...]   or via a symlink named after the mode
       jig /path/to/<compiler> [args...]   launcher for gcc, clang, rustc or go

  cc c++ <args>         compiler driver + compile cache (any other name too)
  rustcwrap <args>      rustc through the cache
  gocacheprog           GOCACHEPROG server on stdin/stdout
  reloc-fixup <dir>     rewrite store references in an output tree
  nix-store <args>      realise paths over the builder RPC socket
  cache get|put <key> <file>, get-dir|put-dir <key> <dir>
  slot <program> <args> run holding a jigd job slot

  JIG_SOCK  jigd socket (default <store>/../var/nix/jigd/socket)
  JIG_LOG, JIG_LOG_ARGS  per-invocation log files
```

The text ends with one `\n` after the last line.

---

## Launcher mode

Selected by `jig <program> <args...>` when `<program>` contains a `/` (absolute or relative,
for example `./gcc`). A package set puts this in front of a compiler's own drivers, under the
language wrapper (nixpkgs `cc-wrapper`, `rustc-wrapper`). jig therefore sees the final
arguments the wrapper settled on, and nixpkgs' bintools wrapper still runs after the compiler
(see `jig-keys.md`, `KeyEnvironment`, for how link keys cover its `NIX_*` inputs).

`self`, the path of this jig binary for `GOCACHEPROG` (`main.cc:109-110`): if `argv[0]`
contains `/`, `std::filesystem::absolute(argv[0])` (made absolute, not resolved). Otherwise
`readlink("/proc/self/exe")`. On error it is `""`.

### ClassifyLaunch (`launcher.cc:28-40`, `launcher.cc:59-76`)

Input: `basename` = the last path component of `<program>`.

1. `StripVersion`: if `basename` has a `-` that is not its last byte, and every byte after the
   **last** `-` is an ASCII digit or `.`, drop that `-` and the rest. `gcc-15` → `gcc`,
   `clang++-21.1` → `clang++`, `gcc-` → `gcc-`, `clang-cpp` → `clang-cpp`.
2. With `name` = the result: for each `d` in `cc`, `c++`, `gcc`, `g++`, `clang`, `clang++`:
   `name == d`, or `name` is longer than `d`, ends with `d`, and the byte before that suffix is
   `-` → **cc**.
3. `name == "rustc"` → **rustc**. `name == "go"` → **go**. Otherwise → **other**.

| `basename` | Kind |
|---|---|
| `gcc`, `g++`, `cc`, `c++`, `clang`, `clang++` | cc |
| `x86_64-unknown-linux-gnu-gcc`, `aarch64-linux-gnu-g++`, `gcc-15`, `clang++-21.1` | cc |
| `x86_64-unknown-linux-gnu-gcc-14.2` | cc |
| `mpi-cc` | cc (any `-cc` suffix) |
| `mpicc`, `distcc`, `tcc`, `gcc-ar`, `clang-cpp`, `gfortran`, `gcc-` | other |
| `rustc`, `rustc-1.80` | rustc |
| `go` | go |
| `go1.22`, `gofmt` | other |

### Actions

| Kind | Action | Exit status |
|---|---|---|
| cc | `RunCcMode(basename, args, socket_path, launched = <program>)`: cc mode without a `jig.json` conf, `<program>` as the compiler, language from `basename` | cc mode's |
| rustc | `RunRustcMode([<program>, args...], socket_path)` | rustc mode's |
| go | if `$GOCACHEPROG` is unset or empty **and** `socket_path` exists (`std::filesystem::exists`, no connect attempt), `setenv GOCACHEPROG="<self> gocacheprog"`. Then `execv(<program>, [<program>, args...])` | go's own (jig is replaced) |
| other | `execv(<program>, [<program>, args...])` untouched | the program's own |

`execv` does no `PATH` search. If it fails, jig prints
`jig: cannot execute <program>: <strerror(errno)>\n` to stderr and exits **127**
(`launcher.cc:43-55`). The message is the `std::generic_category` text, for example
`No such file or directory`.

---

## `cache` subcommands

The full specification of these subcommands is in [`jig-cache-commands.md`](jig-cache-commands.md). The summary here covers dispatch and exit codes only, and that document is authoritative where the two differ.

`jig cache <verb> <key> <path>` (`main.cc:37-62`, `dir_mode.cc:101-194`). `<key>` is used
verbatim as the server key (no prefix). Values travel as in `jig-cache-client.md`.

| Condition | stdout/stderr | Exit |
|---|---|---|
| not exactly 3 arguments after `cache` | nothing | 1 |
| `put-dir`/`get-dir`: cannot connect | nothing | 2 |
| `put`/`get` (and any other verb): cannot connect | nothing | 2 |
| `put`: `<path>` unreadable | nothing | 1 |
| `put`: sent (reply unchecked, `jig-cache-client.md` Q-CLIENT-2) | nothing | 0 |
| `get`: miss, or writing `<path>` failed | nothing | 1 |
| `get`: written (atomic temp-file + rename) | nothing | 0 |
| `put-dir`: done | stderr `jig: put-dir <key>: <files> files, <new> new\n` | 0 |
| `get-dir`: listing missing | nothing | 1 |
| `get-dir`: some blobs missing | stderr `jig: get-dir <key>: <n> files, <missing> missing\n`; every file and symlink written by this run is removed again | 1 |
| `get-dir`: complete | stderr `jig: get-dir <key>: <n> files, 0 missing\n` | 0 |

Any verb other than `put`, `put-dir` and `get-dir` is treated as `get` (Q-DISPATCH-3).

`put-dir` listing format, one line per entry, in `recursive_directory_iterator` order:
`f <mode-octal> <hash>\t<relpath>\n` for regular files (`<mode>` = `st_mode & 0777` in octal
without a leading zero, `<hash>` = `HashOf(Stored(bytes))`, blob under `b/<hash>`), and
`l 0 <target>\t<relpath>\n` for symlinks. Unreadable files are skipped. Blobs are uploaded in
batches of 256, `HAS` first, then only the missing ones in one `PutMany`. The listing is put
last. `Stored`/`Restored` mask and resolve store hashes in text (NUL-free) files; see
`jig-keys.md` Q-KEY-1 for the path-mode effect. A missing or unreadable `<dir>` makes the
iterator fail, which aborts the process, because jig is built without exceptions.

## `slot` subcommand

`jig slot <program> [args...]` (`main.cc:66-74`): with no program, prints
`usage: jig slot <program> [args...]\n` to stderr and exits 2. Otherwise it takes a job slot
(the `Slot` guard, `jig-cache-client.md`), runs `Run(program, args, inherit)` (with a `PATH`
search), releases the slot and exits with the program's status as mapped by `Run`.

## Other entry checks

| Mode | Condition | Message (stderr) | Exit |
|---|---|---|---|
| cc | no conf: no readable `etc/jig.json` next to `/proc/self/exe`'s `../`, and `$JIG_CC` empty (see `jig-driver-policy.md`) | `jig: no etc/jig.json next to the binary and JIG_CC unset` | 1 |
| cc | Fortran name and the conf has no `fc` | `jig: <name> called but etc/jig.json names no fc` | 1 |
| cc | replaying a hit and writing the output or depfile failed | `jig: cannot write <path>: <strerror>` | 1 |
| rustcwrap | no arguments | `rustcwrap: usage: rustcwrap <rustc> args...` | 1 |
| gocacheprog | stdin EOF or a `close` command | (protocol on stdout) | 0 |
| reloc-fixup | see `jig-reloc-fixup.md` | `usage: reloc-fixup …` / `reloc-fixup: unknown argument …` | 2 |
| nix-store | see `jig-nix-store.md` | | |

Otherwise cc and rustc mode exit with the compiler's status (from `Run`), or with the replayed
status of a cached failure (`r/<k2>`, parsed as an unsigned decimal, default 1, then truncated
to 8 bits by `exit`).

---

## Process execution (`process.cc`)

### Run(program, args, stderr_mode) (`process.cc:51-106`)

1. If `stderr_mode` is `kCapture` (stderr) or `kCaptureStdout` (stdout), create a pipe. If
   `pipe()` fails, fall back to inheriting.
2. `argv` = `[program, args...]`, where `program` is the string as given (a bare name or a path).
3. `fork()`. On failure, return status **127** with captured text `jig: fork failed\n`. Callers
   that inherit stderr drop that text (Q-DISPATCH-4).
4. Child: if capturing, `dup2` the pipe's write end onto fd 2 (or 1) and close both pipe fds.
   `execvp(program, argv)` (with a `PATH` search when `program` has no `/`). On failure,
   `_exit(127)` with **no message** (Q-DISPATCH-5). The child inherits jig's whole environment,
   including `JIG_SLOT` and `JIG_ABSENT_LOG` when set, stdin, and the uncaptured stdout or stderr.
5. Parent: if capturing, read the pipe to EOF into `stderr_text` (4096-byte reads).
6. `waitpid(pid, &status, 0)`, retried on `EINTR`. Any other error returns with status **1**
   (the default) and what was captured.
7. Status: `WIFEXITED` → `WEXITSTATUS`. Otherwise (killed by a signal) **128 + signal number**.
   jig does not re-raise the signal, so its own death is an ordinary `exit(128+n)`.

The parent ignores no signals while waiting. A terminal `^C` reaches jig and the compiler alike.

### Deterministic(run) (`process.cc:108-111`)

True iff `status < 128`, and the captured text contains neither `unable to execute command`
nor `PLEASE submit a bug report` (clang's messages when a child crashed). cc mode caches a
failure only when this holds (`jig-cc-mode.md`).

### Exec in launcher mode

Launcher mode for `go` and other programs uses `execv` (no `PATH` search) in place of
`fork`+`exec`. Errors are reported as above (127 with a message).

---

## Logging

`LogOutcome(tool, outcome, subject, clock)` (`base.cc:321-328`) appends one line
`<tool>\t<outcome>\t<subject>\t<ms>\n` to `$JIG_LOG` if set and non-empty (read once per
process). `<ms>` is the elapsed wall time since mode start, formatted `{:.0f}`. Outcome names
(`base.cc:293-319`):

| Outcome | Name |
|---|---|
| hit | `cached` |
| hit, replayed failure | `cached-error` |
| miss, stored | `compiled` |
| miss, failure stored | `compiled-error` |
| miss, failure not stored | `failed` |
| miss, not stored | `compiled-unstored` |
| uncacheable compile | `uncacheable` |
| link | `linked` |
| source unreadable | `uncacheable-nosrc` |
| query (`-v`, `-print-*` …) | `query` |
| no daemon | `no-daemon` |

`<tool>` is `cc`, `rustc` or `go`. `$JIG_LOG_ARGS` (cc only) gets `<outcome>\t<user args joined
by " ">\n` for every uncached invocation except queries.

---

## Quirks

### Q-DISPATCH-1: Mode names work through `argv[0]` and `argv[1]` alike

**Current behavior**: `cache`, `slot`, `nix-store`, `gocacheprog`, `rustcwrap` and `reloc-fixup`
are recognised as the basename of `argv[0]` and as `jig <mode>`. The header comment in `main.cc`
suggests the first group works only via `argv[1]` and the second only via a symlink. Only the
exact basename `jig` triggers the `argv[1]` lookup, so a binary installed as `jig-0.1` or
`jig.real` is cc mode.

**Impact**: a stray symlink named `cache` or `slot` changes meaning. Renaming the binary turns it
into a compiler driver.

**Recommendation**: keep as-is.

### Q-DISPATCH-2: The usage text is not mode-complete

**Current behavior**: the usage text lists `cc c++` as if they were special. Any unrecognised
name is cc mode, and unknown `jig <word>` is passed to cc mode as the compiler name (it then
usually fails for want of a conf). The comment in `main.cc` describes the `JIG_LOG` line format
as `<hit|hit-fail|miss-*|plain-*> <source> <ms>`, which is stale (the real format is above).

**Impact**: typos such as `jig cahce get …` try to compile.

**Recommendation**: keep as-is. The stale comment needs no Rust counterpart.

### Q-DISPATCH-3: `jig cache` is lax

**Current behavior**: a wrong argument count exits 1 with no message. Any verb other than
`put`/`put-dir`/`get-dir` behaves as `get`. `put` exits 0 even if the daemon rejected the value.

**Impact**: `jig cache gte k f` silently fetches. Scripts cannot tell a failed put.

**Recommendation**: keep as-is (change needs sign-off).

### Q-DISPATCH-4: `fork` failure text is dropped for inherited stderr

**Current behavior**: `Run` returns 127 with `jig: fork failed\n` in `stderr_text`, but callers
using `kInherit` (uncached compiles, `jig slot`) only look at `status`.

**Impact**: the build sees exit 127 with no explanation.

**Recommendation**: change, needs sign-off (print the message whenever stderr is inherited).

### Q-DISPATCH-5: exec failure is silent in `Run`, loud in launcher `Exec`

**Current behavior**: when `execvp` fails in `Run`, the child exits 127 with no output (cc mode
with a misconfigured `JIG_CC`, `jig slot nosuch`). Launcher `Exec` prints
`jig: cannot execute …`.

**Impact**: confusing "exit 127" with no message.

**Recommendation**: change, needs sign-off (report `execvp` errors on stderr from the child).

### Q-DISPATCH-6: `waitpid` failure reports status 1

**Current behavior**: a non-`EINTR` `waitpid` error returns the default status 1.

**Impact**: practically unreachable.

**Recommendation**: keep as-is.

### Q-DISPATCH-7: `GOCACHEPROG` decision checks existence only

**Current behavior**: launcher `go` sets `GOCACHEPROG` when the socket path exists, without
connecting. If `self` contains spaces the value is mis-split by go. If `self` could not be
determined, the value is ` gocacheprog`.

**Impact**: a stale socket file makes go start a `gocacheprog` that runs without the shared cache
(harmless). The other two cases break go builds.

**Recommendation**: keep as-is.

### Q-DISPATCH-8: `-cc` suffix classifies unrelated tools as compilers

**Current behavior**: any `*-cc`, `*-gcc`, `*-clang` basename is a cc driver (for example
`mpi-cc`, `ccache-cc`). Names like `mpicc` are not.

**Impact**: a package set that launches an MPI wrapper by path gets cc-mode caching for it.

**Recommendation**: keep as-is.

---

## Open questions

1. Should `jig` reached under a versioned name (`jig-0.1`) also accept `jig <mode>`? Today it
   cannot (Q-DISPATCH-1).
2. The exit status of a cached failure replays the stored integer. Only statuses < 128 are
   stored, but a corrupt `r/` value such as `300` exits with `300 & 0xff`. Should the Rust port
   validate it?
3. Should launcher mode pass `--help` through to the compiler? It does today, since only
   `jig --help` itself is intercepted.
