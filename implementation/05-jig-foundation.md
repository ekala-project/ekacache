# 05: jig foundation (phase P3)

The jig binary skeleton that every mode sits on: dispatch, launcher, process execution,
logging, the jigd client, and the two-key engine (manifest build/validate).

Specified by: `jig-dispatch.md`, `jig-cache-client.md`, `jig-keys.md §Manifest`,
`environment.md`; model `spec/twokey.qnt`.
Adopted here: Q-CLIENT-1 (LE header, via `ekacache_core::frame`), Q-CLIENT-2 (PUT replies checked,
DONE after PUTs), HELLO negotiation, Q-KEY-3 (unreadable input → unstorable manifest).

**jig is synchronous.** It uses std sockets and blocking I/O, no tokio, no threads except where
legacy uses them (it uses none).

## `main.rs`: dispatch

```rust
fn main() -> ! {
    let argv: Vec<OsString> = std::env::args_os().collect();
    let env = Env::capture();                 // the variables jig reads (environment.md), once
    let store = Store::from_env(&env);        // ekacache_core::store_path::Store
    let mode = basename(argv[0]);
    std::process::exit(dispatch(mode, &argv[1..], &env, &store))
}
```

`dispatch` is the table in `jig-dispatch.md §Mode selection`, exactly: `jig` → usage handling
(stderr + 2 for no args; stdout + 0 for `--help`/`-h` as the first arg only), `/` in mode →
launcher, then `nix-store`, `cache`, `slot`, `gocacheprog`, `rustcwrap`, `reloc-fixup`, else
cc mode. Comparisons are byte-exact and case-sensitive. The usage text is a `const &[u8]`
copied from the doc, and a test compares it with the doc block.

`socket_path` = `$JIG_SOCK` if set (even empty), else `ekacache_core::store_path::default_socket()`.
It is computed once.

`self_path` for GOCACHEPROG: `absolute(argv[0])` if it contains `/` (made absolute, **not**
canonicalised: `std::path::absolute`), else `readlink /proc/self/exe`, else `""`.

### Signal disposition (SIGPIPE)

Legacy jig never touches SIGPIPE. It inherits the disposition from its parent, and so do its
children. Two places observe it: writes to a closed pipe on stdout (`jig cc -E … | head`), and
nix-store's plain `write(2)` to the daemon socket (Q-NIX-7, death by SIGPIPE). Rust's
runtime sets SIGPIPE to `SIG_IGN` before `main`, and `Command` resets it to `SIG_DFL` in
children. Both differ from legacy.

The fix:

- jig is `#![no_main]` with `#[unsafe(no_mangle)] extern "C" fn main(argc, argv) -> c_int`.
  std's `lang_start` does not run, so the inherited SIGPIPE disposition is kept, and the
  startup is a little smaller too. `std::env::args_os()` still works on glibc (std reads argv
  from `.init_array`), and the same holds on macOS. Panics still work.
- `process.rs` records the inherited disposition (`sigaction(SIGPIPE, NULL, &old)`) once.
  Every `Command` gets a `pre_exec` hook that restores it, because std's child setup resets
  SIGPIPE to default *before* `pre_exec` closures run. `pre_exec` forces the fork+exec path
  (no `posix_spawn`), which is what legacy does anyway.
- Launcher `exec` (rustix `execve`) needs nothing: the disposition is already the inherited
  one.
- Tests: spawn jig with SIGPIPE ignored and with it default. A child compiler that checks
  its own disposition must see the same as legacy's child does, and `jig cc -E big.c | head
  -c1` must end the same way (signal vs EPIPE).

jigd is a normal `main`: Go ignores SIGPIPE on sockets too, so std's default is right there.

### Exit status

All modes return an `i32` that `main` passes to `exit`. Values above 255 are truncated by the
kernel, just as C's `exit` truncates them. Cached failures replay `r/<k2>` parsed as unsigned
decimal (default 1), and `exit` truncates to 8 bits. That truncation is kept.

`panic = "abort"` in release. A panic is a bug, never a code path. Where legacy *aborts* on
purpose (for example `get-dir` on an iterator failure, because it is built with
`-fno-exceptions`), the Rust code calls `std::process::abort()` explicitly at the same point,
with a `// Q-DIR-…` comment. Those aborts are reproduced, not proposed fixes.

## `launcher.rs`

`ClassifyLaunch` (`StripVersion`, then the six cc suffixes with the `-` boundary rule, then
`rustc`, `go`). Unit-tested with the table in `jig-dispatch.md §ClassifyLaunch`, which includes
the Q-DISPATCH-8 `mpi-cc` row.

- cc → `cc::run(basename, args, socket, Some(program))` (conf-less).
- rustc → `rustc::run([program, args…])`.
- go → if `GOCACHEPROG` is unset or empty and `Path::new(socket).exists()`, add
  `GOCACHEPROG="<self> gocacheprog"` to the **child's** environment, then exec (Q-DISPATCH-7
  kept: an existence check only).
- other → exec.

`exec` = `rustix`'s `execve` with the current environ, plus overrides (no PATH search). On
failure print `jig: cannot execute <program>: <msg>\n` and exit 127. `<msg>` must be the
`std::generic_category` text, which is glibc `strerror` (`No such file or directory`), not
Rust's `io::Error` Display (which appends ` (os error 2)`). A `strerror` helper
(`libc::strerror_r`, so `libc` is a jig dependency too) is shared with every
`jig: cannot write <path>: <strerror>` message.

## `process.rs`: Run, Deterministic, environment

```rust
pub enum Capture { Inherit, Stderr, Stdout }
pub struct RunResult { pub status: i32, pub captured: Vec<u8> }
pub fn run(program: &OsStr, args: &[OsString], capture: Capture, env: &ChildEnv) -> RunResult;
pub fn deterministic(r: &RunResult) -> bool;   // status < 128 && no clang crash markers
```

Built on `std::process::Command`, with care where its semantics differ from `fork`/`execvp`:

| Legacy | Rust |
|---|---|
| `execvp` PATH search when no `/` | `Command::new(program)`. std searches the **parent's** `PATH` unless the child env overrides it, which matches `execvp` in the child (same env). Test: a program found only through `PATH`. |
| exec failure → child `_exit(127)`, no message (Q-DISPATCH-5) | `spawn()` returns `Err` → `RunResult { status: 127, captured: [] }`, no message. |
| fork failure → 127 + `jig: fork failed\n` captured (Q-DISPATCH-4) | a spawn error that is not an exec error (`EAGAIN`/`ENOMEM` from clone) → same. std reports both through `Err`. Distinguish them by `raw_os_error`: ENOENT/EACCES/ENOEXEC/ENOTDIR/ELOOP/ENAMETOOLONG/E2BIG are exec errors. Document this heuristic. |
| pipe failure → inherit | `Stdio::piped()` failure surfaces as a spawn error. Pre-create the pipe with `rustix::pipe` and fall back to inherit on error, as legacy does. |
| capture: read to EOF in 4 KiB reads, then `waitpid` (EINTR retried) | read the pipe to EOF, then `wait()` (std retries EINTR). Status 1 on other wait errors (Q-DISPATCH-6). |
| status: `WEXITSTATUS`, or 128 + signal | `ExitStatusExt::{code, signal}` → same mapping. |
| `argv[0]` = program as given | same by default. Callers that need a different `argv[0]` (driver policy) use `CommandExt::arg0`. |

**Environment changes for children.** Legacy calls `setenv`/`unsetenv` on jig's own
environment (`JIG_SLOT=1`, `GOCACHEPROG`, `JIG_ABSENT_LOG`). In Rust 2024 `set_var` is
`unsafe`, and it is unsound with threads. `ChildEnv` instead collects the overrides, and
`run`/`exec` apply them with `Command::env`/`env_remove`. This is observably identical, because
legacy changes its environment only so that children inherit the change. The one place where
legacy *reads back* its own change (the `JIG_SLOT` check of a nested `Slot` guard inside the
same process) is impossible, because no mode takes two slots. A debug assertion pins that.

## `log.rs`: JIG_LOG, JIG_LOG_ARGS

`LogOutcome`: `<tool>\t<outcome>\t<subject>\t<ms>\n` appended to `$JIG_LOG` (set and
non-empty, read once). `<ms>` is formatted `{:.0}` from an `f64` of elapsed ms; Rust's `{:.0}`
rounds half to even, but C++ `std::format("{:.0f}")` rounds the exact binary value correctly,
and so does Rust, so they agree. Test with 0.5, 1.5 and 2.5 to be sure. The open mode is
`O_APPEND|O_CREAT`, permissions as legacy (check `base.cc`), with one `write` per line. The
outcome-name table is an enum with a `const fn name()`. `JIG_LOG_ARGS` for cc only.

## `client.rs`: the jigd client (sync)

```rust
pub struct Client { sock: Option<UnixStream>, buf: Vec<u8> /* 64 KiB */, version: u32 }
impl Client {
    pub fn connect(path: &OsStr) -> Option<Self>;   // len >= 108 → None w/o syscall; HELLO dance
    pub fn get(&mut self, k: &[u8]) -> Option<Vec<u8>>;
    pub fn get_many(&mut self, ks: &[Vec<u8>]) -> Vec<Option<Vec<u8>>>;   // one send, pipelined
    pub fn has_many(&mut self, ks: &[Vec<u8>]) -> Vec<bool>;
    pub fn put(&mut self, k: &[u8], v: &[u8]) -> bool;                    // ADOPTED: reply checked
    pub fn put_many(&mut self, items: &[(Vec<u8>, Vec<u8>)]) -> usize;    // ADOPTED: count of OKs
    pub fn identities(&mut self, ps: &[Vec<u8>]) -> Vec<Vec<u8>>;          // one IDS, no chunking
    pub fn acquire_slot(&mut self, b: &[u8]) -> bool;
    pub fn release_slot(&mut self, b: &[u8]);
}
```

- **Connect:** `socket(AF_UNIX, SOCK_STREAM|SOCK_CLOEXEC)`, and `connect` with
  `addrlen = sizeof(sockaddr_un)` (legacy passes the full struct length, not the path length).
  This matters for abstract sockets and for paths with embedded NULs, so use `rustix` with an
  explicit `SocketAddrUnix`, and verify the byte-level address with an `strace` test.
- **HELLO** (adopted): send `HELLO 1\n` and read one line. `OK v` (v ≥ 1) → version 1. EOF →
  reconnect once and use v0 without HELLO, and remember v0 for this process. Anything else →
  treat as a failed connect. **Cost:** one extra round trip per jig invocation against a Rust
  jigd (and a reconnect against a Go jigd). Benchmark it in P3. If it is measurable on hit
  latency, raise it with the user: the docs allow skipping HELLO only via the process-local
  memory, so any other shortcut needs sign-off.
- **Framing:** `send` with `MSG_NOSIGNAL` (rustix `send` flags), with **no** retry on `EINTR`
  (Q-CLIENT-6 kept). Reads into the 64 KiB buffer. `RecvLine` has no length limit (client
  side).
- **RecvValue:** `OK ` prefix; `ParseUint` after trimming spaces/tabs; over 2³¹ → close the
  connection; short body → `None` and the connection is **not** closed (Q-CLIENT-5 kept: the
  desync is real behaviour); then `frame::decode`. The Q-CLIENT-7 trust of the raw size is
  bounded inside `decode` (see 02); the observable result is unchanged.
- **PUT replies (adopted Q-CLIENT-2):** `put`/`put_many` read each reply and count only
  exact `OK` lines as stored. Callers use the count (06/07: a failed manifest PUT means
  "stored" must not be logged; see those docs for exactly what changes).
- All failures return the empty result and never print.

### Slot guard

`SlotGuard::acquire(client, socket, env, child_env)`: skip if `JIG_SLOT` is non-empty;
connect if needed; build = `NIX_BUILD_TOP` if set (even empty), else `-`. On `OK`, set
`JIG_SLOT=1` in `child_env`. `Drop` sends `DONE`, reads one line, and removes the override.
**Adopted ordering (Q-CC-2 / Q-CLIENT-2):** in cc and rustc modes the guard is released
*after* the artifact PUTs, not before. The front ends express this by scope: the PUTs happen
inside the guard's lifetime (06, 07).

## `twokey.rs`: manifest engine

Implements `jig-keys.md §Manifest` and `spec/twokey.qnt`:

```rust
pub struct Inputs<'a> { k1: &'a [u8], inputs: &'a [Vec<u8>], primary: &'a [u8], absent: &'a [Vec<u8>] }
pub enum Built { Storable { text: Vec<u8>, k2: Digest }, Unstorable { reason: &'static str } }
pub fn build_manifest(i: &Inputs, ids: &mut IdSource) -> Built;          // ADOPTED Q-KEY-3
pub fn validate_manifest(k1: &[u8], stored: &[u8], ids: &mut IdSource, store: &Store)
    -> Result<Digest, Miss>;                                            // appeared:/inputs-changed:
pub fn find_result(client, k1, ids, store) -> Result<Digest, Miss>;     // new-key / validate
```

- `IdSource` wraps `InputId` with the `PrefetchIdentities` batch (content mode only, store
  paths outside `$out`, one `IDS` per call), falling back to local hashing for empty answers.
- **Adopted Q-KEY-3:** when `InputId(path)` is `None` for a depfile input during
  `build_manifest`, the result is `Unstorable`, so the invocation is not stored. Legacy skips
  the line silently. k2 for manifests that *are* stored is unchanged.
- `exists()` semantics: legacy uses `std::filesystem::exists` relative to the cwd, with errors
  counted as absent. `Path::try_exists` returns `Err` on EACCES; map `Err` → absent.
- Write order on a miss is the front ends' job (m/ first, then artifacts; `jig-keys.md`).

## Tests

- Unit: dispatch table, usage bytes, ClassifyLaunch table, process status mapping (a child
  killed by SIGKILL gives 137), PATH search, exec-failure 127, `deterministic` markers.
- `jig-keys.md §Manifest test vectors`, built under a temp dir with the same layout
  (`/tmp/jigprobe/t` is replaced by a `tempdir`; the vectors' k2 values depend on the path,
  so the test generates the expected k2 with the **legacy** binary in difftest mode, and the
  doc vectors run verbatim only when `/tmp/jigprobe` is free).
- Client against the Go jigd (difftest): GET/PUT/IDS/SLOT round trips, HELLO fallback
  (Go hangs up → reconnect → v0), and against the Rust jigd (v1).
- Quint: `spec/twokey.qnt` traces drive `build_manifest`/`validate_manifest` over a fake file
  system (`IdSource` and `exists` behind a trait), checking that a hit is only ever served for
  a manifest whose inputs still match (the model's soundness invariant).
