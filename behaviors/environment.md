# Environment variables

This document lists every environment variable that jig (`jig/src`) or jigd (`jigd/`) reads,
sets or passes on, with where it is read, what it means, its default, how it is parsed, and
whether it reaches child processes. It also covers the variables jig sets for its children
(`JIG_SLOT`, `JIG_ABSENT_LOG`, `GOCACHEPROG`) and the ones that only enter cache keys. A Rust
implementation MUST read the same variables with the same parsing and defaults, MUST set the
same ones for children, and MUST hash the same ones into keys
([`jig-keys.md`](jig-keys.md)).

Sources: found with `grep -rn 'getenv\|Getenv\|LookupEnv\|"JIG\|"NIX_\|Env(\|environ\|setenv'
jig/src jigd` plus the indirect readers (`execvp`, `std::filesystem::temp_directory_path`,
`os.UserHomeDir`): `jig/src/base.cc` (`Env`, `OnPath`, `LogOutcome`), `jig/src/main.cc`,
`jig/src/store.cc`, `jig/src/process.cc`, `jig/src/cc_mode.cc`, `jig/src/rustc_mode.cc`,
`jig/src/driver.cc`, `jig/src/launcher.cc`, `jig/src/gocache_mode.cc`,
`jig/src/nix_store_mode.cc`, `jigd/main.go`. Test-only `setenv` calls in `jig/src/jig_test.cc`
are not behavior.

Key words MUST, MUST NOT, SHOULD, SHOULD NOT and MAY are as in RFC 2119. Where this document
says "current behavior", that behavior is normative: the Rust implementation MUST reproduce it
unless a Quirk entry's recommendation is accepted by the maintainer.

---

## Reading rules

jig reads every variable through `Env(name, fallback)` (`base.cc:104-107`): the value of
`getenv(name)`, or `fallback` (default `""`) **only when the variable is unset**. A variable set
to the empty string yields `""`, not the fallback (Q-ENV-1). "Non-empty" tests below mean
`Env(name) != ""`, so unset and empty are equivalent for them.

jig never clears or filters the environment. Every variable, read or not, is inherited by every
process jig starts: the compiler (`fork`+`execvp`, `process.cc:83`), and the `go` or other
program in launcher mode (`execv`, which replaces jig). In the table, **Propagated** therefore
says "inherited" unless jig changes the variable.

The store directory `JIG_STORE_DIR` is **not** an environment variable: it is a compile-time
macro (meson option `store_dir`, default `/nix/store`).

---

## jig

| Name | Read by | Meaning | Default | Parse rules | Propagated |
|---|---|---|---|---|---|
| `JIG_SOCK` | `main.cc:96` | path of the jigd socket | `<dirname(JIG_STORE_DIR)>/var/nix/jigd/socket` = `/nix/var/nix/jigd/socket` | verbatim. Set-but-empty → `""`, which never connects. Length ≥ 108 → treated as unreachable | inherited |
| `JIG_STORE_IDENTITY` | `store.cc:29` (once, at `Store` construction in `main`) | store identity mode | `path` | exactly `content` → content mode. Anything else (including `Content`, `""`) → path mode | inherited |
| `JIG_STORE_ROOTS` | `store.cc:37` | store roots the build may read, for resolving masked paths | `""` | content mode only. Split on single spaces, empty pieces dropped. Each cut to `<store>/<hash-name>`. Non-store entries ignored. Duplicate names: first wins | inherited |
| `out` | `store.cc:29` | this build's own output path, for the `eeee…` placeholder and as an extra root | `""` | read in both modes, used only in content mode. Hash taken only if it is `<store>/<32 bytes>-…` | inherited |
| `JIG_LOG` | `base.cc:322` (read once per process, `static`) | outcome log file | unset = no log | non-empty → append `<tool>\t<outcome>\t<subject>\t<ms>\n` (`jig-dispatch.md`) | inherited |
| `JIG_LOG_ARGS` | `cc_mode.cc:435` | log of uncached cc command lines | unset = no log | non-empty → append `<outcome>\t<args joined by " ">\n`, queries excluded | inherited |
| `JIG_SLOT` | `process.cc:30` | "a parent jig already holds a job slot" | unset | non-empty → `Slot` does nothing | **set** to `1` by jig while it holds a slot (`process.cc:40`), **unset** when released (`process.cc:47`). Children see `1` |
| `NIX_BUILD_TOP` | `process.cc:37` | build identity for slot fairness (`SLOT <build>`) | `-` when unset | verbatim. Empty or whitespace breaks the slot request (`jig-cache-client.md` Q-CLIENT-3). Excluded from link keys | inherited |
| `JIG_ABSENT_LOG` | not read. **Set** at `cc_mode.cc:345`, unset at `:347` | file where the patched clang appends lookups that missed, one per line | — | jig sets it to `<dir of -o or .>/.jig<pid>.absent` around every observed (cache-miss) compiler run, then reads, splits on `\n` and deletes the file. Written only by the project's patched clang. Stock GCC and clang ignore it, so there is no absent tracking (`jig-keys.md` Q-KEY-5) | **set** for the compiler child, then unset. Any outer value is overwritten, then removed for the rest of the process |
| `JIG_CC` | `driver.cc:429` | the compiler for cc mode when no `etc/jig.json` is found | unset = error | non-empty → conf-less driver with this compiler (`jig-driver-policy.md`) | inherited |
| `PKGS_CC` | `driver.cc:191` | per-toolchain-root extra `cflags`/`cxxflags`/`ldflags` (JSON) | unset = none | JSON object keyed by toolchain root, see `jig-driver-policy.md` | inherited |
| `PKGS_PREFIX_MAP` | `driver.cc:517` | extra `-ffile-prefix-map=` values | unset = none | split on `:`, empty pieces dropped, each → `-ffile-prefix-map=<piece>` (not for Fortran) | inherited |
| `LIBRARY_PATH` | `cc_mode.cc:117` | hashed into link keys | `""` | links only: field `LIBRARY_PATH=` + value, store hashes masked in content mode | inherited |
| `CPATH`, `C_INCLUDE_PATH`, `CPLUS_INCLUDE_PATH`, `OBJC_INCLUDE_PATH`, `GCC_EXEC_PREFIX`, `COMPILER_PATH` | `cc_mode.cc:557-560` via `environ` (`cc_mode.cc:89-95`) | hashed into every cc key (`env=` field) | absent = not hashed | whole `NAME=value` entries, sorted bytewise (`jig-keys.md`) | inherited |
| every `NIX_*` except the list below | `cc_mode.cc:569` via `environ` | hashed into link keys: inputs of the nixpkgs bintools wrapper, which runs after jig in launcher mode | absent = not hashed | links only. Excluded: `NIX_BUILD_TOP`, `NIX_BUILD_CORES`, `NIX_LOG_FD`, `NIX_ATTRS_JSON_FILE`, `NIX_ATTRS_SH_FILE`, `NIX_SSL_CERT_FILE`, `NIX_REMOTE`, `NIX_PATH`, `NIX_PROFILES`, `NIX_USER_PROFILE_DIR` | inherited |
| `CARGO_PKG_NAME`, `CARGO_PKG_VERSION`, `CARGO_CFG_TARGET_FEATURE`, `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS` | `rustc_mode.cc:313-315` | hashed into rustc keys, in this order | `""` | field `NAME=value`. Unset and empty are the same | inherited |
| `TMPDIR` | `cc_mode.cc:500`, `gocache_mode.cc:206` | where `-E` without `-o` puts its temp output (`$TMPDIR/jig<pid>.i`), and the default gocacheprog dir | `/tmp` when unset | verbatim. Empty → `jig<pid>.i` relative to the cwd, and `/gocacheprog` at the file-system root | inherited |
| `TMPDIR`, `TMP`, `TEMP`, `TEMPDIR` | `cc_mode.cc:372` (`std::filesystem::temp_directory_path`, libstdc++) | which `--dependency-file` entries are the driver's temp objects | first one set, else `/tmp` | libstdc++ rules. An error gives `""`, so the prefix becomes `/` | inherited |
| `GOCACHEPROG` | `launcher.cc:91` | the build's own Go cache program | unset | launcher `go` only: if empty and the socket path exists, **set** to `<self> gocacheprog` before `execv` (`launcher.cc:93`) | set as described, else inherited |
| `GOCACHEPROG_DIR` | `gocache_mode.cc:206` | gocacheprog's on-disk object directory | `$TMPDIR/gocacheprog` (`/tmp/gocacheprog`) | verbatim, created with `create_directories` | inherited |
| `NIX_REMOTE` | `nix_store_mode.cc:329` | builder-rpc daemon socket for `jig nix-store` | — | `unix://<path>`, see `jig-nix-store.md` | inherited |
| `JIG_NIX_STORE_OFFLINE` | `nix_store_mode.cc:569` | `jig nix-store` offline mode | unset = online | non-empty → offline, see `jig-nix-store.md` Q-NIX-3 | inherited |
| `PATH` | `base.cc:111` (`OnPath`), `process.cc:83` (`execvp`) | locating `rustc` / the compiler for `ToolId`, `toolchain=`, `stdlib=`; the program to run | — | `OnPath`: split on `:`, empty pieces dropped, first `dir/name` that `exists`. Only for names without `/`. Launcher `execv` does no `PATH` search | inherited |

## jigd

All read once at startup in `main` (`jigd/main.go:175-214`).

| Name | Read by | Meaning | Default | Parse rules | Propagated |
|---|---|---|---|---|---|
| `XDG_CACHE_HOME` | `main.go:185` | parent of the hot pack directory `$XDG_CACHE_HOME/jigd/packs` | `$HOME/.cache` | empty = unset. If `$HOME` cannot be determined either, fatal | n/a (jigd starts no children) |
| `HOME` | `main.go:187` (`os.UserHomeDir`) | fallback for the above | — | Go rules | n/a |
| `JIGD_SIZE` | `main.go:199` (`gib`, `main.go:193-198`) | hot tier budget in GiB | 50 | `strconv.ParseInt(v, 10, 64)`. Any parse failure (empty, `50G`, spaces) → default, silently. Value `<< 30` | n/a |
| `JIGD_COLD` | `main.go:200` | directory of a second, cold tier | unset = no cold tier | non-empty → tier added | n/a |
| `JIGD_COLD_SIZE` | `main.go:201` | cold tier budget in GiB | 0 = unbounded | as `JIGD_SIZE`. Read only when `JIGD_COLD` is non-empty | n/a |
| `JIGD_SLOTS` | `main.go:204` | host-wide concurrent compiler slots | `runtime.NumCPU()` | `strconv.Atoi`, used only if it parses and is > 0 | n/a |
| `NIX_STORE_DIR` | `main.go:210` | the only tree `IDS` answers for | `/nix/store` | empty = unset. Cleaned, and also resolved through symlinks | n/a |

A budget ≤ 0 means unbounded for **any** tier (`store.go:324`, `store.go:363`), including the hot
one.

---

## Variables jig sets for children (summary)

| Name | Value | Lifetime | Purpose |
|---|---|---|---|
| `JIG_SLOT` | `1` | while a `Slot` guard holds a token | a nested jig (rustc → cc for linking, a compiler calling jig) must not wait for a second slot (`spec/slots.qnt`, `slotNesting`) |
| `JIG_ABSENT_LOG` | `<outdir>/.jig<pid>.absent` | around each cache-miss compiler run in cc mode | the patched clang records missed lookups |
| `GOCACHEPROG` | `<self> gocacheprog` | the exec'd go process | route go's build cache through jigd |

---

## Quirks

### Q-ENV-1: Fallbacks apply only when a variable is unset

**Current behavior**: `Env(name, fallback)` returns `""` for a variable set to the empty string.
`JIG_SOCK=` gives socket path `""` (no cache), `NIX_BUILD_TOP=` gives `SLOT \n` (daemon hangs
up), `TMPDIR=` puts the `-E` temp file in the cwd and the gocacheprog dir at `/gocacheprog`, `GOCACHEPROG_DIR=` gives the cwd. jigd treats empty as
unset for `XDG_CACHE_HOME`, `JIGD_COLD` and `NIX_STORE_DIR`.

**Impact**: an exported-but-empty variable silently disables features or puts files in the cwd.

**Recommendation**: keep as-is. A Rust port MUST distinguish unset from empty exactly like this
(`std::env::var_os(...)` returning `Some("")` is not "unset").

### Q-ENV-2: `JIG_STORE_IDENTITY` is an exact match

**Current behavior**: only the exact string `content` selects content mode. Anything else
silently means path mode.

**Impact**: a typo changes every key and disables masking without a message.

**Recommendation**: keep as-is.

### Q-ENV-3: jigd size parsing is silent and unchecked

**Current behavior**: an unparsable `JIGD_SIZE`/`JIGD_COLD_SIZE` silently falls back to the
default. `0` or a negative value means unbounded (also for the hot tier, although the usage text
documents 0 = unbounded only for `JIGD_COLD_SIZE`). Values ≥ 2³³ overflow the `<< 30` shift.
`JIGD_COLD_SIZE` without `JIGD_COLD` is ignored.

**Impact**: `JIGD_SIZE=50G` gives 50 GiB by luck, `JIGD_SIZE=0` turns off eviction.

**Recommendation**: **ADOPTED (signed off 2026-11-17).** See `jigd-daemon.md` Q-DAEMON-1: strict parsing with M/G/T suffixes; invalid values exit 2.

### Q-ENV-4: `JIG_ABSENT_LOG` is set for every compiler, read only by one

**Current behavior**: jig sets `JIG_ABSENT_LOG` for GCC and stock clang too. They ignore it, so
`!` lines are never produced for them. A value inherited from the outside is overwritten and then
removed from jig's environment.

**Impact**: soundness caveat for stock compilers (`jig-keys.md` Q-KEY-5). No message warns that
absent tracking is off.

**Recommendation**: keep as-is. Consider logging, once per build, when no absent log appeared
after a successful compile (needs sign-off).

### Q-ENV-5: Any inherited `JIG_SLOT` disables admission

**Current behavior**: `JIG_SLOT` is a non-empty test. A value leaking in from an outer shell or
a build environment (`JIG_SLOT=0` included) makes every jig skip the slot.

**Impact**: host-wide compiler admission silently stops working.

**Recommendation**: keep as-is.

### Q-ENV-6: Unset and empty rustc variables hash the same

**Current behavior**: `CARGO_PKG_NAME=` and an unset `CARGO_PKG_NAME` both hash as
`CARGO_PKG_NAME=`.

**Impact**: none known (cargo always sets them).

**Recommendation**: keep as-is.

### Q-ENV-7: `NIX_STORE_DIR` (jigd) and `JIG_STORE_DIR` (jig) are configured independently

**Current behavior**: jigd learns its store from the environment, jig from a compile-time
constant. If they differ, jigd answers `""` to every `IDS` question and jig hashes everything
locally.

**Impact**: only speed. No error is reported.

**Recommendation**: keep as-is.

### Q-ENV-8: Link keys hash every unlisted `NIX_*` variable

**Current behavior**: any `NIX_*` variable not on the exclusion list enters link keys, including
debugging switches (`NIX_DEBUG`) and any per-build variable a future stdenv may add.

**Impact**: extra misses for links. Never a false hit.

**Recommendation**: keep as-is. The exclusion list is part of the key format.

---

## Open questions

1. `JIG_ABSENT_LOG`: what exactly does the patched clang write: absolute or as-looked-up paths,
   and with what escaping? (Also `jig-keys.md` open question 1.)
2. Should `SOURCE_DATE_EPOCH` be documented as deliberately **not** keyed? cc mode instead
   refuses to store translation units that mention `__DATE__`, `__TIME__` or `__TIMESTAMP__`
   (`cc_mode.cc:391-397`, `cc_mode.cc:689`).
3. `HOME` fallback for jigd: is a fatal exit acceptable when neither `XDG_CACHE_HOME` nor `HOME`
   is available (for example under a systemd unit without `HOME`)?
