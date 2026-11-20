# Security and trust

> **Any build that can reach the socket can write entries that other builds will replay.**
> Run `ecd` only on machines where you trust everyone who can start a build.

## Why this matters

`ecd` stores whatever `ec` sends it. A build that can talk to the socket can send a
crafted object file under a key that another derivation will compute later. That derivation
then links the crafted object without running the compiler. Nix's own guarantees do not
cover this: the attacking build and the victim build can be completely unrelated
derivations.

Other consequences of the same trust:

- Entries carry no checksums or signatures.
- A directory listing fetched by `ec cache get-dir` is trusted as written. A crafted listing
  can write files outside the target directory.
- `ecd` has no connection limits or timeouts, so any peer that is allowed in can tie it
  up.

In short, treat the socket as granting write access to the outputs of every future build
on the machine.

## Who may connect (Linux)

When a connection arrives, `ecd` checks the peer's credentials (`SO_PEERCRED`) once, before
reading anything. It accepts the peer if any of these holds:

| Rule | Covers |
|---|---|
| uid is `ecd`'s own uid | the service user, and `ec cache` run by that user |
| uid is 0 | root |
| primary gid is the `nixbld` group | classic Nix build users (`nixbld1`, …) |
| uid in 872415232 to 872415232 + 8388607 | Nix's `auto-allocate-uids` default range |

Everyone else is disconnected silently. `ec` sees that as "no daemon" and compiles
uncached.

Limits of the check:

- Only the **primary** group counts. Supplementary groups are ignored.
- The `auto-allocate-uids` range is fixed. If you set `start-id` or `id-count` in
  `nix.conf`, those build users are refused.
- The `nixbld` group is looked up once at startup. If you create it later, restart `ecd`.

## macOS

On macOS, `ecd` does **no** peer check. Any local user who can reach the socket is served.
The only protection is the permission on the socket's directory, so restrict that
directory accordingly.

## Recommendations

- Run `ecd` only on single-tenant build machines, or where all users who can submit builds
  are trusted equally.
- Don't expose the socket to remote builders that accept builds from untrusted users.
- Run `ecd` as a dedicated user, not your own account, so that your interactive processes
  are not on the allow list.
- If a cache might have been poisoned, stop `ecd` and delete the store (see
  [The store on disk](../operating/store.md#wiping-the-cache)).
