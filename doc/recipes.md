# Recipes

## Fix a mismatch with the spec

1. Find the behaviour doc section and any `Q-…` quirk that covers the case.
2. If the doc doesn't cover the case, stop and report. Don't guess.
3. Reproduce the case as a test: a vector or a unit or integration test.
4. Fix the code and mark quirk lines with `// Q-AREA-n`.

## Add or change intended behaviour

Behaviour changes need sign-off. Once a change is adopted:

1. Add a row to [behaviors/README.md §Adopted changes](../behaviors/README.md) and update the
   doc that specifies the behaviour.
2. Write the tests, and mark the lines that implement the change with `ADOPTED Q-…`.

## Add a test vector

Vectors from a behaviour doc go in `tests/vectors/<doc>.toml`. Each entry's `section` names
the doc heading it comes from. `scripts/check-vectors.sh` checks the coverage.

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
- The original jig/jigd source: behaviour docs cite it as `jig/src/x.cc:line`. It was removed
  from this repository; the last commit with it under `legacy/` is `86ea083`.
- [behaviors/environment.md](../behaviors/environment.md): every environment variable.
- [behaviors/ecosystems.md](../behaviors/ecosystems.md): how other toolchains interact with the
  cache.
