# ec nix-store

Add store objects and derivations from inside a Nix sandbox, without a `nix` binary there.

```text
usage: ec nix-store add-text|add-drv|fetchurls|submit …
```

`ec nix-store` is a small client for the Nix worker protocol. It is meant for *dynamic
derivations*: builds that compute `.drv` files themselves, add them to the store, and assign
one as a build output.

> **Requirement.** Online use needs a Nix daemon with the *builder-rpc-v0* feature, reachable
> through `NIX_REMOTE=unix://<path>` inside the sandbox. Stock Nix does not provide it.
> Without it, use offline mode.

## Verbs

| Verb | Arguments | stdin | stdout |
|---|---|---|---|
| `add-text` | `<name> [<ref>…]` | the object's bytes | its store path |
| `add-drv` | `<name> [<ref>…]` | derivation JSON | the `.drv` store path |
| `fetchurls` | none | a JSON array of `{name, url, algo, hex, sri}` | `[{"drv":"…","out":"…"},…]` |
| `submit` | `<store-path> <output>` | not read | nothing |

### add-drv

The JSON follows the shape of `nix derivation show`:

```json
{
  "system": "x86_64-linux",
  "builder": "/nix/store/…-bash/bin/bash",
  "args": ["-c", "…"],
  "env": { "out": "…" },
  "inputDrvs": { "/nix/store/…-dep.drv": ["out"] },
  "inputSrcs": ["/nix/store/…-src"],
  "outputs": { "out": {} }
}
```

- The store name comes from the `<name>` argument; a `"name"` field in the JSON is ignored.
- References are **not** taken from `inputDrvs` and `inputSrcs`. Pass every referenced store
  path as a `<ref>` argument.

### fetchurls

Each element becomes a `builtin:fetchurl` fixed-output derivation, added as `<name>.drv`.
The output lists each derivation and its output path, in order. An empty array prints `[]`.

```console
$ echo '[{"name":"src.tar.gz","url":"https://example.org/src.tar.gz","algo":"sha256","hex":"…","sri":"sha256-…"}]' \
    | ec nix-store fetchurls
```

If element *k* fails, nothing is printed, but elements before it have already been added.
Running it again is safe.

## Offline mode

With `EKACACHE_NIX_STORE_OFFLINE` set to any non-empty value (including `0` or `false`),
`ec nix-store` computes the same store paths locally and contacts no daemon. `submit`
always succeeds offline.

```console
$ printf hello | EKACACHE_NIX_STORE_OFFLINE=1 ec nix-store add-text hello.txt
/nix/store/q790zdjk75hm2cn42nh77pqw4gbv1b88-hello.txt
```

## Environment

| Variable | Meaning |
|---|---|
| `NIX_REMOTE` | must be exactly `unix://<path>`; `daemon`, `auto` and query parameters are not supported |
| `EKACACHE_NIX_STORE_OFFLINE` | non-empty: offline mode |

## Exit status and messages

| Exit | Cause |
|---|---|
| 0 | success |
| 1 | the store could not be opened, or the operation failed |
| 2 | no arguments (usage), or an unknown verb or wrong argument count (`ec nix-store: bad arguments`) |

The connection is opened **before** the arguments are checked, so a mistyped verb in a
sandbox without a working `NIX_REMOTE` reports the connection error (exit 1), not
`bad arguments`.

Messages, on stderr:

- `ec nix-store: NIX_REMOTE=<value> is not a unix:// socket (builder-rpc-v0 missing?)`
- `ec nix-store: cannot connect to <path>`
- `ec nix-store: bad daemon magic`
- `ec nix-store: daemon protocol <n> too old`
- `nix-daemon: <message>`, an error from the daemon
- `ec nix-store: derivation JSON did not parse`
- `ec nix-store fetchurls: expected a JSON array`

JSON with fields of the wrong type (for example `null` for `args`) aborts `ec` without a
message.
