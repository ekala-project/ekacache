# jigd process: CLI, environment, socket, authentication, signals

jigd is the host-side daemon behind jig's build cache and compiler slots. It serves a Unix
socket that nix.conf's `extra-sandbox-paths` maps into every build as
`/nix/var/nix/jigd/socket`. This document covers the process itself: command line,
environment variables and how they are parsed, the order of startup, socket creation and
permissions, peer authentication, signals and shutdown, exit codes, and log output. The wire
protocol, store, identities and slots each have their own document:
[jigd-protocol.md](jigd-protocol.md), [jigd-store.md](jigd-store.md),
[jigd-identities.md](jigd-identities.md), [jigd-slots.md](jigd-slots.md).

Sources: `jigd/main.go` (`usage`, `main`, the header comment), `jigd/peer_linux.go` (`allowed`,
`nixbld`, `fdUnder`), `jigd/peer_other.go`, `jigd/store.go` (`OpenStore`, `Close`, log lines),
`jigd/identity.go:NewIdentities`, `jigd/slots.go:NewSlots`, and the Go runtime and standard
library behavior noted inline (checked against Go 1.25.9). No Quint model covers this file. It
is the environment for all three models.

The key words MUST, MUST NOT, SHOULD, SHOULD NOT and MAY are used as in RFC 2119. Where this
document says "current behavior", that behavior is normative. The Rust implementation MUST
reproduce it unless the maintainer accepts the recommendation in the matching Quirk entry.

---

## Command line

```
jigd <socket>
jigd --help | -h
```

| argv (after the program name) | Behavior | Output | Exit |
|---|---|---|---|
| exactly `--help` or exactly `-h` | print the usage text | stdout | 0 |
| exactly one other argument | that argument is the socket path, even if it starts with `-` (Q-DAEMON-2) | | runs |
| zero arguments, or two or more | print the usage text | stderr | 2 |

The usage text, byte for byte (`main.go:162-173`), ends with a single `\n` after the
`JIGD_SLOTS` line:

```
usage: jigd <socket>

The compile cache and job-slot daemon jig talks to. Nix builds reach it through
sandbox-paths /nix/var/nix/jigd/socket=<socket>.

  NIX_STORE_DIR   the only tree IDS answers for (default /nix/store)
  XDG_CACHE_HOME  packs live in $XDG_CACHE_HOME/jigd/packs (default ~/.cache)
  JIGD_SIZE       cache budget in GiB (default 50)
  JIGD_COLD       a second, slower pack directory that takes what JIGD_SIZE pushes out
  JIGD_COLD_SIZE  its budget in GiB (default 0: unbounded)
  JIGD_SLOTS      concurrent compile slots (default: number of CPUs)
```

`jigd -h foo` has two arguments, so it prints the usage to stderr and exits 2.

## Environment

All variables are read once, at startup. "Empty" and "unset" are treated the same.

| Variable | Parsing | Default / fallback | Used for |
|---|---|---|---|
| `XDG_CACHE_HOME` | taken verbatim (a relative path is relative to the cwd: Q-DAEMON-9) | `os.UserHomeDir()` + `/.cache`. On Linux that is `$HOME`. If `$HOME` is empty: fatal, `$HOME is not defined` | tier 0 = `<cache>/jigd/packs` |
| `JIGD_SIZE` | `gib()` below | 50 GiB = 53 687 091 200 | tier-0 budget |
| `JIGD_COLD` | verbatim. A non-empty value adds tier 1 | no tier 1 | tier-1 directory |
| `JIGD_COLD_SIZE` | `gib()`. Read only if `JIGD_COLD` is non-empty | 0 (unbounded) | tier-1 budget |
| `JIGD_SLOTS` | `strconv.Atoi`, used only if it parses and is greater than 0. Anything else falls back **silently** | `runtime.NumCPU()` | slot limit |
| `NIX_STORE_DIR` | verbatim | `/nix/store` | IDS root ([jigd-identities.md](jigd-identities.md#configuration)) |

### Budget parsing: `gib(env, def)`

This is the Go behaviour. The Rust port parses strictly and accepts unit suffixes (adopted Q-DAEMON-1).

```
if n, ok = strconv.ParseInt(getenv(env), 10, 64); ok: return n << 30   # int64 shift, wraps
return def << 30
```

- Accepted syntax: an optional `+` or `-`, then ASCII digits; leading zeros allowed. No
  white space, no unit suffix and no fraction: `50G`, ` 50` and `1.5` are all "not a number"
  and give the **default**, 50 GiB.
- The units are GiB only. Fractions cannot be expressed.
- `n << 30` is a 64-bit two's-complement shift: bits that move past bit 63 are lost and the
  result may change sign. A Rust implementation MUST compute `((n as u64) << 30) as i64`, or
  equivalently `n.wrapping_shl(30)`, and MUST NOT saturate, check or panic. Values checked
  with Go 1.25:

| `JIGD_SIZE` | Budget (bytes) | Effect |
|---|---|---|
| unset, empty, `abc`, `50G`, `1.5`, ` 50`, `9223372036854775808` (out of range) | 53 687 091 200 | default 50 GiB |
| `50`, `+50`, `050` | 53 687 091 200 | 50 GiB |
| `0` | 0 | **unbounded** (Budget `<= 0`) |
| `-1` | −1 073 741 824 | **unbounded** |
| `8589934591` (2³³−1) | 9 223 372 035 781 033 984 | effectively unbounded |
| `8589934592` (2³³) | −9 223 372 036 854 775 808 | unbounded (wrapped negative) |
| `17179869184` (2³⁴) | 0 | unbounded (wrapped to zero) |
| `17179869185` (2³⁴+1) | 1 073 741 824 | **1 GiB** (wrapped) |

`JIGD_COLD_SIZE` follows the same table, except that its default is 0 (unbounded). See
Q-DAEMON-1.

### Default slot limit

`runtime.NumCPU()` is the number of CPUs in the process's affinity mask (`sched_getaffinity`),
read when the process starts. It ignores cgroup CPU quotas. Rust's
`std::thread::available_parallelism()` also honours cgroup quotas and can return a smaller
number, so the Rust daemon MUST count the affinity mask (Q-DAEMON-13).

## Startup order

`main.go:175-238`. A fatal error (`log.Fatal`) prints a timestamped log line to stderr and
exits 1.

1. Check the arguments (§Command line).
2. Find the cache directory (`XDG_CACHE_HOME` or `$HOME/.cache`). Fatal if there is no home
   directory.
3. Build the tiers from `JIGD_SIZE`, `JIGD_COLD` and `JIGD_COLD_SIZE`.
4. Set the slot limit from `JIGD_SLOTS` or NumCPU, and create the slots.
5. `NewIdentities(NIX_STORE_DIR)`, which resolves the store's symlinks once.
6. `OpenStore(tiers)`: create the tier directories (`0755`), load the packs, rotate to a new
   active pack, start the demoter. Fatal on error ([jigd-store.md](jigd-store.md#startup-openstore-and-load)).
7. `MkdirAll(dirname(socket), 0755)`. Fatal on error.
8. `os.Remove(socket)`, error ignored. This removes **whatever** is at that path: a stale
   socket, a regular file or an empty directory (Q-DAEMON-3).
9. `net.ListenUnix("unix", socket)`: socket, bind, listen. The listen backlog is the value in
   `/proc/sys/net/core/somaxconn` (Go's `maxListenerBacklog`), and the socket is
   `SOCK_CLOEXEC`. Fatal on error, for example `listen unix <path>: bind: address already in
   use` or a path too long for `sun_path`.
10. `chmod(socket, 0666)`. Fatal on error. Between steps 9 and 10, the socket has mode
   `0777 &~ umask` (Q-DAEMON-4).
11. Install handlers for SIGINT and SIGTERM (§Signals).
12. Log `listening on <socket>, <limit> slots, keys=<k> packs=<p> bytes=<b> live=<l>`.
13. Accept loop: one goroutine per connection, running `serve` ([jigd-protocol.md](jigd-protocol.md)).

The socket mode is `0666` so that build users can reach it. Access control is done per
connection by `allowed()` (§Peer authentication).

A socket path that starts with `@` is an **abstract** socket on Linux, because of Go's address
conversion. Step 10 then runs `chmod` on a file literally named `@…` in the cwd, which usually
does not exist, so the daemon exits 1 (Q-DAEMON-10).

## Peer authentication

On Linux, `allowed(conn)` (`peer_linux.go:15-30`) runs once for each accepted connection,
before anything is read:

```
cred = getsockopt(fd, SOL_SOCKET, SO_PEERCRED)     # failure -> reject
accept iff any of:
    cred.uid == getuid()                           # the daemon's real uid
    cred.uid == 0                                  # root
    nixbld >= 0 and cred.gid == nixbld             # peer's PRIMARY gid is the nixbld group
    872415232 <= cred.uid < 872415232 + 8388608    # nix auto-allocate-uids default range
```

- `SO_PEERCRED` gives the credentials of the process that called `connect()`, as they were at
  connect time (pid, uid and gid in the daemon's user namespace). Only uid and primary gid are
  checked. Supplementary groups and the pid are not (Q-DAEMON-7).
- `nixbld` is computed once, at process start (Go package init), by `user.LookupGroup("nixbld")`.
  If the group does not exist it is −1, and the gid rule never matches. If the gid string
  does not parse, `strconv.Atoi` yields **0**, and every peer with primary gid 0 is
  accepted (Q-DAEMON-7).
- 872 415 232 is `0x34000000` and 8 388 608 is `128 << 16`, the nix.conf defaults for `start-id`
  and `id-count`. They are hard-coded. The host's actual nix.conf is not read.
- A rejected connection is closed at once: no bytes, no log line.
- **Non-Linux** builds (`peer_other.go`): `allowed` always returns true, and `fdUnder` always
  returns true. The only access control is the permission on the socket's directory
  (Q-DAEMON-8).

## Signals and shutdown

- SIGINT and SIGTERM go to a channel with a 1-element buffer. A goroutine waits for the
  **first** one and closes the listener (`main.go:232-237`). Later SIGINT/SIGTERM signals are
  still caught (Go keeps the handler installed), but nobody reads the channel, so they
  have no effect (Q-DAEMON-6).
- Closing the listener unlinks the socket file. Go's `UnixListener.Close` does this for a
  listener it created, unless the path is abstract (`@…`). Verified: the socket file is gone
  after SIGTERM. The Rust daemon MUST unlink it too.
- `AcceptUnix` then fails, and the accept loop ends on **any** accept error, not only on a
  closed listener (Q-DAEMON-5). Go retries `ECONNABORTED` and `EINTR` internally. Others, such
  as `EMFILE` and `ENFILE`, end the loop.
- `store.Close()`: refuse further Puts, wait for the demoter to finish all pending work, and
  seal the active pack ([jigd-store.md §Close](jigd-store.md#close)). An error is fatal
  (exit 1).
- `main` returns, and the process exits 0. Connections still open are cut off without a
  reply, including GETs in the middle of a body and goroutines blocked in `SLOT`. Their deferred
  releases do not run, which does not matter at exit.
- Other signals keep Go's defaults. SIGHUP and SIGQUIT terminate the process without sealing
  (SIGQUIT also dumps the goroutine stacks and exits 2). SIGPIPE is ignored for socket writes,
  which return `EPIPE`, and the Rust daemon MUST NOT die from SIGPIPE either. A SIGTERM that
  arrives before step 11 kills the process with the default action, without sealing. An
  unsealed active pack is recovered by scanning at the next start.

## Exit codes

| Code | When |
|---|---|
| 0 | `--help` / `-h`, or a normal shutdown after the accept loop ends (signal or accept error) |
| 1 | any `log.Fatal`: no home directory, `OpenStore` failure (including the `O_EXCL` collisions of Q-STORE-3 and Q-STORE-4), mkdir, listen or chmod of the socket, `store.Close` failure |
| 2 | wrong argument count (usage on stderr). Rust port also: invalid `JIGD_SIZE`/`JIGD_COLD_SIZE` (adopted Q-DAEMON-1). Go's runtime also exits 2 on SIGQUIT and on an unrecovered panic |
| signal | SIGHUP or SIGKILL terminate the process by signal |

## Logging

- All log output goes to **stderr** through Go's standard logger with default flags: a prefix
  `YYYY/MM/DD HH:MM:SS ` in **local time** (second resolution), then the message, then `\n`
  (added if missing). The daemon writes nothing to stdout except `--help`.
- Complete list of messages:

| Format | When | Source |
|---|---|---|
| `listening on %s, %d slots, %s` | startup done. `%s` = store stats | `main.go:238` |
| `put %s: %v` | a PUT failed after its length parsed | `main.go:65` |
| `pack %06d: %v (skipped)` | a pack failed to load | `store.go:107` |
| `pack %06d: truncating torn tail at %d (was %d)` | an unsealed pack had a partial record | `store.go:188` |
| `pack %06d: %v` | demotion or eviction failed | `store.go:350` |
| `evicted pack %06d (%d MiB, %d MiB live)` | a pack was deleted from the last tier (sizes `>>20`, rounded down) | `store.go:403` |
| `demoted pack %06d to %s (%d MiB, %d MiB live)` | a pack moved to the next tier (`%s` is the destination directory) | `store.go:425` |
| `<err>` | `log.Fatal(err)`, followed by exit 1 | `main.go:189,218,222,227,230,247` |

- `%v` of an error prints Go's error text, for example `bad put`, `store closed`,
  `unexpected EOF`, `open <path>: <strerror>` and `listen unix <path>: bind: <strerror>`. A short
  body copy in `Store.Put` produces a doubled prefix:
  `put <k>: put <k>: wrote <n> of <size>: <err>`. There `<err>` may be `<nil>`, which is how Go
  prints a nil error (Q-DAEMON-12).
- Nothing logs accepted connections, rejected peers, protocol hang-ups, SLOT activity or
  Sscanf-skipped pack files.

---

## Quirks

### Q-DAEMON-1: `JIGD_SIZE` / `JIGD_COLD_SIZE` parsing and overflow

**Current behavior**: Anything that does not parse as an int64, including `50G`, `1.5` and
` 50`, silently gives the default (50 GiB for `JIGD_SIZE`, unbounded for `JIGD_COLD_SIZE`).
`0` and negative values mean **unbounded**. The usage text does not say so for `JIGD_SIZE`.
`n << 30` wraps: 2³³ and above become negative or zero (unbounded) or a small positive budget
(2³⁴+1 gives 1 GiB). `+N` and leading zeros are accepted. Nothing is logged in any of these
cases.

**Impact**: A typo can silently disable eviction, or leave the default in place when the
operator meant a small cache.

**Recommendation**: **ADOPTED (signed off 2026-11-17).** The Rust jigd MUST parse `JIGD_SIZE`, and
`JIGD_COLD_SIZE` when it is read (only if `JIGD_COLD` is non-empty), strictly:
- unset or empty → the default (50 GiB; `JIGD_COLD_SIZE` 0 = unbounded), as before;
- otherwise it MUST match `^[0-9]+[MmGgTt]?$`. Without a suffix the number is GiB (as before);
  `M`/`G`/`T` are MiB/GiB/TiB. `0` means unbounded (as before);
- anything else (a sign, white space, a fraction, another suffix), or a byte count above
  `i64::MAX`, is fatal: write `jigd: invalid <VAR> "<value>": want <n>[M|G|T], 0 = unbounded`
  to stderr and exit **2** before opening the store;
- after opening the store, log one line per tier: `tier <i> <dir> budget <bytes>` or
  `tier <i> <dir> budget unbounded`.
Values that worked before and meant what they said (`50`, `0`, `050`) keep their meaning.
`+50` and negative values are now rejected, and `50G` now means 50 GiB on purpose.

### Q-DAEMON-2: Any single argument is the socket path

**Current behavior**: Only `--help` and `-h` are recognised. `jigd --version` or `jigd -v`
creates a socket named `--version` or `-v` in the cwd and starts serving.

**Impact**: Surprising, and it removes any file with that name (Q-DAEMON-3).

**Recommendation**: keep as-is.

### Q-DAEMON-3: Whatever is at the socket path is removed

**Current behavior**: `os.Remove(sock)` runs unconditionally, with its error ignored. It
deletes a regular file, a symlink (not its target) or an empty directory at that path. It does
not check whether another jigd is still listening, so a second daemon takes over the path and
the first one keeps running, unreachable.

**Impact**: Running two daemons by mistake leaves one orphaned. A mistyped path can delete a
file.

**Recommendation**: keep as-is.

### Q-DAEMON-4: The socket briefly has umask permissions

**Current behavior**: `bind` creates the socket with `0777 &~ umask`, and the `chmod 0666`
comes right after. A client that connects in that window may be refused by file permissions.
A chmod failure is fatal, although the socket is already bound.

**Impact**: Negligible.

**Recommendation**: keep as-is.

### Q-DAEMON-5: Any accept error shuts the daemon down cleanly

**Current behavior**: The accept loop `break`s on any error. If the process runs out of file
descriptors (`EMFILE`), which is reachable through unbounded idle connections (Q-PROTO-9), the
daemon seals its pack, removes the socket and **exits 0**.

**Impact**: A local user who passes `allowed()` can stop the daemon, and a supervisor that
restarts only on failure will not restart it.

**Recommendation**: change, needs sign-off: retry temporary accept errors with backoff. Until
then the Rust daemon MUST also exit 0.

### Q-DAEMON-6: Only the first signal counts, and shutdown can be slow

**Current behavior**: After the first SIGINT/SIGTERM, further ones are caught and ignored.
Shutdown waits for every pending demotion (Q-STORE-13). Open connections are dropped without
replies.

**Impact**: Ctrl-C twice does not force an exit. Use SIGKILL, which loses only the sealing.

**Recommendation**: keep as-is.

### Q-DAEMON-7: Peer check details

**Current behavior**: Only the uid and **primary** gid from `SO_PEERCRED` are checked. A peer
whose supplementary groups include nixbld but whose primary group is different is refused. The
auto-allocated uid range is the hard-coded nix.conf default. A nixbld group whose gid fails to
parse is treated as gid 0. The nixbld lookup happens once at start, so a group created later is
not seen.

**Impact**: Hosts with non-default `start-id` or `id-count`, or build users with a different
primary group, are refused (jig then compiles without the cache). The gid-0 fallback is
theoretical.

**Recommendation**: keep as-is.

### Q-DAEMON-8: No authentication on non-Linux

**Current behavior**: `allowed` returns true and `fdUnder` returns true. The socket is still
`0666`.

**Impact**: Any local user can use the cache and slots, and IDS loses its descriptor-location
check (it still checks paths lexically and through `EvalSymlinks`).

**Recommendation**: keep as-is (document it).

### Q-DAEMON-9: A relative `XDG_CACHE_HOME` is used

**Current behavior**: The value is used verbatim. The XDG specification says relative values
must be ignored.

**Impact**: Packs end up relative to the daemon's cwd.

**Recommendation**: keep as-is.

### Q-DAEMON-10: An `@` prefix makes an abstract socket, and then the daemon exits 1

**Current behavior**: Go maps a leading `@` to the abstract namespace. The following
`chmod("@…")` fails unless a file with that name exists in the cwd, in which case that file is
chmodded to 0666.

**Impact**: Abstract sockets cannot be used.

**Recommendation**: keep as-is.

### Q-DAEMON-11: Signal handlers are installed after listening

**Current behavior**: A SIGTERM during `OpenStore` or before step 11 kills the process with
the default action: no seal, and the socket file stays (it is removed at the next start).

**Impact**: Negligible. The next start recovers by scanning.

**Recommendation**: keep as-is.

### Q-DAEMON-12: Doubled prefix in a PUT log line

**Current behavior**: `Store.Put` returns `put <k>: wrote <n> of <size>: <err>`, and
`main.put` adds `put <k>: ` again. A copy that returns no error but a short count prints
`<nil>`.

**Impact**: Cosmetic.

**Recommendation**: keep as-is (log lines are part of the compatibility surface).

### Q-DAEMON-13: The default slot limit is the affinity-mask CPU count

**Current behavior**: `runtime.NumCPU()` counts the CPUs in the affinity mask at start and
ignores cgroup quotas and later changes to the mask. A malformed or non-positive `JIGD_SLOTS`
is ignored silently.

**Impact**: In a CPU-limited container, the daemon admits more compilers than the quota
allows.

**Recommendation**: keep as-is (count the affinity mask, not `available_parallelism`).

---

## Open questions

1. Is jigd built with cgo? `user.LookupGroup` uses NSS (`getgrnam_r`) with cgo, and only
   parses `/etc/group` without it. On hosts where nixbld comes from NSS but not
   `/etc/group`, the results differ. Nothing in the repository sets `CGO_ENABLED`.
2. Are the Go error texts inside log lines normative (for example `open x: no such file or
   directory`, `$HOME is not defined`)? This document treats the format strings as normative
   and the embedded OS or Go error text as best effort.
3. Should the effective configuration (budgets, tiers, store dir) be logged? Today only the
   socket, the slot limit and the store stats are.
