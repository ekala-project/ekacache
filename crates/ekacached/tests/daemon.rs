//! The `ecd` binary (`behaviors/ecd-daemon.md` §Command line, §Environment, §Startup order,
//! §Signals and shutdown, §Exit codes): usage, the adopted strict budgets (Q-DAEMON-1), the
//! tier log lines, socket setup, and the clean exit on SIGTERM.

// helpers outside #[test] fns: a panic is the failure report
#![allow(clippy::unwrap_used)]

mod common;

use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

use common::{Daemon, log_lines, run, rust_ecd};
use ekacached::daemon::{USAGE, num_cpu, parse_budget};

fn b(p: &Path) -> Vec<u8> {
    p.as_os_str().as_encoded_bytes().to_vec()
}

#[test]
fn usage_and_arguments() {
    let bin = rust_ecd();
    for arg in [&b"--help"[..], b"-h"] {
        let r = run(&bin, &[arg], &[]);
        assert_eq!(r.code, Some(0));
        assert_eq!(r.stdout, USAGE.as_bytes());
        assert!(r.stderr.is_empty());
    }
    for args in [&[][..], &[&b"a"[..], b"b"], &[b"--help", b"x"]] {
        let r = run(&bin, args, &[]);
        assert_eq!(r.code, Some(2), "{args:?}");
        assert!(r.stdout.is_empty());
        assert_eq!(r.stderr, USAGE.as_bytes());
    }
}

#[test]
fn budget_syntax() {
    const G: i64 = 1 << 30;
    for (v, want) in [
        ("0", Some(0)),
        ("1", Some(G)),
        ("50", Some(50 * G)),
        ("007", Some(7 * G)),
        ("10M", Some(10 << 20)),
        ("10m", Some(10 << 20)),
        ("3g", Some(3 * G)),
        ("2T", Some(2 << 40)),
        ("2t", Some(2 << 40)),
        ("0T", Some(0)),
        ("8589934591", Some(8_589_934_591 * G)),
        ("8589934592", None),
        ("8388607T", Some(8_388_607 << 40)),
        ("8388608T", None),
        ("99999999999999999999M", None),
        ("", None),
        ("M", None),
        ("+1", None),
        ("-1", None),
        (" 1", None),
        ("1 ", None),
        ("1.5", None),
        ("1K", None),
        ("1GB", None),
        ("1MG", None),
        ("0x10", None),
        ("１", None),
    ] {
        assert_eq!(parse_budget(v.as_bytes()), want, "{v:?}");
    }
}

/// Q-DAEMON-1 (adopted): an invalid budget exits 2 before anything is opened.
#[test]
fn invalid_budget_exits_2() {
    let bin = rust_ecd();
    let root = tempfile::tempdir().unwrap();
    let sock = b(&root.path().join("sock"));
    let cache = b(&root.path().join("cache"));
    let cold = b(&root.path().join("cold"));
    for (env, want) in [
        (
            vec![("EKACACHED_SIZE", &b"1.5"[..])],
            &b"ecd: invalid EKACACHED_SIZE \"1.5\": want <n>[M|G|T], 0 = unbounded\n"[..],
        ),
        (
            vec![("EKACACHED_SIZE", b"8589934592")],
            b"ecd: invalid EKACACHED_SIZE \"8589934592\": want <n>[M|G|T], 0 = unbounded\n",
        ),
        (
            vec![("EKACACHED_COLD", &cold), ("EKACACHED_COLD_SIZE", b"-1")],
            b"ecd: invalid EKACACHED_COLD_SIZE \"-1\": want <n>[M|G|T], 0 = unbounded\n",
        ),
        (
            vec![("EKACACHED_SIZE", b"\xff\n")],
            b"ecd: invalid EKACACHED_SIZE \"\xff\n\": want <n>[M|G|T], 0 = unbounded\n",
        ),
    ] {
        let mut env = env;
        env.push(("XDG_CACHE_HOME", &cache));
        let r = run(&bin, &[&sock], &env);
        assert_eq!(r.code, Some(2), "{env:?}");
        assert_eq!(
            String::from_utf8_lossy(&r.stderr),
            String::from_utf8_lossy(want)
        );
        assert!(!root.path().join("cache").exists());
        assert!(!root.path().join("sock").exists());
    }
}

/// The budgets as the daemon logs them after opening the store; `EKACACHED_COLD_SIZE` is not read
/// without `EKACACHED_COLD`.
#[test]
fn tier_log_lines() {
    let bin = rust_ecd();
    let slots = num_cpu();
    for (env, want) in [
        (
            vec![],
            vec!["tier 0 $ROOT/cache/ekacache/packs budget 53687091200"],
        ),
        (
            vec![("EKACACHED_SIZE", ""), ("EKACACHED_COLD_SIZE", "x")],
            vec!["tier 0 $ROOT/cache/ekacache/packs budget 53687091200"],
        ),
        (
            vec![("EKACACHED_SIZE", "10M")],
            vec!["tier 0 $ROOT/cache/ekacache/packs budget 10485760"],
        ),
        (
            vec![("EKACACHED_SIZE", "0"), ("EKACACHED_COLD", "$ROOT/cold")],
            vec![
                "tier 0 $ROOT/cache/ekacache/packs budget unbounded",
                "tier 1 $ROOT/cold budget unbounded",
            ],
        ),
        (
            vec![
                ("EKACACHED_COLD", "$ROOT/cold"),
                ("EKACACHED_COLD_SIZE", "2T"),
            ],
            vec![
                "tier 0 $ROOT/cache/ekacache/packs budget 53687091200",
                "tier 1 $ROOT/cold budget 2199023255552",
            ],
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let r = root.path().to_str().unwrap();
        let env: Vec<(&str, String)> = env
            .iter()
            .map(|(k, v)| (*k, v.replace("$ROOT", r)))
            .collect();
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let d = Daemon::start(&bin, root.path(), &env);
        let (status, log) = d.stop();
        assert!(status.success());
        let mut want: Vec<String> = want.iter().map(|s| (*s).to_owned()).collect();
        want.push(format!(
            "listening on $ROOT/sock, {slots} slots, keys=0 packs=1 bytes=0 live=0"
        ));
        assert_eq!(log_lines(&log, root.path()), want, "{env:?}");
    }
}

#[test]
fn home_is_required_without_xdg_cache_home() {
    let bin = rust_ecd();
    let root = tempfile::tempdir().unwrap();
    let sock = b(&root.path().join("sock"));
    for env in [&[][..], &[("HOME", &b""[..]), ("XDG_CACHE_HOME", b"")]] {
        let r = run(&bin, &[&sock], env);
        assert_eq!(r.code, Some(1));
        assert_eq!(log_lines(&r.stderr, root.path()), ["$HOME is not defined"]);
    }
    // HOME alone is enough: the cache goes to $HOME/.cache
    let home = root.path().to_str().unwrap();
    let d = Daemon::start(&bin, root.path(), &[("XDG_CACHE_HOME", ""), ("HOME", home)]);
    assert!(root.path().join(".cache/ekacache/packs").is_dir());
    let (status, log) = d.stop();
    assert!(status.success());
    let lines = log_lines(&log, root.path());
    assert_eq!(
        lines[0],
        "tier 0 $ROOT/.cache/ekacache/packs budget 53687091200"
    );
}

/// The socket: parent directories made, a stale file replaced, mode 0666; SIGTERM removes it,
/// seals the store and exits 0, even with a connection waiting in SLOT.
#[test]
fn socket_and_sigterm() {
    let bin = rust_ecd();
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let cache = cache.to_str().unwrap();
    // missing parents are made (0755)
    let fresh = root.path().join("c/d");
    let d = Daemon::start(&bin, &fresh, &[("XDG_CACHE_HOME", cache)]);
    let mode = std::fs::metadata(root.path().join("c"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o755 & !umask());
    assert!(d.stop().0.success());

    let nested = root.path().join("a/b");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("sock"), b"stale").unwrap();
    let d = Daemon::start(
        &bin,
        &nested,
        &[("EKACACHED_SLOTS", "1"), ("XDG_CACHE_HOME", cache)],
    );
    let meta = std::fs::metadata(&d.sock).unwrap();
    assert!(meta.file_type().is_socket());
    assert_eq!(meta.permissions().mode() & 0o7777, 0o666);
    let mut a = d.connect();
    a.put(b"k", b"v");
    a.send(b"SLOT b\n");
    assert_eq!(a.line(), b"OK\n");
    let mut w = d.connect();
    w.send(b"SLOT b\n");
    assert!(w.quiet());
    let sock = d.sock.clone();
    let t = Instant::now();
    let (status, log) = d.stop();
    assert!(status.success(), "{status:?}");
    assert!(t.elapsed() < Duration::from_secs(5));
    assert!(!sock.exists(), "socket left behind");
    // the first run's pack was empty and deleted at close (adopted Q-STORE-2); this one sealed
    assert!(
        root.path()
            .join("cache/ekacache/packs/000001.hint")
            .exists(),
        "store not sealed"
    );
    let lines = log_lines(&log, root.path());
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(
        lines[1],
        "listening on $ROOT/a/b/sock, 1 slots, keys=0 packs=1 bytes=0 live=0"
    );
}

/// The process umask (the daemon inherits it): the bits it clears from a file created 0777.
fn umask() -> u32 {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = tempfile::tempdir().unwrap();
    let probe = dir.path().join("umask");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o777)
        .open(&probe)
        .unwrap();
    !std::fs::metadata(&probe).unwrap().permissions().mode() & 0o777
}

/// Q-DAEMON-10: `@name` binds an abstract socket, and the chmod of the path then fails. Linux
/// only: on macOS `@name` is an ordinary path.
#[cfg(target_os = "linux")]
#[test]
fn abstract_socket_fails_at_chmod() {
    let bin = rust_ecd();
    let root = tempfile::tempdir().unwrap();
    let cache = b(&root.path().join("cache"));
    let name = format!("@ecd-test-{}", std::process::id());
    let r = run(&bin, &[name.as_bytes()], &[("XDG_CACHE_HOME", &cache)]);
    assert_eq!(r.code, Some(1));
    let lines = log_lines(&r.stderr, root.path());
    assert_eq!(
        lines.last().unwrap(),
        &format!("chmod {name}: no such file or directory")
    );
}

/// Socket setup failures are fatal with Go's error text.
#[test]
fn socket_errors() {
    let bin = rust_ecd();
    let root = tempfile::tempdir().unwrap();
    let cache = b(&root.path().join("cache"));
    let long = root.path().join("x".repeat(120));
    std::fs::write(root.path().join("file"), b"").unwrap();
    let under_file = root.path().join("file/sock");
    for (sock, want) in [
        (
            b(&long),
            format!(
                "listen unix $ROOT/{}: bind: invalid argument",
                "x".repeat(120)
            ),
        ),
        (
            b(&under_file),
            "mkdir $ROOT/file: not a directory".to_owned(),
        ),
    ] {
        let r = run(&bin, &[&sock], &[("XDG_CACHE_HOME", &cache)]);
        assert_eq!(r.code, Some(1));
        assert_eq!(log_lines(&r.stderr, root.path()).last().unwrap(), &want);
    }
}

/// `EKACACHED_SLOTS`: a positive integer, anything else the CPU count (Q-DAEMON-13).
#[test]
fn slot_count() {
    let bin = rust_ecd();
    let cpus = num_cpu();
    for (v, want) in [
        ("3", 3),
        ("+3", 3),
        ("0", cpus),
        ("-2", cpus),
        ("x", cpus),
        (" 3", cpus),
    ] {
        let root = tempfile::tempdir().unwrap();
        let d = Daemon::start(&bin, root.path(), &[("EKACACHED_SLOTS", v)]);
        let (_, log) = d.stop();
        let lines = log_lines(&log, root.path());
        assert!(
            lines.last().unwrap().contains(&format!(", {want} slots, ")),
            "{v:?}: {lines:?}"
        );
    }
}
