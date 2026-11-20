//! Shared helpers for the integration tests: an ecd started from the binary `EC_RUST_ECD`
//! names, and the ec binary under test.
#![allow(dead_code, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// The Rust ec under test.
pub const EC: &str = env!("CARGO_BIN_EXE_ec");

/// A running ecd, killed on drop.
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
        .env("EKACACHED_SLOTS", "2")
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let t = Instant::now();
    // the socket file can exist before listen() returns; wait until it accepts
    while std::os::unix::net::UnixStream::connect(&sock).is_err() {
        assert!(t.elapsed() < Duration::from_secs(20), "ecd did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    Daemon { child, sock, dir }
}

/// The daemon whose binary `var` names, if set.
pub fn daemon(var: &str) -> Option<Daemon> {
    let bin = std::env::var_os(var)?;
    Some(start(Path::new(&bin)))
}
