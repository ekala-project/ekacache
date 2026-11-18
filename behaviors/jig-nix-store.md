# jig nix-store mode

`jig nix-store` lets a builder running inside a Nix sandbox with the `builder-rpc-v0`
feature add content-addressed store objects (plain text and ATerm-encoded derivations,
including batches of `builtin:fetchurl` derivations) and assign one of its own outputs to an
existing store object. It does this by speaking a small client-side subset of the Nix daemon
worker protocol over the Unix socket named in `$NIX_REMOTE`, so no `nix` binary is needed in
the sandbox. This document specifies that behavior byte for byte, so that a Rust
reimplementation can replace the C++ one without anyone noticing. With
`JIG_NIX_STORE_OFFLINE` set, the mode computes the same store paths locally and contacts no
daemon.

Sources: `jig/src/nix_store_mode.h`, `jig/src/nix_store_mode.cc`, `jig/src/main.cc` (dispatch),
`jig/src/jig_test.cc` (`TestNixStore`), `jig/src/base.cc` (`Env`, `HexEncode`), `jig/src/store.h`
and `meson.build` / `meson.options` (`JIG_STORE_DIR`).

Key words MUST, MUST NOT, SHOULD, SHOULD NOT and MAY are as in RFC 2119. Where this document
says "current behavior", that behavior is normative: the Rust implementation MUST reproduce it
unless a Quirk entry's recommendation is accepted by the maintainer.

---

## Overview

### What "builder-rpc-v0" denotes

`builder-rpc-v0` is the code's name for a Nix daemon feature that this repository does not
contain. The name appears in four places only: the header comment in `nix_store_mode.h`, the
section comment `worker protocol client (builder-rpc-v0 subset)`, the protocol-version
comment `builderRpcV0 = 1.38`, and the `NIX_REMOTE` error message. From what the client does,
the feature means:

- the builder is given a worker-protocol Unix stream socket through the environment variable
  `NIX_REMOTE=unix://<path>`;
- on that socket the builder speaks worker protocol **1.38** (`0x126`), including the 1.38
  feature-string exchange;
- the builder may perform a restricted set of operations: **AddToStore** (op 7, used here
  only with the `text:sha256` content-address method) and **SubmitOutput** (op 1000,
  advertised through the feature string `submit-output`). SubmitOutput assigns one output
  of the running build to an existing store object. Op code 1000 lies far outside the op
  range used by upstream Nix and appears to be specific to the builder-rpc daemon.

The stated purpose (header comment) is to let a build emit derivations it computed itself
(dynamic derivations) without a `nix` binary in the sandbox. The project calls only the
client side "builder-rpc-v0 subset". This document specifies that client side as
implemented. It does not specify daemon behavior.

### Verbs

| Verb | argv after `nix-store` | stdin | stdout on success | Exit |
|---|---|---|---|---|
| `add-text` | `add-text <name> [<ref>...]` | raw bytes (the object contents) | store path + `\n` | 0 |
| `add-drv` | `add-drv <name> [<ref>...]` | derivation JSON object | store path of the `.drv` + `\n` | 0 |
| `fetchurls` | `fetchurls` | JSON array of `{name,url,algo,hex,sri}` | JSON array `[{"drv":…,"out":…}]` + `\n` | 0 |
| `submit` | `submit <store-path> <output>` | not read | nothing | 0 |

### Modes of operation

- **Online** (default): one connection to the daemon per process. The handshake happens
  first. Then every AddToStore/SubmitOutput needed by the verb is sent on that same
  connection, one after another. The socket is closed only when the process exits.
- **Offline** (`JIG_NIX_STORE_OFFLINE` non-empty): no socket is opened. `NIX_REMOTE` is
  not read. Text paths are computed locally (§Store path computation). `submit` is a no-op
  that succeeds.

---

## Invocation

### Dispatch

The mode is reached as `jig nix-store <args...>`, or by running the binary under a name
whose basename is `nix-store` (for example a symlink `nix-store -> jig`) with
`<args...>` = `argv[1..]`. See `behaviors/jig-dispatch.md`. Launcher mode (`jig /path/...`)
can never select it. `jig nix-store --help` gets no special handling and is treated like
any other unknown verb (see the processing order below).

Before dispatching, `main` builds the jigd socket path and constructs the `Store` singleton.
That reads `JIG_SOCK`, `JIG_STORE_IDENTITY`, `out` and `JIG_STORE_ROOTS`, but none of it has
any observable effect in this mode (`behaviors/environment.md`).

### Environment variables read by this mode

| Variable | Read when | Semantics |
|---|---|---|
| `JIG_NIX_STORE_OFFLINE` | always, after the empty-args check | unset or empty → online; **any non-empty value** (including `0`, `false`) → offline |
| `NIX_REMOTE` | online only | MUST start with the exact, case-sensitive prefix `unix://`. The rest, verbatim, is the socket path (§Socket) |

Unset and empty are the same for both variables: the C++ `Env()` returns `""` for unset.

### Store directory

`STORE_DIR` is a build-time constant: `-DJIG_STORE_DIR="<dir>"` from the meson option
`store_dir`, default `/nix/store`. The Rust build MUST expose the same build-time knob with
the same default. It is used verbatim, with no trailing-slash normalization. It is used:

- for offline text paths (`add-text`, `add-drv`, `fetchurls` `.drv` paths), and
- for the fixed output path `out` in `fetchurls`, **in both online and offline mode**.

In online mode, the path printed by `add-text`/`add-drv` and the `drv` field of `fetchurls`
are the paths the daemon returned. They are never computed or checked locally.

### Processing order (normative)

1. If there are no arguments after the mode, print
   `usage: jig nix-store add-text|add-drv|fetchurls|submit …` to stderr and exit **2**.
   The `…` is U+2026, UTF-8 `E2 80 A6`. Nothing else happens: no environment is read and
   no socket is opened.
2. **Open the store**, *before* the verb or argument count is checked. Offline: always
   succeeds. Online: connect and do the full handshake (§Worker protocol). If that fails,
   exit **1** after any message the failing step prints.
3. Dispatch, checking these in this order:
   1. verb is `add-text` or `add-drv` **and** there is at least 1 argument after the verb →
      §add-text / add-drv;
   2. verb is `fetchurls` **and** there are exactly 0 arguments after the verb →
      §fetchurls;
   3. verb is `submit` **and** there are exactly 2 arguments after the verb → §submit;
   4. anything else: print `jig nix-store: bad arguments` to stderr and exit **2**. In online
      mode the handshake has already completed by this point. The connection is then
      closed without sending any op.

So in online mode, an invalid verb whose connection or handshake fails exits 1 with the
connection error, not 2 (Q-NIX-2).

Stdin is read only in step 3, after a successful open. The Rust implementation MUST NOT
read stdin before the handshake has completed. Stdin is read to EOF as raw bytes, with no
newline or encoding translation.

### add-text / add-drv

- `name` = first argument after the verb, verbatim (it MAY be empty).
  `refs` = the remaining arguments, verbatim and unvalidated (they MAY be empty, may repeat,
  and need not be store paths).
- `contents` = all of stdin.
- `add-drv` only: `contents` is replaced by `ATerm(contents)` (§Derivations). If that
  conversion signals failure, print `jig nix-store: derivation JSON did not parse` to
  stderr and exit **1**. In online mode no op is sent before this exit. Some malformed
  inputs abort the process instead (§JSON decoding).
- Add the text object:
  - offline: `path = TextPath(name, refs, contents)`;
  - online: `path` = the path returned by AddToStore(`name`, `"text:sha256"`, `refs`,
    `contents`). On any failure, exit **1**. The only stderr output is whatever the drain
    loop printed (§Errors and messages).
- Print `path` followed by `\n` to stdout and exit **0**.

`add-drv` never reads the JSON `"name"` key, and never derives references from
`inputDrvs`/`inputSrcs`. The caller must pass the references as arguments (Q-NIX-13).

### fetchurls

1. Parse stdin as JSON (§JSON decoding). If parsing fails, or the top-level value is not an
   array, print `jig nix-store fetchurls: expected a JSON array` to stderr and exit **1**.
   (The prefix here is `jig nix-store fetchurls:`, with no colon after `nix-store`.)
2. For each element, in array order:
   1. Read `name`, `algo`, `hex`, `sri`, `url` as strings (§JSON decoding; a missing key
      reads as `""`; any other type, or an element that is not an object, aborts the
      process).
   2. `out = FixedOutputPath(STORE_DIR, name, algo, hex)`.
   3. `aterm = FetchurlDerivation(element, out)` (§The fetchurl derivation).
   4. Add the text object with name `name + ".drv"`, **no references**, contents `aterm`.
      Offline this is `TextPath`. Online it is an AddToStore round trip on the shared
      connection. On failure, exit **1** at once. Nothing goes to stdout, and the `.drv`s
      of earlier elements stay in the store.
   5. Append `{"drv": <path from step 4>, "out": out}` to the result.
3. Print the result as compact JSON (§fetchurls output) followed by `\n` to stdout and exit
   **0**. An empty input array prints `[]`. Duplicate elements give duplicate entries.

### submit

- Offline: exit **0** without validating anything or printing anything (Q-NIX-14).
- Online: SubmitOutput(`<store-path>`, `<output>`), both verbatim. Exit **0** if the op
  completes (§SubmitOutput), otherwise **1**.

### Exit status summary

| Status | Cause |
|---|---|
| 0 | success |
| 1 | store open failure (any connect or handshake failure); `add-drv` conversion failure; `fetchurls` non-array; any failure of an AddToStore or SubmitOutput round trip |
| 2 | no arguments (usage); bad verb or argument count |
| killed by SIGABRT | JSON value of the wrong type (§JSON decoding) |
| killed by SIGPIPE | write to a socket the daemon has closed, when SIGPIPE has its default disposition (Q-NIX-7) |

---

## Store path computation

### SHA-256

Standard FIPS 180-4 SHA-256 over arbitrary byte strings, producing 32 raw bytes. `hex(x)` is
lowercase hexadecimal, two digits per byte, no separators.

### Nix32

Alphabet (32 characters, index 0 to 31; it has no `e`, `o`, `t`, `u`):

```
0123456789abcdfghijklmnpqrsvwxyz
```

For input bytes `B` of length `n` (n ≥ 1):

- output length `L = floor((8·n − 1) / 5) + 1` (n = 20 → 32 chars; n = 32 → 52 chars);
- for `i` in `0 .. L−1` (left to right): `b = (L − 1 − i) · 5`, `j = floor(b / 8)`,
  `v = B[j] | (B[j+1] << 8)` (a byte index ≥ n reads as 0),
  `char_i = alphabet[(v >> (b mod 8)) & 0x1f]`.

Put another way: read `B` as one **little-endian** unsigned integer (bit `k` is bit `k mod 8`
of byte `floor(k/8)`) and write it as exactly `L` base-32 digits, **most significant digit
first**. The last character therefore encodes bits 0–4 of `B[0]`. Nix32 is only ever applied
to 20-byte values (store-path hash parts) in this mode. Empty input is unreachable
(Q-NIX-15).

### XOR-fold to 20 bytes

`fold(d)`: start from 20 zero bytes `o`. For `i` in `0 .. len(d)−1`, `o[i mod 20] ^= d[i]`.
For a 32-byte digest, bytes 0–11 are XORed with bytes 20–31 and bytes 12–19 pass through
unchanged.

### MakeStorePath

```
MakeStorePath(store_dir, type, inner, name):
  fingerprint = type ":sha256:" hex(SHA256(inner)) ":" store_dir ":" name
  return store_dir "/" Nix32(fold(SHA256(fingerprint))) "-" name
```

(Juxtaposition means byte concatenation.) Nothing is validated: `name` may be empty or
contain any bytes. The result is always `store_dir` + `/` + 32 Nix32 characters + `-` +
`name`.

### Text paths

```
TextPath(name, refs, contents):
  type = "text" + concat(":" + r  for r in sort(refs))
  return MakeStorePath(STORE_DIR, type, contents, name)
```

- `sort` is ascending, bytewise, unsigned lexicographic order (C++ `std::string`
  `operator<`). **Duplicates are kept**: refs `[a, a]` give type `text:a:a` (Q-NIX-5).
- `inner` is the whole object contents, so offline mode hashes all of stdin (or the ATerm).

### Fixed-output paths (flat)

```
FixedOutputPath(store_dir, name, algo, hex):
  return MakeStorePath(store_dir, "output:out", "fixed:out:" algo ":" hex ":", name)
```

Note the trailing `:` in the inner fingerprint. `algo` and `hex` are used verbatim (no case
folding, no validation). The function is meant for flat ingestion (`sha256`, `sha512`, …).
The path is always derived through SHA-256, whatever `algo` says. Passing an `r:`-prefixed
algo produces a path that Nix itself would compute differently for `r:sha256` (Q-NIX-16).

---

## Derivations

### JSON decoding

All JSON parsing (the `add-drv` input, the `fetchurls` input, and the internal re-parse
inside `FetchurlDerivation`) has the semantics of nlohmann::json 3.12 `parse(text, nullptr,
allow_exceptions=false)`:

- strict RFC 8259 grammar; whitespace is space, TAB, LF, CR; **no comments**;
- one leading UTF-8 BOM (`EF BB BF`) is skipped; a partial BOM is a parse error;
- anything after the value other than whitespace is a parse error;
- invalid UTF-8 in strings and unpaired `\u` surrogates are parse errors; `\u0000` is
  allowed and gives a NUL byte;
- duplicate object keys: **the last one wins**;
- empty input is a parse error.

A parse error counts as "did not parse" (`add-drv`) or "not an array" (`fetchurls`).

Field access follows nlohmann `value(key, default)`, which the Rust implementation MUST
emulate exactly:

- key absent → the default;
- key present → the value MUST have the requested type, **including when it is `null`**.
  Otherwise the process calls `abort()`: SIGABRT, no message, nothing on stdout. A `null`
  value does not count as absent.
- `value()` on something that is not an object → `abort()`.

### `add-drv` input → ATerm

The input SHOULD be an object of the shape `{"name", "system", "builder", "args":[],
"env":{}, "inputDrvs":{path:[outputs]}, "inputSrcs":[], "outputs":{name:{"hashAlgo"?,
"hash"?, "path"?}}}` (a subset of `nix derivation show`). If parsing fails, or the top level
is not an object, the result is **failure** (`derivation JSON did not parse`, exit 1).
Otherwise the fields are decoded **in this order**. The order matters only for deciding
between failure and abort:

| # | Field | Absent | Decoding |
|---|---|---|---|
| 1 | `outputs` | `{}` | iterated as entries (see "Iteration" below); entry values are not type-checked yet |
| 2 | `inputDrvs` | `{}` | iterated as entries; each entry value MUST be an array of strings (else abort); the names are sorted, **not deduplicated** |
| 3 | `inputSrcs` | `[]` | MUST be an array of strings (else abort, including `null`); sorted, **not deduplicated** |
| 4 | `env` | `{}` | iterated as entries; a value that is not a string → **failure** (exit 1, "did not parse"), not abort |
| 5 | `args` | `[]` | MUST be an array of strings (else abort); order kept |
| 6 | each output spec, while rendering | — | MUST be an object (else abort); `path`, `hashAlgo`, `hash`: absent → `""`, otherwise MUST be a string (else abort) |
| 7 | `system`, `builder`, while rendering | `""` | MUST be a string (else abort) |

`name` and any other keys are ignored.

**Iteration** of `outputs`, `inputDrvs`, `env` (nlohmann `items()`) over whatever value is
present:

- object → its entries, key = member name;
- array → one entry per element, key = the decimal index (`"0"`, `"1"`, …);
- `null` → no entries;
- string, number or boolean → one entry with key `""` and the value itself.

So `"env": "x"` produces the env pair `("","x")`, and `"outputs": null` produces `[]`.

**Rendering.** The result is the following, with no whitespace anywhere:

```
Derive(<outputs>,<inputDrvs>,<inputSrcs>,<system>,<builder>,<args>,<env>)
```

- `<outputs>` = `[` + entries sorted by key, joined by `,` + `]`, each entry
  `(Q(name),Q(path),Q(hashAlgo),Q(hash))`.
- `<inputDrvs>` = `[` + entries sorted by path, each `(Q(path),[Q(o1),Q(o2),…])` with the
  output names sorted + `]`.
- `<inputSrcs>` = `[` + `Q(s)` for each source, sorted + `]`.
- `<system>`, `<builder>` = `Q(value)`.
- `<args>` = `[` + `Q(a)` for each arg, in input order + `]`.
- `<env>` = `[` + entries sorted by key, each `(Q(key),Q(value))` + `]`.
- An empty list is `[]`. Sorting is bytewise unsigned ascending everywhere. Keys within one
  iterated value are unique (JSON keys are unique after parsing; array indices are unique),
  so entry lists have no duplicate keys.

`Q(s)` = `"` + `s` with exactly these five bytes escaped + `"`:

| byte | emitted |
|---|---|
| `"` (0x22) | `\"` |
| `\` (0x5C) | `\\` |
| LF (0x0A) | `\n` |
| CR (0x0D) | `\r` |
| TAB (0x09) | `\t` |

Every other byte, including NUL, other control bytes and non-ASCII UTF-8, is copied
verbatim.

The ATerm text is never empty when it succeeds (it always starts with `Derive(`), so an
empty result unambiguously means failure.

### The fetchurl derivation

For one `fetchurls` element with string fields `name`, `url`, `algo`, `hex`, `sri` (each
`""` when absent) and `out = FixedOutputPath(STORE_DIR, name, algo, hex)`, the derivation is
the ATerm of this JSON object. The implementation builds it, serializes it, re-parses it,
and passes it through the add-drv encoder above. That round trip is lossless, so building
the ATerm directly is equivalent.

| JSON key | value |
|---|---|
| `name` | `name` (ignored by the encoder) |
| `system` | `"builtin"` |
| `builder` | `"builtin:fetchurl"` |
| `args` | `[]` |
| `outputs` | `{"out": {"path": out, "hashAlgo": algo, "hash": hex}}` |
| `inputDrvs` | `{}` |
| `inputSrcs` | `[]` |
| `env` | 13 entries, below |

`env` (shown in ATerm, i.e. sorted, order):

| key | value |
|---|---|
| `builder` | `builtin:fetchurl` |
| `executable` | `` (empty) |
| `impureEnvVars` | `http_proxy https_proxy ftp_proxy all_proxy no_proxy` |
| `name` | `name` |
| `out` | `out` |
| `outputHash` | `sri` |
| `outputHashAlgo` | `""` if `sri` starts with `algo + "-"`, else `algo` |
| `outputHashMode` | `flat` |
| `preferLocalBuild` | `1` |
| `system` | `builtin` |
| `unpack` | `` (empty) |
| `url` | `url` |
| `urls` | `url` (same value) |

So the ATerm is:

```
Derive([("out",Q(out),Q(algo),Q(hex))],[],[],"builtin","builtin:fetchurl",[],[("builder","builtin:fetchurl"),("executable",""),("impureEnvVars","http_proxy https_proxy ftp_proxy all_proxy no_proxy"),("name",Q(name)),("out",Q(out)),("outputHash",Q(sri)),("outputHashAlgo",Q(oha)),("outputHashMode","flat"),("preferLocalBuild","1"),("system","builtin"),("unpack",""),("url",Q(url)),("urls",Q(url))])
```

where `oha` is the `outputHashAlgo` value above. The `.drv` is added under the name
`name + ".drv"` with **no references**.

### fetchurls output

The result is printed as nlohmann `dump()` with no indent: an array of objects, each with
exactly the keys `drv` then `out` (sorted key order), with no spaces:

```
[{"drv":"<drv>","out":"<out>"},{"drv":"…","out":"…"}]
```

followed by `\n`. String escaping follows nlohmann `dump()` with `ensure_ascii=false`:
`"` → `\"`, `\` → `\\`, 0x08 → `\b`, 0x09 → `\t`, 0x0A → `\n`, 0x0C → `\f`, 0x0D → `\r`,
any other byte < 0x20 → `\u00xx` (4 lowercase hex digits). Everything else (including `/`,
DEL and non-ASCII UTF-8) is copied verbatim. Inputs come from validated JSON, so no output
string can be invalid UTF-8 offline. A daemon-returned path is printed with the same
escaping.

---

## Worker protocol

### Encoding

- **word**: unsigned 64-bit integer, little-endian, 8 bytes. Booleans are words 0 and 1.
- **string**: word `len`, then `len` bytes, then `(8 − len mod 8) mod 8` zero bytes of
  padding. On receipt the padding bytes are read and discarded, not checked.
- **string list**: word `count`, then `count` strings.
- **framed contents** (AddToStore only): word `len = size(contents)`, then the `len` raw
  bytes **without padding**, then word `0` (terminator). This is always exactly one chunk
  and one terminator, even when `contents` is empty (Q-NIX-6).
- **Size limit**: a received string whose `len` is greater than `64 MiB` (`67108864`) is a
  read failure. Only the length word has been consumed when that happens. `len ==
  67108864` is accepted. Sent strings and contents have no limit.
- **Reads** are exact: the client loops on `read(2)` until it has all the bytes it needs. A
  return of 0 (EOF) or −1 (any error, **including EINTR**, which is not retried) is a read
  failure.
- **Writes** are buffered and sent only at *flush points*: `write(2)` is called in a loop
  until the whole buffer is gone. A return ≤ 0 (including EINTR) is a write failure. The
  client uses plain `write`, not `send(..., MSG_NOSIGNAL)` (Q-NIX-7).

### Socket

Online only:

1. `remote = $NIX_REMOTE` (`""` if unset). If it does not start with `unix://`, print
   `jig nix-store: NIX_REMOTE=<remote> is not a unix:// socket (builder-rpc-v0 missing?)`
   (with `<remote>` verbatim, possibly empty) and fail.
2. `path` = `remote` with the 7-byte prefix removed, **verbatim**: no URL decoding, and no
   stripping of `?query` or `#fragment`. A relative path is resolved by the kernel against
   the cwd.
3. If `len(path) ≥ sizeof(sockaddr_un.sun_path)` (108 on Linux, 104 on Darwin), fail
   **silently** (no message).
4. `socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0)` and `connect` to `path`. (The C++ code
   copies `path` into a zeroed `sun_path` and passes `addrlen = sizeof(struct sockaddr_un)`.
   For any non-empty path that is equivalent to the usual pathname address. See Q-NIX-11 and
   Q-NIX-12 for the empty path.) If `socket` or `connect` fails, print
   `jig nix-store: cannot connect to <path>` and fail.

Any failure here ends the store open (exit 1).

### Handshake

Constants:

| Name | Value | Bytes on the wire (LE) |
|---|---|---|
| `WORKER_MAGIC_1` (client → daemon) | `0x6e697863` | `63 78 69 6e 00 00 00 00` |
| `WORKER_MAGIC_2` (daemon → client) | `0x6478696f` | `6f 69 78 64 00 00 00 00` |
| client protocol version | `0x126` = 294 = 1.38 (`major << 8 \| minor`) | `26 01 00 00 00 00 00 00` |

Sequence (the "flush" marks are normative ordering points):

1. **send** word `WORKER_MAGIC_1`, word `0x126`; **flush**. Both words go out *before*
   anything is read. If the flush fails, or the next read fails, or the word read is not
   `WORKER_MAGIC_2`, print `jig nix-store: bad daemon magic` and fail.
2. **receive** word `WORKER_MAGIC_2` (checked in step 1).
3. **receive** word `daemon_version`. If the read fails, print
   `jig nix-store: daemon protocol 0 too old` and fail. If `daemon_version < 0x126`
   (unsigned comparison; the code has it as `min(daemon_version, 0x126) < 0x126`), print
   `jig nix-store: daemon protocol <daemon_version> too old`, with the number in **plain
   decimal** (for example `293` for 1.37), and fail. Any value ≥ 294 is accepted (for
   example 1.39 or 2.0), and the client keeps behaving as 1.38.
4. **send** the feature list, word `4` followed by these strings, in this order:
   1. `realisation-with-path-not-hash`
   2. `disable-set-options`
   3. `add-to-store-scanning`
   4. `submit-output`

   then **flush**. Failure → fail silently.
5. **receive** the daemon's feature list (string list) and discard it. The client requires
   no daemon feature, not even `submit-output`. Failure → fail silently.
6. **send** word `0` (obsolete CPU affinity: "none") and word `0` (obsolete
   `reserveSpace`: false); **flush**. Failure → fail silently.
7. **receive** string (daemon version string, discarded) and word (trust status,
   discarded, any value). Failure → fail silently.
8. **drain** (§Stderr drain loop) until `STDERR_LAST`. Success → the connection is
   **Ready**. Failure → fail (with whatever message the drain printed).

The client never sends `SetOptions`, any op other than 7 and 1000, or any goodbye message.

### Stderr drain loop

Used after the handshake and after every op. Repeat:

- **receive** word `msg`. Read failure → return failure **silently**.
- dispatch on `msg`:

| Code | Name | Fields read after the code | Client action |
|---|---|---|---|
| `0x616c7473` | `STDERR_LAST` | none | return **success** |
| `0x63787470` | `STDERR_ERROR` | string `type` (normally `Error`), word `level`, string `name`, string `msg` | print `nix-daemon: <msg>` (or `nix-daemon: ?` if reading `msg` failed); return **failure**. Fields after `msg` (havePos, traces, …) are **not** read. Failures reading `type`/`level`/`name` are ignored and the next read is attempted anyway |
| `0x6f6c6d67` | `STDERR_NEXT` | string `text` | print `<text>` + `\n` to stderr verbatim (an empty line if the read failed); continue |
| `0x53545254` | `STDERR_START_ACTIVITY` | word `id`, word `level`, word `type`, string `text`, *fields*, word `parent` | nothing printed; continue |
| `0x53544f50` | `STDERR_STOP_ACTIVITY` | word `id` | nothing printed; continue |
| `0x52534c54` | `STDERR_RESULT` | word `id`, word `type`, *fields* | nothing printed; continue |
| any other (e.g. `STDERR_READ 0x64617461`, `STDERR_WRITE 0x64617416`) | — | none | print `jig nix-store: unexpected daemon message 0x<hex>` (lowercase hex, `0x` prefix, no zero padding, i.e. C++ `{:#x}`); return **failure** |

*fields*: word `count` (`0` if the read fails), then for each of `count` fields: word
`type` (`0` if the read fails); if `type == 1`, a string, otherwise (0 or anything else) a
word.

Read failures inside `STDERR_NEXT`, `START_ACTIVITY`, `STOP_ACTIVITY`, `RESULT` and *fields*
do **not** end the loop. The loop carries on reading, and the next code read normally fails
at EOF. One consequence is that a huge *fields* `count` at EOF makes the client loop
(Q-NIX-8). An oversized string (> 64 MiB) consumes only its length word, so the next read
starts in the middle of the payload (Q-NIX-8).

### AddToStore (op 7)

Request, buffered and then sent in a single **flush**:

| Field | Encoding | Value |
|---|---|---|
| op | word | `7` |
| name | string | the store object name (`<name>` argument, or `<name>.drv` in fetchurls) |
| content-address method | string | `text:sha256` (always) |
| references | string list | refs sorted bytewise ascending, **duplicates kept** |
| repair | word | `0` |
| contents | framed contents | one chunk `len`+raw bytes, then word `0` |

Then:

1. flush (failure → op failure, silent);
2. drain loop (failure → op failure);
3. receive the **ValidPathInfo** reply, in this order:

| # | Field | Encoding | Use |
|---|---|---|---|
| 1 | path | string | **returned** (printed by add-text/add-drv, `drv` in fetchurls) |
| 2 | deriver | string | discarded |
| 3 | narHash | string | discarded |
| 4 | references | string list | discarded |
| 5 | registrationTime | word | discarded |
| 6 | narSize | word | discarded |
| 7 | ultimate | word | discarded |
| 8 | sigs | string list | discarded |
| 9 | ca | string | discarded |

Any read failure → op failure, **silent**. The returned path is not checked in any way.

### SubmitOutput (op 1000)

Request, sent in a single **flush**:

| Field | Encoding | Value |
|---|---|---|
| op | word | `1000` (`0x3e8`) |
| derived-path tag | word | `0` (SingleDerivedPath::Opaque) |
| path | string | `<store-path>` argument, verbatim |
| output | string | `<output>` argument, verbatim |

Then flush, drain loop, and **receive** one word (value discarded). The op succeeds only if
all three succeed. Any failure → exit 1, with only the drain loop's messages, if any.

### Client state-transition table

Online mode. "fail" means: print the listed message, if any, to stderr and exit with the
listed status. States are named for cross-checking against `spec/builder_rpc.qnt`.

| State | Action (send / expect) | Next on success | On failure |
|---|---|---|---|
| `Start` | inspect argv | `Opening` if ≥ 1 arg | no args → `usage: jig nix-store add-text\|add-drv\|fetchurls\|submit …`, exit 2 |
| `Opening` | read `JIG_NIX_STORE_OFFLINE` | non-empty → `ReadyOffline`; empty → `ParseRemote` | — |
| `ParseRemote` | read `NIX_REMOTE`, require prefix `unix://` | `CheckPath` | `jig nix-store: NIX_REMOTE=<v> is not a unix:// socket (builder-rpc-v0 missing?)`, exit 1 |
| `CheckPath` | `len(path) < sizeof(sun_path)` | `Connecting` | silent, exit 1 |
| `Connecting` | `socket` + `connect` | `SendHello` | `jig nix-store: cannot connect to <path>`, exit 1 |
| `SendHello` | send `MAGIC_1`, `0x126`; flush | `AwaitMagic` | `jig nix-store: bad daemon magic`, exit 1 (or SIGPIPE) |
| `AwaitMagic` | expect word == `MAGIC_2` | `AwaitVersion` | `jig nix-store: bad daemon magic`, exit 1 |
| `AwaitVersion` | expect word `v`, require `v ≥ 0x126` | `SendFeatures` | EOF → `jig nix-store: daemon protocol 0 too old`; `v < 0x126` → `jig nix-store: daemon protocol <v> too old`; exit 1 |
| `SendFeatures` | send 4 feature strings; flush | `AwaitFeatures` | silent, exit 1 (or SIGPIPE) |
| `AwaitFeatures` | expect string list (discard) | `SendPost` | silent, exit 1 |
| `SendPost` | send `0`, `0`; flush | `AwaitInfo` | silent, exit 1 (or SIGPIPE) |
| `AwaitInfo` | expect string (version), word (trust) | `DrainStartup` | silent, exit 1 |
| `DrainStartup` | drain loop | `Ready` | drain-loop message (see below), exit 1 |
| `Ready` | dispatch verb/argc | add-text/add-drv → `ReadInput`; fetchurls → `ReadInput`; submit → `SendSubmit` | otherwise `jig nix-store: bad arguments`, exit 2 |
| `ReadInput` | read stdin to EOF; add-drv: ATerm; fetchurls: parse array | add-text/add-drv → `SendAdd`; fetchurls → `SendAdd` for the first element, or `Done` if the array is empty | add-drv → `jig nix-store: derivation JSON did not parse`, exit 1; fetchurls → `jig nix-store fetchurls: expected a JSON array`, exit 1; JSON type error → SIGABRT |
| `SendAdd` | send op 7 request; flush | `DrainAdd` | silent, exit 1 (or SIGPIPE) |
| `DrainAdd` | drain loop | `AwaitPathInfo` | drain-loop message, exit 1 |
| `AwaitPathInfo` | expect ValidPathInfo (9 fields) | add-text/add-drv → `Done`; fetchurls → `SendAdd` for the next element, or `Done` after the last | silent, exit 1 |
| `SendSubmit` | send op 1000 request; flush | `DrainSubmit` | silent, exit 1 (or SIGPIPE) |
| `DrainSubmit` | drain loop | `AwaitSubmitResult` | drain-loop message, exit 1 |
| `AwaitSubmitResult` | expect one word | `Done` | silent, exit 1 |
| `Done` | print the result to stdout (add-*: path; fetchurls: JSON; submit: nothing) | exit 0 | — |
| `ReadyOffline` | dispatch verb/argc exactly as `Ready`; text paths computed locally; submit is a no-op | `Done` | same messages as `Ready`/`ReadInput` |

Drain-loop sub-machine (`DrainStartup`, `DrainAdd`, `DrainSubmit`):

| State | Expect | Next | Failure |
|---|---|---|---|
| `D.Code` | word | `LAST` → caller's success state; `NEXT` → `D.Next`; `START_ACTIVITY` → `D.Start`; `STOP_ACTIVITY` → `D.Stop`; `RESULT` → `D.Result`; `ERROR` → `D.Error` | EOF/read error → silent failure; unknown code → `jig nix-store: unexpected daemon message 0x<hex>` |
| `D.Next` | string; print it + `\n` | `D.Code` | (read failure ignored; prints an empty line) |
| `D.Start` | word, word, word, string, fields, word | `D.Code` | (read failures ignored) |
| `D.Stop` | word | `D.Code` | (ignored) |
| `D.Result` | word, word, fields | `D.Code` | (ignored) |
| `D.Error` | string, word, string, string `msg` | failure after printing `nix-daemon: <msg>` / `nix-daemon: ?` | — |

---

## Errors and messages

Every message goes to **stderr** with a trailing `\n` (C++ `std::println(stderr, …)`).
Verbatim templates (`<…>` is a substitution):

| Message | When | Exit |
|---|---|---|
| `usage: jig nix-store add-text\|add-drv\|fetchurls\|submit …` (U+2026) | no arguments after `nix-store` | 2 |
| `jig nix-store: NIX_REMOTE=<value> is not a unix:// socket (builder-rpc-v0 missing?)` | online and `NIX_REMOTE` lacks the `unix://` prefix (`<value>` is empty when unset) | 1 |
| `jig nix-store: cannot connect to <path>` | `socket()` or `connect()` failed; `<path>` is `NIX_REMOTE` without `unix://` | 1 |
| `jig nix-store: bad daemon magic` | first flush failed, EOF, or the first word is not `0x6478696f` | 1 |
| `jig nix-store: daemon protocol <n> too old` | daemon version < 294 (`<n>` decimal), or `0` if the version word could not be read | 1 |
| `nix-daemon: <msg>` / `nix-daemon: ?` | `STDERR_ERROR` received in any drain | 1 |
| `<text>` | `STDERR_NEXT` relayed (not an error) | — |
| `jig nix-store: unexpected daemon message 0x<hex>` | unknown stderr code in any drain | 1 |
| `jig nix-store: derivation JSON did not parse` | `add-drv`: parse error, non-object top level, or non-string env value | 1 |
| `jig nix-store fetchurls: expected a JSON array` | `fetchurls`: parse error or non-array top level | 1 |
| `jig nix-store: bad arguments` | unknown verb or wrong argument count (after a successful open) | 2 |

These failures print nothing and exit 1: `sun_path` overflow; any failure after
`AwaitVersion` in the handshake that is not reported through a drain-loop message; EOF in a
drain loop; any write failure after the first flush (unless SIGPIPE kills the process
first); any failure reading the ValidPathInfo or the SubmitOutput result word. JSON type
errors print nothing and abort (SIGABRT).

---

## Test vectors

Provenance: **[test]** = asserted in `TestNixStore`; **[bin]** = produced by the built
binary (`/tmp/jigbuild/jig/jig`, `STORE_DIR=/nix/store`) with `JIG_NIX_STORE_OFFLINE=1`;
**[derived]** = computed with an independent reimplementation of the algorithms above. Where
a [derived] value is a text path, it was cross-checked with [bin] by feeding the derived
ATerm to `add-text`.

### SHA-256 [test]

| Input | hex(SHA256) |
|---|---|
| `""` | `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855` |
| `"abc"` | `ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad` |
| 1000 × `a` | `41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3` |

### Nix32

- [test] `Nix32(SHA256("abc"))` = `1b8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s`
  (52 chars).
- [derived] `Nix32(01)` = `01`; `Nix32(1f)` = `0z`; `Nix32(20)` = `10`;
  `Nix32(00 01)` = `0080`; `Nix32(01 02 03 04 05)` = `0l2060h1`; `Nix32(20 × 00)` = 32 × `0`.

### Fixed output path [test]

`FixedOutputPath("/nix/store", "e", "sha256", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")`
= `/nix/store/3hc40wwijh4im7g2i31nk75qappbjw9i-e`

Intermediate values [derived]:

```
inner        = fixed:out:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855:
fingerprint  = output:out:sha256:25c9b18deb1a768db5c09793ae411acb62ba194c8f876ac206a070bbebef4342:/nix/store:e
SHA256(fp)   = 85a06ae4ab58fc6b51acbdc61a0994917340181cb4d1d30afee0600292245f5b
fold         = 3171b9ee55b89c69c388e29d1a0994917340181c
Nix32(fold)  = 3hc40wwijh4im7g2i31nk75qappbjw9i
```

### Text paths [bin]

| Command (offline) | stdin | stdout |
|---|---|---|
| `add-text hello` | (empty) | `/nix/store/56wfjivf3kjkp4zhj4xz7722hgg01bdn-hello` |
| `add-text hello.txt` | `hello` | `/nix/store/q790zdjk75hm2cn42nh77pqw4gbv1b88-hello.txt` |
| `add-text hello.txt /nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a` | `hello` | `/nix/store/ib8bp90l2c4w69hgv1c1s3xd4rifpd1f-hello.txt` |
| `add-text hello.txt /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a` | `hello` | `/nix/store/wyzya2mj8n7qzknk6pzcw6ls86nxhcir-hello.txt` |
| `add-text hello.txt /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a` | `hello` | `/nix/store/0f7wh5bl25mnb8di6vr02zm0pl7r4sf9-hello.txt` (duplicate kept, Q-NIX-5) |
| `add-text ''` | (empty) | `/nix/store/9ip48nkc9rfy0a4yaw98lp6gipqlib1a-` |

All exit 0. Fingerprints [derived]:

```
hello.txt, no refs:
  text:sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824:/nix/store:hello.txt
  SHA256(fp) = df8731676f5b0b48f2d420b351613953b60fd2c1d72a81b04c47d43b52c1e481
  fold       = 08adb0d7231cdf73a015c43251613953b60fd2c1
hello.txt, refs b then a (sorted to a, b):
  text:/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a:/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b:sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824:/nix/store:hello.txt
```

### ATerm [test]

Input JSON (one line):

```
{"name":"x","system":"x86_64-linux","builder":"/b","args":["-c","echo \"hi\"\n"],"env":{"out":"/o","b":"1","a":"2"},"inputSrcs":["/s2","/s1"],"inputDrvs":{"/d.drv":["out","dev"]},"outputs":{"out":{"hashAlgo":"r:sha256"}}}
```

ATerm (one line; `\"` and `\n` are the literal two-byte escapes):

```
Derive([("out","","r:sha256","")],[("/d.drv",["dev","out"])],["/s1","/s2"],"x86_64-linux","/b",["-c","echo \"hi\"\n"],[("a","2"),("b","1"),("out","/o")])
```

Store paths of that ATerm [derived from the ATerm, path checked with bin `add-text`]:

- `add-drv x.drv` (no refs) → `/nix/store/8brqylbyfhqj84mbvsqdh01391dvhcj0-x.drv`
  (inner hash `7e8a68a11394261e8f219ba46c2d358936a42f830da1f851d6f8a8adea1b065e`)
- `add-drv x.drv /s2 /s1 /d.drv` → `/nix/store/yvkb55mkif6rlrm6h2lgk6l1amh5xa5i-x.drv`
  (type `text:/d.drv:/s1:/s2`)

### fetchurls [derived; the .drv path was checked with bin add-text]

stdin:

```
[{"name":"foo-1.0.tar.gz","url":"https://example.org/foo-1.0.tar.gz","algo":"sha256","hex":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","sri":"sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="}]
```

`out` = `/nix/store/hq2j18s54yzc9ahyw79v8zdn5pya2ca9-foo-1.0.tar.gz`

ATerm of `foo-1.0.tar.gz.drv` (`outputHashAlgo` is empty because `sri` starts with `sha256-`):

```
Derive([("out","/nix/store/hq2j18s54yzc9ahyw79v8zdn5pya2ca9-foo-1.0.tar.gz","sha256","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")],[],[],"builtin","builtin:fetchurl",[],[("builder","builtin:fetchurl"),("executable",""),("impureEnvVars","http_proxy https_proxy ftp_proxy all_proxy no_proxy"),("name","foo-1.0.tar.gz"),("out","/nix/store/hq2j18s54yzc9ahyw79v8zdn5pya2ca9-foo-1.0.tar.gz"),("outputHash","sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="),("outputHashAlgo",""),("outputHashMode","flat"),("preferLocalBuild","1"),("system","builtin"),("unpack",""),("url","https://example.org/foo-1.0.tar.gz"),("urls","https://example.org/foo-1.0.tar.gz")])
```

stdout (offline):

```
[{"drv":"/nix/store/6ymfza6jbp5h9vha2mza1c5fkr9xgpwb-foo-1.0.tar.gz.drv","out":"/nix/store/hq2j18s54yzc9ahyw79v8zdn5pya2ca9-foo-1.0.tar.gz"}]
```

### CLI behavior [bin, offline]

| Invocation | stdin | stderr | stdout | Exit |
|---|---|---|---|---|
| `nix-store` | — | `usage: jig nix-store add-text\|add-drv\|fetchurls\|submit …` | — | 2 |
| `nix-store bogus` | — | `jig nix-store: bad arguments` | — | 2 |
| `nix-store add-text` | — | `jig nix-store: bad arguments` | — | 2 |
| `nix-store submit /nix/store/x` | — | `jig nix-store: bad arguments` | — | 2 |
| `nix-store fetchurls x` | — | `jig nix-store: bad arguments` | — | 2 |
| `nix-store submit /nix/store/x out` | — | — | — | 0 |
| `nix-store add-drv x.drv` | `abc` | `jig nix-store: derivation JSON did not parse` | — | 1 |
| `nix-store fetchurls` | `[1,2` | `jig nix-store fetchurls: expected a JSON array` | — | 1 |
| `nix-store fetchurls` | `{}` | `jig nix-store fetchurls: expected a JSON array` | — | 1 |
| `nix-store fetchurls` | `[] x` | `jig nix-store fetchurls: expected a JSON array` | — | 1 |
| `nix-store fetchurls` | `[]` | — | `[]` | 0 |
| `JIG_NIX_STORE_OFFLINE=0 … add-text hello.txt` | `hello` | — | `/nix/store/q790zdjk75hm2cn42nh77pqw4gbv1b88-hello.txt` (offline) | 0 |

### Wire examples [derived]

Client handshake, first flush (16 bytes):

```
63 78 69 6e 00 00 00 00   WORKER_MAGIC_1
26 01 00 00 00 00 00 00   0x126 (1.38)
```

Feature flush (136 bytes):

```
04 00 00 00 00 00 00 00                              count 4
1e 00 00 00 00 00 00 00  "realisation-with-path-not-hash" + 00 00
13 00 00 00 00 00 00 00  "disable-set-options" + 5 × 00
15 00 00 00 00 00 00 00  "add-to-store-scanning" + 3 × 00
0d 00 00 00 00 00 00 00  "submit-output" + 3 × 00
```

Post-handshake flush (16 bytes): `00` × 16.

AddToStore of `hello` as `hello.txt`, no refs (93 bytes):

```
07 00 00 00 00 00 00 00                              op 7
09 00 00 00 00 00 00 00  68 65 6c 6c 6f 2e 74 78 74 00 00 00 00 00 00 00   "hello.txt" + 7 pad
0b 00 00 00 00 00 00 00  74 65 78 74 3a 73 68 61 32 35 36 00 00 00 00 00   "text:sha256" + 5 pad
00 00 00 00 00 00 00 00                              0 references
00 00 00 00 00 00 00 00                              repair = 0
05 00 00 00 00 00 00 00  68 65 6c 6c 6f              chunk: len 5, "hello" (no padding)
00 00 00 00 00 00 00 00                              terminator
```

AddToStore with empty contents and name `e` (80 bytes; note the two trailing zero words,
Q-NIX-6):

```
07 00 00 00 00 00 00 00 | 01 00 00 00 00 00 00 00 65 00 00 00 00 00 00 00 |
0b 00 00 00 00 00 00 00 74 65 78 74 3a 73 68 61 32 35 36 00 00 00 00 00 |
00 00 00 00 00 00 00 00 (refs) | 00 00 00 00 00 00 00 00 (repair) |
00 00 00 00 00 00 00 00 (chunk len 0) | 00 00 00 00 00 00 00 00 (terminator)
```

SubmitOutput(`/nix/store/abc-x.drv`, `out`) (64 bytes):

```
e8 03 00 00 00 00 00 00                              op 1000
00 00 00 00 00 00 00 00                              Opaque
14 00 00 00 00 00 00 00  "/nix/store/abc-x.drv" + 4 pad
03 00 00 00 00 00 00 00  "out" + 5 pad
```

Daemon-side example the client accepts: `6f 69 78 64 00 00 00 00`, `26 01 00 …` (1.38),
`00 …` (0 features), a version string, `01 00 …` (trust), `73 74 6c 61 00 00 00 00`
(`STDERR_LAST`).

---

## Quirks

### Q-NIX-1: Dangling temporary in `DerivationToATerm` (C++ only)

**Current behavior**: The three range-for loops `for (… : drv.value(key, json::object()).items())`
iterate over an iteration proxy that refers to a temporary `json` returned by `value()`.
Without P2718 (lifetime extension of temporaries in range-for initializers, implemented in
GCC 15 / recent Clang), that temporary is destroyed before the loop body runs. Under the
GCC 14 build, `add-drv` (for any input that parses as a JSON object), `fetchurls` (with at
least one element) and `jig_test`'s `TestNixStore` crash with SIGSEGV. The normative behavior is the intended one (P2718
semantics), as specified in §Derivations.

**Impact**: none for Rust. A C++ build with a pre-P2718 compiler cannot produce derivations.

**Recommendation**: document only.

### Q-NIX-2: Store is opened before argument validation

**Current behavior**: Online, the full connect and handshake happen before the verb and
argument count are checked. A bad verb with a reachable daemon exits 2 after a full
handshake. With an unreachable daemon or a missing `NIX_REMOTE` it exits 1 with the
connection message, never `bad arguments`. Only the no-arguments case is checked first.

**Impact**: misleading diagnostics for typos inside a sandbox, and a wasted connection.

**Recommendation**: keep.

### Q-NIX-3: `JIG_NIX_STORE_OFFLINE` is a non-empty test

**Current behavior**: Any non-empty value, including `0`, `no` and `false`, selects offline
mode.

**Impact**: `JIG_NIX_STORE_OFFLINE=0` silently skips the daemon and prints locally computed
paths.

**Recommendation**: keep.

### Q-NIX-4: JSON type errors abort the process

**Current behavior**: Because nlohmann is built with `JSON_NOEXCEPTION`, a value of the wrong
type calls `abort()`: SIGABRT, no message. This covers a non-string `system`/`builder`, a
non-array `args`/`inputSrcs` (including `null`), a non-array or non-string-element
`inputDrvs` value, a non-object output spec, a non-string `path`/`hashAlgo`/`hash`, a
`fetchurls` element that is not an object, and a non-string field in one. Non-string `env`
values are the one exception: they give the clean `derivation JSON did not parse` / exit 1.
`null` counts as present, not as "use the default". Iterating a non-object
`outputs`/`inputDrvs`/`env` silently accepts arrays (index keys) and scalars (key `""`).

**Impact**: producers that feed bad JSON get a core dump instead of a message. In
`fetchurls` online, earlier elements are already in the store.

**Recommendation**: change in Rust — needs maintainer sign-off (map every type error to
`jig nix-store: derivation JSON did not parse` / `jig nix-store fetchurls: expected a JSON
array`-style messages with exit 1). Until signed off, the Rust implementation MUST abort
(SIGABRT, e.g. `std::process::abort()`) in exactly these cases.

### Q-NIX-5: Duplicates are sorted but not removed

**Current behavior**: `refs` (both in the offline text-path type and in the AddToStore
reference list), `inputSrcs` and each `inputDrvs` output-name list are sorted but keep
duplicates. Nix treats all of these as sets, so the daemon deduplicates references, and Nix
would canonicalize a derivation with duplicate sources differently.

**Impact**: with duplicate refs, offline paths differ from what the daemon returns online
(see the test vectors: `0f7wh5…` offline vs. the set form `wyzya2…`). ATerms with duplicates
are not canonical Nix derivations.

**Recommendation**: change in Rust — needs maintainer sign-off (deduplicate after sorting).

### Q-NIX-6: Empty contents produce an extra zero word

**Current behavior**: AddToStore always writes `len, bytes, 0`. For empty contents that is
`0, 0`. In the framed-stream format the first `0` already ends the stream, so the second
zero word is left in the daemon's input and is read as the next op code (0). For `add-text`
this is the last request, so the client normally gets its reply and exits 0, but the daemon
then sees an invalid op. (`add-drv` and `fetchurls` contents are never empty.)

**Impact**: a spurious daemon-side error after an empty `add-text`. It would break a
following op on the same connection if one were ever sent.

**Recommendation**: change in Rust — needs maintainer sign-off (send only the terminator
for empty contents).

### Q-NIX-7: SIGPIPE on a closed daemon socket

**Current behavior**: The socket is written with `write(2)` without `MSG_NOSIGNAL`, and the
mode does not touch the SIGPIPE disposition. If the daemon has closed the connection, the
process is killed by SIGPIPE (if the disposition inherited across exec is default) or gets
EPIPE, which is a silent write failure (`bad daemon magic` on the first flush), exit 1. The
cache client elsewhere in jig uses `MSG_NOSIGNAL`. Note that Rust's runtime sets SIGPIPE to
ignored at startup by default, so a naive Rust port gets the EPIPE path.

**Impact**: an abnormal termination with no message, and the outcome depends on the
inherited signal disposition.

**Recommendation**: change in Rust — needs maintainer sign-off (always treat EPIPE as a
write failure, e.g. `send(..., MSG_NOSIGNAL)`, with exit 1). Without sign-off, the Rust
implementation MUST reproduce death-by-SIGPIPE when the inherited disposition is default.

### Q-NIX-8: Read failures inside stderr messages do not stop the drain loop

**Current behavior**: Inside `STDERR_NEXT`, `START_ACTIVITY`, `STOP_ACTIVITY`, `RESULT`, the
*fields* skipper and the first three fields of `STDERR_ERROR`, read failures are ignored
and reading continues. A truncated `STDERR_NEXT` prints an empty line. A *fields* `count` of
up to 2^64−1 followed by EOF loops that many times on instant-EOF reads, which is
effectively a hang. A string longer than 64 MiB consumes only its length word, so the
stream desynchronizes and the next "code" is taken from the payload. Usually that ends in
`unexpected daemon message 0x…`.

**Impact**: only with a malicious or broken daemon, but then the client can hang or print
garbage diagnostics.

**Recommendation**: change in Rust — needs maintainer sign-off (stop at the first read
failure with a silent failure). Until then, reproduce the current behavior.

### Q-NIX-9: Silent failures

**Current behavior**: Exit 1 with no message for: socket path too long for `sun_path`;
failures after the version check during the handshake (features, post-handshake fields);
EOF in a drain loop; write failures after the first flush; a truncated ValidPathInfo or
SubmitOutput result.

**Impact**: hard to diagnose daemon incompatibilities.

**Recommendation**: document only. Adding messages changes stderr bytes, so it needs
sign-off.

### Q-NIX-10: Misattributed handshake diagnostics

**Current behavior**: `bad daemon magic` is printed when the first flush fails or the
daemon closes before sending 8 bytes, not only when the magic is wrong. An unreadable
version word is reported as `daemon protocol 0 too old`. Versions are printed as raw
decimal (`293`), not `1.37`.

**Impact**: cosmetic.

**Recommendation**: keep.

### Q-NIX-11: `NIX_REMOTE` path handling

**Current behavior**: Only the exact prefix `unix://` is accepted (not `daemon`, `auto`,
`unix:` without slashes, or `UNIX://`). The rest is used verbatim, including any `?query`.
With `NIX_REMOTE=unix://` (empty path), the C++ code connects to the Linux abstract address
made of 107 NUL bytes (because `addrlen` covers all of `sun_path`), which in practice fails
with `jig nix-store: cannot connect to ` (note the trailing space and empty path).

**Impact**: Nix-style store URIs with parameters do not work. The empty path ends in the
same error either way.

**Recommendation**: keep. For an empty path the Rust implementation MAY use any connect
call that fails, as long as it prints the same message.

### Q-NIX-12: `addrlen = sizeof(sockaddr_un)`

**Current behavior**: `connect` is passed the whole structure size rather than
`offsetof(sun_path) + strlen + 1`. For pathname sockets Linux and Darwin ignore the trailing
NULs, so this matches Rust's `UnixStream::connect`. The only difference is the empty path
(Q-NIX-11).

**Impact**: none in practice.

**Recommendation**: document only.

### Q-NIX-13: `add-drv` ignores `"name"` and does not compute references

**Current behavior**: The derivation's store name comes only from argv. The JSON `"name"`
key is never read. References for the `.drv` come only from argv, and are not derived from
`inputDrvs` keys or `inputSrcs` as Nix would derive them.

**Impact**: callers must pass the right references, or the daemon may reject the object, or
the offline path may differ from Nix's.

**Recommendation**: keep.

### Q-NIX-14: Offline `submit` always succeeds

**Current behavior**: With `JIG_NIX_STORE_OFFLINE` set, `submit <path> <output>` checks
nothing and exits 0.

**Impact**: dry runs cannot detect submit mistakes.

**Recommendation**: keep.

### Q-NIX-15: `Nix32` of empty input

**Current behavior**: `(0·8 − 1)` underflows `size_t`, so the C++ function would try to
build an enormous string (abort or hang). It is unreachable: only 20-byte folds (and
32-byte digests in tests) are ever encoded.

**Impact**: none.

**Recommendation**: document only. Rust MAY return `""` or panic.

### Q-NIX-16: No validation of names, refs, algo or hex

**Current behavior**: Store names, references, `algo` and `hex` are used verbatim. Offline,
anything is accepted (for example an empty name gives a path ending in `-`). `algo` is
placed in both the fixed-output fingerprint and the ATerm without checking. An `r:sha256`
algo yields a `fixed:out:r:sha256:` fingerprint, which upstream Nix would not use for a
recursive SHA-256 path (it uses the `source` type). Online, the daemon is the only check.

**Impact**: offline output can differ from what a daemon accepts. Garbage in, garbage path
out.

**Recommendation**: document only.

### Q-NIX-17: Partial `fetchurls` failure

**Current behavior**: The result is printed only after every element has succeeded. A
failure on element *k* exits 1 with nothing on stdout, while elements 0 to k−1 have already
been added to the store (online). An abort on element *k* (Q-NIX-4) behaves the same way.

**Impact**: the caller cannot tell which elements succeeded. Re-running is idempotent
because the paths are content-addressed.

**Recommendation**: keep.

---

## Open questions

1. **Daemon implementation of builder-rpc-v0.** This repository does not identify the Nix
   fork or version that implements the feature, op 1000, or the feature strings
   `realisation-with-path-not-hash`, `disable-set-options` and `add-to-store-scanning`. The
   meaning of the word that ends a SubmitOutput reply (read and ignored), and whether the
   daemon expects or tolerates the extra zero word of Q-NIX-6, cannot be settled from the
   code.
2. **Feature exchange order.** The client writes its features before reading the daemon's.
   That works whichever order the daemon uses, because the client's features sit in the
   socket buffer. The daemon's actual order is not stated.
3. **`add-to-store-scanning`.** The name suggests the daemon may scan the contents for
   references. If it does, the references the daemon records could differ from the ones the
   client sent, and the daemon-returned path could differ from the offline `TextPath`. The
   code does not say.
4. **Trust word.** Any value is accepted and ignored. Whether an untrusted connection
   (`2`) can still perform op 7/1000 depends on the daemon.
5. **Darwin.** `sizeof(sun_path)` is 104 there, so the silent-failure threshold differs.
   Whether builder-rpc-v0 exists on Darwin at all is unknown.
