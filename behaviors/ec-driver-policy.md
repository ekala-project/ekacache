# ec driver policy (the "repkgs driver")

This document is normative for how ec, invoked as a C, C++ or Fortran compiler driver, turns the
build system's command line into the command line it executes for the real compiler. It covers the
toolchain configuration file `etc/ekacache.json`, the per-package flags from `$PKGS_CC`, the
`$PKGS_PREFIX_MAP` mappings, the conf-less `$EKACACHE_CC` fallback, and the per-binary-format link policy:
the ELF RUNPATH with its slack for `ec reloc-fixup`, the dynamic linker and `crt_interp` stub,
Mach-O header padding, and COFF argument rewrites. A Rust reimplementation MUST produce
byte-identical argument vectors, messages and exit statuses for the same inputs, oddities included.
Caching, classification and cache keys are out of scope (see `behaviors/ec-cc-mode.md`). This
document states only the interface cc mode uses.

Sources: `jig/src/driver.h`, `jig/src/driver.cc`, `jig/src/cc_mode.cc` (call sites in `RunCcMode`
and `RunUncached` only), `jig/src/base.cc` (`Split`, `Env`, `ReadFile`), `jig/src/store.cc`
(`Store::IsStorePath`), `jig/src/jig_test.cc` (`TestDriverConf`, `TestDriverPackageFlags`,
`TestDriverLink`, `TestSearchDirs`), `meson.build` (`-fno-exceptions`).

## Overview

### Terms

- **user args**: the build system's arguments after response-file expansion (cc mode expands
  `@file` before anything in this document runs; see `behaviors/ec-cc-mode.md`).
- **conf**: a `DriverConf` value. It is either *present* (it came from `etc/ekacache.json`) or *conf-less*
  (it carries only a compiler path, from `$EKACACHE_CC` or a launcher's program).
- **driver args**: the vector `BuildDriverArgs` returns. cc mode executes the compiler with it
  (`argv[0]` handling is in `behaviors/ec-cc-mode.md`).
- **binfmt**: one of `elf`, `macho`, `coff`. It selects the per-format policy.
- **store dir**: the compile-time constant `EKACACHE_STORE_DIR` (e.g. `/nix/store`). A path *is a store
  path* when it is longer than the store dir, starts with the store dir, and the byte right after the
  store dir is `/` (`Store::IsStorePath`). The check is lexical.
- **host dir**: a path whose bytes start with `/usr`, `/lib` or `/opt`. This is a plain prefix test
  with no component boundary: `/lib64`, `/library` and `/optx` are host dirs too.

### Pipeline

1. cc mode obtains a conf: `LoadDriverConf()` normally, or `ConfLessDriverConf(program)` in launcher
   mode.
2. cc mode picks the language from the invoked name: C, C++ or Fortran.
3. When the conf is present, the driver args are `BuildDriverArgs(conf, lang, user_args)`.
   Otherwise they are the user args unchanged.
4. When the user args contain the exact word `-print-search-dirs`, cc mode passes the compiler's
   stdout through `WithPackageLibDirs` before printing it.

### Interface to cc mode (boundary contract)

`RunCcMode(argv0, raw_args, socket_path, launched)` does the following, in this order:

1. `user_args = ExpandResponseFiles(raw_args)`.
2. `conf = launched.empty() ? LoadDriverConf() : ConfLessDriverConf(launched)`. `LoadDriverConf`
   MAY terminate the process itself (exit 2 or SIGABRT, see [Errors and messages](#errors-and-messages)).
   In launcher mode `etc/ekacache.json` and `$PKGS_CC` are never read.
3. If `conf` is none, cc mode writes `ec: no etc/ekacache.json next to the binary and EKACACHE_CC unset\n` to
   stderr and returns 1.
4. `name` = the final path component of `argv0`. In `ec <mode> args…` form, `argv0` is the mode word.
   In launcher mode it is the basename of the launched program. Language selection is
   case-sensitive and checks the substrings in this order:
   - `name` contains `++` → C++ (`Language::kCxx`);
   - otherwise `name` contains `fortran` or `flang` → Fortran (`Language::kFortran`);
   - otherwise C (`Language::kC`).

   Examples: `c++`, `g++`, `clang++-21`, `x86_64-linux-gnu-g++` are C++. `gfortran`, `flang`,
   `flang-new` are Fortran. `cc`, `gcc`, `clang` are C. A name like `flang++` is C++.
5. If the language is Fortran and `conf.fc` is empty, cc mode writes
   `ec: <name> called but etc/ekacache.json names no fc\n` to stderr and returns 1. A conf-less conf
   always has an empty `fc`.
6. The compiler executed is `conf.fc` for Fortran, else `conf.cc`.
7. cc mode marks a link uncacheable when `conf.binfmt != elf`, and marks every Fortran invocation
   uncacheable. Details are in `behaviors/ec-cc-mode.md`.
8. If `conf.present`: `inv.args = BuildDriverArgs(conf, lang, user_args)`. Then, in output order,
   every element of `inv.args` that is not byte-equal to **some** element of `user_args` is appended
   to the cache-key argument list (`inv.key_args`). This is a membership test, not a positional one,
   and repeats are appended repeatedly. If not present: `inv.args = user_args`.
9. On an uncached run where `user_args` contains the exact element `-print-search-dirs`, cc mode runs
   the compiler with `inv.args`, captures its **stdout**, writes
   `WithPackageLibDirs(captured, ldflags)` to stdout, flushes, and returns the compiler's status.
   `ldflags` is `conf.package.ldflags` when `conf.present`, else the empty list. The function is
   called even with an empty list, which normalises the trailing newline (A13).

Exported API that the Rust port MUST mirror for its own unit tests. Names may be idiomatic, but the
semantics MUST match:

| C++ | Semantics |
|---|---|
| `LoadDriverConf() -> optional<DriverConf>` | §Configuration: locate, parse, `$PKGS_CC`, `$EKACACHE_CC` fallback |
| `ConfLessDriverConf(compiler) -> DriverConf` | conf-less conf (see `$EKACACHE_CC`) |
| `ParseDriverConf(text, root = "") -> DriverConf` | parse ekacache.json text. `root` is what `@/` in `fflags` expands to |
| `IsSharedLibName(basename) -> bool` | A10 |
| `WithPackageLibDirs(search_dirs, ldflags) -> string` | A13 |
| `BuildDriverArgs(conf, lang, raw_args) -> vector<string>` | A2 |
| constants `kRunpathSlack = 48`, `kNeededSlack = 80`, `kInterpSlack = 12` | A8, A6 |

`DriverConf` fields and defaults: `cc = ""`, `fc = ""`, `binfmt = elf`, `libc = ""`,
`interp = "ld-linux-x86-64.so.2"`, `crt = ""`, `runtimes = ""`, `flags = []`, `cxxflags = []`,
`fflags = []`, `prefix_map = []`, `package = {cflags: [], cxxflags: [], ldflags: []}`,
`present = false`.

### Notation used in this document

- Strings are byte strings. "starts with" and "ends with" are byte-prefix and byte-suffix tests.
- In test vectors, the token `_×N` inside a quoted string stands for exactly N `_` (0x5F) bytes.
- `META` is the literal argument `-Wl,--package-metadata={"type":"repkgs"}` (in JSON:
  `"-Wl,--package-metadata={\"type\":\"repkgs\"}"`).

## Configuration

### Locating `etc/ekacache.json`

`LoadDriverConf()` MUST behave as follows:

1. Read the symlink `/proc/self/exe` (`readlink(2)` semantics, `std::filesystem::read_symlink`). On
   Linux the kernel resolves symlinks, so this names the real executable file, not the name it was
   invoked by. A `cc -> /x/bin/ec` symlink elsewhere uses `/x/etc/ekacache.json`.
   - If reading the link fails (no procfs, e.g. any non-Linux host), skip to step 5.
2. `root` = the parent of the parent of the link target. Example: `/nix/store/h-cc/bin/ec` →
   `/nix/store/h-cc`. This is `std::filesystem::path::parent_path` twice, so `/ec` gives root `/`.
3. Read the file `root + "/etc/ekacache.json"`. If it cannot be opened or read (missing, permission
   denied, is a directory, read error), skip to step 5. An empty file is *read successfully* as
   the empty text.
4. `conf = ParseDriverConf(text, root)`, which MAY exit (see below).
   - If `conf.cc` is empty, return **none**. `$EKACACHE_CC` is **not** consulted (Q-DRV-1).
   - Otherwise `conf.package = PackageFlags(root)` (see `$PKGS_CC`) and return `conf`.
5. `$EKACACHE_CC`: if unset or empty, return none. Otherwise return `ConfLessDriverConf($EKACACHE_CC)`.

### ekacache.json schema

The file MUST be a single JSON value parsed with the semantics of
`nlohmann::json::parse(text, nullptr, allow_exceptions=false)`:

- strict RFC 8259: no comments, no trailing commas, no trailing non-whitespace after the value;
- leading and trailing whitespace allowed;
- a leading UTF-8 BOM (`EF BB BF`) is accepted and skipped (a Rust port using `serde_json` MUST
  strip it first);
- invalid UTF-8 in the text is a parse error;
- with duplicate object keys, the **last** occurrence wins;
- `\u0000` escapes are allowed and yield an embedded NUL byte in the string.

If parsing fails or the top-level value is not an object, the process MUST write
`ekacache.json: not a JSON object\n` to stderr and exit with status 2. This covers the empty file, `[]`,
and `{"cc":"/c"} x`.

Recognised keys. Unknown keys are ignored.

| Key | JSON type | Default | Meaning |
|---|---|---|---|
| `cc` | string | `""` | Real compiler for C and C++. Must be non-empty (see Locating, step 4). |
| `fc` | string | `""` | Real compiler for Fortran names. |
| `binfmt` | string | `"elf"` | `elf`, `macho` or `coff`. Selects the policy. |
| `libc` | string | `""` | libc prefix. `<libc>/lib` is a RUNPATH entry and holds the dynamic linker. |
| `interp` | string | `"ld-linux-x86-64.so.2"` | Dynamic linker basename (the default holds for every target). |
| `crt` | string | `""` | Path of the `$ORIGIN`-interp stub object. Non-empty switches the ELF executable policy (A6). |
| `runtimes` | string | `""` | libc++/libunwind dir. A RUNPATH entry when a C++ runtime is linked. |
| `flags` | array of strings | `[]` | Prepended to every C and C++ invocation. |
| `cxxflags` | array of strings | `[]` | Prepended to C++ invocations. |
| `fflags` | array of strings | `[]` | Used instead of `flags` for Fortran. Subject to `@/` expansion. |
| `prefix-map` | array of strings | `[]` | Each becomes `-ffile-prefix-map=<entry>` (C and C++). |

Type handling. This is current behavior and MUST be reproduced:

- String keys: a value that exists but is not a JSON string (number, null, bool, array, object) is
  silently ignored, and the default stays. An empty string *is* a string and overrides the default.
  For example, `"interp": ""` yields an empty interp.
- Array keys: a value that exists but is not a JSON array is silently ignored. An array whose
  elements are not all strings makes the process **abort** (SIGABRT, no message; nlohmann throws
  under `-fno-exceptions`) (Q-DRV-9).
- Read order: `cc`, `fc`, `libc`, `interp`, `crt`, `runtimes`, `flags`, `cxxflags`, `fflags`,
  `prefix-map`, then `binfmt`. An abort from a bad array therefore happens before any `binfmt`
  error.
- `binfmt` is compared case-sensitively against `elf`, `macho` and `coff`. Any other *string*,
  including `""`, writes `ekacache.json: binfmt <value> is none of elf, macho, coff\n` to stderr, with the
  value raw and unquoted, then exits with status 2. With `"binfmt": ""` the message has two
  consecutive spaces. A non-string `binfmt` is ignored, which means `elf`.
- `ParseDriverConf` sets `present = true` for any object. It does not itself require `cc`.
  `package` is left empty: only `LoadDriverConf` fills it.

### `@/` expansion in `fflags`

`ParseDriverConf` applies this to each `fflags` element `flag` after reading. No other key is
expanded.

1. `p` = index of the **first** occurrence of `@/` in `flag`. No occurrence means no change.
2. Expansion happens iff `p == 0`, or `p == 2` and `flag` starts with `-`, or `flag[p-1] == '='`
   (evaluated only when `p > 0`).
3. On expansion, the single byte `@` at `p` is replaced by `root`, leaving the `/` in place.
   `@/lib` becomes `<root>/lib`. Only the first occurrence is ever considered (Q-DRV-13).

`root` is the `LoadDriverConf` root string (no trailing slash, except for `/` itself, which gives
`//lib`), or the `root` argument of `ParseDriverConf`, default `""`, which gives `/lib`.

| flag | expands? | result with root `/self` |
|---|---|---|
| `@/finc` | yes (p=0) | `/self/finc` |
| `-L@/lib` | yes (p=2, starts with `-`) | `-L/self/lib` |
| `--@/x` | yes (p=2, starts with `-`) | `--/self/x` |
| `-resource-dir=@/rd` | yes (preceded by `=`) | `-resource-dir=/self/rd` |
| `@/a@/b` | yes (first only) | `/self/a@/b` |
| `-Da@/b` | no | `-Da@/b` |
| `x@/y` | no | `x@/y` |
| `a=b@/c=@/d` | no (first occurrence is preceded by `b`) | `a=b@/c=@/d` |

### `$PKGS_CC`

The package's flags, read only by `LoadDriverConf` and only when `ekacache.json` supplied a non-empty `cc`
(`PackageFlags(root)`):

1. If `$PKGS_CC` is unset or empty, the package flags are all empty.
2. Parse the value as JSON with the same parser semantics. If parsing fails, if the value is not an
   object, or if it has no key **byte-equal** to `root` (the `LoadDriverConf` root string, e.g.
   `/nix/store/h-cc`; no normalisation, so a trailing `/` does not match), the package flags are
   all empty. Nothing is printed.
3. Otherwise let `entry` be that value. For each of `cflags`, `cxxflags` and `ldflags`: if `entry`
   is not an object, or lacks the key, the list is empty. If the key is present, its value MUST be
   an array of strings. Any other value (string, null, number, or an array with a non-string
   element) aborts the process (SIGABRT, no message) (Q-DRV-9).

The three lists, as used by A2:

- `cflags`: added to every C and C++ invocation after `flags`, FORTIFY-filtered (A4);
- `cxxflags`: added to C++ invocations after `cxxflags`;
- `ldflags`: placed **after** the user args, on link steps only (A5).

A malformed `$PKGS_CC` aborts even invocations such as `cc --version`, because it is parsed while
loading.

### `$PKGS_PREFIX_MAP`

`BuildDriverArgs` reads this on every call for C and C++ only (never for Fortran). The value is split
on `:` with empty elements **dropped** (`Split(…, ':')` with `keep_empty=false`). Each element
becomes `-ffile-prefix-map=<element>`. Unset and empty behave alike: no arguments. A mapping cannot
contain `:`. `conf.prefix_map` entries, by contrast, are used verbatim, even an empty entry, which
yields `-ffile-prefix-map=`.

### `$EKACACHE_CC` and conf-less mode

`ConfLessDriverConf(compiler)` returns a conf with `cc = compiler`, `present = false`, and every other
field at its default, except `binfmt`. That is `macho` when ec itself was compiled for an Apple
target (C++ `__APPLE__`; Rust `cfg(target_vendor = "apple")`), and `elf` on every other build host.
A conf-less conf is never passed to `BuildDriverArgs`, so the user args run unmodified. Its `binfmt`
only affects cc mode's link cacheability. Its `fc` is empty, so Fortran names always fail with the
`names no fc` message.

`$EKACACHE_CC` is used only when step 1 or 3 of Locating fell through. A readable `ekacache.json` without
`cc` returns none.

## Algorithms

### A1. Per-format policy table

| binfmt | takes rpath | arg filter | always flags | libs hook | link policy |
|---|---|---|---|---|---|
| `elf` | yes | ElfArg | `-Wl,--build-id=sha1`, `META` | none | ElfLink (A6) |
| `macho` | no | keep everything | none | none | MachOLink (A11) |
| `coff` | no | CoffArg (A12) | none | CoffLibs (A12) | none |

### A2. `BuildDriverArgs(conf, lang, raw_args)`: output order

Let `cxx = (lang == C++)` and `fortran = (lang == Fortran)`. `user = ScanUserArgs(raw_args, policy)`
(A3) runs first. The output is the concatenation of these parts, in this order:

1. `-Qunused-arguments` if `fortran`, else `--start-no-unused-arguments`.
2. `conf.fflags` if `fortran`, else `conf.flags` (verbatim).
3. *(not Fortran)* each element of `conf.package.cflags`, except elements dropped by the FORTIFY
   rule (A4).
4. *(C++ only)* `--driver-mode=g++`, then `conf.cxxflags`, then `conf.package.cxxflags`. Neither is
   FORTIFY-filtered.
5. *(not Fortran)* `-ffile-prefix-map=<m>` for each `m` in `conf.prefix_map`.
6. *(not Fortran)* `-ffile-prefix-map=<m>` for each non-empty `:`-separated `m` in
   `$PKGS_PREFIX_MAP`.
7. The policy's *always* flags. For `elf` that is `-Wl,--build-id=sha1` then `META`, **including for
   Fortran and for compile-only steps**. Nothing for `macho` and `coff`.
8. *(not Fortran)* `--end-no-unused-arguments`.
9. `user.args`: the kept user args in original order, after any CoffLibs lower-casing (A12).
10. The *tail*:
    - If `user.linking && user.have_input`: `tail = conf.package.ldflags`. Then, if the policy has a
      libs hook, it runs and mutates `user.args` (A12). Then, if `!user.no_policy`, the link policy
      appends to `tail`.
    - Otherwise the tail is empty.

So package `ldflags` appear on every link step with an input, including `-static`, `-r` and
`-nostartfiles` links and Fortran links. They are absent from compile-only steps and queries.

Shapes:

```
C:       START flags pkgcflags' prefixmaps envmaps ALWAYS END user tail
C++:     START flags pkgcflags' --driver-mode=g++ cxxflags pkgcxxflags prefixmaps envmaps ALWAYS END user tail
Fortran: -Qunused-arguments fflags ALWAYS user tail
```

(`START` = `--start-no-unused-arguments`, `END` = `--end-no-unused-arguments`.)

### A3. `ScanUserArgs(raw, policy)`

Initial state: `args = []`, `runpath = empty RunpathList`, `linking = true`, `have_input = false`,
`shared = false`, `no_policy = false`, `optimizes = true`, `sets_fortify = false`.

For `i` from 0 while `i < len(raw)`:

1. If the policy takes rpaths (ELF only) and `TakeRpathArg(raw, i)` yields a value `v` (which may
   advance `i`): `runpath.AddVerbatim(v)`, then go to the next `i`. The consumed argument(s) are
   removed from the output and take no part in the detections below.
2. `arg = raw[i]`.
3. *(no observable effect)* The C++ also records `FlagValue(raw, i, "-o")` as `output`. Nothing
   reads it. A port MAY omit this.
4. `kept = policy.arg(arg)`. If that drops the argument, go to the next `i` with no detection.
5. Detections, all on the original `arg`:
   - `linking = linking && arg ∉ {"-c", "-E", "-S", "-M", "-MM", "-fsyntax-only", "-###"}` (exact
     matches; `-MD` and `-MMD` do not count);
   - `shared = shared || arg == "-shared" || arg == "-dynamiclib"`;
   - `no_policy = no_policy || arg ∈ {"-static", "-static-pie", "-r", "-Wl,-r", "-nostartfiles"}`;
   - `have_input = have_input || !arg.starts_with('-')`. **Any** argument not starting with `-`
     counts, including separate option values such as the `x` of `-o x`, the `c` of `-x c`, the
     directory of `-I dir`, and the empty string. The lone `-` (stdin) does not count (Q-DRV-3);
   - `sets_fortify = sets_fortify || IsFortifyArg(arg)` (A4);
   - if `arg` starts with `-O`: `optimizes = (arg != "-O0")`. The last such argument wins. `-O`,
     `-Os`, `-Og`, `-Ofast`, `-O00` and also `-ObjC` count as optimizing.
6. Append `kept` to `args`. Note that `i` advances by one: `FlagValue`'s separate-value form never
   skips the value.

**`TakeRpathArg(args, i)`**, tried in this order:

1. `args[i]` is exactly `-Wl,-rpath` or `-Wl,--rpath`, `i + 1 < len`, and `args[i+1]` starts with
   `-Wl,`: advance `i` by one and return `args[i]` without its first 4 bytes (`-Wl,`). If the next
   argument does not start with `-Wl,`, this form does not match, and `-Wl,-rpath` falls through as
   an ordinary argument (as does its separate value).
2. `args[i]` starts with `-Wl,-rpath,`, `-Wl,--rpath,`, `-Wl,-rpath=` or `-Wl,--rpath=` (checked in
   this order): return the rest of the argument after that prefix. The rest is taken **whole**,
   commas included, so `-Wl,-rpath,/a,-z,now` yields `/a,-z,now` (Q-DRV-5).
3. `args[i]` is exactly `-Xlinker`, `i + 3 < len`, `args[i+1]` is `-rpath` or `--rpath`, and
   `args[i+2]` is exactly `-Xlinker`: advance `i` by 3 and return `args[i]` verbatim.
4. Otherwise no match. Every other spelling passes through untouched as ordinary arguments:
   `-Wl,-R…`, `-Xlinker -rpath=…`, bare `-rpath`, `-Wl,-rpath-link…`.

**ElfArg**: drops any argument starting with `-Wl,--dynamic-linker` or `-Wl,-dynamic-linker` (this
includes `=value`, `,value` and the bare two-argument first half). Every other argument is kept
unchanged (Q-DRV-6). ELF rpath capture runs before this filter.

**`FlagValue(args, i, flag)`**: if `args[i]` starts with `flag`, the result is the remainder when
`args[i]` is longer than `flag`. When `args[i] == flag`, the result is `args[i+1]` if that exists,
else nothing. If `args[i]` does not start with `flag`, the result is nothing.

### A4. FORTIFY handling

`IsFortifyArg(a)` is true iff `a` starts with `-D_FORTIFY_SOURCE`, or `a == "-U_FORTIFY_SOURCE"`,
or `a` starts with `-Wp,-D_FORTIFY_SOURCE`, or `a` starts with `-Wp,-U_FORTIFY_SOURCE`. The
separate form `-D _FORTIFY_SOURCE=2` is not recognised.

In A2 step 3, an element `f` of `conf.package.cflags` is **omitted** iff
`IsFortifyArg(f) && (user.sets_fortify || !user.optimizes)`. That is:

- the user's own FORTIFY argument wins, because the package's is dropped and the user's stays;
- glibc rejects `_FORTIFY_SOURCE` under `-O0`, so the package's is dropped when the **last** user
  argument starting with `-O` is exactly `-O0`.

Only user args determine `optimizes`. With no `-O…` among the user args, `optimizes` is true,
whatever `conf.flags` or the package flags contain. FORTIFY arguments in `conf.flags`,
`conf.cxxflags` and `conf.package.cxxflags` are never filtered (Q-DRV-8). Non-FORTIFY package
cflags such as `-O3` are never filtered.

### A5. Link tail gating

The tail (A2 step 10) exists iff `linking && have_input`. Within it:

- `conf.package.ldflags` come first, verbatim;
- the libs hook (COFF: CoffLibs) runs whenever the tail exists, `no_policy` links included;
- the link policy runs only if `!no_policy`.

### A6. ELF link policy (`ElfLink`)

1. `libs = CollectRunpath(conf, cxx, user)`. That is `AddRunpathEntries(conf, cxx,
   user.args ++ conf.package.ldflags, user.runpath)` (A9), which returns the `-l` count.
2. Append `-Wl,--undefined-version`, then `-Wl,-rpath,` + `user.runpath.Render(libs)` (A8), then
   `-Wl,--enable-new-dtags`.
3. If `user.shared`, stop.
4. `libc_lib = conf.libc + "/lib/"`.
5. If `conf.crt` is empty: append `-Wl,--dynamic-linker=` + `libc_lib` + `conf.interp`, then stop.
6. Otherwise append these five arguments, in order:
   - `-x`
   - `none`
   - `conf.crt`
   - `-Wl,--dynamic-linker=` + `libc_lib` + `"./"` repeated `kInterpSlack` (12) times + `conf.interp`
   - `-Wl,--export-dynamic-symbol=__reloc_start`

   The `-x none` keeps a trailing `-x c` in the user args from claiming the object. The 12 `./`
   pairs (24 bytes) are slack that `ec reloc-fixup` rewrites into an `$ORIGIN`-relative interp in
   place (see `behaviors/ec-reloc-fixup.md`).

With an empty `libc`, the dynamic linker is `/lib/<interp>` (or `/lib/././…/<interp>`). The RUNPATH
entry `/lib` is dropped as a host dir (Q-DRV-4).

### A7. `RunpathList`

State: `verbatim` (a string) and `entries` (an ordered list of strings).

- `Add(dir)`: ignore `dir` if it is a host dir or is already byte-equal to an element of `entries`.
  Otherwise append it to `entries`. It is not compared against `verbatim`.
- `AddVerbatim(rpath)`: split `rpath` on `:` **keeping empty elements** (`""` → `[""]`,
  `"a:"` → `["a", ""]`, `"a::b"` → `["a", "", "b"]`). For each part that is not a host dir, in
  order, append `part + ":"` to `verbatim`. Duplicates and empty parts are kept: cmake's install
  step string-matches `<build dir>:`. Several captured rpaths concatenate in argument order.

### A8. `Render(libs)` and the slack constants

```
out    = verbatim
for e in entries: out += e + "/.:"
stores = len(entries) + count of ':' bytes in verbatim     # = number of verbatim parts kept
pad    = max(stores, 1) * kRunpathSlack + libs * kNeededSlack - 1
return out + "/" + ("_" × pad)
```

with `kRunpathSlack = 48` and `kNeededSlack = 80`. The final pad entry is `/` followed by `pad`
underscores, `max(stores,1)*48 + libs*80` bytes in all. Our own entries end in `/.` so meson's
install step, which deletes RUNPATH elements that string-equal paths it knows, leaves them alone.
`ec reloc-fixup` normalises the spelling and uses the pad bytes to respell entries
`$ORIGIN`-relative and to add direct `$ORIGIN` NEEDED strings, one per `-l`
(`behaviors/ec-reloc-fixup.md`). Every verbatim part counts as a "store" for the pad, even an
empty, build-tree or `$ORIGIN` part (Q-DRV-15).

Worked example (`TestDriverLink`, first case; conf `kElfConf`, see Test vectors):
user args `["-o","x","x.c","-Wl,-rpath,/usr/lib:/build/lib","-Wl,--dynamic-linker=/lib/ld.so"]`.

1. ScanUserArgs: `-Wl,-rpath,/usr/lib:/build/lib` is captured. Its parts are `/usr/lib` (host,
   dropped) and `/build/lib`, so `verbatim = "/build/lib:"`. `-Wl,--dynamic-linker=/lib/ld.so` is
   dropped by ElfArg. `args = ["-o","x","x.c"]`. `have_input` is true because of `x`.
2. AddRunpathEntries: no `-L`, no `-l`, no store library. C, so no runtimes. `Add("/sr/libc/lib")`.
   So `entries = ["/sr/libc/lib"]` and `libs = 0`.
3. Render: `"/build/lib:" + "/sr/libc/lib/.:" + "/" + "_"×(max(1+1,1)*48 + 0*80 - 1 = 95)`.
4. Result argument: `-Wl,-rpath,/build/lib:/sr/libc/lib/.:/_×95`.

With two `-l` and only the libc entry: `pad = 1*48 + 2*80 - 1 = 207`.

### A9. `AddRunpathEntries(conf, cxx, args, runpath) -> libs`

`args` is `user.args` (after rpath capture and ElfArg) followed by `conf.package.ldflags`.

1. Start with `lib_dirs = []`, `libs = []` and `links_runtime = cxx`.
2. For each index `i` in order:
   - if `FlagValue(args, i, "-L")` yields `d`, append `d` to `lib_dirs`;
   - else if `FlagValue(args, i, "-l")` yields `l`: `links_runtime = links_runtime ||
     IsRuntimeLib(l)`, and append `l` to `libs`;
   - else if `args[i]` is a store path and `IsSharedLibName(basename(args[i]))`:
     `runpath.Add(parent(realpath_or_self(args[i])))`. Here `realpath_or_self` is
     `std::filesystem::canonical` (resolve every symlink, absolute) if that succeeds, else the
     argument unchanged. The store test applies to the argument as written, not the resolved path;
   - then, independently of the branch above, if `args[i]` is `-nostdlib++` or `-nostdlib`, set
     `links_runtime = false`. This is order-dependent: a later runtime `-l` sets it true again
     (Q-DRV-7).

   `FlagValue` does not skip a separate value, so `-L dir` and `-l name` also visit the value at
   `i+1` as an argument in its own right.
3. `IsRuntimeLib(l)`: `l ∈ {"c++", "stdc++", "gcc_s", "unwind", ":libunwind.so"}`.
4. For each `d` in `lib_dirs`, in order: `real = realpath_or_self(d)` (relative to the current
   directory). If `real` is a store path **and** some `l` in `libs` (any of them, wherever it
   appears) has `<real>/lib<l>.so` or `<real>/lib<l>.dylib` existing, then `runpath.Add(real)`.
   Versioned-only (`libfoo.so.1`), static-only and `-l:name` libraries never qualify a dir.
5. If `links_runtime` and `conf.runtimes` is non-empty: `runpath.Add(conf.runtimes)`, used verbatim
   and not canonicalised.
6. `runpath.Add(conf.libc + "/lib")`, verbatim.
7. Return `len(libs)`. This counts every `-l` occurrence in user args and package ldflags,
   duplicates included.

The resulting entry order is: store shared libraries named by path (argument order), then
qualifying `-L` dirs (user args first, then package ldflags), then `runtimes`, then `<libc>/lib`.
Every entry is subject to the host-dir filter and de-duplication of `Add`.

Existence checks use the throwing `std::filesystem::exists`. An error other than "not found" or
"not a directory" (EACCES, ELOOP, ENAMETOOLONG) terminates the process (Q-DRV-10).

### A10. `IsSharedLibName(basename)`

1. If it ends with `.dylib`, true.
2. `p` = position of the **last** `.so`. None means false.
3. `tail` = the bytes after that `.so`. An empty tail means true.
4. Otherwise true iff `tail[0] == '.'` and every byte of `tail[1..]` is `.` or an ASCII digit.

There is no `lib` prefix requirement. `libz.so`, `libz.so.1.3`, `x.so`, `libz.so.` and `a.dylib` are
true. `libz.son`, `libz.so.1a`, `x.o` and `x.dylib.1` are false.

### A11. Mach-O policy

- No rpath capture and no arg filter. User `-Wl,-rpath…` and dynamic-linker args pass through
  unchanged.
- No always flags (no build-id, no package metadata).
- Link policy: append `-Wl,-headerpad_max_install_names`. This applies to **every** policy link,
  shared (`-shared` or `-dynamiclib`) or not. It gives `ec reloc-fixup` header room to respell
  install names `@loader_path`-relative. No RUNPATH, interp or crt is added.

### A12. COFF policy

**CoffArg** (the arg filter):

1. Drop the argument if it is exactly one of `-fPIC`, `-fpic`, `-fPIE`, `-fpie`, `-fno-PIC`,
   `-fno-pic`, `-pie`.
2. For `old` in `++98`, `++03`, `++0x`, `++11` (in order): if the argument starts with `-std=c` or
   `-std=gnu` and ends with `old`, replace it with the argument minus its last 2 bytes plus `14`.
   So `-std=c++11` → `-std=c++14`, `-std=gnu++0x` → `-std=gnu++14`, `-std=c++98` → `-std=c++14`.
   This applies in every language, C included. `-std=gnu++17`, `-std=c11` and `--std=c++11` are
   unchanged.
3. Otherwise keep the argument unchanged.

**CoffLibs** (libs hook; runs on every link with input, `no_policy` links included):

1. `dirs` = every `FlagValue(…, "-L")` value from `user.args`, then from `conf.package.ldflags`.
   Both the joined and the separate form count.
2. For each element `a` of `user.args`: skip it if it does not start with `-l`, or contains `/`, or
   if `present(a[2..])`. Otherwise lower-case bytes 2..end with C-locale `tolower` (ASCII A–Z
   only).
3. `present(name)` is true iff some `d` in `dirs` has an existing `<d>/lib<name>.dll.a`, or
   `<d>/lib<name>.a`, or `<d>/<name>`. Relative paths are resolved against the current directory.
   For the bare `-l`, `name` is `""`, so `<d>/` counts. Either way nothing changes.

The separate form `-l Name` is not lower-cased, because the value does not start with `-l`.
`conf.package.ldflags` `-l` entries are never lower-cased. The same `exists` error behavior as in A9
applies.

COFF has no always flags and no link policy.

### A13. `WithPackageLibDirs(search_dirs, ldflags)`

1. `extra` = the concatenation, in order, of `":" + f[2..]` for each `f` in `ldflags` that starts with
   `-L` and is longer than 2 bytes. The separate form `-L dir` contributes nothing.
2. Split `search_dirs` into lines on `\n`. Iterate while the position is less than the length, so
   a final `\n` does not produce an extra empty line. For each line: emit the line; if it starts
   with `libraries:`, emit `extra`; then emit `\n`.

Consequences:

- An empty input gives an empty output.
- Output always ends with `\n` when the input is non-empty.
- Inner empty lines are preserved.
- Every `libraries:` line gets `extra`.
- A `libraries: =` line with an empty list becomes `libraries: =:/x`.
- `\r` is not special.

## Errors and messages

All messages go to stderr, are followed by a single `\n`, and are written exactly as shown (`<…>`
is substituted raw).

| Condition | Text | Exit |
|---|---|---|
| `ekacache.json` unparsable or not an object | `ekacache.json: not a JSON object` | `exit(2)` from `LoadDriverConf` |
| `binfmt` is a string other than `elf`/`macho`/`coff` | `ekacache.json: binfmt <value> is none of elf, macho, coff` | `exit(2)` |
| `ekacache.json` array key holds a non-string element | *(none)* | abort (SIGABRT; shells report 134) |
| `$PKGS_CC` entry's `cflags`/`cxxflags`/`ldflags` not an array of strings | *(none)* | abort (SIGABRT) |
| filesystem `exists` error other than not-found (A9, A12) | libstdc++ terminate text, e.g. `terminate called after throwing an instance of 'std::filesystem::__cxx11::filesystem_error'` / `  what():  filesystem error: status: Permission denied [<path>]` | abort (SIGABRT) |
| no conf: `ekacache.json` unreadable or absent and `$EKACACHE_CC` empty, **or** `ekacache.json` without a non-empty string `cc` | `ec: no etc/ekacache.json next to the binary and EKACACHE_CC unset` | cc mode returns 1 |
| Fortran name and `conf.fc` empty (always in conf-less mode) | `ec: <name> called but etc/ekacache.json names no fc` | cc mode returns 1 |

These are silent and have no error: `$PKGS_CC` unset, empty, unparsable, a non-object, or without a
matching root; wrong-typed scalar keys in `ekacache.json`; unknown keys. The exit-2 paths flush stdio
(`std::exit`). The Rust port MUST exit 2 with the same bytes. For the abort rows it MUST terminate
by SIGABRT. The libstdc++ text MAY differ (see Q-DRV-9 and Q-DRV-10 for recommended changes).

## Test vectors

Unless stated otherwise, `$PKGS_PREFIX_MAP` is unset, `conf.package` is empty, and the store dir is
`/nix/store`. Test confs:

- `kElfConf` = `{"cc": "/seed/bin/clang", "flags": ["--target=x", "-O2"], "cxxflags": ["-stdlib=libc++"], "libc": "/sr/libc", "crt": "/cc/lib/crt_interp.o", "runtimes": "/sr/rt/lib"}`
- `kCoffConf` = `{"cc": "/seed/bin/clang", "binfmt": "coff", "flags": ["--target=x86_64-pc-windows-msvc"]}`
- `kMachO` = `{"cc": "/seed/bin/clang", "binfmt": "macho", "flags": ["--target=arm64-apple-macos14.0"], "libc": "/sr"}`

Unquoted `START`, `END` and `META` inside arrays stand for the single arguments
`"--start-no-unused-arguments"`, `"--end-no-unused-arguments"` and
`"-Wl,--package-metadata={\"type\":\"repkgs\"}"`.

`CRT_TAIL` is shorthand for these five arguments (kElfConf executable link):
`"-x","none","/cc/lib/crt_interp.o","-Wl,--dynamic-linker=/sr/libc/lib/././././././././././././ld-linux-x86-64.so.2","-Wl,--export-dynamic-symbol=__reloc_start"`.

### From the unit tests

TV-1 (`TestDriverConf`): `ParseDriverConf(kElfConf)` gives `present = true`,
`cc = "/seed/bin/clang"` and `flags = ["--target=x","-O2"]`.

TV-2 (`TestDriverConf`): `ParseDriverConf({"cc": "/c", "fc": "/f/bin/flang", "fflags": ["-L@/lib", "-fintrinsic-modules-path", "@/finc", "-resource-dir=@/rd", "-Da@/b"]}, "/self")`
gives `fc = "/f/bin/flang"` and
`fflags = ["-L/self/lib","-fintrinsic-modules-path","/self/finc","-resource-dir=/self/rd","-Da@/b"]`.

TV-3 (`TestDriverConf`): `kMachO` gives `binfmt = macho`. C, `["a.c","-o","a"]`, joined with spaces:
no `dynamic-linker`, no `crt_interp`, no `$ORIGIN` and no `-rpath`, but it contains
`-Wl,-headerpad_max_install_names`. Full vector (derived):
`["--start-no-unused-arguments","--target=arm64-apple-macos14.0","--end-no-unused-arguments","a.c","-o","a","-Wl,-headerpad_max_install_names"]`.

TV-4 (`TestDriverConf`): `kCoffConf`, C, `["-fPIC","-c","a.c"]` gives no `-fPIC`. Full (derived):
`["--start-no-unused-arguments","--target=x86_64-pc-windows-msvc","--end-no-unused-arguments","-c","a.c"]`.

TV-5 (`TestDriverConf`):

- `kCoffConf`, C++, `["-std=c++11","-c","a.cc"]` contains `-std=c++14`.
- `kCoffConf`, C++, `["-std=gnu++17","-c","a.cc"]` contains `-std=gnu++17`.
- `kMachO`, C++, `["-std=c++11","-c","a.cc"]` contains `-std=c++11`.
- `kCoffConf`, C, `["-c","a.c"]` joined has no `build-id`.

TV-6 (`TestDriverConf`): `kElfConf`, C, `["-c","a.c"]` gives
`["--start-no-unused-arguments","--target=x","-O2","-Wl,--build-id=sha1",META,"--end-no-unused-arguments","-c","a.c"]`.

TV-7 (`TestDriverConf`): `kElfConf`, C++, `["-c","a.cc"]` has `out[3] == "--driver-mode=g++"` and
`out[4] == "-stdlib=libc++"`. Full (derived):
`["--start-no-unused-arguments","--target=x","-O2","--driver-mode=g++","-stdlib=libc++","-Wl,--build-id=sha1",META,"--end-no-unused-arguments","-c","a.cc"]`.

TV-8 (`TestDriverConf`, COFF lower-casing): a temp dir `W` contains only `libLLVM-22.dll.a`.

- `kCoffConf`, C, `["a.o","-L<W>","-lWS2_32","-lLLVM-22"]` contains `-lws2_32` and `-lLLVM-22`.
- `["a.o","-nostartfiles","-lWS2_32.lib"]` contains `-lws2_32.lib` (CoffLibs runs on `no_policy`
  links).
- `["a.o","-L","<W>","-lLLVM-22"]` contains `-lLLVM-22` (the separate `-L` counts).

TV-9 (`TestDriverPackageFlags`): `kElfConf` with
`package = {cflags: ["-O3"], cxxflags: ["-fno-rtti"], ldflags: ["-Wl,-z,x"]}`, C++,
`["-c","a.cc","-O0"]` gives
`["--start-no-unused-arguments","--target=x","-O2","-O3","--driver-mode=g++","-stdlib=libc++","-fno-rtti","-Wl,--build-id=sha1",META,"--end-no-unused-arguments","-c","a.cc","-O0"]`.

TV-10 (`TestDriverPackageFlags`): same package, C, `["-shared","-o","x.so","x.o","-L."]`. The joined
output contains `x.o -L. -Wl,-z,x -Wl,`. Full (derived, assuming the cwd is not a store dir
providing a lib):
`["--start-no-unused-arguments","--target=x","-O2","-O3","-Wl,--build-id=sha1",META,"--end-no-unused-arguments","-shared","-o","x.so","x.o","-L.","-Wl,-z,x","-Wl,--undefined-version","-Wl,-rpath,/sr/libc/lib/.:/_______________________________________________","-Wl,--enable-new-dtags"]`
(47 underscores).

TV-11 (`TestDriverPackageFlags`): same package, C, `["-Wl,--build-id=none","x.o"]`. The joined
output contains `-Wl,--package-metadata={"type":"repkgs"} --end-no-unused-arguments -Wl,--build-id=none x.o`.
Full (derived):
`[START,"--target=x","-O2","-O3","-Wl,--build-id=sha1",META,END,"-Wl,--build-id=none","x.o","-Wl,-z,x","-Wl,--undefined-version","-Wl,-rpath,/sr/libc/lib/.:/_×47","-Wl,--enable-new-dtags",CRT_TAIL]`.

TV-12 (`TestDriverPackageFlags`, FORTIFY): package `cflags = ["-O2","-D_FORTIFY_SOURCE=3"]`, C.

- `["-c","a.c"]`: output contains `FORTIFY`.
- `["-c","a.c","-O0"]`: no `FORTIFY`.
- `["-c","a.c","-O2","-O0"]`: no `=3`.
- `["-c","a.c","-D_FORTIFY_SOURCE=2"]`: no `=3`, contains `=2`.

TV-13 (`TestDriverLink`): `kElfConf`, C,
`["-o","x","x.c","-Wl,-rpath,/usr/lib:/build/lib","-Wl,--dynamic-linker=/lib/ld.so"]`. The joined
output has no `/usr/lib`, no `/lib/ld.so `, no `/sr/rt/lib`. It contains
`-Wl,-rpath,/build/lib:/sr/libc/lib/.:/_` and
`-x none /cc/lib/crt_interp.o -Wl,--dynamic-linker=/sr/libc/lib/././././././././././././ld-linux-x86-64.so.2 -Wl,--export-dynamic-symbol=__reloc_start`.
Full (derived):
`[START,"--target=x","-O2","-Wl,--build-id=sha1",META,END,"-o","x","x.c","-Wl,--undefined-version","-Wl,-rpath,/build/lib:/sr/libc/lib/.:/_×95","-Wl,--enable-new-dtags",CRT_TAIL]`.

TV-14 (`TestDriverLink`): C, `["-o","x","x.c","-Wl,-rpath,/build/build:"]` contains
`-Wl,-rpath,/build/build::/sr/libc/lib/.:/_`. Full rpath (derived):
`-Wl,-rpath,/build/build::/sr/libc/lib/.:/_×143`, because stores = 2 verbatim parts + 1 entry.

TV-15 (`TestDriverLink`): C++, `["-o","x","x.cc"]` contains `/sr/rt/lib/.:/sr/libc/lib/.:/_`, and so
does C, `["-o","x","x.c","-lc++"]`. Full C++ vector (derived):
`[START,"--target=x","-O2","--driver-mode=g++","-stdlib=libc++","-Wl,--build-id=sha1",META,END,"-o","x","x.cc","-Wl,--undefined-version","-Wl,-rpath,/sr/rt/lib/.:/sr/libc/lib/.:/_×95","-Wl,--enable-new-dtags",CRT_TAIL]`.
For the `-lc++` C case the pad is `_×175` (2 stores, 1 lib).

TV-16 (`TestDriverLink`): C, `["-shared","-o","l.so","l.c"]` contains `-rpath` and no
`crt_interp`. Full (derived):
`[START,"--target=x","-O2","-Wl,--build-id=sha1",META,END,"-shared","-o","l.so","l.c","-Wl,--undefined-version","-Wl,-rpath,/sr/libc/lib/.:/_×47","-Wl,--enable-new-dtags"]`.

TV-17 (`TestDriverLink`): each of `-static`, `-static-pie`, `-r` and `-nostartfiles` with
`[<flag>,"-o","x","x.c"]` gives no `-rpath`. Full for `-static` (derived):
`[START,"--target=x","-O2","-Wl,--build-id=sha1",META,END,"-static","-o","x","x.c"]`.

TV-18 (`TestDriverLink`): C, `["-o","x","x.c","/nix/store/h-zlib/lib/libz.so.1"]` (the file need not
exist) contains `-rpath,/nix/store/h-zlib/lib/.:/sr/libc/lib/.:/_`. Full rpath (derived):
`-Wl,-rpath,/nix/store/h-zlib/lib/.:/sr/libc/lib/.:/_×95`. The library path stays in the user args.

TV-19 (`TestDriverLink`): `IsSharedLibName`: `libz.so` true, `libz.so.1.3` true, `libz.son` false,
`x.o` false.

TV-20 (`TestSearchDirs`):

- `WithPackageLibDirs("programs: =/cc/bin\nlibraries: =/cc/lib/clang/23:/sysroot/lib\n", ["-L/deps/zlib/lib","-Wl,-z,now","-L/deps/xz/lib"])`
  gives `"programs: =/cc/bin\nlibraries: =/cc/lib/clang/23:/sysroot/lib:/deps/zlib/lib:/deps/xz/lib\n"`.
- The same text with `[]` is returned unchanged.

### Derived vectors (computed from the code and confirmed against the current build)

TV-21 (`-l` slack): `kElfConf`, C, `["-o","x","x.c","-lfoo","-l","bar"]` gives
`[START,"--target=x","-O2","-Wl,--build-id=sha1",META,END,"-o","x","x.c","-lfoo","-l","bar","-Wl,--undefined-version","-Wl,-rpath,/sr/libc/lib/.:/_×207","-Wl,--enable-new-dtags",CRT_TAIL]`
(48·1 + 80·2 − 1).

TV-22 (rpath spellings): `kElfConf`, C,
`["x.o","-Xlinker","-rpath","-Xlinker","/a","-Wl,-rpath","-Wl,/b","-Wl,--rpath=/c","-Wl,-rpath","/d"]`
gives user part `["x.o","-Wl,-rpath","/d"]` and rpath `-Wl,-rpath,/a:/b:/c:/sr/libc/lib/.:/_×191`.
The last pair is not captured because `/d` does not start with `-Wl,`.

TV-23 (empty and comma rpaths): `kElfConf`, C, `["x.o","-Wl,-rpath,","-Wl,-rpath,/usr/lib,-z,now"]`
gives user part `["x.o"]` and rpath `-Wl,-rpath,:/sr/libc/lib/.:/_×95`. The second argument's single
part `/usr/lib,-z,now` is a host dir, so `-z now` is lost.

TV-24 (output-only target counts as input): `kElfConf`, C, `["-o","x"]` gives
`[START,"--target=x","-O2","-Wl,--build-id=sha1",META,END,"-o","x","-Wl,--undefined-version","-Wl,-rpath,/sr/libc/lib/.:/_×47","-Wl,--enable-new-dtags",CRT_TAIL]`.

TV-25 (no input): `kElfConf`, C, `["--version"]` gives
`[START,"--target=x","-O2","-Wl,--build-id=sha1",META,END,"--version"]`.

TV-26 (`-nostdlib` order): `kElfConf`, C++:

- `["x.o","-nostdlib","-lc++"]`: rpath `-Wl,-rpath,/sr/rt/lib/.:/sr/libc/lib/.:/_×175`.
- `["x.o","-lc++","-nostdlib"]`: rpath `-Wl,-rpath,/sr/libc/lib/.:/_×127`.

Both still end in `CRT_TAIL`.

TV-27 (package FORTIFY scope): `kElfConf` with
`package = {cflags: ["-O3","-D_FORTIFY_SOURCE=3"], cxxflags: ["-fno-rtti","-D_FORTIFY_SOURCE=3"], ldflags: ["-Wl,-z,x","-lpkg"]}`.

- C++, `["-c","a.cc","-O0"]` gives
  `[START,"--target=x","-O2","-O3","--driver-mode=g++","-stdlib=libc++","-fno-rtti","-D_FORTIFY_SOURCE=3","-Wl,--build-id=sha1",META,END,"-c","a.cc","-O0"]`.
  The cflags FORTIFY is dropped, the cxxflags one is kept.
- C, `["-c","a.c","-O0","-Os"]` keeps `-D_FORTIFY_SOURCE=3` (the last `-O` wins).
- C, `["-shared","-o","x.so","x.o","-L."]` gives
  `[START,"--target=x","-O2","-O3","-D_FORTIFY_SOURCE=3","-Wl,--build-id=sha1",META,END,"-shared","-o","x.so","x.o","-L.","-Wl,-z,x","-lpkg","-Wl,--undefined-version","-Wl,-rpath,/sr/libc/lib/.:/_×127","-Wl,--enable-new-dtags"]`.
  The package `-lpkg` counts toward the pad.

TV-28 (no crt): `{"cc":"/c","libc":"/sr/libc","interp":"ld-linux-aarch64.so.1"}`, C, `["x.o","-o","x"]` gives
`["--start-no-unused-arguments","-Wl,--build-id=sha1",META,"--end-no-unused-arguments","x.o","-o","x","-Wl,--undefined-version","-Wl,-rpath,/sr/libc/lib/.:/_×47","-Wl,--enable-new-dtags","-Wl,--dynamic-linker=/sr/libc/lib/ld-linux-aarch64.so.1"]`.

TV-29 (empty libc): `{"cc":"/c","crt":"/crt.o"}`, C, `["x.o","-o","x"]` gives
`[START,"-Wl,--build-id=sha1",META,END,"x.o","-o","x","-Wl,--undefined-version","-Wl,-rpath,/_×47","-Wl,--enable-new-dtags","-x","none","/crt.o","-Wl,--dynamic-linker=/lib/././././././././././././ld-linux-x86-64.so.2","-Wl,--export-dynamic-symbol=__reloc_start"]`.

TV-30 (Fortran):
`ParseDriverConf({"cc":"/c","fc":"/f","fflags":["-L@/lib","@/a@/b","--@/x","a=b@/c=@/d","x@/y","-I","@/i"],"flags":["-fc"],"prefix-map":["/b=/c"],"libc":"/l"}, "/self")`:

- `fflags` = `["-L/self/lib","/self/a@/b","--/self/x","a=b@/c=@/d","x@/y","-I","/self/i"]`.
- Fortran, `["-c","a.f90"]` gives
  `["-Qunused-arguments","-L/self/lib","/self/a@/b","--/self/x","a=b@/c=@/d","x@/y","-I","/self/i","-Wl,--build-id=sha1",META,"-c","a.f90"]`.
- Fortran, `["a.o","-o","a"]` gives the same prefix, then
  `"a.o","-o","a","-Wl,--undefined-version","-Wl,-rpath,/l/lib/.:/_×47","-Wl,--enable-new-dtags","-Wl,--dynamic-linker=/l/lib/ld-linux-x86-64.so.2"`.
- The same conf, C, `["-c","a.c"]` gives
  `[START,"-fc","-ffile-prefix-map=/b=/c","-Wl,--build-id=sha1",META,END,"-c","a.c"]`. With
  `PKGS_PREFIX_MAP="a=b::c=d:"` it gives
  `[START,"-fc","-ffile-prefix-map=/b=/c","-ffile-prefix-map=a=b","-ffile-prefix-map=c=d","-Wl,--build-id=sha1",META,END,"-c","a.c"]`.

TV-31 (Mach-O pass-through): `kMachO`:

- C, `["a.c","-o","a","-Wl,-rpath,/x"]` gives
  `[START,"--target=arm64-apple-macos14.0",END,"a.c","-o","a","-Wl,-rpath,/x","-Wl,-headerpad_max_install_names"]`.
- C++, `["-dynamiclib","a.o","-o","a.dylib"]` gives
  `[START,"--target=arm64-apple-macos14.0","--driver-mode=g++",END,"-dynamiclib","a.o","-o","a.dylib","-Wl,-headerpad_max_install_names"]`.

TV-32 (COFF): `kCoffConf`:

- C++, `["-fPIC","-std=gnu++0x","-c","a.cc","-Wl,-rpath,/x"]` gives
  `[START,"--target=x86_64-pc-windows-msvc","--driver-mode=g++",END,"-std=gnu++14","-c","a.cc","-Wl,-rpath,/x"]`.
- C, `["a.o","-lWS2_32","-l","Foo","-lA/B"]` gives
  `[START,"--target=x86_64-pc-windows-msvc",END,"a.o","-lws2_32","-l","Foo","-lA/B"]`.
- C, `["-std=c++98","-std=gnu++11","-std=c++03","-std=gnu++17","-std=c11","-c","a.c"]` gives user
  part `["-std=c++14","-std=gnu++14","-std=c++14","-std=gnu++17","-std=c11","-c","a.c"]`.

TV-33 (store `-L` qualification and ordering): `Z` is a real store dir containing `libz.so`. Conf
`{"cc":"/c","libc":"/sr/libc","runtimes":"/usr/rt"}` with package `ldflags = ["-L<Z>/../lib","-lm"]`.
C++, `["x.o","-L","<Z>","-lz","<Z>/libz.so","-L/nonstore","-o","x"]` gives rpath
`-Wl,-rpath,<Z>/.:/sr/libc/lib/.:/_×255`.

- `<Z>` is added once: first from the library given by path, after which the `-L` hits are
  de-duplicated, `<Z>/../lib` included after canonicalisation.
- `/usr/rt` is dropped as a host dir even though the invocation is C++.
- libs = 2 (`-lz`, `-lm`).
- The tail ends `"-Wl,--enable-new-dtags","-Wl,--dynamic-linker=/sr/libc/lib/ld-linux-x86-64.so.2"`.

TV-34 (`WithPackageLibDirs` edges):

- `("programs: =/a\nlibraries: =", ["-L/x","-L","/y","-Lz"])` gives
  `"programs: =/a\nlibraries: =:/x:z\n"`.
- `("", ["-L/x"])` gives `""`.
- `("a\n\nlibraries: =/l\n", ["-L/x"])` gives `"a\n\nlibraries: =/l:/x\n"`.

TV-35 (`IsSharedLibName`): `libz.so.1a` false, `a.dylib` true, `libz.so.` true.

TV-36 (conf parse errors):

| ekacache.json text | stderr | status |
|---|---|---|
| `[]` | `ekacache.json: not a JSON object` | 2 |
| *(empty)* | `ekacache.json: not a JSON object` | 2 |
| `{"cc":"/c"} x` | `ekacache.json: not a JSON object` | 2 |
| `{"cc":"/c", /*c*/ "x":1}` | `ekacache.json: not a JSON object` | 2 |
| `{"cc":"\xff"}` (raw byte) | `ekacache.json: not a JSON object` | 2 |
| `{"cc":"/c","binfmt":"pe"}` | `ekacache.json: binfmt pe is none of elf, macho, coff` | 2 |
| `{"cc":"/c","binfmt":""}` | `ekacache.json: binfmt  is none of elf, macho, coff` | 2 |
| `{"cc":"/c","binfmt":5}` | *(none)*: binfmt elf | parse ok |
| `{"cc":"/c","flags":"x","libc":7}` | *(none)*: flags `[]`, libc `""` | parse ok |
| `{"cc":"/a","cc":"/b"}` | *(none)*: cc `/b` | parse ok |
| BOM + `{"cc":"/bom"}` | *(none)*: cc `/bom` | parse ok |
| `{"cc":"/c","flags":[1]}` | *(none)* | SIGABRT |
| `{"cc":"/c","flags":[1],"binfmt":"pe"}` | *(none)* | SIGABRT |

TV-37 (end to end; real binary at `R/bin/ec` with symlinks `cc`, `c++` and `gfortran` next to it):

- `R/etc/ekacache.json` = `{"cc":"<R>/fakecc","flags":["-fx"]}`, and `cc -print-search-dirs` where the
  compiler prints `…\nprograms: =/p\nlibraries: =/l` with no final newline. ec prints the
  compiler's stdout with a `\n` appended.
- With `PKGS_CC={"<R>":{"ldflags":["-L/dep/lib","-L","/x"],"cflags":["-DP"]}}`, the compiler's argv
  is `[--start-no-unused-arguments][-fx][-DP][-Wl,--build-id=sha1][META][--end-no-unused-arguments][-print-search-dirs]`
  and the last line printed is `libraries: =/l:/dep/lib`.
- `gfortran -c a.f` prints `ec: gfortran called but etc/ekacache.json names no fc` and returns 1.
- `ekacache.json` = `{"flags":[]}` with `EKACACHE_CC=/bin/true`: `cc -c a.c` prints
  `ec: no etc/ekacache.json next to the binary and EKACACHE_CC unset` and returns 1.
- No `ekacache.json`, `EKACACHE_CC=<fakecc>`: `c++ -c a.cc -O0` runs the compiler with exactly
  `[-c][a.cc][-O0]`.
- `PKGS_CC={"<R>":5}`, `"not json"`, `[{"<R>":1}]`, or a key `<R>/`: no package flags, no error.
- `PKGS_CC={"<R>":{"cflags":"x"}}` or `{"<R>":{"cflags":null}}`: SIGABRT, even for `cc --version`.

## Quirks

### Q-DRV-1: A `ekacache.json` without `cc` disables the `$EKACACHE_CC` fallback and gives a misleading message

**Current behavior**: When `etc/ekacache.json` is readable and parses as an object but `cc` is absent,
not a string, or empty, `LoadDriverConf` returns none without consulting `$EKACACHE_CC`. cc mode then
prints `ec: no etc/ekacache.json next to the binary and EKACACHE_CC unset`, even though the file exists and
`EKACACHE_CC` may be set.
**Impact**: Confusing diagnosis of a broken toolchain conf.
**Recommendation**: change in Rust — needs maintainer sign-off. Keep the no-fallback behavior, but
use a message naming the missing `cc`. Until approved, emit the current text.

### Q-DRV-2: The executable is located only via `/proc/self/exe`, and `$PKGS_CC` keys must match its spelling

**Current behavior**: Only `readlink("/proc/self/exe")` is tried. On hosts without procfs (macOS),
`ekacache.json` is never read and only `$EKACACHE_CC` works. The `$PKGS_CC` lookup key is the root derived from
the kernel-resolved path, byte for byte.
**Impact**: `binfmt: macho` confs work only when ec runs on Linux (cross builds). A `$PKGS_CC` key
spelled through a symlink or with a trailing slash silently matches nothing.
**Recommendation**: keep. The Rust port MUST read `/proc/self/exe` itself rather than use
`std::env::current_exe` semantics on non-Linux hosts.

### Q-DRV-3: `have_input` counts any argument not starting with `-`

**Current behavior**: Option values such as `-o x`, `-x c`, `-MF d`, `-I dir` and `-include h`, and
also empty-string arguments, set `have_input`. So `cc -o x` with no source gets package ldflags and
the full link policy (TV-24).
**Impact**: Queries with separate option values may receive link flags. That is harmless for the
compiler but changes the cache key.
**Recommendation**: keep.

### Q-DRV-4: The host-dir filter is a bare byte prefix

**Current behavior**: Anything starting with `/usr`, `/lib` or `/opt` is dropped from build-system
rpaths and from our entries. That includes `/lib64`, `/library`, `/optimized` and `/usr2`, and also
`runtimes` (TV-33) and `<libc>/lib` when `libc` is empty (TV-29). The dynamic linker path is not
filtered.
**Impact**: Non-host dirs with those prefixes vanish from RUNPATH. An empty `libc` yields
`--dynamic-linker=/lib/…` with no `/lib` RUNPATH entry.
**Recommendation**: document only.

### Q-DRV-5: Rpath capture takes the whole `-Wl,` remainder, commas included

**Current behavior**: `-Wl,-rpath,/a,-z,now` captures `/a,-z,now` as one part. It survives inside
our `-Wl,-rpath,…` (the linker still sees `-z now`) unless the part is a host dir, in which case
`-z now` is silently lost (TV-23). `-Wl,-rpath,/a,-rpath,/b` counts as one part for the slack while
the linker sees two entries.
**Impact**: Rare loss of linker options, and slack can be undercounted.
**Recommendation**: document only.

### Q-DRV-6: Dynamic-linker drop is by prefix of a single argument

**Current behavior**: ELF drops any argument starting with `-Wl,--dynamic-linker` or
`-Wl,-dynamic-linker`.

- The two-argument form `-Wl,--dynamic-linker -Wl,/x` leaves the orphan `-Wl,/x`, which the linker
  reads as an input file.
- A combined `-Wl,--dynamic-linker=/x,-z,now` loses `-z,now`.
- `-Xlinker --dynamic-linker -Xlinker /x` and `-Wl,-I…` are not dropped.

**Impact**: Occasional broken or altered links for unusual spellings.
**Recommendation**: document only.

### Q-DRV-7: Runtime detection is order-dependent and incomplete

**Current behavior**: `-nostdlib` and `-nostdlib++` reset `links_runtime` to false only at their
position. A later `-lc++` sets it true again, an earlier one does not (TV-26). `-nodefaultlibs` is
not recognised. `-nostdlib` does not suppress the crt/interp policy (only `-nostartfiles` does), so
`crt_interp.o` is still linked into `-nostdlib` executables.
**Impact**: RUNPATH may include or omit `runtimes` depending on argument order.
**Recommendation**: document only.

### Q-DRV-8: FORTIFY filtering looks at user args only

**Current behavior**: `optimizes` defaults to true when no user `-O…` exists, even though clang's
default is `-O0`. Any argument starting with `-O` other than `-O0` counts as optimizing, `-ObjC`
included. Only `conf.package.cflags` is filtered. FORTIFY in `conf.flags`, `conf.cxxflags` or
`conf.package.cxxflags` always passes (TV-27). `-D _FORTIFY_SOURCE` (separate) is not detected.
**Impact**: A package FORTIFY flag can reach a `-O0` build whose `-O0` comes from the toolchain flags.
glibc then warns or errors.
**Recommendation**: keep.

### Q-DRV-9: Malformed JSON types are either silently ignored or abort without a message

**Current behavior**: In `ekacache.json`, wrong-typed scalars and non-array list values are ignored. A
non-string array element aborts (SIGABRT, no output). In `$PKGS_CC`, invalid JSON or a missing root is
ignored, but a present `cflags`, `cxxflags` or `ldflags` that is not an array of strings aborts.
**Impact**: Hard-to-diagnose crashes on any cc invocation, including `--version`.
**Recommendation**: change in Rust — needs maintainer sign-off. Print
`ekacache.json: <key> is not a list of strings` (or similar) and exit 2. Until approved, abort.

### Q-DRV-10: Filesystem probe errors terminate the process

**Current behavior**: `std::filesystem::exists` (throwing overload), used for the `-L` provides check
(A9) and CoffLibs (A12), throws on EACCES, ELOOP or ENAMETOOLONG. Under `-fno-exceptions` that ends
in `std::terminate`, with libstdc++'s "terminate called after throwing…" text and SIGABRT.
**Impact**: An unreadable `-L` dir crashes the compile.
**Recommendation**: change in Rust — needs maintainer sign-off. Treat such errors as "does not
exist".

### Q-DRV-11: CoffLibs ignores the separate `-l Name` form, and lower-casing is ASCII-only

**Current behavior**: Only joined `-lName` arguments in the user args are candidates. `-l Name` and
package-ldflags `-l` entries are never lower-cased. Lower-casing uses C-locale `tolower`.
**Impact**: Mixed-case system libs spelled with a separate `-l` still fail on case-sensitive hosts.
**Recommendation**: document only.

### Q-DRV-12: `WithPackageLibDirs` handles only joined `-L`, and can create an empty element

**Current behavior**: `-L dir` (two arguments) contributes nothing. A `libraries: =` line with no
dirs becomes `libraries: =:/x`. Output gains a trailing `\n` when the input lacks one.
**Impact**: libtool-style searches may miss dependency dirs passed in separate form.
**Recommendation**: keep.

### Q-DRV-13: `@/` expansion considers only the first occurrence, and `--@/` expands

**Current behavior**: Only the first `@/` in an `fflags` element is examined. `a=b@/c=@/d` does not
expand, `@/a@/b` expands only the first. The "short option" rule is "position 2 and starts with
`-`", so `--@/x` expands too.
**Impact**: Rare unexpanded or oddly expanded flags.
**Recommendation**: keep.

### Q-DRV-14: Link-kind detection is spelling-exact

**Current behavior**: Only `-shared` and `-dynamiclib` mark a shared link. `-Wl,-shared` and
`-Wl,--shared` get `crt_interp.o` and an interp. Only `-static`, `-static-pie`, `-r`, `-Wl,-r` and
`-nostartfiles` refuse the policy. `-Wl,--relocatable`, `-Xlinker -r` and `-Wl,-Bstatic` do not.
Output-only detection is limited to the exact seven flags.
**Impact**: Unusual spellings get an executable's link policy.
**Recommendation**: document only.

### Q-DRV-15: RUNPATH slack counts every verbatim part and every `-l`

**Current behavior**: `stores` counts each kept verbatim part (build-tree, `$ORIGIN` and empty parts
included) as needing 48 bytes. `libs` counts every `-l` (static, duplicates, package ldflags),
80 bytes each.
**Impact**: Over-padding of DT_RUNPATH strings, which is harmless. Changing it changes every linked
binary and cache key.
**Recommendation**: keep.

### Q-DRV-16: Store libraries named by path are permissive

**Current behavior**: `IsSharedLibName` needs no `lib` prefix and accepts `libz.so.`. The RUNPATH
entry is the parent of the symlink-resolved path. If that resolves outside the store (and outside
the host dirs), the outside dir is added anyway.
**Impact**: Theoretical impurity. Nix store symlinks normally stay in the store.
**Recommendation**: document only.

### Q-DRV-17: Stale comments in the C++ sources

**Current behavior**: The comment above `TestDriverConf` says Mach-O gets "rpaths and @rpath ids",
but the code adds only `-Wl,-headerpad_max_install_names`, and the test asserts that there is no
`-rpath`. `driver.h` says `libc` is "always an rpath entry", but it is dropped when it is a host
dir.
**Impact**: Misleading for maintainers only. This document is authoritative.
**Recommendation**: document only.

## Open questions

1. Is counting non-store verbatim rpath parts (including empty ones) toward `kRunpathSlack`
   intentional, or should only store entries count? The current behavior is normative either way.
2. Should a `ekacache.json` lacking `cc` fall back to `$EKACACHE_CC` (Q-DRV-1), or is the hard failure
   intended with only the message wrong?
3. Is running ec with an `etc/ekacache.json` on a non-Linux host (no `/proc/self/exe`) a supported
   configuration (Q-DRV-2)? The code cannot do it today.
4. `ScanUserArgs` records `-o` into `UserArgs::output`, but nothing reads it. Is this dead code or
   unfinished? It has no observable effect, and the port may drop it.
5. Abort-on-type-error (Q-DRV-9) and terminate-on-`exists`-error (Q-DRV-10): may the Rust port emit
   a diagnostic before aborting, or must stderr stay byte-identical (empty, or libstdc++'s text)?
