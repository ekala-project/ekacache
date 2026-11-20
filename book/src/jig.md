# Coming from jig

ekacache is the Rust rewrite of jig and jigd, which were written by Jörg Thalheim and the
repkgs contributors. It behaves the same way in most respects, but **it does not interoperate
with jig deployments**. A jig client cannot talk to `ecd`, and `ec` cannot talk to jigd.

## Renamed

| jig | ekacache |
|---|---|
| `jig` | `ec` (there is no `jig` alias) |
| `jigd` | `ecd` |
| `JIG_*` variables | `EKACACHE_*` |
| `JIGD_*` variables | `EKACACHED_*` |
| jig's socket | `/nix/var/nix/ekacache/socket` |
| jig's pack directory | `$XDG_CACHE_HOME/ekacache/packs` |
| jig's driver config | `etc/ekacache.json` |
| `jig: …`, `jigd: …` messages | `ec: …`, `ecd: …` |

Update `nix.conf`'s `extra-sandbox-paths`, service units and any scripts that set the
variables.

## Other changes you may notice

- **Size variables are strict.** `EKACACHED_SIZE` and `EKACACHED_COLD_SIZE` accept
  `<n>[M|G|T]` and `0`. Values jigd used to accept, such as `+50`, now stop `ecd` with exit
  status 2.
- **Failed stores are reported.** `ec` prints `ec: cache put <key> failed`, and
  `ec cache put` exits 1 when the daemon refuses a value.
- **Replayed depfiles name real store paths**, so make no longer fails with "No rule to make
  target" after a hit.
- **More invocations are uncacheable:** coverage, split DWARF, `-save-temps`, time traces,
  C++20 modules, profile-guided optimisation, compiler plugins, `.incbin`, and other
  toolchains' temporary compiles. You get fewer hits, but none that are wrong.
- **Resource-exhaustion failures are not cached** (full disk, out of memory, too many open
  files, …).
- **Unreadable inputs** make a compile uncached instead of being stored.
- **Rust:** `env!` variables are tracked, and `--crate-type lib,cdylib` is treated as a link,
  so it is not cached. Existing rustc entries miss once.
- **Store housekeeping:** empty packs are cleaned up, an over-budget store is trimmed at
  startup, and sealed packs are synced to disk.
- **ecd logs** one `tier …` line per store tier at startup.
- **SIGQUIT** makes `ecd` dump core instead of exiting 2.

## Moving an existing store

`ecd` reads jigd's on-disk pack format. Stop the old daemon, move its packs directory to
`$XDG_CACHE_HOME/ekacache/packs` for `ecd`'s user, and start `ecd`. Most C and C++ entries
remain valid; Rust entries miss once.
