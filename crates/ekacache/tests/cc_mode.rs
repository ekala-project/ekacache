//! cc mode end to end (`implementation/06-ec-cc.md §Tests`, `behaviors/ec-cc-mode.md`,
//! `behaviors/ec-driver-policy.md` TV-37).
//!
//! - TV-37: a fake root `R/bin/ec` (a copy of the binary under test) with `cc`, `c++` and
//!   `gfortran` symlinks and a fake compiler that prints its argv.
//! - Real compiles (launcher mode, `$EKACACHE_TEST_GCC` / `$EKACACHE_TEST_CLANG` or `gcc`/`clang` on
//!   `PATH`) against a daemon (`$EC_RUST_ECD`): miss, hit, invalidation, cached failures, `-E` to
//!   stdout, links; the adopted rules A–D and Q-CC-5.
//! - Q-CC-2: a recording fake ecd sees `DONE` after the last `PUT`.
//! - A storing fake ecd: a PUT batch that dies after `m/` (`object-gone`), ADOPTED Q-KEY-3
//!   (`not-cached:unreadable:`), content identity mode.
//!
//! Every test skips (with a message) when what it needs is not given.
#![allow(clippy::unwrap_used, clippy::too_many_lines)]

use ekacache_testkit::busy::CommandBusyExt as _;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

mod common;
use common::{Daemon, EC, daemon};

fn write_exe(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// `R/bin/ec` (a copy, so `/proc/self/exe` is inside `R`), its mode symlinks, `R/fakecc`, and
/// `R/etc/ekacache.json` when given (`<R>` is replaced by the root).
struct Root {
    dir: tempfile::TempDir,
}

impl Root {
    fn new(ec: &Path, conf: Option<&str>) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("jr")
            .tempdir_in("/tmp")
            .unwrap();
        let r = dir.path();
        std::fs::create_dir_all(r.join("bin")).unwrap();
        std::fs::copy(ec, r.join("bin/ec")).unwrap();
        for name in ["cc", "c++", "gfortran", "gcc"] {
            std::os::unix::fs::symlink("ec", r.join("bin").join(name)).unwrap();
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
            std::fs::write(etc.join("ekacache.json"), root.subst(conf)).unwrap();
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
            .env("EKACACHE_SOCK", "/nonexistent/sock")
            .env("EKACACHE_LOG", &log);
        for (k, v) in env {
            cmd.env(k, self.subst(v));
        }
        let out = cmd.output_busy().unwrap();
        let log = std::fs::read(&log).unwrap_or_default();
        Run::of(&out, &log, self.path())
    }
}

/// A run's observable result, with the root spelt `<R>` and `$EKACACHE_LOG`'s times dropped.
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

/// TV-37 on the Rust binary: the `end-to-end` rows of `tests/vectors/ec-driver-policy.toml`.
#[test]
fn tv37_end_to_end() {
    let ec = Path::new(EC);
    let file = ekacache_testkit::vectors::VectorFile::load("ec-driver-policy");
    for v in file.of_kind("end-to-end", 5) {
        let at = v.label();
        let root = Root::new(ec, v.opt_str("conf"));
        let env: Vec<(&str, &str)> = v
            .strs("env")
            .into_iter()
            .map(|kv| kv.split_once('=').unwrap())
            .collect();
        let r = root.run(v.str("tool"), &v.strs("args"), &env);
        // Q-DRV-2: `etc/ekacache.json` is found only through /proc/self/exe, so on a host without
        // procfs a conf is never read and, without `EKACACHE_CC`, cc mode stops before the compiler
        if !cfg!(target_os = "linux") {
            if v.opt_str("conf").is_some() && !env.iter().any(|(k, _)| *k == "EKACACHE_CC") {
                assert_eq!(r.status, Some(1), "{at}: {r:?}");
                assert_eq!(
                    r.stderr, "ec: no etc/ekacache.json next to the binary and EKACACHE_CC unset\n",
                    "{at}"
                );
            }
            continue;
        }
        let mut checked = 0;
        if v.get("status").is_some() {
            assert_eq!(
                r.status,
                Some(i32::try_from(v.int("status")).unwrap()),
                "{at}: {r:?}"
            );
            checked += 1;
        }
        for (key, got) in [("stdout", &r.stdout), ("stderr", &r.stderr)] {
            if let Some(want) = v.opt_str(key) {
                assert_eq!(got, want, "{at}: {key}");
                checked += 1;
            }
        }
        if let Some(tail) = v.opt_str("stdout_ends_with") {
            assert!(r.stdout.ends_with(tail), "{at}: {r:?}");
            checked += 1;
        }
        if let Some(log) = v.opt_strs("log") {
            assert_eq!(r.log, log, "{at}");
            checked += 1;
        }
        assert!(checked > 0, "{at}: no expectation");
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
    find_compiler("EKACACHE_TEST_GCC", "gcc")
}

fn clang() -> Option<PathBuf> {
    find_compiler("EKACACHE_TEST_CLANG", "clang")
}

/// A build directory and a way to run `ec <compiler> args` in it.
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

    /// `ec <compiler> args`; the log rows without times.
    fn ec(&self, ec: &Path, compiler: &Path, args: &[&str]) -> (Output, Vec<String>) {
        self.ec_env(ec, compiler, args, &[])
    }

    /// `ec` with `env` added.
    fn ec_env(
        &self,
        ec: &Path,
        compiler: &Path,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> (Output, Vec<String>) {
        let log = self.path().join("tmp/log");
        let _ = std::fs::remove_file(&log);
        let out = Command::new(ec)
            .arg(compiler)
            .args(args)
            .current_dir(self.path())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("TMPDIR", self.path().join("tmp"))
            .env("EKACACHE_SOCK", &self.sock)
            .env("EKACACHE_LOG", &log)
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .output_busy()
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
    Path::new(EC)
}

fn row(outcome: &str, subject: &str) -> Vec<String> {
    vec![format!("cc\t{outcome}\t{subject}")]
}

#[test]
fn real_compiles_hit() {
    let Some(d) = daemon("EC_RUST_ECD") else {
        eprintln!("skipped: EC_RUST_ECD not set");
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
        let (out, log) = w.ec(rust(), &compiler, &args);
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
        let (out, log) = w.ec(rust(), &compiler, &args);
        assert!(out.status.success() && out.stderr.is_empty(), "{out:?}");
        assert_eq!(log, row("cached", "a.c"));
        assert_eq!(w.read("out/a.o"), object);
        assert_eq!(w.read("out/a.d"), depfile);
        // -MT retargets the replayed depfile
        let (_, log) = w.ec(rust(), &compiler, &[&args[..], &["-MT", "T"]].concat());
        assert_eq!(log, row("cached", "a.c"));
        assert!(w.read("out/a.d").starts_with(b"T:"));
        // without -MD the entry still serves
        let (_, log) = w.ec(rust(), &compiler, &args[..6]);
        assert_eq!(log, row("cached", "a.c"));
        // a header change invalidates
        w.write("inc/a.h", "#define N 2\n");
        let (_, log) = w.ec(rust(), &compiler, &args);
        assert_eq!(log, row("compiled", "a.c inputs-changed:inc/a.h"));

        // a deterministic failure is cached, with its stderr and status
        w.write("bad.c", "int x = ;\n");
        let (first, log) = w.ec(rust(), &compiler, &["-c", "bad.c"]);
        assert_eq!(log, row("compiled-error", "bad.c new-key"));
        let (second, log) = w.ec(rust(), &compiler, &["-c", "bad.c"]);
        assert_eq!(log, row("cached-error", "bad.c"));
        assert_eq!(second.status.code(), first.status.code());
        assert_eq!(second.stderr, first.stderr);
        // a missing header is never stored
        w.write("miss.c", "#include \"nope.h\"\n");
        let (_, log) = w.ec(rust(), &compiler, &["-c", "miss.c"]);
        assert_eq!(log, row("failed", "miss.c new-key"));
        let (_, log) = w.ec(rust(), &compiler, &["-c", "miss.c"]);
        assert_eq!(log, row("failed", "miss.c new-key"));

        // -E to stdout
        let (first, log) = w.ec(rust(), &compiler, &["-Iinc", "-E", "a.c"]);
        assert_eq!(log, row("compiled", "a.c new-key"));
        let (second, log) = w.ec(rust(), &compiler, &["-Iinc", "-E", "a.c"]);
        assert_eq!(log, row("cached", "a.c"));
        assert_eq!(second.stdout, first.stdout);
        assert!(!first.stdout.is_empty());
        assert_eq!(std::fs::read_dir(w.path().join("tmp")).unwrap().count(), 1);

        // a link (GNU ld and lld both write --dependency-file)
        w.write("m.c", "int f(void);\nint main(void) { return f() - 2; }\n");
        let (_, log) = w.ec(rust(), &compiler, &["-c", "m.c"]);
        assert_eq!(log, row("compiled", "m.c new-key"));
        let link = ["m.o", "out/a.o", "-o", "prog"];
        let (out, log) = w.ec(rust(), &compiler, &link);
        assert!(out.status.success(), "{out:?}");
        // linked when the linker cannot write --dependency-file (GNU ld before 2.40)
        let stored = log == row("compiled", "prog new-key");
        let binary = w.read("prog");
        std::fs::remove_file(w.path().join("prog")).unwrap();
        let (_, log) = w.ec(rust(), &compiler, &link);
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
                .status_busy()
                .unwrap()
                .success()
        );
        // configure probe: one source straight to an executable
        let (_, log) = w.ec(rust(), &compiler, &["-o", "probe", "m.c", "out/a.o"]);
        assert_eq!(log, row("linked", "m.c"));
        let (_, log) = w.ec(rust(), &compiler, &["--version"]);
        assert_eq!(log, row("query", ""));
    }
}

/// The adopted rules A–D and Q-CC-5.
#[test]
fn adopted_rules() {
    let Some(compiler) = gcc().or_else(clang) else {
        eprintln!("skipped: no gcc/clang");
        return;
    };
    let Some(d) = daemon("EC_RUST_ECD") else {
        eprintln!("skipped: EC_RUST_ECD not set");
        return;
    };
    let w = Work::new(&d);
    w.write("a.c", "int f(void) { return 1; }\n");
    // rule A: a side output
    let (out, log) = w.ec(rust(), &compiler, &["-c", "a.c", "-gsplit-dwarf"]);
    assert!(out.status.success());
    assert_eq!(log, row("uncacheable", "a.c"));
    // rule B: a hidden input
    let (_, log) = w.ec(rust(), &compiler, &["-c", "a.c", "-specs=/dev/null"]);
    assert_eq!(log, row("uncacheable", "a.c"));
    // rule D: another toolchain's temp dir
    w.write("go-build123/b001/x.c", "int g(void) { return 2; }\n");
    let (_, log) = w.ec(rust(), &compiler, &["-c", "go-build123/b001/x.c"]);
    assert_eq!(log, row("uncacheable", "go-build123/b001/x.c"));
    // rule C before lookup: the source itself
    w.write("data.bin", "DATA");
    w.write("inc.S", ".data\n.incbin \"data.bin\"\n");
    let (out, log) = w.ec(rust(), &compiler, &["-c", "inc.S"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(log, row("uncacheable", "inc.S"));
    // rule C before storing: an included header
    w.write("blob.h", ".incbin \"data.bin\"\n");
    w.write("via.S", "#include \"blob.h\"\n");
    let (out, log) = w.ec(rust(), &compiler, &["-c", "via.S"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(log, row("compiled-unstored", "via.S incbin"));

    // Q-CC-5: a resource failure is not stored. A fake gcc writes its depfile and fails
    let bin = w.path().join("fake");
    std::fs::create_dir(&bin).unwrap();
    write_exe(
        &bin.join("gcc"),
        "#!/bin/sh\nwhile [ $# -gt 0 ]; do if [ \"$1\" = -MF ]; then printf 'a.o: a.c\\n' > \"$2\"; fi; shift; done\n\
         echo \"a.c: fatal error: cannot write a.o: $EKACACHE_FAKE_ERR\" >&2\nexit 1\n",
    );
    let fake = bin.join("gcc");
    let run_fake = |ec: &Path, err: &str| {
        let log = w.path().join("tmp/log");
        let _ = std::fs::remove_file(&log);
        let out = Command::new(ec)
            .arg(&fake)
            .args(["-c", "a.c"])
            .current_dir(w.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("EKACACHE_SOCK", &w.sock)
            .env("EKACACHE_LOG", &log)
            .env("EKACACHE_FAKE_ERR", err)
            .output_busy()
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
}

/// `-E` to stdout compiles to `<temp_directory>/ekacache<pid>.i`: `$TMPDIR` or `/tmp`, and a
/// relative `ekacache<pid>.i` when `TMPDIR` is set but empty (path join semantics).
#[test]
fn preprocess_temp_path() {
    let Some(d) = daemon("EC_RUST_ECD") else {
        eprintln!("skipped: EC_RUST_ECD not set");
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
    let output_arg = |ec: &Path, tmpdir: Option<&str>| {
        let mut cmd = Command::new(ec);
        cmd.arg(&fake)
            .args(["-E", "a.c"])
            .current_dir(w.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("EKACACHE_SOCK", &w.sock);
        if let Some(t) = tmpdir {
            cmd.env("TMPDIR", t);
        }
        let out = cmd.output_busy().unwrap();
        let err = String::from_utf8(out.stderr).unwrap();
        let after = err.split("[-o][").nth(1).unwrap_or_else(|| panic!("{err}"));
        let path = after.split(']').next().unwrap().to_string();
        path.replace(|c: char| c.is_ascii_digit(), "")
    };
    let tmp = w.path().join("tmp");
    let tmp = tmp.to_str().unwrap();
    assert_eq!(output_arg(rust(), None), "/tmp/ekacache.i");
    assert_eq!(output_arg(rust(), Some("")), "ekacache.i");
    assert_eq!(
        output_arg(rust(), Some(tmp)),
        format!("{tmp}/ekacache.i").replace(|c: char| c.is_ascii_digit(), "")
    );
}

// ---------------------------------------------------------------------------------------------
// Q-CC-2: a recording daemon

/// A fake ecd that answers every request plainly (GET: MISS) and records the command words.
/// With `refuse_puts`, every PUT gets `ERR` (a daemon that cannot store).
fn recording_daemon(refuse_puts: bool) -> (tempfile::TempDir, PathBuf, Arc<Mutex<Vec<String>>>) {
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
                            if refuse_puts { "ERR\n" } else { "OK\n" }.to_string()
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
    let (_dir, sock, seen) = recording_daemon(false);
    let w = Work::with_sock(sock);
    w.write("a.c", "int f(void) { return 1; }\n");
    w.write("bad.c", "int x = ;\n");
    for (args, outcome) in [
        (&["-c", "a.c", "-MD"][..], "compiled"),
        (&["-c", "bad.c"], "compiled-error"),
    ] {
        seen.lock().unwrap().clear();
        let (_, log) = w.ec(rust(), &compiler, args);
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

/// ADOPTED Q-CLIENT-2: a refused PUT is reported, the rest are skipped, the connection is not used
/// again, and the compile is logged as not stored with the compiler's status.
#[test]
fn refused_put() {
    let Some(compiler) = gcc().or_else(clang) else {
        eprintln!("skipped: no gcc/clang");
        return;
    };
    let (_dir, sock, seen) = recording_daemon(true);
    let w = Work::with_sock(sock);
    w.write("a.c", "int f(void) { return 1; }\n");
    w.write("bad.c", "int x = ;\n");
    for (args, outcome, status) in [
        (&["-c", "a.c"][..], "compiled-unstored", Some(0)),
        (&["-c", "bad.c"], "failed", Some(1)),
    ] {
        seen.lock().unwrap().clear();
        let (out, log) = w.ec(rust(), &compiler, args);
        let source = args[1];
        assert_eq!(log, row(outcome, &format!("{source} new-key")));
        assert_eq!(out.status.code(), status);
        let stderr = String::from_utf8(out.stderr).unwrap();
        let last = stderr.lines().last().unwrap();
        assert!(
            last.starts_with("ec: cache put m/") && last.ends_with(" failed"),
            "{stderr}"
        );
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.iter().filter(|c| *c == "PUT").count(), 1, "{seen:?}");
        assert_eq!(seen.last().map(String::as_str), Some("PUT"), "{seen:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// a storing fake daemon

/// A fake ecd that keeps what is PUT and serves it to GET (identities are always unknown).
/// With `die_at`, the first PUT whose key starts with it hangs up instead of storing: a store
/// batch that dies partway.
fn storing_daemon(die_at: Option<&'static str>) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("jf")
        .tempdir_in("/tmp")
        .unwrap();
    let sock = dir.path().join("s");
    let listener = UnixListener::bind(&sock).unwrap();
    let values = Arc::new(Mutex::new(HashMap::<String, Vec<u8>>::new()));
    let armed = Arc::new(Mutex::new(die_at));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let (values, armed) = (Arc::clone(&values), Arc::clone(&armed));
            std::thread::spawn(move || {
                let mut out = stream.try_clone().unwrap();
                let mut r = BufReader::new(stream);
                let mut line = String::new();
                while r.read_line(&mut line).is_ok_and(|n| n > 0) {
                    let words: Vec<String> = line.split_whitespace().map(str::to_string).collect();
                    line.clear();
                    let Some(cmd) = words.first() else { continue };
                    let reply: Vec<u8> = match cmd.as_str() {
                        "HELLO" => b"OK 1\n".to_vec(),
                        "GET" => match values.lock().unwrap().get(&words[1]) {
                            Some(v) => [format!("OK {}\n", v.len()).as_bytes(), v].concat(),
                            None => b"MISS\n".to_vec(),
                        },
                        "PUT" => {
                            let n: usize = words[2].parse().unwrap();
                            let mut v = vec![0; n];
                            r.read_exact(&mut v).unwrap();
                            let mut armed = armed.lock().unwrap();
                            if armed.is_some_and(|p| words[1].starts_with(p)) {
                                *armed = None;
                                return;
                            }
                            values.lock().unwrap().insert(words[1].clone(), v);
                            b"OK\n".to_vec()
                        }
                        "IDS" => {
                            let n: usize = words[1].parse().unwrap();
                            for _ in 0..n {
                                r.read_line(&mut line).unwrap();
                            }
                            line.clear();
                            "\n".repeat(n).into_bytes()
                        }
                        _ => b"OK\n".to_vec(),
                    };
                    if out.write_all(&reply).is_err() {
                        return;
                    }
                }
            });
        }
    });
    (dir, sock)
}

/// `behaviors/ec-cc-mode.md §CompileAndStore`: the manifest is PUT first, so a batch that dies
/// after `m/` leaves an entry without `o/`, which the next lookup reports as `object-gone` (and
/// then recompiles and stores).
#[test]
fn put_batch_dies_after_the_manifest() {
    let Some(compiler) = gcc().or_else(clang) else {
        eprintln!("skipped: no gcc/clang");
        return;
    };
    let (_dir, sock) = storing_daemon(Some("o/"));
    let w = Work::with_sock(sock);
    w.write("a.c", "int f(void) { return 1; }\n");
    let (out, log) = w.ec(rust(), &compiler, &["-c", "a.c"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(log, row("compiled-unstored", "a.c new-key"));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.starts_with("ec: cache put o/"), "{stderr}");
    let (out, log) = w.ec(rust(), &compiler, &["-c", "a.c"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(log, row("compiled", "a.c object-gone"));
    let (_, log) = w.ec(rust(), &compiler, &["-c", "a.c"]);
    assert_eq!(log, row("cached", "a.c"));
}

/// ADOPTED Q-KEY-3 for cc: an input that is gone by the time the manifest is built makes the
/// result unstorable, logged as `<source> not-cached:unreadable:<Key(path)>`, for a success and
/// for a deterministic failure alike.
#[test]
fn unreadable_input_is_not_stored() {
    let Some(compiler) = gcc().or_else(clang) else {
        eprintln!("skipped: no gcc/clang");
        return;
    };
    let (_dir, sock, seen) = recording_daemon(false);
    let w = Work::with_sock(sock);
    // the compiler removes the header it read
    std::fs::create_dir(w.path().join("bin")).unwrap();
    let wrapper = w.path().join("bin/gcc");
    write_exe(
        &wrapper,
        &format!(
            "#!/bin/sh\n'{}' \"$@\"\nrc=$?\nrm -f gone.h\nexit $rc\n",
            compiler.display()
        ),
    );
    w.write("a.c", "#include \"gone.h\"\nint f(void) { return N; }\n");
    w.write("bad.c", "#include \"gone.h\"\nint x = ;\n");
    for (args, outcome, ok) in [
        (&["-c", "a.c"][..], "compiled-unstored", true),
        (&["-c", "bad.c"], "failed", false),
    ] {
        w.write("gone.h", "#define N 1\n");
        seen.lock().unwrap().clear();
        let (out, log) = w.ec(rust(), &wrapper, args);
        assert_eq!(out.status.success(), ok, "{out:?}");
        let source = args[1];
        assert_eq!(
            log,
            row(outcome, &format!("{source} not-cached:unreadable:gone.h"))
        );
        let seen = seen.lock().unwrap().clone();
        assert!(!seen.iter().any(|c| c == "PUT"), "{seen:?}");
    }
}

/// cc in content identity mode against the storing fake daemon (no identities from ecd: every
/// input is hashed locally): miss, hit with the same object and depfile, invalidation.
#[test]
fn content_identity_mode() {
    let Some(compiler) = gcc().or_else(clang) else {
        eprintln!("skipped: no gcc/clang");
        return;
    };
    let (_dir, sock) = storing_daemon(None);
    let w = Work::with_sock(sock);
    w.write("inc/a.h", "#define N 1\n");
    w.write("a.c", "#include \"a.h\"\nint f(void) { return N; }\n");
    let env = [("EKACACHE_STORE_IDENTITY", "content")];
    // no system headers: their store paths resolve only through $EKACACHE_STORE_ROOTS
    let args = [
        "-nostdinc",
        "-Iinc",
        "-c",
        "a.c",
        "-o",
        "a.o",
        "-MD",
        "-MF",
        "a.d",
    ];
    let (out, log) = w.ec_env(rust(), &compiler, &args, &env);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(log, row("compiled", "a.c new-key"));
    let (object, depfile) = (w.read("a.o"), w.read("a.d"));
    std::fs::remove_file(w.path().join("a.o")).unwrap();
    std::fs::remove_file(w.path().join("a.d")).unwrap();
    let (out, log) = w.ec_env(rust(), &compiler, &args, &env);
    assert!(out.status.success() && out.stderr.is_empty(), "{out:?}");
    assert_eq!(log, row("cached", "a.c"));
    assert_eq!(w.read("a.o"), object);
    assert_eq!(w.read("a.d"), depfile);
    w.write("inc/a.h", "#define N 2\n");
    let (_, log) = w.ec_env(rust(), &compiler, &args, &env);
    assert_eq!(log, row("compiled", "a.c inputs-changed:inc/a.h"));
}
