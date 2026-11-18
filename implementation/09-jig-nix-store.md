# 09: `nix-store` mode (phase P8b)

Specified by: `jig-nix-store.md` (all), `spec/builder_rpc.qnt`.
Adopted here: nothing. Q-NIX-4..9 are proposals awaiting sign-off, so every quirk is
reproduced.

This mode is self-contained: it shares no keys or jigd traffic with the rest of jig. It can
therefore be built at any point after P3, in parallel with P5–P7.

## Module (`crates/jig/src/nix_store/`)

| File | Contents | Spec section |
|---|---|---|
| `mod.rs` | processing order: usage → open store → dispatch → verbs; exit codes | §Invocation |
| `hash.rs` | `nix32`, `fold20`, `make_store_path`, `text_path`, `fixed_output_path` (with `sha2`) | §Store path computation |
| `json.rs` | the nlohmann `value(key, default)` emulation over `serde_json::Value`, abort on a type mismatch (including `null`), BOM strip; `dump()`-compatible serialiser | §JSON decoding, §fetchurls output |
| `drv.rs` | `add-drv` JSON → ATerm, in the documented field order (failure vs abort); `fetchurl_derivation` | §Derivations |
| `wire.rs` | word, padded string, string list, framed contents, 64 MiB receive limit, exact reads (EINTR = failure), buffered writes with flush points | §Worker protocol §Encoding |
| `rpc.rs` | socket (`NIX_REMOTE`), handshake (1.38 + feature exchange), stderr drain loop, AddToStore (op 7), SubmitOutput (op 1000), the client state-transition table | §Socket … §Client state-transition table |
| `offline.rs` | `JIG_NIX_STORE_OFFLINE` path: local text paths, no-op submit | §Modes of operation |

## Parity hazards and how each is handled

| Hazard | Handling |
|---|---|
| **SIGPIPE** (Q-NIX-7 kept): legacy writes with plain `write(2)` and dies by SIGPIPE when the inherited disposition is default | jig keeps the inherited SIGPIPE disposition (see 05 §Signal disposition). `wire.rs` uses `rustix::io::write` (not `send` with `MSG_NOSIGNAL`), so the kernel delivers SIGPIPE exactly as for legacy. A test spawns jig with SIGPIPE default and with SIGPIPE ignored against a fake daemon that closes early, and asserts death-by-SIGPIPE in the first case and exit 1 in the second. |
| **JSON abort semantics** (Q-NIX-4 kept) | `json::value_str(obj, key, default)` etc. call `std::process::abort()` on a wrong type, `null` included, and on non-object access. Field order in `drv.rs` follows the table in §`add-drv` input → ATerm exactly, because order decides failure vs abort. |
| **Object iteration order** | nlohmann `json` objects are `std::map`, so iteration is **sorted by key** (bytewise). `serde_json` with `preserve_order` keeps insertion order, and without it uses a `BTreeMap<String, _>` (sorted, but by `String` `Ord`, which for valid UTF-8 equals bytewise order). Use the **default** (BTreeMap) map type for this module. Note that 06 needs nothing order-sensitive, so the workspace does not enable `preserve_order` at all. (Update 01's dependency note when implementing.) |
| **`dump()` escaping** | a hand-written serialiser for the fetchurls output (about 30 lines): `\"`, `\\`, `\b \t \n \f \r`, other bytes < 0x20 as `\u00xx` in lowercase, everything else verbatim. `serde_json`'s escaping is the same for this set except that it emits `\u001f`-style escapes in lowercase too. Write our own anyway, so the bytes are pinned by our code and not by a dependency's version. |
| **Duplicates sorted, not removed** (Q-NIX-5) | `sort()` on `Vec<Vec<u8>>`, no `dedup`. |
| **Empty contents add a zero word** (Q-NIX-6) | framed contents always writes `len`, the bytes, then `0`. |
| **Drain-loop read failures ignored** (Q-NIX-8 kept) | reproduce, including the effectively infinite loop for a huge field count on EOF. The test caps it with a timeout and asserts "still looping", which is the specified behaviour. |
| **Misattributed diagnostics, silent failures** (Q-NIX-9, -10) | reproduce message for message. Every stderr string in §Errors and messages is a `const` in `mod.rs`. |
| **`addrlen = sizeof(sockaddr_un)`, empty path** (Q-NIX-11, -12) | the same explicit `SocketAddrUnix` construction as the jigd client (05), shared helper. |
| **U+2026 in the usage text** | the bytes `E2 80 A6` in a `const &[u8]`. |
| **stdin read only after the handshake** | the order is enforced by structure: `open_store()` returns before `read_stdin()` is reachable. |
| **Q-NIX-1** (dangling temporary, C++ only) | not reproduced: the doc marks it C++-only UB, with no defined behaviour to copy. |

## Tests

1. All vectors in `jig-nix-store.md §Test vectors`: SHA-256, Nix32, fixed output path, text
   paths, ATerm, fetchurls, CLI behaviour offline, and the wire examples (byte-exact client
   transcripts).
2. A **fake builder-rpc daemon** in `jig-testkit` that speaks the 1.38 handshake, the stderr
   message kinds, AddToStore and SubmitOutput. It is scriptable to misbehave (close early,
   oversize strings, bad magic, `STDERR_ERROR`). One test per row of §Client
   state-transition table.
3. Difftest: legacy and Rust jig against the fake daemon with the same scripts, comparing the
   client→daemon bytes, stdout, stderr and the exit status/signal.
4. Quint: `spec/builder_rpc.qnt` traces drive the fake daemon's responses. The client's
   observable sequence (ops sent, exit) must match the trace.
5. Fuzz: `add-drv` JSON input (offline mode). Rust and legacy agree on stdout, stderr and the
   exit status/signal (nightly, with the legacy binary in the fuzz harness as an oracle).
