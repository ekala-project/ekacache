//! The `jig` binary end to end: dispatch, usage, launcher, signal disposition
//! (`behaviors/jig-dispatch.md`, `implementation/05-jig-foundation.md §Tests`).
#![allow(clippy::unwrap_used, clippy::many_single_char_names, clippy::items_after_statements)]

use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Output};

const JIG: &str = env!("CARGO_BIN_EXE_jig");

fn jig(args: &[&str]) -> Output {
    Command::new(JIG).args(args).output().unwrap()
}

#[test]
fn usage() {
    let o = jig(&[]);
    assert_eq!(o.status.code(), Some(2));
    assert!(o.stdout.is_empty());
    assert_eq!(o.stderr, jig::dispatch::USAGE);
    for flag in ["--help", "-h"] {
        let o = jig(&[flag]);
        assert_eq!(o.status.code(), Some(0));
        assert_eq!(o.stdout, jig::dispatch::USAGE);
        assert!(o.stderr.is_empty());
    }
    // only as the first argument
    let o = jig(&["x", "--help"]);
    assert_ne!(o.stdout, jig::dispatch::USAGE);
}

#[test]
fn launcher_exec() {
    let o = jig(&["/bin/sh", "-c", "echo \"$0 $1\"; exit 7", "zero", "one"]);
    assert_eq!(o.status.code(), Some(7));
    assert_eq!(o.stdout, b"zero one\n");
    let o = jig(&["/nonexistent/prog", "x"]);
    assert_eq!(o.status.code(), Some(127));
    assert_eq!(
        o.stderr,
        b"jig: cannot execute /nonexistent/prog: No such file or directory\n"
    );
    // no PATH search in launcher mode: `./sh` relative to the cwd
    let dir = tempfile::tempdir().unwrap();
    let o = Command::new(JIG)
        .args(["./sh", "-c", "true"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(127));
    assert_eq!(
        o.stderr,
        b"jig: cannot execute ./sh: No such file or directory\n"
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
        let mut c = Command::new(JIG);
        c.arg(go).env("JIG_SOCK", sock).env_remove("GOCACHEPROG");
        if let Some(p) = preset {
            c.env("GOCACHEPROG", p);
        }
        String::from_utf8(c.output().unwrap().stdout).unwrap()
    };
    // no socket file: untouched
    assert_eq!(run(&sock, None), "unset\n");
    std::fs::write(&sock, "").unwrap();
    assert_eq!(run(&sock, None), format!("{JIG} gocacheprog\n"));
    assert_eq!(run(&sock, Some("")), format!("{JIG} gocacheprog\n"));
    assert_eq!(run(&sock, Some("mine")), "mine\n");
    // `go-1.22` strips to `go`, `gofmt` does not
    let go2 = dir.path().join("go-1.22");
    std::fs::copy(go, &go2).unwrap();
    let o = Command::new(JIG)
        .arg(&go2)
        .env("JIG_SOCK", &sock)
        .env_remove("GOCACHEPROG")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(o.stdout).unwrap(),
        format!("{JIG} gocacheprog\n")
    );
}

#[test]
fn self_path_is_absolute_not_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let link = dir.path().join("jig");
    std::os::unix::fs::symlink(JIG, &link).unwrap();
    let go = dir.path().join("go");
    std::fs::write(&go, "#!/bin/sh\necho \"$GOCACHEPROG\"\n").unwrap();
    std::fs::set_permissions(&go, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::fs::write(dir.path().join("sock"), "").unwrap();
    let o = Command::new("./jig")
        .args(["./go"])
        .current_dir(dir.path())
        .env("JIG_SOCK", "sock")
        .env_remove("GOCACHEPROG")
        .output()
        .unwrap();
    let cwd = dir.path().to_str().unwrap();
    assert_eq!(
        String::from_utf8(o.stdout).unwrap(),
        format!("{cwd}/./jig gocacheprog\n")
    );
    // found through PATH: /proc/self/exe
    let o = Command::new("jig")
        .args(["./go"])
        .current_dir(dir.path())
        .env("PATH", cwd)
        .env("JIG_SOCK", "sock")
        .env_remove("GOCACHEPROG")
        .output()
        .unwrap();
    let exe = std::fs::canonicalize(JIG).unwrap();
    assert_eq!(
        String::from_utf8(o.stdout).unwrap(),
        format!("{} gocacheprog\n", exe.display())
    );
}

#[test]
fn sigpipe_disposition_passes_through() {
    // std::process::Command starts jig with SIGPIPE at SIG_DFL; jig must not ignore it
    let script = "kill -PIPE $$; echo alive";
    let o = jig(&["/bin/sh", "-c", script]);
    assert_eq!(o.status.signal(), Some(libc::SIGPIPE));
    assert!(o.stdout.is_empty());
    // started with SIGPIPE ignored, the program inherits that
    let o = Command::new("/bin/sh")
        .args([
            "-c",
            "trap '' PIPE; exec \"$0\" /bin/sh -c \"$1\"",
            JIG,
            script,
        ])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(o.stdout, b"alive\n");
}

#[test]
fn mode_names_from_argv0() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["jig-0.1", "jig.real"] {
        let link = dir.path().join(name);
        std::os::unix::fs::symlink(JIG, &link).unwrap();
        // Q-DISPATCH-1: only the exact basename `jig` looks at argv[1]; these are cc mode
        let o = Command::new(&link).arg("--help").output().unwrap();
        assert_ne!(o.stdout, jig::dispatch::USAGE);
    }
}
