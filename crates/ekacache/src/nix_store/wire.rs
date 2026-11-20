//! Worker protocol encoding (`behaviors/ec-nix-store.md §Worker protocol` `§Encoding`): words,
//! padded strings, string lists, exact reads, and buffered writes sent at flush points.
//!
//! Reads loop on `read(2)` until they have every byte; a return of 0 or -1 (`EINTR` included,
//! never retried) is a read failure. Writes go out with plain `write(2)`, not `send` with
//! `MSG_NOSIGNAL`, so a daemon that hung up kills the process with `SIGPIPE` when the inherited
//! disposition is the default (Q-NIX-7; ec keeps the inherited disposition, see `main.rs`).

use std::os::fd::OwnedFd;

/// A received string longer than this is a read failure (only its length word is consumed).
pub const MAX_STRING: u64 = 64 << 20;

const WORD: usize = 8;

/// `(8 - len mod 8) mod 8` zero bytes of padding after a string of `len` bytes.
fn padding(len: u64) -> usize {
    ((WORD as u64 - len % WORD as u64) % WORD as u64) as usize
}

/// A connected socket with a write buffer.
pub struct Conn {
    fd: OwnedFd,
    out: Vec<u8>,
}

impl Conn {
    #[must_use]
    pub fn new(fd: OwnedFd) -> Self {
        Self {
            fd,
            out: Vec::new(),
        }
    }

    /// Buffer a word (little-endian u64).
    pub fn write_word(&mut self, v: u64) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }

    /// Buffer a string: length word, bytes, zero padding to a multiple of 8.
    pub fn write_string(&mut self, s: &[u8]) {
        self.write_word(s.len() as u64);
        self.out.extend_from_slice(s);
        self.out.resize(self.out.len() + padding(s.len() as u64), 0);
    }

    /// Buffer raw bytes (the AddToStore contents chunk, unpadded).
    pub fn write_raw(&mut self, b: &[u8]) {
        self.out.extend_from_slice(b);
    }

    /// Send the buffer: `write(2)` until it is gone; a return <= 0 fails. The buffer is cleared
    /// only on success (after a failure the process exits anyway).
    pub fn flush(&mut self) -> bool {
        let mut pending = &self.out[..];
        while !pending.is_empty() {
            // Q-NIX-7: plain write(2), so SIGPIPE is delivered when the daemon has hung up
            match rustix::io::write(&self.fd, pending) {
                Ok(n) if n > 0 => pending = &pending[n..],
                _ => return false,
            }
        }
        self.out.clear();
        true
    }

    /// Fill `buf` exactly.
    fn read_into(&self, buf: &mut [u8]) -> bool {
        let mut got = 0;
        while got < buf.len() {
            match rustix::io::read(&self.fd, &mut buf[got..]) {
                Ok(n) if n > 0 => got += n,
                _ => return false,
            }
        }
        true
    }

    /// `ReadBytes(count)`. No system call for `count == 0`.
    pub fn read_bytes(&self, count: usize) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; count];
        self.read_into(&mut buf).then_some(buf)
    }

    /// `ReadWord()`.
    pub fn read_word(&self) -> Option<u64> {
        let mut buf = [0u8; WORD];
        self.read_into(&mut buf).then(|| u64::from_le_bytes(buf))
    }

    /// `ReadString()`: length word (> 64 MiB fails with nothing more consumed), bytes, padding
    /// (read and discarded, not checked).
    pub fn read_string(&self) -> Option<Vec<u8>> {
        let len = self.read_word()?;
        if len > MAX_STRING {
            return None;
        }
        let text = self.read_bytes(len as usize)?;
        self.read_bytes(padding(len))?;
        Some(text)
    }

    /// `SkipStrings()`: a string list, discarded; stops at the first failure.
    pub fn skip_strings(&self) -> bool {
        let Some(count) = self.read_word() else {
            return false;
        };
        for _ in 0..count {
            if self.read_string().is_none() {
                return false;
            }
        }
        true
    }

    /// `SkipFields()`: logger fields, `count` (0 if unreadable) then per field a type word (0 if
    /// unreadable) and a string for type 1, else a word. Read failures are ignored, so a huge
    /// `count` at EOF loops that many times (Q-NIX-8).
    pub fn skip_fields(&self) {
        let count = self.read_word().unwrap_or(0);
        for _ in 0..count {
            // Q-NIX-8: read failures inside the fields do not stop the loop
            if self.read_word().unwrap_or(0) == 1 {
                let _ = self.read_string();
            } else {
                let _ = self.read_word();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    fn pair() -> (Conn, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        (Conn::new(OwnedFd::from(a)), b)
    }

    #[test]
    fn encoding() {
        let (mut c, mut peer) = pair();
        c.write_word(0x126);
        c.write_string(b"hello.txt");
        c.write_string(b"");
        c.write_string(b"12345678");
        c.write_raw(b"abc");
        assert!(c.flush());
        drop(c);
        let mut got = Vec::new();
        peer.read_to_end(&mut got).unwrap();
        let mut want = vec![0x26, 1, 0, 0, 0, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0];
        want.extend_from_slice(b"hello.txt\0\0\0\0\0\0\0");
        want.extend_from_slice(&[0; 8]);
        want.extend_from_slice(&[8, 0, 0, 0, 0, 0, 0, 0]);
        want.extend_from_slice(b"12345678abc");
        assert_eq!(got, want);
    }

    #[test]
    fn decoding() {
        let (c, mut peer) = pair();
        let mut msg = Vec::new();
        msg.extend_from_slice(&7u64.to_le_bytes());
        msg.extend_from_slice(&3u64.to_le_bytes());
        msg.extend_from_slice(b"abcXXXXX"); // padding is not checked
        msg.extend_from_slice(&(MAX_STRING + 1).to_le_bytes());
        msg.extend_from_slice(&5u64.to_le_bytes());
        peer.write_all(&msg).unwrap();
        drop(peer);
        assert_eq!(c.read_word(), Some(7));
        assert_eq!(c.read_string().as_deref(), Some(&b"abc"[..]));
        // oversized: only the length word is consumed
        assert_eq!(c.read_string(), None);
        assert_eq!(c.read_word(), Some(5));
        assert_eq!(c.read_word(), None);
        assert_eq!(c.read_bytes(0).as_deref(), Some(&b""[..]));
    }

    #[test]
    fn max_string_is_accepted() {
        let (c, mut peer) = pair();
        let writer = std::thread::spawn(move || {
            peer.write_all(&MAX_STRING.to_le_bytes()).unwrap();
            peer.write_all(&vec![b'x'; MAX_STRING as usize]).unwrap();
        });
        assert_eq!(c.read_string().map(|s| s.len()), Some(MAX_STRING as usize));
        writer.join().unwrap();
    }

    #[test]
    fn fields_ignore_failures() {
        let (c, mut peer) = pair();
        let mut msg = Vec::new();
        for w in [3u64, 1, 2] {
            msg.extend_from_slice(&w.to_le_bytes());
        }
        msg.extend_from_slice(b"hi\0\0\0\0\0\0");
        msg.extend_from_slice(&[0; 8]); // type 0
        msg.extend_from_slice(&[9; 8]); // its word
        peer.write_all(&msg).unwrap();
        drop(peer);
        c.skip_fields();
        // the third field's reads failed at EOF, and the loop still ended
        assert_eq!(c.read_word(), None);
    }

    #[test]
    fn flush_failure() {
        let (mut c, peer) = pair();
        drop(peer);
        c.write_word(1);
        // the test harness ignores SIGPIPE, so this is EPIPE
        assert!(!c.flush());
    }
}
