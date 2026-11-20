# Running ecd

A host needs three things:

1. `ecd` running as a long-lived service, listening on a socket;
2. that socket visible inside Nix build sandboxes;
3. compilers in the package set that go through `ec` (see
   [Hooking ec into a build](../builds/integration.md)).

## Start the daemon

`ecd` takes exactly one argument, the socket path:

```console
$ ecd /nix/var/nix/ekacache/socket
```

`ecd` runs in the foreground and logs to stderr. On startup it:

1. creates the socket's parent directory (`mkdir -p`, mode 0755);
2. **removes whatever is already at the socket path**, whether a stale socket or a file;
3. listens, and sets the socket's mode to 0666. Access is decided per connection by
   [peer authentication](security.md), not by the file mode.

It logs a `listening on …` line when it is ready. The full command line is in
[`ecd`](../reference/ecd.md).

> **Run only one ecd per socket.** `ecd` does not check for a running daemon. A second
> `ecd` started on the same path deletes the first one's socket and takes over. The first
> keeps running, but nothing can reach it.

## Make the socket visible to builds

Add the socket's directory to the sandbox in `nix.conf`:

```ini
extra-sandbox-paths = /nix/var/nix/ekacache
```

Then restart `nix-daemon`. Mapping the **directory** rather than the socket file means builds
still see the socket after `ecd` restarts and creates a new one.

If `ecd` listens somewhere else on the host, map that path to the default location inside
the sandbox:

```ini
extra-sandbox-paths = /nix/var/nix/ekacache/socket=/run/ekacache/socket
```

Builds need nothing else. `ec`'s default socket, `/nix/var/nix/ekacache/socket`, is the
in-sandbox path.

## Who to run it as

The user `ecd` runs as matters in two ways:

- **Where the store lives.** The packs go in `$XDG_CACHE_HOME/ekacache/packs`, falling back
  to `$HOME/.cache/ekacache/packs`. If neither `XDG_CACHE_HOME` nor `HOME` is set, `ecd`
  exits with `$HOME is not defined`.
- **Who can connect.** On Linux, `ecd` accepts its own user, root, members of `nixbld`
  (by primary group) and the default `auto-allocate-uids` range. See
  [Security and trust](security.md).

A dedicated system user with its own state directory is a good choice. That user must be
able to create or write `/nix/var/nix/ekacache`.

## Example: a systemd unit

ekacache does not ship a service unit or a NixOS module yet. The following is an example to
adapt, not a tested file:

```ini
# /etc/systemd/system/ecd.service
[Unit]
Description=ekacache daemon (compile cache and job slots)
Before=nix-daemon.service

[Service]
ExecStart=/path/to/ekacached/bin/ecd /nix/var/nix/ekacache/socket
User=ekacache
Group=ekacache
# the packs go in $XDG_CACHE_HOME/ekacache/packs
StateDirectory=ekacache
Environment=XDG_CACHE_HOME=/var/lib/ekacache
Environment=EKACACHED_SIZE=100G
# ecd exits 0 on some fatal accept errors, so restart it on any exit
Restart=always
# a clean stop waits for pending pack moves to the cold tier
TimeoutStopSec=10min

[Install]
WantedBy=multi-user.target
```

The settings that matter:

- **`Restart=always`.** If `ecd` runs out of file descriptors it shuts down cleanly with exit
  status 0, so `Restart=on-failure` would leave it down.
- **A generous stop timeout.** On SIGTERM `ecd` waits for pending demotions to the cold tier,
  which can take minutes when that tier is on another filesystem. A second SIGTERM is
  ignored; SIGKILL is safe, and the next start recovers the unsealed pack.
- **Don't send SIGHUP.** `ecd` has no reload signal. SIGHUP terminates it without sealing
  the active pack. Recovery at the next start handles that, but it is not a reload.
- **`/nix/var/nix/ekacache` must be writable** by the service user, or created for it in
  advance.

## Stopping

SIGINT or SIGTERM:

1. stops accepting connections and removes the socket;
2. waits for pending demotions and evictions;
3. seals the active pack, and exits 0.

Open connections are dropped without replies. The builds behind them see a miss and compile
normally.
