# 03: jigd server (phase P2)

Specified by: `jigd-daemon.md`, `jigd-protocol.md`, `jigd-slots.md`, `jigd-identities.md`,
`environment.md` (jigd rows); models `spec/jigd_protocol.qnt`, `spec/slots.qnt`.
Adopted here: HELLO (protocol v1), Q-SLOT-1 (dequeue abandoned waiters), Q-DAEMON-1 (strict
`JIGD_SIZE`/`JIGD_COLD_SIZE` with M/G/T suffixes, exit 2).

## Runtime shape

```
main (sync)
 ├─ parse argv, env, budgets           exit 2 / fatal exit 1 exactly as legacy
 ├─ Identities::new, Store::open        (sync, before the runtime starts: no async needed)
 ├─ socket: mkdir, remove, bind, listen(backlog = somaxconn), chmod 0666   (std, then into tokio)
 └─ tokio multi-thread runtime
      ├─ signal task: first SIGINT/SIGTERM → close listener (unlink socket)
      ├─ accept loop: any non-retryable accept error ends it (Q-DAEMON-5 kept)
      │    └─ per connection: peer check → spawn conn task
      └─ on loop end: spawn_blocking(store.close()) → exit 0 (open conns are dropped)
```

- **Why start the runtime late:** startup is sequential and fatal-on-error, and the store load
  is pure blocking I/O. Doing it before `Runtime::new` keeps the startup order (steps 1–12 of
  `jigd-daemon.md §Startup order`) literal, and makes "SIGTERM before step 11 kills without
  sealing" automatic.
- **As built (signed off 2026-11-18):** store calls and IDS hashing run with
  `tokio::task::block_in_place` on the worker thread, not `spawn_blocking`. The hand-off to the
  blocking pool made each GET and PUT about 1.5× slower than Go; with `block_in_place` the
  Rust jigd is faster than Go on every jigd benchmark (STATUS.md §Performance). The other
  workers keep serving connections while one blocks, so a slow call still holds up no other
  connection. Read `spawn_blocking` below as "a blocking store call".
- Worker threads: tokio's default (one per core). The blocking pool serves store calls and
  IDS hashing. Cap it at 512 (tokio's default), because a build of 1 000 parallel compiles
  sending PUTs must not spawn 1 000 threads; queued blocking calls wait instead.
- **SIGPIPE:** Rust ignores it at startup (std sets `SIG_IGN`), and tokio writes return
  `EPIPE`. This matches the requirement.
- Other signals keep their defaults (SIGHUP terminates without sealing, as in Go). SIGQUIT in
  Go dumps goroutines and exits 2. Rust's default action is core-and-terminate. Exit-by-signal
  vs exit 2 on SIGQUIT is an observable difference. **Decided (2026-11-18):** keep the Rust
  default, no handler; the core dump serves the same debugging purpose.

## Connection task

```rust
async fn serve(conn: UnixStream, ctx: Arc<Ctx>) {
    let mut rd = LineReader::new(conn_read, 65_536);   // exact Go ReadSlice semantics
    let mut wr = GoBufWriter::new(conn_write, 4_096);  // exact Go bufio.Writer semantics
    let mut held: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut version = 0;
    let mut first = true;
    loop {
        let Some(line) = rd.read_line().await else { break };   // EOF/err/too long → hang up
        match wire::parse_request(line) { ... }                 // None → hang up
        wr.flush().await?;                                       // after every successful handler
        first = false;
    }
    // cleanup: release each held token, in sorted build order (see §SLOT with cancellation)
}
```

### Exact-buffer emulation

Two Go behaviours leak through the buffers and are part of the contract:

1. **Input:** a 65 536-byte buffer, and `ReadSlice('\n')` returns `ErrBufferFull` on the
   65 536th byte without a newline. `LineReader` is a hand-written 64 KiB ring/linear buffer
   with the same rule. Bytes already buffered after a request line are the start of the next
   request or of a PUT body (PUT reads its body from the same buffer first).
2. **Output:** Go's `bufio.Writer.Write` **fills** the 4 096-byte buffer to the brim and then
   flushes it. When nothing is buffered it writes a large chunk directly. That is why
   `IDS 5000` followed by an early EOF delivered exactly 4 096 bytes (Q-PROTO-5). Tokio's
   `BufWriter` flushes *before* a write that doesn't fit, which would deliver a different byte
   count. So `GoBufWriter` reimplements Go's `Write`/`Flush` algorithm (about 40 lines), and a
   unit test pins the 4 096-byte outcome.

The GET body path bypasses `GoBufWriter`: Go flushes the header and then `io.Copy`s from the
file (sendfile). Rust flushes the header, then copies the value in chunks from a
`ValueReader` (pread in `spawn_blocking`, 256 KiB chunks) to the socket. A short copy hangs up
after a partial body (Q-PROTO-8 kept, `hits` already counted).

### Commands

| Verb | Implementation notes |
|---|---|
| `HELLO n` | Adopted. Valid only when `first`; else hang up (H11). `n` parsed by `wire::parse_hello_ver`; bad → hang up. Reply `OK min(n, 1)`. Touches no counters. |
| `GET k` | `gets += 1`; `spawn_blocking(store.get)`; MISS, or `hits += 1`, header, flush, body copy. |
| `HAS k` | `store.get` (recency side effect), reply `1`/`0`, no counters (Q-PROTO-1). |
| `PUT k n` | parse `n` (hang up without counting on failure); `puts += 1`; `store.validate_put` → log `put k: bad put` + hang up; read body (from the buffer first) into memory, or into an unlinked `put-*.tmp` spool in tier 0 when `n ≥ 4 MiB`; `spawn_blocking(store.put)`; `OK` or log + hang up. Log text follows Go formats (see §Error text). |
| `IDS n` | parse + range check; then for each path line: read, `spawn_blocking(identities.of)`, write answer into `GoBufWriter` (interleaved, as in Go). `IDS 0` writes nothing (Q-PROTO-6). |
| `SLOT b` | see §SLOT with cancellation. |
| `DONE b` | if `held[b] > 0`: decrement, `slots.release(b)`. Always `OK`. |
| `STATS` | the exact format string; counters are `AtomicI64`; not a snapshot (matches Go). |

Counters are `Relaxed` atomics in `Ctx`. Go uses `sync/atomic`, with no ordering promised in
STATS.

### SLOT with cancellation (adopted Q-SLOT-1)

`Slots` is a `std::sync::Mutex<SlotState>` (short critical sections, never held across
`.await`). Each waiter gets a `tokio::sync::oneshot::Sender<()>` plus a unique waiter id.
`grant()` is literally the Go loop: fewest held by the waiter's build, strict `<` for FIFO
among equals, counts re-read per iteration, `out++`, `held[b]++`, then send on the oneshot.

```rust
SLOT b =>
    match slots.acquire(b) {               // immediate grant iff out < limit && queue empty
        Granted => {}
        Queued(id, rx) => tokio::select! {
            biased;
            _ = rx => {}                                   // granted (token already counted)
            () = rd.wait_hangup() => {                     // peer gone
                if slots.cancel(id) { break 'conn }        // removed from queue: no token
                // else: the grant won the race (it held the lock first); token is ours,
                // fall through to the Go path (OK write will fail, cleanup releases it)
            }
        }
    }
    held[b] += 1; write "OK\n"
```

`rd.wait_hangup()` must not *consume* pipelined requests in the protocol sense, as the doc
requires. Implementation: it reads into the connection's own 64 KiB input buffer, which is
where those bytes would land anyway, and reports a hang-up only on EOF (read returns 0) or a
read error. Buffered bytes are parsed normally after the grant. When the buffer is full it
stops reading and waits on `rx` alone, which is the Go behaviour. This avoids both a
`MSG_PEEK` busy loop and tokio readiness edge cases, and it is equivalent because the reader
enforces the line limit at parse time, not at read time.

Hang-up cleanup releases the connection's tokens. The order of releases is not observable
except through which waiter `grant()` picks after each release, and Go's map order is random,
so any order conforms. Use sorted order for reproducible tests.

Tests: ported `slots_test.go` (`TestSlotsFewestHeldFirst`), `serve_test.go` hang-up cases, and
a new `abandoned_waiter_is_dequeued` test: limit 1, A holds, B queues then closes, STATS shows
`waiting=0` within 100 ms, and C's later SLOT is granted when A releases. Replay of
`spec/slots.qnt` traces with `CANCEL_ABANDONED = true` checks `noDeadWaiters` and the token
accounting invariants after each step.

## Identities (`identity.rs`)

`jigd-identities.md` line by line: lexical check with a port of Go `filepath.Clean` (a pure
function with a table test that includes `//`, `/./`, `/../` and a trailing `/`), then
`EvalSymlinks`. That is **not** `fs::canonicalize`: Go's `EvalSymlinks` resolves components
one by one and fails differently on dangling links, so port it and test the edge cases against
Go (vectors generated once). Then `realStore` prefix, `open`, `fstat` regular-file check,
`fdUnder` via `readlink(/proc/self/fd/N)`, the stamp `(ino, size, mtime_ns)`, and the memo
(`Mutex<HashMap<Vec<u8>, (Stamp, [u8; 34])>>`, never pruned; `ids=` reports its length).

Hashing streams the file through `jig_core::hasher` and checks that it read exactly `st_size`
bytes, so a short read gives `""`. The bytes hashed and the result are identical to Go, so
Q-ID-2 (whole file in memory) needs no sign-off to change. It is an internal resource fix.

## Peer authentication (`peer/linux.rs`)

`SO_PEERCRED` via `rustix::net::sockopt::socket_peercred`. Accept if uid == `getuid()`, or
uid == 0, or (nixbld ≥ 0 and gid == nixbld), or uid in `[0x34000000, 0x34000000 + 128<<16)`.

`nixbld` is computed once at startup. **Parity trap:** Go's `user.LookupGroup` uses
`getgrnam_r` (NSS) when built with cgo, but parses `/etc/group` itself when built without cgo.
Before P2 is done, check which mode the legacy Nix package uses (`CGO_ENABLED`), and match it:
`libc::getgrnam_r` for cgo, or an `/etc/group` parser for pure Go. Keep the
"gid string fails `Atoi` → 0" quirk (Q-DAEMON-7). It can only happen with the parser variant.

`peer/other.rs` (P9) is `true` / `true`, the same as Go.

## Environment and budgets (adopted Q-DAEMON-1)

`environment.md` and `jigd-daemon.md §Environment`, with the adopted strict budget parser:

- `JIGD_SIZE`, `JIGD_COLD_SIZE`: accept the syntax that the adopted section of
  `jigd-daemon.md` defines (a plain number = GiB, or M/G/T suffixes). Anything invalid prints
  the documented message to stderr and exits **2**. Budgets are logged at startup.
- `JIGD_SLOTS`: `Atoi`-compatible, used only if it parses and is > 0; otherwise the affinity
  count from `sched_getaffinity` (`rustix::process::sched_getaffinity`, count the set bits),
  **not** `available_parallelism` (Q-DAEMON-13).
- `XDG_CACHE_HOME` verbatim; else `$HOME/.cache`; empty `$HOME` → fatal `$HOME is not defined`.
  Go's `os.UserHomeDir` on Linux reads only `$HOME`, so the Rust code must not fall back to
  `getpwuid`.
- `NIX_STORE_DIR` verbatim, default `/nix/store` (jigd does not use the compile-time
  `STORE_DIR`).

## Logging and error text

- `log.rs`: `YYYY/MM/DD HH:MM:SS ` in **local time**, then the message and `\n` (only if
  missing), written to stderr with one `write` per line (Go's logger serialises lines). Local
  time comes from `libc::localtime_r`, so `libc` joins the dependency table for jigd. This is
  the one thing rustix doesn't provide. Avoid `chrono`/`time` (their local-offset handling is
  either heavy or refuses to run in multi-threaded processes).
- **Go error text.** Messages embed Go's error strings: `open <path>: no such file or
  directory`, `listen unix <path>: bind: address already in use`, `unexpected EOF`, `bad put`,
  `store closed`, and the doubled `put <k>: put <k>: wrote <n> of <size>: <nil>` (Q-DAEMON-12
  kept). Rust's `io::Error` displays as `No such file or directory (os error 2)`, so the
  crate has a `GoErr` type: an enum of the Go-shaped wrappers (`PathError{op, path, errno}`,
  `OpError`, sentinel strings). Its `Display` uses Go's **lowercase** errno table (Go's
  `syscall` error strings differ from glibc `strerror` in case and in a few wordings). The
  table is generated from Go's `zerrors_linux_*.go` and committed.
- The complete message list in `jigd-daemon.md §Logging` becomes a test: every format is
  produced at least once in the test suite and compared, after stripping the timestamp, with
  the Go daemon's output for the same scenario (difftest).

## Shutdown

First SIGINT/SIGTERM: stop accepting and unlink the socket path (Go does this on close unless
the path is abstract). Later signals are swallowed (Q-DAEMON-6 kept). The accept loop ends,
then `spawn_blocking(store.close())` runs: drain the demoter, then seal or drop the active pack.
A close error is fatal (exit 1). Exit 0 without waiting for connection tasks. Do not run the
runtime's graceful shutdown, which would wait on SLOT waiters; call `std::process::exit(0)`
after close.

Abstract socket paths (`@…`): reproduce Q-DAEMON-10. Bind as abstract, then `chmod` the
literal `@…` path, which fails and exits 1.

## Tests

- Ported `serve_test.go` (8 tests), `slots_test.go`, `identity_test.go`, run against an
  in-process server on a temp socket.
- Ported wire examples (`jigd-protocol.md §Wire examples`) as transcript tests.
- Difftest (12): the same scripted client transcripts against the Go jigd and the Rust jigd,
  comparing reply bytes, hang-up points and log lines (timestamp stripped). The scripts cover
  all of H1–H11, including endless line, bad numbers, `IDS 1048577`, `PUT k -1`, and Unicode
  separators.
- Quint replay of `spec/jigd_protocol.qnt` traces: each trace step becomes a client action on
  a real socket. Replies and hang-ups must match the model's state.
- Load test (nightly, not gating): 512 concurrent clients doing PUT/GET/IDS/SLOT for 60 s;
  no errors, and RSS bounded.
