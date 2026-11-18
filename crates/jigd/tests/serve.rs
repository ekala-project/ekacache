//! The server against an in-process daemon on a temporary socket (implementation/03 §Tests):
//! `legacy/jigd/serve_test.go` ported, the wire examples of `jigd-protocol.md`, the adopted
//! HELLO and Q-SLOT-1, the Go buffer sizes that show through, and every hang-up that can be
//! provoked from a client.

// helpers outside #[test] fns: a panic is the failure report
#![allow(clippy::unwrap_used)]

mod common;

use std::io::Write;
use std::time::{Duration, Instant};

use common::{Client, End, InProc, eventually, put_req, show};
use jigd::peer::Auth;

fn path(srv: &InProc, name: &str) -> Vec<u8> {
    [
        srv.root().as_os_str().as_encoded_bytes(),
        b"/",
        name.as_bytes(),
    ]
    .concat()
}

/// `TestServeIdsAndPipelinedGets`: IDS answers one line per path in order, and pipelined GETs
/// are answered in order on one stream; then the one slot is taken, returned with DONE, taken
/// again and freed by hanging up.
#[test]
fn serve_ids_and_pipelined_gets() {
    let srv = InProc::start(1);
    std::fs::write(srv.root().join("h.h"), b"abc").unwrap();
    let mut c = srv.connect();
    let mut req = b"PUT k1 3\nabcIDS 2\n".to_vec();
    req.extend_from_slice(&path(&srv, "h.h"));
    req.push(b'\n');
    req.extend_from_slice(&path(&srv, "missing"));
    req.extend_from_slice(b"\nGET nope\nGET k1\n");
    c.send(&req);
    let mut got = Vec::new();
    for _ in 0..5 {
        let line = c.line_str();
        if line == "OK 3\n" {
            assert_eq!(c.exact(3), b"abc");
        }
        got.push(line.trim_end_matches('\n').to_owned());
    }
    assert_eq!(
        got,
        [
            "OK",
            "C:6437b3ac38465133ffb63b75273a8db5",
            "",
            "MISS",
            "OK 3"
        ]
    );

    c.send(b"SLOT b1\nDONE b1\nSLOT b1\n");
    for _ in 0..3 {
        assert_eq!(c.line_str(), "OK\n");
    }
    assert_eq!(srv.slots().0, 1);
    drop(c);
    let mut other = srv.connect();
    other.send(b"SLOT b2\n");
    assert_eq!(other.line_str(), "OK\n", "slot after hang-up");
}

/// `TestServeHangsUpOnEndlessLine`: a request line longer than the read buffer is a broken or
/// hostile client: hang up, don't buffer.
#[test]
fn serve_hangs_up_on_endless_line() {
    let srv = InProc::start(1);
    let mut c = srv.connect();
    let mut w = c.stream().try_clone().unwrap();
    let writer = std::thread::spawn(move || {
        let junk = vec![b'a'; 1 << 16];
        while w.write_all(&junk).is_ok() {}
    });
    let (got, end) = c.rest();
    assert!(
        got.is_empty(),
        "daemon answered an endless line: {}",
        show(&got)
    );
    assert_ne!(end, End::Open, "daemon did not hang up");
    writer.join().unwrap();
}

/// `jigd-protocol.md` §Wire examples, byte for byte, then the three PUTs on their own
/// connections and their log lines.
#[test]
fn wire_examples() {
    let srv = InProc::start(2);
    let mut c = srv.connect();
    c.send(b"PUT k +3\nabc");
    assert_eq!(c.line(), b"OK\n");
    c.send("GET\u{a0}k\r\n".as_bytes());
    assert_eq!(c.line(), b"OK 3\n");
    assert_eq!(c.exact(3), b"abc");
    // IDS 0 writes nothing: the next reply is HAS's
    c.send(b"IDS 0\nHAS k\n");
    assert_eq!(c.line(), b"1\n");
    c.send(b"HAS z\n");
    assert_eq!(c.line(), b"0\n");
    c.send(b"DONE b\n");
    assert_eq!(c.line(), b"OK\n");
    assert_eq!(
        c.stats(),
        "gets=1 hits=1 puts=1 ids=0 slots=0 waiting=0 keys=1 packs=1 bytes=12 live=3"
    );
    assert!(srv.logs.lines().is_empty());

    for req in [&b"PUT k -1\n"[..], b"PUT k 5\nab", b"PUT k x\n"] {
        let mut c = srv.connect();
        c.send(req);
        c.half_close();
        let (got, end) = c.rest();
        assert!(got.is_empty(), "{}: {}", show(req), show(&got));
        assert_eq!(end, End::Eof, "{}", show(req));
    }
    assert_eq!(
        srv.logs.strings(),
        ["put k: bad put", "put k: unexpected EOF"]
    );
    assert!(srv.connect().stats().contains(" puts=3 "));
}

/// The rest of a connection that sent `req` and then EOF.
fn transcript(srv: &InProc, req: &[u8]) -> (Vec<u8>, End) {
    let mut c = srv.connect();
    c.send(req);
    c.half_close();
    c.rest()
}

/// Adopted HELLO: `OK min(n, 1)` as the first request; anything else about it hangs up.
#[test]
fn hello() {
    let srv = InProc::start(2);
    for (req, want) in [
        (&b"HELLO 1\n"[..], &b"OK 1\n"[..]),
        (b"HELLO 7\n", b"OK 1\n"),
        (b"HELLO 2147483647\n", b"OK 1\n"),
        (b"HELLO +01\n", b"OK 1\n"),
        (b" HELLO\t1 \r\n", b"OK 1\n"),
        (b"HELLO 0\n", b""),
        (b"HELLO -1\n", b""),
        (b"HELLO 2147483648\n", b""),
        (b"HELLO x\n", b""),
        (b"HELLO\n", b""),
        (b"HELLO 1 1\n", b""),
        (b"hello 1\n", b""),
        // H11: only as the first request
        (b"HELLO 1\nHELLO 1\nSTATS\n", b"OK 1\n"),
        (b"HAS k\nHELLO 1\nHAS k\n", b"0\n"),
    ] {
        let (got, end) = transcript(&srv, req);
        assert_eq!(show(&got), show(want), "{}", show(req));
        assert_eq!(end, End::Eof, "{}", show(req));
    }
    // version 1 is version 0 plus HELLO: the requests after it behave as without it, and HELLO
    // touches no counter
    let (got, _) = transcript(&srv, b"HELLO 1\nPUT k 1\nxGET k\nSTATS\n");
    assert_eq!(
        show(&got),
        show(b"OK 1\nOK\nOK 1\nxgets=1 hits=1 puts=1 ids=0 slots=0 waiting=0 keys=1 packs=1 bytes=10 live=1\n")
    );
}

/// Adopted Q-SLOT-1: a queued SLOT whose peer hangs up is dequeued; the token goes to the next
/// waiter, not to the dead one.
#[test]
fn abandoned_waiter_is_dequeued() {
    let srv = InProc::start(1);
    let mut a = srv.connect();
    a.send(b"SLOT b\n");
    assert_eq!(a.line(), b"OK\n");
    let mut b = srv.connect();
    b.send(b"SLOT b\n");
    assert!(b.quiet());
    assert_eq!(srv.slots(), (1, 1));
    let t = Instant::now();
    drop(b);
    assert!(eventually(|| srv.slots() == (1, 0)), "waiter not dequeued");
    assert!(
        t.elapsed() < Duration::from_secs(1),
        "dequeued after {:?}",
        t.elapsed()
    );
    let mut c = srv.connect();
    c.send(b"SLOT c\n");
    assert!(c.quiet());
    a.send(b"DONE b\n");
    assert_eq!(a.line(), b"OK\n");
    assert_eq!(c.line(), b"OK\n");
    assert_eq!(srv.slots(), (1, 0));
    drop(c);
    assert!(eventually(|| srv.slots() == (0, 0)));
}

/// A half-close is a hang-up too (the peer can send no DONE); the waiter is dequeued and the
/// connection closed.
#[test]
fn half_closed_waiter_is_dequeued() {
    let srv = InProc::start(1);
    let mut a = srv.connect();
    a.send(b"SLOT b\n");
    assert_eq!(a.line(), b"OK\n");
    let mut b = srv.connect();
    b.send(b"SLOT b\n");
    b.half_close();
    assert_eq!(b.rest(), (Vec::new(), End::Eof));
    assert_eq!(srv.slots(), (1, 0));
}

/// While a SLOT waits, requests pipelined after it are kept, and answered once it is granted.
#[test]
fn requests_behind_a_queued_slot_are_kept() {
    let srv = InProc::start(1);
    let mut a = srv.connect();
    a.send(b"SLOT x\n");
    assert_eq!(a.line(), b"OK\n");
    let mut b = srv.connect();
    b.send(b"SLOT y\nHAS z\n");
    assert!(b.quiet());
    b.send(b"STATS\n");
    assert!(b.quiet());
    a.send(b"DONE x\n");
    assert_eq!(a.line(), b"OK\n");
    assert_eq!(b.line(), b"OK\n");
    assert_eq!(b.line(), b"0\n");
    assert!(b.line_str().contains(" slots=1 waiting=0 "));
}

/// A connection's tokens go back when it hangs up, all of them; an unheld DONE changes nothing.
#[test]
fn hang_up_releases_every_token() {
    let srv = InProc::start(3);
    let mut a = srv.connect();
    a.send(b"DONE b\nSLOT b\nSLOT b\nSLOT c\nDONE d\n");
    for _ in 0..5 {
        assert_eq!(a.line(), b"OK\n");
    }
    assert_eq!(srv.slots(), (3, 0));
    a.send(b"DONE b\nDONE b\nDONE b\n");
    for _ in 0..3 {
        assert_eq!(a.line(), b"OK\n");
    }
    assert_eq!(srv.slots(), (1, 0));
    a.send(b"SLOT b\n");
    assert_eq!(a.line(), b"OK\n");
    drop(a);
    assert!(eventually(|| srv.slots() == (0, 0)));
}

/// `TestSlotsFewestHeldFirst` across connections: the build holding fewer tokens is woken
/// first, whatever the arrival order.
#[test]
fn fewest_held_first_over_the_wire() {
    let srv = InProc::start(2);
    let mut a = srv.connect();
    a.send(b"SLOT big\nSLOT big\n");
    assert_eq!(a.line(), b"OK\n");
    assert_eq!(a.line(), b"OK\n");
    let mut big = srv.connect();
    big.send(b"SLOT big\n");
    assert!(eventually(|| srv.slots() == (2, 1)));
    let mut small = srv.connect();
    small.send(b"SLOT small\n");
    assert!(eventually(|| srv.slots() == (2, 2)));
    a.send(b"DONE big\n");
    assert_eq!(a.line(), b"OK\n");
    assert_eq!(small.line(), b"OK\n");
    assert!(big.quiet());
    a.send(b"DONE big\n");
    assert_eq!(a.line(), b"OK\n");
    assert_eq!(big.line(), b"OK\n");
}

/// Q-PROTO-5 and the 4 096-byte reply buffer: answers leave in full buffers only, and a failed
/// path line drops what is still buffered.
#[test]
fn ids_cut_short_delivers_whole_buffers() {
    let srv = InProc::start(1);
    let mut req = b"IDS 5000\n".to_vec();
    for _ in 0..4500 {
        req.extend_from_slice(b"/nope\n");
    }
    let (got, end) = transcript(&srv, &req);
    assert_eq!(got, vec![b'\n'; 4096]);
    assert_eq!(end, End::Eof);

    std::fs::write(srv.root().join("h.h"), b"abc").unwrap();
    let mut req = b"IDS 3\n".to_vec();
    req.extend_from_slice(&path(&srv, "h.h"));
    req.extend_from_slice(b"\n/x\n");
    let (got, end) = transcript(&srv, &req);
    assert_eq!(show(&got), "");
    assert_eq!(end, End::Eof);

    // a path line is not tokenized, and only its \n is stripped
    std::fs::write(srv.root().join("a b"), b"").unwrap();
    let mut req = b"IDS 3\n".to_vec();
    for name in ["a b", "h.h\r", "a b"] {
        req.extend_from_slice(&path(&srv, name));
        req.push(b'\n');
    }
    req.extend_from_slice(b"STATS\n");
    let (got, _) = transcript(&srv, &req);
    let got = String::from_utf8(got).unwrap();
    let mut lines = got.lines();
    assert_eq!(lines.next(), Some("C:af1349b9f5f9a1a6a0404dea36dcc949"));
    assert_eq!(lines.next(), Some(""));
    assert_eq!(lines.next(), Some("C:af1349b9f5f9a1a6a0404dea36dcc949"));
    // h.h from the earlier batch is memoised too
    assert!(lines.next().unwrap().contains(" ids=2 "), "{got}");
}

/// H2 at the boundary: 65 535 bytes and the newline fit, one more byte does not.
#[test]
fn line_limit() {
    let srv = InProc::start(1);
    let mut req = b"GET ".to_vec();
    req.resize(65_535, b'k');
    req.push(b'\n');
    req.extend_from_slice(b"HAS k\n");
    assert_eq!(transcript(&srv, &req), (b"MISS\n0\n".to_vec(), End::Eof));

    let mut req = b"GET ".to_vec();
    req.resize(65_536, b'k');
    req.extend_from_slice(b"\nHAS k\n");
    let (got, end) = transcript(&srv, &req);
    assert_eq!(show(&got), "");
    // the newline and HAS were never read
    assert_eq!(end, End::Reset);

    // a partial line before EOF is discarded (Q-PROTO-11)
    assert_eq!(
        transcript(&srv, b"HAS k\nSTATS"),
        (b"0\n".to_vec(), End::Eof)
    );
}

/// H3, H4, H5, H7: malformed requests hang up without a reply and without logging.
#[test]
fn malformed_requests_hang_up() {
    let srv = InProc::start(1);
    for req in [
        &b""[..],
        b"\n",
        b" \t\r\n",
        b"GET\n",
        b"GET a b\n",
        b"get k\n",
        b"STATS x\n",
        b"FOO\n",
        b"GET\xa0k\n",
        "GET\u{200b}k\n".as_bytes(),
        b"PUT k\n",
        b"PUT k 0x3\n",
        b"PUT k 3e0\n",
        b"PUT k ++3\n",
        b"PUT k 9223372036854775808\n",
        b"IDS x\n",
        b"IDS -1\n",
        b"IDS 1048577\n",
        b"IDS 99999999999999999999\n",
        b"IDS 1048576\n",
        b"SLOT\n",
        b"DONE a b\n",
    ] {
        let (got, end) = transcript(&srv, req);
        assert_eq!(show(&got), "", "{}", show(req));
        assert_eq!(end, End::Eof, "{}", show(req));
    }
    assert!(srv.logs.lines().is_empty(), "{:?}", srv.logs.strings());
    assert!(
        srv.connect()
            .stats()
            .starts_with("gets=0 hits=0 puts=0 ids=0 ")
    );
}

/// Unicode separators split fields the way Go's `strings.Fields` does.
#[test]
fn unicode_separators() {
    let srv = InProc::start(1);
    for sep in [
        "\t", "\u{b}", "\u{c}", "\r", " ", "\u{85}", "\u{a0}", "\u{1680}", "\u{2000}", "\u{200a}",
        "\u{2028}", "\u{2029}", "\u{202f}", "\u{205f}", "\u{3000}",
    ] {
        let req = format!("{sep}HAS{sep}{sep}k{sep}\n");
        assert_eq!(
            transcript(&srv, req.as_bytes()),
            (b"0\n".to_vec(), End::Eof),
            "{sep:?}"
        );
    }
}

/// H6: a PUT the store refuses is logged and hangs up; the request still counts.
#[test]
fn refused_puts_are_logged() {
    let srv = InProc::start(1);
    let long = vec![b'k'; 4097];
    let mut req = b"PUT ".to_vec();
    req.extend_from_slice(&long);
    req.extend_from_slice(b" 1\nx");
    for req in [
        &b"PUT k -1\n"[..],
        b"PUT k 2147483649\n",
        &req,
        b"PUT k 3\n",
        b"PUT k 5\nab",
        b"PUT big 5000000\nabc",
    ] {
        let (got, _) = transcript(&srv, req);
        assert_eq!(show(&got), "", "{}", show(req));
    }
    let mut want = vec!["put k: bad put".to_owned(), "put k: bad put".to_owned()];
    want.push(format!("put {}: bad put", "k".repeat(4097)));
    want.extend(["put k: EOF", "put k: unexpected EOF", "put big: EOF"].map(String::from));
    assert_eq!(srv.logs.strings(), want);
    let stats = srv.connect().stats();
    assert!(
        stats.contains(" puts=6 ") && stats.contains(" keys=0 "),
        "{stats}"
    );

    // the largest key and an empty value are fine
    let key = vec![b'k'; 4096];
    let mut c = srv.connect();
    c.put(&key, b"");
    c.put(b"z", b"");
    assert_eq!(c.get(&key), Some(Vec::new()));
    let (got, _) = transcript(&srv, b"PUT k -0\nGET k\n");
    assert_eq!(got, b"OK\nOK 0\n");
}

/// Values in memory, spooled values (4 MiB and up) and GET bodies larger than the chunk size,
/// with requests pipelined behind them.
#[test]
fn large_values() {
    let srv = InProc::start(1);
    let small: Vec<u8> = (0..(1u32 << 20) + 3).map(|i| (i % 251) as u8).collect();
    let big: Vec<u8> = (0..5_000_000u32).map(|i| (i % 253) as u8).collect();
    let mut c = srv.connect();
    let mut req = put_req(b"small", &small);
    req.extend_from_slice(&put_req(b"big", &big));
    c.send(&req);
    assert_eq!(c.line(), b"OK\n");
    assert_eq!(c.line(), b"OK\n");
    c.send(b"GET big\nGET small\nHAS big\n");
    assert_eq!(c.line_str(), "OK 5000000\n");
    assert!(c.exact(big.len()) == big);
    assert_eq!(c.line_str(), format!("OK {}\n", small.len()));
    assert!(c.exact(small.len()) == small);
    assert_eq!(c.line(), b"1\n");
    assert!(srv.logs.lines().is_empty());
}

/// H10 and H9 from the client side: a client that goes away in the middle of a large GET only
/// ends its own connection.
#[test]
fn vanishing_reader() {
    let srv = InProc::start(1);
    let big = vec![7u8; 8 << 20];
    let mut c = srv.connect();
    c.put(b"big", &big);
    for _ in 0..3 {
        let mut c = srv.connect();
        c.send(b"GET big\nGET big\n");
        assert_eq!(c.line_str(), format!("OK {}\n", big.len()));
        c.exact(10);
    }
    let mut c = srv.connect();
    assert!(c.stats().starts_with("gets=3 hits=3 puts=1 "));
    assert!(srv.logs.lines().is_empty());
}

/// H9: a pack shorter than the index says; the header and what there is are sent, then the
/// daemon hangs up.
#[test]
fn short_pack_hangs_up() {
    let srv = InProc::start(1);
    let mut c = srv.connect();
    c.put(b"k", &[b'v'; 100]);
    for e in std::fs::read_dir(srv.root().join("packs")).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "pack") {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&p)
                .unwrap()
                .set_len(50)
                .unwrap();
        }
    }
    c.send(b"GET k\nHAS k\n");
    c.half_close();
    let (got, end) = c.rest();
    // record header 8 bytes, key 1 byte: 41 value bytes left
    assert_eq!(
        show(&got),
        show(&[b"OK 100\n".as_slice(), &[b'v'; 41]].concat())
    );
    assert_eq!(end, End::Eof);
}

/// Q-DAEMON-8: off Linux every peer is served (`peer_other.go`), whatever its uid.
#[cfg(not(target_os = "linux"))]
#[test]
fn every_peer_is_served_off_linux() {
    let me = rustix::process::getuid().as_raw();
    let srv = InProc::with_auth(
        1,
        Auth {
            uid: me.wrapping_add(1),
            nixbld: -1,
        },
    );
    assert!(srv.connect().stats().starts_with("gets=0 "));
}

/// H1: a peer that is neither the daemon's user, root, nixbld nor an auto-allocated uid is
/// closed on without a byte read.
#[cfg(target_os = "linux")]
#[test]
fn foreign_peer_is_refused() {
    let me = rustix::process::getuid().as_raw();
    if me == 0
        || (jigd::peer::AUTO_UIDS..jigd::peer::AUTO_UIDS + jigd::peer::AUTO_COUNT).contains(&me)
    {
        eprintln!("note: running as uid {me}, which is always allowed; skipping");
        return;
    }
    let srv = InProc::with_auth(
        1,
        Auth {
            uid: me + 1,
            nixbld: -1,
        },
    );
    let mut c = srv.connect();
    c.send(b"STATS\n");
    let (got, end) = c.rest();
    assert_eq!(show(&got), "");
    assert_ne!(end, End::Open);
    // and its own group is enough
    let gid = rustix::process::getgid().as_raw();
    let srv = InProc::with_auth(
        1,
        Auth {
            uid: me + 1,
            nixbld: i64::from(gid),
        },
    );
    assert!(srv.connect().stats().starts_with("gets=0 "));
}

/// The stats line is the Go format, counters included.
#[test]
fn stats_counts() {
    let srv = InProc::start(4);
    std::fs::write(srv.root().join("h.h"), b"abc").unwrap();
    let mut c = srv.connect();
    c.put(b"a", b"xyz");
    assert_eq!(c.get(b"a"), Some(b"xyz".to_vec()));
    assert_eq!(c.get(b"b"), None);
    let mut req = b"HAS a\nHAS b\nSLOT q\nIDS 2\n".to_vec();
    req.extend_from_slice(&path(&srv, "h.h"));
    req.push(b'\n');
    req.extend_from_slice(&path(&srv, "h.h"));
    req.push(b'\n');
    c.send(&req);
    for want in [
        "1",
        "0",
        "OK",
        "C:6437b3ac38465133ffb63b75273a8db5",
        "C:6437b3ac38465133ffb63b75273a8db5",
    ] {
        assert_eq!(c.line_str(), format!("{want}\n"));
    }
    assert_eq!(
        c.stats(),
        "gets=2 hits=1 puts=1 ids=1 slots=1 waiting=0 keys=1 packs=1 bytes=12 live=3"
    );
}

/// A connection is served on its own: one blocked in SLOT does not hold up another.
#[test]
fn connections_are_independent() {
    let srv = InProc::start(1);
    let mut a = srv.connect();
    a.send(b"SLOT b\n");
    assert_eq!(a.line(), b"OK\n");
    let mut b = srv.connect();
    b.send(b"SLOT b\n");
    let mut c = Client::connect(&srv.sock);
    c.put(b"k", b"v");
    assert_eq!(c.get(b"k"), Some(b"v".to_vec()));
    assert!(c.stats().contains(" slots=1 waiting=1 "));
    assert!(b.quiet());
}
