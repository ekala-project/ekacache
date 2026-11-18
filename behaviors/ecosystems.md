# Other language ecosystems

Informative, not normative. This page records how jig behaves when builds of other ecosystems
run with it in place, which interactions the cc-mode rules cover, and what caching those
ecosystems would take. Nothing here changes the compatibility contract, except where a section
points at an adopted rule in another document.

Sources: `jig/src/launcher.cc` (`ClassifyLaunch`), `jig/src/cc_mode.cc` (`IsSourceFile`,
`ParseInvocation`), observed tool behaviour (GCC 14 probes, 2026-11-17).

## What jig does with an unknown tool today

- **Launcher mode** (`jig /path/to/<prog> …`) classifies only `cc c++ gcc g++ clang clang++`
  (bare, or as the last dash-separated word of a target-prefixed name), `rustc` and `go`
  (`launcher.cc:59-77`). Anything else (`javac`, `kotlinc`, `gfortran`, `nvcc`, `ghc`,
  `ocamlopt`, `zig`, `mpicc`, `hipcc`, `swiftc`, …) is `exec`ed unchanged: no cache, no slot.
- **cc mode** treats only `.c .cc .cpp .cxx .c++ .C .S .s .sx .m` as sources
  (`cc_mode.cc:56-62`). A compile with any other source (`.f90`, `.cu`, `.mm`, `.cppm`, `.ixx`,
  `.hs`, …) has zero sources and runs uncached. Fortran is also excluded explicitly.

So no other language is cached by accident. The interactions that matter are the C compiler
calls these toolchains make themselves.

## Ecosystems that drive the C compiler

| Ecosystem | How it calls `cc` | Result with jig |
|---|---|---|
| Python C extensions (setuptools, meson-python), node-gyp, cargo `cc` crate, Haskell/OCaml C stubs in the source tree, JNI libraries | ordinary `-c foo.c -o foo.o` with stable paths | cached normally |
| cgo | `-I $WORK/b001`, `$WORK/b001/_cgo_export.c`, `$WORK = $TMPDIR/go-build<digits>` | never hits. Uncacheable in the Rust port (Q-CC-16) |
| GHC | C stubs and assembly in `$TMPDIR/ghc<pid>_<n>/`; links through `gcc` with temp objects | never hits. Uncacheable in the Rust port (Q-CC-16) |
| ocamlopt | `gcc -c -o …/camlasm*.o …/camlasm*.s` | never hits. Uncacheable in the Rust port (Q-CC-16) |
| nvcc | host compiles of `tmpxft_*` intermediates | never hits. Uncacheable in the Rust port (Q-CC-16) |
| swiftc, zig cc, mpicc, hipcc | their own driver names are not classified as cc. What they exec underneath may be a jig-wrapped compiler | as for whatever they call |

## JVM (Java, Kotlin, Scala)

**Why wrapping `javac` would not help.** Gradle compiles in its worker or compiler daemon
through `javax.tools`, and Maven's compiler plugin runs in-process unless `fork=true`. Neither
execs a `javac` binary, so a launcher never sees the compile. sbt and Bazel's Java rules work
the same way (persistent workers).

**What caching `javac` directly would take, if it ever ran as a process:**
- **Outputs:** one call writes many class files: nested and anonymous classes (`Foo$1.class`),
  and classes compiled implicitly from `-sourcepath`. The `-d` directory has to be snapshotted;
  outputs cannot be derived from the arguments.
- **Inputs:** there is no depfile. Classpath entries (jars and directories) would have to be
  hashed as a whole, or traced (`-verbose` loading lines).
- **Annotation processors** (`-processorpath`, Lombok, Dagger, AutoValue) run arbitrary code,
  generate sources into `-s` and resources into `-d`, and may be nondeterministic.
- **Identity:** jar files carry zip timestamps. Content hashes are stable only for jars that
  were normalised (nixpkgs `stripJavaArchivesHook`).
- **Key:** the JDK identity, `--release`/`-source`/`-target`, and `-parameters`/`-g` all
  change the output.
- **Kotlin:** `kotlinc` adds a compile daemon and incremental caches that store absolute paths.

**The workable route: the build tool's remote cache.** As jig does for Go with GOCACHEPROG,
jig could serve the build tool's cache protocol on loopback inside the sandbox, with jigd as
storage. Keys and soundness are then the build tool's responsibility:

| Tool | Protocol | Notes |
|---|---|---|
| Gradle | HTTP build cache: `GET`/`PUT /<path>/<cache-key>` | off by default. Needs `org.gradle.caching=true` and `buildCache { remote(HttpBuildCache) { url = … } }`, which a setup hook could inject as an init script in `$GRADLE_USER_HOME/init.d` so that derivations still never mention the cache |
| Maven | `maven-build-cache-extension`, HTTP remote | needs the extension in `.mvn/extensions.xml`. Injecting it is more invasive |
| Bazel | Remote Execution API cache (gRPC) or HTTP cache | rarely used in nixpkgs builds |

Open points for such a mode: Nix's sandbox has a loopback interface, but the protocol server
must start before the build tool and stop after it (a wrapper like `jig gradle-cache -- gradle
…`, or a background process from the setup hook). Gradle's cache entries are tar archives of
outputs, and can be large.

## Other ecosystems

| Ecosystem | Notes for a future cache mode |
|---|---|
| Fortran | `gfortran` writes module files (`.mod`/`.smod`) into the `-J` directory as a second output, and importers read them. The ccache experience: treat `.mod` as an artifact, or refuse modules. Uncached today |
| CUDA/HIP | `nvcc` runs a multi-stage pipeline with temp intermediates. `clang -x cuda` has a `.cu` source, so it is uncached today. A dedicated mode would need the device-side toolchain identity in the key |
| Objective-C++ | `.mm` is not a source extension. Adding it is a pure hit-rate change (it behaves like `.cc`) |
| Haskell | GHC `--make` compiles modules in-process. Only C stubs and links reach `cc`. The equivalent of GOCACHEPROG does not exist for GHC |
| Node/TypeScript | `tsc`, `esbuild`, `swc` and bundlers compile whole projects in-process. Nothing per file to intercept |
| Zig | `zig build` has its own content-addressed cache (`ZIG_GLOBAL_CACHE_DIR`). Wiring it to jigd would need a remote-cache feature in Zig |
| Python bytecode | `compileall` is cheap. `.pyc` files depend on `SOURCE_DATE_EPOCH`. Not worth caching |

## C/C++ features found during this survey

These are bugs in the existing cc mode, not other-ecosystem issues, and are adopted for the Rust
port (see `jig-cc-mode.md` §Additional uncacheable cases):

- `.incbin` data is missing from the depfile (Q-CC-12, verified);
- profile, plugin and spec files are missing from the depfile (Q-CC-13, `-fprofile-use`
  verified);
- side outputs (`.gcno`, `.dwo`, …) are lost on a hit (Q-CC-14, verified);
- C++20 modules have BMI and `.ddi` side outputs, and BMI inputs (Q-CC-15);
- other toolchains' temp-file compiles pollute the cache (Q-CC-16).

Still to verify: whether C23 `#embed` files appear in GCC 15 / clang 19 depfiles (`jig-cc-mode.md`
open question 5).
