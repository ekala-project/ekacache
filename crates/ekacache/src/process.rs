//! Running the real compiler (`behaviors/jig-dispatch.md §Process execution`, `process.cc`), and
//! jig's own environment as its children see it (`behaviors/environment.md §Reading rules`).
//!
//! `run` is a direct port of `Run`: `pipe`, `fork`, `dup2`, `execvp`, `_exit(127)`, read the pipe
//! to EOF in 4096-byte reads, `waitpid`. It does not use `std::process::Command`, which differs
//! from legacy in three observable ways (`implementation/05-jig-foundation.md §Signal
//! disposition`):
//!
//! - it resets `SIGPIPE` to `SIG_DFL` in the child; legacy passes on the disposition jig
//!   inherited (jig is `#![no_main]`, so jig itself keeps it too);
//! - when any variable changes it rebuilds the child's environment sorted and de-duplicated
//!   (last duplicate wins); legacy `setenv`s its own environment, which keeps the order and lets
//!   the first duplicate win;
//! - it folds a failed `fork` and a failed `exec` into one `io::Error`; legacy reports
//!   `127` + `jig: fork failed\n` for the first and a silent `127` for the second (Q-DISPATCH-4,
//!   Q-DISPATCH-5).
//!
//! Between `fork` and `exec` the child calls only `dup2`, `execvp` and `_exit`, and stores the
//! prepared environment pointer: everything it needs is allocated before `fork`, so this is
//! sound in a multi-threaded test process as well.

use std::ffi::{CString, c_char};
use std::os::fd::{AsRawFd, OwnedFd};

use ekacache_core::env::Env;

use crate::sys;

/// Exit status of a failed `fork` or `exec` (`kExecFailedStatus`).
pub const EXEC_FAILED: i32 = 127;

/// Death by signal `n` reports as `128 + n` (`kSignalStatusBase`).
pub const SIGNAL_BASE: i32 = 128;

/// jig's environment: what `getenv` returns, and what every child inherits.
///
/// Legacy changes its own environment with `setenv`/`unsetenv` (`JIG_SLOT`, `JIG_ABSENT_LOG`,
/// `GOCACHEPROG`) so that children inherit the change, and reads it back with `getenv`. This
/// keeps the same model without touching the real process environment (unsound with threads in
/// Rust 2024): `set` is `setenv(name, value, 1)` (replace the first entry, else append), `remove`
/// is `unsetenv` (every entry), and a child gets these entries as its `environ` once anything
/// changed. Before any change the child inherits `environ` untouched.
#[derive(Clone, Debug)]
pub struct ProcEnv {
    env: Env,
    changed: bool,
}

impl ProcEnv {
    #[must_use]
    pub fn new(env: Env) -> Self {
        Self {
            env,
            changed: false,
        }
    }

    /// The current view.
    #[must_use]
    pub fn env(&self) -> &Env {
        &self.env
    }

    /// `getenv(name)`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.env.get(name)
    }

    /// `Env(name, fallback)`: the fallback only when unset.
    #[must_use]
    pub fn get_or<'a>(&'a self, name: &str, fallback: &'a [u8]) -> &'a [u8] {
        self.env.get_or(name, fallback)
    }

    /// Set and not empty.
    #[must_use]
    pub fn non_empty(&self, name: &str) -> Option<&[u8]> {
        self.env.non_empty(name)
    }

    /// `setenv(name, value, 1)`.
    pub fn set(&mut self, name: &str, value: impl Into<Vec<u8>>) {
        self.env.set(name, value);
        self.changed = true;
    }

    /// `unsetenv(name)`.
    pub fn remove(&mut self, name: &str) {
        self.env.remove(name);
        self.changed = true;
    }

    /// The `NAME=value` strings for a child, or `None` to inherit `environ` as it is.
    fn envp(&self) -> Option<Vec<CString>> {
        self.changed
            .then(|| self.env.raw_entries().map(|e| sys::cstring(&e)).collect())
    }
}

/// Which output of the child `run` collects (`StderrMode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capture {
    Inherit,
    Stderr,
    Stdout,
}

/// `RunResult`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunResult {
    /// `WEXITSTATUS`, `128 + signal`, `127` when `fork` or `exec` failed, `1` when `waitpid`
    /// failed (Q-DISPATCH-6).
    pub status: i32,
    /// The captured stream; `jig: fork failed\n` after a failed `fork` (Q-DISPATCH-4).
    pub captured: Vec<u8>,
}

/// `pipe2(O_CLOEXEC)`.
#[cfg(not(target_vendor = "apple"))]
fn cloexec_pipe() -> Option<(OwnedFd, OwnedFd)> {
    rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).ok()
}

/// macOS has no `pipe2` (`11-macos.md`): `pipe`, then `FD_CLOEXEC` on both ends. jig forks from
/// one thread, so no child can inherit the ends between the calls.
#[cfg(target_vendor = "apple")]
fn cloexec_pipe() -> Option<(OwnedFd, OwnedFd)> {
    let (r, w) = rustix::pipe::pipe().ok()?;
    rustix::io::fcntl_setfd(&r, rustix::io::FdFlags::CLOEXEC).ok()?;
    rustix::io::fcntl_setfd(&w, rustix::io::FdFlags::CLOEXEC).ok()?;
    Some((r, w))
}

/// `Run(program, args, mode)`: `execvp(program, [program, args...])` with a `PATH` search when
/// `program` has no `/`, and wait.
pub fn run<A: AsRef<[u8]>>(
    program: &[u8],
    args: &[A],
    capture: Capture,
    env: &ProcEnv,
) -> RunResult {
    // everything the child touches is allocated here, before fork
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(sys::cstring(program));
    argv.extend(args.iter().map(|a| sys::cstring(a.as_ref())));
    let argv_ptrs = sys::ptr_array(&argv);
    let envp = env.envp();
    let envp_ptrs: Option<Vec<*const c_char>> = envp.as_deref().map(sys::ptr_array);
    let target_fd = if capture == Capture::Stdout { 1 } else { 2 };
    // a pipe that cannot be made means inheriting (`process.cc:55-57`); CLOEXEC closes both
    // originals in the child at exec, as legacy closes them by hand
    let pipe = match capture {
        Capture::Inherit => None,
        Capture::Stderr | Capture::Stdout => cloexec_pipe(),
    };

    // SAFETY: fork has no preconditions; the child below only calls async-signal-safe
    // functions on memory prepared above.
    #[allow(unsafe_code)]
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return RunResult {
            status: EXEC_FAILED,
            captured: b"jig: fork failed\n".to_vec(),
        };
    }
    if pid == 0 {
        // SAFETY: the child of fork: dup2, the environ store, execvp and _exit only. The argv
        // and envp arrays point into CStrings that live until exec replaces the image.
        #[allow(unsafe_code)]
        unsafe {
            if let Some((_, write_end)) = &pipe {
                libc::dup2(write_end.as_raw_fd(), target_fd);
            }
            if let Some(envp) = &envp_ptrs {
                sys::set_environ(envp.as_ptr());
            }
            libc::execvp(argv_ptrs[0], argv_ptrs.as_ptr());
            libc::_exit(EXEC_FAILED);
        }
    }

    let mut captured = Vec::new();
    if let Some((read_end, write_end)) = pipe {
        drop(write_end);
        let mut buf = [0u8; 4096];
        // legacy stops at the first read that returns <= 0, EINTR included
        while let Ok(n @ 1..) = rustix::io::read(&read_end, &mut buf) {
            captured.extend_from_slice(&buf[..n]);
        }
    }
    let mut wstatus: libc::c_int = 0;
    loop {
        // SAFETY: waitpid on our own child with a valid status pointer.
        #[allow(unsafe_code)]
        let rc = unsafe { libc::waitpid(pid, &raw mut wstatus, 0) };
        if rc >= 0 {
            break;
        }
        if sys::errno() != libc::EINTR {
            // Q-DISPATCH-6: the default status
            return RunResult {
                status: 1,
                captured,
            };
        }
    }
    let status = if libc::WIFEXITED(wstatus) {
        libc::WEXITSTATUS(wstatus)
    } else {
        SIGNAL_BASE + libc::WTERMSIG(wstatus)
    };
    RunResult { status, captured }
}

/// ADOPTED Q-CC-5 (`behaviors/jig-cc-mode.md`): resource errors that say nothing about the
/// inputs. A failure whose stderr mentions one is not stored.
const TRANSIENT: [&[u8]; 9] = [
    b"No space left on device",
    b"Disk quota exceeded",
    b"Cannot allocate memory",
    b"virtual memory exhausted",
    b"out of memory",
    b"Too many open files",
    b"Input/output error",
    b"Read-only file system",
    b"Resource temporarily unavailable",
];

/// `Deterministic(run)`: not killed by a signal, the driver does not report a crashed child,
/// and (ADOPTED Q-CC-5) no resource error is mentioned.
#[must_use]
pub fn deterministic(r: &RunResult) -> bool {
    use ekacache_core::bytes::contains;
    r.status < SIGNAL_BASE
        && !contains(&r.captured, b"unable to execute command")
        && !contains(&r.captured, b"PLEASE submit a bug report")
        && !TRANSIENT.iter().any(|t| contains(&r.captured, t))
}

/// Launcher `Exec` (`launcher.cc:43-55`): `execv(program, [program, args...])`, no `PATH`
/// search, with jig's environment. Returns only on failure, after printing
/// `jig: cannot execute <program>: <strerror>`; the caller exits with the returned 127.
pub fn exec<A: AsRef<[u8]>>(program: &[u8], args: &[A], env: &ProcEnv) -> i32 {
    use std::io::Write;
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(sys::cstring(program));
    argv.extend(args.iter().map(|a| sys::cstring(a.as_ref())));
    let argv_ptrs = sys::ptr_array(&argv);
    let envp = env.envp();
    // SAFETY: NULL-terminated arrays of C strings that outlive the call.
    #[allow(unsafe_code)]
    unsafe {
        match &envp {
            Some(envp) => {
                let envp_ptrs = sys::ptr_array(envp);
                libc::execve(argv_ptrs[0], argv_ptrs.as_ptr(), envp_ptrs.as_ptr());
            }
            None => {
                libc::execv(argv_ptrs[0], argv_ptrs.as_ptr());
            }
        }
    }
    let err = sys::errno();
    let mut msg = b"jig: cannot execute ".to_vec();
    msg.extend_from_slice(program);
    msg.extend_from_slice(b": ");
    msg.extend_from_slice(&sys::strerror(err));
    msg.push(b'\n');
    let _ = std::io::stderr().write_all(&msg);
    EXEC_FAILED
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    fn env() -> ProcEnv {
        ProcEnv::new(Env::capture())
    }

    fn sh(script: &str, capture: Capture, env: &ProcEnv) -> RunResult {
        run(b"/bin/sh", &[&b"-c"[..], script.as_bytes()], capture, env)
    }

    #[test]
    fn status_mapping() {
        let e = env();
        assert_eq!(sh("exit 3", Capture::Inherit, &e).status, 3);
        assert_eq!(sh("exit 0", Capture::Inherit, &e).status, 0);
        assert_eq!(sh("kill -KILL $$", Capture::Inherit, &e).status, 137);
        assert_eq!(sh("kill -TERM $$", Capture::Inherit, &e).status, 143);
    }

    #[test]
    fn capture_streams() {
        let e = env();
        let r = sh("echo out; echo err >&2; exit 2", Capture::Stderr, &e);
        assert_eq!(
            r,
            RunResult {
                status: 2,
                captured: b"err\n".to_vec()
            }
        );
        let r = sh("echo out; echo err >&2", Capture::Stdout, &e);
        assert_eq!(
            r,
            RunResult {
                status: 0,
                captured: b"out\n".to_vec()
            }
        );
        let r = sh("echo out", Capture::Inherit, &e);
        assert!(r.captured.is_empty());
    }

    #[test]
    fn large_capture() {
        let e = env();
        let r = sh("head -c 1000000 /dev/zero >&2", Capture::Stderr, &e);
        assert_eq!(r.status, 0);
        assert_eq!(r.captured.len(), 1_000_000);
    }

    #[test]
    fn exec_failure_is_a_silent_127() {
        let e = env();
        let r = run(b"/nonexistent/prog", &[b"x"], Capture::Stderr, &e);
        assert_eq!(
            r,
            RunResult {
                status: 127,
                captured: Vec::new()
            }
        );
        let r = run(
            b"jig-no-such-program-on-path",
            &[] as &[&[u8]],
            Capture::Stderr,
            &e,
        );
        assert_eq!(r.status, 127);
    }

    #[test]
    fn path_search_and_argv0() {
        let e = env();
        // `sh` found through PATH, `$0` is the name as given
        let r = run(b"sh", &[&b"-c"[..], b"echo $0"], Capture::Stdout, &e);
        assert_eq!(
            r,
            RunResult {
                status: 0,
                captured: b"sh\n".to_vec()
            }
        );
    }

    #[test]
    fn environment_changes_reach_the_child() {
        let mut e = ProcEnv::new(Env::from_pairs(&[
            ("PATH", "/usr/bin:/bin"),
            ("A", "first"),
            ("A", "second"),
            ("B", "b"),
        ]));
        e.set("JIG_SLOT", "1");
        e.set("A", "new");
        e.remove("B");
        // env(1) prints `environ` as it is: order kept, the first duplicate replaced. Found
        // through the test's own PATH (the Nix build sandbox has no /usr/bin/env).
        let Some(env_bin) = std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|d| d.join("env"))
                .find(|p| p.is_file())
        }) else {
            eprintln!("no env(1) on PATH: skipped");
            return;
        };
        let r = run(
            env_bin.as_os_str().as_bytes(),
            &[] as &[&[u8]],
            Capture::Stdout,
            &e,
        );
        let out = String::from_utf8(r.captured).unwrap();
        assert_eq!(out, "PATH=/usr/bin:/bin\nA=new\nA=second\nJIG_SLOT=1\n");
        assert_eq!(e.get("A"), Some(&b"new"[..]));
        assert_eq!(e.get("B"), None);
    }

    #[test]
    fn sigpipe_disposition_is_inherited() {
        // the test harness runs with SIGPIPE ignored (std's runtime); a child must see the same,
        // where std::process::Command would reset it to SIG_DFL
        let e = env();
        let r = sh("kill -PIPE $$; echo alive", Capture::Stdout, &e);
        assert_eq!(
            r,
            RunResult {
                status: 0,
                captured: b"alive\n".to_vec()
            }
        );
    }

    #[test]
    fn deterministic_markers() {
        let ok = |status, text: &[u8]| {
            deterministic(&RunResult {
                status,
                captured: text.to_vec(),
            })
        };
        assert!(ok(1, b"error: x"));
        assert!(!ok(128, b""));
        assert!(!ok(139, b""));
        assert!(ok(127, b""));
        assert!(!ok(
            1,
            b"clang: error: unable to execute command: Segmentation fault"
        ));
        assert!(!ok(1, b"PLEASE submit a bug report to ..."));
        // ADOPTED Q-CC-5
        for t in TRANSIENT {
            let mut text = b"a.c:1: fatal error: ".to_vec();
            text.extend_from_slice(t);
            assert!(!ok(1, &text));
        }
        assert!(!ok(1, b"cc1: out of memory allocating 65536 bytes"));
        assert!(ok(1, b"error: Out Of Memory is not a type"));
    }
}
