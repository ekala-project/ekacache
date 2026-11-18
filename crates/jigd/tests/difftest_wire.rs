//! The Rust jigd against the Go one, over the wire (implementation/03 §Tests "Difftest",
//! implementation/12 §Differential tests): the same scripted client transcripts, each on a
//! fresh pair of daemons, must give the same reply bytes, the same hang-up points (and whether
//! the daemon left request bytes unread), the same exit status and the same log lines with
//! timestamps stripped. The scripts cover H2-H10 (H1 needs a second uid and is covered in
//! `serve.rs`), bad numbers, `IDS 1048577`, `PUT k -1`, Unicode separators, IDS over a store
//! of symlinks, spooled values and slots across connections.
//!
//! Rust-only, and normalised away: the adopted HELLO (Rust answers `OK 1` and goes on, Go hangs
//! up as on any unknown verb, H4) and the `tier` log lines (adopted Q-DAEMON-1). Command-line
//! and startup failures are compared too.
//!
//! Needs the Go binary: `JIG_LEGACY_JIGD=/path/to/jigd`. Without it each test passes and says
//! it skipped.

// helpers outside #[test] fns: a panic is the failure report
#![allow(clippy::unwrap_used)]

mod common;

use std::collections::BTreeMap;
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use common::{Client, Daemon, End, legacy_jigd, log_lines, run, rust_jigd, show};

/// One client action in a script. Connections are numbered and opened on first use.
#[derive(Clone)]
enum Step {
    Send(usize, Vec<u8>),
    /// `shutdown(SHUT_WR)`
    Shut(usize),
    Close(usize),
    /// read one reply line (or record the hang-up)
    Line(usize),
    /// read exactly n bytes (or record the hang-up)
    Exact(usize, usize),
    /// read until the daemon closes
    Rest(usize),
    /// record that nothing arrives for a while
    Quiet(usize),
    /// truncate every pack file in tier 0
    Truncate(u64),
}

use Step::{Close, Exact, Line, Quiet, Rest, Send, Shut, Truncate};

/// What a read step observed.
#[derive(PartialEq, Eq)]
enum Obs {
    Line(usize, Vec<u8>),
    Bytes(usize, Vec<u8>),
    /// the connection ended (bytes that came before the end, and how it ended)
    Ended(usize, Vec<u8>, End),
    Quiet(usize, bool),
}

impl fmt::Debug for Obs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Obs::Line(c, b) => write!(f, "c{c} line \"{}\"", show(b)),
            Obs::Bytes(c, b) => write!(f, "c{c} bytes \"{}\"", show(b)),
            Obs::Ended(c, b, e) => write!(f, "c{c} ended {e:?} after \"{}\"", show(b)),
            Obs::Quiet(c, q) => write!(f, "c{c} quiet {q}"),
        }
    }
}

struct Scenario {
    name: String,
    slots: &'static str,
    steps: Vec<Step>,
}

/// Send `req` on connection `c`, half-close, read everything.
fn one(c: usize, req: &[u8]) -> Vec<Step> {
    vec![Send(c, req.to_vec()), Shut(c), Rest(c)]
}

/// Each request on its own connection, then STATS on one more.
fn each(reqs: &[Vec<u8>]) -> Vec<Step> {
    let mut steps: Vec<Step> = reqs
        .iter()
        .enumerate()
        .flat_map(|(i, r)| one(i, r))
        .collect();
    steps.extend(one(reqs.len(), b"STATS\n"));
    steps
}

fn sc(name: &str, slots: &'static str, steps: Vec<Step>) -> Scenario {
    Scenario {
        name: name.to_owned(),
        slots,
        steps,
    }
}

/// The store IDS answers for, shared by both daemons, and a directory outside it.
struct Fixture {
    _tmp: tempfile::TempDir,
    store: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        let out = tmp.path().join("outside");
        std::fs::create_dir_all(store.join("sub")).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("secret"), b"key").unwrap();
        for (name, body) in [
            (&b"h.h"[..], &b"abc"[..]),
            (b"empty", b""),
            (b"a b", b"x"),
            (b"sub/f", b"abc"),
            ("uni\u{a0}name".as_bytes(), b"u"),
            (b"bin\xff", b"q"),
            (b"h.h\r", b"cr"),
        ] {
            std::fs::write(store.join(std::ffi::OsStr::from_bytes(name)), body).unwrap();
        }
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 249) as u8).collect();
        std::fs::write(store.join("big"), big).unwrap();
        symlink("h.h", store.join("inside")).unwrap();
        symlink("sub/../h.h", store.join("dotted")).unwrap();
        symlink(out.join("secret"), store.join("outlink")).unwrap();
        symlink(&out, store.join("outdir")).unwrap();
        symlink("loop", store.join("loop")).unwrap();
        symlink("nothing", store.join("dangling")).unwrap();
        symlink(store.join("sub"), store.join("subabs")).unwrap();
        std::os::unix::net::UnixListener::bind(store.join("sock")).unwrap();
        Fixture { _tmp: tmp, store }
    }

    /// `$STORE/<rel>` as bytes.
    fn p(&self, rel: &[u8]) -> Vec<u8> {
        let mut v = self.store.as_os_str().as_bytes().to_vec();
        v.extend_from_slice(rel);
        v
    }
}

/// A whole run of one script against one daemon.
#[derive(PartialEq, Eq)]
struct Outcome {
    obs: Vec<Obs>,
    code: Option<i32>,
    log: Vec<String>,
}

impl fmt::Debug for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "exit {:?}", self.code)?;
        for o in &self.obs {
            writeln!(f, "  {o:?}")?;
        }
        for l in &self.log {
            writeln!(f, "  log: {}", show(l.as_bytes()))?;
        }
        Ok(())
    }
}

/// Connection `c`, opened on first use.
fn conn<'a>(conns: &'a mut BTreeMap<usize, Client>, d: &Daemon, c: usize) -> &'a mut Client {
    conns.entry(c).or_insert_with(|| d.connect())
}

fn play(bin: &Path, fx: &Fixture, sc: &Scenario, rust: bool) -> Outcome {
    let root = tempfile::tempdir().unwrap();
    let store = fx.store.to_str().unwrap();
    let daemon = Daemon::start(
        bin,
        root.path(),
        &[("JIGD_SLOTS", sc.slots), ("NIX_STORE_DIR", store)],
    );
    let mut conns: BTreeMap<usize, Client> = BTreeMap::new();
    let mut obs = Vec::new();
    for step in &sc.steps {
        match step {
            Send(c, bytes) => conn(&mut conns, &daemon, *c).send(bytes),
            Shut(c) => conn(&mut conns, &daemon, *c).half_close(),
            Close(c) => {
                conns.remove(c);
            }
            Line(c) => {
                let cl = conn(&mut conns, &daemon, *c);
                obs.push(if let Some(line) = cl.try_line() {
                    Obs::Line(*c, line)
                } else {
                    let (bytes, end) = cl.rest();
                    Obs::Ended(*c, bytes, end)
                });
            }
            Exact(c, n) => {
                let cl = conn(&mut conns, &daemon, *c);
                obs.push(if let Some(bytes) = cl.try_exact(*n) {
                    Obs::Bytes(*c, bytes)
                } else {
                    let (bytes, end) = cl.rest();
                    Obs::Ended(*c, bytes, end)
                });
            }
            Rest(c) => {
                let (bytes, end) = conn(&mut conns, &daemon, *c).rest();
                obs.push(Obs::Ended(*c, bytes, end));
            }
            Quiet(c) => obs.push(Obs::Quiet(*c, conn(&mut conns, &daemon, *c).quiet())),
            Truncate(n) => {
                let packs = root.path().join("cache/jigd/packs");
                for entry in std::fs::read_dir(packs).unwrap() {
                    let path = entry.unwrap().path();
                    if path.extension().is_some_and(|x| x == "pack") {
                        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                        file.set_len(*n).unwrap();
                    }
                }
            }
        }
    }
    drop(conns);
    let (status, log) = daemon.stop();
    let log = log_lines(&log, root.path())
        .into_iter()
        // adopted Q-DAEMON-1: Rust logs each tier's budget after opening the store
        .filter(|l| !(rust && l.starts_with("tier ")))
        .collect();
    Outcome {
        obs,
        code: status.code(),
        log,
    }
}

/// Play every script against both daemons; the Go outcomes by name.
fn compare(go: &Path, fx: &Fixture, scenarios: &[Scenario]) -> BTreeMap<String, Outcome> {
    let rust = rust_jigd();
    let mut failed = Vec::new();
    let mut outcomes = BTreeMap::new();
    for s in scenarios {
        let want = play(go, fx, s, false);
        let got = play(&rust, fx, s, true);
        if got != want {
            eprintln!("--- {}: Go\n{want:?}--- {}: Rust\n{got:?}", s.name, s.name);
            failed.push(s.name.clone());
        }
        outcomes.insert(s.name.clone(), want);
    }
    assert!(failed.is_empty(), "transcripts differ: {failed:?}");
    outcomes
}

fn lines(prefix: &[u8], body: &[Vec<u8>]) -> Vec<u8> {
    let mut v = prefix.to_vec();
    for b in body {
        v.extend_from_slice(b);
        v.push(b'\n');
    }
    v
}

fn put(key: &[u8], val: &[u8]) -> Vec<u8> {
    common::put_req(key, val)
}

fn scenarios(fx: &Fixture) -> Vec<Scenario> {
    let mut all = Vec::new();

    // jigd-protocol.md §Wire examples
    let mut steps = vec![
        Send(0, b"PUT k +3\nabc".to_vec()),
        Line(0),
        Send(0, "GET\u{a0}k\r\n".into()),
        Line(0),
        Exact(0, 3),
        Send(0, b"IDS 0\nHAS k\nHAS z\nDONE b\nSTATS\n".to_vec()),
        Line(0),
        Line(0),
        Line(0),
        Line(0),
    ];
    steps.extend(one(1, b"PUT k -1\n"));
    steps.extend(one(2, b"PUT k 5\nab"));
    steps.extend(one(3, b"PUT k x\n"));
    steps.extend(one(4, b"STATS\n"));
    all.push(sc("wire examples", "2", steps));

    // serve_test.go: IDS in order, pipelined GETs, then the slot handed back by a hang-up
    let mut req = b"PUT k1 3\nabcIDS 2\n".to_vec();
    req.extend(lines(b"", &[fx.p(b"/h.h"), fx.p(b"/missing")]));
    req.extend_from_slice(b"GET nope\nGET k1\nSLOT b1\nDONE b1\nSLOT b1\nSTATS\n");
    let mut steps = vec![Send(0, req)];
    steps.extend((0..5).map(|_| Line(0)));
    steps.extend([Exact(0, 3), Line(0), Line(0), Line(0), Line(0), Close(0)]);
    steps.extend(one(1, b"SLOT b2\nSTATS\n"));
    all.push(sc("pipelined", "1", steps));

    // H2, Q-PROTO-11: the line limit and partial lines
    let mut at = b"GET ".to_vec();
    at.resize(65_535, b'k');
    at.extend_from_slice(b"\nHAS k\n");
    let mut over = b"GET ".to_vec();
    over.resize(65_536, b'k');
    over.extend_from_slice(b"\nHAS k\n");
    let mut exact_full = b"GET ".to_vec();
    exact_full.resize(65_536, b'k');
    all.push(sc(
        "line limit",
        "2",
        each(&[
            at,
            over,
            exact_full,
            vec![b'a'; 70_000],
            vec![b'a'; 200_000],
            b"HAS k\nSTATS".to_vec(),
            Vec::new(),
        ]),
    ));

    // H4: field count, verbs, separators
    let mut h4: Vec<Vec<u8>> = [
        &b"\n"[..],
        b" \t\r\x0b\x0c\n",
        b"GET\n",
        b"GET a b\n",
        b"get k\n",
        b"Get k\n",
        b"STATS x\n",
        b"STATS\x00\n",
        b"FOO\n",
        b"HAS\n",
        b"PUT k\n",
        b"PUT k 1 2\n",
        b"IDS\n",
        b"SLOT\n",
        b"SLOT a b\n",
        b"DONE\n",
        b"GET\xa0k\n",
        b"GET\xc2k\n",
        b"\xffSTATS\n",
    ]
    .iter()
    .map(|r| r.to_vec())
    .collect();
    for sep in ["\u{200b}", "\u{feff}", "\u{2060}", "\u{180e}"] {
        h4.push(format!("GET{sep}k\n").into_bytes());
    }
    all.push(sc("unknown requests", "2", each(&h4)));

    let mut seps = Vec::new();
    for sep in [
        "\t", "\u{b}", "\u{c}", "\r", " ", "\u{85}", "\u{a0}", "\u{1680}", "\u{2000}", "\u{2001}",
        "\u{2002}", "\u{2003}", "\u{2004}", "\u{2005}", "\u{2006}", "\u{2007}", "\u{2008}",
        "\u{2009}", "\u{200a}", "\u{2028}", "\u{2029}", "\u{202f}", "\u{205f}", "\u{3000}",
    ] {
        seps.extend(format!("{sep}HAS{sep}{sep}k\u{a0}{sep}\n").into_bytes());
        seps.extend(format!("PUT{sep}k{sep}1\nx").into_bytes());
    }
    seps.extend_from_slice("\u{3000}STATS\u{85}\n".as_bytes());
    all.push(sc("unicode separators", "2", one(0, &seps)));

    // H5 and number parsing
    all.push(sc(
        "numbers",
        "2",
        each(
            &[
                &b"PUT k x\n"[..],
                b"PUT k 0x3\n",
                b"PUT k 3e0\n",
                b"PUT k ++3\n",
                b"PUT k +-3\n",
                b"PUT k 9223372036854775808\n",
                b"PUT k -9223372036854775809\n",
                b"PUT k 1_0\n",
                b"PUT k \xef\xbc\x93\n",
                b"PUT k 03\nabcGET k\n",
                b"PUT k -0\nGET k\n",
                b"PUT k +0000000000000000000000001\nzGET k\n",
                b"IDS x\n",
                b"IDS -1\n",
                b"IDS 1048577\n",
                b"IDS 99999999999999999999\n",
                b"IDS 1048576\n",
                b"IDS +0\nIDS -0\nIDS 00\nHAS k\n",
                b"IDS +1\n/x\nHAS k\n",
            ]
            .map(<[u8]>::to_vec),
        ),
    ));

    // H6: what the store refuses, logged
    let mut long = b"PUT ".to_vec();
    long.extend(vec![b'k'; 4097]);
    long.extend_from_slice(b" 1\nx");
    let mut max = b"PUT ".to_vec();
    max.extend(vec![b'k'; 4096]);
    max.extend_from_slice(b" 0\nHAS ");
    max.extend(vec![b'k'; 4096]);
    max.push(b'\n');
    all.push(sc(
        "refused puts",
        "2",
        each(&[
            b"PUT k -1\n".to_vec(),
            b"PUT k -9223372036854775808\n".to_vec(),
            b"PUT k 2147483649\n".to_vec(),
            b"PUT k 2147483648\nabc".to_vec(),
            long,
            max,
            b"PUT k 3\n".to_vec(),
            b"PUT k 5\nab".to_vec(),
            [b"PUT spooled 4194304\n".as_slice(), &[1u8; 1000]].concat(),
            [b"PUT spooled 4194303\n".as_slice(), &[1u8; 1000]].concat(),
            b"PUT \xff\x01 1\nx".to_vec(),
            b"PUT \xff\x02 x\n".to_vec(),
        ]),
    ));

    // H7, H8 and IDS answers
    let mut ids5000 = b"IDS 5000\n".to_vec();
    for _ in 0..4500 {
        ids5000.extend_from_slice(b"/nope\n");
    }
    let hh = fx.p(b"/h.h");
    let mut partial = b"IDS 200\n".to_vec();
    for _ in 0..150 {
        partial.extend_from_slice(&hh);
        partial.push(b'\n');
    }
    let mut too_long = b"IDS 2\n".to_vec();
    too_long.extend_from_slice(&hh);
    too_long.push(b'\n');
    too_long.extend(vec![b'/'; 70_000]);
    all.push(sc(
        "ids cut short",
        "2",
        each(&[
            lines(b"IDS 3\n", &[hh.clone(), b"/x".to_vec()]),
            ids5000,
            partial,
            too_long,
        ]),
    ));

    let paths: Vec<Vec<u8>> = [
        &b"/h.h"[..],
        b"/h.h",
        b"/empty",
        b"/a b",
        b"/sub/f",
        "/uni\u{a0}name".as_bytes(),
        b"/bin\xff",
        b"/h.h\r",
        b"/big",
        b"/inside",
        b"/dotted",
        b"/outlink",
        b"/outdir/secret",
        b"/loop",
        b"/dangling",
        b"/subabs/f",
        b"/sock",
        b"/sub",
        b"",
        b"/",
        b"//h.h",
        b"/./h.h",
        b"/sub/../h.h",
        b"/h.h/",
        b"/h.h/x",
        b"/../outside/secret",
        b"x/h.h",
        b"/missing",
    ]
    .iter()
    .map(|r| fx.p(r))
    .chain([
        b"h.h".to_vec(),
        b"/etc/passwd".to_vec(),
        Vec::new(),
        b"\0".to_vec(),
    ])
    .collect();
    let mut req = format!("IDS {}\n", paths.len()).into_bytes();
    req.extend(lines(b"", &paths));
    req.extend_from_slice(b"STATS\n");
    req.extend(lines(b"IDS 2\n", &[fx.p(b"/h.h"), fx.p(b"/big")]));
    req.extend_from_slice(b"STATS\n");
    all.push(sc("ids answers", "2", one(0, &req)));

    // H9: the pack is shorter than the index says
    let mut steps = vec![Send(0, put(b"k", &[b'v'; 100])), Line(0), Truncate(50)];
    steps.extend(one(0, b"GET k\nHAS k\n"));
    steps.extend(one(1, b"HAS k\nSTATS\n"));
    all.push(sc("short pack", "2", steps));

    // values through memory and through the spool
    let mem: Vec<u8> = (0..(1u32 << 20) + 3).map(|i| (i % 251) as u8).collect();
    let spool: Vec<u8> = (0..5_000_000u32).map(|i| (i % 253) as u8).collect();
    let mut req = put(b"mem", &mem);
    req.extend(put(b"spool", &spool));
    req.extend(put(b"mem", b"again"));
    req.extend_from_slice(b"GET spool\nGET mem\nHAS spool\nSTATS\n");
    all.push(sc("large values", "2", one(0, &req)));

    // counters: GET counts, HAS does not, PUT counts once its length parses
    all.push(sc(
        "counters",
        "2",
        one(
            0,
            b"GET a\nHAS a\nPUT a 1\nxGET a\nHAS a\nPUT a 1\nyGET a\nDONE q\nSTATS\n",
        ),
    ));

    // slots: queueing, DONE, hang-up release, fewest held first
    all.push(sc(
        "slots",
        "1",
        vec![
            Send(0, b"SLOT b\n".to_vec()),
            Line(0),
            Send(1, b"SLOT b\n".to_vec()),
            Quiet(1),
            Send(2, b"STATS\n".to_vec()),
            Line(2),
            Send(0, b"DONE b\nDONE b\n".to_vec()),
            Line(0),
            Line(0),
            Line(1),
            Send(2, b"STATS\n".to_vec()),
            Line(2),
            Send(0, b"SLOT c\n".to_vec()),
            Quiet(0),
            Close(1),
            Line(0),
            Send(2, b"STATS\n".to_vec()),
            Line(2),
            Close(0),
            Send(3, b"SLOT d\nSLOT d\n".to_vec()),
            Line(3),
            Quiet(3),
            Send(2, b"STATS\n".to_vec()),
            Line(2),
        ],
    ));
    all.push(sc(
        "fewest held first",
        "2",
        vec![
            Send(0, b"SLOT big\nSLOT big\n".to_vec()),
            Line(0),
            Line(0),
            Send(1, b"SLOT big\n".to_vec()),
            Quiet(1),
            Send(2, b"SLOT small\n".to_vec()),
            Quiet(2),
            Send(3, b"STATS\n".to_vec()),
            Line(3),
            Send(0, b"DONE big\n".to_vec()),
            Line(0),
            Line(2),
            Quiet(1),
            Send(0, b"DONE big\n".to_vec()),
            Line(0),
            Line(1),
            Send(3, b"STATS\n".to_vec()),
            Line(3),
        ],
    ));
    all.push(sc(
        "many tokens",
        "3",
        vec![
            Send(
                0,
                b"SLOT a\nSLOT a\nSLOT b\nDONE c\nDONE a\nDONE a\nDONE a\nSLOT a\nSTATS\n".to_vec(),
            ),
            Line(0),
            Line(0),
            Line(0),
            Line(0),
            Line(0),
            Line(0),
            Line(0),
            Line(0),
            Line(0),
            Send(1, b"SLOT x\nSLOT x\n".to_vec()),
            Line(1),
            Quiet(1),
            Close(0),
            Line(1),
            Send(2, b"STATS\n".to_vec()),
            Line(2),
        ],
    ));

    // H10: readers that go away mid-reply
    let big = vec![9u8; 4 << 20];
    let mut steps = vec![Send(0, put(b"big", &big)), Line(0)];
    for c in 1..4 {
        steps.extend([
            Send(c, b"GET big\nGET big\n".to_vec()),
            Line(c),
            Exact(c, 10),
            Close(c),
        ]);
    }
    steps.extend(one(9, b"HAS big\n"));
    all.push(sc("vanishing readers", "2", steps));

    all
}

#[test]
fn transcripts_match_go() {
    let Some(go) = legacy_jigd() else {
        return;
    };
    let fx = Fixture::new();
    let go = compare(&go, &fx, &scenarios(&fx));
    // the comparison is not vacuous: the Go side says what jigd-protocol.md says it does
    let wire = &go["wire examples"];
    assert_eq!(wire.code, Some(0));
    assert!(wire.obs.contains(&Obs::Line(
        0,
        b"gets=1 hits=1 puts=1 ids=0 slots=0 waiting=0 keys=1 packs=1 bytes=12 live=3\n".to_vec()
    )));
    assert!(wire.obs.contains(&Obs::Ended(
        4,
        b"gets=1 hits=1 puts=3 ids=0 slots=0 waiting=0 keys=1 packs=1 bytes=12 live=3\n".to_vec(),
        End::Eof
    )));
    assert_eq!(
        wire.log,
        [
            "listening on $ROOT/sock, 2 slots, keys=0 packs=1 bytes=0 live=0",
            "put k: bad put",
            "put k: unexpected EOF"
        ]
    );
    let ids = &go["ids cut short"];
    assert!(
        ids.obs
            .contains(&Obs::Ended(1, vec![b'\n'; 4096], End::Eof)),
        "{ids:?}"
    );
    let slots = &go["fewest held first"];
    assert!(slots.obs.contains(&Obs::Quiet(1, true)), "{slots:?}");
}

/// HELLO normalised away: Rust answers `HELLO 1` with `OK 1` and then behaves exactly as Go
/// does without it; Go hangs up on it (H4).
#[test]
fn hello_is_the_only_difference() {
    let Some(go) = legacy_jigd() else {
        return;
    };
    let fx = Fixture::new();
    let store = fx.store.to_str().unwrap();
    let rust = rust_jigd();
    let mut req = b"PUT k 3\nabcGET k\nHAS k\nIDS 1\n".to_vec();
    req.extend_from_slice(&fx.p(b"/h.h"));
    req.extend_from_slice(b"\nSLOT b\nDONE b\nSTATS\nGET");
    let transcript = |bin: &Path, req: &[u8]| {
        let root = tempfile::tempdir().unwrap();
        let d = Daemon::start(
            bin,
            root.path(),
            &[("NIX_STORE_DIR", store), ("JIGD_SLOTS", "2")],
        );
        let mut c = d.connect();
        c.send(req);
        c.half_close();
        let got = c.rest();
        assert!(d.stop().0.success());
        got
    };
    let (want, want_end) = transcript(&go, &req);
    assert!(
        want.ends_with(b"keys=1 packs=1 bytes=12 live=3\n"),
        "{}",
        show(&want)
    );
    let (got, got_end) = transcript(&rust, &[b"HELLO 1\n".as_slice(), &req].concat());
    assert_eq!(show(&got), show(&[b"OK 1\n".as_slice(), &want].concat()));
    assert_eq!(got_end, want_end);
    assert_eq!(transcript(&go, b"HELLO 1\nSTATS\n"), (Vec::new(), End::Eof));
}

/// A startup case: name, arguments, environment.
type Case<'a> = (&'a str, Vec<&'a [u8]>, Vec<(&'a str, &'a [u8])>);

/// Command-line and startup failures: exit status, stdout, and the log (stderr) without
/// timestamps. The adopted strict budgets are not compared (Go ignores a bad value).
#[test]
fn startup_matches_go() {
    let Some(go) = legacy_jigd() else {
        return;
    };
    let rust = rust_jigd();
    let mut failed = Vec::new();
    let abs = format!("@jigd-difftest-{}", std::process::id());
    let cases: Vec<Case> = vec![
        ("--help", vec![b"--help"], vec![]),
        ("-h", vec![b"-h"], vec![]),
        ("no args", vec![], vec![]),
        ("two args", vec![b"a", b"b"], vec![]),
        ("--help x", vec![b"--help", b"x"], vec![]),
        ("no home", vec![b"$ROOT/sock"], vec![]),
        ("empty home", vec![b"$ROOT/sock"], vec![("HOME", b""), ("XDG_CACHE_HOME", b"")]),
        ("abstract", vec![abs.as_bytes()], vec![("XDG_CACHE_HOME", b"$ROOT/cache")]),
        ("long socket", vec![b"$ROOT/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"], vec![("XDG_CACHE_HOME", b"$ROOT/cache")]),
        ("socket under a file", vec![b"$ROOT/file/sock"], vec![("XDG_CACHE_HOME", b"$ROOT/cache")]),
        ("cache under a file", vec![b"$ROOT/sock"], vec![("XDG_CACHE_HOME", b"$ROOT/file/cache")]),
        ("cold under a file", vec![b"$ROOT/sock"], vec![("XDG_CACHE_HOME", b"$ROOT/cache"), ("JIGD_COLD", b"$ROOT/file/cold")]),
    ];
    for (name, args, env) in &cases {
        let mut outs = Vec::new();
        for (bin, is_rust) in [(&go, false), (&rust, true)] {
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("file"), b"").unwrap();
            let r = root.path().as_os_str().as_bytes();
            let sub = |b: &[u8]| common::replace(b, b"$ROOT", r);
            let args: Vec<Vec<u8>> = args.iter().map(|a| sub(a)).collect();
            let args: Vec<&[u8]> = args.iter().map(Vec::as_slice).collect();
            let env: Vec<(&str, Vec<u8>)> = env.iter().map(|(k, v)| (*k, sub(v))).collect();
            let env: Vec<(&str, &[u8])> = env.iter().map(|(k, v)| (*k, v.as_slice())).collect();
            let out = run(bin, &args, &env);
            let log: Vec<String> = log_lines(&out.stderr, root.path())
                .into_iter()
                .filter(|l| !(is_rust && l.starts_with("tier ")))
                .collect();
            outs.push((out.code, show(&out.stdout), log));
        }
        if outs[0] != outs[1] {
            eprintln!("--- {name}\nGo:   {:?}\nRust: {:?}", outs[0], outs[1]);
            failed.push(*name);
        }
    }
    assert!(failed.is_empty(), "startup differs: {failed:?}");
}

/// A running daemon's startup line under several `JIGD_SLOTS` and budget settings that both
/// accept.
#[test]
fn startup_log_matches_go() {
    let Some(go) = legacy_jigd() else {
        return;
    };
    let rust = rust_jigd();
    let mut failed = Vec::new();
    for env in [
        vec![],
        vec![("JIGD_SLOTS", "3")],
        vec![("JIGD_SLOTS", "+3")],
        vec![("JIGD_SLOTS", "0")],
        vec![("JIGD_SLOTS", "-1")],
        vec![("JIGD_SLOTS", " 3")],
        vec![("JIGD_SLOTS", "x")],
        vec![
            ("JIGD_SIZE", "10"),
            ("JIGD_COLD", "$ROOT/cold"),
            ("JIGD_COLD_SIZE", "1"),
        ],
        vec![("NIX_STORE_DIR", "$ROOT/nowhere")],
    ] {
        let mut outs = Vec::new();
        for (bin, is_rust) in [(&go, false), (&rust, true)] {
            let root = tempfile::tempdir().unwrap();
            let r = root.path().to_str().unwrap();
            let env: Vec<(&str, String)> = env
                .iter()
                .map(|(k, v)| (*k, v.replace("$ROOT", r)))
                .collect();
            let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let d = Daemon::start(bin, root.path(), &env);
            let stats = d.connect().stats();
            let (status, log) = d.stop();
            let log: Vec<String> = log_lines(&log, root.path())
                .into_iter()
                .filter(|l| !(is_rust && l.starts_with("tier ")))
                .collect();
            outs.push((status.code(), stats, log));
        }
        if outs[0] != outs[1] {
            eprintln!("--- {env:?}\nGo:   {:?}\nRust: {:?}", outs[0], outs[1]);
            failed.push(format!("{env:?}"));
        }
    }
    assert!(failed.is_empty(), "startup differs: {failed:?}");
}
