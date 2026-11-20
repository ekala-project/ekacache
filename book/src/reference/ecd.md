# ecd

```text
usage: ecd <socket>

The compile cache and job-slot daemon ec talks to. Nix builds reach it through
sandbox-paths /nix/var/nix/ekacache/socket=<socket>.

  NIX_STORE_DIR   the only tree IDS answers for (default /nix/store)
  XDG_CACHE_HOME  packs live in $XDG_CACHE_HOME/ekacache/packs (default ~/.cache)
  EKACACHED_SIZE       cache budget in GiB (default 50)
  EKACACHED_COLD       a second, slower pack directory that takes what EKACACHED_SIZE pushes out
  EKACACHED_COLD_SIZE  its budget in GiB (default 0: unbounded)
  EKACACHED_SLOTS      concurrent compile slots (default: number of CPUs)
```

## Arguments

| Invocation | Effect |
|---|---|
| `ecd <socket>` | run the daemon on `<socket>` |
| `ecd --help`, `ecd -h` | print usage to stdout, exit 0 |
| `ecd` with no argument, or more than one | print usage to stderr, exit 2 |

Any single argument other than `--help`/`-h` is the socket path, even if it starts with `-`.
`ecd --version` creates a socket named `--version`. There is no version flag.

The environment variables are described in [Configuring ecd](../setup/configuring-ecd.md).
The size budgets also accept `M`, `G` and `T` suffixes, and `0` means unbounded.

## Exit status

| Status | Cause |
|---|---|
| 0 | `--help`; a clean shutdown after SIGINT or SIGTERM; an error accepting connections, such as running out of file descriptors |
| 1 | a fatal startup error, logged on stderr: no `HOME`, the store cannot be opened, the socket cannot be created |
| 2 | wrong number of arguments; an invalid `EKACACHED_SIZE` or `EKACACHED_COLD_SIZE` |

## Signals

| Signal | Effect |
|---|---|
| SIGINT, SIGTERM | stop listening, remove the socket, finish pending demotions, seal the active pack, exit 0. Repeats are ignored |
| SIGHUP | terminates without sealing (no reload) |
| SIGQUIT | terminates with a core dump |
| SIGKILL | terminates; the next start recovers the active pack |

## Logging

To stderr, as `YYYY/MM/DD HH:MM:SS <message>`. See
[Monitoring](../operating/monitoring.md#the-daemon-log).

## Protocol

`ec` and `ecd` talk a line protocol over the socket. The one request that is useful by hand
is `STATS`; see [Monitoring](../operating/monitoring.md#daemon-statistics).
