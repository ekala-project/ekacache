# jigd pack store

jigd keeps cache values in a bitcask-style store. Values are appended to an active pack file,
an in-memory index maps each key to the place its value lives, and sealed packs get a hint
file so that startup does not have to scan their data. Packs live in one or two tiers, each a
directory with a byte budget. A tier that goes over its budget moves whole packs to the next
tier, and the last tier deletes them. This document specifies the on-disk formats byte for
byte, file naming, writing, sealing and rotation, startup recovery, `live` and budget
accounting, victim selection, demotion and eviction, and recency. A Rust store MUST be able to
open a directory written by the Go daemon and the other way round, and it MUST make the same
eviction decisions.

Sources: `jigd/store.go` (`OpenStore`, `load`, `seal`, `rotate`, `Get`, `Put`, `kick`,
`demoter`, `victim`, `moveDown`, `moveFile`, `Stats`, `Close`), `jigd/main.go:main` (tier
construction), `jigd/store_test.go` (all tests). Formal model: [`spec/store.qnt`](../spec/store.qnt).

The key words MUST, MUST NOT, SHOULD, SHOULD NOT and MAY are used as in RFC 2119. Where this
document says "current behavior", that behavior is normative. The Rust implementation MUST
reproduce it unless the maintainer accepts the recommendation in the matching Quirk entry.

---

## Constants

| Name | Value | Meaning | Source |
|---|---|---|---|
| `packLimit` | 268 435 456 (256 MiB) | a non-empty active pack is sealed before a record would take it past this size | `store.go:33` |
| `recHeader` | 8 | bytes of record header (u32 + u32) | `store.go:34` |
| `spoolMin` | 4 194 304 (4 MiB) | a PUT body of at least this size is read into a temp file instead of memory | `store.go:35` |
| `touchEvery` | 1 minute | minimum interval between mtime updates of one pack on reads | `store.go:30` |
| max key length | 4 096 bytes | longer keys get `bad put` | `store.go:264` |
| max value length | 2 147 483 648 (`2<<30`), inclusive | longer values get `bad put` | `store.go:264` |

## Directory layout and naming

- **Tier 0** (writes): `$XDG_CACHE_HOME/jigd/packs`, or `~/.cache/jigd/packs` when
  `XDG_CACHE_HOME` is unset or empty. Its budget is `JIGD_SIZE` GiB.
- **Tier 1** (optional): `$JIGD_COLD`, used verbatim when it is non-empty. Its budget is
  `JIGD_COLD_SIZE` GiB. Budget parsing is in [jigd-daemon.md](jigd-daemon.md#environment).
- A budget of `<= 0` means unbounded (`store.go:44`, `store.go:363`).
- `OpenStore` creates every tier directory with `MkdirAll(dir, 0755)`, so the mode is subject
  to the umask.

| File | Name | Mode at creation | Notes |
|---|---|---|---|
| pack | `%06d.pack`: decimal id, zero-padded to at least 6 digits | `0644` &~umask, `O_RDWR\|O_CREAT\|O_EXCL` | ids above 999 999 get 7 or more digits (Q-STORE-3) |
| hint | `%06d.hint` | `0666` &~umask (`os.Create`) | written as `%06d.hint.tmp`, then renamed |
| hint temp | `%06d.hint.tmp` | `0666` &~umask, truncating | a leftover is never cleaned up, and the next seal overwrites it |
| put spool | `put-<random>.tmp` in tier 0 | `0600` (`os.CreateTemp`) | unlinked straight after creation |
| copy temp | `<dest>.tmp` in tier 1 | `0666` &~umask | only used for a cross-filesystem demotion |

Pack ids are `uint32`, assigned in increasing order, and unique across tiers. A pack keeps its
id when it is demoted.

## Pack file format

A pack is a concatenation of records with no file header, footer, padding or alignment.

| Offset in record | Size | Field | Encoding |
|---|---|---|---|
| 0 | 4 | `keylen` | u32 little-endian |
| 4 | 4 | `vallen` | u32 little-endian |
| 8 | `keylen` | key | raw bytes |
| 8 + `keylen` | `vallen` | value | raw bytes, exactly as the client sent it (jig compresses with zstd; the store neither knows nor cares) |

Record length = `8 + keylen + vallen`. Records carry no checksum, timestamp, tombstone or
version. The same key may appear many times, in one pack and across packs. The index decides
which record is current.

Example: `PUT k 3` with body `abc` gives the 12 bytes
`01 00 00 00 03 00 00 00 6B 61 62 63`.

## Hint file format

A hint lists the index entries that pointed into its pack **at the moment it was sealed**,
with no values.

| Offset in entry | Size | Field | Encoding |
|---|---|---|---|
| 0 | 4 | `keylen` | u32 LE |
| 4 | 4 | `vallen` | u32 LE |
| 8 | 8 | `offset` | u64 LE, offset in the pack of the **record** (its header), not of the value |
| 16 | `keylen` | key | raw bytes |

Entry length = `16 + keylen`. The file has no header and no count. Entries come in Go map
iteration order, which is random per run (Q-STORE-1). Each key appears at most once per hint.
Superseded records within the pack are left out. An empty hint (0 bytes) is valid.

Example: the hint for the pack above is 17 bytes:
`01 00 00 00 03 00 00 00 00 00 00 00 00 00 00 00 6B`.

## In-memory state

```
loc    = { pack: u32, off: u32 /* offset of the VALUE in the pack */, len: u32 }
pack   = { id: u32, tier: int, file: fd, size: i64, live: i64,
           used: atomic i64 /* unix ns of last read */, touched: atomic i64 /* mtime as last set */ }
Tier   = { Dir, Budget: i64, total: i64 }
Store  = { tiers, index: map<bytes, loc>, packs: map<u32, pack>, active: pack, closed: bool,
           demote: channel(cap 1), mu: RWMutex }
```

- `size` is the pack's byte length as the daemon tracks it.
- `live` is the sum of `len` over index entries that point into the pack (`spec/store.qnt`
  invariant `liveAccounting`).
- `tiers[t].total` is the sum of `size` over the packs in tier `t`, the active pack included
  (invariant `tierTotals`).
- `Stats()` returns `keys=len(index) packs=len(packs) bytes=Σ total live=Σ live`.

## Writing

### Put

`Put(key, size, r)` (`store.go:263-328`):

```
if len(key)==0 or len(key)>4096 or key contains '\n' or ' ' or size<0 or size>2<<30:
    return error "bad put"
if size < spoolMin: body = read exactly size bytes from r into memory    # error -> return it
else:
    tmp = CreateTemp(tiers[0].Dir, "put-*.tmp"); unlink(tmp)             # error -> return it
    copy exactly size bytes r -> tmp; seek tmp to 0; body = tmp           # error -> return it
lock mu (exclusive)
if closed: return error "store closed"
recLen = 8 + len(key) + size
if active.size + recLen > packLimit and active.size > 0:
    rotate(active.id + 1)                                                # error -> return it
seek active.file to active.size
write header(u32 LE len(key), u32 LE size) + key   # on error: truncate(active.size), return error
copy body -> active.file                           # on error or short: truncate(active.size),
                                                   #   return "put <key>: wrote <n> of <size>: <err>"
if key in index: packs[index[key].pack].live -= index[key].len
index[key] = { active.id, active.size + 8 + len(key), size }
active.size += recLen; active.live += size; tiers[0].total += recLen
if tiers[0].Budget > 0 and tiers[0].total > tiers[0].Budget: kick()
```

- **A record that does not fit goes in alone.** Rotation happens only when the active pack is
  non-empty. A record larger than `packLimit` is therefore written into an empty active pack
  (a fresh one if needed), and the next PUT rotates away from it. That pack holds exactly one
  record and is larger than `packLimit` (model test `oversizedRecordTest`). A pack may end up
  exactly `packLimit` bytes, because the test is `>`.
- Writing the same key again always appends a new record, even when the value is identical.
- `kick()` does a non-blocking send on the 1-slot `demote` channel, so repeated kicks
  coalesce. Only `Put` kicks, and only for tier 0 (Q-STORE-5).
- The body is read before the lock is taken. A slow uploader does not block other writers, and
  the RAM held per PUT stays below 4 MiB.

### Seal

`seal(p)` (`store.go:199-225`) writes `%06d.hint.tmp` with one entry for each index entry
whose `pack == p.id`, in map order. The record offset written is `loc.off − 8 − keylen`. The
function then flushes, closes, and renames the file to `%06d.hint`. Write errors show up at
Flush (Go's bufio errors are sticky), and Flush, Close and Rename errors are returned. On a
Flush error the temp file stays behind. Nothing is fsynced, neither the file nor the
directory (the Rust port fsyncs, adopted Q-STORE-7). Seal walks the **whole** index while holding the lock (Q-STORE-12).

### Rotate

`rotate(id)` (`store.go:227-242`):

```
if active != nil: seal(active)                       # error -> return it, nothing changes
file = open(tiers[0].Dir/%06d.pack(id), O_RDWR|O_CREAT|O_EXCL, 0644)   # error -> return it
active = { id, tier 0, file, size 0, live 0, used = touched = now }
packs[id] = active
```

`O_EXCL` means an existing file with that name makes rotate fail. This can make startup fatal
or block writes (Q-STORE-3, Q-STORE-4, Q-STORE-11). The active pack is always in tier 0 and
never has a hint while the daemon runs normally.

## Reading

### Get

`Get(key)` (`store.go:246-260`) holds the read lock throughout:

```
l = index[key] or return nil
p = packs[l.pack]
now = clock()
p.used = now
last = p.touched
if now - last > touchEvery and CAS(p.touched, last, now):
    chtimes(path of p's .pack in its current tier, atime = unchanged, mtime = now)  # error ignored
return a reader over p.file at [l.off, l.off + l.len)
```

- The reader is positional (`pread`/`sendfile` with an offset), so it does not depend on the
  file position that `Put` moves.
- Readers stay valid after the pack is demoted or evicted, because the old descriptor is never
  closed explicitly. Go's finalizer closes it once it is unreachable (`store.go:51-52`,
  `TestGetSurvivesEviction`). The Rust code MUST keep the file alive while any reader exists,
  for example with `Arc<File>`.
- `HAS` goes through the same function (see [jigd-protocol.md](jigd-protocol.md#has)).
- `Get` does not check whether the store is closed.

### Recency

- `used` is updated on every hit (GET or HAS) and orders victims.
- The pack's **mtime** stores recency across restarts. `load` initialises both `used` and
  `touched` from the mtime (`store.go:132-133`). `Get` moves the mtime at most once per
  `touchEvery` per pack, and the CAS ensures only one of several concurrent readers does it.
  Writes to the active pack also move its mtime (the kernel does this). After a restart,
  recency therefore equals max(last write, last touch). Reads in the last minute before a
  restart may be lost.
- `chtimes` passes Go's zero `time.Time` for atime, which the toolchain in use (Go 1.25)
  treats as "leave unchanged" (`UTIME_OMIT`). The Rust code MUST set only the mtime.
- After a pack is demoted, the destination file's mtime is set to `used`
  (`store.go:416`, error ignored).
- A new active pack starts with `used = touched = now` (`store.go:238-239`), so "being written
  counts as recent".
- `TestEvictRecencySurvivesRestart` pins this: pack 1 is read one (virtual) hour after pack 2
  was written, and after a restart pack 2 is evicted, not pack 1.

## Startup: `OpenStore` and `load`

### OpenStore

```
last = 0
for t in 0 .. len(tiers)-1:                         # fast tier first
    MkdirAll(tiers[t].Dir, 0755)                    # error -> fatal
    names = sort_bytewise(glob(tiers[t].Dir + "/*.pack"))
    for name in names:
        id = sscanf(basename(name), "%06d.pack")   # failure -> skip silently (no log)
        if id in packs:                            # already known (normally from a faster tier)
            remove(name); remove(tiers[t].Dir/%06d.hint(id))   # errors ignored
            continue
        if load(t, id) fails with err:
            log "pack %06d: <err> (skipped)"; continue        # `last` NOT updated
        last = max(last, id)
rotate(last + 1)                                    # error -> fatal (OpenStore returns it)
start demoter goroutine                             # NOT kicked (Q-STORE-5)
```

**Parsing names with `Sscanf("%06d.pack")`** is looser than the name format (checked with Go
1.25). Matching is on `basename`, and the glob also matches names that merely end in `.pack`:

| Basename | Parsed id |
|---|---|
| `000042.pack` | 42 |
| `42.pack` | 42 (then `load` opens `000042.pack`) |
| ` 00042.pack` (leading space) | 42 |
| `000042.pack.pack` | 42 (trailing text ignored) |
| `999999.pack` | 999 999 |
| `1000000.pack`, `0000042.pack` | skip (at most 6 digits are read, then `.pack` must follow) |
| `+00042.pack`, `-00001.pack`, `abc.pack` | skip |

The Rust implementation MUST reproduce this table (Q-STORE-9 describes the consequences).

**Duplicates.** If an id has already been loaded, the file is a leftover of an interrupted
demotion. The slower copy and the hint **named after the id** are deleted. Because tiers are
scanned fast first, the fast copy wins (`TestReopenNewestAcrossTiers`). The check is "id is in
`packs`", and a pack that failed late in `load` is in `packs` (Q-STORE-4).

### load(tier, id)

```
file = open(Dir/%06d.pack(id), O_RDWR)              # error -> return it (nothing registered)
size = fstat(file).size                             # error -> close, return it
p = { id, tier, file, size, live 0, used = touched = mtime }
packs[id] = p                                       # registered from here on
add(key, l):                                        # newest pack id wins, whatever the load order
    if key in index and index[key].pack > l.pack: return
    if key in index: packs[index[key].pack].live -= index[key].len
    index[key] = l; p.live += l.len
if open(Dir/%06d.hint(id)) succeeds:                # sealed
    loop:
        read 16 bytes (klen u32, vlen u32, off u64) # short/EOF/error -> stop silently
        read klen bytes key                          # short/error -> stop silently
        if off + 8 + klen + vlen > size: continue    # entry past EOF: skip it
        add(key, { id, u32(off) + 8 + klen, vlen })
    tiers[tier].total += size
    return ok                                        # a hinted pack is never truncated or rewritten
else:                                               # unsealed: was active when the daemon stopped
    off = 0
    loop:
        read 8 bytes (klen, vlen)                    # short/EOF -> stop
        read klen bytes key                          # short -> stop
        skip vlen bytes                              # short -> stop
        add(key, { id, off + 8 + klen, vlen }); off += 8 + klen + vlen
    if off != size:
        log "pack %06d: truncating torn tail at %d (was %d)"
        ftruncate(file, off)                         # error -> return it (partial state stays)
        p.size = off
    tiers[tier].total += p.size
    return seal(p)                                   # error -> return it (partial state stays)
```

Notes:

- **Torn tail truncation.** Only an unsealed pack is scanned and truncated, after the last
  complete record (`TestTornTail`: a 9-byte half record is cut off and the file returns to
  14 bytes). The pack is then sealed and never written again. New writes go to a new pack.
- **Hint entries past EOF are skipped.** The check uses the pack size at open time
  (`TestHintPastEOF`). Hint entries are never checked against the pack's contents.
- **Newest pack id wins.** Within a pack, a later record replaces an earlier one, because the
  rule is "replace unless the existing entry's pack is newer". The replaced bytes leave the
  old pack's `live`.
- **The empty-key oddity.** Because records have no checksum, a run of zero bytes at the end of
  an unsealed pack (common after power loss) scans as valid records with key `""` and length
  0 (Q-STORE-6).
- Corrupt length fields lead to allocations of up to 4 GiB for the key buffer
  (`make([]byte, klen)`), and possibly to an OOM kill (Q-STORE-6).

### Each start makes a new pack

(The Rust port deletes empty packs at open and at close and kicks the demoter after rotate: adopted Q-STORE-2 and Q-STORE-5.)


`rotate(last + 1)` always creates a new empty active pack, even when the newest pack is almost
empty (Q-STORE-2). Verified: start, `PUT`, stop, start, stop leaves `000001.pack` (12 bytes),
`000001.hint` (17 bytes), `000002.pack` (0 bytes) and `000002.hint` (0 bytes).

## Budgets, victims, demotion and eviction

### Demoter loop

```
demoter():                                         # one goroutine, started by OpenStore
    for each receive on demote (until the channel is closed):
        loop:
            lock mu; (p, ok) = victim(); unlock mu
            if not ok: break
            if moveDown(p) fails with err: log "pack %06d: <err>"; break
    signal done
```

Victim choice and the move are not atomic. Between them, `used` may change and Puts may
happen, but only the demoter moves or deletes packs.

### victim()

Called with `mu` held (`store.go:361-386`):

```
for t in 0 .. len(tiers)-1:
    if tiers[t].Budget <= 0 or tiers[t].total <= tiers[t].Budget: continue
    cands = [p in packs where p.tier == t and p != active]
    if cands empty: continue                        # e.g. only the active pack is over budget
    stale(p) = p.live * 2 < p.size                  # strictly more than half dead
    sort cands by: stale first; then used ascending; then id ascending
    return cands[0]
return none
```

- The first over-budget tier (in tier order) that has a candidate is served first.
- A 0-byte pack is never stale (`0 < 0` is false).
- Pinned by `TestEvictOldestPack`, `TestEvictKeepsReadPack`, `TestEvictSupersededFirst` (a
  stale pack goes first even when its `used` is in the future) and `TestTiers`.

### moveDown(p)

The last tier evicts (`store.go:391-405`). Everything below runs under `mu`:

```
delete every index entry with pack == p.id        # walks the whole index
remove .pack and .hint                            # errors ignored
delete packs[p.id]; tiers[last].total -= p.size
log "evicted pack %06d (%d MiB, %d MiB live)" (p.size>>20, p.live>>20)
```

Older records of the same keys in other packs are **not** re-indexed. Those keys miss until a
restart brings them back (Q-STORE-8).

Any other tier demotes (`store.go:406-427`):

```
to = tiers[from+1].Dir
for ext in [pack, hint]:                           # without the lock
    moveFile(from/%06d.ext, to/%06d.ext)           # error -> return it (the demoter logs and stops)
file = open(to/%06d.pack, O_RDWR)                  # error -> return it
chtimes(to/%06d.pack, atime unchanged, mtime = p.used)   # error ignored
lock mu
p.file = file; p.tier = from+1
tiers[from].total -= p.size; tiers[from+1].total += p.size
remove from/%06d.pack, from/%06d.hint              # errors ignored
log "demoted pack %06d to %s (%d MiB, %d MiB live)" (id, to, size>>20, live>>20)
unlock
```

`moveFile(from, to)` (`store.go:430-453`):

```
if link(from, to) succeeds or fails with EEXIST: return ok       # Q-STORE-10
copy from -> to + ".tmp" (create/truncate); on copy error remove tmp
close; rename(to + ".tmp", to)
```

There is no fsync anywhere (Rust port: see adopted Q-STORE-7). Reads keep coming from the old file until the switch. A crash
between the copy and the switch leaves the pack in both tiers, and the next start deletes
the slower copy.

### Close

`Close()` (`store.go:482-491`): set `closed` under the lock, close the `demote` channel, and
**wait until the demoter has finished every pending demotion and eviction** (its inner loop
runs until no victim is left). Then take the lock and seal the active pack. File descriptors
are not closed. A Put after Close returns `store closed` (`TestPutAfterClose`). Gets keep
working until the process exits.

## Durability

This section describes the Go daemon. The Rust port adds fsync on seal and demotion (adopted Q-STORE-7).


- **No fsync**, of data or of directories. A crash of the daemon alone loses nothing that
  `write(2)` accepted. A power loss can lose any suffix of recent writes and renames.
- **No checksums.** A record is trusted if its lengths fit inside the file.
- What recovery guarantees: `Get` never promises bytes beyond EOF as seen at load. Index
  entries come only from complete-looking records or from hint entries that fit. The model
  invariant `indexSound` holds even after power loss: every served value is one that was
  acknowledged under that key, as long as the bytes on disk are the bytes written.
- What it does not guarantee: after power loss a key can come back with an **older** value
  (Q-STORE-7). Zero-filled or garbage blocks are not detected (Q-STORE-6).

---

## Quirks

### Q-STORE-1: Hint entries are written in nondeterministic order

**Current behavior**: `seal` iterates a Go map, so the entry order of a hint differs from run
to run for the same index contents.

**Impact**: Hint files are not byte-reproducible. The order has no meaning when the file is
read (keys are unique per hint).

**Recommendation**: **ADOPTED (signed off 2026-11-17).** Write entries sorted by record offset. Readers
MUST accept any order, so Go and Rust files remain mutually readable. "Byte-compatible" for
hints MUST mean "same set of entries".

### Q-STORE-2: Each restart leaves a new pack

**Current behavior**: `OpenStore` always rotates to `last+1`. A start followed by a stop with
no PUTs leaves a 0-byte pack and a 0-byte hint. A start with a few PUTs leaves a small sealed
pack that is never appended to again.

**Impact**: Packs pile up (`packs=` grows) for daemons that restart often. 0-byte packs are
never stale, and leave only by LRU eviction or demotion.

**Recommendation**: **ADOPTED (signed off 2026-11-17).** Two rules, which together stop empty packs piling up
without ever reusing a pack id:
1. `Close` with an active pack of size 0 removes `%06d.pack` instead of sealing it (no hint).
2. At the end of `OpenStore`'s load loop (before `rotate`), every loaded pack of size 0 is
   deleted with its hint, in any tier, and dropped from `packs`. `last` still counts it, so
   the new active pack is `last + 1` and ids stay monotonic. Each deletion is logged as
   `removed empty pack %06d`.
Small non-empty packs from short runs are kept as they are. Model: `spec/store.qnt`
`DROP_EMPTY_PACKS = true` (invariant `noEmptySealedPacks`).

### Q-STORE-3: Pack ids above 999 999 cannot be parsed and are leaked

**Current behavior**: `%06d` writes 7 digits for ids of 1 000 000 and above, but
`Sscanf("%06d.pack")` reads at most 6 digits and then fails. Such packs are skipped
**silently**. They are never indexed, counted, demoted or evicted, and they stay on disk
forever. Worse, `last` then stops at 999 999 or below, so `rotate(last+1)` can hit an existing
file through `O_EXCL`, and `OpenStore` fails. The daemon exits 1 with
`open …/1000000.pack: file exists` on every start until someone removes the files.

**Impact**: Reaching this takes about a million packs or restarts (256 TB written, or a
million restarts), so it is unlikely but permanent once it happens.

**Recommendation**: change, needs sign-off: parse any run of decimal digits followed by
`.pack`. Until then, the Rust daemon MUST behave the same way.

### Q-STORE-4: A load failure leaves partial state and does not update `last`

**Current behavior**: The failures and what they leave behind:

- `open` or `fstat` fails (for example `EACCES`): nothing is registered. `last` is not
  updated.
- `ftruncate` or `seal` fails (read-only filesystem, ENOSPC) after `packs[id]` was set: the
  pack stays in `packs` and its records stay in the index. The pack is also counted in
  `total` if the seal failed, but not if the truncate failed. `last` is not updated.

In both cases, if the failing pack has the highest id (which is typical for the unsealed pack
that was active), `rotate(last+1)` names that very file, `O_EXCL` fails, and the daemon exits
1. If a lower id fails late, the pack is half-registered with no hint. A copy of the same id
in a slower tier is then deleted as a "duplicate". Once this hintless sealed pack is chosen
as victim, demoting it fails at the hint link (`ENOENT`) after the pack file was already
linked. The demoter logs the error and stops, and every later kick retries the same victim,
so tier 0 stays over budget.

**Impact**: One unwritable or unreadable pack file can stop the daemon from starting or
disable demotion.

**Recommendation**: keep as-is for compatibility. A change (unregister on failure and update
`last` from every parsed id) needs sign-off.

### Q-STORE-5: The demoter is not kicked at startup and only Put kicks

**Current behavior**: `OpenStore` starts the demoter without a kick. Only a `Put` that leaves
tier 0 over budget kicks it. A store that starts over budget (after `JIGD_SIZE` was lowered, or
after a cold tier came back) stays over budget until such a Put. A tier 1 that is over budget
while tier 0 is not is never reduced. The model shows this with the failing invariant
`idleWithinBudgetEvenAfterStart`.

**Impact**: Disk use can exceed the configured budget indefinitely on a read-mostly host.

**Recommendation**: **ADOPTED (signed off 2026-11-17).** `OpenStore` MUST kick the demoter once after `rotate`,
so any tier that starts over budget is reduced without waiting for a Put. victim() already
covers every bounded tier, so an over-budget cold tier is handled too. Model:
`spec/store.qnt` `KICK_AT_START = true` (`idleWithinBudgetEvenAfterStart` then holds).

### Q-STORE-6: No record checksums

**Current behavior**: Records and hints carry no checksum. Garbage that parses is indexed as
data. In particular, zero-filled blocks at the end of an unsealed pack (typical after power
loss on delayed-allocation filesystems) scan as a series of 8-byte records with key `""` and an
empty value. The empty key cannot be requested over the protocol, but it counts in `keys=`
and is written to hints. A corrupt `keylen` makes the daemon allocate up to 4 GiB.

**Impact**: Silent corruption of values is possible, though values are zstd frames, so jig's
decompression usually rejects them. There is also a chance of an OOM at startup.

**Recommendation**: keep as-is. Adding checksums changes the format and needs sign-off.

### Q-STORE-7: Without fsync, superseded values can come back after power loss

**Current behavior**: Nothing is fsynced. After power loss, the newer pack that holds the
latest record of a key can lose that record, while an older sealed pack, long since on disk,
still has the earlier record. At load, the newer entry is missing (scan) or skipped (hint
past EOF), so the older value is served. The model shows this with the witness
`witnessPowerLossFallback`, and `newestWins` holds only without power loss. A hint renamed
into place can also be empty or short after power loss, in which case that pack's keys are
silently missing.

**Impact**: A key may return a stale value. jig's keys are content hashes in practice, so a
stale value is usually still correct.

**Recommendation**: **ADOPTED (signed off 2026-11-17).** **fsync on seal and on demotion** (not per PUT):
- `seal(p)`: `fsync` the pack file. Write `%06d.hint.tmp`, flush, `fsync` it, close, rename it
  to `%06d.hint`, then `fsync` the tier directory. An fsync error is a seal error (same
  handling as a Flush error today).
- `moveDown` into a non-last tier: after the copy path, `fsync` `to.tmp` before the rename.
  After both files are in place (linked or copied), `fsync` the destination directory before
  taking the lock for the switch, so the source is removed only once the destination is
  durable.
- The active pack is still not fsynced per PUT. After a power loss the most recent PUTs to it
  can be lost, and for such a key an older value from a sealed pack is served. A sealed pack
  never loses records, so the Go fallback where a sealed pack's hint points past its truncated
  end is gone. Model: `spec/store.qnt` `FSYNC_ON_SEAL = true` (`witnessPowerLossFallback` no
  longer reachable).

### Q-STORE-8: Evicting a newer pack hides older values until a restart

**Current behavior**: Eviction deletes the index entries that point into the evicted pack. It
does not look for older records of those keys in surviving packs. After a restart, those
older records are indexed again from their packs' hints (they were live when those packs were
sealed). A key that missed before the restart therefore hits after it, with an older value.
No power loss is needed for this.

**Impact**: Hit or miss behavior is not stable across restarts. The value is an old one.

**Recommendation**: keep as-is.

### Q-STORE-9: Pack name parsing is loose and can delete a live hint

**Current behavior**: `42.pack`, ` 00042.pack` and `000042.pack.pack` all parse as id 42
(§OpenStore). If `000042.pack` also exists, it sorts first and loads. The stray file is then
treated as a duplicate, and the daemon removes the stray file **and `000042.hint`**, the hint
of the live pack. The next start scans pack 42 instead (data is kept). If no `000042.pack`
exists, `load` fails to open it and logs `(skipped)`.

**Impact**: Foreign files in the pack directory have surprising effects.

**Recommendation**: keep as-is. A change to strict parsing needs sign-off.

### Q-STORE-10: Demotion accepts an existing destination file

**Current behavior**: `moveFile` treats `EEXIST` from `link` as success, without checking
that the existing file is the same pack. The daemon then opens the destination and points the
pack at it.

**Impact**: In normal operation the destination cannot exist, because ids are unique and
leftovers are deleted at startup. With foreign files (Q-STORE-3, Q-STORE-4), the daemon could
serve bytes from an unrelated file.

**Recommendation**: keep as-is.

### Q-STORE-11: A rotate that fails after sealing leaves a stale hint

**Current behavior**: `rotate` seals the active pack first and then creates the new pack with
`O_EXCL`. If the create fails, the old pack is still active but now has a hint. PUTs that
would rotate fail. Smaller PUTs that still fit are appended after the point where the hint
ends. If the daemon then crashes, those records are not loaded, because a pack with a hint is
not scanned.

**Impact**: Only reachable through the collisions in Q-STORE-3 and Q-STORE-4.

**Recommendation**: keep as-is.

### Q-STORE-12: Seal and eviction walk the whole index under the write lock

**Current behavior**: Both `seal` (on every rotation) and eviction iterate over the entire
index while holding `mu` exclusively, which blocks all Gets.

**Impact**: Latency spikes in proportion to the number of keys on each rotation.

**Recommendation**: keep the observable behavior. The Rust implementation MAY use a per-pack
key set, because the results are identical.

### Q-STORE-13: Shutdown waits for every pending demotion

**Current behavior**: `Close` waits until the demoter has no victim left. Cross-filesystem
copies of 256 MiB packs can make shutdown take minutes, and a second SIGTERM is ignored
(Q-DAEMON-6).

**Impact**: Slow shutdown. If a supervisor sends SIGKILL, the active pack is left unsealed,
which the next start handles by scanning it.

**Recommendation**: keep as-is.

### Q-STORE-14: Packs must be writable to load

**Current behavior**: `load` opens every pack, sealed ones included, with `O_RDWR`. Sealed
packs in a read-only cold tier fail to open and are skipped, with a log line.

**Impact**: A tier cannot be mounted read-only.

**Recommendation**: keep as-is.

### Q-STORE-15: Errors in a hint stop its load silently

**Current behavior**: A short read or I/O error in a hint ends the hint loop without an error.
Entries after that point are missing, the pack's `size` still counts fully, and its `live`
is low, so the pack looks stale and becomes an early victim.

**Impact**: Silent loss of index entries.

**Recommendation**: keep as-is.

---

## Disagreements with `spec/store.qnt`

- **Demotion onto an existing file.** The model's `demoteEffect` overwrites any destination
  (`files.put(...)`). The code keeps an existing destination (Q-STORE-10). This only matters
  with foreign files. The model now carries a comment explaining why the two are equivalent for
  the states it explores.
- **Not modeled.** The model has no I/O failures (Q-STORE-4, Q-STORE-11), no name parsing
  (Q-STORE-3, Q-STORE-9) and no hint read errors (Q-STORE-15). Its `openStore` iterates ids
  `1..maxId` numerically, which matches the code's lexical order only while every id is at most
  999 999.
- Everything else (record and hint layouts, rotation, `live`, totals, victim order, newest
  wins, past-EOF skipping, the empty pack per restart, no kick at start, power-loss fallback)
  matches the code.

## Open questions

1. Is a hint's entry order meant to carry any meaning (for example, for a future compaction
   pass)? The code gives it none.
2. Should a key that misses because its newest pack was evicted come back after a restart
   (Q-STORE-8)? This is intended only if values are content-addressed. The store cannot tell.
3. Which Go toolchain is normative for `os.Chtimes(zero atime)`? The Go 1.25 standard
   library documents that a zero `time.Time` leaves that timestamp unchanged. `go.mod` says
   `go 1.22`, and it was not checked whether older toolchains behave the same way. This
   document assumes "unchanged".
