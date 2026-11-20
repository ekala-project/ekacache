# ecd compiler slots

ecd limits how many real compiler and linker runs happen at once on a host. ec sends
`SLOT <build>` before it execs a compiler (on a cache miss or for an uncacheable command,
never for a hit) and `DONE <build>` afterwards. A token also dies with its connection. With
several Nix builds on one host, each running `make -j$(nproc)`, this keeps the number of
running compilers at `limit` instead of max-jobs × nproc. Among waiters, the build that holds
the fewest tokens is served first, so one build's `-j16` burst cannot starve another build's
serial configure probes (plain FIFO gave a 7× tail slowdown in experiments). This document
specifies the grant rule, the queue order, per-connection bookkeeping, and the client rule
that avoids deadlock.

Sources: `jigd/slots.go` (`NewSlots`, `Acquire`, `Release`, `grant`, `Stats`), `jigd/main.go:serve`
(the `SLOT`/`DONE` cases and the deferred release), `jigd/main.go:main` (`EKACACHED_SLOTS`),
`jigd/slots_test.go:TestSlotsFewestHeldFirst`, `jigd/serve_test.go:TestServeIdsAndPipelinedGets`,
`jig/src/process.cc` (`Slot`), `jig/src/cache_client.cc` (`AcquireSlot`, `ReleaseSlot`).
Formal model: [`spec/slots.qnt`](../spec/slots.qnt) (modules `slots` and `slotNesting`).

The key words MUST, MUST NOT, SHOULD, SHOULD NOT and MAY are used as in RFC 2119. Where this
document says "current behavior", that behavior is normative. The Rust implementation MUST
reproduce it unless the maintainer accepts the recommendation in the matching Quirk entry.

---

## State

```
Slots { limit: int, out: int, held: map<build, int>, waiting: list<{build, ready}> }   # one per daemon
serve-local: held_conn: map<build, int>                                                # one per connection
```

- `limit` is fixed at startup: `EKACACHED_SLOTS` if it parses as an int greater than 0, otherwise
  `runtime.NumCPU()` ([ecd-daemon.md](ecd-daemon.md#environment)). It is at least 1.
- `out` is the number of tokens handed out. `held[b]` is the number of tokens held by build
  `b`, summed over all connections. A key is deleted when its count drops to 0 or below.
- `waiting` is the queue, in arrival order.
- Invariants (all checked in the model): `0 <= out <= limit`, `out == Σ held ==
  Σ over connections of Σ held_conn`, `held[b] > 0` for every key, and the queue is non-empty
  only while `out == limit`.

## Operations

### Acquire(build), for `SLOT <build>`

```
lock
if out < limit and waiting is empty:           # immediate grant
    out++; held[build]++; unlock; return
append {build, ready} to waiting; unlock
block until ready is closed                    # the waiter's token was already counted by grant()
```

An immediate grant requires **both** a free token **and** an empty queue (`slots.go:31`). Given
the invariant, the second condition never decides anything, but the Rust code MUST keep the
same rule.

### Release(build)

```
lock
out--
held[build]--; if held[build] <= 0: delete held[build]
grant()
unlock
```

`Release` has no guard. It relies on `serve` calling it only for tokens the connection holds.

### grant(), with the lock held

```
while out < limit and waiting not empty:
    best = 0
    for i, w in enumerate(waiting):
        if held[w.build] < held[waiting[best].build]: best = i     # strict <: FIFO among equals
    w = waiting.remove(best)
    out++; held[w.build]++
    close(w.ready)
```

- `held[x]` reads as 0 for a build with no entry.
- The order is: fewest tokens held **by the waiter's build** (counted across all
  connections) first, and among equal counts, the earliest arrival first.
- The counts are re-read on every iteration, so a grant raises that build's count before
  the next one is chosen.
- `TestSlotsFewestHeldFirst` (`slots_test.go:9`): limit 2, `big` holds both tokens and queues a
  third request, then `small` queues. One `Release("big")` wakes `small` first (it holds 0
  versus 1), and the next release wakes `big`. The model tests `fewestHeldFirstTest` and
  `fifoAmongEqualsTest` check the same rule.

### Stats()

Returns `(out, len(waiting))`, reported as `slots=` and `waiting=` in STATS.

## Per-connection bookkeeping (`serve`)

| Event | Action | Reply |
|---|---|---|
| `SLOT b` | `Acquire(b)` (may block), then `held_conn[b]++` | `OK\n` after the grant |
| `DONE b` with `held_conn[b] > 0` | `held_conn[b]--`, `Release(b)` | `OK\n` |
| `DONE b` with `held_conn[b] == 0` | nothing | `OK\n` (Q-SLOT-2) |
| connection ends, for any reason | `Release(b)` once per held token, for each `b` in Go map order | none |

- One connection MAY hold several tokens, for several builds or the same one. Hanging up
  returns all of them (model test `hangUpReturnsTokensTest`, `serve_test.go:70-88`). In that
  test, `SLOT b1`, `DONE b1`, `SLOT b1` and a hang-up let a second connection's `SLOT b2` be
  granted, with limit 1.
- While blocked in `Acquire`, the goroutine does not read the socket. It cannot see a hang-up
  until the grant arrives (Q-SLOT-1).
- Tokens are released only by `DONE` or by the connection ending. The protocol has no lease,
  timeout or liveness check. A client process that is stopped (SIGSTOP) but not dead holds its
  token indefinitely.

## Client rule: EKACACHE_SLOT (informative, but required to avoid deadlock)

`jig/src/process.cc:29-49`:

- If `EKACACHE_SLOT` is set to a non-empty value in the environment, ec requests no slot.
- Otherwise ec connects, sends `SLOT $NIX_BUILD_TOP` (or `SLOT -` if `NIX_BUILD_TOP` is unset),
  and on `OK` sets `EKACACHE_SLOT=1` for the child it execs next. When it finishes, it sends
  `DONE <same build>`, reads one line, and unsets `EKACACHE_SLOT`.
- If anything fails (no daemon, I/O error, a reply other than `OK`), ec runs without a slot.

The rule exists because a nested request can deadlock (Q-SLOT-3). The model `slotNesting`
proves `noNestedWait` with `HONOR_EKACACHE_SLOT = true`, and shows it fails without the rule.

---

## Quirks

### Q-SLOT-1: An abandoned SLOT waiter is still granted

**Current behavior**: A client that disconnects while its `SLOT` is queued stays in the queue.
When its turn comes, `grant()` hands it a token (`out++`, `held[b]++`), the goroutine wakes,
increments `held_conn`, and writes `OK\n`. Writing to a Unix socket whose peer has closed
fails with `EPIPE`, so `serve` returns and the deferred loop releases the token. That release
runs `grant()` again. For a short time the dead waiter holds a real token, and its build's
count affects the fairness order. The model shows this with the witness
`witnessAbandonedWaiterGranted` and the test `abandonedWaiterTest`.

**Impact**: The token is briefly wasted and the queue is disturbed for a moment. No token is
leaked.

**Recommendation**: **ADOPTED (signed off 2026-11-17).** While a connection's `SLOT` is queued, the Rust ecd
MUST watch that connection for peer hang-up **without consuming** bytes the client may have
pipelined behind the `SLOT` (for example `poll` for `POLLRDHUP`/`POLLHUP`, or a `MSG_PEEK` read
that returns 0). On hang-up it MUST, under the slots lock, remove the waiter from the queue
(no token, no `OK`), then run the normal hang-up cleanup (release the tokens the connection
already holds) and close. Removing a waiter frees no token, so no grant follows from it.
`waiting=` in STATS drops at once. If the grant wins the race (it took the lock first), the
Go path applies: the write of `OK` fails and the token is released. Model:
`spec/slots.qnt` with `CANCEL_ABANDONED = true` (invariant `noDeadWaiters`).

### Q-SLOT-2: An unheld DONE is answered OK

**Current behavior**: `DONE b` replies `OK\n` even when this connection holds no token for `b`
(it never asked, already released it, or the token belongs to another connection). Nothing
changes. Model test `unheldDoneTest`.

**Impact**: A client cannot tell a real release from a no-op. A client also cannot release a
token held by another connection, which is a deliberate safety property.

**Recommendation**: keep as-is.

### Q-SLOT-3: A second SLOT on one connection, or a nested one, can deadlock

**Current behavior**: Two cases.

- **Same connection.** If a connection holds a token and sends another `SLOT` while
  `out == limit`, the request queues and the goroutine blocks. The `DONE` that would free the
  first token is never read, because the connection reads nothing while blocked. With
  limit 1 this blocks forever. With a larger limit, it blocks until someone else releases, and
  enough such connections deadlock the whole host. The protocol model notes this: "a second
  SLOT on the connection that holds the only token waits forever".
- **Nested processes.** If an ec process holds a token and its child ec (for example, rustc
  running `cc` for linking) asks for one on another connection, the child waits for a token
  its ancestor holds, and the ancestor cannot finish before the child does. This is
  `slotNesting`.

The daemon does nothing to detect or prevent either case. Avoiding them is entirely up to the
client's `EKACACHE_SLOT` rule.

**Impact**: A host-wide hang if a client ignores `EKACACHE_SLOT`, for example a different client,
a cleared environment, or a tool that scrubs variables between ec and its child.

**Recommendation**: keep as-is (normative client rule: a holder MUST export `EKACACHE_SLOT` and MUST
NOT send a second SLOT). Any daemon-side detection needs sign-off.

### Q-SLOT-4: The release order on hang-up is random

**Current behavior**: The deferred cleanup iterates over `held_conn`, a Go map, in random order.
Each `Release` runs `grant()`. When a connection holds tokens for several builds and others are
waiting, which waiters get the freed tokens can depend on the order of the releases.

**Impact**: Grant order is not deterministic for multi-build connections, which ec never
creates (one ec process sends one build name). The model chooses a build nondeterministically
(`drainOne`).

**Recommendation**: keep as-is. A Rust implementation MAY release in any order.

### Q-SLOT-5: Build names are arbitrary tokens

**Current behavior**: `<build>` is any whitespace-free token. ec sends `$NIX_BUILD_TOP`, or
`-` outside a Nix build. All non-Nix callers therefore share the bucket `-` and its fairness
count. A `NIX_BUILD_TOP` that contains white space (Unicode white space included) makes
`SLOT` a 3-field line, and the daemon hangs up (see Q-PROTO-3). ec then runs without a slot.

**Impact**: Fairness works per build only if every build has a distinct `$NIX_BUILD_TOP`.

**Recommendation**: keep as-is.

---

## Disagreements with the models

- `spec/slots.qnt`, module `slots`: none found. The grant rule, `bestIndex` (strict `<`), the
  deletion at `<= 0`, the abandoned-waiter path and the unheld DONE all match the code.
- `spec/slots.qnt`, module `slotNesting`: its `finish` hands the token to the **head** of the
  queue (FIFO), not to the fewest-held build. With LIMIT = 1 and the purpose of that module,
  this simplification does not affect `noNestedWait`, but it is not the daemon's order.
- `spec/ecd_protocol.qnt` used to grant `SLOT` whenever `out < LIMIT`, even with blocked
  waiters. Fixed 2026-11-17: an immediate grant now also requires an empty queue.

## Open questions

1. Should the build name be part of the fairness key at all for `-` (non-Nix) callers, which
   share one count?
2. Is it intended that `limit` follows the CPU affinity mask at daemon start (Go's
   `runtime.NumCPU`) rather than the online CPU count or a cgroup quota? See
   [Q-DAEMON-13](ecd-daemon.md#q-daemon-13-the-default-slot-limit-is-the-affinity-mask-cpu-count).
