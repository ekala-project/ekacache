# ec gocacheprog mode

`ec gocacheprog` implements the Go toolchain's `GOCACHEPROG` protocol: JSON lines on
stdin/stdout. It maps the protocol's `get` and `put` onto ecd keys `go/a/<ActionID>` and
`go/o/<OutputID>`, and writes each body to a local file because Go wants a `DiskPath`. This
document specifies the handshake, every request and response byte for byte, the key/value
layout, the DiskPath files, and the exact (deliberately naive) JSON and base64 parsing, so
that a Rust implementation can be a drop-in replacement. The socket protocol and value
compression are in `behaviors/ec-cache-client.md`.

Sources: `jig/src/gocache_mode.h`, `jig/src/gocache_mode.cc`, `jig/src/main.cc` (dispatch),
`jig/src/launcher.cc` (setting `GOCACHEPROG`), `jig/src/keys.h` (`slot::GoAction`,
`slot::GoOutput`), `jig/src/base.cc` (`HexEncode`, `ParseUint`, `WriteFile`, `Env`,
`LogOutcome`), `jig/src/jig_test.cc` (`TestGoCache`), `spec/gocache.qnt`.

Key words MUST, MUST NOT, SHOULD and MAY are as in RFC 2119. Current behavior is normative.
Line numbers refer to `jig/src/gocache_mode.cc` unless another file is named.

---

## Invocation

- Reached as `ec gocacheprog`, or via a symlink whose basename is `gocacheprog`. Any further
  arguments are ignored (`main.cc:123-125`).
- Launcher mode sets it up: for `ec /path/to/go …`, if `$GOCACHEPROG` is empty or unset and
  the ecd socket path exists, ec sets `GOCACHEPROG="<self> gocacheprog"` and then `execv`s
  go (`launcher.cc:118-125`). `<self>` is `absolute(argv[0])` if argv[0] contains `/`, else
  `readlink(/proc/self/exe)` (`main.cc:107-111`).
- The exit status is always **0** (`:233`).

## Startup (`RunGoCacheProg`, `:203-234`)

1. `live = cache.Connect(socket_path)`. A failure is silent. The session then behaves as an
   always-miss cache that still writes DiskPath files.
2. `dir = Env("GOCACHEPROG_DIR", Env("TMPDIR", "/tmp") + "/gocacheprog")`. `Env` returns the
   fallback only when the variable is **unset**. A set-but-empty `GOCACHEPROG_DIR` gives
   `dir = ""` (Q-GO-6).
3. `create_directories(dir)`, ignoring errors.
4. Write the handshake and flush.

## Handshake

Exactly these bytes, followed by `\n`:

```
{"ID":0,"KnownCommands":["get","put","close"]}
```

It is sent unprompted, before any input is read.

## Request loop

`LineReader` (`:49-76`) splits stdin on `\n` only. A `\r` stays part of the line. It reads in
1 MiB chunks. At EOF a final unterminated non-empty line is still returned. On EOF or a read
error the loop ends and the process returns 0.

For each line:

1. An empty line is skipped.
2. `id = JsonInt(line, "ID")`, `command = JsonField(line, "Command")`.
3. If `command == "close"`: write `{"ID":<id>}\n`, flush, and **exit the loop** (return 0).
   Later input is not read.
4. `action = HexEncode(Base64Decode(JsonField(line, "ActionID")))`: lowercase hex, two
   characters per decoded byte.
5. `get` → HandleGet. `put` → HandlePut. **Any other command, including a missing
   `Command`, gets no reply** (Q-GO-2).
6. Flush stdout.

`<id>` in every response is `JsonInt` printed as a signed decimal `int64`.

### `get` (`HandleGet`, `:105-126`)

1. If `live`: `GET go/a/<action>` → `output_id` (raw bytes).
2. If found: `GET go/o/<HexEncode(output_id)>` → `body`.
3. If either is missing, or the session is not live, write:
   ```
   {"ID":<id>,"Miss":true}
   ```
   No EKACACHE_LOG line.
4. Otherwise log `go\tcached\t<action>\t<ms>` and set
   `disk_path = dir / HexEncode(output_id)`. If `disk_path` does not exist, `WriteFile` it
   with `body` (temp `<dir>/.ekacache<pid>.<hex>` + rename, ignoring errors). An existing file is
   **not** compared or rewritten. Write:
   ```
   {"ID":<id>,"OutputID":"<Base64Encode(output_id)>","Size":<body size>,"DiskPath":"<disk_path>"}
   ```
   `Size` is decimal. `DiskPath` is `disk_path` as a string, **not JSON-escaped** (Q-GO-3).
   There is no `Time` field.

### `put` (`HandlePut`, `:128-155`)

1. `output_id = Base64Decode(JsonField(line, "OutputID"))`. `ObjectID` is **not** read.
2. `body = ""`. Let `body_size = JsonInt(line, "BodySize")`. If `body_size > 0`:
   1. Read lines until a non-empty one or EOF. That line is the body line.
   2. If the body line has length ≥ 2 and starts and ends with `"`:
      `body = Base64Decode(line[1 : len-1])`. Otherwise `body` stays `""`.
   3. If `body.size() != body_size`, write
      ```
      {"ID":<id>,"Err":"body: got <body.size()> of <body_size> bytes"}
      ```
      and return. **Nothing is written to disk or to the cache**, and nothing is logged. This
      covers the torn stream a killed go command leaves (`spec/gocache.qnt` `noTornBodies`).
3. `disk_path = dir / HexEncode(output_id)`. `WriteFile(disk_path, body)` unconditionally,
   overwriting, with errors ignored.
4. If `live`, two sequential PUTs, each waiting for `OK`, **in this order**:
   1. `go/o/<HexEncode(output_id)>` = `body` (the decoded bytes)
   2. `go/a/<action>` = `output_id` (the **raw decoded bytes**, typically 32 bytes, not hex)
5. Log `go\tcompiled\t<action>\t<ms>`, even when not live.
6. Write:
   ```
   {"ID":<id>,"DiskPath":"<disk_path>"}
   ```

With `BodySize` absent, `0`, negative or unparseable, no body line is consumed and an empty
body is stored. If Go did send a body line in that case, the next loop iteration reads it as
a request: it has no `Command`, so it gets no reply.

### Key and value layout

| Key | Value | Written by |
|---|---|---|
| `go/a/<lowercase hex of ActionID bytes>` | raw OutputID bytes | put, second |
| `go/o/<lowercase hex of OutputID bytes>` | raw body bytes | put, first |

No masking is applied. ec validates neither the OutputID length nor the body hash. The order
"body before pointer" guarantees that a `go/a/` entry never names a body that was never sent.
With eviction, a `go/a/` hit whose `go/o/` is gone is a Miss (`gocache.qnt` `getFinish`).

### DiskPath files

| Property | Value |
|---|---|
| Directory | `$GOCACHEPROG_DIR`, else `$TMPDIR/gocacheprog`, else `/tmp/gocacheprog` |
| Name | lowercase hex of the OutputID bytes, no extension |
| Written | on every successful put (overwrite), and on a get hit when absent |
| Atomicity | temp file `.ekacache<pid>.<name>` in the same directory, then rename |
| Mode | `0666 & ~umask` |
| Cleanup | never |

The path is not made absolute. A relative `GOCACHEPROG_DIR` yields relative `DiskPath`s.

---

## JSON parsing (exact)

There is no JSON parser. `JsonField(line, key)` (`:80-93`):

1. Search the **whole line** for the first occurrence of the byte string `"<key>":`. There is
   no whitespace allowed between the closing quote and the colon. Not found → `""`.
2. If the byte right after the colon is `"`: return the bytes up to (not including) the next
   `"`. Backslash escapes are not interpreted. No closing quote → `""`.
3. Otherwise return the bytes up to (not including) the first `,` or `}`, or to the end of the
   line. Leading and trailing whitespace is kept.

`JsonInt(line, key)` = `ParseUint(JsonField(line, key))` (trims spaces and tabs, decimal
digits only, must consume everything, `uint64` range), cast to `int64`. On failure → 0.

| Input | Result |
|---|---|
| `{"ID":5,"Command":"get",...}` (Go's compact encoding) | `ID` 5, command `get` |
| `{"ID": 5}` | `ID` 5 (the space is trimmed by ParseUint) |
| `{"Command": "get"}` | command ` "get"`, which matches nothing → no reply |
| `{"ID" :5}` | `ID` 0 |
| `{"ID":"7"}` | `ID` 7 (a quoted number is accepted) |
| `{"ID":-3}`, `{"ID":1.0}`, `{"ID":1e3}` | `ID` 0 |
| `{"ID":18446744073709551615}` | `ID` printed as `-1` |
| duplicate keys | the **first** occurrence wins (Go's decoder takes the last) |
| `"Command"` nested in another value | matched if it is the first occurrence in the line |
| missing braces, trailing garbage, invalid JSON | accepted if the patterns are found |
| `"Command":"get"` | command `get` → no reply |
| a line ending in `\r` | values at the end of the line keep the `\r`; a quoted body line fails the `"…"` check → Err |

Key names are case-sensitive. `"ActionID":` does not match the `ID` search, because the
pattern starts with `"`.

## Base64

`Base64Encode` (`:159-180`): standard RFC 4648 alphabet `A–Z a–z 0–9 + /`, padded with `=` to
a multiple of 4. Vectors (`TestGoCache`): `abc`→`YWJj`, `ab`→`YWI=`, `a`→`YQ==`, `""`→`""`.

`Base64Decode` (`:182-201`): for each input byte, if it is not in the alphabet, **skip it**.
This covers `=`, whitespace, `-`, `_`, `\` and every other byte (Q-GO-4). Otherwise shift its 6
bits into an accumulator and emit a byte each time 8 or more bits are pending. Leftover bits
at the end are discarded. It never fails. Examples: `YWI=` → `ab`; `Y W I` → `ab`; `YW-I` →
`ab`; `YWJ` → `ab` (the trailing 2 bits are dropped).

---

## Concurrency model

`spec/gocache.qnt` models several go commands sharing ecd with eviction. It checks
`hitIsConsistent`, `noTornBodies` and `pointersAnnounced`. The PUT order above, and refusing
torn bodies before anything is written, are the properties it relies on.

---

## Adopted: JSON handling (Rust port)

Signed off 2026-11-17. This replaces the substring scanner (Q-GO-1, Q-GO-3, Q-GO-4).

- Each stdin line MUST be parsed as one JSON object. Field names are matched exactly and
  case-sensitively: `ID`, `Command`, `ActionID`, `OutputID`, `BodySize`. Unknown fields are
  ignored. Whitespace and escapes follow RFC 8259.
- `ActionID` and `OutputID` are JSON strings holding standard padded base64 (Go's `[]byte`
  encoding). The body line that follows a `put` with `BodySize > 0` is a JSON string of
  standard padded base64. Decoding MUST be strict.
- A line that is not valid JSON, has no integer `ID`, or has a field of the wrong type: if an
  integer `ID` can be recovered, reply `{"ID":<n>,"Err":"<message>"}`. Otherwise write
  `ec gocacheprog: bad request line` to stderr and exit 1, since the stream can no longer be
  trusted.
- Invalid base64 in `ActionID`/`OutputID`/body: reply with `Err` and store nothing. A put body
  that fails to decode MUST NOT be stored (the same rule as the BodySize mismatch).
- Unknown `Command` values keep the current behaviour (Q-GO-2) unless signed off separately.
- Responses MUST be emitted with a JSON serializer: compact, fields in the order this document
  lists for each response, strings escaped. For every response the C++ code produces today the
  bytes are identical.

## Quirks

### Q-GO-1: Naive JSON parsing

**Current behavior**: as in §JSON parsing. Fields are found by substring search. Escapes are
not decoded, whitespace after a colon breaks string values, the first duplicate wins, and
invalid JSON is accepted.

**Impact**: none with Go's `encoding/json` output, which is compact and never escapes base64
or the command names. A different client could be misread.

**Recommendation**: **ADOPTED (signed off 2026-11-17).** The Rust port MUST parse each request line with a real JSON
parser (see §Adopted: JSON handling), not reproduce this scanner. For every line Go's
`cmd/go` emits, the result is the same.

### Q-GO-2: Unknown commands get no reply

**Current behavior**: a request whose `Command` is not `get`, `put` or `close`, or has no
`Command` at all, is silently dropped (`:226-230`).

**Impact**: a Go version that sends a new command after the handshake (it should not, given
`KnownCommands`) waits forever for its response.

**Recommendation**: keep as-is. Optionally reply `{"ID":n,"Err":"unknown command"}`, which
needs sign-off.

### Q-GO-3: DiskPath is not JSON-escaped

**Current behavior**: `disk_path.string()` is inserted between quotes verbatim (`:124`,
`:154`).

**Impact**: a `GOCACHEPROG_DIR` or `TMPDIR` containing `"`, `\` or control characters produces
invalid JSON or a wrong path, and Go fails.

**Recommendation**: **ADOPTED (signed off 2026-11-17).** Responses are produced by a JSON serializer, so DiskPath (and
Err texts) are escaped properly. This gives identical bytes for every path in use today.

### Q-GO-4: Base64 decoding skips invalid characters

**Current behavior**: bytes outside the alphabet are skipped, padding is ignored, and trailing
bits are dropped. It never errors.

**Impact**: a corrupted ActionID or OutputID silently maps to a different key. A corrupted
body is caught only by the BodySize check.

**Recommendation**: **ADOPTED (signed off 2026-11-17).** Standard base64 (RFC 4648 §4, padded, as Go emits) MUST be
decoded strictly. Invalid input is an error (see §Adopted: JSON handling).

### Q-GO-5: DiskPath failures are invisible

**Current behavior**: `create_directories` and `WriteFile` errors are ignored. `put` still
replies with the DiskPath, and a `get` hit replies with a DiskPath that may not exist.
`fs::exists(disk_path)` in HandleGet is the throwing overload. With `-fno-exceptions`, a
filesystem error there (for example `EACCES` on the directory) terminates the process with
SIGABRT.

**Impact**: Go reports an error reading the file, or loses its cache program.

**Recommendation**: keep as-is. The Rust port SHOULD NOT abort in that case. Treating it as
"exists = false" matches the non-error path.

### Q-GO-6: A set-but-empty `GOCACHEPROG_DIR` or `TMPDIR`

**Current behavior**: `Env` returns `""` for a set-but-empty variable. `GOCACHEPROG_DIR=""`
makes DiskPath the bare hex name, relative to the cwd. `TMPDIR=""` gives `/gocacheprog`.

**Impact**: files land in surprising places.

**Recommendation**: keep as-is.

### Q-GO-7: `ObjectID` (pre-Go-1.24 name) is ignored

**Current behavior**: only `OutputID` is read. A client that sends only `ObjectID` gets an
empty OutputID. The body is then stored under `go/o/` (empty hex), `go/a/<action>` = `""`, and
DiskPath = the directory itself (the write fails silently).

**Impact**: that version's cache is broken. Go ≥ 1.24 sends `OutputID`.

**Recommendation**: keep as-is. Document the minimum Go version.

### Q-GO-8: Not-live puts are still logged as stored

**Current behavior**: without a daemon, every put logs `compiled` (`kMissStored`).

**Impact**: EKACACHE_LOG summaries overcount stores.

**Recommendation**: keep as-is.

---

## Open questions

1. Does Go require `DiskPath` to be absolute? If it does, a relative `GOCACHEPROG_DIR` breaks
   the build.
2. What minimum Go version is supported (Q-GO-7)?
3. Should the DiskPath directory ever be cleaned? Today it grows without bound within
   `$TMPDIR`, which in a Nix sandbox is discarded with the build.
