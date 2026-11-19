//! Shared helpers for the integration tests: a jigd started from a binary named by an
//! environment variable (`EC_RUST_ECD`, `JIG_LEGACY_JIGD`), and the jig binaries under test.
#![allow(dead_code, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// The Rust jig under test.
pub const JIG: &str = env!("CARGO_BIN_EXE_ec");

/// A running jigd, killed on drop.
pub struct Daemon {
    child: Child,
    pub sock: PathBuf,
    pub dir: tempfile::TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    /// The socket path as bytes.
    pub fn sock_bytes(&self) -> &[u8] {
        self.sock.as_os_str().as_encoded_bytes()
    }
}

/// Start `bin <socket>` with its cache under a fresh directory and two slots, and wait until it
/// accepts connections.
pub fn start(bin: &Path) -> Daemon {
    let dir = tempfile::Builder::new()
        .prefix("jc")
        .tempdir_in("/tmp")
        .unwrap();
    let sock = dir.path().join("s");
    let child = Command::new(bin)
        .arg(&sock)
        .env("XDG_CACHE_HOME", dir.path().join("cache"))
        .env("JIGD_SLOTS", "2")
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let t = Instant::now();
    // the socket file can exist before listen() returns; wait until it accepts
    while std::os::unix::net::UnixStream::connect(&sock).is_err() {
        assert!(t.elapsed() < Duration::from_secs(20), "jigd did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    Daemon { child, sock, dir }
}

/// The daemon whose binary `var` names, if set.
pub fn daemon(var: &str) -> Option<Daemon> {
    let bin = std::env::var_os(var)?;
    Some(start(Path::new(&bin)))
}

/// Any available daemon: the Rust jigd if given, else the Go one.
pub fn any_daemon() -> Option<Daemon> {
    daemon("EC_RUST_ECD").or_else(|| daemon("JIG_LEGACY_JIGD"))
}

/// The legacy C++ jig, if `JIG_LEGACY_JIG` names it.
pub fn legacy_jig() -> Option<PathBuf> {
    std::env::var_os("JIG_LEGACY_JIG").map(PathBuf::from)
}
