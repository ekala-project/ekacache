# The store on disk

## Layout

| Path | Contents |
|---|---|
| `$XDG_CACHE_HOME/ekacache/packs/` (default `~/.cache/ekacache/packs/` of `ecd`'s user) | the main tier |
| `$EKACACHED_COLD/` | the cold tier, if configured |
| `NNNNNN.pack` | data: an append-only file of up to about 256 MiB |
| `NNNNNN.hint` | the index of a sealed pack |

`ecd` writes new entries to one *active* pack at a time. When it is full, `ecd` seals it,
writes its hint file, and starts a new one. Each start of `ecd` also begins a new active
pack.

Limits: a single value can be at most 2 GiB, and a key at most 4096 bytes.

**Keep the pack directories for `ecd` alone.** Don't put other files in them. Stray files
whose names look like packs can confuse startup or stop eviction.

## Staying within budget

When a write takes the main tier over `EKACACHED_SIZE`, a background task moves whole
packs out, never the active one. It picks:

1. packs that are more than half superseded by newer values;
2. then the least recently read;
3. then the oldest.

They move to the cold tier, or are deleted if there is none. The cold tier is trimmed the
same way, by deletion. Read recency is kept in the packs' modification times, so it
survives restarts.

Eviction works on whole packs. After a restart, a key whose newest value was evicted can
come back with an older value, if an older copy survives in a pack that was kept. That
older value is still validated like any other entry.

## Crashes and power loss

- A sealed pack is synced to disk, along with its hint and the directory. Moves to the cold
  tier are synced too.
- Individual writes to the active pack are **not** synced. After a power loss, the latest
  entries can be lost, and an older value of a key can come back.
- At startup, `ecd` checks the pack that was active, truncates a partial last record
  (`pack N: truncating torn tail …`) and seals it.

None of this affects build correctness: every lookup is validated, and a lost entry is a
miss.

## Disk full

- A write that fails is rolled back and logged as `put <key>: <error>`. `ec` prints
  `ec: cache put <key> failed`, and the build carries on uncached.
- A full disk while `ecd` is starting can make it fail to start, or stop eviction. Free
  some space or lower `EKACACHED_SIZE`. An over-budget store is trimmed at startup.

## Wiping the cache

The store is only a cache. Builds never depend on it for correctness. To start over:

```console
# systemctl stop ecd
# rm -rf /var/lib/ekacache/ekacache/packs     # i.e. $XDG_CACHE_HOME/ekacache/packs
# rm -rf "$EKACACHED_COLD"/*.pack "$EKACACHED_COLD"/*.hint   # if you use a cold tier
# systemctl start ecd
```

Stop `ecd` first. `ecd` creates the directories again when it starts. There is no command to
delete individual entries.

## Restarts and counters

The `STATS` counters start at zero after a restart. The entries themselves survive.
