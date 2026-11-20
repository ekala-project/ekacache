# C and C++

## What is cached

| Kind | Example | Cached |
|---|---|---|
| Compile | `cc -c foo.c -o foo.o`, `-S`, `-E` to a file or stdout | yes |
| Single-source link | `cc conftest.c -o conftest` (configure and CMake probes) | yes |
| Link of objects and archives | `cc a.o b.o libx.a -o prog`, `-shared` from `.o` | yes, ELF only |
| Deterministic compile failure | a syntax error | yes, the exit status and stderr are replayed |
| Link failure | an undefined symbol | no |

A hit replays the output file, the dependency file (if the build asked for one) and the
compiler's stderr, including warnings, then exits with the stored status.

**Source files** are recognised by extension: `.c`, `.cc`, `.cpp`, `.cxx`, `.c++`, `.C`,
`.S`, `.s`, `.sx` and `.m`. Others, including `.mm`, `.i`, `.ii`, `.cu`, `.cppm`, `.ixx` and
all Fortran, are passed straight to the compiler.

## What runs uncached

These invocations always run the real compiler:

- **Queries:** `-M`, `-MM`, `-v`, `-V`, `--version`, `-###`, `-fsyntax-only`, `-print*`,
  `-dump*`, `-E` with no source. These also take no slot.
- **Unusual shapes:** more or fewer than one source in a compile, sources mixed with
  objects, no inputs, `-cc1`, unexpanded response files, precompiled-header output (`.pch`,
  `.gch`), output to `/dev/null`.
- **Flags that write side files** `ec` cannot capture: `--coverage`, `-ftest-coverage`,
  `-gsplit-dwarf`, `-save-temps`, `-ftime-trace`, `-fstack-usage`, `-fdump-*`,
  `-fsave-optimization-record`, `-MJ`, SARIF diagnostics, `-Wa,-a…=` listings, linker
  `-Map`/`--dependency-file`/`--trace` in `-Wl,` form, and all C++20 module flags.
- **Flags with inputs `ec` cannot see:** `-fprofile-use`, `-fprofile-instr-use=`,
  `-fprofile-sample-use=`, `-fauto-profile`, `-fplugin=`, `-fpass-plugin=`,
  `-Xclang -load`, `-Xclang -plugin`, `-specs=`.
- **Assembly with `.incbin`** in the source or anything it includes.
- **Temporary-directory compiles from other toolchains:** cgo (`go-build…`), GHC
  (`ghc…_…`), nvcc (`tmpxft_…`), ocamlopt (`camlasm…`). They would never hit.
- **Timestamps:** a source that mentions `__DATE__`, `__TIME__` or `__TIMESTAMP__`. If a
  header in the build tree mentions them, the compile runs and the result is not stored.
- **Links** with `-fuse-ld=gold`, and all Mach-O and COFF links.

Set `EKACACHE_LOG_ARGS` to a file to record the full command line of every uncached
invocation (see [Monitoring](../operating/monitoring.md)).

## Failures that are not stored

A failing compile is replayed on later hits, so `ec` declines to store failures that look
environmental:

- exit by signal (status 128 or more);
- stderr containing `file not found`, `No such file or directory`,
  `unable to execute command` or `PLEASE submit a bug report`;
- stderr naming resource exhaustion: `No space left on device`, `Disk quota exceeded`,
  `Cannot allocate memory`, `virtual memory exhausted`, `out of memory`,
  `Too many open files`, `Input/output error`, `Read-only file system`,
  `Resource temporarily unavailable`.

Any other failure **is** cached, and replays until one of its inputs changes. That is only a
problem when the failure came from something that is not an input, such as a flaky
filesystem with an error message not in the list above.

## Dependency files

`ec` always needs the compiler's dependency file, because it lists what the compile read:

- If the build did not ask for one, `ec` adds `-MD -MF <outdir>/.ekacache<pid>.d` and
  deletes the file afterwards.
- If the build asked for one (`-MD`, `-MMD`, `-MF`, `-MT`, `-MQ`, `-MP`), `ec` makes sure
  it includes system headers. With GCC, `-MMD` becomes `-MD`; with clang, `ec` adds
  `-Xclang -sys-header-deps`. **The depfile the build sees lists system headers too.**
- Links add `-Wl,--dependency-file=…`.

`ec` writes temporary files next to the output, named `.ekacache<pid>.*`.

## Hits and replay

- The output is written to a temporary file and renamed into place.
- Its mode is `0666 & ~umask`, plus executable bits for links. The original file's mode is
  not stored.
- The depfile's target is rewritten to the current output, or to the `-MT`/`-MQ` value.
- On a miss, the compiler's stderr is collected and written out when it exits. It is not
  streamed while the compiler runs.

## Notes

- **The working directory is part of the key.** In a Nix sandbox, `/build` keeps it stable.
  Outside a sandbox, the same compile in two checkouts at different paths does not share
  entries.
- **Environment variables.** Only `CPATH`, `C_INCLUDE_PATH`, `CPLUS_INCLUDE_PATH`,
  `OBJC_INCLUDE_PATH`, `GCC_EXEC_PREFIX` and `COMPILER_PATH` are part of a compile's key.
  Links also include `LIBRARY_PATH` and nearly all `NIX_*` variables, because the
  bintools wrapper reads them. `SOURCE_DATE_EPOCH` is not in the key, which is why sources
  that use `__DATE__` and friends are never stored.
- **Separate-argument linker options.** `-Xlinker -Map -Xlinker out.map` is not recognised
  as a side output. The link is cached, and on a hit the map file is not written. Use the
  `-Wl,-Map,out.map` form, which makes the link uncacheable.
