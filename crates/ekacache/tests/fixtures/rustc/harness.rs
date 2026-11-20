//! Used by `rustc_mode.rs`: an ecd stand-in that keeps values in
//! memory, a recording proxy in front of a real ecd (both log every request an ec makes), the
//! fake rustc of `fake-rustc.sh`, and helpers to run an ec and read its `$EKACACHE_LOG`.
#![allow(dead_code, clippy::unwrap_used)]

use ekacache_testkit::busy::CommandBusyExt as _;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use ekacache_core::wire::{Request, parse_request};

/// The Rust ec under test.
pub const EC: &str = env!("CARGO_BIN_EXE_ec");

/// One request as the daemon saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Req {
    /// Which connection (in accept order).
    pub conn: usize,
    pub verb: String,
    /// The key of GET/HAS/PUT, the build of SLOT/DONE, the number of HELLO/IDS.
    pub arg: Vec<u8>,
    /// A PUT's value, unframed (`None` when it does not decode).
    pub value: Option<Vec<u8>>,
    /// The paths of an IDS.
    pub paths: Vec<Vec<u8>>,
}

impl Req {
    /// `VERB arg`.
    pub fn line(&self) -> String {
        format!("{} {}", self.verb, String::from_utf8_lossy(&self.arg))
    }
}

#[derive(Default)]
pub struct State {
    pub log: Vec<Req>,
    /// The in-memory daemon's values (unframed).
    pub store: HashMap<Vec<u8>, Vec<u8>>,
    /// In-memory daemon: hang up instead of storing a PUT whose key starts with this.
    pub refuse_put_prefix: Option<Vec<u8>>,
    conns: usize,
}

/// A daemon socket whose requests are recorded.
pub struct Server {
    pub sock: PathBuf,
    state: Arc<Mutex<State>>,
    _dir: tempfile::TempDir,
}

impl Server {
    /// An in-memory ecd: HELLO → `OK 1`, GET/HAS from memory, PUT stored, IDS answered with
    /// empty lines (no identity), SLOT and DONE → `OK`.
    pub fn fake() -> Self {
        Self::start(None)
    }

    /// A proxy in front of the ecd at `upstream`, which answers everything.
    pub fn proxy(upstream: &Path) -> Self {
        Self::start(Some(upstream.to_path_buf()))
    }

    fn start(upstream: Option<PathBuf>) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("jr")
            .tempdir_in("/tmp")
            .unwrap();
        let sock = dir.path().join("s");
        let listener = UnixListener::bind(&sock).unwrap();
        let state = Arc::new(Mutex::new(State::default()));
        let st = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let conn = {
                    let mut s = st.lock().unwrap();
                    s.conns += 1;
                    s.conns
                };
                let st = Arc::clone(&st);
                let upstream = upstream.clone();
                std::thread::spawn(move || match upstream {
                    None => serve(stream, conn, &st),
                    Some(up) => forward(stream, conn, &up, &st),
                });
            }
        });
        Self {
            sock,
            state,
            _dir: dir,
        }
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }

    /// The requests so far, and forget them.
    pub fn take_log(&self) -> Vec<Req> {
        std::mem::take(&mut self.state.lock().unwrap().log)
    }

    pub fn value(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.with(|s| s.store.get(key).cloned())
    }

    pub fn put(&self, key: &[u8], value: &[u8]) {
        self.with(|s| s.store.insert(key.to_vec(), value.to_vec()));
    }

    pub fn remove(&self, key: &[u8]) {
        self.with(|s| s.store.remove(key));
    }
}

/// `VERB arg` of each request, connections merged in order.
pub fn lines(log: &[Req]) -> Vec<String> {
    log.iter().map(Req::line).collect()
}

/// A raw request line including its `\n`; `None` at end of stream.
fn read_raw_line(r: &mut impl BufRead) -> Option<Vec<u8>> {
    let mut line = Vec::new();
    match r.read_until(b'\n', &mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line),
    }
}

fn strip_nl(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\n").unwrap_or(line)
}

/// Read the rest of a request after its line (a PUT's body, an IDS's paths): the record and
/// the raw bytes read. An unparsable line is recorded with the verb `?<line>`.
fn read_request(line: &[u8], conn: usize, r: &mut impl BufRead) -> Option<(Req, Vec<u8>)> {
    let text = strip_nl(line);
    let mut req = Req {
        conn,
        verb: String::new(),
        arg: Vec::new(),
        value: None,
        paths: Vec::new(),
    };
    let mut raw = Vec::new();
    let (verb, arg): (String, Vec<u8>) = match parse_request(text) {
        None => (format!("?{}", String::from_utf8_lossy(text)), Vec::new()),
        Some(Request::Hello(v)) => ("HELLO".into(), v.to_string().into_bytes()),
        Some(Request::Get(k)) => ("GET".into(), k.to_vec()),
        Some(Request::Has(k)) => ("HAS".into(), k.to_vec()),
        Some(Request::Put(k, n)) => {
            let mut body = vec![0u8; usize::try_from(n).ok()?];
            r.read_exact(&mut body).ok()?;
            req.value = ekacache_core::frame::decode(&body);
            raw = body;
            ("PUT".into(), k.to_vec())
        }
        Some(Request::Ids(n)) => {
            for _ in 0..n {
                let l = read_raw_line(r)?;
                req.paths.push(strip_nl(&l).to_vec());
                raw.extend_from_slice(&l);
            }
            ("IDS".into(), n.to_string().into_bytes())
        }
        Some(Request::Slot(b)) => ("SLOT".into(), b.to_vec()),
        Some(Request::Done(b)) => ("DONE".into(), b.to_vec()),
        Some(Request::Stats) => ("STATS".into(), Vec::new()),
    };
    req.verb = verb;
    req.arg = arg;
    Some((req, raw))
}

fn serve(stream: UnixStream, conn: usize, st: &Mutex<State>) {
    let mut w = stream.try_clone().unwrap();
    let mut r = BufReader::new(stream);
    while let Some(line) = read_raw_line(&mut r) {
        let Some((req, _)) = read_request(&line, conn, &mut r) else {
            return;
        };
        let mut s = st.lock().unwrap();
        s.log.push(req.clone());
        let reply: Vec<u8> = match req.verb.as_str() {
            "HELLO" => b"OK 1\n".to_vec(),
            "GET" => match s.store.get(&req.arg) {
                Some(v) => {
                    let wire = ekacache_core::frame::encode(v).unwrap();
                    let mut out = format!("OK {}\n", wire.len()).into_bytes();
                    out.extend_from_slice(&wire);
                    out
                }
                None => b"MISS\n".to_vec(),
            },
            "HAS" => {
                if s.store.contains_key(&req.arg) {
                    b"1\n".to_vec()
                } else {
                    b"0\n".to_vec()
                }
            }
            "PUT" => {
                let refused = s
                    .refuse_put_prefix
                    .as_ref()
                    .is_some_and(|p| req.arg.starts_with(p));
                let Some(v) = req.value.clone().filter(|_| !refused) else {
                    return; // hang up
                };
                s.store.insert(req.arg.clone(), v);
                b"OK\n".to_vec()
            }
            "IDS" => vec![b'\n'; req.paths.len()],
            "SLOT" | "DONE" => b"OK\n".to_vec(),
            "STATS" => b"OK 0\n".to_vec(),
            _ => return, // hang up, as ecd does
        };
        drop(s);
        if w.write_all(&reply).is_err() {
            return;
        }
    }
}

fn forward(client: UnixStream, conn: usize, upstream: &Path, st: &Mutex<State>) {
    let Ok(up) = UnixStream::connect(upstream) else {
        return;
    };
    let mut up_w = up.try_clone().unwrap();
    let mut client_w = client.try_clone().unwrap();
    let mut up_r = up;
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut up_r, &mut client_w);
        let _ = client_w.shutdown(Shutdown::Both);
    });
    let mut r = BufReader::new(client);
    while let Some(line) = read_raw_line(&mut r) {
        let Some((req, raw)) = read_request(&line, conn, &mut r) else {
            break;
        };
        // recorded before the daemon can answer it
        st.lock().unwrap().log.push(req);
        if up_w.write_all(&line).is_err() || up_w.write_all(&raw).is_err() {
            break;
        }
    }
    let _ = up_w.shutdown(Shutdown::Write);
}

/// A crate directory: `src/lib.rs` (`pub mod m;`), `src/m.rs`, `out/`, and the fake rustc.
pub struct Ws {
    _tmp: tempfile::TempDir,
    pub root: PathBuf,
    pub rustc: PathBuf,
    pub out: PathBuf,
    pub log: PathBuf,
    pub rustc_log: PathBuf,
}

impl Ws {
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("out")).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub mod m;\n").unwrap();
        std::fs::write(root.join("src/m.rs"), "pub fn f() {}\n").unwrap();
        let rustc = fake_rustc(&root.join("bin"));
        Self {
            out: root.join("out"),
            log: root.join("ec.log"),
            rustc_log: root.join("rustc.log"),
            root,
            rustc,
            _tmp: tmp,
        }
    }

    pub fn p(&self, rel: &str) -> String {
        self.root.join(rel).to_str().unwrap().to_owned()
    }

    /// cargo's arguments for library crate `foo` with stem `-abc`, then `extra`.
    pub fn lib_args(&self, extra: &[&str]) -> Vec<OsString> {
        let mut a: Vec<OsString> = [
            "--crate-name",
            "foo",
            "--edition=2021",
            "src/lib.rs",
            "--crate-type",
            "lib",
            "--emit=dep-info,metadata,link",
            "-C",
            "metadata=abc",
            "-C",
            "extra-filename=-abc",
            "--out-dir",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        a.push(self.out.clone().into_os_string());
        a.extend(extra.iter().map(OsString::from));
        a
    }

    /// `ec <argv…>` (the Rust ec).
    pub fn ec(&self, sock: &Path, argv: &[OsString], env: &[(&str, &str)]) -> Output {
        self.ec_as(OsStr::new(EC), sock, argv, env)
    }

    /// `ec rustcwrap <fake rustc> <args…>` (the Rust ec).
    pub fn wrap(&self, sock: &Path, args: &[OsString], env: &[(&str, &str)]) -> Output {
        self.wrap_as(OsStr::new(EC), sock, args, env)
    }

    /// `<bin> <argv…>` in the crate directory with the fake rustc's log and
    /// `FAKE_INPUTS=src/m.rs` (overridable).
    pub fn ec_as(
        &self,
        bin: &OsStr,
        sock: &Path,
        argv: &[OsString],
        env: &[(&str, &str)],
    ) -> Output {
        let mut e: Vec<(&str, &OsStr)> = vec![
            ("FAKE_RUSTC_LOG", self.rustc_log.as_os_str()),
            ("FAKE_INPUTS", os("src/m.rs")),
        ];
        e.extend(env.iter().map(|(k, v)| (*k, os(v))));
        let argv: Vec<&OsStr> = argv.iter().map(OsString::as_os_str).collect();
        run_in(bin, &argv, &self.root, sock, &self.log, &e)
    }

    /// `<bin> rustcwrap <fake rustc> <args…>`.
    pub fn wrap_as(
        &self,
        bin: &OsStr,
        sock: &Path,
        args: &[OsString],
        env: &[(&str, &str)],
    ) -> Output {
        let mut argv = vec![
            OsString::from("rustcwrap"),
            self.rustc.clone().into_os_string(),
        ];
        argv.extend_from_slice(args);
        self.ec_as(bin, sock, &argv, env)
    }

    pub fn rustc_runs(&self) -> usize {
        std::fs::read_to_string(&self.rustc_log)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    pub fn clear_out(&self) {
        std::fs::remove_dir_all(&self.out).unwrap();
        std::fs::create_dir(&self.out).unwrap();
    }
}

/// The fake rustc, installed as `<dir>/rustc`.
pub fn fake_rustc(dir: &Path) -> PathBuf {
    let src = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/rustc/fake-rustc.sh"
    );
    let path = dir.join("rustc");
    std::fs::copy(src, &path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Run `bin args…` in `cwd` with only `PATH`, `EKACACHE_SOCK`, `EKACACHE_LOG` and `env` set.
pub fn run_in(
    bin: &OsStr,
    args: &[&OsStr],
    cwd: &Path,
    sock: &Path,
    ec_log: &Path,
    env: &[(&str, &OsStr)],
) -> Output {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("EKACACHE_SOCK", sock)
        .env("EKACACHE_LOG", ec_log);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output_busy().unwrap()
}

/// `(outcome, subject)` of each `$EKACACHE_LOG` line, after checking the tool name and the time.
pub fn read_log(path: &Path) -> Vec<(String, String)> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            assert_eq!(f.len(), 4, "log line {l:?}");
            assert_eq!(f[0], "rustc", "log line {l:?}");
            assert!(f[3].parse::<u64>().is_ok(), "log line {l:?}");
            (f[1].to_owned(), f[2].to_owned())
        })
        .collect()
}

/// The log lines written since the last call, and truncate the file.
pub fn take_log(path: &Path) -> Vec<(String, String)> {
    let l = read_log(path);
    let _ = std::fs::remove_file(path);
    l
}

/// Files by relative path: bytes and mode.
pub type Snapshot = std::collections::BTreeMap<String, (Vec<u8>, u32)>;

/// Every file under `dir`, recursively.
pub fn snapshot(dir: &Path) -> Snapshot {
    fn walk(base: &Path, d: &Path, out: &mut Snapshot) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for e in rd {
            let p = e.unwrap().path();
            let md = std::fs::symlink_metadata(&p).unwrap();
            if md.is_dir() {
                walk(base, &p, out);
            } else {
                let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
                out.insert(
                    rel,
                    (
                        std::fs::read(&p).unwrap_or_default(),
                        md.permissions().mode() & 0o7777,
                    ),
                );
            }
        }
    }
    let mut out = Snapshot::new();
    walk(dir, dir, &mut out);
    out
}

/// A real rustc on `PATH` (`rustc -vV` works).
pub fn have_rustc() -> bool {
    Command::new("rustc")
        .arg("-vV")
        .output_busy()
        .is_ok_and(|o| o.status.success())
}

/// `s` as an `OsStr`.
pub fn os(s: &str) -> &OsStr {
    OsStr::new(s)
}
