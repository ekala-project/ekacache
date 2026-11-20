//! `ec cache …` and `ec slot …` end to end (`behaviors/ec-cache-commands.md`,
//! `implementation/08-ec-gocache-and-cache-cmds.md §Tests`).
//!
//! Tests that need a daemon use `$EC_RUST_ECD` and are skipped without it.
#![allow(clippy::unwrap_used, clippy::too_many_lines)]

mod common;

use ekacache_testkit::busy::CommandBusyExt as _;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Output};

use common::{Daemon, EC, daemon};

/// A store path whose hash `MaskHashes` recognises.
const STORE_REF: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-foo-1.0";

/// Run `bin` with a clean environment: `PATH`, `EKACACHE_SOCK` and `extra`.
fn run_with(bin: &OsStr, sock: &Path, args: &[&OsStr], extra: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("EKACACHE_SOCK", sock);
    for (k, v) in extra {
        cmd.env(k, v);
    }
    cmd.output_busy().unwrap()
}

fn ec(sock: &Path, args: &[&str]) -> Output {
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    run_with(OsStr::new(EC), sock, &args, &[])
}

fn cache(
    bin: &OsStr,
    sock: &Path,
    verb: &str,
    key: &str,
    path: &Path,
    env: &[(&str, &str)],
) -> Output {
    run_with(
        bin,
        sock,
        &[
            OsStr::new("cache"),
            OsStr::new(verb),
            OsStr::new(key),
            path.as_os_str(),
        ],
        env,
    )
}

fn rust() -> &'static OsStr {
    OsStr::new(EC)
}

fn need_daemon() -> Option<Daemon> {
    let d = daemon("EC_RUST_ECD");
    if d.is_none() {
        eprintln!("EC_RUST_ECD not set: skipped");
    }
    d
}

#[test]
fn argument_count_and_no_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let nosock = dir.path().join("nosock");
    // Q-DIR-6 kept: a wrong argument count is a silent 1, checked before connecting
    for args in [
        &["cache"][..],
        &["cache", "get", "k"],
        &["cache", "put", "k", "f", "x"],
    ] {
        let o = ec(&nosock, args);
        assert_eq!(o.status.code(), Some(1), "{args:?}");
        assert!(o.stdout.is_empty() && o.stderr.is_empty());
    }
    // no daemon: 2, before the file or directory is touched
    for verb in ["get", "put", "typo", "put-dir", "get-dir"] {
        let target = dir.path().join("absent");
        let o = ec(&nosock, &["cache", verb, "k", target.to_str().unwrap()]);
        assert_eq!(o.status.code(), Some(2), "{verb}");
        assert!(o.stdout.is_empty() && o.stderr.is_empty());
        assert!(!target.exists());
    }
}

#[test]
fn slot_without_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let nosock = dir.path().join("nosock");
    let o = ec(&nosock, &["slot"]);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(o.stderr, b"usage: ec slot <program> [args...]\n");
    // no daemon: run without a slot; the status passes through, PATH is searched
    let o = ec(
        &nosock,
        &["slot", "sh", "-c", "echo \"[$EKACACHE_SLOT]\"; exit 3"],
    );
    assert_eq!(o.status.code(), Some(3));
    assert_eq!(o.stdout, b"[]\n");
    // Q-DIR-7 kept: exec failure is a silent 127
    let o = ec(&nosock, &["slot", "/nonexistent/prog"]);
    assert_eq!(o.status.code(), Some(127));
    assert!(o.stderr.is_empty());
    let o = ec(&nosock, &["slot", "sh", "-c", "kill -TERM $$"]);
    assert_eq!(o.status.code(), Some(128 + 15));
}

#[test]
fn slot_with_daemon() {
    let Some(d) = need_daemon() else { return };
    let o = ec(
        &d.sock,
        &["slot", "sh", "-c", "echo \"[$EKACACHE_SLOT]\"; exit 4"],
    );
    assert_eq!(o.status.code(), Some(4));
    assert_eq!(o.stdout, b"[1]\n");
    // a parent's slot is reused: no SLOT request, EKACACHE_SLOT passes through
    let o = run_with(
        rust(),
        &d.sock,
        &[
            OsStr::new("slot"),
            OsStr::new("sh"),
            OsStr::new("-c"),
            OsStr::new("echo $EKACACHE_SLOT"),
        ],
        &[("EKACACHE_SLOT", "x")],
    );
    assert_eq!(o.stdout, b"x\n");
}

#[test]
fn blobs() {
    let Some(d) = need_daemon() else { return };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 253) as u8).collect();
    std::fs::write(&src, &data).unwrap();
    let o = cache(rust(), &d.sock, "put", "user/key", &src, &[]);
    assert_eq!((o.status.code(), o.stderr.as_slice()), (Some(0), &b""[..]));
    // any verb but put/put-dir/get-dir is get (Q-DIR-6 kept); `-` is a file name
    for (verb, name) in [("get", "out"), ("fetch", "-")] {
        let dst = dir.path().join(name);
        let o = Command::new(EC)
            .args(["cache", verb, "user/key", name])
            .current_dir(dir.path())
            .env("EKACACHE_SOCK", &d.sock)
            .output_busy()
            .unwrap();
        assert_eq!(o.status.code(), Some(0), "{verb}");
        assert!(o.stdout.is_empty() && o.stderr.is_empty());
        assert_eq!(std::fs::read(&dst).unwrap(), data);
    }
    // a miss, an unwritable destination, an unreadable source
    let o = cache(
        rust(),
        &d.sock,
        "get",
        "user/none",
        &dir.path().join("x"),
        &[],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(!dir.path().join("x").exists());
    let o = cache(
        rust(),
        &d.sock,
        "get",
        "user/key",
        Path::new("/nonexistent/x"),
        &[],
    );
    assert_eq!(o.status.code(), Some(1));
    let o = cache(
        rust(),
        &d.sock,
        "put",
        "user/k2",
        Path::new("/nonexistent/x"),
        &[],
    );
    assert_eq!((o.status.code(), o.stderr.as_slice()), (Some(1), &b""[..]));
    // ADOPTED Q-CLIENT-2: a PUT the daemon refuses (key over 4096 bytes) exits 1 with a message
    let long = "k".repeat(5000);
    let o = cache(rust(), &d.sock, "put", &long, &src, &[]);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        o.stderr,
        format!("ec: cache put {long} failed\n").into_bytes()
    );
}

/// Every file and symlink under `root` (directories are implied): kind, mode bits, content or
/// target.
fn snapshot(root: &Path) -> BTreeMap<String, (char, u32, Vec<u8>)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            let rel = p.strip_prefix(root).unwrap().to_str().unwrap().to_owned();
            let m = std::fs::symlink_metadata(&p).unwrap();
            if m.file_type().is_symlink() {
                let t = std::fs::read_link(&p).unwrap();
                out.insert(rel, ('l', 0, t.into_os_string().into_encoded_bytes()));
            } else if m.is_dir() {
                stack.push(p);
            } else if m.is_file() {
                let data = std::fs::read(&p).unwrap_or_default();
                out.insert(rel, ('f', m.mode() & 0o7777, data));
            }
        }
    }
    out
}

fn chmod(p: &Path, mode: u32) {
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// A tree with binaries, text naming store paths, duplicates, symlinks (dangling, to a
/// directory), an empty file, a setuid file, an unreadable file, an empty directory, a fifo
/// when `mkfifo` exists, and more than one batch of files.
fn make_tree(root: &Path) -> bool {
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::create_dir_all(root.join("share/doc")).unwrap();
    std::fs::create_dir_all(root.join("many/deeper")).unwrap();
    std::fs::create_dir_all(root.join("emptydir")).unwrap();
    std::fs::write(root.join("bin/tool"), b"\x7fELF\0\x01binary\0").unwrap();
    chmod(&root.join("bin/tool"), 0o755);
    std::fs::write(
        root.join("share/a.txt"),
        format!("see {STORE_REF}/bin/foo\n"),
    )
    .unwrap();
    std::fs::write(root.join("share/doc/dup1"), "same\n").unwrap();
    std::fs::write(root.join("share/doc/dup2"), "same\n").unwrap();
    std::fs::write(root.join("empty"), "").unwrap();
    chmod(&root.join("empty"), 0o600);
    std::fs::write(root.join("bin/suid"), "#!/bin/sh\n").unwrap();
    chmod(&root.join("bin/suid"), 0o4755);
    std::fs::write(root.join("secret"), "hidden").unwrap();
    chmod(&root.join("secret"), 0o000);
    symlink(format!("{STORE_REF}/lib"), root.join("lib")).unwrap();
    symlink("share", root.join("link_dir")).unwrap();
    for i in 0..300 {
        let sub = if i % 2 == 0 { "many" } else { "many/deeper" };
        std::fs::write(root.join(format!("{sub}/f{i:03}")), format!("file {i}\n")).unwrap();
    }
    let _ = Command::new("mkfifo").arg(root.join("fifo")).status_busy();
    // root can read a mode-000 file
    std::fs::File::open(root.join("secret")).is_ok()
}

fn stderr_lines(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn tree_round_trip() {
    let Some(d) = need_daemon() else { return };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let secret_readable = make_tree(&src);
    let o = cache(rust(), &d.sock, "put-dir", "tree/1", &src, &[]);
    assert_eq!(o.status.code(), Some(0));
    // 300 + tool + suid + a.txt + dup1 + dup2 + empty (+ secret)
    let files = 306 + usize::from(secret_readable);
    // the duplicates upload twice, unless a batch boundary falls between them
    let first = stderr_lines(&o);
    let head = format!("ec: put-dir tree/1: {files} files, ");
    let new: usize = first
        .strip_prefix(&head)
        .unwrap()
        .strip_suffix(" new\n")
        .unwrap()
        .parse()
        .unwrap();
    assert!(new == files || new + 1 == files, "{first}");
    // again: everything is there
    let o = cache(rust(), &d.sock, "put-dir", "tree/1", &src, &[]);
    assert_eq!(
        stderr_lines(&o),
        format!("ec: put-dir tree/1: {files} files, 0 new\n")
    );
    // the listing: modes without setuid, symlinks verbatim, no directories
    let lst = dir.path().join("listing");
    assert_eq!(
        cache(rust(), &d.sock, "get", "tree/1", &lst, &[])
            .status
            .code(),
        Some(0)
    );
    let listing = String::from_utf8(std::fs::read(&lst).unwrap()).unwrap();
    assert!(listing.contains(&format!("l 0 {STORE_REF}/lib\tlib\n")));
    assert!(listing.contains("l 0 share\tlink_dir\n"));
    assert!(
        listing
            .lines()
            .any(|l| l.starts_with("f 755 ") && l.ends_with("\tbin/suid"))
    );
    assert!(
        listing
            .lines()
            .any(|l| l.starts_with("f 600 ") && l.ends_with("\tempty"))
    );
    assert!(!listing.contains("emptydir") && !listing.contains("fifo"));
    assert!(!listing.contains("link_dir/"));
    assert_eq!(listing.lines().count(), files + 2);

    let dst = dir.path().join("dst");
    let o = cache(rust(), &d.sock, "get-dir", "tree/1", &dst, &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr_lines(&o));
    assert_eq!(
        stderr_lines(&o),
        format!("ec: get-dir tree/1: {files} files, 0 missing\n")
    );
    let mut want = snapshot(&src);
    want.remove("fifo");
    if !secret_readable {
        want.remove("secret");
    }
    want.get_mut("bin/suid").unwrap().1 = 0o755;
    // ADOPTED Q-DIR-1 / Q-KEY-1: path mode stores and restores text unchanged
    assert_eq!(snapshot(&dst), want);
    assert!(!dst.join("emptydir").exists());

    // an empty tree: an empty listing; restoring it creates nothing, not even the directory
    let empty = dir.path().join("empty_src");
    std::fs::create_dir(&empty).unwrap();
    let o = cache(rust(), &d.sock, "put-dir", "tree/empty", &empty, &[]);
    assert_eq!(stderr_lines(&o), "ec: put-dir tree/empty: 0 files, 0 new\n");
    let nowhere = dir.path().join("nowhere");
    let o = cache(rust(), &d.sock, "get-dir", "tree/empty", &nowhere, &[]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(
        stderr_lines(&o),
        "ec: get-dir tree/empty: 0 files, 0 missing\n"
    );
    assert!(!nowhere.exists());
    // a missing listing: 1, nothing printed or created
    let o = cache(rust(), &d.sock, "get-dir", "tree/none", &nowhere, &[]);
    assert_eq!(o.status.code(), Some(1));
    assert!(o.stderr.is_empty() && !nowhere.exists());
}

#[test]
fn content_mode_masks_and_resolves() {
    let Some(d) = need_daemon() else { return };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("t"), format!("{STORE_REF}/bin\n")).unwrap();
    let content = [("EKACACHE_STORE_IDENTITY", "content")];
    let o = cache(rust(), &d.sock, "put-dir", "tree/c", &src, &content);
    assert_eq!(o.status.code(), Some(0));
    // restored without the root: stays masked; with it: mapped back to this build's root
    let other = "/nix/store/zyxwvsrqpnmlkjihgfdcba9876543210-foo-1.0";
    let a = dir.path().join("a");
    let o = cache(rust(), &d.sock, "get-dir", "tree/c", &a, &content);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(
        std::fs::read(a.join("t")).unwrap(),
        b"/nix/store/*-foo-1.0/bin\n"
    );
    let b = dir.path().join("b");
    let env = [
        ("EKACACHE_STORE_IDENTITY", "content"),
        ("EKACACHE_STORE_ROOTS", other),
    ];
    let o = cache(rust(), &d.sock, "get-dir", "tree/c", &b, &env);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(
        std::fs::read(b.join("t")).unwrap(),
        format!("{other}/bin\n").into_bytes()
    );
}

/// Put `blob` under its `b/<hash>` key through `put-dir` of a one-file tree and return the hash.
fn blob_hash(bin: &OsStr, d: &Daemon, dir: &Path, content: &[u8]) -> String {
    let t = tempfile::tempdir_in(dir).unwrap();
    std::fs::write(t.path().join("x"), content).unwrap();
    let key = format!("tmp/{}", t.path().file_name().unwrap().to_str().unwrap());
    assert_eq!(
        cache(bin, &d.sock, "put-dir", &key, t.path(), &[])
            .status
            .code(),
        Some(0)
    );
    let lst = t.path().join("lst");
    assert_eq!(
        cache(bin, &d.sock, "get", &key, &lst, &[]).status.code(),
        Some(0)
    );
    let l = String::from_utf8(std::fs::read(&lst).unwrap()).unwrap();
    l.split(' ')
        .nth(2)
        .unwrap()
        .split('\t')
        .next()
        .unwrap()
        .to_owned()
}

fn put_listing(bin: &OsStr, d: &Daemon, dir: &Path, key: &str, listing: &str) {
    let f = dir.join(format!("{}.listing", key.replace('/', "_")));
    std::fs::write(&f, listing).unwrap();
    assert_eq!(
        cache(bin, &d.sock, "put", key, &f, &[]).status.code(),
        Some(0)
    );
}

#[test]
fn get_dir_rolls_back_on_a_missing_blob() {
    let Some(d) = need_daemon() else { return };
    let dir = tempfile::tempdir().unwrap();
    let have = blob_hash(rust(), &d, dir.path(), b"present\n");
    let gone = "0".repeat(32);
    let listing = format!(
        "f 640 {have}\tsub/a\nl 0 target\tsub/link\nf 644 {gone}\tsub/b\nf 600 {have}\tpre\n"
    );
    put_listing(rust(), &d, dir.path(), "tree/partial", &listing);
    let dst = dir.path().join("dst");
    std::fs::create_dir(&dst).unwrap();
    std::fs::write(dst.join("pre"), "old").unwrap();
    std::fs::write(dst.join("other"), "untouched").unwrap();
    let o = cache(rust(), &d.sock, "get-dir", "tree/partial", &dst, &[]);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        stderr_lines(&o),
        "ec: get-dir tree/partial: 3 files, 1 missing\n"
    );
    // everything written by this run is removed (the overwritten `pre` too); directories stay
    let left = snapshot(&dst);
    assert_eq!(left.keys().collect::<Vec<_>>(), ["other"]);
    assert!(dst.join("sub").is_dir());
}

#[test]
fn get_dir_trusts_the_listing() {
    let Some(d) = need_daemon() else { return };
    let dir = tempfile::tempdir().unwrap();
    let have = blob_hash(rust(), &d, dir.path(), b"payload\n");
    // Q-DIR-5 kept: an absolute rel discards the root; `..` is followed; modes keep all 12
    // bits; a bad mode is 0644; malformed lines are skipped
    let abs = dir.path().join("elsewhere/abs");
    let listing = format!(
        "f 1750 {have}\t{}\nf 640 {have}\t../up\nf zz {have}\tbadmode\nnot a line\n\
         f 644\tshort\nx 644 {have}\tunknown-kind\n",
        abs.display()
    );
    put_listing(rust(), &d, dir.path(), "tree/trust", &listing);
    let dst = dir.path().join("dst");
    let o = cache(rust(), &d.sock, "get-dir", "tree/trust", &dst, &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr_lines(&o));
    assert_eq!(
        stderr_lines(&o),
        "ec: get-dir tree/trust: 3 files, 0 missing\n"
    );
    assert_eq!(std::fs::read(&abs).unwrap(), b"payload\n");
    assert_eq!(std::fs::metadata(&abs).unwrap().mode() & 0o7777, 0o1750);
    assert_eq!(std::fs::read(dir.path().join("up")).unwrap(), b"payload\n");
    let bad = std::fs::metadata(dst.join("badmode")).unwrap();
    assert_eq!(bad.mode() & 0o7777, 0o644);
    assert!(!dst.join("unknown-kind").exists());
}

#[test]
fn put_dir_of_a_missing_directory_aborts() {
    let Some(d) = need_daemon() else { return };
    let dir = tempfile::tempdir().unwrap();
    // Q-DIR-4 kept: SIGABRT after connecting, and the listing is never PUT
    let o = cache(
        rust(),
        &d.sock,
        "put-dir",
        "tree/abort",
        &dir.path().join("absent"),
        &[],
    );
    assert_eq!(o.status.signal(), Some(libc::SIGABRT));
    let o = cache(
        rust(),
        &d.sock,
        "get",
        "tree/abort",
        &dir.path().join("l"),
        &[],
    );
    assert_eq!(o.status.code(), Some(1));
}
