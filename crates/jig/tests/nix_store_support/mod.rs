//! Support for the `jig nix-store` integration tests (`implementation/09-jig-nix-store.md
//! §Tests`): worker-protocol byte builders, a scriptable fake builder-rpc daemon, and a runner
//! that runs the Rust jig and, when `JIG_LEGACY_JIG` names a legacy `jig` binary, the legacy jig
//! against the same script, then compares the client→daemon bytes, stdout, stderr and the exit
//! status or signal.
//!
//! The fake daemon lives here rather than in `jig-testkit` because only these tests use it.
#![allow(dead_code)]

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const JIG: &str = env!("CARGO_BIN_EXE_jig");

pub const MAGIC_1: u64 = 0x6e69_7863;
pub const MAGIC_2: u64 = 0x6478_696f;
pub const V1_38: u64 = 0x126;
pub const LAST: u64 = 0x616c_7473;
pub const ERROR: u64 = 0x6378_7470;
pub const NEXT: u64 = 0x6f6c_6d67;
pub const START_ACTIVITY: u64 = 0x5354_5254;
pub const STOP_ACTIVITY: u64 = 0x5354_4f50;
pub const RESULT: u64 = 0x5253_4c54;
pub const MAX_STRING: u64 = 64 << 20;

// ---- messages and vectors (`behaviors/jig-nix-store.md §Errors and messages`, `§Test vectors`)

pub const USAGE: &[u8] = "usage: jig nix-store add-text|add-drv|fetchurls|submit …".as_bytes();
pub const BAD_ARGUMENTS: &[u8] = b"jig nix-store: bad arguments";
pub const DID_NOT_PARSE: &[u8] = b"jig nix-store: derivation JSON did not parse";
pub const NOT_AN_ARRAY: &[u8] = b"jig nix-store fetchurls: expected a JSON array";
pub const ATERM_JSON: &[u8] = br#"{"name":"x","system":"x86_64-linux","builder":"/b","args":["-c","echo \"hi\"\n"],"env":{"out":"/o","b":"1","a":"2"},"inputSrcs":["/s2","/s1"],"inputDrvs":{"/d.drv":["out","dev"]},"outputs":{"out":{"hashAlgo":"r:sha256"}}}"#;
pub const ATERM: &[u8] = br#"Derive([("out","","r:sha256","")],[("/d.drv",["dev","out"])],["/s1","/s2"],"x86_64-linux","/b",["-c","echo \"hi\"\n"],[("a","2"),("b","1"),("out","/o")])"#;
pub const FETCH_ONE: &[u8] = br#"[{"name":"foo-1.0.tar.gz","url":"https://example.org/foo-1.0.tar.gz","algo":"sha256","hex":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","sri":"sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="}]"#;
pub const FETCH_ONE_OUT: &[u8] = br#"[{"drv":"/nix/store/6ymfza6jbp5h9vha2mza1c5fkr9xgpwb-foo-1.0.tar.gz.drv","out":"/nix/store/hq2j18s54yzc9ahyw79v8zdn5pya2ca9-foo-1.0.tar.gz"}]"#;
pub const FETCH_ONE_ATERM: &[u8] = br#"Derive([("out","/nix/store/hq2j18s54yzc9ahyw79v8zdn5pya2ca9-foo-1.0.tar.gz","sha256","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")],[],[],"builtin","builtin:fetchurl",[],[("builder","builtin:fetchurl"),("executable",""),("impureEnvVars","http_proxy https_proxy ftp_proxy all_proxy no_proxy"),("name","foo-1.0.tar.gz"),("out","/nix/store/hq2j18s54yzc9ahyw79v8zdn5pya2ca9-foo-1.0.tar.gz"),("outputHash","sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="),("outputHashAlgo",""),("outputHashMode","flat"),("preferLocalBuild","1"),("system","builtin"),("unpack",""),("url","https://example.org/foo-1.0.tar.gz"),("urls","https://example.org/foo-1.0.tar.gz")])"#;

/// How long a client may run before the test gives up on it.
const CLIENT_DEADLINE: Duration = Duration::from_secs(60);
/// How long the daemon waits for client bytes before giving up on a script step.
const DAEMON_READ_TIMEOUT: Duration = Duration::from_secs(30);

// ---- encoding ---------------------------------------------------------------------------

/// A table row: arguments, stdin, and the expected stdout or stderr line.
pub type Row<'a> = (&'a [&'a [u8]], &'a [u8], &'a [u8]);

pub fn word(v: u64) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

pub fn string(s: &[u8]) -> Vec<u8> {
    let mut out = word(s.len() as u64);
    out.extend_from_slice(s);
    out.resize(out.len() + (8 - s.len() % 8) % 8, 0);
    out
}

pub fn strings(list: &[&[u8]]) -> Vec<u8> {
    let mut out = word(list.len() as u64);
    for s in list {
        out.extend(string(s));
    }
    out
}

pub fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

// ---- what the client sends ---------------------------------------------------------------

/// `WORKER_MAGIC_1`, `0x126` (16 bytes).
pub fn client_hello() -> Vec<u8> {
    cat(&[&word(MAGIC_1), &word(V1_38)])
}

/// The feature list (136 bytes).
pub fn client_features() -> Vec<u8> {
    strings(&[
        b"realisation-with-path-not-hash",
        b"disable-set-options",
        b"add-to-store-scanning",
        b"submit-output",
    ])
}

/// CPU affinity and reserveSpace (16 zero bytes).
pub fn client_post() -> Vec<u8> {
    vec![0; 16]
}

pub fn client_handshake() -> Vec<u8> {
    cat(&[&client_hello(), &client_features(), &client_post()])
}

/// AddToStore with `refs` in the order the client sends them (sorted).
pub fn client_add(name: &[u8], refs: &[&[u8]], contents: &[u8]) -> Vec<u8> {
    cat(&[
        &word(7),
        &string(name),
        &string(b"text:sha256"),
        &strings(refs),
        &word(0),
        &word(contents.len() as u64),
        contents,
        &word(0),
    ])
}

pub fn client_submit(path: &[u8], output: &[u8]) -> Vec<u8> {
    cat(&[&word(1000), &word(0), &string(path), &string(output)])
}

// ---- what the daemon sends ---------------------------------------------------------------

pub fn daemon_hello(version: u64) -> Vec<u8> {
    cat(&[&word(MAGIC_2), &word(version)])
}

/// The daemon's version string and trust word.
pub fn daemon_info() -> Vec<u8> {
    cat(&[&string(b"2.99.0-fake"), &word(1)])
}

pub fn next(text: &[u8]) -> Vec<u8> {
    cat(&[&word(NEXT), &string(text)])
}

/// A whole `STDERR_ERROR`, with the trailing fields the client does not read.
pub fn error(msg: &[u8]) -> Vec<u8> {
    cat(&[
        &word(ERROR),
        &string(b"Error"),
        &word(0),
        &string(b"Error"),
        &string(msg),
        &word(0),
        &word(0),
    ])
}

/// The nine ValidPathInfo fields.
pub fn path_info(path: &[u8]) -> Vec<u8> {
    cat(&[
        &string(path),
        &string(b""),
        &string(b"sha256:0000000000000000000000000000000000000000000000000000"),
        &strings(&[]),
        &word(0),
        &word(0),
        &word(0),
        &strings(&[]),
        &string(b"text:sha256:0000000000000000000000000000000000000000000000000000"),
    ])
}

// ---- the fake daemon ---------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum Step {
    /// Read exactly this many client bytes; EOF before that ends the script.
    Recv(usize),
    Send(Vec<u8>),
    /// `shutdown(SHUT_RD)`: from now on the client's writes fail (EPIPE, or SIGPIPE).
    ShutRd,
}

/// After the steps the daemon shuts down its write side (the client sees EOF on its next read)
/// and, unless it shut down its read side, reads what the client sends until it closes.
pub type Script = Vec<Step>;

/// The daemon's half of a successful handshake with `drain` as the startup stderr messages
/// (`STDERR_LAST` is appended).
pub fn handshake_with(drain: &[u8]) -> Script {
    vec![
        Step::Recv(16),
        Step::Send(daemon_hello(V1_38)),
        Step::Recv(136),
        Step::Send(strings(&[b"submit-output"])),
        Step::Recv(16),
        Step::Send(cat(&[&daemon_info(), drain, &word(LAST)])),
    ]
}

pub fn handshake() -> Script {
    handshake_with(&[])
}

/// A handshake, then one AddToStore of `request_len` bytes answered with `reply`.
pub fn add_reply(request_len: usize, reply: &[u8]) -> Script {
    let mut s = handshake();
    s.push(Step::Recv(request_len));
    s.push(Step::Send(reply.to_vec()));
    s
}

struct Daemon {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<Option<Vec<u8>>>,
}

impl Daemon {
    fn start(path: &Path, script: &[Step]) -> Self {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let script = script.to_vec();
        let thread = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                loop {
                    // read the flag before accepting: a client that connected and exited before
                    // `stop` was set is still in the backlog
                    let stopping = stop.load(Ordering::SeqCst);
                    match listener.accept() {
                        Ok((stream, _)) => return Some(serve(stream, &script)),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if stopping {
                                return None;
                            }
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(e) => panic!("accept: {e}"),
                    }
                }
            })
        };
        Self { stop, thread }
    }

    /// The client→daemon bytes, or `None` if the client never connected. Call after the client
    /// exited.
    fn finish(self) -> Option<Vec<u8>> {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.join().unwrap()
    }
}

fn serve(mut stream: UnixStream, script: &[Step]) -> Vec<u8> {
    stream.set_nonblocking(false).unwrap();
    stream.set_read_timeout(Some(DAEMON_READ_TIMEOUT)).unwrap();
    let mut got = Vec::new();
    let mut read_shut = false;
    'script: for step in script {
        match step {
            Step::Recv(n) => {
                let want = got.len() + n;
                while got.len() < want {
                    let mut buf = vec![0; want - got.len()];
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break 'script,
                        Ok(k) => got.extend_from_slice(&buf[..k]),
                    }
                }
            }
            Step::Send(bytes) => {
                if stream.write_all(bytes).is_err() {
                    break 'script;
                }
            }
            Step::ShutRd => {
                stream.shutdown(std::net::Shutdown::Read).unwrap();
                read_shut = true;
            }
        }
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);
    if !read_shut {
        // the rest, until the client closes (an error such as ECONNRESET also ends it)
        let mut buf = vec![0; 1 << 16];
        while let Ok(k @ 1..) = stream.read(&mut buf) {
            got.extend_from_slice(&buf[..k]);
        }
    }
    got
}

// ---- invocations -------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum Remote {
    /// `NIX_REMOTE=unix://<the fake daemon's socket>`
    Daemon,
    Unset,
    Value(Vec<u8>),
}

#[derive(Clone, Debug)]
pub enum Input {
    Bytes(Vec<u8>),
    /// A pipe the test keeps open, never writing to it, until the client exits: a client that
    /// read stdin would block.
    HeldOpen,
}

/// One `jig nix-store …` invocation, run the same way for the Rust and the legacy binary.
#[derive(Clone, Debug)]
pub struct Inv {
    pub args: Vec<Vec<u8>>,
    pub env: Vec<(OsString, OsString)>,
    pub remote: Remote,
    pub stdin: Input,
    pub sigpipe_ignored: bool,
    /// Run in the per-test directory, which holds the daemon's socket `sock`.
    pub in_dir: bool,
    /// Run under `strace`, with `connect` returning only after a delay.
    pub delay_connect: bool,
}

impl Inv {
    /// Online against the fake daemon, empty stdin, SIGPIPE at its default disposition.
    pub fn new<A: AsRef<[u8]>>(args: &[A]) -> Self {
        Self {
            args: args.iter().map(|a| a.as_ref().to_vec()).collect(),
            env: Vec::new(),
            remote: Remote::Daemon,
            stdin: Input::Bytes(Vec::new()),
            sigpipe_ignored: false,
            in_dir: false,
            delay_connect: false,
        }
    }

    #[must_use]
    pub fn stdin(mut self, bytes: &[u8]) -> Self {
        self.stdin = Input::Bytes(bytes.to_vec());
        self
    }

    #[must_use]
    pub fn held_open(mut self) -> Self {
        self.stdin = Input::HeldOpen;
        self
    }

    #[must_use]
    pub fn env(mut self, k: &str, v: &[u8]) -> Self {
        self.env
            .push((k.into(), OsStr::from_bytes(v).to_os_string()));
        self
    }

    /// `JIG_NIX_STORE_OFFLINE=1`, with `NIX_REMOTE` still naming the fake daemon.
    #[must_use]
    pub fn offline(self) -> Self {
        self.env("JIG_NIX_STORE_OFFLINE", b"1")
    }

    #[must_use]
    pub fn remote(mut self, r: Remote) -> Self {
        self.remote = r;
        self
    }

    #[must_use]
    pub fn sigpipe_ignored(mut self) -> Self {
        self.sigpipe_ignored = true;
        self
    }

    #[must_use]
    pub fn in_dir(mut self) -> Self {
        self.in_dir = true;
        self
    }

    #[must_use]
    pub fn delay_connect(mut self) -> Self {
        self.delay_connect = true;
        self
    }

    /// The verbs whose legacy build crashes before P2718 (Q-NIX-1).
    fn makes_derivations(&self) -> bool {
        matches!(
            self.args.first().map(Vec::as_slice),
            Some(b"add-drv" | b"fetchurls")
        )
    }
}

/// What a run produced.
#[derive(PartialEq, Eq)]
pub struct Outcome {
    pub connected: bool,
    pub sent: Vec<u8>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// Long transcripts are cut short in failure messages.
fn shown(b: &[u8]) -> String {
    const MAX: usize = 400;
    if b.len() <= MAX {
        format!("\"{}\"", b.escape_ascii())
    } else {
        format!("\"{}\"… ({} bytes)", b[..MAX].escape_ascii(), b.len())
    }
}

impl fmt::Debug for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Outcome")
            .field("connected", &self.connected)
            .field("code", &self.code)
            .field("signal", &self.signal)
            .field("stdout", &format_args!("{}", shown(&self.stdout)))
            .field("stderr", &format_args!("{}", shown(&self.stderr)))
            .field("sent", &format_args!("{}", shown(&self.sent)))
            .finish()
    }
}

impl Outcome {
    /// Exited with `code`, `stderr` (lines joined, each with `\n`) and nothing on stdout.
    #[track_caller]
    pub fn assert_fails(&self, code: i32, stderr: &[&[u8]]) {
        let want: Vec<u8> = stderr.iter().flat_map(|l| [*l, b"\n"].concat()).collect();
        assert_eq!(self.code, Some(code), "{self:?}");
        assert_eq!(self.stderr, want, "{self:?}");
        assert!(self.stdout.is_empty(), "{self:?}");
    }

    /// Exited 0 with `stdout` + `\n` and nothing on stderr.
    #[track_caller]
    pub fn assert_prints(&self, stdout: &[u8]) {
        assert_eq!(self.code, Some(0), "{self:?}");
        assert_eq!(self.stdout, [stdout, b"\n"].concat(), "{self:?}");
        assert!(self.stderr.is_empty(), "{self:?}");
    }

    /// Killed by `signal` with no output.
    #[track_caller]
    pub fn assert_killed(&self, signal: i32) {
        assert_eq!(self.signal, Some(signal), "{self:?}");
        assert!(self.stdout.is_empty(), "{self:?}");
        assert!(self.stderr.is_empty(), "{self:?}");
    }
}

/// A client running against a fake daemon.
pub struct Running {
    child: Child,
    daemon: Daemon,
    stdin: Option<ChildStdin>,
    stdout: JoinHandle<Vec<u8>>,
    stderr: JoinHandle<Vec<u8>>,
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Whether `strace` with fault injection works here (it needs ptrace).
pub fn strace() -> Option<PathBuf> {
    let strace = find_in_path("strace")?;
    let ok = Command::new(&strace)
        .args(["-qqq", "-e", "trace=none", "--", "/bin/sh", "-c", "exit 0"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    ok.then_some(strace)
}

fn reader(mut r: impl Read + Send + 'static) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        buf
    })
}

impl Running {
    /// Start `bin` with `inv`, its daemon at `dir/sock` following `script`.
    pub fn start(bin: &Path, inv: &Inv, script: &[Step], dir: &Path) -> Self {
        let sock = dir.join("sock");
        let daemon = Daemon::start(&sock, script);
        // the shell sets the dispositions the client inherits: no core files from SIGABRT, and
        // SIGPIPE ignored on request
        let mut prelude = String::from("ulimit -c 0; ");
        if inv.sigpipe_ignored {
            prelude.push_str("trap '' PIPE; ");
        }
        prelude.push_str("exec \"$@\"");
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(prelude).arg("sh");
        if inv.delay_connect {
            let strace = strace().expect("strace");
            // the injection needs `connect` traced; the trace itself goes nowhere
            cmd.arg(strace).args([
                "-o",
                "/dev/null",
                "-f",
                "-qqq",
                "-e",
                "trace=connect",
                "-e",
                "signal=none",
                "-e",
                "inject=connect:delay_exit=300000",
                "--",
            ]);
        }
        cmd.arg(bin).arg("nix-store");
        cmd.args(inv.args.iter().map(|a| OsStr::from_bytes(a)));
        cmd.env_clear();
        match &inv.remote {
            Remote::Daemon => {
                let mut v = b"unix://".to_vec();
                v.extend_from_slice(sock.as_os_str().as_bytes());
                cmd.env("NIX_REMOTE", OsStr::from_bytes(&v));
            }
            Remote::Unset => {}
            Remote::Value(v) => {
                cmd.env("NIX_REMOTE", OsStr::from_bytes(v));
            }
        }
        cmd.envs(inv.env.iter().map(|(k, v)| (k, v)));
        if inv.in_dir {
            cmd.current_dir(dir);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take();
        if let Input::Bytes(bytes) = &inv.stdin {
            let mut pipe = stdin.take().unwrap();
            let bytes = bytes.clone();
            // a client that exits without reading makes this fail with EPIPE
            std::thread::spawn(move || {
                let _ = pipe.write_all(&bytes);
            });
        }
        let stdout = reader(child.stdout.take().unwrap());
        let stderr = reader(child.stderr.take().unwrap());
        Self {
            child,
            daemon,
            stdin,
            stdout,
            stderr,
        }
    }

    /// Whether the client is still running after `d`.
    pub fn alive_after(&mut self, d: Duration) -> bool {
        std::thread::sleep(d);
        self.child.try_wait().unwrap().is_none()
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }

    /// Wait for the client (killing it after `CLIENT_DEADLINE`) and collect everything.
    pub fn finish(mut self) -> Outcome {
        let deadline = Instant::now() + CLIENT_DEADLINE;
        let status = loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                break s;
            }
            if Instant::now() > deadline {
                self.child.kill().unwrap();
                panic!("the client did not exit within {CLIENT_DEADLINE:?}");
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        drop(self.stdin.take());
        let sent = self.daemon.finish();
        Outcome {
            connected: sent.is_some(),
            sent: sent.unwrap_or_default(),
            stdout: self.stdout.join().unwrap(),
            stderr: self.stderr.join().unwrap(),
            code: status.code(),
            signal: status.signal(),
        }
    }
}

/// The legacy binary to compare against, from `JIG_LEGACY_JIG`.
pub fn legacy() -> Option<PathBuf> {
    let p = std::env::var_os("JIG_LEGACY_JIG").filter(|p| !p.is_empty());
    if p.is_none() {
        eprintln!("JIG_LEGACY_JIG is unset: not compared with the legacy jig");
    }
    p.map(PathBuf::from)
}

/// Whether `bin` is a legacy build without P2718 (GCC 14 and older): it cannot encode the
/// `§ATerm` vector (Q-NIX-1).
fn pre_p2718(bin: &Path) -> bool {
    static PRE: OnceLock<bool> = OnceLock::new();
    *PRE.get_or_init(|| {
        let json = br#"{"system":"s","builder":"/b","args":["a"],"env":{"a":"1"},"inputSrcs":["/s"],"inputDrvs":{"/d.drv":["out"]},"outputs":{"out":{"hashAlgo":"r:sha256"}}}"#;
        let inv = Inv::new(&[&b"add-drv"[..], b"x.drv"]).stdin(json).offline();
        let dir = tempfile::tempdir().unwrap();
        let o = Running::start(bin, &inv, &[], dir.path()).finish();
        let pre = o.code != Some(0);
        if pre {
            eprintln!("the legacy jig was built without P2718 (Q-NIX-1): {o:?}");
        }
        pre
    })
}

/// Whether a difference is the pre-P2718 undefined behaviour of a derivation verb (Q-NIX-1:
/// usually SIGSEGV, sometimes SIGABRT from `std::bad_alloc` or a wrong result), which Rust does
/// not reproduce.
fn legacy_q_nix_1(bin: &Path, inv: &Inv, rust: &Outcome, old: &Outcome) -> bool {
    let ub = inv.makes_derivations() && rust != old && pre_p2718(bin);
    if ub {
        eprintln!(
            "legacy jig without P2718 (Q-NIX-1) gave {old:?}: not compared; use a GCC 15 build \
             of the legacy jig"
        );
    }
    ub
}

/// Run the Rust jig, and the legacy jig if there is one, with `inv` against `script`; assert
/// they agree, and return the Rust outcome.
#[track_caller]
pub fn check(inv: &Inv, script: &[Step]) -> Outcome {
    let dir = tempfile::tempdir().unwrap();
    let rust = Running::start(Path::new(JIG), inv, script, dir.path()).finish();
    if let Some(bin) = legacy() {
        let old = Running::start(&bin, inv, script, dir.path()).finish();
        if !legacy_q_nix_1(&bin, inv, &rust, &old) {
            assert_eq!(
                rust, old,
                "Rust jig (left) and legacy jig (right) differ for {inv:?}"
            );
        }
    }
    rust
}

/// Offline: no daemon is expected to be contacted.
#[track_caller]
pub fn check_offline(inv: &Inv) -> Outcome {
    let o = check(&inv.clone().offline(), &[]);
    assert!(!o.connected, "{o:?}");
    o
}
