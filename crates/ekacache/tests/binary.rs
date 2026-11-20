//! The `ec` binary end to end: dispatch, usage, launcher, signal disposition
//! (`behaviors/ec-dispatch.md`, `implementation/05-ec-foundation.md §Tests`).
#![allow(
    clippy::unwrap_used,
    clippy::many_single_char_names,
    clippy::items_after_statements
)]

use ekacache_testkit::busy::CommandBusyExt as _;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Output};

const EC: &str = env!("CARGO_BIN_EXE_ec");

fn ec(args: &[&str]) -> Output {
    Command::new(EC).args(args).output_busy().unwrap()
}

#[test]
fn usage() {
    let o = ec(&[]);
    assert_eq!(o.status.code(), Some(2));
    assert!(o.stdout.is_empty());
    assert_eq!(o.stderr, ekacache::dispatch::USAGE);
    for flag in ["--help", "-h"] {
        let o = ec(&[flag]);
        assert_eq!(o.status.code(), Some(0));
        assert_eq!(o.stdout, ekacache::dispatch::USAGE);
        assert!(o.stderr.is_empty());
    }
    // only as the first argument
    let o = ec(&["x", "--help"]);
    assert_ne!(o.stdout, ekacache::dispatch::USAGE);
}

/// `ec` is the binary itself: usage with no arguments, `<name> <mode>` dispatch.
#[test]
fn self_name() {
    let dir = tempfile::tempdir().unwrap();
    let link = dir.path().join("ec");
    std::os::unix::fs::symlink(EC, &link).unwrap();
    let o = Command::new(&link).output_busy().unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(o.stderr, ekacache::dispatch::USAGE);
    let o = Command::new(&link)
        .args(["cache", "get"])
        .output_busy()
        .unwrap();
    assert_ne!(o.stderr, ekacache::dispatch::USAGE);
}

#[test]
fn launcher_exec() {
    let o = ec(&["/bin/sh", "-c", "echo \"$0 $1\"; exit 7", "zero", "one"]);
    assert_eq!(o.status.code(), Some(7));
    assert_eq!(o.stdout, b"zero one\n");
    let o = ec(&["/nonexistent/prog", "x"]);
    assert_eq!(o.status.code(), Some(127));
    assert_eq!(
        o.stderr,
        b"ec: cannot execute /nonexistent/prog: No such file or directory\n"
    );
    // no PATH search in launcher mode: `./sh` relative to the cwd
    let dir = tempfile::tempdir().unwrap();
    let o = Command::new(EC)
        .args(["./sh", "-c", "true"])
        .current_dir(dir.path())
        .output_busy()
        .unwrap();
    assert_eq!(o.status.code(), Some(127));
    assert_eq!(
        o.stderr,
        b"ec: cannot execute ./sh: No such file or directory\n"
    );
}

#[test]
fn launcher_go_sets_gocacheprog() {
    let dir = tempfile::tempdir().unwrap();
    let go = dir.path().join("go");
    std::fs::write(&go, "#!/bin/sh\necho \"${GOCACHEPROG-unset}\"\n").unwrap();
    std::fs::set_permissions(&go, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let sock = dir.path().join("sock");
    let go = go.to_str().unwrap();
    let run = |sock: &std::path::Path, preset: Option<&str>| {
        let mut c = Command::new(EC);
        c.arg(go)
            .env("EKACACHE_SOCK", sock)
            .env_remove("GOCACHEPROG");
        if let Some(p) = preset {
            c.env("GOCACHEPROG", p);
        }
        String::from_utf8(c.output_busy().unwrap().stdout).unwrap()
    };
    // no socket file: untouched
    assert_eq!(run(&sock, None), "unset\n");
    std::fs::write(&sock, "").unwrap();
    assert_eq!(run(&sock, None), format!("{EC} gocacheprog\n"));
    assert_eq!(run(&sock, Some("")), format!("{EC} gocacheprog\n"));
    assert_eq!(run(&sock, Some("mine")), "mine\n");
    // `go-1.22` strips to `go`, `gofmt` does not
    let go2 = dir.path().join("go-1.22");
    std::fs::copy(go, &go2).unwrap();
    let o = Command::new(EC)
        .arg(&go2)
        .env("EKACACHE_SOCK", &sock)
        .env_remove("GOCACHEPROG")
        .output_busy()
        .unwrap();
    assert_eq!(
        String::from_utf8(o.stdout).unwrap(),
        format!("{EC} gocacheprog\n")
    );
}

#[test]
fn self_path_is_absolute_not_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let link = dir.path().join("ec");
    std::os::unix::fs::symlink(EC, &link).unwrap();
    let go = dir.path().join("go");
    std::fs::write(&go, "#!/bin/sh\necho \"$GOCACHEPROG\"\n").unwrap();
    std::fs::set_permissions(&go, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::fs::write(dir.path().join("sock"), "").unwrap();
    let o = Command::new("./ec")
        .args(["./go"])
        .current_dir(dir.path())
        .env("EKACACHE_SOCK", "sock")
        .env_remove("GOCACHEPROG")
        .output_busy()
        .unwrap();
    let cwd = dir.path().to_str().unwrap();
    assert_eq!(
        String::from_utf8(o.stdout).unwrap(),
        format!("{cwd}/./ec gocacheprog\n")
    );
    // found through PATH: /proc/self/exe
    let o = Command::new("ec")
        .args(["./go"])
        .current_dir(dir.path())
        .env("PATH", cwd)
        .env("EKACACHE_SOCK", "sock")
        .env_remove("GOCACHEPROG")
        .output_busy()
        .unwrap();
    // without procfs (macOS) the readlink fails and `self` is "" (behaviors/ec-dispatch.md)
    let exe = if cfg!(target_os = "linux") {
        std::fs::canonicalize(EC).unwrap().display().to_string()
    } else {
        String::new()
    };
    assert_eq!(
        String::from_utf8(o.stdout).unwrap(),
        format!("{exe} gocacheprog\n")
    );
}

#[test]
fn sigpipe_disposition_passes_through() {
    // std::process::Command starts ec with SIGPIPE at SIG_DFL; ec must not ignore it
    let script = "kill -PIPE $$; echo alive";
    let o = ec(&["/bin/sh", "-c", script]);
    assert_eq!(o.status.signal(), Some(libc::SIGPIPE));
    assert!(o.stdout.is_empty());
    // started with SIGPIPE ignored, the program inherits that
    let o = Command::new("/bin/sh")
        .args([
            "-c",
            "trap '' PIPE; exec \"$0\" /bin/sh -c \"$1\"",
            EC,
            script,
        ])
        .output_busy()
        .unwrap();
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(o.stdout, b"alive\n");
}

#[test]
fn mode_names_from_argv0() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["ec-0.1", "ec.real", "jig"] {
        let link = dir.path().join(name);
        std::os::unix::fs::symlink(EC, &link).unwrap();
        // Q-DISPATCH-1: only the exact basename `ec` looks at argv[1]; these are cc mode (`jig` is
        // not a self name any more)
        let o = Command::new(&link).arg("--help").output_busy().unwrap();
        assert_ne!(o.stdout, ekacache::dispatch::USAGE);
    }
}
