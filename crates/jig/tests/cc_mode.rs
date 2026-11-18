//! cc mode end to end (`implementation/06-jig-cc.md §Tests`, `behaviors/jig-cc-mode.md`,
//! `behaviors/jig-driver-policy.md` TV-37).
//!
//! - TV-37 and the driver-argv difftest: a fake root `R/bin/jig` (a copy of the binary under
//!   test) with `cc`, `c++` and `gfortran` symlinks and a fake compiler that prints its argv.
//!   The difftest runs the same corpus through the legacy jig (`$JIG_LEGACY_JIG`) and compares
//!   status, stdout, stderr and the `$JIG_LOG` rows.
//! - Real compiles (launcher mode, `$JIG_TEST_GCC` / `$JIG_TEST_CLANG` or `gcc`/`clang` on
//!   `PATH`) against a daemon (`$JIG_RUST_JIGD` or `$JIG_LEGACY_JIGD`): miss, hit, invalidation,
//!   cached failures, `-E` to stdout, links; cross-implementation hits both ways (the mixed-fleet
//!   guarantee); the adopted rules A–D and Q-CC-5 against legacy's behaviour.
//! - Q-CC-2: a recording fake jigd sees `DONE` after the last `PUT`.
//!
//! Every test skips (with a message) when what it needs is not given.
#![allow(clippy::unwrap_used, clippy::too_many_lines)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

mod common;
use common::{Daemon, JIG, any_daemon, legacy_jig};

fn write_exe(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// `R/bin/jig` (a copy, so `/proc/self/exe` is inside `R`), its mode symlinks, `R/fakecc`, and
/// `R/etc/jig.json` when given (`<R>` is replaced by the root).
struct Root {
    dir: tempfile::TempDir,
}

impl Root {
    fn new(jig: &Path, conf: Option<&str>) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("jr")
            .tempdir_in("/tmp")
            .unwrap();
        let r = dir.path();
        std::fs::create_dir_all(r.join("bin")).unwrap();
        std::fs::copy(jig, r.join("bin/jig")).unwrap();
        for name in ["cc", "c++", "gfortran", "gcc"] {
            std::os::unix::fs::symlink("jig", r.join("bin").join(name)).unwrap();
        }
        // argv on stderr; -print-search-dirs output without a final newline (TV-37)
        write_exe(
            &r.join("fakecc"),
            "#!/bin/sh\nfor a in \"$@\"; do printf '[%s]' \"$a\" >&2; done\nprintf '\\n' >&2\n\
             for a in \"$@\"; do if [ \"$a\" = -print-search-dirs ]; then \
             printf 'install: /i\\nprograms: =/p\\nlibraries: =/l'; fi; done\nexit 0\n",
        );
        let etc = r.join("etc");
        let root = Self { dir };
        if let Some(conf) = conf {
            std::fs::create_dir_all(&etc).unwrap();
            std::fs::write(etc.join("jig.json"), root.subst(conf)).unwrap();
        }
        root
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn subst(&self, s: &str) -> String {
        s.replace("<R>", self.path().to_str().unwrap())
    }

    /// Run `R/bin/<name> args` in `R` with a clean environment (no daemon) plus `env`.
    fn run(&self, name: &str, args: &[&str], env: &[(&str, &str)]) -> Run {
        let log = self.path().join("log");
        let _ = std::fs::remove_file(&log);
        let mut cmd = Command::new(self.path().join("bin").join(name));
        cmd.args(args)
            .current_dir(self.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("JIG_SOCK", "/nonexistent/sock")
            .env("JIG_LOG", &log);
        for (k, v) in env {
            cmd.env(k, self.subst(v));
        }
        let out = cmd.output().unwrap();
        let log = std::fs::read(&log).unwrap_or_default();
        Run::of(&out, &log, self.path())
    }
}

/// A run's observable result, with the root spelt `<R>` and `$JIG_LOG`'s times dropped.
#[derive(Debug, PartialEq, Eq)]
struct Run {
    status: Option<i32>,
    stdout: String,
    stderr: String,
    log: Vec<String>,
}

impl Run {
    fn of(out: &Output, log: &[u8], root: &Path) -> Self {
        let root = root.to_str().unwrap();
        let norm = |b: &[u8]| String::from_utf8_lossy(b).replace(root, "<R>");
        let log = norm(log)
            .lines()
            .map(|l| l.rsplit_once('\t').map_or(l, |(head, _)| head).to_string())
            .collect();
        Self {
            status: out.status.code(),
            stdout: norm(&out.stdout),
            stderr: norm(&out.stderr),
            log,
        }
    }
}

const ELF_CONF: &str = r#"{"cc": "<R>/fakecc", "flags": ["--target=x", "-O2"], "cxxflags": ["-stdlib=libc++"], "libc": "/sr/libc", "crt": "/cc/lib/crt_interp.o", "runtimes": "/sr/rt/lib", "prefix-map": ["-ffile-prefix-map=@/=/jig"]}"#;
const COFF_CONF: &str =
    r#"{"cc": "<R>/fakecc", "binfmt": "coff", "flags": ["--target=x86_64-pc-windows-msvc"]}"#;
const MACHO_CONF: &str = r#"{"cc": "<R>/fakecc", "binfmt": "macho", "flags": ["--target=arm64-apple-macos14.0"], "libc": "/sr"}"#;
const PKGS_CC: &str = r#"{"<R>":{"ldflags":["-L/dep/lib","-L","/x","-Wl,-rpath,/dep/lib"],"cflags":["-DP","-O2","-D_FORTIFY_SOURCE=3"],"cxxflags":["-DPP"]}}"#;

struct Case {
    conf: Option<&'static str>,
    name: &'static str,
    args: &'static [&'static str],
    env: &'static [(&'static str, &'static str)],
}

const fn case(
    conf: Option<&'static str>,
    name: &'static str,
    args: &'static [&'static str],
) -> Case {
    Case {
        conf,
        name,
        args,
        env: &[],
    }
}

/// The driver corpus: seeded from the policy's test vectors plus the classification edges.
fn corpus() -> Vec<Case> {
    let elf = Some(ELF_CONF);
    let mut cases = vec![
        case(elf, "cc", &["-c", "a.c"]),
        case(
            elf,
            "cc",
            &["-c", "a.c", "-o", "out/a.o", "-MD", "-MF", "out/a.d"],
        ),
        case(elf, "cc", &["-o", "x", "x.c"]),
        case(elf, "cc", &["x.o", "-o", "x"]),
        case(elf, "cc", &["-shared", "-o", "l.so", "l.c"]),
        case(elf, "cc", &["-shared", "-o", "l.so", "l.o", "-L.", "-lz"]),
        case(elf, "cc", &["-o", "x", "x.c", "-lfoo", "-l", "bar"]),
        case(
            elf,
            "cc",
            &["x.o", "-Wl,-rpath,", "-Wl,-rpath,/usr/lib,-z,now"],
        ),
        case(
            elf,
            "cc",
            &[
                "-o",
                "x",
                "x.c",
                "-Wl,-rpath,/build/build:",
                "-Wl,-rpath",
                "-Wl,/a",
                "-Xlinker",
                "-rpath",
                "-Xlinker",
                "/b",
            ],
        ),
        case(elf, "cc", &["-o", "x"]),
        case(elf, "cc", &["--version"]),
        case(elf, "cc", &["-v"]),
        case(elf, "cc", &["-static", "-o", "x", "x.c"]),
        case(elf, "cc", &["-static-pie", "-o", "x", "x.c"]),
        case(elf, "cc", &["-r", "-o", "x.o", "a.o", "b.o"]),
        case(elf, "cc", &["-nostartfiles", "-o", "x", "x.c"]),
        case(elf, "cc", &["-nostdlib", "x.o", "-o", "x"]),
        case(elf, "cc", &["-O2", "-D_FORTIFY_SOURCE=2", "-c", "a.c"]),
        case(elf, "cc", &["-c", "a.c", "-U_FORTIFY_SOURCE"]),
        case(elf, "cc", &["-O0", "-c", "a.c"]),
        case(
            elf,
            "cc",
            &["-o", "x", "x.c", "/nix/store/h-zlib/lib/libz.so.1"],
        ),
        case(elf, "cc", &["-E", "a.c"]),
        case(elf, "cc", &["-M", "a.c"]),
        case(elf, "cc", &["-cc1", "-emit-obj", "a.c"]),
        case(elf, "cc", &["-print-search-dirs"]),
        case(elf, "cc", &["-print-file-name=libc.so"]),
        case(elf, "c++", &["-c", "a.cc"]),
        case(elf, "c++", &["-o", "x", "x.cc"]),
        case(elf, "c++", &["-nostdlib++", "-o", "x", "x.cc"]),
        case(elf, "c++", &["-shared", "-o", "l.so", "l.cc", "-nostdlib"]),
        case(elf, "gfortran", &["-c", "a.f"]),
        case(elf, "cc", &["@missing.rsp", "-c", "a.c"]),
        case(Some(COFF_CONF), "cc", &["-fPIC", "-c", "a.c"]),
        case(
            Some(COFF_CONF),
            "c++",
            &["-std=c++11", "-o", "x.exe", "x.cc", "-lFoo", "-lBar/x"],
        ),
        case(Some(MACHO_CONF), "cc", &["a.c", "-o", "a"]),
        case(Some(MACHO_CONF), "cc", &["-shared", "-o", "l.dylib", "l.o"]),
        case(Some(r#"{"flags":[]}"#), "cc", &["-c", "a.c"]),
        case(Some(r#"{"cc": 1}"#), "cc", &["-c", "a.c"]),
        case(
            Some(r#"{"cc": "<R>/fakecc", "binfmt": "wasm"}"#),
            "cc",
            &["-c", "a.c"],
        ),
        case(Some("not json"), "cc", &["-c", "a.c"]),
        case(None, "cc", &["-c", "a.c"]),
        case(None, "c++", &["-c", "a.cc", "-O0"]),
    ];
    for name in ["cc", "c++"] {
        for args in [
            &["-print-search-dirs"][..],
            &["-c", "a.c"],
            &["-o", "x", "x.c"],
            &["-shared", "-o", "l.so", "l.o"],
            &["-O0", "-c", "a.c"],
        ] {
            cases.push(Case {
                conf: Some(ELF_CONF),
                name,
                args,
                env: &[("PKGS_CC", PKGS_CC), ("PKGS_PREFIX_MAP", "/src=/p")],
            });
        }
    }
    cases.push(Case {
        conf: None,
        name: "c++",
        args: &["-c", "a.cc", "-O0"],
        env: &[("JIG_CC", "<R>/fakecc")],
    });
    cases.push(Case {
        conf: Some(r#"{"flags":[]}"#),
        name: "cc",
        args: &["-c", "a.c"],
        env: &[("JIG_CC", "/bin/true")],
    });
    cases.push(Case {
        conf: Some(ELF_CONF),
        name: "cc",
        args: &["-c", "a.c"],
        env: &[("PKGS_CC", "{")],
    });
    cases
}

fn run_corpus(jig: &Path) -> Vec<Run> {
    corpus()
        .iter()
        .map(|c| Root::new(jig, c.conf).run(c.name, c.args, c.env))
        .collect()
}

/// TV-37 on the Rust binary.
#[test]
fn tv37_end_to_end() {
    let jig = Path::new(JIG);
    let root = Root::new(jig, Some(r#"{"cc":"<R>/fakecc","flags":["-fx"]}"#));
    let r = root.run("cc", &["-print-search-dirs"], &[]);
    assert_eq!(r.status, Some(0));
    assert_eq!(r.stdout, "install: /i\nprograms: =/p\nlibraries: =/l\n");
    let meta = r#"-Wl,--package-metadata={"type":"repkgs"}"#;
    let pkgs = r#"{"<R>":{"ldflags":["-L/dep/lib","-L","/x"],"cflags":["-DP"]}}"#;
    let r = root.run("cc", &["-print-search-dirs"], &[("PKGS_CC", pkgs)]);
    assert_eq!(
        r.stderr,
        format!(
            "[--start-no-unused-arguments][-fx][-DP][-Wl,--build-id=sha1][{meta}][--end-no-unused-arguments][-print-search-dirs]\n"
        )
    );
    assert!(r.stdout.ends_with("\nlibraries: =/l:/dep/lib\n"), "{r:?}");
    assert_eq!(r.log, ["cc\tquery\t"]);

    let r = root.run("gfortran", &["-c", "a.f"], &[]);
    assert_eq!(r.status, Some(1));
    assert_eq!(
        r.stderr,
        "jig: gfortran called but etc/jig.json names no fc\n"
    );

    let root = Root::new(jig, Some(r#"{"flags":[]}"#));
    let r = root.run("cc", &["-c", "a.c"], &[("JIG_CC", "/bin/true")]);
    assert_eq!(r.status, Some(1));
    assert_eq!(
        r.stderr,
        "jig: no etc/jig.json next to the binary and JIG_CC unset\n"
    );

    let root = Root::new(jig, None);
    let r = root.run("c++", &["-c", "a.cc", "-O0"], &[("JIG_CC", "<R>/fakecc")]);
    assert_eq!(r.status, Some(0));
    assert_eq!(r.stderr, "[-c][a.cc][-O0]\n");
    // a.cc does not exist: no primary
    assert_eq!(r.log, ["cc\tuncacheable-nosrc\ta.cc"]);
}

/// The driver-argv difftest: legacy and Rust print the same argv, output and log rows.
#[test]
fn driver_argv_difftest() {
    let Some(legacy) = legacy_jig() else {
        eprintln!("skipped: JIG_LEGACY_JIG not set");
        return;
    };
    let cases = corpus();
    let want = run_corpus(&legacy);
    let got = run_corpus(Path::new(JIG));
    for ((c, w), g) in cases.iter().zip(&want).zip(&got) {
        assert_eq!(g, w, "{} {:?} {:?}", c.name, c.args, c.env);
    }
}

// ---------------------------------------------------------------------------------------------
// real compilers

fn find_compiler(var: &str, name: &str) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(var) {
        return Some(PathBuf::from(p));
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn gcc() -> Option<PathBuf> {
    find_compiler("JIG_TEST_GCC", "gcc")
}

fn clang() -> Option<PathBuf> {
    find_compiler("JIG_TEST_CLANG", "clang")
}

/// A build directory and a way to run `jig <compiler> args` in it.
struct Work {
    dir: tempfile::TempDir,
    sock: PathBuf,
}

impl Work {
    fn new(d: &Daemon) -> Self {
        Self::with_sock(d.sock.clone())
    }

    fn with_sock(sock: PathBuf) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("jw")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::create_dir(dir.path().join("tmp")).unwrap();
        Self { dir, sock }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn write(&self, name: &str, text: &str) {
        let p = self.path().join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn read(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.path().join(name)).unwrap()
    }

    /// `jig <compiler> args`; the log rows without times.
    fn jig(&self, jig: &Path, compiler: &Path, args: &[&str]) -> (Output, Vec<String>) {
        let log = self.path().join("tmp/log");
        let _ = std::fs::remove_file(&log);
        let out = Command::new(jig)
            .arg(compiler)
            .args(args)
            .current_dir(self.path())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("TMPDIR", self.path().join("tmp"))
            .env("JIG_SOCK", &self.sock)
            .env("JIG_LOG", &log)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let rows = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .map(|l| l.rsplit_once('\t').map_or(l, |(h, _)| h).to_string())
            .collect();
        (out, rows)
    }
}

fn rust() -> &'static Path {
    Path::new(JIG)
}

fn row(outcome: &str, subject: &str) -> Vec<String> {
    vec![format!("cc\t{outcome}\t{subject}")]
}

#[test]
fn real_compiles_hit() {
    let Some(d) = any_daemon() else {
        eprintln!("skipped: no jigd (JIG_RUST_JIGD / JIG_LEGACY_JIGD)");
        return;
    };
    for compiler in [gcc(), clang()].into_iter().flatten() {
        let w = Work::new(&d);
        w.write("inc/a.h", "#define N 1\n");
        w.write("a.c", "#include \"a.h\"\nint f(void) { return N; }\n");
        let args = [
            "-Iinc", "-O2", "-c", "a.c", "-o", "out/a.o", "-MD", "-MF", "out/a.d",
        ];
        std::fs::create_dir(w.path().join("out")).unwrap();
        let (out, log) = w.jig(rust(), &compiler, &args);
        assert!(out.status.success(), "{out:?}");
        assert_eq!(
            log,
            row("compiled", "a.c new-key"),
            "{}",
            compiler.display()
        );
        let (object, depfile) = (w.read("out/a.o"), w.read("out/a.d"));
        std::fs::remove_file(w.path().join("out/a.o")).unwrap();
        std::fs::remove_file(w.path().join("out/a.d")).unwrap();
        let (out, log) = w.jig(rust(), &compiler, &args);
        assert!(out.status.success() && out.stderr.is_empty(), "{out:?}");
        assert_eq!(log, row("cached", "a.c"));
        assert_eq!(w.read("out/a.o"), object);
        assert_eq!(w.read("out/a.d"), depfile);
        // -MT retargets the replayed depfile
        let (_, log) = w.jig(rust(), &compiler, &[&args[..], &["-MT", "T"]].concat());
        assert_eq!(log, row("cached", "a.c"));
        assert!(w.read("out/a.d").starts_with(b"T:"));
        // without -MD the entry still serves
        let (_, log) = w.jig(rust(), &compiler, &args[..6]);
        assert_eq!(log, row("cached", "a.c"));
        // a header change invalidates
        w.write("inc/a.h", "#define N 2\n");
        let (_, log) = w.jig(rust(), &compiler, &args);
        assert_eq!(log, row("compiled", "a.c inputs-changed:inc/a.h"));

        // a deterministic failure is cached, with its stderr and status
        w.write("bad.c", "int x = ;\n");
        let (first, log) = w.jig(rust(), &compiler, &["-c", "bad.c"]);
        assert_eq!(log, row("compiled-error", "bad.c new-key"));
        let (second, log) = w.jig(rust(), &compiler, &["-c", "bad.c"]);
        assert_eq!(log, row("cached-error", "bad.c"));
        assert_eq!(second.status.code(), first.status.code());
        assert_eq!(second.stderr, first.stderr);
        // a missing header is never stored
        w.write("miss.c", "#include \"nope.h\"\n");
        let (_, log) = w.jig(rust(), &compiler, &["-c", "miss.c"]);
        assert_eq!(log, row("failed", "miss.c new-key"));
        let (_, log) = w.jig(rust(), &compiler, &["-c", "miss.c"]);
        assert_eq!(log, row("failed", "miss.c new-key"));

        // -E to stdout
        let (first, log) = w.jig(rust(), &compiler, &["-Iinc", "-E", "a.c"]);
        assert_eq!(log, row("compiled", "a.c new-key"));
        let (second, log) = w.jig(rust(), &compiler, &["-Iinc", "-E", "a.c"]);
        assert_eq!(log, row("cached", "a.c"));
        assert_eq!(second.stdout, first.stdout);
        assert!(!first.stdout.is_empty());
        assert_eq!(std::fs::read_dir(w.path().join("tmp")).unwrap().count(), 1);

        // a link (GNU ld and lld both write --dependency-file)
        w.write("m.c", "int f(void);\nint main(void) { return f() - 2; }\n");
        let (_, log) = w.jig(rust(), &compiler, &["-c", "m.c"]);
        assert_eq!(log, row("compiled", "m.c new-key"));
        let link = ["m.o", "out/a.o", "-o", "prog"];
        let (out, log) = w.jig(rust(), &compiler, &link);
        assert!(out.status.success(), "{out:?}");
        // linked when the linker cannot write --dependency-file (GNU ld before 2.40)
        let stored = log == row("compiled", "prog new-key");
        let binary = w.read("prog");
        std::fs::remove_file(w.path().join("prog")).unwrap();
        let (_, log) = w.jig(rust(), &compiler, &link);
        if stored {
            assert_eq!(log, row("cached", "prog"));
            assert_eq!(w.read("prog"), binary);
            let mode = std::fs::metadata(w.path().join("prog"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111);
        }
        assert!(
            Command::new(w.path().join("prog"))
                .status()
                .unwrap()
                .success()
        );
        // configure probe: one source straight to an executable
        let (_, log) = w.jig(rust(), &compiler, &["-o", "probe", "m.c", "out/a.o"]);
        assert_eq!(log, row("linked", "m.c"));
        let (_, log) = w.jig(rust(), &compiler, &["--version"]);
        assert_eq!(log, row("query", ""));
    }
}

/// The mixed-fleet guarantee: an entry stored by one implementation is a hit for the other.
#[test]
fn cross_implementation_hits() {
    let Some(legacy) = legacy_jig() else {
        eprintln!("skipped: JIG_LEGACY_JIG not set");
        return;
    };
    for compiler in [gcc(), clang()].into_iter().flatten() {
        for (writer, reader) in [(legacy.as_path(), rust()), (rust(), legacy.as_path())] {
            let Some(d) = any_daemon() else {
                eprintln!("skipped: no jigd");
                return;
            };
            let w = Work::new(&d);
            w.write("inc/a.h", "#define N 1\n");
            w.write("a.c", "#include \"a.h\"\nint f(void) { return N; }\n");
            w.write("bad.c", "#include \"a.h\"\nint x = N +;\n");
            w.write("m.c", "int f(void);\nint main(void) { return f() - 1; }\n");
            let jobs: [&[&str]; 5] = [
                &["-Iinc", "-c", "a.c", "-o", "a.o", "-MMD"],
                &["-Iinc", "-c", "bad.c"],
                &["-Iinc", "-E", "a.c"],
                &["-c", "m.c"],
                &["m.o", "a.o", "-o", "prog"],
            ];
            let mut stored = Vec::new();
            for job in jobs {
                let (out, log) = w.jig(writer, &compiler, job);
                stored.push((out, log));
            }
            for (job, (first, first_log)) in jobs.iter().zip(&stored) {
                let (out, log) = w.jig(reader, &compiler, job);
                let first_outcome = first_log[0].split('\t').nth(1).unwrap();
                let want = match first_outcome {
                    "compiled" => "cached",
                    "compiled-error" => "cached-error",
                    other => other,
                };
                let got = log[0].split('\t').nth(1).unwrap();
                assert_eq!(
                    got,
                    want,
                    "{} {job:?} writer {} reader {}: {first_log:?} then {log:?}",
                    compiler.display(),
                    writer.display(),
                    reader.display()
                );
                assert_eq!(out.status.code(), first.status.code());
                assert_eq!(out.stdout, first.stdout);
                assert_eq!(out.stderr, first.stderr);
            }
            assert!(!w.read("a.d").is_empty());
        }
    }
}

/// The adopted rules A–D and Q-CC-5, against legacy where it behaves differently.
#[test]
fn adopted_rules() {
    let Some(compiler) = gcc().or_else(clang) else {
        eprintln!("skipped: no gcc/clang");
        return;
    };
    let Some(d) = any_daemon() else {
        eprintln!("skipped: no jigd");
        return;
    };
    let w = Work::new(&d);
    w.write("a.c", "int f(void) { return 1; }\n");
    // rule A: a side output
    let (out, log) = w.jig(rust(), &compiler, &["-c", "a.c", "-gsplit-dwarf"]);
    assert!(out.status.success());
    assert_eq!(log, row("uncacheable", "a.c"));
    // rule B: a hidden input
    let (_, log) = w.jig(rust(), &compiler, &["-c", "a.c", "-specs=/dev/null"]);
    assert_eq!(log, row("uncacheable", "a.c"));
    // rule D: another toolchain's temp dir
    w.write("go-build123/b001/x.c", "int g(void) { return 2; }\n");
    let (_, log) = w.jig(rust(), &compiler, &["-c", "go-build123/b001/x.c"]);
    assert_eq!(log, row("uncacheable", "go-build123/b001/x.c"));
    // rule C before lookup: the source itself
    w.write("data.bin", "DATA");
    w.write("inc.S", ".section .rodata\n.incbin \"data.bin\"\n");
    let (out, log) = w.jig(rust(), &compiler, &["-c", "inc.S"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(log, row("uncacheable", "inc.S"));
    // rule C before storing: an included header
    w.write("blob.h", ".incbin \"data.bin\"\n");
    w.write("via.S", "#include \"blob.h\"\n");
    let (out, log) = w.jig(rust(), &compiler, &["-c", "via.S"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(log, row("compiled-unstored", "via.S incbin"));

    if let Some(legacy) = legacy_jig() {
        // the C++ jig caches all of these
        let (_, log) = w.jig(&legacy, &compiler, &["-c", "a.c", "-gsplit-dwarf"]);
        assert_eq!(log, row("compiled", "a.c new-key"));
        let (_, log) = w.jig(rust(), &compiler, &["-c", "a.c", "-gsplit-dwarf"]);
        assert_eq!(log, row("uncacheable", "a.c"));
        // rule C on a hit: an entry the C++ jig wrote is refused, and the compile not stored
        let (_, log) = w.jig(&legacy, &compiler, &["-c", "via.S"]);
        assert_eq!(log, row("compiled", "via.S new-key"));
        let (_, log) = w.jig(&legacy, &compiler, &["-c", "via.S"]);
        assert_eq!(log, row("cached", "via.S"));
        for _ in 0..2 {
            let (out, log) = w.jig(rust(), &compiler, &["-c", "via.S"]);
            assert!(out.status.success());
            assert_eq!(log, row("compiled-unstored", "via.S incbin"));
        }
    }

    // Q-CC-5: a resource failure is not stored. A fake gcc writes its depfile and fails
    let bin = w.path().join("fake");
    std::fs::create_dir(&bin).unwrap();
    write_exe(
        &bin.join("gcc"),
        "#!/bin/sh\nwhile [ $# -gt 0 ]; do if [ \"$1\" = -MF ]; then printf 'a.o: a.c\\n' > \"$2\"; fi; shift; done\n\
         echo \"a.c: fatal error: cannot write a.o: $JIG_FAKE_ERR\" >&2\nexit 1\n",
    );
    let fake = bin.join("gcc");
    let run_fake = |jig: &Path, err: &str| {
        let log = w.path().join("tmp/log");
        let _ = std::fs::remove_file(&log);
        let out = Command::new(jig)
            .arg(&fake)
            .args(["-c", "a.c"])
            .current_dir(w.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("JIG_SOCK", &w.sock)
            .env("JIG_LOG", &log)
            .env("JIG_FAKE_ERR", err)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1));
        std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|l| l.rsplit_once('\t').unwrap().0.to_string())
            .collect::<Vec<_>>()
    };
    for err in [
        "No space left on device",
        "Disk quota exceeded",
        "Cannot allocate memory",
        "virtual memory exhausted",
        "out of memory",
        "Too many open files",
        "Input/output error",
        "Read-only file system",
        "Resource temporarily unavailable",
    ] {
        assert_eq!(run_fake(rust(), err), row("failed", "a.c new-key"), "{err}");
    }
    assert_eq!(
        run_fake(rust(), "Permission denied"),
        row("compiled-error", "a.c new-key")
    );
    if let Some(legacy) = legacy_jig() {
        w.write("a.c", "int f(void) { return 3; }\n");
        assert_eq!(
            run_fake(&legacy, "No space left on device"),
            row("compiled-error", "a.c new-key")
        );
    }
}

/// `-E` to stdout compiles to `<temp_directory>/jig<pid>.i`: `$TMPDIR` or `/tmp`, and a
/// relative `jig<pid>.i` when `TMPDIR` is set but empty (path join semantics), as legacy does.
#[test]
fn preprocess_temp_path() {
    let Some(d) = any_daemon() else {
        eprintln!("skipped: no jigd");
        return;
    };
    let w = Work::new(&d);
    w.write("a.c", "int x;\n");
    std::fs::create_dir(w.path().join("bin")).unwrap();
    let fake = w.path().join("bin/cc");
    write_exe(
        &fake,
        "#!/bin/sh\nfor a in \"$@\"; do printf '[%s]' \"$a\" >&2; done\nprintf '\\n' >&2\n",
    );
    let output_arg = |jig: &Path, tmpdir: Option<&str>| {
        let mut cmd = Command::new(jig);
        cmd.arg(&fake)
            .args(["-E", "a.c"])
            .current_dir(w.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("JIG_SOCK", &w.sock);
        if let Some(t) = tmpdir {
            cmd.env("TMPDIR", t);
        }
        let out = cmd.output().unwrap();
        let err = String::from_utf8(out.stderr).unwrap();
        let after = err.split("[-o][").nth(1).unwrap_or_else(|| panic!("{err}"));
        let path = after.split(']').next().unwrap().to_string();
        path.replace(|c: char| c.is_ascii_digit(), "")
    };
    let tmp = w.path().join("tmp");
    let tmp = tmp.to_str().unwrap();
    assert_eq!(output_arg(rust(), None), "/tmp/jig.i");
    assert_eq!(output_arg(rust(), Some("")), "jig.i");
    assert_eq!(
        output_arg(rust(), Some(tmp)),
        format!("{tmp}/jig.i").replace(|c: char| c.is_ascii_digit(), "")
    );
    if let Some(legacy) = legacy_jig() {
        for t in [None, Some(""), Some(tmp)] {
            assert_eq!(
                output_arg(rust(), t),
                output_arg(&legacy, t),
                "TMPDIR={t:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Q-CC-2: a recording daemon

/// A fake jigd that answers every request plainly (GET: MISS) and records the command words.
fn recording_daemon() -> (tempfile::TempDir, PathBuf, Arc<Mutex<Vec<String>>>) {
    let dir = tempfile::Builder::new()
        .prefix("jf")
        .tempdir_in("/tmp")
        .unwrap();
    let sock = dir.path().join("s");
    let listener = UnixListener::bind(&sock).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let record = Arc::clone(&record);
            std::thread::spawn(move || {
                let mut out = stream.try_clone().unwrap();
                let mut r = BufReader::new(stream);
                let mut line = String::new();
                while r.read_line(&mut line).is_ok_and(|n| n > 0) {
                    let words: Vec<String> = line.split_whitespace().map(str::to_string).collect();
                    line.clear();
                    let Some(cmd) = words.first() else { continue };
                    record.lock().unwrap().push(cmd.clone());
                    let reply = match cmd.as_str() {
                        "HELLO" => "OK 1\n".to_string(),
                        "GET" | "HAS" => "MISS\n".to_string(),
                        "PUT" => {
                            let n: usize = words[2].parse().unwrap();
                            let mut v = vec![0; n];
                            r.read_exact(&mut v).unwrap();
                            "OK\n".to_string()
                        }
                        "IDS" => {
                            let n: usize = words[1].parse().unwrap();
                            for _ in 0..n {
                                r.read_line(&mut line).unwrap();
                            }
                            line.clear();
                            "\n".repeat(n)
                        }
                        _ => "OK\n".to_string(),
                    };
                    if out.write_all(reply.as_bytes()).is_err() {
                        return;
                    }
                }
            });
        }
    });
    (dir, sock, seen)
}

#[test]
fn done_after_the_last_put() {
    let Some(compiler) = gcc().or_else(clang) else {
        eprintln!("skipped: no gcc/clang");
        return;
    };
    let (_dir, sock, seen) = recording_daemon();
    let w = Work::with_sock(sock);
    w.write("a.c", "int f(void) { return 1; }\n");
    w.write("bad.c", "int x = ;\n");
    for (args, outcome) in [
        (&["-c", "a.c", "-MD"][..], "compiled"),
        (&["-c", "bad.c"], "compiled-error"),
    ] {
        seen.lock().unwrap().clear();
        let (_, log) = w.jig(rust(), &compiler, args);
        assert_eq!(log[0].split('\t').nth(1).unwrap(), outcome, "{log:?}");
        let seen = seen.lock().unwrap().clone();
        let slot = seen.iter().position(|c| c == "SLOT").unwrap();
        let done = seen.iter().position(|c| c == "DONE").unwrap();
        let puts: Vec<usize> = seen
            .iter()
            .enumerate()
            .filter(|(_, c)| *c == "PUT")
            .map(|(i, _)| i)
            .collect();
        let want_puts = if outcome == "compiled" { 4 } else { 3 };
        assert_eq!(puts.len(), want_puts, "{seen:?}");
        assert!(puts.iter().all(|&p| slot < p && p < done), "{seen:?}");
        assert_eq!(done, seen.len() - 1, "{seen:?}");
    }
}
