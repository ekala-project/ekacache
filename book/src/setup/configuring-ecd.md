# Configuring ecd

`ecd` has no configuration file and no flags besides the socket path. You configure it with
environment variables, which it reads once at startup. An empty variable counts as unset.

| Variable | Meaning | Default |
|---|---|---|
| `EKACACHED_SIZE` | size budget of the main store | `50` (GiB) |
| `EKACACHED_COLD` | directory of an optional second, slower tier | none |
| `EKACACHED_COLD_SIZE` | size budget of the cold tier | `0` (unbounded) |
| `EKACACHED_SLOTS` | host-wide limit on concurrent compiles | number of CPUs |
| `XDG_CACHE_HOME` | the main store is `$XDG_CACHE_HOME/ekacache/packs` | `$HOME/.cache` |
| `NIX_STORE_DIR` | the store whose files `ecd` fingerprints | `/nix/store` |

Changing any of them takes a restart.

## Store size

`EKACACHED_SIZE` and `EKACACHED_COLD_SIZE` take a whole number with an optional suffix:

| Value | Means |
|---|---|
| `50` | 50 GiB (no suffix means GiB) |
| `512M`, `512m` | 512 MiB |
| `100G` | 100 GiB |
| `2T` | 2 TiB |
| `0` | unbounded |

Anything else, such as `1.5G`, `+50`, `-1`, `50GB` or a value with spaces, is refused at
startup:

```text
ecd: invalid EKACACHED_SIZE "1.5G": want <n>[M|G|T], 0 = unbounded
```

and `ecd` exits with status 2. `EKACACHED_COLD_SIZE` is only checked when `EKACACHED_COLD` is
set.

The budget counts the size of the pack files. When new entries take the store over budget,
`ecd` moves whole packs out, most stale first, then least recently read. They move to the
cold tier if one is configured, and are deleted otherwise. Lowering the budget and
restarting shrinks the store right away. See [The store on disk](../operating/store.md).

## A cold tier

```console
$ EKACACHED_SIZE=50G EKACACHED_COLD=/mnt/hdd/ekacache EKACACHED_COLD_SIZE=1T \
    ecd /nix/var/nix/ekacache/socket
```

New entries are always written to the main tier. Packs pushed out of it move to the cold
tier, by hard link if both are on the same filesystem, or by copy otherwise. Packs pushed out
of the cold tier are deleted. Entries in both tiers are served the same way.

The cold tier must be writable. A read-only cold tier does not work: its packs fail to load
and are skipped.

## Slots

`EKACACHED_SLOTS` is a positive integer. The default is the number of CPUs `ecd` may run on
(its affinity mask at startup).

Two things to watch for:

- **Containers with a CPU quota.** The default ignores cgroup CPU quotas, so in a container
  limited to 4 CPUs on a 64-core host it is 64. Set `EKACACHED_SLOTS` explicitly there.
- **Bad values are ignored silently.** `0`, a negative number or junk falls back to the
  default without a message.

Slots only limit misses. Cache hits take no slot.

## Store directory

`NIX_STORE_DIR` tells `ecd` which tree it fingerprints when `ec` asks about store files. It
should match the store `ec` was built for (see
[Installing](installing.md#the-store-directory-is-fixed-at-build-time)). A mismatch does not
cause wrong results, only slower ones.
