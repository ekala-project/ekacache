# Environment variables

**Empty is not unset.** For `ec`, a default applies only when a variable is not set at all.
A variable set to the empty string is used as the empty string. For example,
`EKACACHE_SOCK=` disables the cache. `ecd` treats empty as unset.

## ec

| Variable | Meaning | Default |
|---|---|---|
| `EKACACHE_SOCK` | path of `ecd`'s socket; empty disables the cache | `/nix/var/nix/ekacache/socket` |
| `EKACACHE_LOG` | file to append one outcome line per compile to ([format](../operating/monitoring.md#per-compile-log)) | none |
| `EKACACHE_LOG_ARGS` | file to append the command line of each uncached C/C++ compile to | none |
| `EKACACHE_CC` | the real compiler when no `etc/ekacache.json` is found | none |
| `EKACACHE_STORE_IDENTITY` | `content` selects [content mode](../concepts/keys.md#content-mode); anything else is path mode | path mode |
| `EKACACHE_STORE_ROOTS` | space-separated store paths, used in content mode to map masked paths back | empty |
| `EKACACHE_SLOT` | set by `ec` to `1` for its children while it holds a slot; any non-empty value means "don't ask for a slot" | unset |
| `EKACACHE_NIX_STORE_OFFLINE` | non-empty: [`ec nix-store`](ec-nix-store.md) works offline | unset |
| `PKGS_CC`, `PKGS_PREFIX_MAP` | per-package flags for toolchain drivers ([ekacache.json](../builds/driver-policy.md#per-package-settings)) | unset |
| `GOCACHEPROG_DIR` | where `ec gocacheprog` writes files for Go | `$TMPDIR/gocacheprog` |

Other variables `ec` reads:

- `NIX_BUILD_TOP` identifies the build for slot fairness. It must not be set to an empty or
  whitespace value.
- `TMPDIR` is used for temporary files.
- `GOCACHEPROG` is set by launcher-mode `go` if empty.
- `NIX_REMOTE` is read by `ec nix-store`.
- `out` is used in content mode.

`ec` passes its whole environment through to the compiler.

`EKACACHE_STORE_DIR` is **not** a runtime variable. It is read when `ec` is built (see
[Installing](../setup/installing.md#the-store-directory-is-fixed-at-build-time)).

## ecd

| Variable | Meaning | Default |
|---|---|---|
| `EKACACHED_SIZE` | main store budget: `<n>[M\|G\|T]`, plain numbers are GiB, `0` is unbounded | `50` |
| `EKACACHED_COLD` | directory of a cold tier | none |
| `EKACACHED_COLD_SIZE` | cold tier budget, same syntax | `0` |
| `EKACACHED_SLOTS` | concurrent compile slots | number of CPUs |
| `XDG_CACHE_HOME` | the main store is `$XDG_CACHE_HOME/ekacache/packs` | `$HOME/.cache` |
| `HOME` | fallback for `XDG_CACHE_HOME` | |
| `NIX_STORE_DIR` | the store whose files `ecd` fingerprints | `/nix/store` |

See [Configuring ecd](../setup/configuring-ecd.md).
