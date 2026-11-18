# 02: `jig-core` (phase P0)

The shared library. Everything that both binaries must agree on byte for byte lives here, so
the agreement is enforced by the compiler and not by convention.

Specified by: `jig-keys.md` (§The Hasher, §Key types and prefixes, §Store paths, §Key: lexical
normalisation, §Manifest), `jig-cache-client.md` (§Value encoding, §Version negotiation),
`jigd-protocol.md` (§Framing, §Tokenization, §Number parsing, §HELLO).

## Modules

### `bytes`: byte-string helpers

Free functions on `&[u8]`, chosen so we don't need `bstr`: `find`, `rfind`, `starts_with`,
`replace_all` (non-overlapping, left to right), `split_once`, ASCII-decimal `itoa` into a
stack buffer, and `os_bytes(&OsStr) -> &[u8]` (Unix `OsStrExt`). Each one has a unit test that
fixes the edge cases legacy depends on (empty needle, overlapping matches).

### `hasher`

```rust
pub struct Hasher(blake3::Hasher);
impl Hasher {
    pub fn new() -> Self;
    pub fn update(&mut self, d: &[u8]) -> &mut Self;   // raw bytes
    pub fn field(&mut self, d: &[u8]) -> &mut Self;    // itoa(len) ":" d
    pub fn finish(&self) -> Digest;                    // first 16 bytes; non-consuming
}
pub struct Digest([u8; 16]);   // Display = 32 lowercase hex; also to_hex() -> [u8; 32]
pub fn hash_of(d: &[u8]) -> Digest;                    // HashOf: unframed
```

Tests: all eight rows of `jig-keys.md §Digest test vectors`, plus a property test that
`field(a).field(b)` never equals `field(a').field(b')` for different splits of `a‖b`.

Streaming: `hash_of` for files reads in 64 KiB chunks through `update`. jigd's IDS hashing
uses the same path (see 03, Q-ID-2 stays as-is: legacy reads the whole file, but streaming
gives the same digest, so it is not an observable change).

### `keys`

- `enum Tool { Cc, Rustc }`. `RequestKey::text()` gives `<hex>` or `rs/<hex>`.
- Prefix constructors: `manifest(k1)`, `object(k2)`, `stderr(k2)`, `depfile(k2)`,
  `retcode(k2)`, `blob(hex)`, `go_action(hex)`, `go_output(hex)`. Each returns `Vec<u8>`, and
  these are the **only** places the prefixes `m/ o/ e/ d/ r/ b/ go/a/ go/o/ rs/` are spelled.
- `Key(path)` lexical normalisation and `InputId` (`S:` / `C:` / absent): `jig-keys.md §Key:
  lexical normalisation`, `§InputId`. Test vectors in §Key test vectors and §Composite test
  vector.
- `KeyEnvironment` (the env-var fields that go into link keys): `jig-cc-mode.md §Request key`,
  test vector `TestKeyEnvironment`.

k1 field *sequences* are tool-specific and live in the front ends (06, 07). `keys` gives the
building blocks, and a `#[must_use]` builder so a field cannot be silently dropped.

### `store_path`

```rust
pub const STORE_DIR: &[u8];                     // from build.rs (01-workspace.md)
pub fn is_store_path(p: &[u8]) -> bool;
pub fn state_dir() -> Vec<u8>;                  // STORE up to last '/' + "/var/nix"
pub fn default_socket() -> Vec<u8>;             // state_dir() + "/jigd/socket"
pub fn mask_hashes(text: &[u8]) -> Vec<u8>;     // MaskHashes, rescans from h+1 in the modified text
pub enum IdentityMode { Path, Content }         // JIG_STORE_IDENTITY == "content"
pub struct Store { mode, out_hash, roots }      // constructed once from env
impl Store {
    pub fn mask_out(&self, b: &[u8]) -> Cow<[u8]>;
    pub fn unmask_out(&self, b: &[u8]) -> Cow<[u8]>;
    pub fn mask_for_replay(&self, t: &[u8]) -> Cow<[u8]>;
    pub fn resolve(&self, p: &[u8]) -> Option<Cow<[u8]>>;
    pub fn resolve_all(&self, t: &[u8]) -> Cow<[u8]>;   // ADOPTED Q-KEY-1: identity in path mode
    pub fn tool_id(&self, path: &Path) -> Vec<u8>;
}
```

`Store` is built from an explicit `Env` snapshot (a small struct holding the variables read),
not from `std::env` inside the functions. This keeps the functions pure for tests and lets the
difftest harness give both implementations the same environment.

**Adopted Q-KEY-1:** `resolve_all` returns its input unchanged in path mode. Legacy masks in
both modes. The adopted-change test feeds a depfile containing a store path through the
path-mode replay and asserts the bytes come back unmasked. The difftest allow-list expects the
legacy output to differ in exactly those bytes.

### `frame`: value encoding

```rust
pub fn encode(raw: &[u8]) -> Result<Vec<u8>, FrameError>;   // u32 LE (len mod 2^32) + zstd level 1
pub fn decode(wire: &[u8]) -> Option<Vec<u8>>;               // <4 bytes → None; exact size; multi-frame
```

- **Adopted Q-CLIENT-1:** the header is little-endian on every host (`u32::to_le_bytes`), not
  host order.
- `decode` uses `zstd::bulk::Decompressor::decompress_to_buffer` into a buffer of exactly
  `size` bytes. To accept concatenated frames it loops with `zstd::stream::raw` (the bulk API
  stops after one frame), and succeeds only if the total equals `size`.
- Allocation guard: `size` comes from the wire. `decode` refuses `size > 4 GiB - 1`
  (impossible anyway) and also checks `ZSTD_getFrameContentSize` sums when they are present,
  before it allocates. This guard is internal and cannot change an observable result: legacy
  would fail on the same input, just after allocating.
- Tests: the `hello` vector (only when the linked libzstd is 1.5.7, otherwise decode-only),
  round-trip proptests, two concatenated frames, truncated header, size mismatch.

### `wire`: jigd line protocol

Shared by the client (jig) and server (jigd), so the parsing rules exist once.

```rust
pub const MAX_LINE: usize = 65_535;             // content bytes; the 65 536th non-'\n' byte = hang up
pub fn fields(line: &[u8]) -> Fields<'_>;             // Go strings.Fields on raw bytes (Q-PROTO-3);
                                                      // first 4 fields + total count, no allocation
pub fn parse_int64(s: &[u8]) -> Option<i64>;          // Go strconv.ParseInt(s,10,64): [+-]?[0-9]+
pub fn parse_ids_count(s: &[u8]) -> Option<usize>;    // Atoi + 0..=1<<20
pub fn parse_hello_ver(s: &[u8]) -> Option<u32>;      // 1..=i32::MAX
pub enum Request<'a> { Hello(u32), Get(&'a [u8]), Has(..), Put(&'a [u8], i64), Ids(usize),
                       Slot(&'a [u8]), Done(&'a [u8]), Stats }
pub fn parse_request(line: &[u8]) -> Option<Request>; // None → hang up (dispatch table)
pub const PROTOCOL_MAX: u32 = 1;
```

`fields` details (from `jigd-protocol.md §Tokenization`): separators are the ASCII set
`\t \n \v \f \r space`, plus each multi-byte UTF-8 encoding of a `White_Space` code point
(U+0085, U+00A0, U+1680, U+2000–U+200A, U+2028, U+2029, U+202F, U+205F, U+3000). Decode one
UTF-8 scalar at a time with a hand-written decoder. Invalid sequences count as non-space and
advance one byte, as Go's `utf8.DecodeRune` does with `RuneError`. Test against a table built
by running Go `strings.Fields` over a byte corpus (the generator is checked in under
`jig-testkit`; its output is a committed vector file, so CI needs no Go for this test).

`Fields` holds a fixed `[&[u8]; 4]` plus the total count. Dispatch needs at most 3 fields, and
"more than 3" only means "hang up", so nothing is allocated per request.

### `manifest`

Formatting and parsing of manifest lines, `jig-keys.md §Manifest`, with its test vectors. Shared
because cc and rustc both use it, and because the difftest harness decodes manifests fetched
from jigd to explain key mismatches.

## Done when

- every vector in `jig-keys.md` and `jig-cache-client.md §Value encoding` passes;
- the `fields` corpus agrees with Go on 100 % of entries;
- the `parse_int64` corpus (signs, leading zeros, overflow at ±2⁶³) agrees with Go;
- `cargo test -p jig-core` runs in under 2 s with no network and no Go.
