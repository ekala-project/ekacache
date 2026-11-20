# Conventions

The authoritative list is
[implementation/README.md §Cross-cutting rules](../implementation/README.md#cross-cutting-rules-for-every-phase).
In short:

## Behaviour

- **The behaviour docs are normative.** The specified behaviour is the contract, quirks
  included: stdout/stderr bytes, exit codes, wire bytes, key bits and on-disk files.
- **Quirks.** Each one is `### Q-AREA-n` in a behaviour doc. Code that reproduces a quirk
  carries `// Q-AREA-n` on that line, so `grep -rn 'Q-CC-12' crates` finds it.
- **Adopted changes** ([behaviors/README.md](../behaviors/README.md) §Adopted changes) are the
  only intended differences from the specified current behaviour. Each one has a named test,
  and the code that implements it is marked `ADOPTED Q-…`.
- **A proposed change is not implemented.** A quirk whose recommendation reads "change, needs
  sign-off" stays as the doc's current behaviour. Ask the user rather than quietly fixing it.

## Code

- **Byte strings end to end.** Paths, argv, env values, keys and manifest lines are
  `Vec<u8>`, `&[u8]` or `OsString`, because the spec never validates UTF-8. The helpers are in
  `ekacache_core::bytes`. Don't use `bstr`.
- **No panics on input.** `unwrap()` is only for real invariants, and `clippy::unwrap_used`
  warns about it outside tests.
- **Module doc comments cite the spec,** naming the behaviour-doc sections and, where useful,
  the original jig/jigd `file:line`.
- **ec stays synchronous.** It runs on the compile hot path, so it has no async runtime. ecd
  uses tokio.
- **Dependencies are a small vetted set** ([01-workspace.md](../implementation/01-workspace.md)),
  checked by `cargo deny`. Ask before adding one.
- **Lints:** clippy pedantic with `-D warnings`. Rust edition 2024.

## Names

The project and binaries are named ekacache, `ec` and `ecd`, and so is everything the outside
world sees:

- the `EKACACHE_*`/`EKACACHED_*` environment variables (`EKACACHE_STORE_DIR` at build time);
- the socket path `/nix/var/nix/ekacache/socket` and the packs in `$XDG_CACHE_HOME/ekacache/packs`;
- `etc/ekacache.json`;
- the usage and error text (`ec: …`, `ecd: …`).

`ec` answers only to `ec` as its own argv[0] name; there is no `jig` alias. Behaviour docs
still cite the original jig/jigd sources as `jig/src/x.cc:line`.

## Commits

- Messages use the form `area: summary`. The areas in use are `ec`, `ecd`, `core`, `tests`,
  `ci`, `implementation`, `rename` and similar.
- Make one commit per logical change, and keep the tree green at every commit.
