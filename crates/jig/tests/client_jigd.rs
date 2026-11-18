//! The client against real daemons (`implementation/05-jig-foundation.md §Tests`): the Go jigd
//! (`$JIG_LEGACY_JIGD`, HELLO falls back to version 0) and the Rust jigd (version 1). Each test
//! is skipped when its daemon binary is not given.
#![allow(clippy::unwrap_used, clippy::many_single_char_names, clippy::items_after_statements)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use jig::client::Client;

struct Daemon {
    child: Child,
    sock: PathBuf,
    _dir: tempfile::TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(bin: &Path) -> Daemon {
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
    while !sock.exists() {
        assert!(t.elapsed() < Duration::from_secs(20), "jigd did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    // the socket file exists before listen() returns in some daemons; wait until it accepts
    while std::os::unix::net::UnixStream::connect(&sock).is_err() {
        std::thread::sleep(Duration::from_millis(20));
    }
    Daemon {
        child,
        sock,
        _dir: dir,
    }
}

fn daemon(var: &str) -> Option<Daemon> {
    let bin = std::env::var_os(var)?;
    Some(start(Path::new(&bin)))
}

fn round_trips(d: &Daemon, version: u32) {
    let sock = d.sock.as_os_str().as_encoded_bytes();
    let mut c = Client::connected_to(sock).expect("connect");
    assert_eq!(c.version(), version);
    assert_eq!(c.get(b"o/none"), None);
    assert!(c.connected());
    assert!(c.put(b"k1", b"hello"));
    assert_eq!(c.get(b"k1").as_deref(), Some(&b"hello"[..]));
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    assert_eq!(c.put_many(&[(&b"k2"[..], &big[..]), (b"k3", b"")]), 2);
    assert_eq!(
        c.get_many(&[&b"k2"[..], b"nope", b"k3"]),
        vec![Some(big.clone()), None, Some(Vec::new())]
    );
    assert_eq!(c.has_many(&[&b"k1"[..], b"nope"]), vec![true, false]);
    // not a store path: no answer
    assert_eq!(c.identities(&[b"/etc/hostname"]), vec![Vec::<u8>::new()]);
    assert!(c.acquire_slot(b"/build"));
    c.release_slot(b"/build");
    assert!(c.connected());
    // a key the daemon refuses (over 4096 bytes): the daemon hangs up, the client notices
    // (ADOPTED Q-CLIENT-2) and is disconnected
    assert!(!c.put(&[b'k'; 5000], b"v"));
    assert!(!c.connected());
    // a second client still works
    let mut c2 = Client::connected_to(sock).expect("connect");
    assert_eq!(c2.get(b"k1").as_deref(), Some(&b"hello"[..]));
}

#[test]
fn legacy_go_jigd() {
    let Some(d) = daemon("JIG_LEGACY_JIGD") else {
        eprintln!("JIG_LEGACY_JIGD not set: skipped");
        return;
    };
    round_trips(&d, 0);
}

#[test]
fn rust_jigd() {
    let Some(d) = daemon("JIG_RUST_JIGD") else {
        eprintln!("JIG_RUST_JIGD not set: skipped");
        return;
    };
    round_trips(&d, 1);
}

/// The HELLO cost (`§HELLO ... Benchmark it in P3`): connect + one GET, many times.
#[test]
#[ignore = "benchmark; run with --ignored --nocapture"]
fn hello_cost() {
    for var in ["JIG_LEGACY_JIGD", "JIG_RUST_JIGD"] {
        let Some(d) = daemon(var) else { continue };
        let sock = d.sock.as_os_str().as_encoded_bytes();
        let n = 2000;
        let t = Instant::now();
        for _ in 0..n {
            let mut c = Client::connected_to(sock).unwrap();
            let _ = c.get(b"o/none");
        }
        let per = t.elapsed() / n;
        eprintln!("{var}: connect (with HELLO negotiation) + GET: {per:?}");
        let t = Instant::now();
        for _ in 0..n {
            let mut s = std::os::unix::net::UnixStream::connect(&d.sock).unwrap();
            use std::io::{BufRead, Write};
            s.write_all(b"GET o/none\n").unwrap();
            let mut l = String::new();
            std::io::BufReader::new(s).read_line(&mut l).unwrap();
        }
        eprintln!("{var}: bare connect + GET: {:?}", t.elapsed() / n);
    }
}
