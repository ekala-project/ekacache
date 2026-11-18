//! The client against real daemons (`implementation/05-jig-foundation.md §Tests`): the Go jigd
//! (`$JIG_LEGACY_JIGD`, HELLO falls back to version 0) and the Rust jigd (version 1). Each test
//! is skipped when its daemon binary is not given.
#![allow(
    clippy::unwrap_used,
    clippy::many_single_char_names,
    clippy::items_after_statements
)]

use std::time::Instant;

use jig::client::Client;

mod common;
use common::{Daemon, daemon};

fn round_trips(d: &Daemon, version: u32) {
    let sock = d.sock_bytes();
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
        let sock = d.sock_bytes();
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
