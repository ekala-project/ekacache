//! `jig gocacheprog` transcripts: stdin → stdout bytes against an in-process fake jigd
//! (`behaviors/jig-gocacheprog.md`, `implementation/08-jig-gocache-and-cache-cmds.md §Part 2
//! §Tests`).
//!
//! Every request/response pair of the spec has a test here, plus the adopted JSON handling
//! (Q-GO-1, -3, -4), the kept quirks (Q-GO-2, -5, -6, -7, -8), ADOPTED Q-CLIENT-2, and the
//! recorded `cmd/go` sessions in `fixtures/gocache/` replayed against the legacy C++ jig's
//! recorded responses. Nothing here needs a real jigd or the legacy binaries.
#![allow(clippy::unwrap_used, clippy::too_many_lines)]

mod gocache_support;

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Output;

use sha2::Digest as _;

use gocache_support::{FakeJigd, HANDSHAKE, JIG, Run, b64, close, files, get, hex, log_rows, put};

struct Setup {
    fake: FakeJigd,
    tmp: tempfile::TempDir,
    dp: PathBuf,
    log: PathBuf,
}

fn setup() -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    Setup {
        fake: FakeJigd::start(),
        dp: tmp.path().join("dp"),
        log: tmp.path().join("log"),
        tmp,
    }
}

impl Setup {
    fn run(&self) -> Run<'_> {
        Run::new(OsStr::new(JIG), Some(&self.fake.sock))
            .env("GOCACHEPROG_DIR", self.dp.as_os_str())
            .env("JIG_LOG", self.log.as_os_str())
    }

    /// A live session with `stdin`.
    fn session(&self, stdin: &str) -> Output {
        self.run().stdin(stdin.as_bytes())
    }

    /// `<dp>/<hex of output>`.
    fn disk_path(&self, output: &[u8]) -> String {
        format!("{}/{}", self.dp.display(), hex(output))
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8(b.to_vec()).unwrap()
}

/// stdout after the handshake, which must come first.
fn replies(out: &Output) -> String {
    let s = text(&out.stdout);
    s.strip_prefix(HANDSHAKE)
        .unwrap_or_else(|| panic!("no handshake: {s:?}"))
        .to_owned()
}

fn ok(out: &Output) -> String {
    assert_eq!(out.status.code(), Some(0), "stderr: {}", text(&out.stderr));
    assert_eq!(text(&out.stderr), "");
    replies(out)
}

const ACT: &[u8] = b"action-id-0123456789abcdef012345";
const OUT: &[u8] = b"output-id-0123456789abcdef012345";

fn a_key(action: &[u8]) -> String {
    format!("go/a/{}", hex(action))
}

fn o_key(output: &[u8]) -> String {
    format!("go/o/{}", hex(output))
}

#[test]
fn handshake_only() {
    let s = setup();
    let out = s.session("");
    assert_eq!(text(&out.stdout), HANDSHAKE);
    assert_eq!(out.status.code(), Some(0));
    // §Startup 3: the directory is created
    assert!(s.dp.is_dir());
}

#[test]
fn close_replies_and_stops_reading() {
    let s = setup();
    let out = s.session(&format!("{}{}", close(7), get(8, ACT)));
    assert_eq!(ok(&out), "{\"ID\":7}\n");
    assert!(s.fake.requests().is_empty());
}

#[test]
fn put_then_get() {
    let s = setup();
    let body = b"compiled object bytes\x00\xff";
    let out = s.session(&format!("{}{}", put(2, ACT, OUT, body), close(3)));
    let dp = s.disk_path(OUT);
    assert_eq!(
        ok(&out),
        format!("{{\"ID\":2,\"DiskPath\":\"{dp}\"}}\n{{\"ID\":3}}\n")
    );
    // §Key and value layout: the body under go/o/, the raw OutputID under go/a/, body first
    assert_eq!(s.fake.get(&o_key(OUT)).unwrap(), body);
    assert_eq!(s.fake.get(&a_key(ACT)).unwrap(), OUT);
    let reqs = s.fake.requests();
    assert_eq!(reqs.len(), 2);
    assert!(
        reqs[0].starts_with(&format!("PUT {} ", o_key(OUT))),
        "{reqs:?}"
    );
    assert!(
        reqs[1].starts_with(&format!("PUT {} ", a_key(ACT))),
        "{reqs:?}"
    );
    assert_eq!(std::fs::read(&dp).unwrap(), body);
    assert_eq!(log_rows(&s.log), [format!("go\tcompiled\t{}", hex(ACT))]);

    // a hit writes the DiskPath file when it is absent
    std::fs::remove_file(&dp).unwrap();
    let out = s.session(&get(4, ACT));
    assert_eq!(
        ok(&out),
        format!(
            "{{\"ID\":4,\"OutputID\":\"{}\",\"Size\":{},\"DiskPath\":\"{dp}\"}}\n",
            b64(OUT),
            body.len()
        )
    );
    assert_eq!(std::fs::read(&dp).unwrap(), body);
    let reqs = s.fake.requests();
    assert_eq!(
        reqs[2..],
        [format!("GET {}", a_key(ACT)), format!("GET {}", o_key(OUT))]
    );
    assert_eq!(log_rows(&s.log)[1], format!("go\tcached\t{}", hex(ACT)));

    // and leaves an existing one alone, whatever it holds
    std::fs::write(&dp, "other").unwrap();
    let out = s.session(&get(5, ACT));
    assert!(ok(&out).contains("\"Size\":23,"));
    assert_eq!(std::fs::read(&dp).unwrap(), b"other");
    // no leftover temp files
    assert_eq!(files(&s.dp).len(), 1);
}

#[test]
fn put_overwrites_the_disk_path() {
    let s = setup();
    std::fs::create_dir_all(&s.dp).unwrap();
    std::fs::write(s.disk_path(OUT), "stale").unwrap();
    ok(&s.session(&put(1, ACT, OUT, b"new")));
    assert_eq!(std::fs::read(s.disk_path(OUT)).unwrap(), b"new");
}

#[test]
fn get_misses() {
    let s = setup();
    // nothing stored
    let out = s.session(&get(1, ACT));
    assert_eq!(ok(&out), "{\"ID\":1,\"Miss\":true}\n");
    // a go/a/ hit whose body was evicted (gocache.qnt getFinish)
    s.fake.insert(&a_key(ACT), OUT);
    let out = s.session(&get(2, ACT));
    assert_eq!(ok(&out), "{\"ID\":2,\"Miss\":true}\n");
    assert_eq!(
        s.fake.requests()[1..],
        [format!("GET {}", a_key(ACT)), format!("GET {}", o_key(OUT))]
    );
    // no JIG_LOG line for a miss
    assert!(log_rows(&s.log).is_empty());
    assert!(files(&s.dp).is_empty());
}

#[test]
fn empty_and_absent_bodies() {
    let s = setup();
    let a = b64(ACT);
    let o = b64(OUT);
    let dp = s.disk_path(OUT);
    // BodySize absent, 0 and negative: no body line is read, the body is empty
    for (id, size) in [
        (1, String::new()),
        (2, ",\"BodySize\":0".into()),
        (3, ",\"BodySize\":-4".into()),
    ] {
        let line = format!(
            "{{\"ID\":{id},\"Command\":\"put\",\"ActionID\":\"{a}\",\"OutputID\":\"{o}\"{size}}}\n"
        );
        let out = s.session(&line);
        assert_eq!(ok(&out), format!("{{\"ID\":{id},\"DiskPath\":\"{dp}\"}}\n"));
        assert_eq!(s.fake.get(&o_key(OUT)).unwrap(), b"");
        assert_eq!(std::fs::read(&dp).unwrap(), b"");
    }
    // a hit on an empty body has Size 0
    let out = s.session(&get(4, ACT));
    assert_eq!(
        ok(&out),
        format!("{{\"ID\":4,\"OutputID\":\"{o}\",\"Size\":0,\"DiskPath\":\"{dp}\"}}\n")
    );
}

#[test]
fn torn_bodies_are_refused() {
    let s = setup();
    let a = b64(ACT);
    let o = b64(OUT);
    let req = |id: i64, size: usize| {
        format!(
            "{{\"ID\":{id},\"Command\":\"put\",\"ActionID\":\"{a}\",\"OutputID\":\"{o}\",\"BodySize\":{size}}}\n\n"
        )
    };
    // the decoded size differs from BodySize
    let out = s.session(&format!(
        "{}\"{}\"\n{}",
        req(1, 5),
        b64(b"xyz"),
        get(2, ACT)
    ));
    assert_eq!(
        ok(&out),
        "{\"ID\":1,\"Err\":\"body: got 3 of 5 bytes\"}\n{\"ID\":2,\"Miss\":true}\n"
    );
    // EOF before the body line (after blank lines)
    let out = s.session(&format!("{}\n\n", req(3, 5)));
    assert_eq!(ok(&out), "{\"ID\":3,\"Err\":\"body: got 0 of 5 bytes\"}\n");
    // nothing written, stored or logged
    assert!(s.fake.kv().is_empty());
    assert!(files(&s.dp).is_empty());
    assert!(log_rows(&s.log).is_empty());
    assert!(s.fake.requests().iter().all(|r| !r.starts_with("PUT")));
}

#[test]
fn body_after_blank_lines_and_large_bodies() {
    let s = setup();
    // more than one 1 MiB read, and a body line split across reads
    let body: Vec<u8> = (0..3_000_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let line = put(1, ACT, OUT, &body).replacen("\n\n", "\n\n\n\n", 1);
    let out = s.session(&format!("{line}{}", get(2, ACT)));
    let dp = s.disk_path(OUT);
    assert_eq!(
        ok(&out),
        format!(
            "{{\"ID\":1,\"DiskPath\":\"{dp}\"}}\n{{\"ID\":2,\"OutputID\":\"{}\",\"Size\":3000000,\"DiskPath\":\"{dp}\"}}\n",
            b64(OUT)
        )
    );
    assert_eq!(std::fs::read(&dp).unwrap(), body);
    assert_eq!(s.fake.get(&o_key(OUT)).unwrap(), body);
}

#[test]
fn unknown_and_missing_commands_get_no_reply() {
    let s = setup();
    // Q-GO-2
    let out = s.session(&format!(
        "{{\"ID\":1,\"Command\":\"stat\",\"ActionID\":\"{}\"}}\n{{\"ID\":2}}\n{{\"ID\":3,\"Command\":null}}\n\
         {{\"ID\":4,\"Command\":\"GET\"}}\n{}",
        b64(ACT),
        get(5, ACT)
    ));
    assert_eq!(ok(&out), "{\"ID\":5,\"Miss\":true}\n");
    // an unknown command's ActionID is not looked at
    let out = s.session("{\"ID\":1,\"Command\":\"stat\",\"ActionID\":\"!!\"}\n");
    assert_eq!(ok(&out), "");
}

#[test]
fn a_body_line_without_body_size_is_a_bad_request() {
    // jig-gocacheprog.md §put: with BodySize absent a body line is read as a request. Legacy
    // gave it no reply (no Command); under the adopted JSON handling it is a JSON string, not
    // an object, so it has no ID: fatal. Go never sends one.
    let s = setup();
    let out = s.session(&format!(
        "{{\"ID\":1,\"Command\":\"put\",\"ActionID\":\"{}\",\"OutputID\":\"{}\"}}\n\"eHl6\"\n{}",
        b64(ACT),
        b64(OUT),
        get(2, ACT)
    ));
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(text(&out.stderr), "jig gocacheprog: bad request line\n");
    assert_eq!(
        replies(&out),
        format!("{{\"ID\":1,\"DiskPath\":\"{}\"}}\n", s.disk_path(OUT))
    );
}

#[test]
fn not_live() {
    // §Startup 1: no daemon → an always-miss cache that still writes DiskPath files; Q-GO-8:
    // the put is logged as compiled
    let s = setup();
    let out = Run::new(OsStr::new(JIG), None)
        .env("GOCACHEPROG_DIR", s.dp.as_os_str())
        .env("JIG_LOG", s.log.as_os_str())
        .stdin(format!("{}{}{}", put(1, ACT, OUT, b"xyz"), get(2, ACT), close(3)).as_bytes());
    let dp = s.disk_path(OUT);
    assert_eq!(
        ok(&out),
        format!("{{\"ID\":1,\"DiskPath\":\"{dp}\"}}\n{{\"ID\":2,\"Miss\":true}}\n{{\"ID\":3}}\n")
    );
    assert_eq!(std::fs::read(&dp).unwrap(), b"xyz");
    assert_eq!(log_rows(&s.log), [format!("go\tcompiled\t{}", hex(ACT))]);
}

#[test]
fn disk_path_directory() {
    let s = setup();
    let cwd = s.tmp.path().join("cwd");
    std::fs::create_dir(&cwd).unwrap();
    let line = put(1, ACT, OUT, b"x");
    let h = hex(OUT);
    let reply = |d: &str| format!("{{\"ID\":1,\"DiskPath\":\"{d}\"}}\n");
    let session = |env: &[(&str, &OsStr)]| {
        let mut r = Run::new(OsStr::new(JIG), Some(&s.fake.sock)).cwd(&cwd);
        for (k, v) in env {
            r = r.env(k, v);
        }
        ok(&r.stdin(line.as_bytes()))
    };
    // Q-GO-6: a set-but-empty GOCACHEPROG_DIR is the cwd, and DiskPath is relative
    assert_eq!(session(&[("GOCACHEPROG_DIR", OsStr::new(""))]), reply(&h));
    assert_eq!(std::fs::read(cwd.join(&h)).unwrap(), b"x");
    // §DiskPath files: a relative directory is not made absolute
    assert_eq!(
        session(&[("GOCACHEPROG_DIR", OsStr::new("rel/d"))]),
        reply(&format!("rel/d/{h}"))
    );
    assert_eq!(std::fs::read(cwd.join("rel/d").join(&h)).unwrap(), b"x");
    // $TMPDIR/gocacheprog
    let t = s.tmp.path().join("t");
    assert_eq!(
        session(&[("TMPDIR", t.as_os_str())]),
        reply(&format!("{}/gocacheprog/{h}", t.display()))
    );
    assert_eq!(std::fs::read(t.join("gocacheprog").join(&h)).unwrap(), b"x");
    // Q-GO-6: TMPDIR="" → /gocacheprog (not writable here; the reply is still sent, Q-GO-5)
    if !Path::new("/gocacheprog").exists() {
        assert_eq!(
            session(&[("TMPDIR", OsStr::new(""))]),
            reply(&format!("/gocacheprog/{h}"))
        );
    }
    // GOCACHEPROG_DIR wins over TMPDIR
    assert_eq!(
        session(&[
            ("TMPDIR", t.as_os_str()),
            ("GOCACHEPROG_DIR", OsStr::new("g"))
        ]),
        reply(&format!("g/{h}"))
    );
}

#[test]
fn unwritable_disk_path_does_not_abort() {
    // Q-GO-5: legacy's throwing fs::exists aborts on EACCES; the Rust port counts it as absent
    if rustix::process::geteuid().is_root() {
        eprintln!("running as root: permissions do not apply, skipped");
        return;
    }
    let s = setup();
    ok(&s.session(&put(1, ACT, OUT, b"xyz")));
    let locked = s.tmp.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    let dp = locked.join("d");
    let perms = |mode| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    perms(0o000);
    let out = Run::new(OsStr::new(JIG), Some(&s.fake.sock))
        .env("GOCACHEPROG_DIR", dp.as_os_str())
        .stdin(format!("{}{}", get(2, ACT), put(3, ACT, OUT, b"xyz")).as_bytes());
    perms(0o755);
    let d = format!("{}/{}", dp.display(), hex(OUT));
    assert_eq!(
        ok(&out),
        format!(
            "{{\"ID\":2,\"OutputID\":\"{}\",\"Size\":3,\"DiskPath\":\"{d}\"}}\n{{\"ID\":3,\"DiskPath\":\"{d}\"}}\n",
            b64(OUT)
        )
    );
}

#[test]
fn object_id_is_ignored() {
    // Q-GO-7: only OutputID is read; without it the output id is empty
    let s = setup();
    let line = format!(
        "{{\"ID\":1,\"Command\":\"put\",\"ActionID\":\"{}\",\"ObjectID\":\"{}\",\"BodySize\":3}}\n\"eHl6\"\n",
        b64(ACT),
        b64(OUT)
    );
    let out = s.session(&line);
    assert_eq!(
        ok(&out),
        format!("{{\"ID\":1,\"DiskPath\":\"{}/\"}}\n", s.dp.display())
    );
    assert_eq!(s.fake.get("go/o/").unwrap(), b"xyz");
    assert_eq!(s.fake.get(&a_key(ACT)).unwrap(), b"");
    // writing the DiskPath (the directory itself) fails; its temp file stays behind, as in
    // legacy (`gocache_difftest.rs` compares)
    assert!(s.dp.is_dir());
    let fs = files(&s.dp);
    assert_eq!(fs.len(), 1);
    let (name, data) = fs.iter().next().unwrap();
    assert!(name.starts_with(".jig"), "{name}");
    assert_eq!(data, b"xyz");
}

#[test]
fn ids_are_echoed() {
    let s = setup();
    let out = s.session(&format!(
        "{}{}{}",
        get(0, ACT),
        get(-3, ACT),
        get(i64::MAX, ACT)
    ));
    assert_eq!(
        ok(&out),
        format!(
            "{{\"ID\":0,\"Miss\":true}}\n{{\"ID\":-3,\"Miss\":true}}\n{{\"ID\":{},\"Miss\":true}}\n",
            i64::MAX
        )
    );
}

#[test]
fn crlf_lines() {
    // a `\r` stays part of the line (§Request loop); JSON treats it as white space (adopted).
    // A line holding only `\r` is not empty, so it is a bad request line (see
    // `adopted_invalid_json_without_an_id`).
    let s = setup();
    let input = format!(
        "{{\"ID\":1,\"Command\":\"put\",\"ActionID\":\"{a}\",\"OutputID\":\"{o}\",\"BodySize\":3}}\r\n\
         \"eHl6\"\r\n\n{{\"ID\":2,\"Command\":\"get\",\"ActionID\":\"{a}\"}}\r\n",
        a = b64(ACT),
        o = b64(OUT)
    );
    let out = s.session(&input);
    let dp = s.disk_path(OUT);
    assert_eq!(
        ok(&out),
        format!(
            "{{\"ID\":1,\"DiskPath\":\"{dp}\"}}\n{{\"ID\":2,\"OutputID\":\"{}\",\"Size\":3,\"DiskPath\":\"{dp}\"}}\n",
            b64(OUT)
        )
    );
    assert_eq!(std::fs::read(&dp).unwrap(), b"xyz");
}

// ---- ADOPTED: JSON handling (Q-GO-1, Q-GO-3, Q-GO-4) ----

#[test]
fn adopted_escaped_strings() {
    let s = setup();
    // "get" and the base64 of "abc" with JSON escapes; legacy saw neither
    let out = s.session(
        "{\"ID\":1,\"Command\":\"g\\u0065t\",\"ActionID\":\"YWJ\\u006a\"}\n\
         {\"ID\":2,\"Command\":\"put\",\"ActionID\":\"YWJj\",\"OutputID\":\"b3V\\/\",\"BodySize\":3}\n\
         \"eH\\u006c6\"\n",
    );
    let dp = s.disk_path(b"ou\x7f");
    assert_eq!(
        ok(&out),
        format!("{{\"ID\":1,\"Miss\":true}}\n{{\"ID\":2,\"DiskPath\":\"{dp}\"}}\n")
    );
    assert_eq!(s.fake.requests()[0], "GET go/a/616263");
    assert_eq!(s.fake.get("go/a/616263").unwrap(), b"ou\x7f");
    assert_eq!(s.fake.get(&o_key(b"ou\x7f")).unwrap(), b"xyz");
}

#[test]
fn adopted_whitespace() {
    let s = setup();
    let out = s.session(&format!(
        "{{ \"ID\" : 1 , \"Command\" : \"put\" , \"ActionID\" : \"{}\" , \"OutputID\": \"{}\", \"BodySize\" :3 }}\n  \"eHl6\"  \n\
         \t{{\"ID\": 2, \"Command\": \"get\", \"ActionID\": \"{}\"}}\t\n",
        b64(ACT),
        b64(OUT),
        b64(ACT)
    ));
    let dp = s.disk_path(OUT);
    assert_eq!(
        ok(&out),
        format!(
            "{{\"ID\":1,\"DiskPath\":\"{dp}\"}}\n{{\"ID\":2,\"OutputID\":\"{}\",\"Size\":3,\"DiskPath\":\"{dp}\"}}\n",
            b64(OUT)
        )
    );
}

#[test]
fn adopted_duplicate_keys() {
    // legacy took the first occurrence, Go's decoder the last: refused, with the ID recovered
    // from the last occurrence
    let s = setup();
    let out = s.session(&format!(
        "{{\"ID\":1,\"Command\":\"get\",\"Command\":\"put\"}}\n{{\"ID\":9,\"ID\":2,\"Command\":\"get\"}}\n{}",
        get(3, ACT)
    ));
    let r = ok(&out);
    let lines: Vec<&str> = r.lines().collect();
    assert_eq!(lines.len(), 3, "{r}");
    assert!(
        lines[0].starts_with("{\"ID\":1,\"Err\":\"bad request: duplicate field `Command`"),
        "{r}"
    );
    assert!(
        lines[1].starts_with("{\"ID\":2,\"Err\":\"bad request: duplicate field `ID`"),
        "{r}"
    );
    assert_eq!(lines[2], "{\"ID\":3,\"Miss\":true}");
    assert!(s.fake.requests().len() == 1);
}

#[test]
fn adopted_invalid_base64() {
    let s = setup();
    let a = b64(ACT);
    let o = b64(OUT);
    let out = s.session(&format!(
        // unpadded, white space, URL alphabet: legacy decoded all of them leniently
        "{{\"ID\":1,\"Command\":\"get\",\"ActionID\":\"YWJ\"}}\n\
         {{\"ID\":2,\"Command\":\"get\",\"ActionID\":\"YW I=\"}}\n\
         {{\"ID\":3,\"Command\":\"put\",\"ActionID\":\"YW-_\",\"OutputID\":\"{o}\",\"BodySize\":3}}\n\n\"eHl6\"\n\
         {{\"ID\":4,\"Command\":\"put\",\"ActionID\":\"{a}\",\"OutputID\":\"YQ=\",\"BodySize\":3}}\n\n\"eHl6\"\n\
         {{\"ID\":5,\"Command\":\"put\",\"ActionID\":\"{a}\",\"OutputID\":\"{o}\",\"BodySize\":3}}\n\n\"eHl\"\n\
         {{\"ID\":6,\"Command\":\"put\",\"ActionID\":\"{a}\",\"OutputID\":\"{o}\",\"BodySize\":3}}\n\n\"eHl6\n\
         {{\"ID\":7,\"Command\":\"put\",\"ActionID\":\"{a}\",\"OutputID\":\"{o}\"}}\n"
    ));
    assert_eq!(
        ok(&out),
        format!(
            "{{\"ID\":1,\"Err\":\"ActionID: invalid base64\"}}\n\
             {{\"ID\":2,\"Err\":\"ActionID: invalid base64\"}}\n\
             {{\"ID\":3,\"Err\":\"ActionID: invalid base64\"}}\n\
             {{\"ID\":4,\"Err\":\"OutputID: invalid base64\"}}\n\
             {{\"ID\":5,\"Err\":\"body: invalid base64\"}}\n\
             {{\"ID\":6,\"Err\":\"body: not a JSON string\"}}\n\
             {{\"ID\":7,\"DiskPath\":\"{}\"}}\n",
            s.disk_path(OUT)
        )
    );
    // only the last put stored anything: the body line of each refused put was consumed
    assert_eq!(s.fake.get(&o_key(OUT)).unwrap(), b"");
    assert_eq!(s.fake.kv().len(), 2);
    assert_eq!(files(&s.dp).len(), 1);
    assert_eq!(log_rows(&s.log).len(), 1);
}

#[test]
fn adopted_invalid_json_with_an_id() {
    let s = setup();
    let out = s.session(&format!(
        "{{\"ID\":1,\"Command\":[\"get\"]}}\n{{\"ID\":2,\"Command\":\"put\",\"BodySize\":\"3\"}}\n\
         {{\"ID\":3,\"Command\":\"get\",\"ActionID\":7}}\n{}",
        get(4, ACT)
    ));
    let r = ok(&out);
    let lines: Vec<&str> = r.lines().collect();
    assert_eq!(lines.len(), 4, "{r}");
    for (i, l) in lines[..3].iter().enumerate() {
        assert!(
            l.starts_with(&format!(
                "{{\"ID\":{},\"Err\":\"bad request: invalid type: ",
                i + 1
            )),
            "{l}"
        );
    }
    assert_eq!(lines[3], "{\"ID\":4,\"Miss\":true}");
}

#[test]
fn adopted_invalid_json_without_an_id() {
    for bad in [
        "{\"ID\":1,\"Command\":\"get\"",
        "{\"Command\":\"get\"}",
        "{\"ID\":\"7\",\"Command\":\"get\"}",
        "{\"ID\":1.0,\"Command\":\"get\"}",
        "{\"ID\":18446744073709551615,\"Command\":\"get\"}",
        "[1,\"get\"]",
        "get",
        " ",
        "\r",
    ] {
        let s = setup();
        let out = s.session(&format!("{}{bad}\n{}", get(1, ACT), get(2, ACT)));
        assert_eq!(out.status.code(), Some(1), "{bad}");
        assert_eq!(
            text(&out.stderr),
            "jig gocacheprog: bad request line\n",
            "{bad}"
        );
        // the replies before it were flushed; nothing after it was read
        assert_eq!(replies(&out), "{\"ID\":1,\"Miss\":true}\n", "{bad}");
        assert_eq!(s.fake.requests().len(), 1, "{bad}");
    }
}

#[test]
fn adopted_disk_path_escaping() {
    // ADOPTED Q-GO-3: DiskPath is a JSON string
    let s = setup();
    let dir = s.tmp.path().join("a\"b\\c\td");
    let out = Run::new(OsStr::new(JIG), Some(&s.fake.sock))
        .env("GOCACHEPROG_DIR", dir.as_os_str())
        .stdin(format!("{}{}", put(1, ACT, OUT, b"xyz"), get(2, ACT)).as_bytes());
    let esc = format!("{}/a\\\"b\\\\c\\td/{}", s.tmp.path().display(), hex(OUT));
    assert_eq!(
        ok(&out),
        format!(
            "{{\"ID\":1,\"DiskPath\":\"{esc}\"}}\n{{\"ID\":2,\"OutputID\":\"{}\",\"Size\":3,\"DiskPath\":\"{esc}\"}}\n",
            b64(OUT)
        )
    );
    assert_eq!(std::fs::read(dir.join(hex(OUT))).unwrap(), b"xyz");
    // and the replies are JSON, with the right path
    for l in replies(&out).lines() {
        let v: serde_json::Value = serde_json::from_str(l).unwrap();
        assert_eq!(
            v["DiskPath"].as_str().unwrap(),
            dir.join(hex(OUT)).to_str().unwrap()
        );
    }
}

#[test]
fn non_utf8_disk_path_is_raw() {
    // Q-GO-3 residual: a non-UTF-8 directory cannot be a JSON string; its bytes go out raw, as
    // legacy writes every DiskPath
    let s = setup();
    let mut d = s.tmp.path().as_os_str().as_bytes().to_vec();
    d.extend_from_slice(b"/x\xff\"y");
    let dir = OsStr::from_bytes(&d);
    // APFS refuses names that are not UTF-8 (EILSEQ): there is no such directory to write to
    if std::fs::create_dir(dir).is_err() {
        eprintln!("skipped: this filesystem refuses non-UTF-8 names");
        return;
    }
    std::fs::remove_dir(dir).unwrap();
    let out = Run::new(OsStr::new(JIG), Some(&s.fake.sock))
        .env("GOCACHEPROG_DIR", dir)
        .stdin(format!("{}{}", put(1, ACT, OUT, b"xyz"), get(2, ACT)).as_bytes());
    assert_eq!(out.status.code(), Some(0));
    let mut path = d.clone();
    path.push(b'/');
    path.extend_from_slice(hex(OUT).as_bytes());
    let mut want = HANDSHAKE.as_bytes().to_vec();
    want.extend_from_slice(b"{\"ID\":1,\"DiskPath\":\"");
    want.extend_from_slice(&path);
    want.extend_from_slice(b"\"}\n{\"ID\":2,\"OutputID\":\"");
    want.extend_from_slice(b64(OUT).as_bytes());
    want.extend_from_slice(b"\",\"Size\":3,\"DiskPath\":\"");
    want.extend_from_slice(&path);
    want.extend_from_slice(b"\"}\n");
    assert_eq!(out.stdout, want);
    assert_eq!(std::fs::read(OsStr::from_bytes(&path)).unwrap(), b"xyz");
}

// ---- ADOPTED Q-CLIENT-2 ----

#[test]
fn refused_put_is_reported() {
    let s = setup();
    s.fake.refuse_puts();
    let out = s.session(&format!("{}{}", put(1, ACT, OUT, b"xyz"), get(2, ACT)));
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        text(&out.stderr),
        format!("jig: cache put {} failed\n", o_key(OUT))
    );
    // the put is still answered (and logged, Q-GO-8); the dead connection is not used again
    assert_eq!(
        replies(&out),
        format!(
            "{{\"ID\":1,\"DiskPath\":\"{}\"}}\n{{\"ID\":2,\"Miss\":true}}\n",
            s.disk_path(OUT)
        )
    );
    assert_eq!(s.fake.requests().len(), 1);
    assert!(s.fake.kv().is_empty());
    assert_eq!(log_rows(&s.log), [format!("go\tcompiled\t{}", hex(ACT))]);
}

// ---- recorded cmd/go traffic ----

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/gocache")
            .join(name),
    )
    .unwrap()
}

/// The sessions of `fixtures/gocache/session<n>.in` were recorded through a `tee` wrapper
/// around the legacy C++ jig on a fresh Go jigd (go 1.25: `go build ./...` cold, again warm,
/// `-gcflags=all=-N` cold, `go list -export`, `-N` warm; `session<n>.cmd`). The `.out` files
/// are legacy's responses with the DiskPath directory replaced by `@DIR@`. Replayed in order
/// against one fake jigd, the Rust responses must be byte-identical.
#[test]
fn recorded_sessions_match_legacy() {
    let s = setup();
    let dir = s.dp.to_str().unwrap().to_owned();
    for n in 1..=5 {
        let input = fixture(&format!("session{n}.in"));
        let want = text(&fixture(&format!("session{n}.out"))).replace("@DIR@", &dir);
        let out = s.run().stdin(&input);
        assert_eq!(out.status.code(), Some(0), "session {n}");
        assert_eq!(text(&out.stderr), "", "session {n}");
        assert_eq!(text(&out.stdout), want, "session {n}");
    }
    // every DiskPath file holds the body its name hashes to (Go's OutputID is the SHA-256)
    let fs = files(&s.dp);
    assert_eq!(fs.len(), 10);
    for (name, body) in fs {
        assert_eq!(hex(&sha2::Sha256::digest(&body)), name);
    }
    let rows = log_rows(&s.log);
    assert!(
        rows.iter()
            .all(|r| r.starts_with("go\tcompiled\t") || r.starts_with("go\tcached\t"))
    );
    let hits: usize = (1..=5)
        .map(|n| {
            text(&fixture(&format!("session{n}.out")))
                .matches("\"OutputID\"")
                .count()
        })
        .sum();
    assert!(hits > 0);
    assert_eq!(
        rows.iter()
            .filter(|r| r.starts_with("go\tcached\t"))
            .count(),
        hits
    );
}
