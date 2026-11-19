# Recipes

## Fix a mismatch with legacy

1. Find the behaviour doc section and any `Q-…` quirk that covers the case.
2. If the doc and legacy disagree, or the doc doesn't cover the case, stop and report. Don't
   guess.
3. Reproduce the case as a test. Use a differential test (`*difftest*.rs` or `legacy_jig.rs`)
   when legacy can run it, and a vector or unit test otherwise.
4. Fix the code and mark quirk lines with `// Q-AREA-n`.

## Add or change intended behaviour

Behaviour changes need sign-off. Once a change is adopted:

1. Add a row to [behaviors/README.md §Adopted changes](../behaviors/README.md) and update the
   doc that specifies the behaviour.
2. Add a `[[rule]]` to [`tests/difftest/allow.toml`](../tests/difftest/allow.toml) that names
   the row and the tests.
3. Write the tests, and mark the line in the differential test where the rule applies with
   `ADOPTED Q-…`.
4. Run `scripts/check-allow-list.sh`.

## Add a test vector

Vectors from a behaviour doc go in `tests/vectors/<doc>.toml`. Each entry's `section` names
the doc heading it comes from. `scripts/check-vectors.sh` checks the coverage.

## Port a legacy test

A C++ `TestFooBar` or Go `TestFooBar` becomes `fn test_foo_bar` or `fn foo_bar` somewhere under
`crates/`. `scripts/check-legacy-ports.sh` lists any that are missing.

## Touch concurrent code (slots, store, protocol, two-key, gocache, builder RPC)

1. Check the Quint model in [`spec/`](../spec/README.md).
2. If the model changes, regenerate traces with `scripts/gen-itf.sh` and run the `*_itf.rs`
   replays.
3. A replay that finds no traces passes but says so. CI treats that as a failure.

## Platform-specific behaviour

- Use `cfg(target_os = "linux")` or `cfg(target_vendor = "apple")`, and cite the quirk or doc
  section that says the platforms differ.
- Run `scripts/check-macos.sh` from Linux. It type-checks and lints both Apple targets,
  including the tests, but does not run them. The macOS CI jobs run them.

## Where to look when stuck

- [implementation/STATUS.md](../implementation/STATUS.md): known differences, decisions and
  results.
- The legacy source in `legacy/`: behaviour docs cite it as `jig/src/x.cc:line`, which now
  lives at `legacy/jig/src/x.cc`.
- [behaviors/environment.md](../behaviors/environment.md): every environment variable.
- [behaviors/ecosystems.md](../behaviors/ecosystems.md): how other toolchains interact with the
  cache.
