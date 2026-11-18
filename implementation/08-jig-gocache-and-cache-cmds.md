# 08: `cache` subcommands, `slot` (phase P4) and gocacheprog (phase P7)

Two small modes on top of the foundation (05). Both come before or alongside cc mode, because
they exercise the client and the value framing with the fewest moving parts.

## Part 1 (P4): `jig cache …` and `jig slot …`

Specified by: `jig-cache-commands.md` (all), `jig-dispatch.md §cache subcommands, §slot
subcommand`.
Adopted here: Q-DIR-1 / Q-KEY-1 (path mode stores and restores text unmasked).

### Module (`crates/jig/src/dir.rs`, `slot.rs`)

- `run_cache(args, socket, store)`: exactly 3 args, else exit 1. Verbs `put`, `put-dir`,
  `get-dir`; **any other verb is `get`** (Q-DIR-6 kept). Connect failure → exit 2.
- `put`: read the file (unreadable → 1), `client.put`, exit 0. The PUT reply is now read
  (adopted Q-CLIENT-2), but the exit code stays 0 on a refused PUT, because the doc keeps
  "put exits 0 even if the daemon dropped the PUT" (Q-DIR-6 kept). **Flag:** Q-CLIENT-2 makes
  it possible to report the failure. It is a one-line change if wanted; ask before P4 ships.
- `get`: `client.get`, then an atomic temp file + rename write. Miss or write failure → 1.
- `put-dir`: a walk that reproduces `recursive_directory_iterator` order. That is
  **pre-order in `readdir` order, not sorted** (Q-DIR-2 kept), with symlinks to directories
  not followed. Use `std::fs::read_dir` recursively with an explicit stack, in pre-order
  (descend into a directory right after emitting it, as libstdc++ does), and test the order
  against the legacy binary on the same tmpfs. Batches of 256: pipelined `HAS`, then re-read
  and `put_many` of the missing ones (Q-DIR-3 double read kept). Count duplicates twice. PUT
  the listing last, print the summary line, exit 0.
- `Stored(bytes)`: **adopted Q-KEY-1**: content mode → `mask_hashes` if the file has no NUL;
  path mode → unchanged. Add `Store::stored()` to `jig-core` next to `resolve_all` so the
  mode logic lives in one place.
- `get-dir`: parse the listing (skip malformed lines; octal mode with a `0644` fallback),
  symlinks first, then files in batches of 256 with `get_many`, all 12 mode bits applied,
  the summary line always printed, rollback of recorded paths on any missing blob → exit 1.
  `Restored(blob)` = `store.resolve_all` for NUL-free blobs (the identity in path mode,
  adopted).
- **Kept aborts** (Q-DIR-4): a missing or unreadable `<dir>` during `put-dir` **aborts**
  (SIGABRT) after connecting and before the listing PUT, as legacy does. `abort()` at the
  same points, with `// Q-DIR-4`. The libstdc++ terminate text is not reproduced (same flag as
  Q-DRV-10 in 06).
- **Kept** Q-DIR-5 (no path sanitising in `get-dir`). The Rust code uses `dir.join(rel)`,
  which, like `root / rel`, discards `root` for an absolute `rel`. Test that explicitly so
  the join semantics stay identical.

`run_slot(args)`: no program → usage on stderr, exit 2. Otherwise `SlotGuard`, then
`process::run(program, args, Inherit)` (PATH search; silent 127 on exec failure, Q-DIR-7),
DONE on drop, exit with the status.

### Tests

- Every exit-code row of `jig-dispatch.md §cache subcommands` as a test against an in-process
  Rust jigd.
- Difftest: `put-dir` with legacy then `get-dir` with Rust (and the reverse), on a tree with
  binaries, text files containing store paths, symlinks, mode 000 and setuid files. Compare
  the restored trees with `diff -r` plus modes. Path-mode text blobs with store paths are
  allow-listed (adopted Q-DIR-1: different `b/` hash, unmasked content).
- Rollback: evict one blob (or delete it through a test hook), then `get-dir` must remove
  everything it wrote and exit 1.

## Part 2 (P7): gocacheprog

Specified by: `jig-gocacheprog.md` (all), `spec/gocache.qnt`.
Adopted here: §Adopted: JSON handling (Q-GO-1, Q-GO-3, Q-GO-4): a real JSON parser and
serializer, strict base64.

### Module (`crates/jig/src/gocache.rs`)

```rust
#[derive(Deserialize)] struct Req { #[serde(rename = "ID")] id: Option<i64>,
    #[serde(rename = "Command")] command: Option<String>, #[serde(rename = "ActionID")] action_id: Option<String>,
    #[serde(rename = "OutputID")] output_id: Option<String>, #[serde(rename = "BodySize")] body_size: Option<i64> }
#[derive(Serialize)] struct GetHit<'a> { #[serde(rename="ID")] id: i64, #[serde(rename="OutputID")] output_id: String,
    #[serde(rename="Size")] size: u64, #[serde(rename="DiskPath")] disk_path: &'a str }
// … GetMiss {ID, Miss:true}, PutOk {ID, DiskPath}, Err {ID, Err}, Close {ID}
```

- **Startup:** connect (silent failure → `live = false`); `dir` = `GOCACHEPROG_DIR` if set
  (even empty, Q-GO-6 kept), else `TMPDIR` if set (even empty) + `/gocacheprog`, else
  `/tmp/gocacheprog`; `create_dir_all` with errors ignored; then the exact handshake bytes
  `{"ID":0,"KnownCommands":["get","put","close"]}\n`.
- **Line reader:** split on `\n` only, keep `\r`, 1 MiB reads, and return a final unterminated
  line at EOF. Lines are parsed with `serde_json::from_slice::<Req>`. Requests do not use
  `deny_unknown_fields`, so unknown fields are ignored.
- **Bad lines (adopted):** if the line is invalid JSON or has a wrongly typed field, try to
  recover an integer `ID` (a second, lenient parse into `serde_json::Value` and read `ID`). If
  that works, reply `{"ID":n,"Err":"<message>"}`. Otherwise print
  `jig gocacheprog: bad request line` to stderr and **exit 1**. That is the one non-zero exit
  this mode has, and it is part of the adopted change.
- **Unknown or missing `Command`:** no reply (Q-GO-2 kept).
- **Base64:** `base64::engine::general_purpose::STANDARD` (strict padding, no whitespace
  tolerance). Invalid → `Err` reply, and nothing stored.
- **get / put:** as specified: the `go/a/` → `go/o/` lookup, the DiskPath write-if-absent on a
  hit, the body line read for `BodySize > 0` (a JSON string, now parsed as JSON), the size
  check (exact `body: got <n> of <m> bytes` text), the unconditional DiskPath overwrite, the
  **body PUT before the pointer PUT**, and `go\tcompiled\t…` logged even when not live (Q-GO-8
  kept).
- **DiskPath** is now JSON-escaped by the serializer (adopted Q-GO-3). For the paths Go's
  tooling produces (no `"` or `\` or control characters) the bytes are identical to legacy.
  serde_json does not escape `/` or non-ASCII, and neither did legacy. Non-UTF-8 `dir` bytes:
  serde can't serialise them as a `&str`. **Decision needed only if it happens:** legacy
  emits the raw bytes (invalid JSON); the plan is to fall back to writing the raw bytes
  exactly as legacy did, so behaviour doesn't change. Mark it `// Q-GO-3 residual`.
- **Exit status** 0 on EOF, read error and `close` (after replying `{"ID":n}`).

### Tests

- Every request/response pair in `jig-gocacheprog.md` as a transcript test (stdin → stdout
  bytes) with a fake jigd.
- A corpus of real `cmd/go` GOCACHEPROG traffic (record `go build std` once through a tee
  wrapper, check the recording in), replayed into legacy and Rust. Byte-identical stdout is
  required. This is the "identical for Go's output" promise of the adopted change.
- Adopted tests: escaped strings, whitespace after colons, duplicate keys, invalid base64,
  invalid JSON with and without a recoverable ID.
- Quint: `spec/gocache.qnt` traces (several go clients plus eviction) replayed against real
  gocacheprog processes sharing one Rust jigd. Check `hitIsConsistent`, `noTornBodies` and
  `pointersAnnounced` from the jigd contents after each step.
- End-to-end: `go build ./...` of a mid-size module twice under launcher mode. The second run
  shows only `go\tcached` lines, and the binaries are identical.
