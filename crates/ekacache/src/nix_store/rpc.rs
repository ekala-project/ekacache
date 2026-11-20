//! The builder-rpc-v0 client (`behaviors/ec-nix-store.md §Socket`, `§Handshake`, `§Stderr drain
//! loop`, `§AddToStore (op 7)`, `§SubmitOutput (op 1000)`, `§Client state-transition table`;
//! `spec/builder_rpc.qnt`).
//!
//! One connection per process, never closed explicitly: no `SetOptions`, no goodbye. Every
//! failure is reported through the return value after printing the message the state table
//! lists, if any. Failures that print nothing are Q-NIX-9; the handshake diagnostics that
//! name the wrong cause are Q-NIX-10.

use ekacache_core::bytes::itoa;

use super::wire::Conn;
use super::{
    BAD_MAGIC, CANNOT_CONNECT, DAEMON_ERROR, DAEMON_ERROR_UNREADABLE, NIX_REMOTE_NOT_UNIX,
    NIX_REMOTE_NOT_UNIX_END, PROTOCOL_TOO_OLD, PROTOCOL_TOO_OLD_END, UNEXPECTED_MESSAGE, say,
};
use crate::client::{connect_unix, sun_path_len};
use crate::process::ProcEnv;

const WORKER_MAGIC_1: u64 = 0x6e69_7863;
const WORKER_MAGIC_2: u64 = 0x6478_696f;
/// `builderRpcV0 = 1.38`.
const PROTOCOL_VERSION: u64 = (1 << 8) | 38;

const STDERR_NEXT: u64 = 0x6f6c_6d67;
const STDERR_LAST: u64 = 0x616c_7473;
const STDERR_ERROR: u64 = 0x6378_7470;
const STDERR_START_ACTIVITY: u64 = 0x5354_5254;
const STDERR_STOP_ACTIVITY: u64 = 0x5354_4f50;
const STDERR_RESULT: u64 = 0x5253_4c54;

const OP_ADD_TO_STORE: u64 = 7;
const OP_SUBMIT_OUTPUT: u64 = 1000;

/// The client's features, in the order sent.
const FEATURES: [&[u8]; 4] = [
    b"realisation-with-path-not-hash",
    b"disable-set-options",
    b"add-to-store-scanning",
    b"submit-output",
];

const SCHEME: &[u8] = b"unix://";

/// A connection that completed the handshake (state `Ready`).
pub struct Daemon {
    conn: Conn,
}

impl Daemon {
    /// `ParseRemote` → `CheckPath` → `Connecting` → the handshake → `Ready`.
    pub fn connect(env: &ProcEnv) -> Option<Self> {
        let remote = env.get_or("NIX_REMOTE", b"");
        // Q-NIX-11: only the exact prefix `unix://`; the rest is the path, verbatim
        let Some(path) = remote.strip_prefix(SCHEME) else {
            say(&[NIX_REMOTE_NOT_UNIX, remote, NIX_REMOTE_NOT_UNIX_END]);
            return None;
        };
        if path.len() >= sun_path_len() {
            // Q-NIX-9: silent
            return None;
        }
        // Q-NIX-11, Q-NIX-12: the path in a zeroed `sockaddr_un`, `addrlen = sizeof(sockaddr_un)`
        let Some(fd) = connect_unix(path) else {
            say(&[CANNOT_CONNECT, path]);
            return None;
        };
        let mut daemon = Self {
            conn: Conn::new(fd),
        };
        daemon.handshake().then_some(daemon)
    }

    /// `SendHello` … `DrainStartup`.
    fn handshake(&mut self) -> bool {
        let c = &mut self.conn;
        c.write_word(WORKER_MAGIC_1);
        c.write_word(PROTOCOL_VERSION);
        // Q-NIX-10: a failed flush and an early EOF are reported as a bad magic too
        if !c.flush() || c.read_word() != Some(WORKER_MAGIC_2) {
            say(&[BAD_MAGIC]);
            return false;
        }
        let version = c.read_word();
        // `min(daemon_version, 0x126) < 0x126`: any version >= 1.38 is accepted
        if version.is_none_or(|v| v.min(PROTOCOL_VERSION) < PROTOCOL_VERSION) {
            // Q-NIX-10: an unreadable version is "0", printed in plain decimal
            say(&[
                PROTOCOL_TOO_OLD,
                &itoa(version.unwrap_or(0)),
                PROTOCOL_TOO_OLD_END,
            ]);
            return false;
        }
        // feature exchange (>= 1.38): ours, then theirs (discarded)
        c.write_word(FEATURES.len() as u64);
        for f in FEATURES {
            c.write_string(f);
        }
        // Q-NIX-9: the failures from here on print nothing
        if !c.flush() || !c.skip_strings() {
            return false;
        }
        // obsolete CPU affinity (none) and reserveSpace (false); then the daemon's version
        // string and trust word, discarded
        c.write_word(0);
        c.write_word(0);
        if !c.flush() || c.read_string().is_none() || c.read_word().is_none() {
            return false;
        }
        self.drain()
    }

    /// The stderr drain loop, until `STDERR_LAST` (true) or a failure.
    fn drain(&self) -> bool {
        let c = &self.conn;
        loop {
            let Some(msg) = c.read_word() else {
                // Q-NIX-9: EOF in a drain is silent
                return false;
            };
            // Q-NIX-8: read failures inside a message are ignored and the loop goes on
            match msg {
                STDERR_LAST => return true,
                STDERR_ERROR => {
                    // type, level, name; then msg. havePos, traces, … are not read
                    let _ = c.read_string();
                    let _ = c.read_word();
                    let _ = c.read_string();
                    let text = c.read_string();
                    say(&[
                        DAEMON_ERROR,
                        text.as_deref().unwrap_or(DAEMON_ERROR_UNREADABLE),
                    ]);
                    return false;
                }
                STDERR_NEXT => say(&[&c.read_string().unwrap_or_default()]),
                STDERR_START_ACTIVITY => {
                    let _ = c.read_word();
                    let _ = c.read_word();
                    let _ = c.read_word();
                    let _ = c.read_string();
                    c.skip_fields();
                    let _ = c.read_word();
                }
                STDERR_STOP_ACTIVITY => {
                    let _ = c.read_word();
                }
                STDERR_RESULT => {
                    let _ = c.read_word();
                    let _ = c.read_word();
                    c.skip_fields();
                }
                _ => {
                    say(&[UNEXPECTED_MESSAGE, format!("{msg:#x}").as_bytes()]);
                    return false;
                }
            }
        }
    }

    /// AddToStore (op 7) with a content-address method: `SendAdd` → `DrainAdd` →
    /// `AwaitPathInfo`. The returned path is not checked.
    pub fn add_to_store<R: AsRef<[u8]>>(
        &mut self,
        name: &[u8],
        method: &[u8],
        refs: &[R],
        contents: &[u8],
    ) -> Option<Vec<u8>> {
        let c = &mut self.conn;
        c.write_word(OP_ADD_TO_STORE);
        c.write_string(name);
        c.write_string(method);
        // Q-NIX-5: sorted, duplicates kept
        let refs = super::hash::sorted_refs(refs);
        c.write_word(refs.len() as u64);
        for r in refs {
            c.write_string(r);
        }
        c.write_word(0); // repair = false
        // framed source: one chunk and the terminator
        c.write_word(contents.len() as u64);
        c.write_raw(contents);
        // Q-NIX-6: the terminator follows even an empty chunk, so empty contents send 0, 0
        c.write_word(0);
        if !c.flush() || !self.drain() {
            return None;
        }
        // ValidPathInfo: path, deriver, narHash, references, registrationTime, narSize,
        // ultimate, sigs, ca. Q-NIX-9: a truncated reply is a silent failure
        let c = &self.conn;
        let path = c.read_string()?;
        let complete = c.read_string().is_some()
            && c.read_string().is_some()
            && c.skip_strings()
            && c.read_word().is_some()
            && c.read_word().is_some()
            && c.read_word().is_some()
            && c.skip_strings()
            && c.read_string().is_some();
        complete.then_some(path)
    }

    /// SubmitOutput (op 1000): this build's `output` := the store object at `path`.
    /// `SendSubmit` → `DrainSubmit` → `AwaitSubmitResult` (one word, discarded).
    pub fn submit_output(&mut self, path: &[u8], output: &[u8]) -> bool {
        let c = &mut self.conn;
        c.write_word(OP_SUBMIT_OUTPUT);
        c.write_word(0); // SingleDerivedPath::Opaque
        c.write_string(path);
        c.write_string(output);
        c.flush() && self.drain() && self.conn.read_word().is_some()
    }
}
