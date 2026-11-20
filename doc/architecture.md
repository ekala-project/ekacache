# Architecture

## The pieces

```
 compiler invocation (gcc, clang, rustc, go, …)
        │  argv[0] / argv[1] / RUSTC_WRAPPER / GOCACHEPROG
        ▼
  ec  (crates/ekacache)          synchronous, one short-lived process per compile
        │  line protocol over a Unix socket (EKACACHE_SOCK, default /nix/var/nix/ekacache/socket)
        ▼
  ecd (crates/ekacached)         tokio daemon, one per machine
        ├─ store: append-only bitcask packs + hints, size tiers, demotion, eviction
        ├─ slots: host-wide compile slots, so N sandboxes × make -jN don't oversubscribe
        └─ identities: IDS answers for store files
```

`ekacache-core` holds what both sides must agree on: the hasher, key prefixes, store-path
masking, value framing and the wire codec. `ekacache-testkit` is test-only support, covering
the differential harness, the ITF trace reader, the vector files and process helpers.

Derivations never mention the cache. If the socket is not reachable, ec runs the compiler
normally, so the outputs are the same either way.

## Keys (two-key lookup)

```
k1 = H(tool, cwd, normalised args, primary source)  -> manifest: one "input\tid" line per file read
k2 = H(k1, manifest)                                 -> outputs, stderr, depfile, exit status
```

The inputs come from the compiler's depfile or rustc's dep-info. Store files are identified by
path and build-tree files by content. The details are in
[`behaviors/ec-keys.md`](../behaviors/ec-keys.md) and `crates/ekacache/src/twokey.rs`.

## ec modes

| Mode | Code | Behaviour doc | Plan |
|---|---|---|---|
| dispatch, launcher, process | `dispatch.rs`, `launcher.rs`, `process.rs` | [ec-dispatch](../behaviors/ec-dispatch.md) | [05](../implementation/05-ec-foundation.md) |
| cache client | `client.rs` | [ec-cache-client](../behaviors/ec-cache-client.md) | [05](../implementation/05-ec-foundation.md) |
| driver policy (`ekacache.json`) | `driver/` | [ec-driver-policy](../behaviors/ec-driver-policy.md) | [06](../implementation/06-ec-cc.md) |
| C/C++ compile and link | `cc/` | [ec-cc-mode](../behaviors/ec-cc-mode.md) | [06](../implementation/06-ec-cc.md) |
| rustc | `rustc/` | [ec-rustc-mode](../behaviors/ec-rustc-mode.md) | [07](../implementation/07-ec-rustc.md) |
| GOCACHEPROG | `gocache.rs` | [ec-gocacheprog](../behaviors/ec-gocacheprog.md) | [08](../implementation/08-ec-gocache-and-cache-cmds.md) |
| `cache …`, `slot` | `cache_cmd.rs`, `slot.rs` | [ec-cache-commands](../behaviors/ec-cache-commands.md) | [08](../implementation/08-ec-gocache-and-cache-cmds.md) |
| `nix-store` | `nix_store/` | [ec-nix-store](../behaviors/ec-nix-store.md) | [09](../implementation/09-ec-nix-store.md) |
| `reloc-fixup` (ELF, Mach-O) | `fixup/` | [ec-reloc-fixup](../behaviors/ec-reloc-fixup.md) | [10](../implementation/10-ec-reloc-fixup.md) |

All the state a mode reads (argv, env, store config, socket path) is captured once into `Ctx`
(`crates/ekacache/src/lib.rs`).

## ecd modules

| Area | Code | Behaviour doc | Plan |
|---|---|---|---|
| CLI, socket, peer auth, signals | `daemon.rs`, `peer.rs`, `log.rs` | [ecd-daemon](../behaviors/ecd-daemon.md) | [03](../implementation/03-ecd-server.md) |
| protocol | `server/` | [ecd-protocol](../behaviors/ecd-protocol.md) | [03](../implementation/03-ecd-server.md) |
| store | `store/` | [ecd-store](../behaviors/ecd-store.md) | [04](../implementation/04-ecd-store.md) |
| slots | `slots.rs` | [ecd-slots](../behaviors/ecd-slots.md) | [03](../implementation/03-ecd-server.md) |
| identities | `identity.rs` | [ecd-identities](../behaviors/ecd-identities.md) | [03](../implementation/03-ecd-server.md) |
| Go-compatible errors and paths | `goerr*.rs`, `gofs.rs`, `gopath.rs` | (Go's error text, kept byte for byte) | |

## Platforms

Linux (x86_64, aarch64) is the primary target. macOS is phase P9
([11-macos.md](../implementation/11-macos.md)). Platform splits use
`cfg(target_os = "linux")` and `cfg(target_vendor = "apple")`, and several quirks differ by
platform (for example, abstract sockets exist only on Linux).
