# 04: jigd store (phase P1)

Specified by: `jigd-store.md` (all of it), `spec/store.qnt`, quirks Q-STORE-1..15.
Adopted here: Q-STORE-1 (sorted hints), Q-STORE-2 (drop empty packs), Q-STORE-5 (kick at
start), Q-STORE-7 (fsync on seal and demotion).

The store is plain synchronous Rust (`std::fs`, `std::sync`) with no tokio types in it. The
server (03) calls into it from blocking-pool threads. This keeps the store testable without a
runtime, and maps it one to one onto the Go code and `spec/store.qnt`.

## Module layout (`crates/ekacached/src/store/`)

| File | Contents |
|---|---|
| `mod.rs` | `Store`, `StoreConfig`, `Stats`, public API |
| `naming.rs` | `%06d` formatting; the Go `Sscanf("%06d.pack")` emulation (table in §OpenStore) |
| `pack.rs` | `Pack { id, tier, file: ArcSwap-free Mutex<Arc<File>>, size, live, used: AtomicI64, touched: AtomicI64 }` |
| `record.rs` | record header encode/decode; hint entry encode/decode |
| `load.rs` | `open_store`, `load(tier, id)`, torn-tail truncation |
| `write.rs` | `put`, `rotate`, `seal` |
| `read.rs` | `get` → `ValueReader` |
| `demote.rs` | demoter thread, `victim`, `move_down`, `move_file` |
| `clock.rs` | `trait Clock { fn now_ns(&self) -> i64 }`, the system clock, and a test clock (the Go tests use a virtual clock: `TestEvictRecencySurvivesRestart`) |

## State and locking

```rust
pub struct Store {
    inner: RwLock<Inner>,           // the Go mu: RWMutex
    tiers: Vec<TierCfg>,            // dir, budget (immutable after open)
    demote_tx: SyncSender<()>,      // capacity 1; kick = try_send (coalesces)
    demoter: Mutex<Option<JoinHandle<()>>>,
    clock: Arc<dyn Clock>,
}
struct Inner {
    index: HashMap<Box<[u8]>, Loc>, // Loc { pack: u32, off: u32, len: u32 }
    packs: BTreeMap<u32, Arc<Pack>>,
    totals: Vec<i64>,               // per tier
    active: u32,
    closed: bool,
}
```

- `packs` is a `BTreeMap` so that "iterate packs" is deterministic. Go's map order is random,
  and nothing observable may depend on it. The one place it leaks is hint order, and adopted
  Q-STORE-1 sorts that.
- `Pack::file` is an `Arc<File>` behind a small mutex, because demotion swaps it. A
  `ValueReader` clones the `Arc`, so a demoted or evicted pack's old descriptor stays open
  while any reader holds it (`TestGetSurvivesEviction`). This replaces Go's finalizer.
- `std::sync::RwLock` with writer preference is fine: Go's `RWMutex` also blocks new readers
  once a writer waits. Writer starvation behaviour is not part of the contract.

## Operations

### `put(key, size, body: Body) -> Result<(), PutError>`

`Body` is either `Mem(Vec<u8>)` (size < `spoolMin` = 4 MiB) or `Spool(File)` (an unlinked temp
file in tier 0, `0600`). The **server** fills the body from the socket *before* calling `put`
(the Go code reads the body before taking the lock), so a slow uploader never holds the lock.
Validation (`bad put`: key empty, > 4096, contains `\n` or space, size < 0 or > 2<<30) is done
first, by a separate `validate_put` that the server calls before reading the body. That keeps
the order of effects identical to Go: a `-1` size is rejected without reading.

Then under the write lock: closed check, rotate if `active.size + recLen > packLimit &&
active.size > 0`, positional `write_all_at` of header + key + body, truncate on error, index
update, `live`/`totals` accounting, then `kick()` if tier 0 is over budget. Error messages are
reproduced exactly (`put <key>: wrote <n> of <size>: <err>`), because the daemon logs them.

Go writes through the file position (`Seek` + `Write`). The Rust code uses `pwrite` at
`active.size`. The bytes are the same and there is no shared cursor to get wrong.

### `get(key) -> Option<ValueReader>`

Read lock, `used = now`, the CAS-guarded `touched` update, then a `utimensat` that sets mtime
only (`UTIME_OMIT` for atime, as `jigd-store.md §Recency` requires), with errors ignored.
`ValueReader { file: Arc<File>, off, len }` has `read_at`-based chunked reads and exposes
`(fd, off, len)` so the server can use `sendfile` later (not needed for parity).

`has(key)` is `get(key).is_some()`, which includes the recency side effects (the doc says HAS
goes through the same function).

### `seal(pack)`: adopted Q-STORE-1, Q-STORE-7

1. Collect the index entries of this pack (still a full index walk under the lock, Q-STORE-12
   kept: it is a latency quirk, not an output quirk) and **sort them by record offset**.
2. Write `%06d.hint.tmp` (`0666 & ~umask`, truncating) through a `BufWriter`.
3. Adopted Q-STORE-7: `fsync` the pack file, then `fsync` the hint temp file.
4. `rename` to `%06d.hint`, then `fsync` the directory.

Readers accept any order (Go-written hints are unsorted), so `load` does not assume sorting.
Test: seal a pack with 1 000 keys and check that the hint is sorted and that the Go store
loads it (difftest), and that the Rust store loads a Go-written unsorted hint.

### `open_store(cfg)`: adopted Q-STORE-2, Q-STORE-5

Follows `jigd-store.md §OpenStore` and `§load` line by line, including:

- the bytewise-sorted glob of `*.pack`, and the **exact** `Sscanf("%06d.pack")` table
  (`42.pack` → 42 then open `000042.pack`; ` 00042.pack` → 42; `0000042.pack` → skip; …).
  `naming::scanf_pack_id` gets one unit test per table row (Q-STORE-9 reproduced);
- duplicate ids across tiers: delete the slower copy and the hint named after the id;
- hint loading: stop silently on a short read; skip entries past EOF; newest pack id wins;
- unsealed packs: scan, truncate the torn tail with the exact log line, then seal;
- the empty-key oddity and the up-to-4 GiB key allocation (Q-STORE-6). The allocation is a
  crash risk, and reproducing an OOM is not behaviour anyone depends on. The Rust loader caps
  the key buffer at `min(klen, remaining file bytes)`, and treats `klen > remaining` as a
  short read (stop). That is the outcome Go reaches when the allocation succeeds, so the
  result is the same and only the OOM path differs. **Flag for review:** this is the one
  deliberate deviation in the loader. It needs no sign-off if you agree that "OOM-killed" is
  not specified behaviour.

Adopted changes at the end of open:

- **Q-STORE-2:** a pack with size 0 after load (and its hint) is deleted rather than kept. At
  close, an active pack that is still empty is deleted instead of sealed.
- **Q-STORE-5:** after `rotate(last + 1)`, start the demoter and **kick once**, so a store
  that was over budget at shutdown is trimmed without waiting for the next PUT.

Both flags exist as `DROP_EMPTY_PACKS` / `KICK_AT_START` in `spec/store.qnt`. The Rust replay
test (12) runs traces with the flags on.

### Demoter

A dedicated **std thread** (not a tokio task, because it does blocking file copies), fed by the
capacity-1 `SyncSender<()>`. Its loop is the same as Go's: pick `victim()` under the lock,
release the lock, `move_down`; on error log `pack %06d: <err>` and stop the inner loop.

- `victim()`: first over-budget tier with candidates (not the active pack); order stale
  (`live*2 < size`) first, then `used` ascending, then `id` ascending.
- `move_down` on the last tier evicts: drop the index entries, remove files, adjust totals, log
  `evicted pack %06d (%d MiB, %d MiB live)`. Older records of the same keys are **not**
  re-indexed (Q-STORE-8 kept).
- `move_down` to a slower tier: `move_file` (link, or EEXIST counts as ok (Q-STORE-10 kept),
  else copy to `.tmp` and rename). Adopted Q-STORE-7: `fsync` the destination pack and hint
  and the destination directory **before** the switch under the lock. Then
  open, set mtime = `used`, switch `file`/`tier`/totals under the lock, remove the source
  files, and log `demoted pack %06d to %s (…)`.

### `close()`

Set `closed`, drop the sender so the demoter loop ends after it has drained all pending work,
`join` the thread, then seal (or, with Q-STORE-2, delete if empty) the active pack. File
descriptors stay open, and `get` keeps working until the process exits.

## Tests

1. **Ported `store_test.go`** (340 lines), one Rust test per Go test with the same name in
   snake case: `TestTornTail`, `TestHintPastEOF`, `TestReopenNewestAcrossTiers`,
   `TestGetSurvivesEviction`, `TestEvictOldestPack`, `TestEvictKeepsReadPack`,
   `TestEvictSupersededFirst`, `TestTiers`, `TestEvictRecencySurvivesRestart`,
   `TestPutAfterClose`, and the rest. Where an adopted change alters an expectation (for
   example the extra empty pack after restart), the test asserts the **adopted** result and
   says so in a comment citing the Q number.
2. **Format compatibility (difftest):** Go jigd writes a store → Rust opens it → same `STATS`
   and the same `GET` bytes for every key. Then the reverse: Rust writes → Go opens. This
   includes sorted hints, a torn tail, and a two-tier layout.
3. **Quint replay:** ITF traces from `spec/store.qnt` (with the adopted flags on) drive a
   `Store` with a test clock and small `packLimit`/budgets (made configurable for tests
   through `StoreConfig`, with defaults equal to the constants). After every step the replay
   checks the model's `liveAccounting`, `tierTotals` and `indexSound` against the real state.
   The crash steps reopen the store from disk.
4. **Crash fuzzing:** a `cargo fuzz` target writes random record sequences, truncates or
   zero-fills the tail at a random point, reopens, and checks `indexSound` (every value
   served is one that was written under that key).
5. **Durability check (manual / nightly):** run under `strace -f -e trace=fsync,fdatasync,rename`
   and assert the order fsync(pack) → fsync(hint.tmp) → rename → fsync(dir) on seal.
