# AGENTS.md

ekacache is a compile cache for Nix builds. It was ported to Rust from jig (C++) and jigd (Go)
and has two binaries: `ec`, the compiler launcher, and `ecd`, the per-machine daemon. The
legacy code and compatibility with jig deployments are gone.

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
- [`book/`](book/src/SUMMARY.md): the user-facing docs, published to GitHub Pages. Update it
  when a change is visible to users.

## Ground rules

1. **Follow the behaviour docs byte for byte.** This covers the wire format, keys, on-disk
   layout, stdout/stderr, exit codes and the environment. Changes from the specified behaviour
   are listed in [`behaviors/README.md` §Adopted changes](behaviors/README.md), each with tests.
2. **Reproduce quirks; don't fix them.** Put a `// Q-AREA-n` comment at the line that
   reproduces a quirk. A proposed change (marked "needs sign-off") is not implemented until it
   is adopted. If a quirk looks unreasonable to keep, ask.
3. **Use byte strings, not `String`,** for paths, argv, env, keys and manifests.
4. **No panics on input.** Bad input gets the documented message and exit code.
5. **Cite the spec.** Every module's doc comment names the behaviour-doc sections it
   implements.
6. **Use the ekacache external names.** This means `EKACACHE_*`/`EKACACHED_*`,
   `/nix/var/nix/ekacache/socket`, `$XDG_CACHE_HOME/ekacache/packs`, `etc/ekacache.json` and the
   `ec: …`/`ecd: …` messages. There is no `jig` command or alias. Leave `behaviors/` and `spec/`
   as they are unless the task is about them.

## Commands

Run everything inside `nix-shell`:

```console
$ nix-shell --run 'cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings'
$ nix-shell --run 'cargo test --workspace'
$ nix-shell --run scripts/check-macos.sh     # type-check the macOS cfg branches from Linux
```

The end-to-end tests skip unless `EC_RUST_ECD`, `EKACACHE_TEST_GCC` and `EKACACHE_TEST_CLANG`
are set. See
[`doc/development.md`](doc/development.md).

## Commits

Commit messages use the form `area: summary`, e.g. `tests: …`, `ci: …` or `ecd: …`. Make one
commit per logical change.
