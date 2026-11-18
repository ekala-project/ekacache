//! The jigd client (`behaviors/jig-cache-client.md`), synchronous, one connection per object.
//!
//! A port of `cache_client.cc` with the adopted changes:
//!
//! - **HELLO** (`§Version negotiation`): `HELLO 1` first; `OK <v>` with `v >= 1` → version 1; EOF
//!   or a read error → reconnect once and speak version 0 without HELLO (a Go jigd hung up on the
//!   unknown verb); anything else → the connect failed. The process does not remember a
//!   version-0 result (`MAY`, item 5): every jig mode connects at most once.
//! - **Q-CLIENT-1**: values are framed by `jig_core::frame` (LE header).
//! - **Q-CLIENT-2**: every PUT reply is read and must be exactly `OK`. Anything else, and any
//!   failed send of a PUT, closes the connection; `put` and `put_many` report what was stored.
//!
//! Every failure is silent and returns the empty result (miss, `false`, empty vector), as legacy
//! does. Kept as-is: no timeouts (Q-CLIENT-4), no `EINTR` retry (Q-CLIENT-6), the stream desync
//! after a short body (Q-CLIENT-5), and no `RecvLine` length limit.

use std::mem::size_of;
use std::os::fd::{AsRawFd, OwnedFd};

use jig_core::bytes::{itoa, parse_uint};
use jig_core::frame;
use rustix::net::{AddressFamily, SendFlags, SocketFlags, SocketType};

/// Values longer than this on the wire close the connection (`kMaxObjectSize`, 2 GiB).
pub const MAX_OBJECT_SIZE: u64 = 1 << 31;

/// One `read` fills at most this much (`Fill`).
const BUF_SIZE: usize = 64 * 1024;

/// `sizeof(sun_path)`: 108 on Linux, 104 on macOS. A path this long or longer is refused.
#[must_use]
pub fn sun_path_len() -> usize {
    // SAFETY: sockaddr_un is plain old data; all-zero is a valid value.
    #[allow(unsafe_code)]
    let addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_path.len()
}

/// `Connect` steps 1-4 (`cache_client.cc:27-44`), without HELLO: refuse a path of
/// `sizeof(sun_path)` bytes or more without a syscall, `socket(AF_UNIX, SOCK_STREAM |
/// SOCK_CLOEXEC)`, and `connect` with the path copied into a zeroed `sockaddr_un` and
/// `addrlen = sizeof(sockaddr_un)`. Shared with nix-store (`jig-nix-store.md §Socket`).
#[must_use]
pub fn connect_unix(path: &[u8]) -> Option<OwnedFd> {
    if path.len() >= sun_path_len() {
        return None;
    }
    let fd = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )
    .ok()?;
    // SAFETY: as above.
    #[allow(unsafe_code)]
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (dst, &src) in addr.sun_path.iter_mut().zip(path) {
        *dst = src as libc::c_char;
    }
    // legacy passes the whole struct length, not the path length (`§Connect` step 3)
    let len = size_of::<libc::sockaddr_un>() as libc::socklen_t;
    // SAFETY: `addr` is a valid sockaddr_un of `len` bytes; `fd` is an open socket.
    #[allow(unsafe_code)]
    let rc = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&raw const addr).cast::<libc::sockaddr>(),
            len,
        )
    };
    (rc == 0).then_some(fd)
}

/// A connection to jigd, or none (`CacheClient`).
pub struct Client {
    fd: Option<OwnedFd>,
    buf: Box<[u8]>,
    /// `pending_`: unread bytes of the last `read`, `buf[start..end]`.
    start: usize,
    end: usize,
    version: u32,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    /// Not connected.
    #[must_use]
    pub fn new() -> Self {
        Self {
            fd: None,
            buf: vec![0u8; BUF_SIZE].into_boxed_slice(),
            start: 0,
            end: 0,
            version: 0,
        }
    }

    /// A client already connected to `path`, or `None`.
    #[must_use]
    pub fn connected_to(path: &[u8]) -> Option<Self> {
        let mut c = Self::new();
        c.connect(path).then_some(c)
    }

    #[must_use]
    pub fn connected(&self) -> bool {
        self.fd.is_some()
    }

    /// The negotiated protocol version: 1 after HELLO, 0 for a daemon without it.
    #[must_use]
    pub fn version(&self) -> u32 {
        self.version
    }

    /// `Connect(path)` plus the HELLO exchange.
    pub fn connect(&mut self, path: &[u8]) -> bool {
        self.close();
        let Some(fd) = connect_unix(path) else {
            return false;
        };
        self.attach(fd);
        if !self.send_all(b"HELLO 1\n") {
            return self.reconnect_v0(path);
        }
        match self.recv_line() {
            None => self.reconnect_v0(path),
            Some(line) => match line.strip_prefix(b"OK ").and_then(parse_uint) {
                Some(v) if v >= 1 => {
                    self.version = 1;
                    true
                }
                _ => {
                    self.close();
                    false
                }
            },
        }
    }

    fn reconnect_v0(&mut self, path: &[u8]) -> bool {
        self.close();
        match connect_unix(path) {
            Some(fd) => {
                self.attach(fd);
                self.version = 0;
                true
            }
            None => false,
        }
    }

    fn attach(&mut self, fd: OwnedFd) {
        self.fd = Some(fd);
        self.start = 0;
        self.end = 0;
    }

    /// Drop the connection. Unread bytes stay in the buffer, as `fd_ = UniqueFd()` leaves
    /// `pending_` alone.
    fn close(&mut self) {
        self.fd = None;
    }

    /// `SendAll`: `send(MSG_NOSIGNAL)` until done; any error or a 0 return fails.
    fn send_all(&mut self, mut data: &[u8]) -> bool {
        let Some(fd) = &self.fd else {
            return false;
        };
        while !data.is_empty() {
            match rustix::net::send(fd, data, SendFlags::NOSIGNAL) {
                Ok(n) if n > 0 => data = &data[n..],
                _ => return false,
            }
        }
        true
    }

    /// `Fill`: one `read` of up to 64 KiB into the buffer.
    fn fill(&mut self) -> bool {
        let Some(fd) = &self.fd else {
            return false;
        };
        match rustix::io::read(fd, &mut self.buf[..]) {
            Ok(n) if n > 0 => {
                self.start = 0;
                self.end = n;
                true
            }
            _ => false,
        }
    }

    /// `RecvLine`: up to the next `\n`, which is removed. No length limit.
    fn recv_line(&mut self) -> Option<Vec<u8>> {
        let mut line = Vec::new();
        loop {
            let pending = &self.buf[self.start..self.end];
            if let Some(i) = pending.iter().position(|&c| c == b'\n') {
                line.extend_from_slice(&pending[..i]);
                self.start += i + 1;
                return Some(line);
            }
            line.extend_from_slice(pending);
            // `pending_` is not cleared before `Fill`: on failure the same bytes are still
            // there for the next call, which only matters for a closed connection anyway
            if !self.fill() {
                return None;
            }
        }
    }

    /// `RecvExactly(n)`.
    fn recv_exactly(&mut self, n: usize) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(n.min(BUF_SIZE * 16));
        while out.len() < n {
            if self.start == self.end && !self.fill() {
                return None;
            }
            let take = (self.end - self.start).min(n - out.len());
            out.extend_from_slice(&self.buf[self.start..self.start + take]);
            self.start += take;
        }
        Some(out)
    }

    /// `RecvValue` (`cache_client.cc:143-155`).
    fn recv_value(&mut self) -> Option<Vec<u8>> {
        let line = self.recv_line()?;
        let rest = line.strip_prefix(b"OK ")?;
        let len = match parse_uint(rest) {
            Some(len) if len <= MAX_OBJECT_SIZE => len,
            _ => {
                self.close();
                return None;
            }
        };
        let wire = self.recv_exactly(usize::try_from(len).ok()?)?;
        frame::decode(&wire)
    }

    /// `Get(k)`: `GET k`.
    pub fn get(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        if !self.connected() || !self.send_all(&line(&[b"GET ", key])) {
            return None;
        }
        self.recv_value()
    }

    /// `GetMany(ks)`: every `GET` in one send, then the replies in order.
    pub fn get_many<K: AsRef<[u8]>>(&mut self, keys: &[K]) -> Vec<Option<Vec<u8>>> {
        let mut values = vec![None; keys.len()];
        let request: Vec<u8> = keys
            .iter()
            .flat_map(|k| line(&[b"GET ", k.as_ref()]))
            .collect();
        if !self.connected() || keys.is_empty() || !self.send_all(&request) {
            return values;
        }
        for v in &mut values {
            *v = self.recv_value();
        }
        values
    }

    /// `HasMany(ks)`: `true` only for a reply line that is exactly `1`.
    pub fn has_many<K: AsRef<[u8]>>(&mut self, keys: &[K]) -> Vec<bool> {
        let mut have = vec![false; keys.len()];
        let request: Vec<u8> = keys
            .iter()
            .flat_map(|k| line(&[b"HAS ", k.as_ref()]))
            .collect();
        if !self.connected() || keys.is_empty() || !self.send_all(&request) {
            return have;
        }
        for h in &mut have {
            *h = self.recv_line().as_deref() == Some(b"1");
        }
        have
    }

    /// Send one `PUT`. `None`: the value could not be framed and nothing was sent.
    fn send_put(&mut self, key: &[u8], value: &[u8]) -> Option<bool> {
        let wire = frame::encode(value).ok()?;
        let header = line(&[b"PUT ", key, b" ", &itoa(wire.len() as u64)]);
        Some(self.send_all(&header) && self.send_all(&wire))
    }

    /// `Put(k, v)`, ADOPTED Q-CLIENT-2: `true` iff the reply is exactly `OK`. A failed send or
    /// any other reply closes the connection.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> bool {
        if !self.connected() {
            return false;
        }
        match self.send_put(key, value) {
            None => false,
            Some(false) => {
                self.close();
                false
            }
            Some(true) => {
                if self.recv_line().as_deref() == Some(b"OK") {
                    true
                } else {
                    self.close();
                    false
                }
            }
        }
    }

    /// `PutMany(items)`, ADOPTED Q-CLIENT-2: every `PUT` is sent before any reply is read; the
    /// result is the number of leading items whose reply was exactly `OK` (`items[n]` is the
    /// first one not stored). A value that cannot be framed stops the sending, as in legacy; a
    /// failed send or a reply other than `OK` closes the connection.
    pub fn put_many<K: AsRef<[u8]>, V: AsRef<[u8]>>(&mut self, items: &[(K, V)]) -> usize {
        if !self.connected() || items.is_empty() {
            return 0;
        }
        let mut sent = 0;
        let mut dead = false;
        for (k, v) in items {
            match self.send_put(k.as_ref(), v.as_ref()) {
                None => break,
                Some(false) => {
                    dead = true;
                    break;
                }
                Some(true) => sent += 1,
            }
        }
        let mut stored = 0;
        while stored < sent {
            if self.recv_line().as_deref() != Some(b"OK") {
                dead = true;
                break;
            }
            stored += 1;
        }
        if dead {
            self.close();
        }
        stored
    }

    /// `Identities(ps)`: one `IDS <n>` with every path; `n` reply lines, `""` meaning "no
    /// answer". Any failure gives the empty vector. No chunking (open question 2).
    pub fn identities<P: AsRef<[u8]>>(&mut self, paths: &[P]) -> Vec<Vec<u8>> {
        if !self.connected() || paths.is_empty() {
            return Vec::new();
        }
        let mut request = line(&[b"IDS ", &itoa(paths.len() as u64)]);
        for p in paths {
            request.extend_from_slice(p.as_ref());
            request.push(b'\n');
        }
        if !self.send_all(&request) {
            return Vec::new();
        }
        let mut ids = Vec::with_capacity(paths.len());
        for _ in paths {
            match self.recv_line() {
                Some(l) => ids.push(l),
                None => return Vec::new(),
            }
        }
        ids
    }

    /// `AcquireSlot(b)`: `SLOT b`, blocking until the reply; `true` iff it is exactly `OK`.
    pub fn acquire_slot(&mut self, build: &[u8]) -> bool {
        if !self.connected() || !self.send_all(&line(&[b"SLOT ", build])) {
            return false;
        }
        self.recv_line().as_deref() == Some(b"OK")
    }

    /// `ReleaseSlot(b)`: `DONE b`, one reply line read and ignored.
    pub fn release_slot(&mut self, build: &[u8]) {
        if self.connected() && self.send_all(&line(&[b"DONE ", build])) {
            let _ = self.recv_line();
        }
    }

    /// A connected client over an existing socket (tests).
    #[doc(hidden)]
    #[must_use]
    pub fn from_fd(fd: OwnedFd, version: u32) -> Self {
        let mut c = Self::new();
        c.attach(fd);
        c.version = version;
        c
    }
}

/// `parts` concatenated, plus `\n`.
fn line(parts: &[&[u8]]) -> Vec<u8> {
    let mut l = Vec::with_capacity(parts.iter().map(|p| p.len()).sum::<usize>() + 1);
    for p in parts {
        l.extend_from_slice(p);
    }
    l.push(b'\n');
    l
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::thread;

    /// A scripted server: `script` gets each accepted connection in turn.
    fn serve<F>(
        conns: usize,
        script: F,
    ) -> (tempfile::TempDir, Vec<u8>, thread::JoinHandle<Vec<Vec<u8>>>)
    where
        F: Fn(usize, &mut BufReader<UnixStream>, &mut UnixStream) + Send + 'static,
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s");
        let l = UnixListener::bind(&path).unwrap();
        let h = thread::spawn(move || {
            let mut seen = Vec::new();
            for i in 0..conns {
                let (s, _) = l.accept().unwrap();
                let mut w = s.try_clone().unwrap();
                let mut r = BufReader::new(s);
                script(i, &mut r, &mut w);
                let _ = w.shutdown(std::net::Shutdown::Write);
                let mut rest = Vec::new();
                let _ = r.read_to_end(&mut rest);
                seen.push(rest);
            }
            seen
        });
        let p = path.as_os_str().as_encoded_bytes().to_vec();
        (dir, p, h)
    }

    fn read_line(r: &mut BufReader<UnixStream>) -> Vec<u8> {
        let mut l = Vec::new();
        r.read_until(b'\n', &mut l).unwrap();
        l
    }

    fn value(raw: &[u8]) -> Vec<u8> {
        frame::encode(raw).unwrap()
    }

    #[test]
    fn hello_v1() {
        let (_d, p, h) = serve(1, |_, r, w| {
            assert_eq!(read_line(r), b"HELLO 1\n");
            w.write_all(b"OK 1\n").unwrap();
            assert_eq!(read_line(r), b"GET k\n");
            w.write_all(b"MISS\n").unwrap();
        });
        let mut c = Client::connected_to(&p).unwrap();
        assert_eq!(c.version(), 1);
        assert_eq!(c.get(b"k"), None);
        assert!(c.connected());
        drop(c);
        h.join().unwrap();
    }

    #[test]
    fn hello_fallback_to_v0() {
        let (_d, p, h) = serve(2, |i, r, w| {
            if i == 0 {
                // a Go jigd: unknown verb, hang up
                assert_eq!(read_line(r), b"HELLO 1\n");
                return;
            }
            let v = value(b"hello");
            assert_eq!(read_line(r), b"GET k\n");
            w.write_all(format!("OK {}\n", v.len()).as_bytes()).unwrap();
            w.write_all(&v).unwrap();
        });
        let mut c = Client::connected_to(&p).unwrap();
        assert_eq!(c.version(), 0);
        assert_eq!(c.get(b"k").as_deref(), Some(&b"hello"[..]));
        drop(c);
        h.join().unwrap();
    }

    #[test]
    fn hello_garbage_is_a_failed_connect() {
        for reply in [&b"OK 0\n"[..], b"OK\n", b"ERR\n", b"OK x\n"] {
            let reply = reply.to_vec();
            let (_d, p, h) = serve(1, move |_, r, w| {
                read_line(r);
                w.write_all(&reply).unwrap();
            });
            assert!(Client::connected_to(&p).is_none());
            h.join().unwrap();
        }
    }

    #[test]
    fn no_socket_and_long_paths() {
        assert!(Client::connected_to(b"/nonexistent/jigd/socket").is_none());
        assert!(Client::connected_to(b"").is_none());
        assert!(Client::connected_to(&[b'a'; 108]).is_none());
        let mut c = Client::new();
        assert_eq!(c.get(b"k"), None);
        assert!(!c.put(b"k", b"v"));
        assert_eq!(c.put_many(&[(b"k", b"v")]), 0);
        assert!(c.identities(&[b"/p"]).is_empty());
        assert_eq!(c.has_many(&[b"k"]), vec![false]);
        assert!(!c.acquire_slot(b"-"));
        c.release_slot(b"-");
    }

    fn pair() -> (Client, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        (Client::from_fd(OwnedFd::from(a), 1), b)
    }

    #[test]
    fn put_reply_checked() {
        let (mut c, mut s) = pair();
        s.write_all(b"OK\n").unwrap();
        assert!(c.put(b"k", b"value"));
        let mut r = BufReader::new(s.try_clone().unwrap());
        let hdr = read_line(&mut r);
        let wire = value(b"value");
        assert_eq!(hdr, format!("PUT k {}\n", wire.len()).as_bytes());
        let mut body = vec![0; wire.len()];
        r.read_exact(&mut body).unwrap();
        assert_eq!(frame::decode(&body).as_deref(), Some(&b"value"[..]));
        // a refusal closes the connection
        s.write_all(b"NO\n").unwrap();
        assert!(!c.put(b"k", b"v"));
        assert!(!c.connected());
    }

    #[test]
    fn put_many_counts_leading_oks() {
        let (mut c, mut s) = pair();
        s.write_all(b"OK\nOK\nOK\n").unwrap();
        assert_eq!(c.put_many(&[(b"a", b"1"), (b"b", b"2"), (b"c", b"3")]), 3);
        assert!(c.connected());
        s.write_all(b"OK\n").unwrap();
        s.shutdown(std::net::Shutdown::Write).unwrap();
        assert_eq!(c.put_many(&[(b"a", b"1"), (b"b", b"2")]), 1);
        assert!(!c.connected());
    }

    #[test]
    fn recv_value_rules() {
        let (mut c, mut s) = pair();
        let v = value(b"x");
        // pipelined GETs: a value, a miss, a stray `OK` line (no value, connection kept)
        s.write_all(format!("OK {}\n", v.len()).as_bytes()).unwrap();
        s.write_all(&v).unwrap();
        s.write_all(b"MISS\nOK\n").unwrap();
        let got = c.get_many(&[b"a", b"b", b"c"]);
        assert_eq!(got, vec![Some(b"x".to_vec()), None, None]);
        assert!(c.connected());
        // an unparsable length closes the connection
        s.write_all(b"OK 12x\n").unwrap();
        assert_eq!(c.get(b"d"), None);
        assert!(!c.connected());
        let (mut c, mut s) = pair();
        s.write_all(format!("OK {}\n", MAX_OBJECT_SIZE + 1).as_bytes())
            .unwrap();
        assert_eq!(c.get(b"d"), None);
        assert!(!c.connected());
        let (mut c, mut s) = pair();
        let v = value(b"hello");
        s.write_all(format!("OK  \t{} \n", v.len()).as_bytes())
            .unwrap();
        s.write_all(&v).unwrap();
        assert_eq!(c.get(b"e").as_deref(), Some(&b"hello"[..]));
    }

    #[test]
    fn identities_and_slots() {
        let (mut c, mut s) = pair();
        s.write_all(b"C:1\n\nC:3\n").unwrap();
        assert_eq!(
            c.identities(&[b"/a", b"/b", b"/c"]),
            vec![b"C:1".to_vec(), Vec::new(), b"C:3".to_vec()]
        );
        s.write_all(b"OK\nOK\n1\n0\n").unwrap();
        assert!(c.acquire_slot(b"/build"));
        c.release_slot(b"/build");
        assert_eq!(c.has_many(&[b"x", b"y"]), vec![true, false]);
        s.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(c.identities(&[b"/a"]).is_empty());
        drop(c);
        let mut sent = Vec::new();
        s.read_to_end(&mut sent).unwrap();
        assert_eq!(
            sent,
            b"IDS 3\n/a\n/b\n/c\nSLOT /build\nDONE /build\nHAS x\nHAS y\nIDS 1\n/a\n"
        );
    }
}
