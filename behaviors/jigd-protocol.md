# jigd socket protocol

jigd serves one line-oriented protocol on a Unix stream socket. jig (the compiler wrapper) uses
it to read and write cache objects (`GET`, `HAS`, `PUT`), to fetch memoised identities of
store files (`IDS`), and to take host-wide compiler slots (`SLOT`, `DONE`). Operators use
`STATS` to read counters. This document gives the exact bytes for each request and reply,
how requests are tokenized and parsed, the order replies come in, every case where the
daemon hangs up instead of answering, and the counters. It aims to let a Rust daemon replace
`jigd/main.go` without any client noticing. Storage semantics are in
[jigd-store.md](jigd-store.md), identities in [jigd-identities.md](jigd-identities.md), slots in
[jigd-slots.md](jigd-slots.md), and process setup and peer authentication in
[jigd-daemon.md](jigd-daemon.md).

Sources: `jigd/main.go` (`serve`, `readLine`, `get`, `put`, `ids`, the header comment),
`jigd/store.go:Store.Put` (key and size validation), `jigd/serve_test.go`
(`TestServeIdsAndPipelinedGets`, `TestServeHangsUpOnEndlessLine`), `jig/src/cache_client.{h,cc}`
(the only client). Formal model: [`spec/jigd_protocol.qnt`](../spec/jigd_protocol.qnt).

The key words MUST, MUST NOT, SHOULD, SHOULD NOT and MAY are used as in RFC 2119. Where this
document says "current behavior", that behavior is normative. The Rust implementation MUST
reproduce it unless the maintainer accepts the recommendation in the matching Quirk entry.

---

## Connection lifecycle

1. The daemon accepts a connection and starts one goroutine (`serve`) for it. It sets no
   limit on the number of connections and no read or write timeouts.
2. **Peer check.** `allowed(conn)` runs before anything is read (`main.go:96`). If it fails,
   the daemon closes the connection without sending a byte or writing a log line. The rules
   are in [jigd-daemon.md §Peer authentication](jigd-daemon.md#peer-authentication).
3. **Buffers.** Input goes through a 65 536-byte (`1<<16`) buffered reader (`main.go:99`).
   Output goes through a buffered writer of 4 096 bytes, Go's `bufio` default (`main.go:100`).
4. **Request loop.** Read one line, tokenize it, dispatch it, write the reply, flush. Repeat
   until the daemon hangs up or a read or write fails (`main.go:110-159`).
5. **Cleanup.** When `serve` returns, the daemon closes the socket. Then it calls
   `slots.Release(build)` once for every slot token this connection still holds
   (`main.go:102-109`; see [jigd-slots.md](jigd-slots.md)).

"Hang up" below means that `serve` returns: the connection is closed, step 5 runs, and no
further bytes are sent. Nothing is logged unless stated otherwise.

## Framing

### Request lines

A request line is a run of bytes ending in `\n` (0x0A). The `\n` is removed. Nothing else is
removed: a trailing `\r` stays in the line. `readLine` uses `bufio.Reader.ReadSlice('\n')`
over the 65 536-byte buffer (`main.go:73-79`). The rules are:

| Input | Result |
|---|---|
| `\n` within the first 65 536 buffered bytes of the line | the line is the bytes before `\n` (at most 65 535 bytes) |
| 65 536 bytes without a `\n` | `ErrBufferFull`: hang up (`TestServeHangsUpOnEndlessLine`) |
| EOF, with or without an unterminated partial line | hang up. A partial last line is discarded unexecuted |
| any read error | hang up |

The Rust implementation MUST accept a line of up to 65 535 content bytes plus `\n`. It MUST
hang up on the 65 536th byte that is not followed by a newline, without buffering more.

The same limit and rules apply to the path lines that follow `IDS` (§IDS). The bytes of a
`PUT` body are not lines: they are read as raw bytes (§PUT).

### Tokenization

The line is split with Go's `strings.Fields` (`main.go:115`). This splits on runs of
**Unicode** white space (`unicode.IsSpace`) and drops leading and trailing white space. A
Rust implementation MUST split on exactly the same set of characters:

- ASCII: `\t` `\n` `\v` `\f` `\r` and space (0x09 to 0x0D, 0x20);
- U+0085 (NEL) and U+00A0 (NBSP), when encoded as valid UTF-8 (`C2 85`, `C2 A0`);
- every other code point with the Unicode `White_Space` property: U+1680, U+2000 to U+200A,
  U+2028, U+2029, U+202F, U+205F, U+3000.

These are **not** separators: U+200B (zero-width space), U+FEFF, U+001C to U+001F, NUL,
any byte of invalid UTF-8 (for example a lone `0x85`), and any other byte. All of them stay
inside the token. This was checked against Go 1.25 (`strings.Fields("GET k")` gives
`["GET","k"]`, while `"GET\x85k"` and `"GET​k"` each give one field). `str::split_whitespace`
in Rust uses the same `White_Space` property, but it works on `&str` only. The Rust code MUST
tokenize raw bytes and treat invalid UTF-8 as non-space (Q-PROTO-3).

Verbs are compared byte-for-byte and are case-sensitive: `get k` is an unknown verb.

### Dispatch

The daemon dispatches on the verb (field 0) **and** the number of fields (`main.go:116-151`):

| Fields | Verb | Handler |
|---|---|---|
| 2 | `GET` | §GET |
| 2 | `HAS` | §HAS |
| 3 | `PUT` | §PUT |
| 2 | `IDS` | §IDS |
| 2 | `SLOT` | §SLOT |
| 2 | `DONE` | §DONE |
| 1 | `STATS` | §STATS |
| anything else | | hang up. This includes an empty or all-white-space line (0 fields), a known verb with the wrong number of fields (`GET`, `GET a b`, `STATS x`), and any unknown verb |

### Replies and flushing

Each handler writes its reply into the output buffer. After any handler that returns without
error, the daemon calls `Flush` (`main.go:153-155`). Every reply therefore leaves the daemon
before the next request is read. Replies are never batched across requests. The buffer can
also flush itself in the middle of a reply that exceeds 4 096 bytes; this matters for `IDS`
(Q-PROTO-5).

### Pipelining and ordering

Requests on one connection are handled strictly one after another, in the order received, and
the replies come back in that order. A client MAY send any number of requests before it reads
a reply. `jig/src/cache_client.cc` does this for `GetMany`, `HasMany`, `PutMany` and
`Identities`, and `TestServeIdsAndPipelinedGets` (`serve_test.go:49`) pins it: it pipelines
`PUT k1 3\nabc`, `IDS 2` plus two paths, `GET nope` and `GET k1`, and gets back `OK`, the
identity, an empty line, `MISS`, `OK 3` and `abc`.

While a connection is blocked in `SLOT`, the daemon reads nothing more from it (§SLOT).

There is no ordering between connections. The only cross-connection guarantee is the store's:
once a `PUT` has been answered `OK`, every `GET` or `HAS` that starts later, on any connection,
sees the value, until eviction removes it ([jigd-store.md](jigd-store.md)). The index is
updated under the store lock before `OK` is written (`store.go:317-321`).

## Number parsing

| Field | Parser | Accepts | Rejects (hang up) |
|---|---|---|---|
| `PUT` `<len>` | `strconv.ParseInt(s, 10, 64)` (`main.go:126`) | optional `+` or `-` sign, then one or more ASCII decimal digits; leading zeros; any value in int64 range | empty, `0x..`, `1_0`, inner spaces (impossible after tokenizing), non-ASCII digits, values outside int64 |
| `IDS` `<n>` | `strconv.Atoi(s)` (`main.go:132`), then range check `0 <= n <= 1048576` | same syntax as above, then the range check | same, plus `n < 0` or `n > 1<<20` |

Consequences, all current behavior (checked with Go 1.25):

- `PUT k +3` is the same as `PUT k 3`. `PUT k 003` is also accepted (Q-PROTO-4).
- `PUT k -1` parses. `Store.Put` then rejects it (size < 0): the daemon logs `put k: bad put`
  and hangs up, and `puts` is incremented (Q-PROTO-2).
- `IDS -0` and `IDS +0` are the same as `IDS 0`.
- `IDS 1048576` is accepted. `IDS 1048577` makes the daemon hang up.

## Commands

### HELLO (adopted change, Rust only)

Signed off 2026-11-17. This command is not in the Go daemon. A Rust jigd MUST implement it, and
a Rust jig MUST use it as described in `jig-cache-client.md` §Version negotiation.

```
request:  HELLO <maxver>\n
reply:    OK <ver>\n
```

- Version **0** is the protocol as specified in the rest of this document. It is in effect
  whenever no HELLO is sent, which keeps every existing (C++) client working unchanged.
- Version **1** is version 0 plus HELLO itself, and nothing else. Later protocol changes MUST
  get a new version number and MUST only take effect on a connection that negotiated it.
- `<maxver>` is the highest version the client speaks: decimal, `1..2147483647`, parsed with
  the same rules as other numbers (§Number parsing). Anything else is a hang-up.
- `<ver>` = min(`<maxver>`, the daemon's highest version), written in decimal with no sign or
  leading zeros. The connection uses `<ver>` from the next request on.
- HELLO is valid only as the **first** request on a connection. HELLO anywhere else is a
  hang-up (H11).
- HELLO does not touch counters, slots or the store. STATS is unchanged.
- The Go daemon treats HELLO as an unknown verb and hangs up (H4). That is the client's signal
  to fall back to version 0.


### GET

```
request:  GET <key>\n
hit:      OK <len>\n<len bytes of value>
miss:     MISS\n
```

- `<key>` is any token. It is **not** validated: any length up to the line limit and any
  bytes that are not separators. A key that `PUT` would refuse (longer than 4 096 bytes)
  simply misses.
- `<len>` is the value length in ASCII decimal with no sign and no leading zeros (`%d`).
  A zero-length value is sent as `OK 0\n` and nothing after it.
- Order of operations (`main.go:38-59`): increment `gets`. Look the key up with `store.Get`,
  which also marks the pack as recently used ([jigd-store.md §Recency](jigd-store.md#recency)).
  On a miss, write `MISS\n`. On a hit, increment `hits`, write and **flush** the header
  `OK <len>\n`, then copy the value straight from the pack file to the socket (`sendfile(2)`
  via `io.Copy(*net.UnixConn, *io.SectionReader)`).
- If the copy fails, or ends after fewer than `<len>` bytes because the pack file is shorter
  than the index claims, the daemon hangs up **after the header and part of the body were
  sent** (`main.go:54-57`). The client then reads EOF before `<len>` bytes and treats it as a
  miss. `hits` was already incremented (Q-PROTO-8).
- A value that eviction removes after the lookup is still sent in full: the open file
  descriptor keeps it readable (`TestGetSurvivesEviction`).

### HAS

```
request:  HAS <key>\n
reply:    1\n   if the key is in the index
          0\n   otherwise
```

- The key is not validated, as with `GET`.
- `HAS` calls `store.Get` and throws the reader away (`main.go:119-124`). It therefore has the
  same recency side effect as a `GET` hit: it sets the pack's `used` time and may set its
  mtime. It does **not** count in `gets` or `hits` (Q-PROTO-1).
- The main.go header comment and `cache_client.h` do not mention `HAS`, but
  `CacheClient::HasMany` uses it (`cache_client.cc:243-258`).

### PUT

```
request:  PUT <key> <len>\n<exactly len bytes>
reply:    OK\n
```

Processing order (`main.go:125-130`, `main.go:61-70`, `store.go:263-328`):

1. Parse `<len>` (§Number parsing). If parsing fails, hang up immediately. The body is not
   read and **`puts` is not incremented**.
2. Increment `puts`.
3. `Store.Put` validates (`store.go:264`). It rejects the request with the error `bad put` if
   any of these holds:
   - `len(key) == 0`: cannot happen through the protocol;
   - `len(key) > 4096` bytes;
   - the key contains `\n` or a space: cannot happen through the protocol, because tokenizing
     removes both (Q-PROTO-12);
   - `size < 0`;
   - `size > 2<<30` (2 147 483 648). Exactly 2 147 483 648 is **accepted**, which matches
     the client's `kMaxObjectSize`.
4. Read the body: exactly `size` raw bytes from the same buffered reader, so bytes already
   buffered after the request line count. If `size < 4 MiB` (4 194 304), the body goes to
   memory. Otherwise it goes to an unlinked temporary file `put-*.tmp` in the tier-0
   directory. Both happen before the store lock is taken.
5. Append the record under the store lock ([jigd-store.md §Put](jigd-store.md#put)).
6. On success, write `OK\n`.

Any error in steps 3 to 5 is logged as `put <key>: <error>` (`main.go:65`, format
`"put %s: %v"`), and then the daemon hangs up without replying. The body is left unread or
partly read, because the position in the stream is no longer known. The errors include:
`bad put`, a short body (the peer closed early: `unexpected EOF`), a temp-file failure,
`store closed` (shutdown in progress), and a write failure. A failed write truncates the
active pack back to its previous size (`store.go:309-316`), so a failed PUT stores nothing
(`TestPutSpooled`).

`<key>` is stored as raw bytes. It may hold any byte that is not a separator, including NUL,
`/` and invalid UTF-8.

### IDS

```
request:  IDS <n>\n<path_1>\n ... <path_n>\n
reply:    <id_1>\n ... <id_n>\n
```

- `<n>` is parsed and range-checked as in §Number parsing. A failure makes the daemon hang up
  before it reads any path.
- Each path line is read with `readLine` (same 65 535-byte limit) and is **not** tokenized.
  The whole line minus its `\n` is the path: spaces and a trailing `\r` are part of it.
- For each path, in order, the daemon writes `idents.Of(path) + "\n"`. That is either
  `C:` followed by 32 lowercase hex digits, or the empty string, giving the bare line `\n`
  ([jigd-identities.md](jigd-identities.md)).
- The daemon interleaves reading and answering: it answers path *i* before it reads path
  *i+1*. Answers sit in the 4 096-byte output buffer, which flushes itself whenever it fills.
- **`IDS 0`** is valid. The reply has **zero bytes**: nothing is written, and the daemon goes on
  to the next request (Q-PROTO-6; checked against the running daemon).
- If a path line fails (EOF, or longer than the limit), the daemon hangs up. Any answers that
  already overflowed the output buffer **have been sent**. A client can therefore receive a
  partial IDS reply followed by EOF. This was reproduced: `IDS 5000` followed by 4 500 lines
  of `/x` and then EOF returned exactly 4 096 bytes (4 096 empty answers) before the hang-up
  (Q-PROTO-5).

### SLOT

```
request:  SLOT <build>\n
reply:    OK\n     once a compiler slot has been granted
```

- `<build>` is any token. jig sends `$NIX_BUILD_TOP`, or `-` if it is unset
  (`jig/src/process.cc:37`).
- `slots.Acquire(build)` blocks the connection's goroutine until a token is granted
  ([jigd-slots.md](jigd-slots.md)). During that time, nothing more is read from the
  connection, so the daemon cannot notice that the peer has gone.
- After `Acquire` returns, the connection's `held[build]` goes up by one, and then the daemon
  writes `OK\n` (`main.go:137-140`). If the flush fails (the peer is gone), the daemon hangs up,
  and cleanup returns the token.
- `SLOT` never fails and never makes the daemon hang up.

### DONE

```
request:  DONE <build>\n
reply:    OK\n
```

If this connection holds at least one token for `<build>`, the daemon decrements its
`held[build]` and calls `slots.Release(build)`. In every case it replies `OK\n`, including
when the connection holds no such token (`main.go:141-146`; Q-SLOT-2 in
[jigd-slots.md](jigd-slots.md#q-slot-2-an-unheld-done-is-answered-ok)).

### STATS

```
request:  STATS\n
reply:    gets=<a> hits=<b> puts=<c> ids=<d> slots=<e> waiting=<f> keys=<g> packs=<h> bytes=<i> live=<j>\n
```

The format string is exactly
`"gets=%d hits=%d puts=%d ids=%d slots=%d waiting=%d %s\n"`, where `%s` is
`"keys=%d packs=%d bytes=%d live=%d"` (`main.go:149`, `store.go:465`). All numbers are
signed decimal, single spaces separate the fields, and there are no trailing spaces.

| Field | Meaning | Source |
|---|---|---|
| `gets` | `GET` requests since start (hit or miss). `HAS` is not counted | `main.go:39` |
| `hits` | `GET` requests that found the key, counted before the body is sent | `main.go:45` |
| `puts` | `PUT` requests whose `<len>` parsed, **whether or not the store accepted them** | `main.go:62` |
| `ids` | entries in the identity memo, which is never pruned | `identity.go:Len` |
| `slots` | slot tokens currently granted (`out`) | `slots.go:Stats` |
| `waiting` | `SLOT` requests currently queued | `slots.go:Stats` |
| `keys` | entries in the store index | `store.go:Stats` |
| `packs` | packs known to the daemon in all tiers, the active pack included | `store.go:Stats` |
| `bytes` | sum of the tier totals: pack file sizes as tracked, the active pack included | `store.go:Stats` |
| `live` | sum of `live` over all packs: value bytes the index still points at | `store.go:Stats` |

The counters are process-local, start at 0, and are never reset. The line is not an atomic
snapshot: the counters, the slot stats and the store stats are each read separately.

## Every hang-up

| # | Trigger | Bytes already sent for this request | Logged |
|---|---|---|---|
| H1 | peer fails `allowed()` | none (connection closed before reading) | no |
| H2 | request line longer than 65 535 bytes | none | no |
| H3 | EOF or a read error while reading a request line | none | no |
| H4 | wrong field count or unknown verb (including an empty line) | none | no |
| H5 | `PUT` `<len>` does not parse as int64 | none | no |
| H6 | `Store.Put` fails: bad key or size, short body, spool error, store closed, write error | none | `put <key>: <err>` |
| H7 | `IDS` `<n>` does not parse, or is outside 0..1048576 | none | no |
| H8 | `IDS` path line fails (EOF, too long, read error) | earlier answers, in 4 096-byte chunks | no |
| H9 | `GET` body copy fails or comes up short | header and part of the body | no |
| H10 | any reply write or flush fails | possibly part of the reply | no |
| H11 | (Rust only, adopted) `HELLO` that is not the first request, or with a bad `<maxver>` | none | no |

After a hang-up the connection's slot tokens are released (cleanup step 5). No case sends an
error message: the protocol has no error replies.

## Client view (informative)

`jig/src/cache_client.cc` treats every I/O problem as a miss or an ignored put. It reads
`OK <len>` and refuses lengths above `kMaxObjectSize = 2<<30`. In `GetMany`, `HasMany`,
`PutMany` and `Identities` it pipelines all requests and then reads all replies. Because the
daemon interleaves reading and writing, a pipelining client that stops reading can cause a
mutual block once both socket buffers are full. That is a property of the client, not of the
daemon.

## Wire examples (verified against the running daemon built from `jigd/`)

```
-> PUT k +3\nabc            <- OK\n
-> GET<U+00A0>k\r\n         <- OK 3\nabc
-> IDS 0\n                  <- (nothing)
-> HAS k\n                  <- 1\n
-> HAS z\n                  <- 0\n
-> DONE b\n                 <- OK\n
-> STATS\n                  <- gets=1 hits=1 puts=1 ids=0 slots=0 waiting=0 keys=1 packs=1 bytes=12 live=3\n
```

Then, each on a new connection: `PUT k -1\n` (hang-up, log `put k: bad put`), `PUT k 5\nab`
followed by EOF (hang-up, log `put k: unexpected EOF`), and `PUT k x\n` (hang-up, nothing
logged). After these, `STATS` reports `puts=3`: the request with `x` was not counted, the other
two were.

---

## Quirks

### Q-PROTO-1: `HAS` is undocumented and touches recency

**Current behavior**: `HAS <key>` exists and the client uses it, but the main.go header and
`cache_client.h` do not document it. It is implemented with `store.Get`, so it updates the
pack's `used` time and, at most once per minute, the pack file's mtime. A `HAS` probe
therefore protects a pack from eviction just as a read does. It is not counted in
`gets` or `hits`.

**Impact**: Clients that check for presence keep packs alive without reading them. STATS
under-reports traffic.

**Recommendation**: keep as-is (recency side effect included). Document `HAS` in the Rust
daemon's header.

### Q-PROTO-2: `puts` counts failed PUTs

**Current behavior**: `puts` goes up as soon as `<len>` parses (`main.go:62`), before
validation and before the body is read. Rejected keys or sizes, short bodies, spool errors,
`store closed` and write errors are all counted. A `<len>` that does not parse is not counted.

**Impact**: `puts` is "PUT attempts with a numeric length", not "values stored".

**Recommendation**: keep as-is. Adding a separate failure counter would change the STATS line
format and needs sign-off.

### Q-PROTO-3: Unicode white space splits tokens

**Current behavior**: `strings.Fields` splits on the Unicode white-space characters listed in
§Tokenization, so U+00A0, U+2028 and others separate tokens, and `\r` is dropped as trailing
white space. `GET k` is a valid GET of `k`, and `GET k\r\n` works. A key or build name
that contains such a character becomes two tokens, and the daemon hangs up (wrong field count).

**Impact**: Keys are hex digests in practice, so there is little risk. A `$NIX_BUILD_TOP` with
a space or NBSP in it makes every `SLOT` from that build fail (hang-up), and jig then runs
without a slot.

**Recommendation**: keep as-is. Use a byte-level splitter with exactly Go's set.

### Q-PROTO-4: Lenient number syntax (`+N`, leading zeros, `-0`)

**Current behavior**: `ParseInt` and `Atoi` accept a leading `+` and leading zeros. `PUT k +3`,
`PUT k 0003` and `IDS +2` are all valid. A negative PUT length parses and is then rejected by
the store (hang-up, logged, counted).

**Impact**: none for jig, which always sends canonical decimal.

**Recommendation**: keep as-is.

### Q-PROTO-5: A partial IDS reply can precede a hang-up

**Current behavior**: The answers stream through a 4 096-byte buffer. When a later path line
fails, the daemon hangs up, but every full 4 096-byte chunk of earlier answers has already
been sent. The model treats an IDS reply as all or nothing.

**Impact**: A client may read some identity lines and then EOF. jig's client discards the
whole batch on EOF (`cache_client.cc:192-196`), so this is harmless today.

**Recommendation**: keep as-is. A client MUST NOT assume that a hang-up means nothing was sent.

### Q-PROTO-6: `IDS 0` has an empty reply

**Current behavior**: `IDS 0` reads no path lines and writes nothing. The next reply on the
connection belongs to the next request.

**Impact**: A client that waits for "some reply" would block. jig never sends `IDS 0`
(`Identities` returns early for an empty list).

**Recommendation**: keep as-is.

### Q-PROTO-7: `\r` is ignored in command lines but kept in IDS paths

**Current behavior**: Command lines are tokenized, so a trailing `\r` disappears. IDS path
lines are used verbatim, so `/nix/store/x/h.h\r` is looked up with the `\r`, fails, and
returns an empty identity.

**Impact**: none for jig, which sends `\n` only.

**Recommendation**: keep as-is.

### Q-PROTO-8: A GET hang-up can follow a header

**Current behavior**: The `OK <len>\n` header is flushed before the body is copied. If the
pack file turns out to be shorter (truncated outside the daemon, or a lost write), the daemon
hangs up in the middle of the body. `hits` has already been incremented.

**Impact**: The client sees EOF in the middle of the value and falls back to compiling.
`hits` over-counts.

**Recommendation**: keep as-is.

### Q-PROTO-9: No timeouts and no connection limit

**Current behavior**: A connection may sit idle forever, a client that stops reading blocks
its goroutine forever (holding the pack file descriptor of an in-flight GET), and the number
of connections is unbounded. Each connection costs about 68 KiB of buffers.

**Impact**: Any user allowed to connect can exhaust memory or file descriptors (see
Q-DAEMON-5 for what happens when `accept` fails).

**Recommendation**: keep as-is. Adding limits needs sign-off.

### Q-PROTO-10: STATS is not a snapshot

**Current behavior**: The STATS fields are read at different instants and under different
locks.

**Impact**: Under load the fields can disagree with each other. For example, `keys` and
`live` may come from different moments than `slots`, or than counters that are still being
incremented. Even `hits > gets` can be reported: `gets` is read first, and a GET hit that
completes between the two reads raises both counters.

**Recommendation**: keep as-is.

### Q-PROTO-11: An unterminated last line is ignored

**Current behavior**: When EOF arrives after a partial line, the daemon drops that line
without executing it, even if it is a complete command such as `STATS`.

**Impact**: Clients MUST end every request with `\n`.

**Recommendation**: keep as-is.

### Q-PROTO-12: The store's key checks are partly unreachable

**Current behavior**: `Store.Put` rejects empty keys and keys that contain `' '` or `'\n'`.
Through the protocol, neither can occur. Keys with `\t` or NBSP cannot occur either, because
they would split the token. Keys with NUL, control bytes such as 0x1C, or invalid UTF-8 are
accepted. GET and HAS keys are not validated at all and may exceed 4 096 bytes.

**Impact**: none functionally. The Rust store MUST still apply the same checks, so that
`bad put` is logged in the same cases.

**Recommendation**: keep as-is.

---

## Disagreements with `spec/jigd_protocol.qnt`

Resolved 2026-11-17. The model now grants a SLOT immediately only when no connection is queued
(as in `slots.go:31`), documents replies as an atomic abstraction (partial replies, Q-PROTO-5 and
Q-PROTO-8, are not modelled), and cites `TestServeIdsAndPipelinedGets`. It still does not model
H1 (peer check) or H9/H10 (write failures), and it leaves waiter order to `spec/slots.qnt`.

## Open questions

1. Is the empty `IDS 0` reply intended, or an accident that nobody has hit?
2. Is `HAS` meant to refresh recency (the client calls it only for existence checks), or
   should it be a read-only probe?
3. Should the Rust daemon keep Go's exact `put <key>: <err>` texts (Go's `unexpected EOF`,
   `open …: no such file or directory`, and so on)? These are log lines, not wire bytes. This
   document treats the format string as normative and the Go error text as best effort.
