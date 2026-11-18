//! Differential tests: the legacy C++ `jig gocacheprog` against the Rust one, on real jigds
//! (`implementation/12-testing.md`, `implementation/08-jig-gocache-and-cache-cmds.md §Part 2
//! §Tests`).
//!
//! Needs `$JIG_LEGACY_JIG` and a jigd (`$JIG_RUST_JIGD` or `$JIG_LEGACY_JIGD`); skipped
//! without them. Each binary runs a case on its own fresh daemon, in the same working directory
//! with the same DiskPath directory (emptied in between), so stdout, stderr, exit status, the
//! files written and the `JIG_LOG` rows must all be identical. The exception is allow-list
//! rule `gocache-json` (Q-GO-1, -3, -4, `behaviors/jig-gocacheprog.md §Adopted: JSON
//! handling`): inputs `cmd/go` never sends, on which the two must differ.
#![allow(
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::many_single_char_names
)]

mod common;
mod gocache_support;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use common::{any_daemon, legacy_jig};
use gocache_support::{JIG, Run, b64, close, get, hex, log_rows, put};

const SKIP: &str = "JIG_LEGACY_JIG and JIG_RUST_JIGD or JIG_LEGACY_JIGD are needed: skipped";

/// The legacy jig, when it and a daemon are available.
fn legacy() -> Option<PathBuf> {
    let daemon = std::env::var_os("JIG_RUST_JIGD").is_some()
        || std::env::var_os("JIG_LEGACY_JIGD").is_some();
    legacy_jig().filter(|_| daemon)
}

enum Step {
    /// One `jig gocacheprog` with this stdin.
    Session(Vec<u8>),
    /// Remove a file (relative to the working directory).
    Remove(String),
    /// Write a file (relative to the working directory).
    Write(String, &'static [u8]),
}

struct Case {
    name: &'static str,
    /// Environment beyond `PATH`, `JIG_SOCK` and `JIG_LOG`; `@W@` is the working directory.
    env: Vec<(&'static str, &'static str)>,
    live: bool,
    steps: Vec<Step>,
}

impl Case {
    fn new(name: &'static str, steps: Vec<Step>) -> Self {
        Self {
            name,
            env: vec![("GOCACHEPROG_DIR", "@W@/dp")],
            live: true,
            steps,
        }
    }

    fn one(name: &'static str, stdin: &str) -> Self {
        Self::new(name, vec![Step::Session(stdin.as_bytes().to_vec())])
    }
}

#[derive(PartialEq, Eq)]
struct Seen {
    /// Exit code (or `-signal`), stdout and stderr of each session.
    sessions: Vec<(i32, Vec<u8>, Vec<u8>)>,
    /// Every file under the working directory, by relative path.
    files: BTreeMap<String, Vec<u8>>,
    log: Vec<String>,
}

impl std::fmt::Debug for Seen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (code, out, err) in &self.sessions {
            writeln!(f, "exit {code}")?;
            writeln!(f, "stdout: {}", String::from_utf8_lossy(out))?;
            writeln!(f, "stderr: {}", String::from_utf8_lossy(err))?;
        }
        for (name, data) in &self.files {
            writeln!(f, "file {name}: {} bytes", data.len())?;
        }
        writeln!(f, "log: {:?}", self.log)
    }
}

fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd {
        let p = e.unwrap().path();
        if p.is_dir() {
            walk(root, &p, out);
        } else {
            let mut name = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            // a temp file left behind (`util::write_file` onto a directory, Q-GO-7) has the pid
            if let Some(i) = name.find(".jig") {
                let digits = name[i + 4..].bytes().take_while(u8::is_ascii_digit).count();
                name.replace_range(i + 4..i + 4 + digits, "<pid>");
            }
            out.insert(name, std::fs::read(&p).unwrap());
        }
    }
}

/// Run `case` with `bin` under `base` (emptied first).
fn observe(bin: &Path, base: &Path, case: &Case) -> Seen {
    let work = base.join("w");
    let log = base.join("log");
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_file(&log);
    std::fs::create_dir(&work).unwrap();
    let daemon = if case.live {
        Some(any_daemon().unwrap())
    } else {
        None
    };
    let w = work.to_str().unwrap();
    let env: Vec<(&str, OsString)> = case
        .env
        .iter()
        .map(|(k, v)| (*k, OsString::from(v.replace("@W@", w))))
        .collect();
    let mut sessions = Vec::new();
    for step in &case.steps {
        match step {
            Step::Session(input) => {
                let mut r = Run::new(bin.as_os_str(), daemon.as_ref().map(|d| d.sock.as_path()))
                    .cwd(&work)
                    .env("JIG_LOG", log.as_os_str());
                for (k, v) in &env {
                    r = r.env(k, v);
                }
                let out = r.stdin(input);
                let code = out.status.code().unwrap_or_else(|| {
                    use std::os::unix::process::ExitStatusExt;
                    -out.status.signal().unwrap_or(0)
                });
                sessions.push((code, out.stdout, out.stderr));
            }
            Step::Remove(name) => std::fs::remove_file(work.join(name)).unwrap(),
            Step::Write(name, data) => std::fs::write(work.join(name), data).unwrap(),
        }
    }
    let mut files = BTreeMap::new();
    walk(&work, &work, &mut files);
    Seen {
        sessions,
        files,
        log: log_rows(&log),
    }
}

/// Run `case` with legacy and with the Rust jig; also the working directory both used.
fn both(legacy: &Path, case: &Case) -> (Seen, Seen, PathBuf) {
    let base = tempfile::tempdir().unwrap();
    let l = observe(legacy, base.path(), case);
    let r = observe(Path::new(JIG), base.path(), case);
    (l, r, base.path().join("w"))
}

const ACT: &[u8] = b"action-id-0123456789abcdef012345";
const OUT: &[u8] = b"output-id-0123456789abcdef012345";

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/gocache")
            .join(name),
    )
    .unwrap()
}

/// The recorded `cmd/go` sessions (`gocache_transcript.rs::recorded_sessions_match_legacy`),
/// in order on one daemon: byte-identical, and equal to the recording.
#[test]
fn recorded_corpus_is_identical() {
    let Some(legacy) = legacy() else {
        eprintln!("{SKIP}");
        return;
    };
    let case = Case::new(
        "corpus",
        (1..=5)
            .map(|n| Step::Session(fixture(&format!("session{n}.in"))))
            .collect(),
    );
    let (l, r, work) = both(&legacy, &case);
    assert_eq!(l, r);
    assert_eq!(r.files.len(), 10);
    let dir = work.join("dp").to_str().unwrap().to_owned();
    for (n, (code, out, err)) in r.sessions.iter().enumerate() {
        assert_eq!((*code, err.as_slice()), (0, &b""[..]));
        let want = String::from_utf8(fixture(&format!("session{}.out", n + 1))).unwrap();
        assert_eq!(
            String::from_utf8(out.clone()).unwrap(),
            want.replace("@DIR@", &dir),
            "session {}",
            n + 1
        );
    }
}

/// Entries one jig writes hit in the other (`12-testing.md §Mixed-fleet matrix`), with the
/// recorded responses and the same DiskPath files.
#[test]
fn entries_are_shared_both_ways() {
    let Some(legacy) = legacy() else {
        eprintln!("{SKIP}");
        return;
    };
    let rust = PathBuf::from(JIG);
    for (writer, reader) in [(&legacy, &rust), (&rust, &legacy)] {
        let d = any_daemon().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let wdir = tmp.path().join("w");
        let rdir = tmp.path().join("r");
        let run = |bin: &Path, dir: &Path, n: u32| {
            let out = Run::new(bin.as_os_str(), Some(&d.sock))
                .env("GOCACHEPROG_DIR", dir.as_os_str())
                .stdin(&fixture(&format!("session{n}.in")));
            assert_eq!(out.status.code(), Some(0));
            assert!(
                out.stderr.is_empty(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let want = String::from_utf8(fixture(&format!("session{n}.out")))
                .unwrap()
                .replace("@DIR@", dir.to_str().unwrap());
            assert_eq!(
                String::from_utf8(out.stdout).unwrap(),
                want,
                "{} after {}, session {n}",
                bin.display(),
                writer.display()
            );
        };
        // cold builds by the writer, warm ones and `go list -export` by the reader
        for (bin, dir, n) in [
            (writer, &wdir, 1),
            (reader, &rdir, 2),
            (writer, &wdir, 3),
            (reader, &rdir, 5),
            (reader, &rdir, 4),
        ] {
            run(bin, dir, n);
        }
        let written = gocache_support::files(&wdir);
        let read = gocache_support::files(&rdir);
        // the distinct outputs sessions 2, 4 and 5 hit
        assert_eq!(read.len(), 7);
        for (name, body) in &read {
            assert_eq!(written.get(name), Some(body), "{name}");
        }
    }
}

/// The spec's transcripts on Go-shaped input (and the kept quirks): identical.
#[test]
fn spec_transcripts_are_identical() {
    let Some(legacy) = legacy() else {
        eprintln!("{SKIP}");
        return;
    };
    let a = b64(ACT);
    let o = b64(OUT);
    let h = hex(OUT);
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let put_sized = |id: i64, size: &str| {
        format!(
            "{{\"ID\":{id},\"Command\":\"put\",\"ActionID\":\"{a}\",\"OutputID\":\"{o}\"{size}}}\n\n"
        )
    };
    let mut cases = vec![
        Case::one("handshake", ""),
        Case::one("close", &format!("{}{}", close(7), get(8, ACT))),
        Case::new(
            "put, then hits",
            vec![
                Step::Session(
                    format!("{}{}", put(2, ACT, OUT, b"xyz\0\xff"), close(3)).into_bytes(),
                ),
                Step::Session(get(4, ACT).into_bytes()),
                Step::Remove(format!("dp/{h}")),
                Step::Session(get(5, ACT).into_bytes()),
                Step::Write(format!("dp/{h}"), b"other"),
                Step::Session(get(6, ACT).into_bytes()),
                Step::Session(put(7, ACT, OUT, b"again").into_bytes()),
            ],
        ),
        Case::one("miss", &get(1, ACT)),
        Case::one(
            "empty bodies",
            &format!(
                "{}{}{}{}",
                put_sized(1, ""),
                put_sized(2, ",\"BodySize\":0"),
                put_sized(3, ",\"BodySize\":-4"),
                get(4, ACT)
            ),
        ),
        Case::one(
            "torn body",
            &format!(
                "{}\"{}\"\n{}",
                put_sized(1, ",\"BodySize\":5"),
                b64(b"xyz"),
                get(2, ACT)
            ),
        ),
        Case::one(
            "eof before body",
            &format!("{}\n\n", put_sized(1, ",\"BodySize\":5")),
        ),
        Case::one(
            "large body after blank lines",
            &format!(
                "{}{}",
                put(1, ACT, OUT, &big).replacen("\n\n", "\n\n\n\n", 1),
                get(2, ACT)
            ),
        ),
        Case::one(
            "unknown commands",
            &format!(
                "{{\"ID\":1,\"Command\":\"stat\",\"ActionID\":\"{a}\"}}\n{{\"ID\":2}}\n\
                 {{\"ID\":3,\"Command\":null}}\n{{\"ID\":4,\"Command\":\"GET\"}}\n{}",
                get(5, ACT)
            ),
        ),
        Case::one("ids", &format!("{}{}", get(0, ACT), get(i64::MAX, ACT))),
        Case::one(
            "ObjectID only (Q-GO-7)",
            &format!(
                "{{\"ID\":1,\"Command\":\"put\",\"ActionID\":\"{a}\",\"ObjectID\":\"{o}\",\"BodySize\":3}}\n\"eHl6\"\n{}",
                get(2, ACT)
            ),
        ),
        Case::one(
            "unknown fields",
            &format!(
                "{{\"ID\":1,\"Command\":\"get\",\"ActionID\":\"{a}\",\"X\":[1,{{\"y\":2}}]}}\n"
            ),
        ),
    ];
    let session = format!("{}{}", put(1, ACT, OUT, b"xyz"), get(2, ACT));
    for (name, env) in [
        (
            "empty GOCACHEPROG_DIR (Q-GO-6)",
            vec![("GOCACHEPROG_DIR", "")],
        ),
        (
            "relative GOCACHEPROG_DIR",
            vec![("GOCACHEPROG_DIR", "rel/d")],
        ),
        ("TMPDIR", vec![("TMPDIR", "@W@/t")]),
        ("both", vec![("TMPDIR", "@W@/t"), ("GOCACHEPROG_DIR", "g")]),
    ] {
        let mut c = Case::one(name, &session);
        c.env = env;
        cases.push(c);
    }
    let mut c = Case::one("not live", &format!("{session}{}", close(3)));
    c.live = false;
    cases.push(c);

    for case in &cases {
        let (l, r, _) = both(&legacy, case);
        assert_eq!(l, r, "{}", case.name);
    }
}

/// Allow-list rule `gocache-json` (Q-GO-1, -3, -4): input `cmd/go` never sends, where the
/// Rust jig parses JSON (`Err` replies, exit 1, escaped DiskPath) and legacy scans
/// substrings. Each must differ; `gocache_transcript.rs` pins the Rust output.
#[test]
fn adopted_json_handling_differs() {
    let Some(legacy) = legacy() else {
        eprintln!("{SKIP}");
        return;
    };
    let a = b64(ACT);
    let o = b64(OUT);
    let mut cases = vec![
        Case::one(
            "escaped strings",
            "{\"ID\":1,\"Command\":\"g\\u0065t\",\"ActionID\":\"YWJ\\u006a\"}\n",
        ),
        Case::one(
            "whitespace after colons",
            &format!("{{\"ID\": 1, \"Command\": \"get\", \"ActionID\": \"{a}\"}}\n"),
        ),
        Case::one(
            "duplicate keys",
            &format!("{{\"ID\":1,\"Command\":\"get\",\"Command\":\"put\",\"ActionID\":\"{a}\"}}\n"),
        ),
        Case::one(
            "unpadded base64",
            "{\"ID\":1,\"Command\":\"get\",\"ActionID\":\"YWJ\"}\n",
        ),
        Case::one(
            "invalid base64 body",
            &format!(
                "{{\"ID\":1,\"Command\":\"put\",\"ActionID\":\"{a}\",\"OutputID\":\"{o}\",\"BodySize\":2}}\n\"YW-I\"\n"
            ),
        ),
        Case::one(
            "wrong type with an ID",
            "{\"ID\":1,\"Command\":[\"get\"]}\n",
        ),
        Case::one(
            "quoted BodySize",
            &format!(
                "{{\"ID\":1,\"Command\":\"put\",\"ActionID\":\"{a}\",\"OutputID\":\"{o}\",\"BodySize\":\"3\"}}\n\"eHl6\"\n"
            ),
        ),
        Case::one(
            "unparseable BodySize",
            &format!(
                "{{\"ID\":1,\"Command\":\"put\",\"ActionID\":\"{a}\",\"OutputID\":\"{o}\",\"BodySize\":x}}\n"
            ),
        ),
        Case::one(
            "no recoverable ID",
            "{\"ID\":\"7\",\"Command\":\"close\"}\n",
        ),
        Case::one(
            "not JSON",
            &format!("\"ID\":1,\"Command\":\"get\",\"ActionID\":\"{a}\"\n"),
        ),
        Case::one("negative ID", &get(-3, ACT)),
        Case::one(
            "CRLF body line",
            &format!(
                "{{\"ID\":1,\"Command\":\"put\",\"ActionID\":\"{a}\",\"OutputID\":\"{o}\",\"BodySize\":3}}\r\n\"eHl6\"\r\n"
            ),
        ),
        Case::one(
            "body line without BodySize",
            &format!(
                "{{\"ID\":1,\"Command\":\"put\",\"ActionID\":\"{a}\",\"OutputID\":\"{o}\"}}\n\"eHl6\"\n{}",
                get(2, ACT)
            ),
        ),
    ];
    let mut c = Case::one("escaped DiskPath", &put(1, ACT, OUT, b"xyz"));
    c.env = vec![("GOCACHEPROG_DIR", "@W@/a\"b\\c")];
    cases.push(c);

    for case in &cases {
        let (l, r, _) = both(&legacy, case);
        assert_ne!(l, r, "{}", case.name);
        // the Rust side is well-formed: JSON replies, or the documented exit 1
        let (code, out, err) = &r.sessions[0];
        for line in out.split(|&c| c == b'\n').filter(|l| !l.is_empty()) {
            let v: serde_json::Value = serde_json::from_slice(line).unwrap();
            assert!(v["ID"].is_i64(), "{}: {v}", case.name);
        }
        assert!(
            (*code, err.as_slice()) == (0, &b""[..])
                || (*code, err.as_slice()) == (1, &b"jig gocacheprog: bad request line\n"[..]),
            "{}: {r:?}",
            case.name
        );
    }
}
