# ec cache client

This document specifies how ec talks to the ecd daemon: how the socket is found, what happens
when it cannot be reached, the exact request bytes for each command, how replies are parsed, the
value encoding (u32 length header plus a zstd frame), pipelining, identity (`IDS`) batching, and
the job-slot protocol (`SLOT`/`DONE`) as used by the `Slot` guard in `process.cc`, including
`EKACACHE_SLOT` propagation to child processes. The daemon side is described only as far as the
client depends on it.

Sources: `jig/src/cache_client.h`, `jig/src/cache_client.cc`, `jig/src/process.h`,
`jig/src/process.cc` (`Slot`), `jig/src/main.cc` (socket path), `jig/src/store.h` (`StateDir`),
`jig/src/manifest.cc` (`PrefetchIdentities`), `jig/src/cc_mode.cc`, `jig/src/rustc_mode.cc`,
`jig/src/dir_mode.cc`, `jig/src/gocache_mode.cc` (callers), `jigd/main.go`, `jigd/slots.go`,
`jigd/peer_linux.go`, `jigd/identity.go`, `jigd/store.go` (limits), `spec/slots.qnt`.

Key words MUST, MUST NOT, SHOULD, SHOULD NOT and MAY are as in RFC 2119. Where this document
says "current behavior", that behavior is normative: the Rust implementation MUST reproduce it
unless a Quirk entry's recommendation is accepted by the maintainer.

See also [`ec-keys.md`](ec-keys.md) (what the keys are), [`environment.md`](environment.md),
[`ec-dispatch.md`](ec-dispatch.md).

---

## Overview

- Transport: one `AF_UNIX` `SOCK_STREAM` connection per `CacheClient` object, opened with
  `SOCK_CLOEXEC`. Each ec process uses at most one client per mode invocation (`dir_mode` and
  `cache` subcommands also use one). It is closed when the process exits.
- Protocol: line-based ASCII commands ending in `\n`; binary bodies are length-prefixed.
- The cache is an optimisation: every I/O failure turns into a miss, an ignored put, an empty
  identity list or "no slot". No client operation prints anything or changes the exit status,
  except where a caller (the `ec cache` subcommands) maps a failed connect to exit 2.
- No timeouts: every read and write blocks indefinitely (Q-CLIENT-4).

---

## Socket discovery

`main` computes the path once (`main.cc:96`):

```
socket_path = $EKACACHE_SOCK          if EKACACHE_SOCK is set (even to "")
            = StateDir() + "/ekacache/socket"   otherwise
StateDir()  = EKACACHE_STORE_DIR up to its last '/', + "/var/nix"
```

With the default `EKACACHE_STORE_DIR=/nix/store` the default is `/nix/var/nix/ekacache/socket`. Nix's
`sandbox-paths` maps the host socket there (`jigd/main.go` usage text). An empty `EKACACHE_SOCK` is
used as the empty path and never connects (`environment.md`, Q-ENV-1).

### Connect

`Connect(path)` (`cache_client.cc:27-44`):

1. If `len(path) >= sizeof(sun_path)` (108 on Linux), return false without a syscall.
2. `socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0)`. Failure → false.
3. Copy the path bytes into a zeroed `sockaddr_un` and `connect` with
   `addrlen = sizeof(sockaddr_un)`. Failure → false.
4. Keep the fd.

There is no retry and no message. What the callers do on false:

| Caller | On connect failure |
|---|---|
| cc mode (`cc_mode.cc:691`) | compile uncached, outcome `no-daemon` if otherwise cacheable |
| rustc mode (`rustc_mode.cc:293`) | same, outcome `no-daemon` |
| `Slot` | no slot; the program runs anyway |
| gocacheprog | serves without the shared cache (`session.live = false`) |
| `ec cache get/put/get-dir/put-dir` | exit 2, no message |

ecd checks the peer's credentials after `accept` (`peer_linux.go:15-30`): the daemon's own uid,
root, members of the `nixbld` group, and uids `872415232 … 872415232 + 8388607`. Anyone else is
accepted and then hung up on without a reply. The client sees a successful `connect` and then
EOF on the first read, which behaves as above for every operation (miss, no slot, and so on).

---

### Version negotiation (adopted change, Rust only)

Signed off 2026-11-17. The wire format is in `ecd-protocol.md` §HELLO.

1. After `connect` succeeds, the Rust client MUST send `HELLO 1\n` as its first request. It
   MUST NOT pipeline anything behind it until it has read the reply.
2. `OK <v>\n` with `v >= 1`: the connection speaks version `min(v, 1)`.
3. EOF or a read error before any reply means an older (Go) ecd that hung up on the unknown
   verb. The client MUST close the socket, reconnect once to the same path, and speak version
   0 on the new connection without sending HELLO. If the reconnect fails, apply the normal
   connect-failure rule of the caller (table above).
4. Any other reply means the connection is unusable. Treat it like a failed connect.
5. A client process MAY remember a version-0 result for the rest of its lifetime and skip
   HELLO on later connections. It MUST NOT persist that result anywhere else.

Version 1 changes nothing else on the wire. The step exists so later protocol changes can be
negotiated rather than breaking mixed C++/Rust fleets. It is not modelled in
`spec/ecd_protocol.qnt`.

## Framing primitives

| Primitive | Behavior |
|---|---|
| `SendAll(bytes)` | `send(fd, …, MSG_NOSIGNAL)` until all is sent. A return ≤ 0 → false. `MSG_NOSIGNAL`: a closed daemon gives `EPIPE`, not `SIGPIPE`. `EINTR` is not retried (Q-CLIENT-6) |
| `Fill()` | one `read(fd, buf, 65536)`. ≤ 0 → false |
| `RecvLine()` | bytes up to the next `\n`, the `\n` removed. EOF/error before `\n` → *none* (bytes already read are lost). No length limit |
| `RecvExactly(n)` | exactly `n` bytes from the buffer and the socket. Short → *none* |

Reads share one 64 KiB buffer, so pipelined replies are consumed in order.

---

## Value encoding

Every value sent with `PUT` and received with `GET` is wrapped (`cache_client.cc:105-139`):

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | raw (uncompressed) length as `uint32`, **host byte order** (little-endian on x86-64 and aarch64), truncated modulo 2³² |
| 4 | rest | one zstd frame from `ZSTD_compress(level = 1)` with libzstd defaults (content size in the frame header, no checksum, no dictionary) |

The daemon stores and returns these bytes opaquely. The `<len>` on the wire is the length of
this whole wrapped value.

Encoding: if `ZSTD_compress` reports an error, the result is empty and the caller sends
nothing (`Put`) or stops (`PutMany`).

Decoding (`Decompress`): fewer than 4 bytes → *none*. Otherwise read the header as host-order
`u32 size`, allocate `size` bytes, and call `ZSTD_decompress(dst, size, wire[4..])` (which also
accepts several concatenated frames). Success only if the result equals `size` exactly.
Anything else → *none*.

A Rust implementation MUST write the header in the same byte order as the C++ client on the
same host (in practice little-endian) and MUST emit a valid zstd frame. Byte-identical
compressed output is NOT required: no key is derived from compressed bytes, and the C++
decoder accepts any valid frame. A Rust decoder MUST accept multiple concatenated frames.

Test vector (libzstd 1.5.7, x86-64): the value `hello` is sent as the 18 bytes

```
05 00 00 00  28 b5 2f fd 20 05 29 00 00 68 65 6c 6c 6f
```

(header `5`, zstd magic, frame header `0x20` single-segment with a 1-byte content size `5`,
one raw last block of 5 bytes, `hello`).

---

## Commands

All keys and arguments are inserted verbatim. No escaping exists, so a key containing a space
or `\n` corrupts the stream (ecd rejects such a key and hangs up). ecd parses each command
line with `strings.Fields` and hangs up on anything it does not recognise.

| Method | Request bytes | Expected reply | Client parsing |
|---|---|---|---|
| `Get(k)` | `GET k\n` | `OK <len>\n` + `<len>` bytes, or `MISS\n` | see `RecvValue` |
| `GetMany(ks)` | `GET k1\nGET k2\n…` in one `SendAll` | one reply per key, in order | `RecvValue` per key |
| `HasMany(ks)` | `HAS k1\nHAS k2\n…` in one `SendAll` | `1\n` or `0\n` per key | true iff the line is exactly `1`; *none* → false |
| `Put(k, v)` | `PUT k <wirelen>\n` then the wrapped value (two `SendAll`s) | `OK\n` | one line read and discarded (Q-CLIENT-2) |
| `PutMany(items)` | for each item `PUT k <wirelen>\n` + value, then all replies | `OK\n` each | reads as many lines as items fully sent, all discarded |
| `Identities(ps)` | `IDS <n>\n` then `p\n` for each path, one `SendAll` | `n` lines | see §IDS |
| `AcquireSlot(b)` | `SLOT b\n` | `OK\n` (once a slot is free) | true iff the line is exactly `OK` |
| `ReleaseSlot(b)` | `DONE b\n` | `OK\n` | one line read and discarded |

`<len>`, `<wirelen>` and `<n>` are unsigned decimal without leading zeros (`std::format("{}")`).
ec never sends `STATS`.

Every method first checks that the client is connected. If not, it returns at once with the
empty result (`none`, all-`none` vector, all-false vector, empty vector, false) and sends
nothing. `GetMany`, `HasMany`, `Identities` and `PutMany` also send nothing for an empty list.

### RecvValue (`cache_client.cc:143-155`)

1. `RecvLine`. *none*, or a line that does not start with exactly `OK ` → *none*. This covers
   `MISS`. The connection stays open.
2. `ParseUint(line[3..])`, base 10, after trimming spaces and tabs at both ends, the whole rest
   must be digits. If that fails, or the length exceeds `kMaxObjectSize` = 2³¹ (2 GiB), the
   client **closes the connection** (later operations behave as unconnected) and returns *none*.
3. `RecvExactly(len)`. *none* → *none* (the connection is not closed, Q-CLIENT-5).
4. `Decompress`.

### IDS (`cache_client.cc:179-200`)

Sent only through `PrefetchIdentities` (`manifest.cc:94-108`), only in content mode, and only
for store paths outside `$out` (`Store::DaemonMayIdentify`, see `ec-keys.md`). Each call is
**one** `IDS` request for the whole list, with no chunking. Calls happen:

| Caller | Paths |
|---|---|
| `BuildManifest` | every manifest input candidate after the compile |
| `ValidateManifest` | every resolved input path of the stored manifest |
| cc `ToolchainIds` | the compiler (and `--ld-path`) plus LLVM/clang/lld libraries |
| rustc `StdlibIds` | the libstd/libcore rlibs |

The reply is read as exactly `n` lines. Line `i` belongs to path `i`. An empty line means "the
daemon does not answer for it", and the path is hashed locally. Any read failure discards all
answers (empty result), and everything is hashed locally. ecd hangs up for `n < 0` or
`n > 2²⁰`, and answers `""` for a path that is not `filepath.Clean`, not under its store after
symlink resolution, or not a regular file (`identity.go:52-89`).

---

## Job slots

### Protocol

`SLOT <build>\n` blocks on the daemon side until the host-wide limit (`EKACACHED_SLOTS`, default
CPU count) admits one more compiler for `<build>`. The reply is then `OK\n`. `DONE <build>\n`
returns one token held by this connection for `<build>` (no-op if none) and replies `OK\n`.
Tokens still held when the connection closes are returned by the daemon (`main.go:101-109`).
Among waiters the build holding the fewest tokens is served first (`slots.go`). The slot
request travels on the same connection as cache traffic, so nothing else can happen on it while
it waits.

### Slot guard (`process.cc:29-49`)

```
Slot(cache, socket_path):
  if $EKACACHE_SLOT is non-empty: do nothing
  if not cache.connected() and not cache.Connect(socket_path): do nothing
  build = $NIX_BUILD_TOP if set (even if empty), else "-"
  if cache.AcquireSlot(build):
      remember (cache, build); setenv("EKACACHE_SLOT", "1", overwrite)
~Slot():
  if acquired: cache.ReleaseSlot(build); unsetenv("EKACACHE_SLOT")
```

A failed `AcquireSlot` (no daemon, a hang-up, any reply other than `OK`) means "run without a
slot" and leaves `EKACACHE_SLOT` alone.

`EKACACHE_SLOT=1` is in ec's own environment while the guard is alive, so every child (the
compiler, and anything it spawns, for example rustc invoking `cc`, which is ec again, to link)
inherits it and skips the slot request. Without this a nested ec would wait for a slot its
parent holds (`spec/slots.qnt`, `slotNesting`, `HONOR_EKACACHE_SLOT`). An inherited `EKACACHE_SLOT` is
never modified or removed by an ec that did not set it.

Where a slot is held:

| Site | Condition |
|---|---|
| cc `RunUncached` (`cc_mode.cc:527-530`) | the invocation names a source or at least one object input (not for `-v`, `--version`, `-print-*` …) |
| cc `RunObserved` (`cc_mode.cc:344`) | every cache miss, around the real compiler run. `socket_path` is passed as `""`, which is harmless because the client is already connected |
| rustc `RunUncached` | the invocation names a `.rs` source |
| rustc miss (`rustc_mode.cc:333-336`) | always, around the real rustc run |
| `ec slot <program> …` | always |

No slot is taken for a cache hit, for launcher-mode `go` or other programs (exec'd directly),
for gocacheprog, or for `ec cache`.

---

## Quirks

### Q-CLIENT-1: The value header is host-endian

**Current behavior**: the 4-byte raw-length header is a `bit_cast` of a `uint32_t`, so it is
written and read in host byte order (`cache_client.cc:105`, comment "the cache is per machine").
ecd treats values as opaque.

**Impact**: a store shared between hosts of different endianness (or copied between them)
decodes as garbage. That turns into misses, because the length check fails, not into wrong
data. All supported hosts are little-endian.

**Recommendation**: **ADOPTED (signed off 2026-11-17).** The header is defined as **little-endian u32**. The Rust port MUST
write and read it with `u32::to_le_bytes` / `from_le_bytes`. That is byte-identical to the C++
client on every supported (little-endian) host, so existing caches stay valid.

### Q-CLIENT-2: The PUT reply is unchecked

**Current behavior**: `Put` and `PutMany` read one line per sent `PUT` and discard it. A daemon
that rejects the put (key longer than 4096, value over 2 GiB, write error) hangs up instead of
replying. The client does not notice, and later operations on the connection fail silently.
`ec cache put` exits 0 in that case.

**Impact**: lost entries are invisible. `ec cache put` reports success for a failed store.

**Recommendation**: **ADOPTED (signed off 2026-11-17).** Every PUT's reply MUST be read and checked for exactly `OK`. On EOF or
anything else the client MUST treat the connection as dead (no further requests on it) and log
`ec: cache put <key> failed` in the mode's log format. Build-path modes still exit with the
compiler's status (a failed store is not a build failure). `ec cache put` MUST exit 1.

### Q-CLIENT-3: A malformed slot name kills the connection

**Current behavior**: the build name is `$NIX_BUILD_TOP` verbatim. If it is set but empty
(`SLOT \n`) or contains whitespace, ecd's `strings.Fields` gives the wrong field count and it
hangs up. The client then gets no slot, and because the slot shares the cache connection, every
following `PUT` of that compile is lost silently.

**Impact**: the compile still works, but uncapped and uncached, with no message.

**Recommendation**: keep as-is (sandboxed builds use `/build`). Optionally substitute `-` for an
empty value (needs sign-off).

### Q-CLIENT-4: No timeouts

**Current behavior**: no `SO_RCVTIMEO`/`SO_SNDTIMEO`, no `poll`. A wedged daemon blocks every
ec forever. `SLOT` blocks by design until a token is free.

**Impact**: a stuck daemon hangs builds instead of degrading to uncached compiles.

**Recommendation**: keep as-is for the drop-in port. Add a timeout on non-`SLOT` replies only
with sign-off.

### Q-CLIENT-5: Stream desync after a short body

**Current behavior**: if `RecvExactly` fails partway, or a reply line is neither `OK …` nor
`MISS` (for example a stray `OK\n`), the client does not close the connection. The next request
reads leftover bytes as its reply.

**Impact**: only with a misbehaving daemon. A wrong value cannot result, because the value must
decompress to the exact header length, but later lookups may miss.

**Recommendation**: keep as-is. A Rust port MAY close the connection on any framing error,
since that only turns would-be misses into misses.

### Q-CLIENT-6: `EINTR` is a hard failure

**Current behavior**: `send`/`read` returning −1 with `EINTR` is treated like any error (miss,
no slot). ec installs no signal handlers, so this happens only under a debugger or
job-control signals.

**Impact**: negligible.

**Recommendation**: keep as-is. A Rust port MAY retry on `EINTR`.

### Q-CLIENT-7: The claimed raw size is trusted before decompression

**Current behavior**: `Decompress` allocates the header's `size` bytes (up to 4 GiB) before
checking anything. `kMaxObjectSize` bounds only the compressed length.

**Impact**: a corrupt value can cause a large allocation (and an abort on allocation failure,
since ec is built without exceptions).

**Recommendation**: keep as-is. A Rust port SHOULD cap `size`, for example at
`kMaxObjectSize`, and treat a larger value as a miss.

### Q-CLIENT-8: Values over 4 GiB, and puts over 2 GiB

**Current behavior**: the raw length is truncated to 32 bits. ecd rejects any `PUT` body over
2 GiB by hanging up (`store.go:264`).

**Impact**: huge objects are never cached. This is silent, see Q-CLIENT-2.

**Recommendation**: keep as-is.

---

## Open questions

1. Is a big-endian host ever expected? If not, the Rust port can declare the header
   little-endian outright (Q-CLIENT-1).
2. Should `IDS` be chunked to stay under ecd's 2²⁰ limit? A manifest that large is not
   realistic today. Beyond it the daemon hangs up and the rest of the compile runs uncached.
3. `HAS` is used only by `put-dir`. It is not in the ecd header comment's command list, but
   ecd implements it. The protocol doc for ecd should list it.
