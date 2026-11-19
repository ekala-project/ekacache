# Architecture

## The pieces

```
 compiler invocation (gcc, clang, rustc, go, …)
        │  argv[0] / argv[1] / RUSTC_WRAPPER / GOCACHEPROG
        ▼
  ec  (crates/ekacache)          synchronous, one short-lived process per compile
        │  line protocol over a Unix socket (JIG_SOCK, default /nix/var/nix/jigd/socket)
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
[`behaviors/jig-keys.md`](../behaviors/jig-keys.md) and `crates/ekacache/src/twokey.rs`.

## ec modes

| Mode | Code | Behaviour doc | Plan |
|---|---|---|---|
| dispatch, launcher, process | `dispatch.rs`, `launcher.rs`, `process.rs` | [jig-dispatch](../behaviors/jig-dispatch.md) | [05](../implementation/05-jig-foundation.md) |
| cache client | `client.rs` | [jig-cache-client](../behaviors/jig-cache-client.md) | [05](../implementation/05-jig-foundation.md) |
| driver policy (`jig.json`) | `driver/` | [jig-driver-policy](../behaviors/jig-driver-policy.md) | [06](../implementation/06-jig-cc.md) |
| C/C++ compile and link | `cc/` | [jig-cc-mode](../behaviors/jig-cc-mode.md) | [06](../implementation/06-jig-cc.md) |
| rustc | `rustc/` | [jig-rustc-mode](../behaviors/jig-rustc-mode.md) | [07](../implementation/07-jig-rustc.md) |
| GOCACHEPROG | `gocache.rs` | [jig-gocacheprog](../behaviors/jig-gocacheprog.md) | [08](../implementation/08-jig-gocache-and-cache-cmds.md) |
| `cache …`, `slot` | `cache_cmd.rs`, `slot.rs` | [jig-cache-commands](../behaviors/jig-cache-commands.md) | [08](../implementation/08-jig-gocache-and-cache-cmds.md) |
| `nix-store` | `nix_store/` | [jig-nix-store](../behaviors/jig-nix-store.md) | [09](../implementation/09-jig-nix-store.md) |
| `reloc-fixup` (ELF, Mach-O) | `fixup/` | [jig-reloc-fixup](../behaviors/jig-reloc-fixup.md) | [10](../implementation/10-jig-reloc-fixup.md) |

All the state a mode reads (argv, env, store config, socket path) is captured once into `Ctx`
(`crates/ekacache/src/lib.rs`).

## ecd modules

| Area | Code | Behaviour doc | Plan |
|---|---|---|---|
| CLI, socket, peer auth, signals | `daemon.rs`, `peer.rs`, `log.rs` | [jigd-daemon](../behaviors/jigd-daemon.md) | [03](../implementation/03-jigd-server.md) |
| protocol | `server/` | [jigd-protocol](../behaviors/jigd-protocol.md) | [03](../implementation/03-jigd-server.md) |
| store | `store/` | [jigd-store](../behaviors/jigd-store.md) | [04](../implementation/04-jigd-store.md) |
| slots | `slots.rs` | [jigd-slots](../behaviors/jigd-slots.md) | [03](../implementation/03-jigd-server.md) |
| identities | `identity.rs` | [jigd-identities](../behaviors/jigd-identities.md) | [03](../implementation/03-jigd-server.md) |
| Go-compatible errors and paths | `goerr*.rs`, `gofs.rs`, `gopath.rs` | (Go's error text, kept byte for byte) | |

## Platforms

Linux (x86_64, aarch64) is the primary target. macOS is phase P9
([11-macos.md](../implementation/11-macos.md)). Platform splits use
`cfg(target_os = "linux")` and `cfg(target_vendor = "apple")`, and several quirks differ by
platform (for example, abstract sockets exist only on Linux).

## Mixed fleets

ec works with the Go jigd, and the C++ jig works with ecd. Each daemon opens a store the other
wrote. That compatibility is the reason the jig-era names and formats are frozen; see
[13-rollout.md](../implementation/13-rollout.md).
