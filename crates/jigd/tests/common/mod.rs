//! Helpers shared by the jigd server tests: a blocking protocol client, an in-process server on
//! a temporary socket, and a daemon process (the Rust binary or the Go one) on its own root.

// not every test crate uses every helper
#![allow(dead_code)]
// helpers outside #[test] fns: a panic is the failure report
#![allow(clippy::unwrap_used)]

use std::io::{ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use jigd::identity::Identities;
use jigd::log::{LogCapture, Logger};
use jigd::peer::Auth;
use jigd::server::{self, Ctx};
use jigd::slots::Slots;
use jigd::store::{Store, StoreConfig, Tier};
use tokio::sync::Notify;

/// How long a read waits before the test calls the daemon stuck.
pub const PATIENCE: Duration = Duration::from_secs(10);
/// How long "nothing arrives" is watched for.
pub const QUIET: Duration = Duration::from_millis(300);

/// How a stream of replies ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// the daemon closed the connection after reading everything sent
    Eof,
    /// the daemon closed it with request bytes unread (Linux: `ECONNRESET` after the data)
    Reset,
    /// still open when the patience ran out
    Open,
    /// any other read error
    Errno(i32),
}

/// A protocol client with its own read buffer, so that reads can time out without losing bytes.
pub struct Client {
    s: UnixStream,
    buf: Vec<u8>,
    closed: Option<End>,
}

impl Client {
    pub fn connect(sock: &Path) -> Client {
        let s =
            UnixStream::connect(sock).unwrap_or_else(|e| panic!("connect {}: {e}", sock.display()));
        s.set_read_timeout(Some(PATIENCE)).unwrap();
        Client {
            s,
            buf: Vec::new(),
            closed: None,
        }
    }

    pub fn stream(&self) -> &UnixStream {
        &self.s
    }

    /// Write everything; a failed write is ignored (the daemon may have hung up already, and
    /// how far a write got before that is timing).
    pub fn send(&mut self, b: &[u8]) {
        let _ = self.s.write_all(b);
    }

    /// `shutdown(SHUT_WR)`: the daemon sees EOF after what was sent.
    pub fn half_close(&self) {
        let _ = self.s.shutdown(Shutdown::Write);
    }

    /// One read into the buffer, waiting at most `wait`. False if nothing came.
    fn fill(&mut self, wait: Duration) -> bool {
        if self.closed.is_some() {
            return false;
        }
        self.s.set_read_timeout(Some(wait)).unwrap();
        let mut chunk = vec![0u8; 1 << 16];
        let got = loop {
            match self.s.read(&mut chunk) {
                Ok(0) => {
                    self.closed = Some(End::Eof);
                    break false;
                }
                Ok(n) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    break true;
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    break false;
                }
                Err(e) if e.raw_os_error() == Some(libc::ECONNRESET) => {
                    self.closed = Some(End::Reset);
                    break false;
                }
                Err(e) => {
                    self.closed = Some(End::Errno(e.raw_os_error().unwrap_or(-1)));
                    break false;
                }
            }
        };
        self.s.set_read_timeout(Some(PATIENCE)).unwrap();
        got
    }

    /// One reply line with its `\n`; panics if the daemon hangs up or stalls first.
    pub fn line(&mut self) -> Vec<u8> {
        self.try_line().unwrap_or_else(|| {
            panic!(
                "no reply line: got {:?}, {:?}",
                show(&self.buf),
                self.closed
            )
        })
    }

    pub fn line_str(&mut self) -> String {
        String::from_utf8(self.line()).unwrap()
    }

    /// One reply line, or `None` (and the partial bytes stay buffered) at the end of the stream.
    pub fn try_line(&mut self) -> Option<Vec<u8>> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
                return Some(self.buf.drain(..=i).collect());
            }
            if self.closed.is_some() || Instant::now() > deadline {
                return None;
            }
            self.fill(PATIENCE);
        }
    }

    /// Exactly `n` bytes, or `None` (and what came stays buffered) at the end of the stream.
    pub fn try_exact(&mut self, n: usize) -> Option<Vec<u8>> {
        let deadline = Instant::now() + PATIENCE;
        while self.buf.len() < n {
            if self.closed.is_some() || Instant::now() > deadline {
                return None;
            }
            self.fill(PATIENCE);
        }
        Some(self.buf.drain(..n).collect())
    }

    /// Exactly `n` bytes; panics if they do not come.
    pub fn exact(&mut self, n: usize) -> Vec<u8> {
        let deadline = Instant::now() + PATIENCE;
        while self.buf.len() < n {
            assert!(
                self.closed.is_none() && Instant::now() < deadline,
                "wanted {n} bytes, got {} and {:?}",
                self.buf.len(),
                self.closed
            );
            self.fill(PATIENCE);
        }
        self.buf.drain(..n).collect()
    }

    /// Everything until the daemon closes the connection (or `PATIENCE` runs out).
    pub fn rest(&mut self) -> (Vec<u8>, End) {
        let deadline = Instant::now() + PATIENCE;
        while self.closed.is_none() && Instant::now() < deadline {
            self.fill(PATIENCE);
        }
        (
            std::mem::take(&mut self.buf),
            self.closed.unwrap_or(End::Open),
        )
    }

    /// Whatever arrives within `wait`, without waiting for the end.
    pub fn within(&mut self, wait: Duration) -> Vec<u8> {
        let deadline = Instant::now() + wait;
        loop {
            let now = Instant::now();
            if now >= deadline || self.closed.is_some() {
                break;
            }
            self.fill(deadline - now);
        }
        std::mem::take(&mut self.buf)
    }

    /// True if nothing arrives (and the connection stays open) for `QUIET`.
    pub fn quiet(&mut self) -> bool {
        self.buf.is_empty() && !self.fill(QUIET) && self.closed.is_none()
    }

    pub fn end(&self) -> Option<End> {
        self.closed
    }

    /// `STATS` and its reply, without the newline.
    pub fn stats(&mut self) -> String {
        self.send(b"STATS\n");
        let l = self.line_str();
        l.trim_end_matches('\n').to_owned()
    }

    /// `PUT key len\nval` and its `OK`.
    pub fn put(&mut self, key: &[u8], val: &[u8]) {
        self.send(&put_req(key, val));
        assert_eq!(show(&self.line()), "OK\\n", "PUT {}", show(key));
    }

    /// `GET key`: the value, or `None` on `MISS`.
    pub fn get(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        let mut req = b"GET ".to_vec();
        req.extend_from_slice(key);
        req.push(b'\n');
        self.send(&req);
        let l = self.line_str();
        if l == "MISS\n" {
            return None;
        }
        let n: usize = l
            .strip_prefix("OK ")
            .and_then(|s| s.trim_end().parse().ok())
            .unwrap_or_else(|| panic!("GET reply {l:?}"));
        Some(self.exact(n))
    }
}

/// `PUT key len\n` and the value.
pub fn put_req(key: &[u8], val: &[u8]) -> Vec<u8> {
    let mut req = b"PUT ".to_vec();
    req.extend_from_slice(key);
    req.extend_from_slice(format!(" {}\n", val.len()).as_bytes());
    req.extend_from_slice(val);
    req
}

/// Bytes for an assertion message: escaped, and long runs shortened.
pub fn show(b: &[u8]) -> String {
    const MAX: usize = 300;
    if b.len() > MAX {
        format!("{}... ({} bytes)", escape(&b[..MAX]), b.len())
    } else {
        escape(b)
    }
}

/// Bytes as an ASCII string, everything else escaped.
pub fn escape(b: &[u8]) -> String {
    b.iter()
        .flat_map(|&c| std::ascii::escape_default(c))
        .map(char::from)
        .collect()
}

/// Poll `f` until it is true or `PATIENCE` runs out.
pub fn eventually(mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

/// The jigd server in this process, on `dir/s`, with its store in `dir/packs` and `dir` as the
/// identity store.
pub struct InProc {
    rt: Option<tokio::runtime::Runtime>,
    pub ctx: Arc<Ctx>,
    pub sock: PathBuf,
    pub logs: LogCapture,
    stop: Arc<Notify>,
    pub dir: tempfile::TempDir,
}

impl InProc {
    pub fn start(slots: i64) -> InProc {
        InProc::with_auth(slots, Auth::from_system())
    }

    pub fn with_auth(slots: i64, auth: Auth) -> InProc {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().as_os_str().as_encoded_bytes().to_vec();
        let (log, logs) = Logger::capture();
        let mut cfg = StoreConfig::new(vec![Tier {
            dir: [root.as_slice(), b"/packs"].concat(),
            budget: 1 << 30,
        }]);
        cfg.logger = log.clone();
        let store = Store::open(cfg).unwrap();
        let idents = Identities::new(&root);
        let ctx = Arc::new(Ctx::new(store, idents, Slots::new(slots), auth, log));
        let sock = dir.path().join("s");
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = jigd::daemon::listen_unix(sock.as_os_str().as_encoded_bytes()).unwrap();
        let listener = {
            let _guard = rt.enter();
            tokio::net::UnixListener::from_std(listener).unwrap()
        };
        let stop = Arc::new(Notify::new());
        let (c, s) = (Arc::clone(&ctx), Arc::clone(&stop));
        rt.spawn(async move { server::accept_loop(&listener, &c, &s).await });
        InProc {
            rt: Some(rt),
            ctx,
            sock,
            logs,
            stop,
            dir,
        }
    }

    pub fn connect(&self) -> Client {
        Client::connect(&self.sock)
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// `(slots out, waiting)`.
    pub fn slots(&self) -> (i64, usize) {
        self.ctx.slots.stats()
    }
}

impl Drop for InProc {
    fn drop(&mut self) {
        self.stop.notify_one();
        if let Some(rt) = self.rt.take() {
            rt.shutdown_timeout(Duration::from_secs(5));
        }
    }
}

/// A daemon process on `root/sock`, its cache in `root/cache`.
pub struct Daemon {
    child: Child,
    pub sock: PathBuf,
    pub root: PathBuf,
    /// stderr so far, filled by `reader`
    log: Arc<Mutex<Vec<u8>>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

/// The Rust binary under test.
pub fn rust_jigd() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_jigd"))
}

/// The Go binary, from `JIG_LEGACY_JIGD`; `None` (and a note) when it is not set.
pub fn legacy_jigd() -> Option<PathBuf> {
    match std::env::var_os("JIG_LEGACY_JIGD") {
        Some(p) if !p.is_empty() => Some(PathBuf::from(p)),
        _ => {
            eprintln!("note: JIG_LEGACY_JIGD is not set; skipping the comparison with the Go jigd");
            None
        }
    }
}

impl Daemon {
    /// Start `bin` with a clean environment plus `env`, and wait until it accepts.
    pub fn start(bin: &Path, root: &Path, env: &[(&str, &str)]) -> Daemon {
        let sock = root.join("sock");
        let mut cmd = Command::new(bin);
        cmd.arg(&sock)
            .env_clear()
            .env("XDG_CACHE_HOME", root.join("cache"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .unwrap_or_else(|e| panic!("start {}: {e}", bin.display()));
        let log: Arc<Mutex<Vec<u8>>> = Arc::default();
        let mut stderr = child.stderr.take().unwrap();
        let sink = Arc::clone(&log);
        let reader = std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&chunk[..n]);
            }
        });
        let mut d = Daemon {
            child,
            sock,
            root: root.to_path_buf(),
            log,
            reader: Some(reader),
        };
        // the startup line comes after the signal handlers are installed (step 11): a SIGTERM
        // before it would kill the daemon (Q-DAEMON-11)
        let deadline = Instant::now() + Duration::from_secs(20);
        while !d
            .log
            .lock()
            .unwrap()
            .windows(13)
            .any(|w| w == b"listening on ")
        {
            if let Ok(Some(status)) = d.child.try_wait() {
                let (_, log) = d.wait();
                panic!(
                    "{} exited {status:?}: {}",
                    bin.display(),
                    String::from_utf8_lossy(&log)
                );
            }
            assert!(
                Instant::now() < deadline,
                "{} did not start listening",
                bin.display()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        d
    }

    pub fn connect(&self) -> Client {
        Client::connect(&self.sock)
    }

    pub fn pid(&self) -> rustix::process::Pid {
        rustix::process::Pid::from_child(&self.child)
    }

    /// SIGTERM, wait, and return the exit status and the log.
    pub fn stop(mut self) -> (ExitStatus, Vec<u8>) {
        rustix::process::kill_process(self.pid(), rustix::process::Signal::TERM).unwrap();
        self.wait()
    }

    pub fn wait(&mut self) -> (ExitStatus, Vec<u8>) {
        let status = self.child.wait().unwrap();
        if let Some(r) = self.reader.take() {
            r.join().unwrap();
        }
        (status, std::mem::take(&mut *self.log.lock().unwrap()))
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What a run of the binary to its exit gave.
#[derive(Debug, PartialEq, Eq)]
pub struct Run {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Run `bin args` to its exit with only `env` in the environment.
pub fn run(bin: &Path, args: &[&[u8]], env: &[(&str, &[u8])]) -> Run {
    use std::os::unix::ffi::OsStrExt;
    let mut cmd = Command::new(bin);
    cmd.env_clear().stdin(Stdio::null());
    for a in args {
        cmd.arg(std::ffi::OsStr::from_bytes(a));
    }
    for (k, v) in env {
        cmd.env(k, std::ffi::OsStr::from_bytes(v));
    }
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("run {}: {e}", bin.display()));
    Run {
        code: out.status.code(),
        stdout: out.stdout,
        stderr: out.stderr,
    }
}

/// Log lines without Go's `2006/01/02 15:04:05 ` prefix, `root` replaced by `$ROOT`.
pub fn log_lines(log: &[u8], root: &Path) -> Vec<String> {
    let root = root.as_os_str().as_encoded_bytes();
    log.split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| {
            let l = if l.len() >= 20
                && l[4] == b'/'
                && l[7] == b'/'
                && l[10] == b' '
                && l[13] == b':'
            {
                &l[20..]
            } else {
                l
            };
            escape(&replace(l, root, b"$ROOT"))
        })
        .collect()
}

/// Every occurrence of `from` in `hay` replaced by `to`.
pub fn replace(hay: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(hay.len());
    let mut i = 0;
    while i < hay.len() {
        if !from.is_empty() && hay[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    out
}
