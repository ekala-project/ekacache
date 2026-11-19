//! Helpers for the `gocache_*` tests: an in-process fake jigd, and running a `jig gocacheprog`
//! with given stdin.
//!
//! The fake jigd speaks the subset of `behaviors/jigd-protocol.md` the Go cache program uses:
//! `HELLO 1` (→ `OK 1`), `GET`, `PUT` (values framed by `ekacache_core::frame`, stored decoded), and
//! hangs up on anything else. Beyond a real jigd it can evict any key, refuse PUTs (hang up
//! after reading one, as jigd does for a rejected value), and hold a connection before a
//! request with a given prefix until released, which is how the Quint replay interleaves two
//! processes at the granularity of single daemon requests (`spec/gocache.qnt`).
#![allow(dead_code, clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// The Rust jig under test.
pub const JIG: &str = env!("CARGO_BIN_EXE_ec");

/// The handshake (`jig-gocacheprog.md §Handshake`).
pub const HANDSHAKE: &str = "{\"ID\":0,\"KnownCommands\":[\"get\",\"put\",\"close\"]}\n";

pub fn b64(b: &[u8]) -> String {
    STANDARD.encode(b)
}

pub fn hex(b: &[u8]) -> String {
    String::from_utf8(ekacache_core::bytes::hex_encode(b)).unwrap()
}

#[derive(Default)]
struct State {
    kv: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Every request line, with the connection number (accept order, from 0).
    log: Vec<(usize, Vec<u8>)>,
    conns: usize,
    refuse_puts: bool,
    /// `(connection, request prefix)` pairs to hold before executing.
    holds: BTreeSet<(usize, Vec<u8>)>,
    /// Connections currently waiting on a hold, with the held request line.
    blocked: BTreeMap<usize, Vec<u8>>,
}

/// A fake jigd on a socket in a fresh temp directory; its threads end with the test process.
pub struct FakeJigd {
    pub sock: PathBuf,
    _dir: tempfile::TempDir,
    state: Arc<(Mutex<State>, Condvar)>,
}

impl FakeJigd {
    pub fn start() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("jgc")
            .tempdir_in("/tmp")
            .unwrap();
        let sock = dir.path().join("s");
        let listener = UnixListener::bind(&sock).unwrap();
        let state = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let st = Arc::clone(&state);
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { return };
                let id = {
                    let mut s = st.0.lock().unwrap();
                    s.conns += 1;
                    st.1.notify_all();
                    s.conns - 1
                };
                let st = Arc::clone(&st);
                std::thread::spawn(move || serve(id, conn, &st));
            }
        });
        Self {
            sock,
            _dir: dir,
            state,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.0.lock().unwrap()
    }

    pub fn kv(&self) -> BTreeMap<Vec<u8>, Vec<u8>> {
        self.lock().kv.clone()
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.lock().kv.get(key.as_bytes()).cloned()
    }

    pub fn insert(&self, key: &str, value: &[u8]) {
        self.lock()
            .kv
            .insert(key.as_bytes().to_vec(), value.to_vec());
    }

    pub fn evict(&self, key: &[u8]) {
        self.lock().kv.remove(key);
    }

    /// The request lines received so far, as text, without HELLO.
    pub fn requests(&self) -> Vec<String> {
        self.lock()
            .log
            .iter()
            .filter(|(_, l)| !l.starts_with(b"HELLO"))
            .map(|(_, l)| String::from_utf8_lossy(l).into_owned())
            .collect()
    }

    pub fn log(&self) -> Vec<(usize, Vec<u8>)> {
        self.lock().log.clone()
    }

    pub fn refuse_puts(&self) {
        self.lock().refuse_puts = true;
    }

    pub fn hold(&self, conn: usize, prefix: &str) {
        self.lock().holds.insert((conn, prefix.as_bytes().to_vec()));
    }

    /// Lift every hold on `conn`.
    pub fn release(&self, conn: usize) {
        let mut s = self.lock();
        s.holds.retain(|(c, _)| *c != conn);
        self.state.1.notify_all();
    }

    /// The request `conn` waits on, if it is held.
    pub fn blocked(&self, conn: usize) -> Option<Vec<u8>> {
        self.lock().blocked.get(&conn).cloned()
    }

    /// Wait until `n` connections have been accepted.
    pub fn wait_conns(&self, n: usize) {
        let t = Instant::now();
        let mut s = self.lock();
        while s.conns < n {
            assert!(t.elapsed() < Duration::from_secs(20), "no connection");
            s = self
                .state
                .1
                .wait_timeout(s, Duration::from_millis(50))
                .unwrap()
                .0;
        }
    }
}

fn serve(id: usize, conn: UnixStream, st: &(Mutex<State>, Condvar)) {
    let mut w = conn.try_clone().unwrap();
    let mut r = BufReader::new(conn);
    loop {
        let mut line = Vec::new();
        if r.read_until(b'\n', &mut line).unwrap_or(0) == 0 || line.last() != Some(&b'\n') {
            return;
        }
        line.pop();
        {
            let mut s = st.0.lock().unwrap();
            s.log.push((id, line.clone()));
            while s.holds.iter().any(|(c, p)| *c == id && line.starts_with(p)) {
                s.blocked.insert(id, line.clone());
                st.1.notify_all();
                s = st.1.wait(s).unwrap();
            }
            s.blocked.remove(&id);
        }
        let words: Vec<&[u8]> = line.split(|&c| c == b' ').collect();
        let reply: Vec<u8> = match words.as_slice() {
            [b"HELLO", _] => b"OK 1\n".to_vec(),
            [b"GET", key] => {
                let value = st.0.lock().unwrap().kv.get(*key).cloned();
                match value {
                    Some(v) => {
                        let wire = ekacache_core::frame::encode(&v).unwrap();
                        let mut out = format!("OK {}\n", wire.len()).into_bytes();
                        out.extend_from_slice(&wire);
                        out
                    }
                    None => b"MISS\n".to_vec(),
                }
            }
            [b"PUT", key, len] => {
                let len: usize = std::str::from_utf8(len).unwrap().parse().unwrap();
                let mut wire = vec![0; len];
                if r.read_exact(&mut wire).is_err() {
                    return;
                }
                let mut s = st.0.lock().unwrap();
                if s.refuse_puts {
                    return;
                }
                let value = ekacache_core::frame::decode(&wire).unwrap();
                s.kv.insert(key.to_vec(), value);
                b"OK\n".to_vec()
            }
            _ => return,
        };
        if w.write_all(&reply).is_err() {
            return;
        }
    }
}

/// How a `jig gocacheprog` is run.
pub struct Run<'a> {
    pub bin: &'a OsStr,
    /// `JIG_SOCK`; `None` gives a path that does not exist (not live).
    pub sock: Option<&'a Path>,
    /// Extra environment, after `PATH` and `JIG_SOCK`.
    pub env: Vec<(&'a str, &'a OsStr)>,
    pub cwd: Option<&'a Path>,
}

impl<'a> Run<'a> {
    pub fn new(bin: &'a OsStr, sock: Option<&'a Path>) -> Self {
        Self {
            bin,
            sock,
            env: Vec::new(),
            cwd: None,
        }
    }

    pub fn env(mut self, k: &'a str, v: &'a OsStr) -> Self {
        self.env.push((k, v));
        self
    }

    pub fn cwd(mut self, dir: &'a Path) -> Self {
        self.cwd = Some(dir);
        self
    }

    /// Run with `stdin` written by a thread (and then closed), collecting stdout and stderr.
    pub fn stdin(&self, stdin: &[u8]) -> Output {
        let mut cmd = self.command();
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        let data = stdin.to_vec();
        // an early exit (close, a fatal line) makes the rest fail with EPIPE
        let writer = std::thread::spawn(move || {
            let _ = input.write_all(&data);
        });
        let out = child.wait_with_output().unwrap();
        writer.join().unwrap();
        out
    }

    pub fn command(&self) -> Command {
        let mut cmd = Command::new(self.bin);
        cmd.arg("gocacheprog")
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env(
                "JIG_SOCK",
                self.sock
                    .map_or(OsStr::new("/nonexistent/jigd/socket"), Path::as_os_str),
            );
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        if let Some(d) = self.cwd {
            cmd.current_dir(d);
        }
        cmd
    }
}

/// `JIG_LOG` rows without the `<ms>` column (checked to be a number).
pub fn log_rows(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .map(|l| {
            let (head, ms) = l.rsplit_once('\t').unwrap();
            assert!(ms.parse::<u64>().is_ok(), "{l}");
            head.to_owned()
        })
        .collect()
}

/// The files of a directory, by name (not recursive).
pub fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return BTreeMap::new();
    };
    rd.map(|e| {
        let e = e.unwrap();
        (
            e.file_name().to_string_lossy().into_owned(),
            std::fs::read(e.path()).unwrap_or_default(),
        )
    })
    .collect()
}

/// A `get` request line, as Go writes it (compact, then an empty line).
pub fn get(id: i64, action: &[u8]) -> String {
    format!(
        "{{\"ID\":{id},\"Command\":\"get\",\"ActionID\":\"{}\"}}\n\n",
        b64(action)
    )
}

/// A `put` request as Go writes it: the request, an empty line, and the body line when the
/// body is not empty.
pub fn put(id: i64, action: &[u8], output: &[u8], body: &[u8]) -> String {
    if body.is_empty() {
        return format!(
            "{{\"ID\":{id},\"Command\":\"put\",\"ActionID\":\"{}\",\"OutputID\":\"{}\"}}\n\n",
            b64(action),
            b64(output)
        );
    }
    format!(
        "{{\"ID\":{id},\"Command\":\"put\",\"ActionID\":\"{}\",\"OutputID\":\"{}\",\"BodySize\":{}}}\n\n\"{}\"\n",
        b64(action),
        b64(output),
        body.len(),
        b64(body)
    )
}

pub fn close(id: i64) -> String {
    format!("{{\"ID\":{id},\"Command\":\"close\"}}\n\n")
}
