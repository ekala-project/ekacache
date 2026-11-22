# ec cc mode

cc mode is ec's C/C++ compiler front end. It is reached through `cc`, `c++`, `gcc`, `g++`,
`clang`, `clang++`, any other unrecognised mode name, or launcher mode (`ec /path/to/gcc …`).
It decides whether an invocation can be served from the ecd cache. If it can, it computes a
request key (k1), looks up a manifest, validates it, derives the result key (k2) and replays
the stored artifacts. On a miss it runs the real compiler, learns the inputs from depfiles and
an absent-path log, and stores the result. Anything else runs the compiler unchanged. This
document specifies argument classification, the k1 field list, lookup and replay, storage, and
the GCC/clang differences, byte for byte, so that a Rust implementation can replace the C++
one. Hasher framing, `Store::Key`/`MaskOut`/`MaskForReplay`, the manifest format and the
k2 derivation are in `behaviors/ec-keys.md`. The socket protocol and value compression are in
`behaviors/ec-cache-client.md`. Environment variables are in `behaviors/environment.md`.
Driver flag injection (`etc/ekacache.json`) is in `behaviors/ec-driver-policy.md`.

Sources: `jig/src/cc_mode.h`, `jig/src/cc_mode.cc`, `jig/src/main.cc` (dispatch),
`jig/src/launcher.cc`, `jig/src/manifest.cc` (`ParseDepfile`, `BuildManifest`,
`ValidateManifest`, `FindResult`), `jig/src/process.cc` (`Slot`, `Run`, `Deterministic`),
`jig/src/base.cc` (`ExpandResponseFiles`, `WriteFile`, `ParseUint`, `LogOutcome`),
`jig/src/store.cc`, `jig/src/keys.h`, `jig/src/jig_test.cc` (`TestParse*`, `TestDepfile`,
`TestManifest`, `TestKeyEnvironment`, `TestParseGuards`), `spec/twokey.qnt`, and commits
`1e4484e` (GCC as the compiler) and `0f1e25a` (keys behind nixpkgs-style wrappers).

Key words MUST, MUST NOT, SHOULD, SHOULD NOT and MAY are as in RFC 2119. Current behavior is
normative: the Rust implementation MUST reproduce it unless the maintainer accepts a Quirk
entry's recommendation. Line numbers refer to `jig/src/cc_mode.cc` unless another file is named.

---

## Overview

### Pipeline

`RunCcMode(argv0, raw_args, socket_path, launched)` (`:645-706`) runs these steps in order:

1. `user_args = ExpandResponseFiles(raw_args)` (`:650`). Every `@file` that names a readable
   file is replaced by its words, tokenised GNU-style. An unreadable `@word` stays as it is.
2. Load the driver conf, pick the language and the compiler. The conf errors exit 1. See
   `ec-driver-policy.md` §Interface to cc mode, steps 2–6.
3. `inv = ParseInvocation(user_args)` (§Argument classification).
4. Mark the invocation uncacheable if it links (`link` or `link_one`) and `conf.binfmt != elf`,
   or if the language is Fortran (`:673-675`).
5. If `conf.present`: `inv.args = BuildDriverArgs(conf, lang, user_args)`. Every element of
   `inv.args` that is not byte-equal to some element of `user_args` is appended to
   `inv.key_args`, in order (`:676-683`).
6. If still cacheable: `primary = PrimaryIdentity(inv)` (§Primary identity). For a non-link,
   if `primary` mentions build time (`__DATE__`, `__TIME__` or `__TIMESTAMP__` as a
   substring), the invocation becomes uncacheable (`:686-690`).
7. If uncacheable, or `primary` is none, or `cache.Connect(socket_path)` fails, go to
   **RunUncached** (§Uncached runs).
8. `primary += "\ntoolchain=" + ToolchainIds(...)`, then compute k1 (§Request key).
9. `Lookup`: on success, **Replay** and return its status (§Lookup and replay).
10. Otherwise **CompileAndStore** with the lookup's error string as the reason (§CompileAndStore).

### Invocation kinds

| Kind | Flag in `Invocation` | Meaning | `mode=` field |
|---|---|---|---|
| compile | `compile_only` | `-c`, `-S` or `-E`: one translation unit in, one file out | `mode=compile` |
| link-one | `link_one` | one source straight to an executable, no object inputs (configure/cmake probes) | `mode=link-one` |
| link | `link` | object/archive inputs only, no source | `mode=link` |
| query | `query` | asks the compiler something (`-v`, `-print-*`, `-E` of nothing, `-cc1`) | never cached |

"links" below means `link || link_one`.

---

## Argument classification

### Suffix predicates

`HasSuffix(arg, list)` is true when `arg.size() > suffix.size()` and `arg` ends with the
suffix. A bare `.c` is therefore not a source (`:40-43`). All comparisons are case-sensitive.

| Predicate | Rule | Lines |
|---|---|---|
| `IsSourceFile` | does not start with `-`, suffix in `.c .cc .cpp .cxx .c++ .C .S .s .sx .m` | `:56-62` |
| `IsAssembly` | suffix in `.S .s .sx` | `:45-48` |
| `IsPlainAssembly` | suffix `.s` (not preprocessed) | `:51-54` |
| `IsObjectInput` | does not start with `-`, and suffix in `.o .os .lo .a .so .obj` **or** contains `.so.` anywhere | `:64-67` |
| `IsNoOutputOption` | exactly one of `-M -MM - -v -V --version -### -fsyntax-only`, or starts with `-print`, `--print` or `-dump` | `:80-87` |
| `HasLinkerSideOutput` | starts with `-Wl,` or `-Xlinker`, **and** (contains one of `-Map`, `--print-map`, `,-M,`, `--dependency-file`, `--out-implib`, `--trace`, `,-t,`, `--verbose`, `--print-gc-sections`, `--stats`, or ends with `,-M` or `,-t`) | `:69-77` |

`.mm`, `.i`, `.ii`, `.f`/`.f90` and similar are not sources. An argument such as `x.i` falls
into the "everything else" branch.

### The scan (`ParseInvocation`, `:586-643`)

`inv.args = args`. Counters: `objects = false`, `sources = 0`, `stop = 0`. For each index `i`,
the **first** matching rule applies:

| # | Condition on `arg = args[i]` | Effect | In `key_args`? |
|---|---|---|---|
| 1 | `TakeDepfileOption` matches (below) | depfile bookkeeping, may consume `args[i+1]` | no |
| 2 | `-c`, `-S` or `-E` | `compile_only = true`, `stop = arg[1]` | yes |
| 3 | `-o` and `i+1 < size` | `output = args[++i]` | no |
| 4 | starts with `-o`, length > 2 | `output = arg[2:]` (cgo's `-o/tmp/x.o`) | no |
| 5 | `IsNoOutputOption` | `cacheable = false`, `query = true` | no |
| 6 | `IsSourceFile` | `source = arg` (last wins), `++sources` | no |
| 7 | `arg == "-include-pch"` (`TakePchOption`) | see below | yes (all consumed words) |
| 8 | `IsObjectInput` | `objects = true`, append to `inputs` | yes |
| 9 | anything else | see below | yes |

Rule 9 (`:618-625`): `objects |= (arg == "-shared" || arg == "-r")`. The invocation becomes
uncacheable if `arg` starts with `@` (a response file that did not expand), contains `,@`
(`-Wl,@rsp`), satisfies `HasLinkerSideOutput`, or ends with `-header` (`-x c-header`,
`-x c++-header`).

**`TakeDepfileOption`** (`:452-467`) matches, sets `wants_depfile = true`, and returns true
for:

| Arg | Extra effect |
|---|---|
| `-MF` with a next arg | `depfile = args[++i]` |
| `-MT` or `-MQ` with a next arg | `depfile_target = args[++i]` (last wins, no quoting applied) |
| starts with `-Wp,-MD,` or `-Wp,-MMD,` | `depfile` = everything after the first `,` found at or after offset 5 (that is, after `-Wp,-MD,` / `-Wp,-MMD,`) |
| exactly `-MD`, `-MMD` or `-MP` | none |

`-MF`, `-MT` and `-MQ` as the last argument do not match. They fall through to rule 9.

**`TakePchOption`** (`:471-484`): it pushes `-include-pch` to `key_args`. Then, while the next
arg is `-Xclang`, it consumes and pushes that too. Then, if there is a next arg, it consumes
it, pushes it to `key_args` and appends it to `inv.pch`. So both `-include-pch x.pch` and
cmake's `-Xclang -include-pch -Xclang /b/x.pch` record the PCH (`jig_test.cc` `TestParsePch`).

Options whose value is a separate token are **not** paired, apart from `-o`, `-MF`, `-MT`, `-MQ`
and `-include-pch`. The value of `-I dir`, `-x c`, `-Xlinker foo.o` and so on goes through the
table on its own (Q-CC-9).

### After the scan

In this order:

1. Uncacheable if `output.extension()` is `.pch` or `.gch`, or `output == "/dev/null"`
   (`:629-630`).
2. If `args[0]` is `-cc1` or `-cc1as`: `cacheable = false`, `query = true` (`:633-636`).
3. `Classify(inv, sources, objects, stop)` (`:492-518`):
   - **compile_only**:
     - `cacheable &= (sources == 1)`;
     - `query |= (stop == 'E' && (sources == 0 || source == "/dev/null"))`;
     - if `stop == 'E'` and `output` is empty or `-`: `to_stdout = true` and
       `output = $TMPDIR/ekacache<pid>.i`. `$TMPDIR` defaults to `/tmp` when unset; set-but-empty
       gives a relative `ekacache<pid>.i` in the cwd (`path("") / name` is `name`);
     - otherwise, if `output` is empty: `output = stem(source) + (stop == 'S' ? ".s" : ".o")`.
       Only the stem is kept, so `-c dir/foo.c` writes `foo.o` in the cwd.
   - **otherwise**:
     - `link_one = !objects && sources == 1`;
     - `link = objects && sources == 0 && !inputs.empty()`;
     - `cacheable &= (link_one || link)`;
     - if `output` is empty: `output = "a.out"`;
     - if `link`: `source = output.filename()`, used only as the log label.
   - In both cases: if `wants_depfile` and `depfile` is empty,
     `depfile = output` with its extension replaced by `.d`.
4. If links and `args` contains the exact element `-fuse-ld=gold`: uncacheable (`:639-641`).

`stop` is the **last** of `-c`/`-S`/`-E` given.

### Cacheability summary

An invocation falls through to an uncached run if any of these is true:

- an `IsNoOutputOption` argument (`-M`, `-MM`, `-`, `-v`, `-V`, `--version`, `-###`,
  `-fsyntax-only`, `-print*`, `--print*`, `-dump*`);
- `-cc1`/`-cc1as` as the first argument;
- a compile with a number of sources other than 1;
- a non-compile that is neither link-one nor link (sources mixed with objects, `-shared a.c`,
  several sources, no inputs at all);
- `@…` left after expansion, `,@`, a linker side-output flag, an argument ending in `-header`;
- output `*.pch`, `*.gch` or `/dev/null`;
- a link with `-fuse-ld=gold`, or with `conf.binfmt != elf`;
- Fortran;
- an unreadable source, PCH or link input (`kPlainNoSource`);
- a non-link primary that contains `__DATE__`, `__TIME__` or `__TIMESTAMP__`;
- no daemon (`kPlainNoSocket`);
- Rust port only: the adopted rules A to D below.

### Additional uncacheable cases (adopted, Rust port)

Signed off 2026-11-17 (Q-CC-12 to Q-CC-16). The C++ jig caches all of these. The Rust port
MUST apply the rules below **in addition** to the summary above. Arguments are matched after
response-file expansion, against `args` (the user arguments, not the injected ldflags), and
the rules apply to every invocation kind, links included.

**A. Flags that write a second output** (Q-CC-14, Q-CC-15). A hit restores only the object,
depfile and stderr, so any of these makes the invocation uncacheable:

| Match | Side output |
|---|---|
| `--coverage`, `-ftest-coverage` | `.gcno` |
| `-gsplit-dwarf`, `-gsplit-dwarf=…` | `.dwo` |
| `-save-temps`, `-save-temps=…` | `.i`/`.ii`/`.s`/`.bc` |
| `-ftime-trace`, `-ftime-trace=…` | clang `.json` trace |
| `-fstack-usage` | `.su` |
| `-fcallgraph-info`, `-fcallgraph-info=…` | `.ci` |
| starts with `-fdump-` | GCC dump files |
| starts with `-fopt-info` and contains `=` | optimisation report file |
| `-fsave-optimization-record`, `-fsave-optimization-record=…`, `-foptimization-record-file=…` | `.opt.yaml` |
| `-MJ`, starts with `-MJ` | compilation-database fragment |
| `-fdiagnostics-format=sarif-file`, starts with `-fdiagnostics-add-output=` | `.sarif` |
| a `-Wa,` argument with a comma-separated element that starts with `-a` and contains `=` | assembler listing |
| **C++ modules:** `-fmodules-ts`, `-fmodules`, `-fmodule-output`, starts with `-fmodule-output=`, `-fmodule-file=`, `-fmodule-mapper=`, `-fprebuilt-module-path=`, `-fdeps-`; `--precompile` | `gcm.cache/*.gcm`, `.pcm`, `.ddi` (outputs), or a module file read that may not appear in the depfile (input) |

**B. Flags that name inputs missing from the depfile** (Q-CC-13). The key holds only the path,
not the contents:

| Match | Hidden input |
|---|---|
| `-fprofile-use`, starts with `-fprofile-use=`, `-fprofile-instr-use=`, `-fprofile-sample-use=`; `-fauto-profile`, starts with `-fauto-profile=` | profile data (`.gcda`, `.profdata`, AutoFDO) |
| starts with `-fplugin=`, `-fpass-plugin=`; `-Xclang` followed by `-load` or `-plugin` | compiler plugin |
| starts with `-specs=` or `--specs=` | GCC spec file |

**C. `.incbin` in assembler sources** (Q-CC-12). For a primary source whose extension is
`.s`, `.S` or `.sx`:
- **before lookup:** if the primary source contains the bytes `.incbin`, the invocation is
  uncacheable;
- **on a validated hit:** if any input path of the stored manifest contains the bytes
  `.incbin`, treat it as a miss (log subject `<source> incbin`), and do not store;
- **before storing:** if the primary source or any input in the new manifest contains
  `.incbin`, do not store anything.

The check is a plain byte search: a match inside a comment also counts, and that only costs
caching. The hit-time check guards against entries written by the C++ jig. cc keys have no
schema version (Q-KEY-10), so those entries are reachable from the Rust port.

**D. Temp-dir compiles from other toolchains** (Q-CC-16). Uncacheable if the primary source,
or any element of `args`, contains a path component matching one of these (ASCII, whole
component):

| Pattern | Producer |
|---|---|
| `go-build[0-9]+` | Go's `$WORK` (cgo) |
| `ghc[0-9]+_[0-9]+` | GHC temp dir |
| `tmpxft_` followed by anything | nvcc |
| `camlasm` followed by anything | ocamlopt assembly |

These compiles never hit (the random path is in the key) and would only push useful entries
out of the cache. The rule matches names, not `$TMPDIR`: inside the Nix sandbox `TMPDIR` is
`/build`, where the project sources live too.

In all four cases the uncached run behaves exactly like today's `uncacheable` path (slot taken,
compiler run, result forwarded). The log subject is `uncacheable`, except for C's hit-time case.

Test vectors: `jig_test.cc` `TestParseJoinedOutput`, `TestParseLink`, `TestParseInvocation`,
`TestParsePch`, `TestParsePreprocess`, `TestParseGuards`.

---

## Primary identity

`PrimaryIdentity(inv)` (`:167-190`) returns none if an input is unreadable:

- **non-link**: the source file's bytes. Then, for each `pch` in order:
  `"\npch=" + Store::InputId(pch)`.
- **link**: for each `inputs[i]` in order: `input + "=" + InputId(input) + "\n"`.

`ToolchainIds(cache, compiler, inv)` (`:131-164`):

1. `tools = [OnPath(compiler)]`. For links, also append the value of every `inv.args` element
   that starts with `--ld-path=`.
2. For each tool: `real = canonical(tool)`, skipped on error. Add `real`, then every directory
   entry of `real.parent_path().parent_path()/lib` whose filename starts with `libLLVM`,
   `libclang-cpp` or `liblld`, and which is a regular file and not a symlink.
3. Sort the list bytewise and remove duplicates. Run one `PrefetchIdentities` (an IDS round
   trip).
4. Result: the concatenation of `InputId(file)` (or `?` if unreadable), each followed by `,`.

Then `primary += "\ntoolchain=" + ToolchainIds` (`:697`).

## Request key (k1)

`ComputeRequestKey(compiler, inv, primary)` (`:98-128`). The `Hasher` fields, in order. Framing
and the meaning of `Key`, `MaskOut` and `MaskForReplay` are defined in `ec-keys.md`.

| # | Field | Present |
|---|---|---|
| 1 | `"cc=" + ToolId(compiler)` | always |
| 2 | `"cwd=" + Key(current_path())` | always |
| 3 | `"src=" + Key(MaskOut(inv.source))` | not `link` |
| 4 | `"mode=compile"` / `"mode=link"` / `"mode=link-one"` | always |
| 5 | `"LIBRARY_PATH=" + MaskForReplay(Env("LIBRARY_PATH"))` | links |
| 6 | `"env=" + MaskForReplay(MaskOut(KeyEnvironment(environ, links)))` | always |
| 7… | for each `a` in `key_args`: `MaskOut(a)` if links and `a` contains `dynamic-linker` or `rpath`, else `Key(MaskOut(a))` | always |
| last | `MaskOut(primary)` | always |

k1 text = the digest's hex with **no** prefix (`Tool::kCc`, `keys.h`). The manifest key is
`m/<k1>`.

`compiler` here is the program cc mode runs: `conf.cc`/`conf.fc`, or the launched path in
launcher mode. `ToolId` resolves only the directory (`store.h`).

### `KeyEnvironment(environ_entries, links)` (`:556-580`)

For each `NAME=value` entry (`NAME` = text before the first `=`), keep the entry if:

- `NAME` is one of `CPATH`, `C_INCLUDE_PATH`, `CPLUS_INCLUDE_PATH`, `OBJC_INCLUDE_PATH`,
  `GCC_EXEC_PREFIX`, `COMPILER_PATH`; or
- `links` and `NAME` starts with `NIX_` and is not one of `NIX_BUILD_TOP`, `NIX_BUILD_CORES`,
  `NIX_LOG_FD`, `NIX_ATTRS_JSON_FILE`, `NIX_ATTRS_SH_FILE`, `NIX_SSL_CERT_FILE`, `NIX_REMOTE`,
  `NIX_PATH`, `NIX_PROFILES`, `NIX_USER_PROFILE_DIR`.

Sort the kept entries bytewise as whole strings. Output each entry followed by `\n`.
Test vector (`TestKeyEnvironment`): from `PATH=/bin`, `NIX_HARDENING_ENABLE=bindnow relro`,
`NIX_BUILD_TOP=/build`, `CPATH=/inc`, `NIX_LDFLAGS=-rpath /nix/store/x/lib`, `NIX_LOG_FD=2`,
`NIX_ATTRS_SH_FILE=/build/.attrs.sh`:

- compile → `CPATH=/inc\n`
- link → `CPATH=/inc\nNIX_HARDENING_ENABLE=bindnow relro\nNIX_LDFLAGS=-rpath /nix/store/x/lib\n`

### What `key_args` contains

Every user argument except: depfile options and their values, `-o` and its value, `-oX`,
`IsNoOutputOption` arguments, and source files. `-c`/`-S`/`-E`, PCH words, object inputs and
everything else are kept in argv order. Driver-injected arguments not byte-equal to a user
argument follow them (`:676-683`). Vector: `-O2 -c foo.c -o out/foo.o -MD -MF out/foo.d -MT x`
→ `key_args = [-O2, -c]` (`TestParseInvocation`).

---

## Lookup and replay

### Lookup (`:201-235`)

1. `FindResult(cache, k1)` (`manifest.cc:172-178`): `GET m/<k1>`. A miss gives the error
   `new-key`. Otherwise `ValidateManifest` gives k2, or the error `inputs-changed:<path>` or
   `appeared:<path>` (`ec-keys.md`).
2. One pipelined `GetMany` of exactly these four keys, in this order:
   `r/<k2>`, `o/<k2>`, `d/<k2>`, `e/<k2>`.
3. `status = 0`. If **not** links and `r/` is present:
   `status = (int) ParseUint(r_value).value_or(1)` (`:222`). `ParseUint` trims spaces and tabs
   only, accepts decimal digits only, and fails on anything else: empty, a sign, a newline,
   overflow. **A value that does not parse means status 1** (Q-CC-1). For links, `r/` is
   ignored.
4. If `status == 0`: fail with `object-gone` if `o/` is absent, or if `wants_depfile` and `d/`
   is absent. Otherwise `object = UnmaskOut(o)`, and `depfile = UnmaskOut(d)` if
   `wants_depfile`, else none.
   If `status != 0`, `o/` and `d/` are ignored even when present.
5. `stderr_text = UnmaskOut(e or "")`. A missing `e/` is not an error.

### Artifact slots under k2

All values are stored zstd-compressed on the wire by the client (`ec-cache-client.md`). The
table gives the uncompressed value.

| Key | Written when | Value (exact bytes) |
|---|---|---|
| `m/<k1>` | every store | manifest text (`ec-keys.md`) |
| `o/<k2>` | success | `MaskOut(output file bytes)`: the object (`-c`), the text (`-S`, `-E`), or the executable (links) |
| `e/<k2>` | every store | `MaskOut(captured compiler stderr)`, possibly empty |
| `d/<k2>` | success and `wants_depfile` | `MaskOut(MaskForReplay(dep_text))`: the build system's depfile as the compiler wrote it, after the GCC/clang adjustments in §Running the compiler |
| `r/<k2>` | deterministic failure | `std::to_string(status)`: ASCII decimal, no sign, no newline, for example `1` |

A success never writes `r/`, and a failure never writes `o/` or `d/`.

### Replay (`:261-283`)

In this order:

1. **Output**:
   - If `to_stdout`, `ForwardStdout` (`:238-248`): remove `inv.output` (the temp path,
     ignoring errors). If `object` is present, write it to stdout and flush. A replayed
     failure has no object, so nothing is printed.
   - Else, if `object` is present: `WriteFile(inv.output, object)` (temp
     `<dir>/.ekacache<pid>.<name>` + rename, `base.cc:90-102`). On failure, print
     `ec: cannot write <output>: <strerror(errno)>\n` to stderr and **return 1** without
     printing the cached stderr. On success, for links, add `u+x,g+x,o+x` to the file mode,
     ignoring errors. The base mode comes from `ofstream` creation (`0666 & ~umask`), not
     from the original compiler run.
2. **Depfile**: if `depfile` is present, write
   `RetargetDepfile(Store::ResolveAll(depfile), inv)` to `inv.depfile`. On failure, print
   `ec: cannot write <depfile>: <strerror(errno)>\n` and return 1.
3. **stderr**: write `stderr_text` to stderr verbatim.
4. Return `status`.

The EKACACHE_LOG outcome is `cached` (`kHit`) if the returned status is 0, else `cached-error`
(`kHitFail`). The subject is `inv.source` (`:700-703`).

#### `RetargetDepfile(text, inv)` (`:252-259`)

`target = depfile_target` if non-empty, else `inv.output` as a string. Let `colon` be the
first occurrence of the two bytes `": "`. If it exists and the first `\n` (or end of text)
comes after it, replace bytes `[0, colon)` with `target`. Otherwise leave the text unchanged.
Only the first rule's head is rewritten. Prerequisites and `-MP` phony rules stay as they are.

---

## CompileAndStore

### Running the compiler (`RunObserved`, `:306-381`)

Start with `args = inv.args` and derive the temp paths:

| Name | Value |
|---|---|
| `out_dir` | `inv.output.parent_path()` if it has one, else `.` |
| `tmp_base` | `out_dir/.ekacache<pid>` |
| `own_depfile` | `!wants_depfile && !link` |
| `depfile` | `tmp_base + ".d"` if `own_depfile`, else `inv.depfile` |
| `link_depfile` | `tmp_base + ".link.d"` |
| `absent_log` | `tmp_base + ".absent"` |

Arguments appended, in this order:

1. If `own_depfile` and the source is not `.s`: append `-MD -MF <depfile>`.
   Else if `!own_depfile && !link && !IsAssembly(source)` (the build system asked for a
   depfile):
   - **clang** (`IsClangDriver`: the compiler's basename contains `clang`, `:286-288`):
     append `-Xclang -sys-header-deps`;
   - **otherwise (GCC)**: rewrite in place every arg equal to `-MMD` to `-MD`, and every arg
     starting with `-Wp,-MMD,` to `-Wp,-MD,` + the rest (`:324-332`).
2. If `to_stdout`: append `-o <inv.output>` (the temp `.i` path).
3. If links: append `-Wl,--dependency-file=<link_depfile>`.

Then, within one scope (`:343-348`):

- acquire a `Slot(cache, "")`. The client is already connected, so the slot uses the same
  connection. It is a no-op if `EKACACHE_SLOT` is set or there is no daemon (`process.cc:468-481`);
- `setenv("EKACACHE_ABSENT_LOG", absent_log, 1)`;
- `Run(compiler, args, kCapture)`. stdout is inherited, stderr is captured;
- `unsetenv("EKACACHE_ABSENT_LOG")`. This also clears an inherited value;
- the slot is released (**DONE**) when the scope ends, **before** any output is read or any
  PUT is sent (Q-CC-2).

After the scope:

1. Write the captured stderr to stderr verbatim, all at once, after the compiler exits
   (`:349`).
2. `dep_text`:
   - `link` → `""`;
   - `own_depfile` and the source is `.s` → `"o: " + source`;
   - otherwise → `ReadFile(depfile)`, none if unreadable.
3. `link_dep_text = ReadFile(link_depfile)` for links, else none.
4. If `absent_log` is readable: `absent = Split(text, '\n')`. Empty lines are dropped.
5. Remove `absent_log`, `depfile` if `own_depfile`, and `link_depfile`, ignoring errors.
6. `inputs = ParseDepfile(dep_text)` (prerequisites of the first rule only, `manifest.cc:23-62`).
   Then, for each prerequisite of `ParseDepfile(link_dep_text)`, append it unless it is a
   driver temp object: it starts with `temp_directory_path() + "/"` and contains no further
   `/` (`:370-378`).

`Run` returns 127 if exec fails, `128+N` for death by signal N, else the exit status
(`process.cc:490-545`).

`EKACACHE_ABSENT_LOG` is written only by the project's patched clang (one path per line, for
lookups that missed). Stock GCC and stock clang ignore it, so their manifests have no `!`
lines (Q-CC-3).

### Build-time check (`:391-397`)

For non-links, `build_time` is true if any input that is not a store path
(`Store::IsStorePath`) is readable and contains `__DATE__`, `__TIME__` or `__TIMESTAMP__`.

### Failure (`run.status != 0`, `:399-413`)

1. `ForwardStdout(inv, none)`: in `to_stdout` mode, remove the temp file and print nothing.
2. **Not stored** (outcome `failed`, `kMissFail`) if any of these holds:
   - `dep_text` is none;
   - links. Link failures are never stored;
   - `build_time`;
   - `MissingInput(stderr)`: stderr contains `file not found` (clang) or
     `No such file or directory` (GCC) (`:292-294`);
   - `!Deterministic(run)`: status ≥ 128, or stderr contains `unable to execute command` or
     `PLEASE submit a bug report` (`process.cc:547-550`). The Rust port adds the resource-error strings of Q-CC-5 (adopted).
3. Otherwise: `manifest = BuildManifest(cache, k1, inputs, inv.source, absent)`, then three
   sequential PUTs, each waiting for its `OK`:
   1. `m/<k1>` = manifest text
   2. `r/<k2>` = `to_string(status)`
   3. `e/<k2>` = `MaskOut(stderr)`

   Outcome `compiled-error` (`kMissStoredFail`).
4. Return `run.status`.

### Success (`:415-430`)

1. `object = ReadFile(inv.output)`. Then `ForwardStdout(inv, object)`: in `to_stdout` mode,
   remove the temp file and print the text.
2. **Not stored** (outcome `compiled-unstored`, `kMissUnstored`, return 0) if `dep_text` is
   none, `object` is none, links and `link_dep_text` is none, or `build_time`.
3. Otherwise: `BuildManifest(cache, k1, inputs, inv.source, absent)`, then sequential PUTs:
   1. `m/<k1>` = manifest text
   2. `o/<k2>` = `MaskOut(object)`
   3. `e/<k2>` = `MaskOut(stderr)`
   4. if `wants_depfile`: `d/<k2>` = `MaskOut(MaskForReplay(dep_text))`

   Outcome `compiled` (`kMissStored`). Return 0.

The manifest is PUT first in both cases. A concurrent reader that sees `m/` before `o/` gets
`object-gone` and recompiles, which is harmless. `primary_source` is compared to manifest
inputs as a string, so a depfile that spells the source differently (`./x.c`) also lists it
in the manifest.

The EKACACHE_LOG subject on a miss is `<inv.source> <why>`, where `<why>` is the Lookup error:
`new-key`, `inputs-changed:<path>`, `appeared:<path>` or `object-gone`.

---

## Uncached runs (`RunUncached`, `:521-552`)

1. If `inv.source` or `inv.inputs` is non-empty, hold a `Slot(cache, socket_path)` for the
   run. Queries such as `-print-*` and `--version` take no slot.
2. If `user_args` contains the exact element `-print-search-dirs`: run with stdout captured,
   write `WithPackageLibDirs(stdout, ldflags)` to stdout and flush
   (`ec-driver-policy.md` A13). Otherwise run with stdio inherited.
3. The outcome is the first that applies: `query`, then `uncacheable` (compile) or `linked`
   (otherwise) if not cacheable, then `uncacheable-nosrc` if there is no primary, else
   `no-daemon`.
4. `LogOutcome`. Then, unless the outcome is `query`, append
   `<outcome name>\t<user_args joined by " ">\n` to `$EKACACHE_LOG_ARGS` if it is set and non-empty
   (`:433-444`).
5. Return the compiler's status.

`EKACACHE_ABSENT_LOG` is not set on this path.

---

## GCC versus clang

| Aspect | clang (basename contains `clang`) | GCC (anything else) |
|---|---|---|
| Build-system depfile with system headers | append `-Xclang -sys-header-deps` | rewrite `-MMD`→`-MD`, `-Wp,-MMD,`→`-Wp,-MD,` |
| Effect on the user's depfile | lists system headers (also on a hit, from `d/`) | lists system headers; `-MMD` is silently upgraded |
| Absent lookups (`EKACACHE_ABSENT_LOG`) | written by the patched clang only | never written: no `!` lines |
| Missing-input text | `file not found` | `No such file or directory` |
| Detection | by the **compiler** path's basename, not argv0 | |

Language selection uses argv0 (`++` → C++), not the compiler (`ec-driver-policy.md`).

### "Wrapper" handling (commit 0f1e25a)

No wrapper is detected. Commit `0f1e25a` makes keys correct when ec runs between nixpkgs'
cc-wrapper and the compiler (launcher mode), with the bintools wrapper still running after
it:

- links hash every non-volatile `NIX_*` variable (`KeyEnvironment`), because the bintools
  wrapper turns them into linker flags ec never sees;
- compiles hash only the variables the compiler reads (`CPATH` and friends);
- a source or build-tree header that mentions `__DATE__`/`__TIME__`/`__TIMESTAMP__` is never
  stored, because a package set may derive `SOURCE_DATE_EPOCH` from the newest source;
- `-cc1`/`-cc1as` and `-fuse-ld=gold` links are never cached.

Launcher mode (`launcher.cc:88-105`) sends a program to cc mode when its basename, after any
trailing `-<digits and dots>` is stripped, is `cc`, `c++`, `gcc`, `g++`, `clang` or `clang++`,
or ends in `-` + one of those (`x86_64-unknown-linux-gnu-gcc`). It calls
`RunCcMode(basename, args, socket, program)`, which means conf-less mode
(`ConfLessDriverConf`, `binfmt = macho` on Apple builds, otherwise ELF).

---

## Soundness model

`spec/twokey.qnt` models the k1→manifest→k2 scheme. It covers absent-path tracking, deterministic
failure caching and the rule that a MissingInput failure is never stored. Its `ABSENT_LOG`
parameter set to `false` corresponds to GCC (Q-CC-3). `MUTATE_DURING_COMPILE` set to `true`
corresponds to Q-CC-4. The model does not cover links, `build_time` or `Deterministic`.

---

## Errors and messages

| Situation | stderr (exact) | Exit |
|---|---|---|
| no conf and no `$EKACACHE_CC` | `ec: no etc/ekacache.json next to the binary and EKACACHE_CC unset\n` | 1 |
| Fortran without `fc` | `ec: <name> called but etc/ekacache.json names no fc\n` | 1 |
| replay cannot write output | `ec: cannot write <path>: <message>\n` | 1 |
| replay cannot write depfile | `ec: cannot write <path>: <message>\n` | 1 |
| everything else | the compiler's or the cached stderr, verbatim | compiler's or cached status |

`<message>` is `std::error_code(errno, generic_category()).message()`, the C library's
`strerror` text.

---

## Quirks

### Q-CC-1: A cached exit status that does not parse becomes 1

**Current behavior**: `r/<k2>` is parsed with `ParseUint(...).value_or(1)` (`:222`). An empty,
signed, newline-terminated or otherwise malformed value replays as status 1. A value of `0`
replays as a success (and then needs `o/`). A value above `INT_MAX` is truncated by
`static_cast<int>`, and the process exit code is that int modulo 256. For example, `256` is
treated as a failure (no output written) yet exits 0.

**Impact**: none with values ec writes: always `1`–`127`, because signal statuses are not
stored. It matters only for corrupt or foreign entries.

**Recommendation**: keep as-is. The Rust implementation MUST parse with the same rules.

### Q-CC-2: DONE is sent before the PUTs

**Current behavior**: the slot is released when the compiler exits (`:343-348`). Reading the
output, hashing every manifest input and the three or four PUTs happen after DONE.

**Impact**: more concurrent work than `EKACACHED_SLOTS` allows (hashing large objects, sending
them). A waiting compiler starts while the previous one is still uploading.

**Recommendation**: **ADOPTED (signed off 2026-11-17).** DONE MUST be sent after the last PUT's reply has been read (or
the connection has died), in cc mode and in rustc mode alike (`ec-rustc-mode.md` §Compile and
store). The slot then also covers hashing and upload, so `EKACACHED_SLOTS` bounds all heavy work.
ecd needs no change. The cost is slightly lower compiler concurrency while uploads run.

### Q-CC-3: GCC compiles have no absent-path tracking

**Current behavior**: only the patched clang writes `EKACACHE_ABSENT_LOG`. With GCC, or stock clang,
`absent` is empty and manifests have no `!` lines.

**Impact**: the `twokey.qnt` counterexample with `ABSENT_LOG = false`. A header that later
appears earlier on the include path (`-Ia` shadowing `-Ib`) is not noticed, and a stale
object is replayed.

**Recommendation**: change, needs sign-off. Consider making GCC compiles uncacheable when any
`-I`/`-iquote` directory is outside the store, or using GCC's `-H`/`-MG` to find missed
lookups. Until then, document it.

### Q-CC-4: Manifest identities are taken after the compiler exits

**Current behavior**: `BuildManifest` hashes inputs after `Run` returns. An input edited
between the compiler reading it and ec hashing it is recorded with its new identity.

**Impact**: `twokey.qnt` `MUTATE_DURING_COMPILE`. This needs a concurrent writer in the build
tree.

**Recommendation**: keep as-is.

### Q-CC-5: Transient failures can be cached as deterministic

**Current behavior**: any non-zero exit below 128 whose stderr does not contain `file not
found`, `No such file or directory`, `unable to execute command` or `PLEASE submit a bug
report` is stored (`:403`). That includes `No space left on device`, `Cannot allocate memory`
and `Too many open files`. A stored `r/` is never removed by a later success, because a hit
replays the failure and no recompile happens until eviction.

**Impact**: a disk-full or memory-pressure failure is replayed in later builds with the same
inputs.

**Recommendation**: **ADOPTED (signed off 2026-11-17).** A failed compile MUST NOT be stored if its stderr
contains any of these byte strings (added to the `!Deterministic` list, `process.cc`):
`No space left on device`, `Disk quota exceeded`, `Cannot allocate memory`,
`virtual memory exhausted`, `out of memory`, `Too many open files`, `Input/output error`,
`Read-only file system`, `Resource temporarily unavailable`. A false positive (a deterministic
error whose text contains one of them) only loses caching for that invocation. Keys and stored
formats are unchanged.

### Q-CC-6: `-MT`, `-MQ`, `-MP` or `-MF` alone imply a build-system depfile

**Current behavior**: `TakeDepfileOption` sets `wants_depfile` for any of these, even without
`-MD`/`-MMD`. ec then neither adds its own `-MD -MF` nor asks the compiler for one, and
reads `inv.depfile` (default `<output stem>.d`) after the run.

**Impact**: if no such file exists, the result is `compiled-unstored`. If a **stale** file from
an earlier build exists, its prerequisites become the manifest, which may be wrong. A hit then
also writes a depfile the compiler would not have written.

**Recommendation**: change, needs sign-off. Set `wants_depfile` only for `-MD`, `-MMD` and
`-Wp,-M[M]D,`.

### Q-CC-7: Replayed depfiles have store hashes masked in path mode

**Current behavior**: `Replay` writes `ResolveAll(depfile)`. `Store::ResolveAll`
(`store.cc:170-185`) calls `MaskHashes` unconditionally, then maps masked roots back through
`masked_to_real_`, which is empty in path mode (`EKACACHE_STORE_IDENTITY` unset or `path`,
`store.cc:29-32`). Every `/nix/store/<hash>-name/…` in a replayed depfile therefore becomes
`/nix/store/*-name/…`. A miss writes the real paths.

**Impact**: in the default identity mode, a depfile written by a hit names nonexistent
prerequisites. Without `-MP`, make fails with "No rule to make target". ninja treats the edge
as always dirty.

**Recommendation**: **ADOPTED (signed off 2026-11-17).** See `ec-keys.md` Q-KEY-1: in path mode a replayed depfile is written exactly as stored (real paths).

### Q-CC-8: The `-E` to stdout depfile target is a temp path

**Current behavior**: for `cc -E -MD x.c` without `-o`, `inv.output` is `$TMPDIR/ekacache<pid>.i`.
The default depfile is `$TMPDIR/ekacache<pid>.d`, and `RetargetDepfile` writes that temp path as
the rule target. On a miss, `-o <tmp>` is appended to an argument list that may already
contain `-o -`.

**Impact**: rare (preprocessor probes with depfiles). The target name varies by pid.

**Recommendation**: keep as-is.

### Q-CC-9: Separate-value options are not paired

**Current behavior**: only `-o`, `-MF`, `-MT`, `-MQ` and `-include-pch` consume their value.
`-Xlinker -Map -Xlinker out.map` escapes `HasLinkerSideOutput`, because only `-Wl,…`-style
glued forms are checked. `-Xlinker foo.o` and `-include x.c` count as object input and source.

**Impact**: a linker map side output is not reproduced on a hit. An argument may be
misclassified.

**Recommendation**: keep as-is. Document it for the Rust port.

### Q-CC-10: `.S`/`.sx` with a build-system `-MMD` miss system headers

**Current behavior**: the clang `-sys-header-deps` / GCC `-MD` upgrade is skipped for
`IsAssembly` sources (`:318`), so a preprocessed assembly file built with `-MMD` records no
`-isystem` headers in its manifest.

**Impact**: in content-identity mode, a changed system header is not noticed for such files.

**Recommendation**: keep as-is. Low impact, because store headers are immutable per path.

### Q-CC-11: The replayed file mode differs from a real run

**Current behavior**: `WriteFile` creates with `0666 & ~umask`. Links get `+x` added. The
compiler's own mode (for example a `-c` output from a compiler that honours a different umask)
is not stored.

**Impact**: negligible.

**Recommendation**: keep as-is.

---

### Q-CC-12: `.incbin` data is not an input

**Current behavior**: an assembler directive `.incbin "file"` (in `.s`, or `.S` after
preprocessing) makes the assembler read `file`, but the depfile does not list it. Verified with
GCC 14: `gcc -c b.S -MD` lists only `b.S` and `stdc-predef.h`. The manifest therefore lacks the
file, and a changed `file` still hits.

**Impact**: a stale object with the old embedded data is replayed. The popular `incbin.h`
header and firmware or kernel builds use this.

**Recommendation**: **ADOPTED (signed off 2026-11-17).** See §Additional uncacheable cases,
rule C.

### Q-CC-13: Flags that name hidden inputs

**Current behavior**: `-fprofile-use[=path]` and the clang profile flags, `-fplugin=`, and
`-specs=` name files the compiler reads but the depfile does not list. Verified with GCC 14 for
`-fprofile-use=<dir>`: the depfile has only the source and `stdc-predef.h`. Only the flag
text, and so the path, is in k1.

**Impact**: with new profile data (a PGO rebuild, or another build's profile at the same path)
an object optimised with the old profile is replayed. The code is still correct, but the build
is not reproducible. A changed plugin or spec file can change code generation arbitrarily.

**Recommendation**: **ADOPTED (signed off 2026-11-17).** See §Additional uncacheable cases,
rule B.

### Q-CC-14: Side outputs are lost on a hit

**Current behavior**: ec stores only the object, depfile, stderr and status. Flags that make
the compiler write more files (`--coverage` → `.gcno`, `-gsplit-dwarf` → `.dwo`, both verified
with GCC 14; `-save-temps`, `-ftime-trace`, `-fstack-usage`, `-fdump-*`, `-MJ`, …) are not
recognised, so such invocations are cached.

**Impact**: after a hit the extra files are missing. Coverage tools fail, split-DWARF debug
info is incomplete, and compilation-database fragments are missing.

**Recommendation**: **ADOPTED (signed off 2026-11-17).** See §Additional uncacheable cases,
rule A.

### Q-CC-15: C++20 modules are not handled

**Current behavior**: nothing in the scan knows about modules. A GCC `-fmodules-ts` compile of
an interface unit with a `.cpp`/`.cc` extension is cacheable, although it also writes
`gcm.cache/<name>.gcm`. CMake's scan step (`-fdeps-file=…`, `-fdeps-format=p1689r5`) writes a
`.ddi` file. A consumer reads BMIs (`-fmodule-file=`, module mappers) that the depfile may not
list. `.cppm`/`.ixx` sources are already uncacheable, because they are not source extensions.

**Impact**: with CMake 3.28+ and GCC 14+ or clang 16+, a hit on an interface unit leaves no BMI,
and every importer fails to compile. A hit on the scan leaves no `.ddi`, and ninja's dyndep
step fails.

**Recommendation**: **ADOPTED (signed off 2026-11-17).** See §Additional uncacheable cases,
rule A (modules row). Caching modules properly (storing the BMI as another artifact) is
future work.

### Q-CC-16: Compiles of other toolchains' temp files pollute the cache

**Current behavior**: ocamlopt (`gcc -c /build/camlasm123.s`), GHC (`/build/ghc1234_0/ghc_1.c`),
nvcc (`tmpxft_…`) and cgo (`-I $WORK/b001` with `$WORK=/build/go-build…`) call the C compiler on
randomly named files. `.s` and `.c` are sources, so these are cacheable. Every compile is a miss
followed by 3 to 4 PUTs that can never hit again.

**Impact**: no wrong results, but wasted daemon bandwidth and eviction of useful entries.
`MaskRandomNames` covers only CMake's `TryCompile-` and `cmTC_`.

**Recommendation**: **ADOPTED (signed off 2026-11-17).** See §Additional uncacheable cases,
rule D. Masking these names instead would not produce hits, because the manifest would then
name inputs under random directories that cannot be resolved later.

## Open questions

1. Is path identity mode (Q-CC-7) used in practice with make/ninja depfiles, or do all real
   builds run with `EKACACHE_STORE_IDENTITY=content`?
2. With `-E -o -`, the observed run's argument list contains both `-o -` and the appended
   `-o <tmp>`. Both clang and GCC appear to take the last `-o`. Is this relied on, and is it
   true for every supported GCC version?
3. The patched clang's `EKACACHE_ABSENT_LOG` format is assumed to be one path per line,
   `\n`-separated, relative paths against the cwd. The patch is not in this repository.
4. Should `r/` be cleared, or ignored, when a later run under the same k2 succeeds (Q-CC-5)?
   Today that cannot happen without eviction, because the failure is replayed.
5. C23 `#embed`: do GCC 15 and clang 19+ list embedded files in the depfile? If not, sources
   that use `#embed` need a rule like Q-CC-12. Verify before the Rust port ships.
6. The rule D patterns come from known tool behaviour, not from traces of real builds. A survey
   of `EKACACHE_LOG` output across nixpkgs (sources whose k1 never hits) would confirm or extend the
   list.
