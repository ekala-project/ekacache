# Monitoring

## Daemon statistics

Send `STATS` on the socket:

```console
$ printf 'STATS\n' | nc -U -w1 /nix/var/nix/ekacache/socket
gets=18234 hits=15022 puts=3480 ids=9120 slots=6 waiting=0 keys=41877 packs=37 bytes=8811204608 live=8402107392
```

On Linux, the peer check applies, so run this as `ecd`'s user or as root.

| Field | Meaning |
|---|---|
| `gets` | lookups since `ecd` started |
| `hits` | lookups that found their key |
| `puts` | store requests, including failed ones |
| `ids` | store-file identities remembered; this only grows |
| `slots` | slots handed out right now |
| `waiting` | slot requests queued right now |
| `keys` | entries in the index |
| `packs` | pack files in all tiers |
| `bytes` | bytes of pack files on disk |
| `live` | bytes of values still referenced |

`hits / gets` is a rough hit rate, but every compile makes more than one lookup (manifest,
then results), so it is not the same as the fraction of compiles that hit. The counters
reset when `ecd` restarts.

`waiting` above zero with `slots` at the limit means the machine is busy and compiles are
queueing, which is what slots are for. If it persists while nothing is compiling, see
[Troubleshooting](troubleshooting.md#builds-hang).

## Per-compile log

Set `EKACACHE_LOG` to a file and `ec` appends one line per C, C++, Rust or Go invocation:

```text
<tool>	<outcome>	<subject>	<milliseconds>
```

For example:

```text
cc	cached	src/foo.c	1
cc	compiled	src/bar.c inputs-changed:include/bar.h	312
rustc	compiled	src/lib.rs new-key	2051
cc	uncacheable	conftest.c	40
```

`<tool>` is `cc`, `rustc` or `go`. The outcomes:

| Outcome | Meaning |
|---|---|
| `cached` | hit, result replayed |
| `cached-error` | hit on a stored failure, failure replayed |
| `compiled` | miss, compiled and stored |
| `compiled-error` | miss, the compile failed, failure stored |
| `failed` | miss, the compile failed, failure not stored |
| `compiled-unstored` | miss, compiled, but the result could not be stored |
| `linked` | a link that ran |
| `uncacheable` | the invocation cannot be cached; the compiler ran plainly |
| `uncacheable-nosrc` | a source or input could not be read |
| `query` | a query such as `--version` |
| `no-daemon` | the socket could not be reached |

On a miss, the subject also gives the reason:

| Reason | Meaning |
|---|---|
| `new-key` | nothing stored for this request yet |
| `inputs-changed:<path>` | a file the last compile read has changed |
| `appeared:<path>` | a file the last compile looked for and did not find now exists |
| `object-gone` | the manifest was found but its results were evicted |
| `env-changed:<NAME>` | (Rust) an environment variable the crate reads has changed |
| `not-cached:unreadable:<path>` | an input could not be read, so nothing was stored |
| `incbin` | an assembler source uses `.incbin` |

Inside a Nix sandbox, point `EKACACHE_LOG` at a path the build can write, such as
`$NIX_BUILD_TOP/ekacache.log`, and copy it out in a later phase if you want to keep it.

## Command lines of uncached compiles

`EKACACHE_LOG_ARGS` (C/C++ only) appends the outcome and full argument list of every
invocation that did not hit, excluding queries. It is the quickest way to see which flag
makes something uncacheable.

## Failed stores

When `ecd` refuses or drops a store request, `ec` prints

```text
ec: cache put <key> failed
```

on stderr. The compile still succeeds, but nothing is stored. Look in `ecd`'s log for the
reason, usually a full disk.

## The daemon log

`ecd` logs to stderr, one line per event, as `YYYY/MM/DD HH:MM:SS <message>`, in local
time. There is no verbosity setting. It logs startup, store maintenance and errors. It does
**not** log connections, refused peers or slot activity.

| Line | Meaning |
|---|---|
| `tier <i> <dir> budget <bytes>` / `budget unbounded` | a store tier, at startup |
| `listening on <socket>, <n> slots, keys=… packs=… bytes=… live=…` | ready |
| `demoted pack 000012 to <dir> (X MiB, Y MiB live)` | a pack moved to the cold tier |
| `evicted pack 000012 (X MiB, Y MiB live)` | a pack deleted to stay within budget |
| `removed empty pack 000012` | housekeeping |
| `pack 000012: truncating torn tail at N (was M)` | recovery after a crash |
| `put <key>: <error>` | a write failed, usually a full disk |

## A quick end-to-end check

From a checkout, `scripts/smoke.sh` starts a private `ecd`, compiles a file twice and checks
that the second run is a hit with an identical object:

```console
$ nix-shell --run scripts/smoke.sh
…
ok: miss, then hit with the same object
```

Pass a compiler to test another one: `scripts/smoke.sh "$(command -v clang)"`.
