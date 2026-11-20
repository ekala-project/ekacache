# 11: macOS (phase P9)

The decision: Linux first, macOS later. This phase brings the Rust ec and ecd to the
**macOS host** behaviour of legacy, and finishes the host-independent Mach-O fixup deferred
from P8a (see [10 §A correction](10-ec-reloc-fixup.md#a-correction-to-the-platform-split)).

Until P9 lands, the crates still **compile** on macOS (CI runs `cargo check
--target aarch64-apple-darwin` from P0), so nothing Linux-only creeps into shared code by
accident. Linux-only items sit behind `cfg(target_os = "linux")`, with a `compile_error!` or an
explicit runtime refusal on other targets, never a silent fallback.

## Inventory of host-specific behaviour

Everything the behaviour docs say differs on macOS or non-Linux hosts:

| Item | Legacy on macOS | Spec | Rust work |
|---|---|---|---|
| Conf-less `binfmt` | `macho` when built for Apple (`#ifdef __APPLE__`, `driver.cc:439`) | `ec-driver-policy.md §$EKACACHE_CC and conf-less mode` | `cfg(target_vendor = "apple")` → `BinFmt::MachO`. A one-liner, but it changes the cc link-cacheability rule (non-ELF links are uncacheable). Test it. |
| Locating `etc/ekacache.json` | `readlink("/proc/self/exe")` fails → skip to step 5 (`$EKACACHE_CC`) | `ec-driver-policy.md §Locating`, Q-DRV-2, OQ3 | Reproduce: **do not** use `current_exe()` on macOS. Q-DRV-2 recommends it, but that is unsigned. Ask at P9 start whether ekacache.json on macOS hosts should work, which would be a sign-off. |
| `self` path for GOCACHEPROG | `/proc/self/exe` fails → `""` when argv[0] has no `/` | `ec-dispatch.md §Launcher mode` | reproduce (`""`) |
| ecd peer authentication | `allowed` and `fdUnder` always true (Q-DAEMON-8) | `ecd-daemon.md §Peer authentication` | `peer/other.rs`. Reproduce: no checks. **Flag:** macOS has `getpeereid`/`LOCAL_PEERCRED`, and using it would be a proposed change (Q-DAEMON-8). Raise it at P9 start; it is cheap. |
| IDS `fdUnder` | always true | `ecd-identities.md` | follows from the row above |
| Default slot count | Go `runtime.NumCPU()` on darwin (`getncpu`) | `ecd-daemon.md §Default slot limit` | `sysctl hw.ncpu` via `libc::sysctlbyname`. Verify against Go's darwin `getncpu` (it reads `hw.ncpu`). |
| `XDG_CACHE_HOME` fallback | Go `os.UserHomeDir()` on darwin = `$HOME` | `ecd-daemon.md §Environment` | same code as Linux. Go does **not** use `~/Library/Caches` here, so neither do we. |
| `sun_path` size | 104 | `ec-cache-client.md §Connect`, `ec-nix-store.md §Socket` | use `size_of::<sockaddr_un>().sun_path` from `libc`, not a literal 108 |
| `SO_PEERCRED` / `/proc/self/fd` | absent | | not used on macOS (see above) |
| `sched_getaffinity` | absent | | not used on macOS |
| `MSG_NOSIGNAL`, `SOCK_CLOEXEC` | legacy uses both unconditionally (`cache_client.cc:34,49`, `nix_store_mode.cc:341`), and older macOS SDKs define neither | `ec-cache-client.md §Framing` | **Open item:** confirm whether legacy builds on macOS at all. If it does not, macOS "parity" means only the docs' non-Linux notes, and P9 shrinks. Rust: `SO_NOSIGPIPE` on the socket in place of `MSG_NOSIGNAL`, and `fcntl(FD_CLOEXEC)` after `socket` |
| abstract sockets (`@…`) | Linux only | Q-DAEMON-10 | on macOS `@…` is an ordinary path, as Go treats it |
| `utimensat` with `UTIME_OMIT` | supported on macOS ≥ 10.13 | `ecd-store.md §Recency` | same code |
| `link(2)` + EEXIST in demotion; `fsync` | `fsync` on macOS doesn't flush the drive cache (`F_FULLFSYNC` does) | `ecd-store.md`, adopted Q-STORE-7 | **Flag:** the adopted fsync-on-seal intends durability. On macOS that needs `F_FULLFSYNC`. Propose using `F_FULLFSYNC` on macOS, which is in the spirit of the adopted change. Confirm at P9 start. |
| nix-store builder-rpc on Darwin | unknown whether it exists | `ec-nix-store.md` OQ5 | same code with the 104-byte limit. Test against the fake daemon only. |
| Mach-O fixup | runs on every host | `ec-reloc-fixup.md` A6 | implement A6 (10) |
| Mach-O driver policy (A11) | runs on every host | `ec-driver-policy.md` A11 | already in P5, as argument text; exercised end to end here |

## Work items

1. macOS CI job: `cargo test --workspace`, plus difftest against a legacy build if one exists
   (see the open item on whether legacy builds on macOS).
2. `cfg` sweep: grep for `target_os = "linux"` and make sure every gated item has a macOS
   counterpart or an explicit refusal.
3. Mach-O fixup (A6) with the tests listed in 10 §Tests item 5.
4. End-to-end on an Apple Silicon host: build a small C/C++ project with nixpkgs-darwin stdenv
   through ec with ecd, run it twice, get hits, and check that the reloc-fixup'd dylibs
   load from a relocated tree (code signature valid: `codesign -v`).

## Decisions to collect at the start of P9

These are questions for later, recorded so they are not lost. None is needed for P0–P8.

1. Should `etc/ekacache.json` be found on macOS (via `current_exe`)? (Q-DRV-2)
2. Should ecd authenticate peers on macOS (`getpeereid`)? (Q-DAEMON-8)
3. Should ecd use `F_FULLFSYNC` for the adopted fsync-on-seal?
4. Should `pageSize ≥ 64` in a CodeDirectory be an error? (C++ UB; see 10)
