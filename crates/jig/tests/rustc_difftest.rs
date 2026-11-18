//! rustc mode against the legacy C++ jig (`$JIG_LEGACY_JIG`) over the legacy Go jigd
//! (`$JIG_LEGACY_JIGD`), behind a proxy that records every request; skipped unless both are set.
//!
//! Both jigs run in the same crate directory, one after the other, with the out-dir emptied
//! before each run. What must match: exit status, stdout, stderr, the `$JIG_LOG` line, the files
//! left in the out-dir, whether rustc ran, the request sequence (keys numbered by first
//! appearance), the stored values, and k1 (computed here from `jig::rustc::key`, with legacy's
//! `rs-schema=7` swapped in for legacy). The allow-listed differences are the adopted ones: the
//! Rust client's `HELLO`, DONE after the PUTs (Q-CC-2 / Q-CLIENT-2), the schema, `$env:` lines
//! (Q-RS-3), comma-separated crate types (Q-RS-5) and unreadable inputs (Q-KEY-3).
#![allow(clippy::unwrap_used, clippy::too_many_lines)]

mod common;
#[path = "fixtures/rustc/harness.rs"]
mod harness;

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Command;

use common::Daemon;
use harness::{JIG, Req, Server, Snapshot, Ws, os, run_in, snapshot, take_log};
use jig::client::Client;
use jig::rustc::{key, pack, parse::parse_rust_invocation};
use jig_core::env::Env;
use jig_core::store_path::Store;

struct Diff {
    legacy: std::path::PathBuf,
    d: Daemon,
    proxy: Server,
}

fn setup() -> Option<Diff> {
    let Some(legacy) = common::legacy_jig() else {
        eprintln!("JIG_LEGACY_JIG not set: skipped");
        return None;
    };
    let Some(d) = common::daemon("JIG_LEGACY_JIGD") else {
        eprintln!("JIG_LEGACY_JIGD not set: skipped");
        return None;
    };
    let proxy = Server::proxy(&d.sock);
    Some(Diff { legacy, d, proxy })
}

/// What one run did.
#[derive(Debug)]
struct Run {
    code: Option<i32>,
    signal: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    log: Vec<(String, String)>,
    out: Snapshot,
    rustc_ran: usize,
    reqs: Vec<Req>,
}

impl Diff {
    fn legacy(&self) -> &OsStr {
        self.legacy.as_os_str()
    }

    /// `bin <argv…>` in `ws` against the proxy (or `sock`), out-dir as it is.
    fn run_at(
        &self,
        ws: &Ws,
        bin: &OsStr,
        sock: Option<&Path>,
        argv: &[OsString],
        env: &[(&str, &str)],
    ) -> Run {
        let before = ws.rustc_runs();
        let o = ws.jig_as(bin, sock.unwrap_or(&self.proxy.sock), argv, env);
        Run {
            code: o.status.code(),
            signal: o.status.signal(),
            stdout: o.stdout,
            stderr: o.stderr,
            log: take_log(&ws.log),
            out: snapshot(&ws.out),
            rustc_ran: ws.rustc_runs() - before,
            reqs: self.proxy.take_log(),
        }
    }

    /// Both jigs, legacy first, each with an empty out-dir that `prepare` then fills.
    fn both_with(
        &self,
        ws: &Ws,
        sock: Option<&Path>,
        argv: &[OsString],
        env: &[(&str, &str)],
        prepare: &dyn Fn(&Ws),
    ) -> (Run, Run) {
        ws.clear_out();
        prepare(ws);
        let l = self.run_at(ws, self.legacy(), sock, argv, env);
        ws.clear_out();
        prepare(ws);
        let r = self.run_at(ws, OsStr::new(JIG), sock, argv, env);
        (l, r)
    }

    fn both(&self, ws: &Ws, argv: &[OsString], env: &[(&str, &str)]) -> (Run, Run) {
        self.both_with(ws, None, argv, env, &|_| {})
    }

    /// k1 as legacy and Rust compute it for `rustc args…` in `ws` (stdlib identities asked of
    /// the daemon, as the jigs do).
    fn k1_pair(
        &self,
        ws: &Ws,
        rustc: &Path,
        args: &[OsString],
        env: &[(&str, &str)],
    ) -> (String, String) {
        let args: Vec<Vec<u8>> = args.iter().map(|a| a.as_bytes().to_vec()).collect();
        let inv = parse_rust_invocation(&args);
        let env = Env::from_pairs(env);
        let store = Store::from_env(&env);
        let mut client = Client::connected_to(self.d.sock_bytes()).unwrap();
        let tool = rustc.as_os_str().as_bytes();
        let stdlib = key::stdlib_ids(&mut client, &store, tool, &inv.sysroot);
        let source = std::fs::read(ws.root.join(OsStr::from_bytes(&inv.source))).unwrap();
        let mut fields = key::key_fields(
            &store,
            &inv,
            &key::KeyInputs {
                tool_path: tool,
                stdlib: &stdlib,
                cwd: ws.root.as_os_str().as_bytes(),
                env: &env,
                source: &source,
            },
        );
        let rust = text(key::request_key(&fields).text());
        fields[0] = b"rs-schema=7".to_vec();
        (text(key::request_key(&fields).text()), rust)
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8(b.to_vec()).unwrap()
}

/// Everything a user can see of a run is the same.
fn same(l: &Run, r: &Run, what: &str) {
    assert_eq!(l.code, r.code, "{what}: status");
    assert_eq!(l.signal, r.signal, "{what}: signal");
    assert_eq!(
        String::from_utf8_lossy(&l.stdout),
        String::from_utf8_lossy(&r.stdout),
        "{what}: stdout"
    );
    assert_eq!(
        String::from_utf8_lossy(&l.stderr),
        String::from_utf8_lossy(&r.stderr),
        "{what}: stderr"
    );
    assert_eq!(l.log, r.log, "{what}: JIG_LOG");
    assert_eq!(pid_free(&l.out), pid_free(&r.out), "{what}: out-dir");
    assert_eq!(l.rustc_ran, r.rustc_ran, "{what}: rustc runs");
}

/// The out-dir with `WriteFile`'s temporary names (`.jig<pid>.<name>`, left behind when the
/// rename fails) without the pid.
fn pid_free(out: &Snapshot) -> Snapshot {
    out.iter()
        .map(|(name, v)| {
            let name = match name.strip_prefix(".jig").and_then(|s| s.split_once('.')) {
                Some((pid, rest)) if pid.bytes().all(|b| b.is_ascii_digit()) => {
                    format!(".jig<pid>.{rest}")
                }
                _ => name.clone(),
            };
            (name, v.clone())
        })
        .collect()
}

/// The request lines without HELLO, each 32-hex-digit key named by first appearance.
fn shape(reqs: &[Req]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut name = |hex: &str| {
        let i = seen.iter().position(|s| s == hex).unwrap_or_else(|| {
            seen.push(hex.to_owned());
            seen.len() - 1
        });
        format!("#{i}")
    };
    reqs.iter()
        .filter(|r| r.verb != "HELLO")
        .map(|r| {
            let line = r.line();
            let mut out = String::new();
            let mut tok = String::new();
            for c in line.chars().chain(std::iter::once(' ')) {
                if c.is_ascii_hexdigit() && !c.is_ascii_uppercase() {
                    tok.push(c);
                    continue;
                }
                if tok.len() == 32 {
                    out.push_str(&name(&tok));
                } else {
                    out.push_str(&tok);
                }
                tok.clear();
                out.push(c);
            }
            out.pop();
            out
        })
        .collect()
}

/// ADOPTED Q-CC-2 / Q-CLIENT-2: legacy's DONE, which comes before its PUTs, moved after them.
fn adopted_order(mut legacy: Vec<String>) -> Vec<String> {
    if let Some(i) = legacy.iter().position(|l| l.starts_with("DONE "))
        && legacy.get(i + 1).is_some_and(|l| l.starts_with("PUT "))
    {
        let done = legacy.remove(i);
        let end = (i..legacy.len())
            .find(|&j| !legacy[j].starts_with("PUT "))
            .unwrap_or(legacy.len());
        legacy.insert(end, done);
    }
    legacy
}

/// The PUT values in order, by slot (`m`, `o`, `e`); `$env:` manifest lines left out.
fn stored(reqs: &[Req]) -> Vec<(char, Vec<u8>)> {
    reqs.iter()
        .filter(|r| r.verb == "PUT")
        .map(|r| {
            let slot = char::from(r.arg[0]);
            let mut v = r.value.clone().unwrap();
            if slot == 'm' {
                v = v
                    .split_inclusive(|&b| b == b'\n')
                    .filter(|l| !l.starts_with(b"$env:"))
                    .flatten()
                    .copied()
                    .collect();
            }
            (slot, v)
        })
        .collect()
}

fn env_lines(reqs: &[Req]) -> Vec<String> {
    let m = reqs
        .iter()
        .find(|r| r.verb == "PUT" && r.arg.starts_with(b"m/"))
        .unwrap();
    text(m.value.as_ref().unwrap())
        .lines()
        .filter(|l| l.starts_with("$env:"))
        .map(str::to_owned)
        .collect()
}

/// The k1 of the first `GET m/…`.
fn k1_of(reqs: &[Req]) -> String {
    reqs.iter()
        .find(|r| r.verb == "GET" && r.arg.starts_with(b"m/"))
        .map(|r| text(&r.arg[2..]))
        .unwrap()
}

/// The k2 of the first `PUT o/…`.
fn k2_of(reqs: &[Req]) -> Vec<u8> {
    reqs.iter()
        .find(|r| r.verb == "PUT" && r.arg.starts_with(b"o/"))
        .map(|r| r.arg[2..].to_vec())
        .unwrap()
}

fn log1(outcome: &str, subject: &str) -> Vec<(String, String)> {
    vec![(outcome.to_owned(), subject.to_owned())]
}

fn argv(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

/// `rustcwrap <rustc> <args…>`.
fn wrap_argv(rustc: &Path, args: &[OsString]) -> Vec<OsString> {
    let mut a = vec![OsString::from("rustcwrap"), rustc.as_os_str().to_owned()];
    a.extend_from_slice(args);
    a
}

fn write_mode(path: &Path, data: &[u8], mode: u32) {
    std::fs::write(path, data).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// A name, the rustc args, extra environment, the expected outcome.
type Case<'a> = (&'a str, Vec<OsString>, Vec<(&'a str, &'a str)>, &'a str);

/// §Invocation and §Uncached runs: usage, queries, what links, what is uncacheable, no source,
/// no daemon, the slot rules.
#[test]
fn difftest_uncached_and_usage() {
    let Some(t) = setup() else { return };
    let ws = Ws::new();
    std::fs::write(ws.root.join("src/main.rs"), "fn main() {}\n").unwrap();
    let args_log = ws.root.join("args.log");
    let args_log_s = args_log.to_str().unwrap().to_owned();

    let (l, r) = t.both(&ws, &argv(&["rustcwrap"]), &[]);
    same(&l, &r, "usage");
    assert_eq!(r.code, Some(1));
    assert!(l.reqs.is_empty() && r.reqs.is_empty());
    assert!(r.log.is_empty());

    let bin = argv(&[
        "--crate-name",
        "foo",
        "src/main.rs",
        "--crate-type",
        "bin",
        "--emit=dep-info,link",
        "--out-dir",
        "out",
    ]);
    let mut incremental = ws.lib_args(&["-C", "incremental=/tmp/inc"]);
    incremental.push(OsString::from("--cap-lints=allow"));
    let mut nosrc = ws.lib_args(&[]);
    nosrc[3] = OsString::from("src/missing.rs");
    let mut no_crate_type = ws.lib_args(&[]);
    no_crate_type.drain(4..6);
    let cases: Vec<Case<'_>> = vec![
        (
            "-vV",
            argv(&["-vV"]),
            vec![("FAKE_STDOUT", "rustc 1.0\n")],
            "query",
        ),
        (
            "print",
            argv(&[
                "-",
                "--crate-name",
                "___",
                "--print=file-names",
                "--crate-type",
                "bin",
            ]),
            vec![("FAKE_STDOUT", "lib___.rlib\n"), ("FAKE_STDERR", "e\n")],
            "query",
        ),
        (
            "bin",
            bin.clone(),
            vec![("FAKE_STATUS", "5"), ("FAKE_STDERR", "x\n")],
            "linked",
        ),
        (
            "bin build",
            bin.clone(),
            vec![("NIX_BUILD_TOP", "/build")],
            "linked",
        ),
        (
            "bin slot held",
            bin.clone(),
            vec![("JIG_SLOT", "1")],
            "linked",
        ),
        ("no crate type", no_crate_type, vec![], "linked"),
        (
            "incremental",
            incremental,
            vec![("FAKE_STDOUT", "o\n")],
            "uncacheable",
        ),
        (
            "no source",
            nosrc,
            vec![("FAKE_STATUS", "1")],
            "uncacheable-nosrc",
        ),
    ];
    for (what, args, mut env, outcome) in cases {
        env.push(("JIG_LOG_ARGS", &args_log_s));
        let (l, r) = t.both(&ws, &wrap_argv(&ws.rustc, &args), &env);
        same(&l, &r, what);
        assert_eq!(r.log[0].0, outcome, "{what}");
        assert_eq!(shape(&l.reqs), shape(&r.reqs), "{what}");
    }
    let (l, r) = t.both_with(
        &ws,
        Some(Path::new("/nonexistent/s")),
        &wrap_argv(&ws.rustc, &ws.lib_args(&[])),
        &[("FAKE_STDERR", "w\n")],
        &|_| {},
    );
    same(&l, &r, "no daemon");
    assert_eq!(r.log, log1("no-daemon", "foo-abc"));
    // rustc mode never writes JIG_LOG_ARGS
    assert!(!args_log.exists());
}

/// §Lookup and §Compile and store with the fake rustc: miss, stored values, k1, hit, other
/// stems and out-dirs, the launcher.
#[test]
fn difftest_miss_and_hit() {
    let Some(t) = setup() else { return };
    let ws = Ws::new();
    let out = ws.p("out");
    // outputs beyond the two rustc writes: an `@` in a name, a setuid file, a relative path, one
    // that does not exist
    let heads = format!(
        "{out}/libfoo-abc.rlib {out}/foo-abc.d {out}/x@y-abc.txt out/rel-abc.bin {out}/missing-abc.o"
    );
    let env = [
        ("FAKE_STDERR", "warning: w\n"),
        ("FAKE_MODE", "640"),
        ("FAKE_HEADS", heads.as_str()),
        ("FAKE_RLIB_EXTRA", "\x01\x7f"),
    ];
    let extra = |ws: &Ws| {
        write_mode(&ws.out.join("x@y-abc.txt"), b"xy", 0o4755);
        write_mode(&ws.out.join("rel-abc.bin"), b"\x00\x01", 0o600);
    };
    let args = ws.lib_args(&[]);
    let wrapped = wrap_argv(&ws.rustc, &args);

    let (l, r) = t.both_with(&ws, None, &wrapped, &env, &extra);
    same(&l, &r, "miss");
    // the schemas keep the two apart: Rust misses after legacy stored
    assert_eq!(r.log, log1("compiled", "foo-abc new-key"));
    assert_eq!(shape(&r.reqs), adopted_order(shape(&l.reqs)));
    assert_eq!(
        shape(&r.reqs),
        [
            "GET m/rs/#0",
            "SLOT -",
            "PUT m/rs/#0",
            "PUT o/#1",
            "PUT e/#1",
            "DONE -"
        ]
    );
    assert_eq!(stored(&l.reqs), stored(&r.reqs));
    let blob = &stored(&r.reqs)[1].1;
    for record in [
        &b"libfoo@.rlib 640 "[..],
        b"x@y@.txt 755 2\nxy",
        b"rel@.bin 600 2\n\x00\x01",
    ] {
        assert!(
            blob.windows(record.len()).any(|w| w == record),
            "{record:?}"
        );
    }
    assert!(!blob.windows(11).any(|w| w == b"missing@.o "));
    let (k1_legacy, k1_rust) = t.k1_pair(&ws, &ws.rustc, &args, &[]);
    assert_eq!(k1_of(&l.reqs), k1_legacy);
    assert_eq!(k1_of(&r.reqs), k1_rust);

    // hits: no rustc, the stored stderr
    let (l, r) = t.both(&ws, &wrapped, &[]);
    same(&l, &r, "hit");
    assert_eq!(r.log, log1("cached", "foo-abc"));
    assert_eq!(r.rustc_ran, 0);
    assert_eq!(r.stderr, b"warning: w\n");
    assert_eq!(shape(&l.reqs), shape(&r.reqs));
    // Q-RS-8: an `@` of the original name is rewritten too
    assert!(r.out.contains_key("x-abcy-abc.txt"), "{:?}", r.out.keys());
    assert_eq!(r.out["x-abcy-abc.txt"].1, 0o755);

    // Q-RS-7: other metadata, stem and out-dir hit the same entry
    let mut other = ws.lib_args(&[]);
    other[8] = OsString::from("metadata=def");
    other[10] = OsString::from("extra-filename=-def");
    let (l, r) = t.both(&ws, &wrap_argv(&ws.rustc, &other), &[]);
    same(&l, &r, "other stem");
    assert_eq!(r.log, log1("cached", "foo-def"));
    std::fs::create_dir(ws.root.join("out2")).unwrap();
    let mut moved = ws.lib_args(&[]);
    moved[12] = OsString::from("out2");
    let (l, r) = t.both(&ws, &wrap_argv(&ws.rustc, &moved), &[]);
    same(&l, &r, "other out-dir");
    assert_eq!(r.log, log1("cached", "foo-abc"));

    // the launcher: `jig /path/to/rustc …`
    let mut launcher = vec![ws.rustc.clone().into_os_string()];
    launcher.extend_from_slice(&args);
    let (l, r) = t.both(&ws, &launcher, &[]);
    same(&l, &r, "launcher");
    assert_eq!(r.log, log1("cached", "foo-abc"));

    // an input changed; then the crate root
    std::fs::write(ws.root.join("src/m.rs"), "pub fn g() {}\n").unwrap();
    let (l, r) = t.both(&ws, &wrapped, &[]);
    same(&l, &r, "input changed");
    assert_eq!(
        r.log,
        log1(
            "compiled",
            &format!("foo-abc inputs-changed:{}", ws.p("src/m.rs"))
        )
    );
    assert_eq!(stored(&l.reqs), stored(&r.reqs));
    std::fs::write(ws.root.join("src/lib.rs"), "pub mod m; // x\n").unwrap();
    let (l, r) = t.both(&ws, &wrapped, &[]);
    same(&l, &r, "source changed");
    assert_eq!(r.log, log1("compiled", "foo-abc new-key"));
}

/// The k1 fields from a cargo-like argv and environment: key args, externs (one missing), the
/// five variables, normalized paths.
#[test]
fn difftest_key_parity() {
    let Some(t) = setup() else { return };
    let ws = Ws::new();
    std::fs::write(ws.root.join("libbar.rlib"), "bar").unwrap();
    let root = ws.p("");
    let root = root.trim_end_matches('/');
    let glued_l = format!("-Ldependency={root}/./deps/");
    let bar = format!("bar={root}/libbar.rlib");
    let baz = format!("baz={root}/nope.rlib");
    let remap = format!("--remap-path-prefix={root}/./src/=/x/");
    let crate_root = format!("{root}/./src/../src/lib.rs");
    let mut args = ws.lib_args(&[
        "--cfg",
        "feature=\"x\"",
        &glued_l,
        "-L",
        "dependency=/elsewhere",
        "--extern",
        &bar,
        "--extern",
        &baz,
        "--extern",
        "noval",
        "-C",
        "opt-level=2",
        "-Cdebuginfo=2",
        "--cap-lints",
        "allow",
        "--error-format=json",
        "--json=diagnostic-rendered-ansi",
        &remap,
        "--check-cfg",
        "cfg(feature, values(\"x\"))",
        "-Ccodegen-units=1",
        "--crate-type=lib",
    ]);
    args[3] = OsString::from(&crate_root);
    let env = [
        ("CARGO_PKG_NAME", "foo"),
        ("CARGO_PKG_VERSION", "0.1.0"),
        ("CARGO_CFG_TARGET_FEATURE", "fxsr,sse"),
        ("RUSTFLAGS", "-Dwarnings"),
        ("CARGO_ENCODED_RUSTFLAGS", "-Dwarnings\x1f-Cx"),
        ("CARGO_PKG_AUTHORS", "not in the key"),
    ];
    let (l, r) = t.both(&ws, &wrap_argv(&ws.rustc, &args), &env);
    same(&l, &r, "cargo-like");
    assert_eq!(r.log, log1("compiled", "foo-abc new-key"));
    let (k1_legacy, k1_rust) = t.k1_pair(&ws, &ws.rustc, &args, &env);
    assert_eq!(k1_of(&l.reqs), k1_legacy);
    assert_eq!(k1_of(&r.reqs), k1_rust);
    assert_eq!(stored(&l.reqs), stored(&r.reqs));
    let (l, r) = t.both(&ws, &wrap_argv(&ws.rustc, &args), &env);
    same(&l, &r, "cargo-like hit");
    assert_eq!(r.log, log1("cached", "foo-abc"));
}

/// Q-RS-1, no dep-info; and the adopted Q-RS-5 and Q-KEY-3 differences.
#[test]
fn difftest_failures_and_unstored() {
    let Some(t) = setup() else { return };
    let ws = Ws::new();
    let wrapped = wrap_argv(&ws.rustc, &ws.lib_args(&[]));
    for (what, env, outcome) in [
        (
            "failure",
            vec![("FAKE_STATUS", "3"), ("FAKE_STDERR", "error[E0425]\n")],
            "failed",
        ),
        (
            "no dep-info",
            vec![("FAKE_NO_DEPINFO", "1"), ("FAKE_STDERR", "w\n")],
            "compiled-unstored",
        ),
    ] {
        for _ in 0..2 {
            let (l, r) = t.both(&ws, &wrapped, &env);
            same(&l, &r, what);
            assert_eq!(r.log, log1(outcome, "foo-abc new-key"), "{what}");
            assert_eq!(r.rustc_ran, 1);
            assert_eq!(shape(&l.reqs), shape(&r.reqs), "{what}");
            assert_eq!(shape(&r.reqs)[1..], ["SLOT -", "DONE -"], "{what}");
        }
    }

    // ADOPTED Q-RS-5: `lib,cdylib` links; legacy cached it
    let mut lc = ws.lib_args(&[]);
    lc[5] = OsString::from("lib,cdylib");
    let (l, r) = t.both(&ws, &wrap_argv(&ws.rustc, &lc), &[]);
    assert_eq!(l.log, log1("compiled", "foo-abc new-key"));
    assert_eq!(r.log, log1("linked", "foo-abc"));
    assert_eq!(shape(&r.reqs), ["SLOT -", "DONE -"]);
    assert_eq!((l.code, &l.out), (r.code, &r.out));
    // the separate form was already split by legacy's codegen-option joining: both link
    let (l, r) = t.both(
        &ws,
        &wrap_argv(&ws.rustc, &ws.lib_args(&["--crate-type", "cdylib"])),
        &[],
    );
    same(&l, &r, "separate cdylib");
    assert_eq!(r.log, log1("linked", "foo-abc"));

    // ADOPTED Q-KEY-3: an input that is gone makes the result unstorable; legacy dropped the line
    let (l, r) = t.both(&ws, &wrapped, &[("FAKE_INPUTS", "src/m.rs gone.rs")]);
    assert_eq!(l.log, log1("compiled", "foo-abc new-key"));
    assert_eq!(
        r.log,
        log1(
            "compiled-unstored",
            &format!("foo-abc not-cached:unreadable:{}", ws.p("gone.rs"))
        )
    );
    assert!(l.reqs.iter().any(|q| q.verb == "PUT"));
    assert!(!r.reqs.iter().any(|q| q.verb == "PUT"));
    assert_eq!((l.code, &l.stderr, &l.out), (r.code, &r.stderr, &r.out));
}

/// ADOPTED Q-RS-3: legacy ignores `# env-dep:` lines (and replays a stale result); Rust adds
/// `$env:` lines and misses. Everything else is stored alike.
#[test]
fn difftest_env_deps() {
    let Some(t) = setup() else { return };
    let ws = Ws::new();
    let wrapped = wrap_argv(&ws.rustc, &ws.lib_args(&[]));
    let deps = ("FAKE_ENV_DEPS", "JIG_T=x OTHER");
    let (l, r) = t.both(&ws, &wrapped, &[deps, ("JIG_T", "1")]);
    same(&l, &r, "env-dep miss");
    assert_eq!(stored(&l.reqs), stored(&r.reqs));
    assert!(env_lines(&l.reqs).is_empty());
    assert_eq!(
        env_lines(&r.reqs),
        [
            text(&jig_core::manifest::env_line(b"JIG_T", Some(b"1"))),
            "$env:OTHER\tunset".to_owned()
        ]
    );
    let (l, r) = t.both(&ws, &wrapped, &[deps, ("JIG_T", "1")]);
    same(&l, &r, "env-dep hit");
    let (l, r) = t.both(&ws, &wrapped, &[deps, ("JIG_T", "2")]);
    assert_eq!(l.log, log1("cached", "foo-abc"));
    assert_eq!(r.log, log1("compiled", "foo-abc env-changed:JIG_T"));
    assert_eq!((l.code, &l.out), (r.code, &r.out));
}

/// §Output blob: byte-identical blobs both ways, and the same restore from crafted blobs written
/// over each jig's `o/<k2>`.
#[test]
fn difftest_pack_interop() {
    let Some(t) = setup() else { return };
    let ws = Ws::new();
    let wrapped = wrap_argv(&ws.rustc, &ws.lib_args(&[]));
    let (l, r) = t.both(
        &ws,
        &wrapped,
        &[("FAKE_STDERR", "s\n"), ("FAKE_MODE", "751")],
    );
    same(&l, &r, "miss");
    let (legacy_blob, rust_blob) = (stored(&l.reqs)[1].1.clone(), stored(&r.reqs)[1].1.clone());
    assert_eq!(legacy_blob, rust_blob);
    let keys = [k2_of(&l.reqs), k2_of(&r.reqs)];

    // Rust's unpack of legacy's blob is legacy's restore
    let (lh, rh) = t.both(&ws, &wrapped, &[]);
    same(&lh, &rh, "hit");
    let dir = tempfile::tempdir().unwrap();
    assert!(pack::unpack_files(
        &legacy_blob,
        dir.path().as_os_str().as_bytes(),
        b"-abc"
    ));
    assert_eq!(snapshot(dir.path()), lh.out);

    let crafted: [&[u8]; 20] = [
        b"libfoo@.rlib 600 3\nabcfoo@.d 755 0\nx@@y 4755 2\nhi",
        b"",
        b"a  644  2\nhi",
        b"a 644 2\t\nhi",
        b"a\t644 2\nhi",
        b" a 644 1\nz",
        b"a 1000640 1\nz",
        b"a 0644 1\nz",
        b"a 777 1\nz",
        b"a 644 99\nxx",
        b"a 6x4 1\nz",
        b"a 8 1\nz",
        b"a 644\nz",
        b"a 644 1 x\nz",
        b"sub/x 644 1\nz",
        b"a 644 1\nzb 644 1\n",
        b"a 644 1",
        b"a 644 -1\nz",
        b"a 644 +1\nz",
        b".. 644 1\nz",
    ];
    let mut client = Client::connected_to(t.d.sock_bytes()).unwrap();
    for blob in crafted {
        let what = format!("blob {:?}", String::from_utf8_lossy(blob));
        for k2 in &keys {
            let mut slot = b"o/".to_vec();
            slot.extend_from_slice(k2);
            assert!(client.put(&slot, blob), "{what}");
        }
        let (l, r) = t.both(&ws, &wrapped, &[]);
        same(&l, &r, &what);
        if r.log[0].0 == "cached" {
            let dir = tempfile::tempdir().unwrap();
            assert!(pack::unpack_files(
                blob,
                dir.path().as_os_str().as_bytes(),
                b"-abc"
            ));
            assert_eq!(snapshot(dir.path()), r.out, "{what}");
        } else {
            assert_eq!(r.log, log1("compiled", "foo-abc object-gone"), "{what}");
        }
    }
}

/// The real toolchain by its full path (so StdlibIds hashes the stdlib rlibs), in path and in
/// content identity mode (the stdlib identities come from jigd's IDS).
#[test]
fn difftest_real_rustc() {
    let Some(t) = setup() else { return };
    let Ok(o) = Command::new("rustc").args(["--print", "sysroot"]).output() else {
        eprintln!("no rustc on PATH: skipped");
        return;
    };
    let sysroot = String::from_utf8(o.stdout).unwrap().trim().to_owned();
    let rustc = Path::new(&sysroot).join("bin/rustc");
    let ws = Ws::new();
    std::fs::write(ws.root.join("src/m.rs"), "pub fn f() -> u32 {\n    42\n}\n").unwrap();
    let args = ws.lib_args(&[]);
    let wrapped = wrap_argv(&rustc, &args);
    for mode in ["path", "content"] {
        let env: Vec<(&str, &str)> = if mode == "content" {
            vec![("JIG_STORE_IDENTITY", "content")]
        } else {
            vec![]
        };
        let (l, r) = t.both(&ws, &wrapped, &env);
        same(&l, &r, mode);
        assert_eq!(r.log, log1("compiled", "foo-abc new-key"), "{mode}");
        assert!(r.out.contains_key("libfoo-abc.rlib") && r.out.contains_key("libfoo-abc.rmeta"));
        assert_eq!(shape(&r.reqs), adopted_order(shape(&l.reqs)), "{mode}");
        let ids = |reqs: &[Req]| -> Vec<Vec<Vec<u8>>> {
            reqs.iter()
                .filter(|q| q.verb == "IDS")
                .map(|q| q.paths.clone())
                .collect()
        };
        assert_eq!(ids(&l.reqs), ids(&r.reqs), "{mode}");
        assert_eq!(ids(&r.reqs).is_empty(), mode == "path", "{mode}");
        assert_eq!(stored(&l.reqs), stored(&r.reqs), "{mode}");
        let (k1_legacy, k1_rust) = t.k1_pair(&ws, &rustc, &args, &env);
        assert_eq!(k1_of(&l.reqs), k1_legacy, "{mode}");
        assert_eq!(k1_of(&r.reqs), k1_rust, "{mode}");
        let mut client = Client::connected_to(t.d.sock_bytes()).unwrap();
        let stdlib = key::stdlib_ids(
            &mut client,
            &Store::from_env(&Env::from_pairs(&env)),
            rustc.as_os_str().as_bytes(),
            b"",
        );
        assert!(
            stdlib.len() > 2 && !stdlib.contains(&b'?'),
            "{}",
            String::from_utf8_lossy(&stdlib)
        );

        let (l, r) = t.both(&ws, &wrapped, &env);
        same(&l, &r, mode);
        assert_eq!(r.log, log1("cached", "foo-abc"), "{mode}");
    }
}

/// `fs::current_path()` fails: both terminate on SIGABRT before running rustc (the message
/// differs: libstdc++'s terminate handler).
#[test]
fn difftest_removed_cwd() {
    let Some(t) = setup() else { return };
    let ws = Ws::new();
    let gone = ws.root.join("gone");
    let mut args = ws.lib_args(&[]);
    args[3] = OsString::from(ws.p("src/lib.rs"));
    let mut runs = Vec::new();
    for bin in [t.legacy(), OsStr::new(JIG)] {
        std::fs::create_dir(&gone).unwrap();
        let mut script = format!(
            "cd '{0}' && rmdir '{0}' && exec '{1}' rustcwrap '{2}'",
            gone.display(),
            Path::new(bin).display(),
            ws.rustc.display()
        );
        for a in &args {
            script.push_str(" '");
            script.push_str(a.to_str().unwrap());
            script.push('\'');
        }
        let o = run_in(
            os("sh"),
            &[os("-c"), os(&script)],
            &ws.root,
            &t.proxy.sock,
            &ws.log,
            &[("FAKE_RUSTC_LOG", ws.rustc_log.as_os_str())],
        );
        runs.push((
            o.status.signal(),
            o.stdout,
            take_log(&ws.log),
            shape(&t.proxy.take_log()),
        ));
    }
    assert_eq!(runs[0], runs[1]);
    assert_eq!(runs[1].0, Some(6));
    assert_eq!(ws.rustc_runs(), 0);
}
