//! `jig nix-store` online, against a scripted fake builder-rpc daemon
//! (`behaviors/jig-nix-store.md §Worker protocol`, `§Client state-transition table`, `§Wire
//! examples`; `spec/builder_rpc.qnt` scenarios; Q-NIX-2/6/7/8/9/10/11/17). One test per row of
//! the state table and of the drain sub-machine. With `JIG_LEGACY_JIG` set, every run is
//! repeated with the legacy jig and the client→daemon bytes, stdout, stderr and exit status or
//! signal must be identical.
#![allow(clippy::unwrap_used)]

mod nix_store_support;

use ekacache_testkit::vectors::VectorFile;

use std::time::Duration;

use nix_store_support::{
    Inv, JIG, LAST, MAX_STRING, NEXT, Outcome, RESULT, Remote, Row, Running, START_ACTIVITY,
    STOP_ACTIVITY, Script, Step, V1_38, add_reply, bad_arguments, cat, check, client_add,
    client_features, client_handshake, client_hello, client_post, client_submit, daemon_hello,
    daemon_info, did_not_parse, error, fetch_one, fetch_one_aterm, handshake, handshake_with,
    legacy, next, not_an_array, path_info, strace, string, strings, usage, word,
};

const BAD_MAGIC: &[u8] = b"jig nix-store: bad daemon magic";
const HELLO_TXT: &[u8] = b"/nix/store/q790zdjk75hm2cn42nh77pqw4gbv1b88-hello.txt";
/// What the fake daemon answers for an added object.
const DAEMON_PATH: &[u8] = b"/nix/store/00000000000000000000000000000000-from-daemon";

fn add_hello() -> Inv {
    Inv::new(&[&b"add-text"[..], b"hello.txt"]).stdin(b"hello")
}

/// The 93-byte AddToStore of `hello` as `hello.txt`.
fn hello_request() -> Vec<u8> {
    client_add(b"hello.txt", &[], b"hello")
}

/// A handshake, then AddToStore of `hello.txt` answered with `reply`.
fn hello_script(reply: &[u8]) -> Script {
    add_reply(hello_request().len(), reply)
}

fn ok_reply(path: &[u8]) -> Vec<u8> {
    cat(&[&word(LAST), &path_info(path)])
}

/// The handshake with `ShutRd` just before the daemon's last send: the client's next flush
/// fails.
fn shut_before_last_send(mut s: Script) -> Script {
    let last = s
        .iter()
        .rposition(|st| matches!(st, Step::Send(_)))
        .unwrap();
    s.insert(last, Step::ShutRd);
    s
}

#[track_caller]
fn assert_sent(o: &Outcome, want: &[u8]) {
    assert!(o.connected, "{o:?}");
    assert_eq!(o.sent, want, "{o:?}");
}

// ---- §Client state-transition table, one test per row ------------------------------------

/// `Start`: no arguments → usage, before anything else.
#[test]
fn row_start() {
    let o = check(&Inv::new::<&[u8]>(&[]), &handshake());
    o.assert_fails(2, &[usage()]);
    assert!(!o.connected);
}

/// `Opening`: an empty `JIG_NIX_STORE_OFFLINE` is online (Q-NIX-3), non-empty is offline.
#[test]
fn row_opening() {
    let o = check(
        &add_hello().env("JIG_NIX_STORE_OFFLINE", b""),
        &hello_script(&ok_reply(DAEMON_PATH)),
    );
    o.assert_prints(DAEMON_PATH);
    assert_sent(&o, &cat(&[&client_handshake(), &hello_request()]));
    let o = check(
        &add_hello().env("JIG_NIX_STORE_OFFLINE", b"0"),
        &hello_script(&ok_reply(DAEMON_PATH)),
    );
    o.assert_prints(HELLO_TXT);
    assert!(!o.connected);
}

/// `ParseRemote`: only the exact prefix `unix://` (Q-NIX-11); also for a bad verb (Q-NIX-2).
#[test]
fn row_parse_remote() {
    let msg = |v: &[u8]| {
        cat(&[
            b"jig nix-store: NIX_REMOTE=",
            v,
            b" is not a unix:// socket (builder-rpc-v0 missing?)",
        ])
    };
    let o = check(&add_hello().remote(Remote::Unset), &handshake());
    o.assert_fails(1, &[&msg(b"")]);
    assert!(!o.connected);
    for v in [
        &b""[..],
        b"daemon",
        b"auto",
        b"unix:/tmp/sock",
        b"UNIX:///tmp/sock",
        b"unix//x",
        b" unix:///tmp/sock",
        b"ssh-ng://host",
        b"\xff\n",
    ] {
        let rows: &[&[&[u8]]] = &[&[b"add-text", b"x"], &[b"bogus"]];
        for args in rows {
            let o = check(&Inv::new(args).remote(Remote::Value(v.to_vec())), &[]);
            o.assert_fails(1, &[&msg(v)]);
        }
    }
}

/// `CheckPath`: a path of `sizeof(sun_path)` bytes or more fails silently (Q-NIX-9).
#[test]
fn row_check_path() {
    for len in [108, 109, 4096] {
        let path = cat(&[b"/", &vec![b'x'; len - 1]]);
        let o = check(
            &add_hello().remote(Remote::Value(cat(&[b"unix://", &path]))),
            &[],
        );
        o.assert_fails(1, &[]);
    }
    // one shorter: tried, and the connection fails
    let path = cat(&[b"/", &[b'x'; 106]]);
    let o = check(
        &add_hello().remote(Remote::Value(cat(&[b"unix://", &path]))),
        &[],
    );
    o.assert_fails(1, &[&cat(&[b"jig nix-store: cannot connect to ", &path])]);
}

/// `Connecting`: the path verbatim (Q-NIX-11), relative to the cwd; the empty path (Q-NIX-12).
#[test]
fn row_connecting() {
    for path in [
        &b"/nonexistent/sock"[..],
        b"/dev/null",
        b"",
        b"sock?x=1#f",
        b"%2Ftmp%2Fsock",
        b"\xff",
    ] {
        let o = check(
            &add_hello()
                .remote(Remote::Value(cat(&[b"unix://", path])))
                .in_dir(),
            &handshake(),
        );
        o.assert_fails(1, &[&cat(&[b"jig nix-store: cannot connect to ", path])]);
        assert!(!o.connected);
    }
    // relative to the cwd, where the daemon listens
    let o = check(
        &add_hello()
            .remote(Remote::Value(b"unix://sock".to_vec()))
            .in_dir(),
        &hello_script(&ok_reply(DAEMON_PATH)),
    );
    o.assert_prints(DAEMON_PATH);
}

/// `SendHello`: the first flush fails. The daemon must have hung up before the client's first
/// write, so `connect` is delayed under `strace` while the daemon accepts and shuts down.
#[test]
fn row_send_hello() {
    if strace().is_none() {
        eprintln!("strace with fault injection is unavailable: row_send_hello not run");
        return;
    }
    let inv = add_hello().delay_connect();
    // Q-NIX-7: default disposition → SIGPIPE
    let o = check(&inv, &[Step::ShutRd]);
    o.assert_killed(libc::SIGPIPE);
    // Q-NIX-10: ignored → EPIPE, reported as a bad magic
    let o = check(&inv.sigpipe_ignored(), &[Step::ShutRd]);
    o.assert_fails(1, &[BAD_MAGIC]);
    assert_sent(&o, b"");
}

/// `AwaitMagic`: a wrong word, a short read or EOF (Q-NIX-10); stdin is not read yet.
#[test]
fn row_await_magic() {
    for reply in [
        word(0),
        word(nix_store_support::MAGIC_1),
        word(nix_store_support::MAGIC_2 | 1 << 32),
        b"oixd".to_vec(),
        Vec::new(),
    ] {
        let o = check(
            &add_hello().held_open(),
            &[Step::Recv(16), Step::Send(reply)],
        );
        o.assert_fails(1, &[BAD_MAGIC]);
        assert_sent(&o, &client_hello());
    }
}

/// `AwaitVersion`: below 1.38 or unreadable is too old, printed in decimal (Q-NIX-10); anything
/// from 1.38 up is accepted.
#[test]
fn row_await_version() {
    let too_old = |v: &[u8]| cat(&[b"jig nix-store: daemon protocol ", v, b" too old"]);
    for (reply, n) in [
        (word(0x125), &b"293"[..]),
        (word(0), b"0"),
        (word(0x100), b"256"),
        (word(nix_store_support::MAGIC_2), &[][..]),
        (Vec::new(), b"0"),
        (vec![0x26, 0x01], b"0"),
    ] {
        let mut bytes = word(nix_store_support::MAGIC_2);
        bytes.extend_from_slice(&reply);
        let o = check(&add_hello(), &[Step::Recv(16), Step::Send(bytes)]);
        if n.is_empty() {
            // 0x6478696f is >= 0x126: accepted, and the handshake goes on to EOF
            o.assert_fails(1, &[]);
            assert_sent(&o, &cat(&[&client_hello(), &client_features()]));
        } else {
            o.assert_fails(1, &[&too_old(n)]);
            assert_sent(&o, &client_hello());
        }
    }
    for v in [0x126, 0x127, 0x200, u64::MAX] {
        let mut s = hello_script(&ok_reply(DAEMON_PATH));
        s[1] = Step::Send(daemon_hello(v));
        let o = check(&add_hello(), &s);
        o.assert_prints(DAEMON_PATH);
        // the client keeps behaving as 1.38
        assert_sent(&o, &cat(&[&client_handshake(), &hello_request()]));
    }
}

/// `SendFeatures`: the flush fails after the version was accepted (Q-NIX-7, Q-NIX-9).
#[test]
fn row_send_features() {
    let s = vec![
        Step::Recv(16),
        Step::ShutRd,
        Step::Send(daemon_hello(V1_38)),
    ];
    check(&add_hello(), &s).assert_killed(libc::SIGPIPE);
    let o = check(&add_hello().sigpipe_ignored(), &s);
    o.assert_fails(1, &[]);
    assert_sent(&o, &client_hello());
}

/// `AwaitFeatures`: any list is discarded; a short or oversized one fails silently.
#[test]
fn row_await_features() {
    let prefix = || {
        vec![
            Step::Recv(16),
            Step::Send(daemon_hello(V1_38)),
            Step::Recv(136),
        ]
    };
    for bad in [
        Vec::new(),
        vec![1, 0, 0],
        word(1),
        cat(&[&word(2), &string(b"submit-output")]),
        cat(&[&word(1), &word(MAX_STRING + 1), b"x"]),
        cat(&[&word(1), &word(3), b"ab"]),
    ] {
        let mut s = prefix();
        s.push(Step::Send(bad));
        let o = check(&add_hello(), &s);
        o.assert_fails(1, &[]);
        assert_sent(&o, &cat(&[&client_hello(), &client_features()]));
    }
    // the daemon needs no feature, and unknown ones are fine
    let mut s = hello_script(&ok_reply(DAEMON_PATH));
    s[3] = Step::Send(strings(&[b"", b"x\xff", b"realisation-with-path-not-hash"]));
    check(&add_hello(), &s).assert_prints(DAEMON_PATH);
    s[3] = Step::Send(strings(&[]));
    check(&add_hello(), &s).assert_prints(DAEMON_PATH);
}

/// `SendPost`: the third flush fails.
#[test]
fn row_send_post() {
    let s = vec![
        Step::Recv(16),
        Step::Send(daemon_hello(V1_38)),
        Step::Recv(136),
        Step::ShutRd,
        Step::Send(strings(&[])),
    ];
    check(&add_hello(), &s).assert_killed(libc::SIGPIPE);
    let o = check(&add_hello().sigpipe_ignored(), &s);
    o.assert_fails(1, &[]);
    assert_sent(&o, &cat(&[&client_hello(), &client_features()]));
}

/// `AwaitInfo`: the version string and trust word; any trust value is accepted.
#[test]
fn row_await_info() {
    for bad in [
        Vec::new(),
        string(b"2.99")[..10].to_vec(),
        string(b"2.99"),
        cat(&[&string(b"2.99"), &[1, 0, 0]]),
        cat(&[&word(MAX_STRING + 1), &word(1)]),
    ] {
        let mut s = handshake();
        s[5] = Step::Send(bad);
        let o = check(&add_hello(), &s);
        o.assert_fails(1, &[]);
        assert_sent(&o, &client_handshake());
    }
    for trust in [0, 2, u64::MAX] {
        let mut s = hello_script(&ok_reply(DAEMON_PATH));
        s[5] = Step::Send(cat(&[&string(b""), &word(trust), &word(LAST)]));
        check(&add_hello(), &s).assert_prints(DAEMON_PATH);
    }
}

/// `DrainStartup`: the drain loop's messages end the open with exit 1, even for a bad verb.
#[test]
fn row_drain_startup() {
    let rows: &[&[&[u8]]] = &[&[b"add-text", b"x"], &[b"bogus"]];
    for args in rows {
        let mut s = handshake();
        s[5] = Step::Send(cat(&[&daemon_info(), &error(b"not allowed")]));
        let o = check(&Inv::new(args).held_open(), &s);
        o.assert_fails(1, &[b"nix-daemon: not allowed"]);
        assert_sent(&o, &client_handshake());
    }
    let mut s = handshake();
    s[5] = Step::Send(daemon_info());
    check(&add_hello(), &s).assert_fails(1, &[]);
}

/// `Ready`: the verb and argument count are checked only after the handshake (Q-NIX-2); then
/// the connection closes without an op, and stdin is never read.
#[test]
fn row_ready() {
    let rows: &[&[&[u8]]] = &[
        &[b"bogus"],
        &[b"--help"],
        &[b"add-text"],
        &[b"add-drv"],
        &[b"fetchurls", b"x"],
        &[b"submit", b"/nix/store/x"],
        &[b"submit", b"a", b"b", b"c"],
    ];
    for args in rows {
        let o = check(&Inv::new(args).held_open(), &handshake());
        o.assert_fails(2, &[bad_arguments()]);
        assert_sent(&o, &client_handshake());
    }
    // an unreachable daemon: the connection error, exit 1, never "bad arguments"
    let o = check(
        &Inv::new(&[b"bogus"]).remote(Remote::Value(b"unix:///nonexistent".to_vec())),
        &[],
    );
    o.assert_fails(1, &[b"jig nix-store: cannot connect to /nonexistent"]);
}

/// `ReadInput`: stdin after the handshake; conversion failures send no op.
#[test]
fn row_read_input() {
    let rows: &[Row<'_>] = &[
        (&[b"add-drv", b"x.drv"], b"abc", did_not_parse()),
        (
            &[b"add-drv", b"x.drv"],
            br#"{"env":{"a":1}}"#,
            did_not_parse(),
        ),
        (&[b"fetchurls"], b"{}", not_an_array()),
        (&[b"fetchurls"], b"", not_an_array()),
    ];
    for (args, stdin, msg) in rows {
        let o = check(&Inv::new(args).stdin(stdin), &handshake());
        o.assert_fails(1, &[msg]);
        assert_sent(&o, &client_handshake());
    }
    // Q-NIX-4: type errors abort, also with no op sent
    let rows: &[(&[&[u8]], &[u8])] = &[
        (&[b"add-drv", b"x.drv"], br#"{"system":1}"#),
        (&[b"fetchurls"], b"[1]"),
        (&[b"fetchurls"], br#"[{"sri":1}]"#),
    ];
    for (args, stdin) in rows {
        let o = check(&Inv::new(args).stdin(stdin), &handshake());
        o.assert_killed(libc::SIGABRT);
        assert_sent(&o, &client_handshake());
    }
    // an empty array: no op, `[]`
    let o = check(&Inv::new(&[b"fetchurls"]).stdin(b"[]"), &handshake());
    o.assert_prints(b"[]");
    assert_sent(&o, &client_handshake());
}

/// `SendAdd`: the op's flush fails.
#[test]
fn row_send_add() {
    let s = shut_before_last_send(handshake());
    check(&add_hello(), &s).assert_killed(libc::SIGPIPE);
    let o = check(&add_hello().sigpipe_ignored(), &s);
    o.assert_fails(1, &[]);
    assert_sent(&o, &client_handshake());
}

/// `DrainAdd`: stderr lines are relayed; the drain's failures end the op.
#[test]
fn row_drain_add() {
    let reply = cat(&[&next(b"building"), &next(b""), &ok_reply(DAEMON_PATH)]);
    let o = check(&add_hello(), &hello_script(&reply));
    assert_eq!(o.code, Some(0), "{o:?}");
    assert_eq!(o.stdout, cat(&[DAEMON_PATH, b"\n"]));
    assert_eq!(o.stderr, b"building\n\n");
    for (reply, msg) in [
        (
            error(b"path is invalid"),
            &b"nix-daemon: path is invalid"[..],
        ),
        (
            word(0x6461_7461),
            b"jig nix-store: unexpected daemon message 0x64617461",
        ),
        (Vec::new(), b""),
    ] {
        let o = check(&add_hello(), &hello_script(&reply));
        let msgs: &[&[u8]] = if msg.is_empty() { &[] } else { &[msg] };
        o.assert_fails(1, msgs);
        assert_sent(&o, &cat(&[&client_handshake(), &hello_request()]));
    }
}

/// `AwaitPathInfo`: nine fields; any truncation fails silently (Q-NIX-9); the path is not
/// checked.
#[test]
fn row_await_path_info() {
    let info = path_info(DAEMON_PATH);
    // field boundaries, and a cut inside each field
    let fields = [
        string(DAEMON_PATH).len(),
        8,
        string(b"sha256:0000000000000000000000000000000000000000000000000000").len(),
        8,
        8,
        8,
        8,
        8,
        string(b"text:sha256:0000000000000000000000000000000000000000000000000000").len(),
    ];
    assert_eq!(fields.iter().sum::<usize>(), info.len());
    let mut at = 0;
    for f in fields {
        for cut in [at, at + 3] {
            let o = check(
                &add_hello(),
                &hello_script(&cat(&[&word(LAST), &info[..cut]])),
            );
            o.assert_fails(1, &[]);
        }
        at += f;
    }
    check(&add_hello(), &hello_script(&cat(&[&word(LAST), &info]))).assert_prints(DAEMON_PATH);
    // whatever the daemon returns is printed, raw
    for path in [&b"not a store path"[..], b"", b"\xff\x00\n\"x"] {
        check(&add_hello(), &hello_script(&ok_reply(path))).assert_prints(path);
    }
    // non-empty lists in the reply
    let info = cat(&[
        &string(DAEMON_PATH),
        &string(b"/nix/store/x.drv"),
        &string(b"h"),
        &strings(&[b"/r1", b"/r2"]),
        &word(1),
        &word(2),
        &word(1),
        &strings(&[b"sig"]),
        &string(b""),
    ]);
    check(&add_hello(), &hello_script(&cat(&[&word(LAST), &info]))).assert_prints(DAEMON_PATH);
}

fn submit() -> Inv {
    Inv::new(&[&b"submit"[..], b"/nix/store/abc-x.drv", b"out"])
}

fn submit_script(reply: &[u8]) -> Script {
    add_reply(64, reply)
}

/// `SendSubmit`: the op's flush fails.
#[test]
fn row_send_submit() {
    let s = shut_before_last_send(handshake());
    check(&submit(), &s).assert_killed(libc::SIGPIPE);
    let o = check(&submit().sigpipe_ignored(), &s);
    o.assert_fails(1, &[]);
    assert_sent(&o, &client_handshake());
}

/// `DrainSubmit`: the drain's messages.
#[test]
fn row_drain_submit() {
    let o = check(
        &submit(),
        &submit_script(&error(b"output out: no such path")),
    );
    o.assert_fails(1, &[b"nix-daemon: output out: no such path"]);
    assert_sent(
        &o,
        &cat(&[
            &client_handshake(),
            &client_submit(b"/nix/store/abc-x.drv", b"out"),
        ]),
    );
    let reply = cat(&[&next(b"registering"), &word(LAST), &word(1)]);
    let o = check(&submit(), &submit_script(&reply));
    assert_eq!(o.code, Some(0), "{o:?}");
    assert_eq!(o.stderr, b"registering\n");
    assert!(o.stdout.is_empty());
}

/// `AwaitSubmitResult`: one word, any value; EOF fails silently.
#[test]
fn row_await_submit_result() {
    for reply in [word(LAST), cat(&[&word(LAST), &[1, 2, 3]])] {
        check(&submit(), &submit_script(&reply)).assert_fails(1, &[]);
    }
    for v in [0, 1, u64::MAX] {
        let o = check(&submit(), &submit_script(&cat(&[&word(LAST), &word(v)])));
        o.assert_fails(0, &[]);
        assert_sent(
            &o,
            &cat(&[
                &client_handshake(),
                &client_submit(b"/nix/store/abc-x.drv", b"out"),
            ]),
        );
    }
    // both arguments verbatim, unvalidated
    let request = client_submit(b"", b"\xff\n");
    let o = check(
        &Inv::new(&[&b"submit"[..], b"", b"\xff\n"]),
        &add_reply(request.len(), &cat(&[&word(LAST), &word(0)])),
    );
    o.assert_fails(0, &[]);
    assert_sent(&o, &cat(&[&client_handshake(), &request]));
}

/// `Done`: fetchurls prints the daemon's `.drv` paths with the locally computed `out`, through
/// `dump()` (escaped; invalid UTF-8 aborts).
#[test]
fn row_done() {
    let two = cat(&[
        &fetch_one()[..fetch_one().len() - 1],
        b",",
        &fetch_one()[1..],
    ]);
    let request = client_add(b"foo-1.0.tar.gz.drv", &[], fetch_one_aterm());
    let out = b"/nix/store/hq2j18s54yzc9ahyw79v8zdn5pya2ca9-foo-1.0.tar.gz";
    let script = |d1: &[u8], d2: &[u8]| {
        let mut s = add_reply(request.len(), &ok_reply(d1));
        s.push(Step::Recv(request.len()));
        s.push(Step::Send(ok_reply(d2)));
        s
    };
    let inv = Inv::new(&[b"fetchurls"]).stdin(&two);
    let o = check(&inv, &script(b"/d/1.drv", b"x\"\\\x01\x1f\x7f\xc3\xa9/"));
    let want = cat(&[
        br#"[{"drv":"/d/1.drv","out":""#,
        out,
        br#""},{"drv":"x\"\\\u0001\u001f"#,
        b"\x7f\xc3\xa9/",
        br#"","out":""#,
        out,
        br#""}]"#,
    ]);
    o.assert_prints(&want);
    assert_sent(&o, &cat(&[&client_handshake(), &request, &request]));
    // a daemon path that is not UTF-8: dump()'s strict check aborts, after both ops
    let o = check(&inv, &script(b"/d/1.drv", b"/d/\xff.drv"));
    o.assert_killed(libc::SIGABRT);
    assert_sent(&o, &cat(&[&client_handshake(), &request, &request]));
}

/// Q-NIX-17: a failure on element k prints nothing; the earlier ops were sent and completed.
#[test]
fn fetchurls_partial_failure() {
    let two = cat(&[
        &fetch_one()[..fetch_one().len() - 1],
        b",",
        &fetch_one()[1..],
    ]);
    let request = client_add(b"foo-1.0.tar.gz.drv", &[], fetch_one_aterm());
    let mut s = add_reply(request.len(), &ok_reply(b"/d/1.drv"));
    s.push(Step::Recv(request.len()));
    s.push(Step::Send(error(b"no")));
    let o = check(&Inv::new(&[b"fetchurls"]).stdin(&two), &s);
    o.assert_fails(1, &[b"nix-daemon: no"]);
    assert_sent(&o, &cat(&[&client_handshake(), &request, &request]));
    // an abort on the second element, after the first op
    let json = cat(&[&fetch_one()[..fetch_one().len() - 1], b",1]"]);
    let o = check(
        &Inv::new(&[b"fetchurls"]).stdin(&json),
        &add_reply(request.len(), &ok_reply(b"/d/1.drv")),
    );
    o.assert_killed(libc::SIGABRT);
    assert_sent(&o, &cat(&[&client_handshake(), &request]));
}

/// `ReadyOffline`: nothing is contacted (the rest is in `nix_store_offline.rs`).
#[test]
fn row_ready_offline() {
    let o = check(&add_hello().offline(), &handshake());
    o.assert_prints(HELLO_TXT);
    assert!(!o.connected);
}

// ---- the drain sub-machine (in the startup drain) -----------------------------------------

/// A handshake whose startup drain is `drain` (no `STDERR_LAST` appended), then `add-text`.
fn drain_script(drain: &[u8]) -> Script {
    let mut s = handshake();
    s[5] = Step::Send(cat(&[&daemon_info(), drain]));
    s
}

/// A startup drain of `drain` + `STDERR_LAST`, then a successful add.
fn drain_ok_script(drain: &[u8]) -> Script {
    let mut s = handshake_with(drain);
    s.push(Step::Recv(hello_request().len()));
    s.push(Step::Send(ok_reply(DAEMON_PATH)));
    s
}

/// `D.Code`: EOF is silent; an unknown code prints it in `{:#x}` form.
#[test]
fn drain_code() {
    check(&add_hello(), &drain_script(b"")).assert_fails(1, &[]);
    check(&add_hello(), &drain_script(&[0x73, 0x74])).assert_fails(1, &[]);
    for (code, hex) in [
        (0x6461_7461, &b"0x64617461"[..]),
        (0x6461_7416, b"0x64617416"),
        (0, b"0x0"),
        (7, b"0x7"),
        (u64::MAX, b"0xffffffffffffffff"),
        (LAST << 32, b"0x616c747300000000"),
    ] {
        let o = check(&add_hello(), &drain_script(&word(code)));
        o.assert_fails(
            1,
            &[&cat(&[b"jig nix-store: unexpected daemon message ", hex])],
        );
        assert_sent(&o, &client_handshake());
    }
}

/// `D.Next`: the text verbatim plus `\n`; a failed read prints an empty line (Q-NIX-8).
#[test]
fn drain_next() {
    let lines = cat(&[
        &next(b"hello"),
        &next(b""),
        &next(b"two\nlines"),
        &next(b"\xff\x00\r"),
    ]);
    let o = check(&add_hello(), &drain_ok_script(&lines));
    assert_eq!(o.code, Some(0), "{o:?}");
    assert_eq!(o.stderr, b"hello\n\ntwo\nlines\n\xff\x00\r\n");
    assert_eq!(o.stdout, cat(&[DAEMON_PATH, b"\n"]));
    // truncated: an empty line, then EOF at the next code
    let o = check(
        &add_hello(),
        &drain_script(&cat(&[&word(NEXT), &word(10), b"abc"])),
    );
    o.assert_fails(1, &[b""]);
}

/// `D.Start`: six fields, with logger fields of both kinds, all ignored.
#[test]
fn drain_start_activity() {
    let fields = cat(&[
        &word(4),
        &word(1),
        &string(b"text field"),
        &word(0),
        &word(42),
        &word(1),
        &string(b""),
        &word(9),
        &word(7),
    ]);
    let start = cat(&[
        &word(START_ACTIVITY),
        &word(1),
        &word(3),
        &word(105),
        &string(b"building '/nix/store/x.drv'"),
        &fields,
        &word(0),
    ]);
    let o = check(&add_hello(), &drain_ok_script(&start));
    o.assert_prints(DAEMON_PATH);
    // with no fields
    let start = cat(&[
        &word(START_ACTIVITY),
        &word(1),
        &word(0),
        &word(0),
        &string(b""),
        &word(0),
        &word(0),
    ]);
    check(&add_hello(), &drain_ok_script(&start)).assert_prints(DAEMON_PATH);
    // truncated inside: the failures are ignored, then EOF at the next code
    let o = check(
        &add_hello(),
        &drain_script(&cat(&[&word(START_ACTIVITY), &word(1)])),
    );
    o.assert_fails(1, &[]);
}

/// `D.Stop`: one word.
#[test]
fn drain_stop_activity() {
    let stop = cat(&[&word(STOP_ACTIVITY), &word(1)]);
    check(&add_hello(), &drain_ok_script(&stop)).assert_prints(DAEMON_PATH);
    check(&add_hello(), &drain_script(&word(STOP_ACTIVITY))).assert_fails(1, &[]);
}

/// `D.Result`: two words and fields.
#[test]
fn drain_result() {
    let result = cat(&[
        &word(RESULT),
        &word(1),
        &word(101),
        &word(2),
        &word(1),
        &string(b"progress"),
        &word(0),
        &word(5),
    ]);
    check(&add_hello(), &drain_ok_script(&result)).assert_prints(DAEMON_PATH);
    // a finite count at EOF: the loop ends and the next code fails
    let o = check(
        &add_hello(),
        &drain_script(&cat(&[&word(RESULT), &word(1), &word(101), &word(3)])),
    );
    o.assert_fails(1, &[]);
}

/// `D.Error`: `msg` after three fields whose read failures are ignored; `?` if `msg` fails.
#[test]
fn drain_error() {
    let err = nix_store_support::ERROR;
    for (bytes, msg) in [
        (error(b"boom"), &b"nix-daemon: boom"[..]),
        (error(b""), b"nix-daemon: "),
        (error(b"two\nlines \xff"), b"nix-daemon: two\nlines \xff"),
        (word(err), b"nix-daemon: ?"),
        (
            cat(&[&word(err), &string(b"Error"), &word(0), &string(b"Error")]),
            b"nix-daemon: ?",
        ),
        (
            cat(&[
                &word(err),
                &string(b"Error"),
                &word(0),
                &string(b"Error"),
                &word(MAX_STRING + 1),
            ]),
            b"nix-daemon: ?",
        ),
        // an oversized `type` consumes only its length; `level`, `name`, `msg` follow
        (
            cat(&[
                &word(err),
                &word(MAX_STRING + 1),
                &word(5),
                &string(b"name"),
                &string(b"after oversize"),
            ]),
            b"nix-daemon: after oversize",
        ),
    ] {
        let o = check(&add_hello(), &drain_script(&bytes));
        o.assert_fails(1, &[msg]);
        assert_sent(&o, &client_handshake());
    }
}

/// Q-NIX-8: an oversized string consumes only its length word, and the payload is read as
/// codes.
#[test]
fn drain_oversize_desync() {
    let payload = next(b"resync");
    let o = check(
        &add_hello(),
        &drain_ok_script(&cat(&[&word(NEXT), &word(MAX_STRING + 1), &payload])),
    );
    assert_eq!(o.code, Some(0), "{o:?}");
    assert_eq!(o.stderr, b"\nresync\n");
    // usually it ends in an unexpected message taken from the payload
    let mut bytes = cat(&[&word(NEXT), &word(MAX_STRING + 1)]);
    bytes.resize(bytes.len() + MAX_STRING as usize + 1, b'x');
    let o = check(&add_hello(), &drain_script(&bytes));
    o.assert_fails(
        1,
        &[
            b"",
            b"jig nix-store: unexpected daemon message 0x7878787878787878",
        ],
    );
}

/// Exactly 64 MiB is accepted.
#[test]
fn drain_max_string() {
    let text = vec![b'x'; MAX_STRING as usize];
    let o = check(&add_hello(), &drain_ok_script(&next(&text)));
    assert_eq!(o.code, Some(0), "{o:?}");
    assert_eq!(o.stderr.len(), text.len() + 1);
    assert!(o.stderr.starts_with(&text) && o.stderr.ends_with(b"x\n"));
}

/// Q-NIX-8: a huge fields count at EOF keeps the client looping on failed reads. The test
/// asserts it is still running after a while, then kills it.
#[test]
fn drain_huge_fields_count_loops() {
    let s = drain_script(&cat(&[
        &word(RESULT),
        &word(1),
        &word(101),
        &word(u64::MAX),
    ]));
    let mut bins = vec![std::path::PathBuf::from(JIG)];
    bins.extend(legacy());
    for bin in bins {
        let dir = tempfile::tempdir().unwrap();
        let mut run = Running::start(&bin, &add_hello(), &s, dir.path());
        let alive = run.alive_after(Duration::from_millis(1500));
        run.kill();
        let o = run.finish();
        assert!(alive, "{} exited: {o:?}", bin.display());
        assert_eq!(o.signal, Some(libc::SIGKILL));
        assert!(o.stdout.is_empty() && o.stderr.is_empty(), "{o:?}");
        assert_eq!(o.sent, client_handshake());
    }
}

// ---- §Wire examples ------------------------------------------------------------------------

#[test]
fn wire_examples() {
    let file = VectorFile::load("jig-nix-store");
    for v in file.of_kind("wire", 8) {
        let words: Vec<&str> = v.str("message").split(' ').collect();
        let got = match words[..] {
            ["client_hello"] => client_hello(),
            ["client_features"] => client_features(),
            ["client_post"] => client_post(),
            ["add_text", name] => client_add(name.as_bytes(), &[], b""),
            ["add_text", name, contents] => client_add(name.as_bytes(), &[], contents.as_bytes()),
            ["submit", path, output] => client_submit(path.as_bytes(), output.as_bytes()),
            ["daemon_hello"] => daemon_hello(V1_38),
            ["stderr_last"] => word(LAST),
            _ => panic!("{}: unknown message", v.label()),
        };
        assert_eq!(hex(&got), hex(&v.hex("wire_hex")), "{}", v.label());
    }
    // the client sends exactly these on the socket
    let request = hello_request();
    let o = check(&add_hello(), &hello_script(&ok_reply(DAEMON_PATH)));
    o.assert_prints(DAEMON_PATH);
    assert_sent(&o, &cat(&[&client_handshake(), &request]));
    // Q-NIX-6: empty contents send a zero-length chunk and the terminator
    let empty = client_add(b"e", &[], b"");
    let o = check(
        &Inv::new(&[&b"add-text"[..], b"e"]),
        &add_reply(empty.len(), &ok_reply(DAEMON_PATH)),
    );
    o.assert_prints(DAEMON_PATH);
    assert_sent(&o, &cat(&[&client_handshake(), &empty]));
}

fn hex(b: &[u8]) -> String {
    use std::fmt::Write;
    b.iter().fold(String::new(), |mut out, x| {
        write!(out, "{x:02x}").unwrap();
        out
    })
}

/// Q-NIX-5: the references go out sorted with duplicates; Q-NIX-13: add-drv sends the ATerm
/// under the argv name with the argv refs.
#[test]
fn add_requests() {
    let a: &[u8] = b"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a";
    let b: &[u8] = b"/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b";
    let request = client_add(b"hello.txt", &[a, a, b], b"hello");
    let o = check(
        &Inv::new(&[&b"add-text"[..], b"hello.txt", b, a, a]).stdin(b"hello"),
        &add_reply(request.len(), &ok_reply(DAEMON_PATH)),
    );
    o.assert_prints(DAEMON_PATH);
    assert_sent(&o, &cat(&[&client_handshake(), &request]));

    let request = client_add(
        b"x.drv",
        &[b"/d.drv", b"/s1", b"/s2"],
        nix_store_support::aterm(),
    );
    let o = check(
        &Inv::new(&[&b"add-drv"[..], b"x.drv", b"/s2", b"/s1", b"/d.drv"])
            .stdin(nix_store_support::aterm_json()),
        &add_reply(request.len(), &ok_reply(DAEMON_PATH)),
    );
    o.assert_prints(DAEMON_PATH);
    assert_sent(&o, &cat(&[&client_handshake(), &request]));

    // the encoder's output byte for byte: escapes, NUL, iteration shapes
    let json = br#"{"env":["a\u0000\"b"],"outputs":{"o":{"path":"\t"}},"system":"\r\n\\"}"#;
    let aterm = b"Derive([(\"o\",\"\\t\",\"\",\"\")],[],[],\"\\r\\n\\\\\",\"\",[],[(\"0\",\"a\x00\\\"b\")])";
    let request = client_add(b"y", &[], aterm);
    let o = check(
        &Inv::new(&[&b"add-drv"[..], b"y"]).stdin(json),
        &add_reply(request.len(), &ok_reply(DAEMON_PATH)),
    );
    o.assert_prints(DAEMON_PATH);
    assert_sent(&o, &cat(&[&client_handshake(), &request]));
}

// ---- Q-NIX-7 -------------------------------------------------------------------------------

/// Every write point after a daemon hang-up: death by SIGPIPE with the default disposition,
/// a silent exit 1 with SIGPIPE ignored.
#[test]
fn sigpipe() {
    let features = vec![
        Step::Recv(16),
        Step::ShutRd,
        Step::Send(daemon_hello(V1_38)),
    ];
    let post = vec![
        Step::Recv(16),
        Step::Send(daemon_hello(V1_38)),
        Step::Recv(136),
        Step::ShutRd,
        Step::Send(strings(&[])),
    ];
    let op = shut_before_last_send(handshake());
    for (inv, s) in [
        (add_hello(), &features),
        (add_hello(), &post),
        (add_hello(), &op),
        (submit(), &op),
        (Inv::new(&[b"fetchurls"]).stdin(fetch_one()), &op),
    ] {
        check(&inv, s).assert_killed(libc::SIGPIPE);
        check(&inv.sigpipe_ignored(), s).assert_fails(1, &[]);
    }
}

// ---- spec/builder_rpc.qnt scenarios -------------------------------------------------------

/// `fetchTwoTest`: a NEXT in the startup drain, an activity in the first op's drain, two ops,
/// exit 0, seven writes.
#[test]
fn quint_fetch_two() {
    let two = cat(&[
        &fetch_one()[..fetch_one().len() - 1],
        b",",
        &fetch_one()[1..],
    ]);
    let request = client_add(b"foo-1.0.tar.gz.drv", &[], fetch_one_aterm());
    let activity = cat(&[&word(STOP_ACTIVITY), &word(1)]);
    let mut s = handshake_with(&next(b"hello"));
    s.push(Step::Recv(request.len()));
    s.push(Step::Send(cat(&[&activity, &ok_reply(b"/d/1")])));
    s.push(Step::Recv(request.len()));
    s.push(Step::Send(ok_reply(b"/d/2")));
    let o = check(&Inv::new(&[b"fetchurls"]).stdin(&two), &s);
    assert_eq!(o.code, Some(0), "{o:?}");
    assert_eq!(o.stderr, b"hello\n");
    assert!(o.stdout.starts_with(br#"[{"drv":"/d/1","out":"#), "{o:?}");
    assert_sent(&o, &cat(&[&client_handshake(), &request, &request]));
}

/// `submitTest`.
#[test]
fn quint_submit() {
    let mut s = handshake_with(&next(b"hello"));
    s.push(Step::Recv(64));
    s.push(Step::Send(cat(&[&word(LAST), &word(0)])));
    let o = check(&submit(), &s);
    assert_eq!(o.code, Some(0), "{o:?}");
    assert!(o.stdout.is_empty());
    assert_sent(
        &o,
        &cat(&[
            &client_handshake(),
            &client_submit(b"/nix/store/abc-x.drv", b"out"),
        ]),
    );
}

/// `daemonErrorTest`.
#[test]
fn quint_daemon_error() {
    let mut s = handshake_with(&next(b"hello"));
    s.push(Step::Recv(hello_request().len()));
    s.push(Step::Send(error(b"boom")));
    let o = check(&add_hello(), &s);
    o.assert_fails(1, &[b"hello", b"nix-daemon: boom"]);
}

/// `badVerbAfterHandshakeTest` (Q-NIX-2).
#[test]
fn quint_bad_verb_after_handshake() {
    let o = check(&Inv::new(&[b"bogus"]), &handshake_with(&next(b"hello")));
    o.assert_fails(2, &[b"hello", bad_arguments()]);
    assert_sent(&o, &client_handshake());
}

/// `oldDaemonTest`.
#[test]
fn quint_old_daemon() {
    let o = check(
        &Inv::new(&[&b"add-drv"[..], b"x"]).stdin(b"{}"),
        &[Step::Recv(16), Step::Send(daemon_hello(0x125))],
    );
    o.assert_fails(1, &[b"jig nix-store: daemon protocol 293 too old"]);
    assert_sent(&o, &client_hello());
}
