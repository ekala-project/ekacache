# Conventions

The authoritative list is
[implementation/README.md §Cross-cutting rules](../implementation/README.md#cross-cutting-rules-for-every-phase).
In short:

## Behaviour

- **The behaviour docs are normative.** Current legacy behaviour is the contract, quirks
  included: stdout/stderr bytes, exit codes, wire bytes, key bits and on-disk files.
- **Quirks.** Each one is `### Q-AREA-n` in a behaviour doc. Code that reproduces a quirk
  carries `// Q-AREA-n` on that line, so `grep -rn 'Q-CC-12' crates` finds it.
- **Adopted changes** ([behaviors/README.md](../behaviors/README.md) §Adopted changes) are the
  only intended differences from legacy. Each one has a named test and a rule in
  [`tests/difftest/allow.toml`](../tests/difftest/allow.toml). Differential tests apply the
  rule at a line marked `ADOPTED Q-…`.
- **A proposed change is not implemented.** A quirk whose recommendation reads "change, needs
  sign-off" stays as legacy has it. Ask the user rather than quietly fixing it.

## Code

- **Byte strings end to end.** Paths, argv, env values, keys and manifest lines are
  `Vec<u8>`, `&[u8]` or `OsString`, because legacy never validates UTF-8. The helpers are in
  `ekacache_core::bytes`. Don't use `bstr`.
- **No panics on input.** `unwrap()` is only for real invariants, and `clippy::unwrap_used`
  warns about it outside tests.
- **Module doc comments cite the spec,** naming the behaviour-doc sections and, where useful,
  the legacy `file:line`.
- **ec stays synchronous.** It runs on the compile hot path, so it has no async runtime. ecd
  uses tokio.
- **Dependencies are a small vetted set** ([01-workspace.md](../implementation/01-workspace.md)),
  checked by `cargo deny`. Ask before adding one.
- **Lints:** clippy pedantic with `-D warnings`. Rust edition 2024.

## Names

The project and binaries are named ekacache, `ec` and `ecd`. Everything the outside world sees
keeps jig's names:

- the `JIG_*`/`JIGD_*` environment variables;
- the socket path `/nix/var/nix/jigd/socket`;
- `jig.json`;
- the usage and error text (`jig: …`);
- `jig` as an argv[0] alias of `ec`.

The behaviour, spec and plan docs still say jig/jigd. See
[implementation/README.md §Naming](../implementation/README.md#naming).

## Commits

- Messages use the form `area: summary`. The areas in use are `ec`, `ecd`, `core`, `tests`,
  `ci`, `implementation`, `rename` and similar.
- Make one commit per logical change, and keep the tree green at every commit.
- Don't edit `legacy/` unless the task is about legacy.
