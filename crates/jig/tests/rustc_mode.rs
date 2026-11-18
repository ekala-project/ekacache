//! rustc mode end to end (`behaviors/jig-rustc-mode.md`, `implementation/07-jig-rustc.md
//! §Tests`): the jig binary against an in-memory jigd that records every request, with a fake
//! rustc (`fixtures/rustc/fake-rustc.sh`) for the paths through `run`, and the real rustc and
//! cargo for env-deps and a workspace built twice.
#![allow(clippy::unwrap_used, clippy::too_many_lines)]

#[path = "fixtures/rustc/harness.rs"]
mod harness;

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Command;

use harness::{JIG, Server, Ws, have_rustc, lines, os, read_log, run_in, snapshot, take_log};
use jig_core::env::Env;
use jig_core::manifest::{env_id, env_line, result_key_of};
use jig_core::store_path::Store;

fn input_line(path: &str) -> String {
    let store = Store::from_env(&Env::default());
    let id = store.input_id(path.as_bytes()).unwrap();
    format!("{path}\t{}", String::from_utf8(id).unwrap())
}

fn log1(outcome: &str, subject: &str) -> Vec<(String, String)> {
    vec![(outcome.to_owned(), subject.to_owned())]
}

/// The k1 text of the first `GET m/…`.
fn k1_of(log: &[harness::Req]) -> String {
    log.iter()
        .find_map(|r| {
            (r.verb == "GET")
                .then(|| String::from_utf8(r.arg.clone()).unwrap())
                .and_then(|k| k.strip_prefix("m/").map(str::to_owned))
        })
        .unwrap()
}

fn put_value<'a>(log: &'a [harness::Req], key: &str) -> &'a [u8] {
    log.iter()
        .find(|r| r.verb == "PUT" && r.arg == key.as_bytes())
        .and_then(|r| r.value.as_deref())
        .unwrap_or_else(|| panic!("no PUT {key} in {:?}", lines(log)))
}

/// `o/` record of a file as `PackFiles` writes it.
fn record(name: &str, path: &Path) -> Vec<u8> {
    let data = std::fs::read(path).unwrap();
    let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let mut r = format!("{name} {mode:o} {}\n", data.len()).into_bytes();
    r.extend_from_slice(&data);
    r
}

#[test]
fn usage() {
    let ws = Ws::new();
    let o = ws.jig(
        Path::new("/nonexistent/s"),
        &[OsString::from("rustcwrap")],
        &[],
    );
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(o.stderr, b"rustcwrap: usage: rustcwrap <rustc> args...\n");
    assert!(o.stdout.is_empty());
    assert!(read_log(&ws.log).is_empty());
    // the same as argv[0]
    let link = ws.root.join("bin/rustcwrap");
    symlink(JIG, &link).unwrap();
    let o = run_in(
        link.as_os_str(),
        &[],
        &ws.root,
        Path::new("/x"),
        &ws.log,
        &[],
    );
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(o.stderr, b"rustcwrap: usage: rustcwrap <rustc> args...\n");
}

/// §Uncached runs: the outcome table, the slot rule, inherited stdio, the status.
#[test]
fn uncached_outcomes() {
    let ws = Ws::new();
    let d = Server::fake();
    let args_log = ws.root.join("args.log");
    let args_log_s = args_log.to_str().unwrap().to_owned();
    let env = [("JIG_LOG_ARGS", args_log_s.as_str())];

    // a query: no connection at all, stdout and stderr pass through
    let q: Vec<OsString> = [
        "-",
        "--crate-name",
        "___",
        "--print=file-names",
        "--crate-type",
        "bin",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    let o = ws.wrap(
        &d.sock,
        &q,
        &[
            ("FAKE_STDOUT", "lib___.rlib\n"),
            ("FAKE_STDERR", "e\n"),
            env[0],
        ],
    );
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(o.stdout, b"lib___.rlib\n");
    assert_eq!(o.stderr, b"e\n");
    assert_eq!(take_log(&ws.log), log1("query", "___"));
    assert!(d.take_log().is_empty());
    // no crate name: the label is the (empty) source
    let o = ws.wrap(&d.sock, &[OsString::from("-vV")], &[]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(take_log(&ws.log), log1("query", ""));
    assert!(d.take_log().is_empty());

    // a binary links: under a slot, status passed through
    let bin: Vec<OsString> = [
        "--crate-name",
        "foo",
        "src/main.rs",
        "--crate-type",
        "bin",
        "--emit=dep-info,link",
        "--out-dir",
        "out",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    let o = ws.wrap(&d.sock, &bin, &[("FAKE_STATUS", "5"), env[0]]);
    assert_eq!(o.status.code(), Some(5));
    assert_eq!(take_log(&ws.log), log1("linked", "foo"));
    assert_eq!(lines(&d.take_log()), ["HELLO 1", "SLOT -", "DONE -"]);
    // NIX_BUILD_TOP names the build; a parent's slot is reused
    ws.wrap(&d.sock, &bin, &[("NIX_BUILD_TOP", "/build")]);
    assert_eq!(
        lines(&d.take_log()),
        ["HELLO 1", "SLOT /build", "DONE /build"]
    );
    ws.wrap(&d.sock, &bin, &[("JIG_SLOT", "1")]);
    assert!(d.take_log().is_empty());
    take_log(&ws.log);

    // incremental: uncacheable, not linked
    let o = ws.wrap(
        &d.sock,
        &ws.lib_args(&["-C", "incremental=/tmp/inc"]),
        &[env[0]],
    );
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(take_log(&ws.log), log1("uncacheable", "foo-abc"));
    assert_eq!(lines(&d.take_log()), ["HELLO 1", "SLOT -", "DONE -"]);
    // Q-RS-5: a comma-separated crate type that links
    let mut lc = ws.lib_args(&[]);
    lc[5] = OsString::from("lib,cdylib");
    ws.wrap(&d.sock, &lc, &[]);
    assert_eq!(take_log(&ws.log), log1("linked", "foo-abc"));
    d.take_log();

    // cacheable, but the source cannot be read: uncacheable-nosrc, under a slot
    let mut nosrc = ws.lib_args(&[]);
    nosrc[3] = OsString::from("src/missing.rs");
    let o = ws.wrap(&d.sock, &nosrc, &[("FAKE_STATUS", "1")]);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(take_log(&ws.log), log1("uncacheable-nosrc", "foo-abc"));
    assert_eq!(lines(&d.take_log()), ["HELLO 1", "SLOT -", "DONE -"]);

    // no daemon: rustc runs anyway
    let o = ws.wrap(
        Path::new("/nonexistent/s"),
        &ws.lib_args(&[]),
        &[("FAKE_STDERR", "w\n")],
    );
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(o.stderr, b"w\n");
    assert_eq!(take_log(&ws.log), log1("no-daemon", "foo-abc"));
    assert!(ws.out.join("libfoo-abc.rlib").exists());

    // rustc mode never writes JIG_LOG_ARGS
    assert!(!args_log.exists());
}

/// §Lookup and §Compile and store: miss, store, hit; the adopted DONE-after-PUTs order.
#[test]
fn miss_store_hit() {
    let ws = Ws::new();
    let d = Server::fake();
    let args = ws.lib_args(&[]);
    let env = [("FAKE_STDERR", "warning: unused\n"), ("FAKE_MODE", "640")];

    let o = ws.wrap(&d.sock, &args, &env);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert_eq!(o.stderr, b"warning: unused\n");
    assert_eq!(take_log(&ws.log), log1("compiled", "foo-abc new-key"));
    let log = d.take_log();
    let k1 = k1_of(&log);
    assert!(k1.starts_with("rs/") && k1.len() == 35, "{k1}");
    let manifest = format!("{}\n", input_line(&ws.p("src/m.rs")));
    let k2 = result_key_of(k1.as_bytes(), &[manifest.trim_end()]);
    let k2 = String::from_utf8(k2.text().to_vec()).unwrap();
    // ADOPTED Q-CC-2 / Q-CLIENT-2: DONE after the PUTs
    assert_eq!(
        lines(&log),
        [
            "HELLO 1".to_owned(),
            format!("GET m/{k1}"),
            "SLOT -".to_owned(),
            format!("PUT m/{k1}"),
            format!("PUT o/{k2}"),
            format!("PUT e/{k2}"),
            "DONE -".to_owned(),
        ]
    );
    assert_eq!(put_value(&log, &format!("m/{k1}")), manifest.as_bytes());
    let mut blob = record("libfoo@.rlib", &ws.out.join("libfoo-abc.rlib"));
    blob.extend(record("foo@.d", &ws.out.join("foo-abc.d")));
    assert_eq!(put_value(&log, &format!("o/{k2}")), &blob[..]);
    // Q-RS-2: stderr stored raw
    assert_eq!(put_value(&log, &format!("e/{k2}")), b"warning: unused\n");
    let dep = std::fs::read_to_string(ws.out.join("foo-abc.d")).unwrap();
    assert!(dep.starts_with(&format!(
        "{}/libfoo-abc.rlib: src/lib.rs src/m.rs\n",
        ws.p("out")
    )));

    // a hit: no slot, no rustc, files and modes restored, the stored stderr replayed
    let before = snapshot(&ws.out);
    assert_eq!(before["libfoo-abc.rlib"].1, 0o640);
    ws.clear_out();
    let runs = ws.rustc_runs();
    let o = ws.wrap(&d.sock, &args, &[]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(o.stderr, b"warning: unused\n");
    assert!(o.stdout.is_empty());
    assert_eq!(ws.rustc_runs(), runs);
    assert_eq!(take_log(&ws.log), log1("cached", "foo-abc"));
    let log = d.take_log();
    assert_eq!(
        lines(&log),
        [
            "HELLO 1".to_owned(),
            format!("GET m/{k1}"),
            format!("GET o/{k2}"),
            format!("GET e/{k2}"),
        ]
    );
    assert_eq!(snapshot(&ws.out), before);

    // Q-RS-7: another metadata/extra-filename is the same k1; outputs are renamed, contents
    // (the .d too) are the stored ones
    ws.clear_out();
    let mut other = ws.lib_args(&[]);
    other[8] = OsString::from("metadata=def");
    other[10] = OsString::from("extra-filename=-def");
    let o = ws.wrap(&d.sock, &other, &[]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(take_log(&ws.log), log1("cached", "foo-def"));
    assert_eq!(k1_of(&d.take_log()), k1);
    let now = snapshot(&ws.out);
    assert_eq!(
        now.keys().collect::<Vec<_>>(),
        ["foo-def.d", "libfoo-def.rlib"]
    );
    assert_eq!(now["foo-def.d"], before["foo-abc.d"]);
    // so does another out-dir
    let elsewhere = ws.root.join("out2");
    std::fs::create_dir(&elsewhere).unwrap();
    let mut moved = ws.lib_args(&[]);
    moved[12] = elsewhere.clone().into_os_string();
    ws.wrap(&d.sock, &moved, &[]);
    assert_eq!(take_log(&ws.log), log1("cached", "foo-abc"));
    assert_eq!(snapshot(&elsewhere), before);

    // the launcher: `jig /path/to/rustc …` is the same mode and key
    ws.clear_out();
    let mut argv = vec![ws.rustc.clone().into_os_string()];
    argv.extend_from_slice(&args);
    let o = ws.jig(&d.sock, &argv, &[]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(take_log(&ws.log), log1("cached", "foo-abc"));
    assert_eq!(k1_of(&d.take_log()), k1);

    // the object is gone: recompile and store again
    d.remove(format!("o/{k2}").as_bytes());
    ws.wrap(&d.sock, &args, &[]);
    assert_eq!(take_log(&ws.log), log1("compiled", "foo-abc object-gone"));
    // a blob that does not unpack is "gone" too; files before the bad record stay
    ws.clear_out();
    d.put(format!("o/{k2}").as_bytes(), b"first 600 2\nokbad header\n");
    let runs = ws.rustc_runs();
    ws.wrap(&d.sock, &args, &[("FAKE_NO_DEPINFO", "1")]);
    assert_eq!(
        take_log(&ws.log),
        log1("compiled-unstored", "foo-abc object-gone")
    );
    assert_eq!(ws.rustc_runs(), runs + 1);
    assert_eq!(std::fs::read(ws.out.join("first")).unwrap(), b"ok");
    assert_eq!(snapshot(&ws.out)["first"].1, 0o600);
    d.take_log();

    // an input changed
    std::fs::write(ws.root.join("src/m.rs"), "pub fn g() {}\n").unwrap();
    ws.wrap(&d.sock, &args, &[]);
    assert_eq!(
        take_log(&ws.log),
        log1(
            "compiled",
            &format!("foo-abc inputs-changed:{}", ws.p("src/m.rs"))
        )
    );
    // the crate root is in k1 itself
    std::fs::write(ws.root.join("src/lib.rs"), "pub mod m; // x\n").unwrap();
    ws.wrap(&d.sock, &args, &[]);
    assert_eq!(take_log(&ws.log), log1("compiled", "foo-abc new-key"));
}

/// Q-RS-1: failures are never cached; nothing is PUT and the next run compiles again.
#[test]
fn failures_are_not_cached() {
    let ws = Ws::new();
    let d = Server::fake();
    let args = ws.lib_args(&[]);
    for _ in 0..2 {
        let o = ws.wrap(
            &d.sock,
            &args,
            &[("FAKE_STATUS", "3"), ("FAKE_STDERR", "error[E0425]\n")],
        );
        assert_eq!(o.status.code(), Some(3));
        assert_eq!(o.stderr, b"error[E0425]\n");
        assert_eq!(take_log(&ws.log), log1("failed", "foo-abc new-key"));
        let log = d.take_log();
        assert_eq!(lines(&log)[2..], ["SLOT -", "DONE -"]);
    }
    assert_eq!(ws.rustc_runs(), 2);
}

/// No dep-info, an unreadable input (ADOPTED Q-KEY-3), refused PUTs (ADOPTED Q-CLIENT-2).
#[test]
fn compiled_unstored() {
    let ws = Ws::new();
    let d = Server::fake();
    let args = ws.lib_args(&[]);
    let no_put = |log: &[harness::Req]| !log.iter().any(|r| r.verb == "PUT");

    let o = ws.wrap(&d.sock, &args, &[("FAKE_NO_DEPINFO", "1")]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(
        take_log(&ws.log),
        log1("compiled-unstored", "foo-abc new-key")
    );
    let log = d.take_log();
    assert!(no_put(&log));
    assert_eq!(lines(&log).last().unwrap(), "DONE -");

    // an input that is gone after the compile
    let o = ws.wrap(&d.sock, &args, &[("FAKE_INPUTS", "src/m.rs gone.rs")]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(
        take_log(&ws.log),
        log1(
            "compiled-unstored",
            &format!("foo-abc not-cached:unreadable:{}", ws.p("gone.rs"))
        )
    );
    assert!(no_put(&d.take_log()));

    // the manifest PUT is refused: reported, nothing else is sent (the connection is dead)
    d.with(|s| s.refuse_put_prefix = Some(b"m/".to_vec()));
    let o = ws.wrap(&d.sock, &args, &[("FAKE_STDERR", "w\n")]);
    assert_eq!(o.status.code(), Some(0));
    let log = d.take_log();
    let k1 = k1_of(&log);
    assert_eq!(
        String::from_utf8(o.stderr).unwrap(),
        format!("w\njig: cache put m/{k1} failed\n")
    );
    assert_eq!(
        take_log(&ws.log),
        log1("compiled-unstored", "foo-abc new-key")
    );
    assert_eq!(lines(&log).last().unwrap(), &format!("PUT m/{k1}"));
    // the object PUT is refused: the manifest is stored, the stderr PUT is not sent
    d.with(|s| s.refuse_put_prefix = Some(b"o/".to_vec()));
    let o = ws.wrap(&d.sock, &args, &[]);
    let log = d.take_log();
    let puts: Vec<String> = lines(&log)
        .into_iter()
        .filter(|l| l.starts_with("PUT"))
        .collect();
    assert_eq!(puts.len(), 2);
    assert_eq!(
        String::from_utf8(o.stderr).unwrap(),
        format!("jig: cache put {} failed\n", &puts[1][4..])
    );
    assert_eq!(
        take_log(&ws.log),
        log1("compiled-unstored", "foo-abc new-key")
    );
    // the next run finds the manifest without its object
    d.with(|s| s.refuse_put_prefix = None);
    ws.wrap(&d.sock, &args, &[]);
    assert_eq!(take_log(&ws.log), log1("compiled", "foo-abc object-gone"));
}

/// Q-RS-4: no readable output is an empty blob, stored and replayed as a hit that writes
/// nothing.
#[test]
fn q_rs_4_empty_blob() {
    let ws = Ws::new();
    let d = Server::fake();
    let args = ws.lib_args(&[]);
    let env = [("FAKE_HEADS", "elsewhere/libfoo-abc.rlib")];
    ws.wrap(&d.sock, &args, &env);
    assert_eq!(take_log(&ws.log), log1("compiled", "foo-abc new-key"));
    let log = d.take_log();
    let o_put = log
        .iter()
        .find(|r| r.verb == "PUT" && r.arg.starts_with(b"o/"))
        .unwrap();
    assert_eq!(o_put.value.as_deref(), Some(&b""[..]));
    ws.clear_out();
    let o = ws.wrap(&d.sock, &args, &env);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(take_log(&ws.log), log1("cached", "foo-abc"));
    assert!(snapshot(&ws.out).is_empty());
}

/// ADOPTED Q-RS-3 with the fake rustc: `$env:` lines from jig's environment, in dep-info order,
/// after the inputs; `env-changed:` misses; unset and set-but-empty differ.
#[test]
fn env_dep_lines() {
    let ws = Ws::new();
    let d = Server::fake();
    let args = ws.lib_args(&[]);
    let deps = ("FAKE_ENV_DEPS", "JIG_T=from-rustc OTHER JIG_T=again");

    ws.wrap(&d.sock, &args, &[deps, ("JIG_T", "1")]);
    assert_eq!(take_log(&ws.log), log1("compiled", "foo-abc new-key"));
    let log = d.take_log();
    let k1 = k1_of(&log);
    let want = format!(
        "{}\n{}\n{}\n",
        input_line(&ws.p("src/m.rs")),
        String::from_utf8(env_line(b"JIG_T", Some(b"1"))).unwrap(),
        "$env:OTHER\tunset",
    );
    assert_eq!(put_value(&log, &format!("m/{k1}")), want.as_bytes());
    // the same environment hits
    ws.wrap(&d.sock, &args, &[deps, ("JIG_T", "1")]);
    assert_eq!(take_log(&ws.log), log1("cached", "foo-abc"));
    // a changed value, an unset one, a set-but-empty one
    for (env, why) in [
        (vec![("JIG_T", "2")], "env-changed:JIG_T"),
        (vec![], "env-changed:JIG_T"),
        (vec![("OTHER", "")], "env-changed:OTHER"),
    ] {
        let mut e = vec![deps];
        e.extend(env);
        ws.wrap(&d.sock, &args, &e);
        assert_eq!(
            take_log(&ws.log),
            log1("compiled", &format!("foo-abc {why}")),
            "{why}"
        );
    }
    let log = d.take_log();
    let m = log
        .iter()
        .rev()
        .find(|r| r.verb == "PUT" && r.arg.starts_with(b"m/"))
        .unwrap();
    let text = String::from_utf8(m.value.clone().unwrap()).unwrap();
    assert!(text.contains("\n$env:JIG_T\tunset\n"), "{text}");
    assert!(text.ends_with(&format!(
        "$env:OTHER\t{}\n",
        String::from_utf8(env_id(Some(b""))).unwrap()
    )));
    // inputs are checked before env entries
    ws.wrap(&d.sock, &args, &[deps, ("OTHER", "")]);
    assert_eq!(take_log(&ws.log), log1("cached", "foo-abc"));
    std::fs::write(ws.root.join("src/m.rs"), "// changed\n").unwrap();
    ws.wrap(&d.sock, &args, &[deps, ("JIG_T", "9")]);
    assert_eq!(
        take_log(&ws.log),
        log1(
            "compiled",
            &format!("foo-abc inputs-changed:{}", ws.p("src/m.rs"))
        )
    );
}

/// ADOPTED Q-RS-3 with the real rustc: `env!` and `option_env!` dependencies.
#[test]
fn env_deps_with_real_rustc() {
    if !have_rustc() {
        eprintln!("no rustc on PATH: skipped");
        return;
    }
    let ws = Ws::new();
    std::fs::write(
        ws.root.join("src/lib.rs"),
        "pub const T: &str = env!(\"JIG_T\");\npub const U: Option<&str> = option_env!(\"JIG_U\");\n",
    )
    .unwrap();
    let d = Server::fake();
    let mut args = vec![OsString::from("rustcwrap"), OsString::from("rustc")];
    args.extend(ws.lib_args(&[]));
    let run = |env: &[(&str, &str)]| {
        let o = ws.jig(&d.sock, &args, env);
        assert_eq!(
            o.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&o.stderr)
        );
        take_log(&ws.log)
    };
    assert_eq!(run(&[("JIG_T", "a")]), log1("compiled", "foo-abc new-key"));
    let dep = std::fs::read_to_string(ws.out.join("foo-abc.d")).unwrap();
    assert!(dep.contains("# env-dep:JIG_T=a\n"), "{dep}");
    assert!(dep.contains("# env-dep:JIG_U\n"), "{dep}");
    let log = d.take_log();
    let k1 = k1_of(&log);
    let m = String::from_utf8(put_value(&log, &format!("m/{k1}")).to_vec()).unwrap();
    let t_line = String::from_utf8(env_line(b"JIG_T", Some(b"a"))).unwrap();
    assert!(m.lines().any(|l| l == t_line), "{m}");
    assert!(m.lines().any(|l| l == "$env:JIG_U\tunset"), "{m}");
    assert!(m.lines().all(|l| !l.ends_with("src/lib.rs")), "{m}");
    let rlib_a = std::fs::read(ws.out.join("libfoo-abc.rlib")).unwrap();

    ws.clear_out();
    assert_eq!(run(&[("JIG_T", "a")]), log1("cached", "foo-abc"));
    assert_eq!(
        std::fs::read(ws.out.join("libfoo-abc.rlib")).unwrap(),
        rlib_a
    );
    assert_eq!(
        run(&[("JIG_T", "b")]),
        log1("compiled", "foo-abc env-changed:JIG_T")
    );
    assert_ne!(
        std::fs::read(ws.out.join("libfoo-abc.rlib")).unwrap(),
        rlib_a
    );
    assert_eq!(
        run(&[("JIG_T", "b"), ("JIG_U", "x")]),
        log1("compiled", "foo-abc env-changed:JIG_U")
    );
    assert_eq!(
        run(&[("JIG_T", "b"), ("JIG_U", "x")]),
        log1("cached", "foo-abc")
    );
    // one manifest per k1: the latest store wins
    assert_eq!(
        run(&[("JIG_T", "a")]),
        log1("compiled", "foo-abc env-changed:JIG_T")
    );
}

/// The k1 fields that come from outside the argv: the five variables, the toolchain, the cwd.
#[test]
fn key_environment() {
    let ws = Ws::new();
    let d = Server::fake();
    let args = ws.lib_args(&[]);
    ws.wrap(&d.sock, &args, &[]);
    let k1 = k1_of(&d.take_log());
    let k1_with = |env: &[(&str, &str)]| {
        ws.wrap(&d.sock, &args, env);
        k1_of(&d.take_log())
    };
    for var in [
        "CARGO_PKG_NAME",
        "CARGO_PKG_VERSION",
        "CARGO_CFG_TARGET_FEATURE",
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
    ] {
        assert_ne!(k1_with(&[(var, "x")]), k1, "{var}");
        // set-but-empty is the same as unset here
        assert_eq!(k1_with(&[(var, "")]), k1, "{var}");
    }
    // anything else is not in k1
    assert_eq!(k1_with(&[("CARGO_PKG_AUTHORS", "x"), ("JIG_T", "1")]), k1);
    // Q-RS-6: a glued -L is in the key, a separate one is not
    let mut a = ws.lib_args(&["-L", "dependency=/x"]);
    ws.wrap(&d.sock, &a, &[]);
    assert_eq!(k1_of(&d.take_log()), k1);
    a.pop();
    a.pop();
    a.push(OsString::from("-Ldependency=/x"));
    ws.wrap(&d.sock, &a, &[]);
    assert_ne!(k1_of(&d.take_log()), k1);
    // the same rustc through a symlinked directory is the same tool (ToolId)
    symlink(ws.root.join("bin"), ws.root.join("bin2")).unwrap();
    let mut argv = vec![
        OsString::from("rustcwrap"),
        ws.root.join("bin2/rustc").into_os_string(),
    ];
    argv.extend_from_slice(&args);
    ws.jig(&d.sock, &argv, &[]);
    assert_eq!(k1_of(&d.take_log()), k1);
    // a bare name is resolved on PATH
    let mut argv = vec![OsString::from("rustcwrap"), OsString::from("rustc")];
    argv.extend_from_slice(&args);
    let path = format!(
        "{}:{}",
        ws.p("bin"),
        std::env::var("PATH").unwrap_or_default()
    );
    ws.jig(&d.sock, &argv, &[("PATH", &path)]);
    assert_eq!(k1_of(&d.take_log()), k1);
    take_log(&ws.log);
}

/// `fs::current_path()` failing (the cwd was removed) terminates the process before rustc runs,
/// as legacy's uncaught `filesystem_error` does.
#[test]
fn removed_cwd_aborts() {
    let ws = Ws::new();
    let d = Server::fake();
    let gone = ws.root.join("gone");
    std::fs::create_dir(&gone).unwrap();
    let mut args = ws.lib_args(&[]);
    args[3] = OsString::from(ws.p("src/lib.rs"));
    let mut script = format!(
        "cd '{0}' && rmdir '{0}' && exec '{JIG}' rustcwrap '{1}'",
        gone.display(),
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
        &d.sock,
        &ws.log,
        &[("FAKE_RUSTC_LOG", ws.rustc_log.as_os_str())],
    );
    assert_eq!(o.status.signal(), Some(SIGABRT), "{o:?}");
    assert_eq!(ws.rustc_runs(), 0);
    assert!(read_log(&ws.log).is_empty());
}

const SIGABRT: i32 = 6;

/// `cargo build` of a small workspace with `RUSTC_WRAPPER`, twice: the second build is all
/// hits for the library crates, and every output matches an uncached build.
#[test]
fn cargo_workspace_twice() {
    if !have_rustc() || Command::new("cargo").arg("--version").output().is_err() {
        eprintln!("no rustc or cargo on PATH: skipped");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let w = root.join("ws");
    let files = [
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"a\", \"b\", \"app\"]\nresolver = \"2\"\n",
        ),
        (
            "b/Cargo.toml",
            "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        ("b/src/lib.rs", "pub fn b() -> u32 {\n    41\n}\n"),
        (
            "a/Cargo.toml",
            "[package]\nname = \"a\"\nversion = \"0.2.0\"\nedition = \"2021\"\n\n[dependencies]\nb = { path = \"../b\" }\n",
        ),
        (
            "a/src/lib.rs",
            "mod inner;\npub fn a() -> u32 {\n    b::b() + inner::ONE\n}\npub const NAME: &str = env!(\"CARGO_PKG_NAME\");\n",
        ),
        ("a/src/inner.rs", "pub const ONE: u32 = 1;\n"),
        (
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\na = { path = \"../a\" }\n",
        ),
        (
            "app/src/main.rs",
            "fn main() {\n    println!(\"{} {}\", a::a(), a::NAME);\n}\n",
        ),
    ];
    for (rel, text) in files {
        let p = w.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    let wrapper = root.join("rustcwrap");
    symlink(JIG, &wrapper).unwrap();
    let d = Server::fake();
    let log = root.join("jig.log");
    let build = |wrap: bool| {
        let _ = std::fs::remove_dir_all(w.join("target"));
        let mut cmd = Command::new("cargo");
        cmd.args(["build", "--offline", "--quiet"])
            .current_dir(&w)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &root)
            .env("CARGO_HOME", root.join("cargo-home"))
            .env("CARGO_INCREMENTAL", "0")
            .env("CARGO_TERM_COLOR", "never")
            .env("JIG_SOCK", &d.sock)
            .env("JIG_LOG", &log);
        if wrap {
            cmd.env("RUSTC_WRAPPER", &wrapper);
        }
        let o = cmd.output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let mut snap = snapshot(&w.join("target/debug/deps"));
        let app = std::fs::read(w.join("target/debug/app")).unwrap();
        snap.insert("app".into(), (app, 0));
        snap
    };
    let plain = build(false);
    assert!(read_log(&log).is_empty());
    assert!(
        plain
            .keys()
            .any(|k| k.starts_with("liba-") && Path::new(k).extension() == Some(OsStr::new("rlib")))
    );

    let first = build(true);
    let l1 = take_log(&log);
    let libs = |l: &[(String, String)], outcome: &str| {
        l.iter()
            .filter(|(o, s)| o == outcome && (s.starts_with("a-") || s.starts_with("b-")))
            .count()
    };
    assert_eq!(libs(&l1, "compiled"), 2, "{l1:?}");
    assert!(
        l1.iter()
            .all(|(o, _)| ["compiled", "linked", "query"].contains(&o.as_str())),
        "{l1:?}"
    );
    assert_eq!(first, plain);

    let second = build(true);
    let l2 = take_log(&log);
    assert_eq!(libs(&l2, "cached"), 2, "{l2:?}");
    // nothing but hits for what is cacheable; the binary links, cargo's probes are queries
    assert!(
        l2.iter()
            .all(|(o, s)| o == "cached" || (o == "linked" && s.starts_with("app")) || o == "query"),
        "{l2:?}"
    );
    assert_eq!(second, plain);
    let out = Command::new(w.join("target/debug/app")).output().unwrap();
    assert_eq!(out.stdout, b"42 a\n");
}
