# Troubleshooting

Start with [the per-compile log](monitoring.md#per-compile-log): set `EKACACHE_LOG` and
look at the outcomes.

## Nothing ever hits

**Every line says `no-daemon`.** `ec` cannot reach the socket.

- Check `nix.conf` has `extra-sandbox-paths = /nix/var/nix/ekacache`, and that
  `nix-daemon` was restarted after the change.
- Check `ecd` is running and listening on the path builds expect:
  `ls -l /nix/var/nix/ekacache/socket`.
- Check `EKACACHE_SOCK` is not set to an empty string somewhere. An empty value disables
  the cache; it does not mean "use the default".
- Socket paths of 108 bytes or more (104 on macOS) cannot be used.

**The socket is there but nothing is stored**, and stderr shows
`ec: cache put … failed`, or `ec cache get` behaves as if there were no daemon. `ecd` is
probably refusing the connection. On Linux it accepts only its own user, root, users whose
**primary** group is `nixbld`, and Nix's default `auto-allocate-uids` range. Check:

- that build users' primary group is `nixbld`;
- that `nix.conf` does not change `start-id` or `id-count`;
- that the `nixbld` group existed when `ecd` started (restart it if not).

Refusals are not logged by `ecd`.

**Every line says `uncacheable`.** Set `EKACACHE_LOG_ARGS` to see the full command lines,
then compare them with [what runs uncached](../builds/c-cpp.md#what-runs-uncached). Common
causes are coverage or split-DWARF flags, `-save-temps`, profile-guided optimisation and
C++20 modules.

**Misses with `inputs-changed:` every time.** The named file changes between builds.
Generated headers that embed timestamps or build paths are the usual cause.

**Misses with `new-key` every time.** Something in the key changes between builds: the
working directory (outside a sandbox), an argument with a random or absolute temporary
path, or a different compiler store path.

**Rust: everything is `uncacheable`.** Check for `-C incremental=` (debug profiles with
incremental compilation) and that crates are libraries.

## A failure keeps being replayed

The log says `cached-error`. `ec` stores deterministic compile failures and replays them
while the inputs are the same. If the failure was really caused by something outside the
inputs, change an input, for example by adding a harmless `-D` flag, or bypass the cache
for one build with `EKACACHE_SOCK=`. There is no command to delete a single entry. Wiping
the store (see [The store on disk](store.md#wiping-the-cache)) removes all of them.

## Builds hang

`ec` has no timeouts. A daemon that accepts connections but does not answer hangs every
compile.

- `STATS` shows `waiting` above zero and `slots` at the limit while nothing compiles: slot
  tokens are stuck. Restart `ecd`; tokens are released when connections close.
- A tool between `ec` and its child that removes `EKACACHE_SLOT` from the environment can
  make nested `ec` calls (rustc linking through `cc`, for example) wait for a second slot
  and deadlock. Keep `EKACACHE_SLOT` in the environment.
- `ecd` itself is wedged: restart it. Builds that were waiting see a miss and continue.

## Too many compilers at once

- An inherited `EKACACHE_SLOT` with any non-empty value, even `0`, turns slot admission off.
  Make sure it is not set in the build environment.
- An empty or whitespace `NIX_BUILD_TOP` makes `ecd` drop the connection, so those compiles
  run uncached and unlimited.
- In a container with a CPU quota, the default slot count is the host's CPU count. Set
  `EKACACHED_SLOTS`.

## ecd will not start

| Message | Fix |
|---|---|
| `ecd: invalid EKACACHED_SIZE "…": want <n>[M\|G\|T], 0 = unbounded` (exit 2) | use a whole number with an optional `M`, `G` or `T` |
| `$HOME is not defined` (exit 1) | set `XDG_CACHE_HOME` or `HOME` in the service environment |
| `listen unix …: bind: …` (exit 1) | check the socket's directory is writable and the path is under 108 bytes |
| `usage: ecd <socket>` (exit 2) | pass exactly one argument |
| `open …/1000000.pack: file exists` (exit 1) | pack numbers ran out after about a million packs or restarts; wipe the store |

An abstract socket name (starting with `@`) does not work on Linux.

## ecd stopped by itself with status 0

Running out of file descriptors makes `ecd` shut down cleanly. Raise its file descriptor
limit, and run it with `Restart=always` rather than `on-failure`.

## ecd is slow to stop

On SIGTERM it waits for pending moves to the cold tier. A second SIGTERM is ignored. SIGKILL
is safe: the next start recovers.

## ec messages

| Message | Meaning |
|---|---|
| `ec: no etc/ekacache.json next to the binary and EKACACHE_CC unset` (1) | a compiler-named `ec` cannot find the real compiler; see [Hooking ec into a build](../builds/integration.md#option-2-a-compiler-named-symlink) |
| `ec: <name> called but etc/ekacache.json names no fc` (1) | a Fortran name with no Fortran compiler configured |
| `ec: cannot write <path>: <error>` (1) | a hit could not write its output |
| `ec: cannot execute <program>: <error>` (127) | launcher mode could not run the program |
| `ec: cache put <key> failed` | a result was not stored; the build is unaffected |
| exit 127 with no message | `ec slot` or C/C++ mode could not run the program, for example a wrong `EKACACHE_CC` |

## Builds behave differently with the cache

This should not happen, and is worth a bug report. Known limits are listed in
[Correctness and limits](../concepts/correctness.md). To rule the cache in or out, rebuild
with `EKACACHE_SOCK=` set to an empty string.
