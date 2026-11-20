# Rust

## Setting it up

Point cargo's `RUSTC_WRAPPER` at the `rustcwrap` symlink:

```console
$ export RUSTC_WRAPPER=$ekacache/libexec/ekacache/rustcwrap
$ cargo build
```

cargo then runs `rustcwrap <rustc> <args…>`. Launcher mode also works:
`ec /path/to/rustc …`.

## What is cached

`ec` caches a rustc invocation when all of these hold:

- the crate type is `lib`, `rlib` or `staticlib`;
- there is exactly one `.rs` source;
- `--out-dir` is given and `--emit` includes `dep-info` (cargo does both);
- there is no `-o` and no `-C incremental=`.

So library crates, including all dependencies from crates.io, are cached. These are not:

- binaries, tests, build scripts and anything without `--crate-type`;
- `proc-macro`, `cdylib` and `dylib` crates;
- a comma list containing any of those, such as `--crate-type lib,cdylib`;
- queries such as `--print`, `--version` and `-vV`;
- **failing crates**: a crate that fails to compile is compiled again every time.

Release profiles usually set `incremental = false`. A debug profile with incremental
compilation passes `-C incremental=…`, which makes every crate uncacheable.

## What is in the key

- the rustc binary and the standard library rlibs;
- the working directory, source path and arguments, minus `--out-dir`, `-L`,
  `-C metadata` and `-C extra-filename`;
- the identities of every `--extern` file;
- `CARGO_PKG_NAME`, `CARGO_PKG_VERSION`, `CARGO_CFG_TARGET_FEATURE`, `RUSTFLAGS` and
  `CARGO_ENCODED_RUSTFLAGS`;
- environment variables the crate reads with `env!` or `option_env!`, as recorded in
  rustc's dep-info. A change shows as the miss reason `env-changed:NAME`.

## On a hit

`ec` restores every output named in the dep-info into `--out-dir`, renamed to the current
`-C extra-filename`, prints the stored stderr, and exits 0. The restored `.d` file and the
rlib's embedded metadata are those of the run that stored the entry.
