# AGENTS.md

ekacache is a compile cache for Nix builds. It is the Rust rewrite of jig (C++) and jigd (Go)
and has two binaries: `ec`, the compiler launcher, and `ecd`, the per-machine daemon. The
legacy code stays in `legacy/` until the cutover.

## Read first

- [`doc/`](doc/README.md): high-level guides for agents covering the architecture, how to
  develop and test, conventions, and recipes for common changes.
- [`behaviors/`](behaviors/README.md): the **normative** behaviour spec, quirks included. If
  code and a behaviour doc disagree, the doc wins.
- [`implementation/`](implementation/README.md): the plan for the port, with crate design per
  phase, the test layers ([12](implementation/12-testing.md)) and the rollout
  ([13](implementation/13-rollout.md)). [`STATUS.md`](implementation/STATUS.md) says what is
  done.
- [`spec/`](spec/README.md): Quint models of the concurrent parts.

## Ground rules

1. **Be byte-compatible with legacy.** This covers the wire format, keys, on-disk layout,
   stdout/stderr, exit codes and the environment. The only intended differences are listed in
   [`behaviors/README.md` §Adopted changes](behaviors/README.md), and each one has a rule in
   [`tests/difftest/allow.toml`](tests/difftest/allow.toml).
2. **Reproduce quirks; don't fix them.** Put a `// Q-AREA-n` comment at the line that
   reproduces a quirk. A proposed change (marked "needs sign-off") is not implemented until it
   is adopted. If a quirk looks unreasonable to keep, ask.
3. **Use byte strings, not `String`,** for paths, argv, env, keys and manifests.
4. **No panics on input.** Bad input gets the documented message and exit code.
5. **Cite the spec.** Every module's doc comment names the behaviour-doc sections it
   implements.
6. **Keep the jig-era external names.** This means `JIG_*`/`JIGD_*`, `/nix/var/nix/jigd/socket`,
   `jig.json` and the `jig: …` messages. Leave `legacy/`, `behaviors/` and `spec/` as they are
   unless the task is about them.

## Commands

Run everything inside `nix-shell`:

```console
$ nix-shell --run 'cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings'
$ nix-shell --run 'cargo test --workspace'
$ nix-shell --run scripts/check-macos.sh     # type-check the macOS cfg branches from Linux
```

The differential and end-to-end tests skip unless `JIG_LEGACY_JIG`, `JIG_LEGACY_JIGD`,
`EC_RUST_ECD`, `JIG_TEST_GCC` and `JIG_TEST_CLANG` are set. See
[`doc/development.md`](doc/development.md).

## Commits

Commit messages use the form `area: summary`, e.g. `tests: …`, `ci: …` or `ecd: …`. Make one
commit per logical change.
