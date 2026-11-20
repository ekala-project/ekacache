//! Reading: `Get` and the value reader (`ecd-store.md` §Reading, §Recency).

use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::Shared;
use crate::gofs;

/// A positional reader over one value (Go's `io.SectionReader` over the pack fd). It holds the
/// pack's descriptor, so it keeps working after the pack is demoted or evicted.
#[derive(Debug, Clone)]
pub struct ValueReader {
    file: Arc<File>,
    off: u64,
    len: u64,
    pos: u64,
}

impl ValueReader {
    /// The promised length (`val.Size()`).
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bytes not read yet.
    pub fn remaining(&self) -> u64 {
        self.len - self.pos
    }

    /// The descriptor, offset and length, for a caller that wants `sendfile`.
    pub fn parts(&self) -> (&Arc<File>, u64, u64) {
        (&self.file, self.off, self.len)
    }
}

impl Read for ValueReader {
    /// `pread` at the current position; `Ok(0)` at the end of the value or of the file (a
    /// pack shorter than the index says gives a short value, which the server turns into a
    /// hang-up).
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.len - self.pos;
        if left == 0 || buf.is_empty() {
            return Ok(0);
        }
        let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
        let n = self.file.read_at(&mut buf[..want], self.off + self.pos)?;
        self.pos += n as u64;
        Ok(n)
    }
}

/// `Get(key)`: under the read lock, mark the pack used and, at most every touchEvery (CAS so
/// that one of several concurrent readers does it), set its mtime; errors ignored. Does not
/// check `closed`.
pub(crate) fn get(s: &Shared, key: &[u8]) -> Option<ValueReader> {
    let inner = s.read();
    let l = *inner.index.get(key)?;
    let p = inner.packs.get(&l.pack)?;
    let now = s.now();
    p.used.store(now, Ordering::SeqCst);
    let last = p.touched.load(Ordering::SeqCst);
    if now.wrapping_sub(last) > s.touch_every_ns
        && p.touched
            .compare_exchange(last, now, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    {
        // the path of the pack in its current tier; atime unchanged
        let _ = gofs::set_mtime(&p.path, now);
    }
    Some(ValueReader {
        file: Arc::clone(&p.file),
        off: u64::from(l.off),
        len: u64::from(l.len),
        pos: 0,
    })
}
