//! The connection's buffers, with Go `bufio` semantics where they show through
//! (`behaviors/jigd-protocol.md` §Request lines, §Replies and flushing, §IDS, Q-PROTO-5,
//! Q-PROTO-11; `implementation/03-jigd-server.md` §Exact-buffer emulation).
//!
//! - [`LineReader`] is `bufio.NewReaderSize(conn, 1<<16)` with `ReadSlice('\n')`: a line must fit
//!   the 65 536-byte buffer with its `\n`; the 65 536th byte without one is a hang-up, and a
//!   partial line before EOF is never returned. PUT bodies are read from the same buffer first.
//! - [`GoBufWriter`] is `bufio.NewWriter(conn)` (4 096 bytes): `Write` fills the buffer to the
//!   brim and flushes it, and writes a large chunk straight through when the buffer is empty.
//!   That is why `IDS 5000` cut short by EOF delivers exactly 4 096 bytes.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// `1<<16`, the request buffer.
pub const READ_BUF: usize = 1 << 16;
/// Go's `bufio` default, the reply buffer.
pub const WRITE_BUF: usize = 4096;

/// Why a line could not be read: every case is a hang-up.
#[derive(Debug)]
pub enum LineError {
    /// EOF, with or without a partial line before it (Q-PROTO-11)
    Eof,
    /// `ErrBufferFull`: 65 536 bytes without a newline (H2)
    TooLong,
    Io(io::Error),
}

/// `bufio.Reader` over the connection, as far as `ReadSlice`, `Read` and `fill` go.
#[derive(Debug)]
pub struct LineReader<R> {
    inner: R,
    buf: Box<[u8]>,
    r: usize,
    w: usize,
    /// Go's `b.err`: returned once, after the buffered bytes
    err: Option<LineError>,
}

impl<R: AsyncRead + Unpin> LineReader<R> {
    pub fn new(inner: R) -> LineReader<R> {
        LineReader::with_capacity(inner, READ_BUF)
    }

    pub fn with_capacity(inner: R, cap: usize) -> LineReader<R> {
        LineReader {
            inner,
            buf: vec![0u8; cap].into_boxed_slice(),
            r: 0,
            w: 0,
            err: None,
        }
    }

    /// Bytes buffered and not consumed.
    pub fn buffered(&self) -> usize {
        self.w - self.r
    }

    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// `ReadSlice('\n')` minus the `\n`.
    pub async fn read_line(&mut self) -> Result<&[u8], LineError> {
        let mut s = 0;
        loop {
            match self.scan(s) {
                Scan::Line(start, end) => return Ok(&self.buf[start..end]),
                Scan::Failed(e) => return Err(e),
                Scan::NeedMore => {}
            }
            s = self.w - self.r;
            self.fill().await;
        }
    }

    /// A line that is complete in the buffer, without any I/O: `Some(Ok)`, or `Some(Err)` where
    /// `ReadSlice` would fail without reading, or `None` if it would have to read.
    pub fn buffered_line(&mut self) -> Option<Result<&[u8], LineError>> {
        match self.scan(0) {
            Scan::Line(start, end) => Some(Ok(&self.buf[start..end])),
            Scan::Failed(e) => Some(Err(e)),
            Scan::NeedMore => None,
        }
    }

    /// One pass of `ReadSlice`'s loop body over the buffer, from offset `s` of the unread bytes.
    fn scan(&mut self, s: usize) -> Scan {
        if let Some(i) = memchr(b'\n', &self.buf[self.r + s..self.w]) {
            let start = self.r;
            let end = self.r + s + i;
            self.r = end + 1;
            return Scan::Line(start, end);
        }
        if let Some(e) = self.err.take() {
            self.r = self.w;
            return Scan::Failed(e);
        }
        if self.buffered() >= self.buf.len() {
            self.r = self.w;
            return Scan::Failed(LineError::TooLong);
        }
        Scan::NeedMore
    }

    /// `fill()`: slide the unread bytes to the front, then one successful read.
    async fn fill(&mut self) {
        if self.r > 0 {
            self.buf.copy_within(self.r..self.w, 0);
            self.w -= self.r;
            self.r = 0;
        }
        loop {
            match self.inner.read(&mut self.buf[self.w..]).await {
                Ok(0) => {
                    self.err = Some(LineError::Eof);
                    return;
                }
                Ok(n) => {
                    self.w += n;
                    return;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.err = Some(LineError::Io(e));
                    return;
                }
            }
        }
    }

    /// `Read(p)`: buffered bytes first; with an empty buffer, one read (straight into `p` when
    /// it is at least as large as the buffer). Never `Ok(0)` for a non-empty `p`.
    pub async fn read(&mut self, p: &mut [u8]) -> Result<usize, LineError> {
        if p.is_empty() {
            return Ok(0);
        }
        if self.r == self.w {
            if let Some(e) = self.err.take() {
                return Err(e);
            }
            if p.len() >= self.buf.len() {
                return read_some(&mut self.inner, p).await;
            }
            self.r = 0;
            self.w = 0;
            self.w = read_some(&mut self.inner, &mut self.buf).await?;
        }
        let n = p.len().min(self.w - self.r);
        p[..n].copy_from_slice(&self.buf[self.r..self.r + n]);
        self.r += n;
        Ok(n)
    }

    /// Adopted Q-SLOT-1: wait until the peer hangs up (EOF or a read error), without consuming
    /// anything: bytes that arrive meanwhile are kept in the buffer for the requests after the
    /// SLOT. With a full buffer it stops reading and never returns, which is Go's behaviour
    /// (the daemon reads nothing while a SLOT waits). Cancel-safe: dropping the future loses no
    /// byte.
    pub async fn wait_hangup(&mut self) {
        loop {
            if self.err.is_some() {
                return;
            }
            if self.r > 0 && self.w == self.buf.len() {
                self.buf.copy_within(self.r..self.w, 0);
                self.w -= self.r;
                self.r = 0;
            }
            if self.w == self.buf.len() {
                std::future::pending::<()>().await;
            }
            match self.inner.read(&mut self.buf[self.w..]).await {
                Ok(0) => self.err = Some(LineError::Eof),
                Ok(n) => self.w += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => self.err = Some(LineError::Io(e)),
            }
        }
    }
}

/// One read that returns data: EOF and errors become [`LineError`]s.
async fn read_some<R: AsyncRead + Unpin>(inner: &mut R, p: &mut [u8]) -> Result<usize, LineError> {
    loop {
        match inner.read(p).await {
            Ok(0) => return Err(LineError::Eof),
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(LineError::Io(e)),
        }
    }
}

enum Scan {
    Line(usize, usize),
    Failed(LineError),
    NeedMore,
}

fn memchr(needle: u8, hay: &[u8]) -> Option<usize> {
    hay.iter().position(|&b| b == needle)
}

/// `bufio.Writer` over the connection. After a write error the writer is dead (Go keeps
/// `b.err`); the server hangs up on the first one anyway.
#[derive(Debug)]
pub struct GoBufWriter<W> {
    inner: W,
    buf: Box<[u8]>,
    n: usize,
    failed: bool,
}

fn dead() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "write failed earlier")
}

impl<W: AsyncWrite + Unpin> GoBufWriter<W> {
    pub fn new(inner: W) -> GoBufWriter<W> {
        GoBufWriter::with_capacity(inner, WRITE_BUF)
    }

    pub fn with_capacity(inner: W, cap: usize) -> GoBufWriter<W> {
        GoBufWriter {
            inner,
            buf: vec![0u8; cap].into_boxed_slice(),
            n: 0,
            failed: false,
        }
    }

    fn available(&self) -> usize {
        self.buf.len() - self.n
    }

    /// Bytes waiting for a flush.
    pub fn buffered(&self) -> usize {
        self.n
    }

    /// `Write(p)`.
    pub async fn write(&mut self, mut p: &[u8]) -> io::Result<()> {
        while p.len() > self.available() {
            if self.failed {
                return Err(dead());
            }
            let n = if self.n == 0 {
                // a large write into an empty buffer goes straight through
                if let Err(e) = self.inner.write_all(p).await {
                    self.failed = true;
                    return Err(e);
                }
                p.len()
            } else {
                let n = self.available();
                self.buf[self.n..].copy_from_slice(&p[..n]);
                self.n += n;
                self.flush().await?;
                n
            };
            p = &p[n..];
        }
        if self.failed {
            return Err(dead());
        }
        self.buf[self.n..self.n + p.len()].copy_from_slice(p);
        self.n += p.len();
        Ok(())
    }

    /// `Flush()`: nothing to do when nothing is buffered.
    pub async fn flush(&mut self) -> io::Result<()> {
        if self.failed {
            return Err(dead());
        }
        if self.n == 0 {
            return Ok(());
        }
        if let Err(e) = self.inner.write_all(&self.buf[..self.n]).await {
            self.failed = true;
            return Err(e);
        }
        self.n = 0;
        Ok(())
    }

    /// The connection itself, for the GET body that bypasses the buffer (Go `io.Copy(conn, ..)`
    /// after a flush).
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader that hands out the given chunks, one per read, then EOF.
    struct Chunks(std::collections::VecDeque<Vec<u8>>);

    impl AsyncRead for Chunks {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            if let Some(mut c) = self.0.pop_front() {
                let n = c.len().min(buf.remaining());
                buf.put_slice(&c[..n]);
                if n < c.len() {
                    c.drain(..n);
                    self.0.push_front(c);
                }
            }
            std::task::Poll::Ready(Ok(()))
        }
    }

    fn chunks(parts: &[&[u8]]) -> Chunks {
        Chunks(parts.iter().map(|p| p.to_vec()).collect())
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
    }

    #[test]
    fn lines_split_across_reads_and_a_partial_last_line() {
        rt().block_on(async {
            let mut r = LineReader::new(chunks(&[b"GET a\nGE", b"T b\r\n", b"STATS"]));
            assert_eq!(r.read_line().await.unwrap(), b"GET a");
            assert_eq!(r.read_line().await.unwrap(), b"GET b\r");
            assert!(matches!(r.read_line().await, Err(LineError::Eof)));
        });
    }

    #[test]
    fn line_limit_is_the_buffer() {
        rt().block_on(async {
            // 65 535 content bytes and the newline fit exactly
            let mut line = vec![b'a'; READ_BUF - 1];
            line.push(b'\n');
            let mut r = LineReader::new(chunks(&[&line[..1000], &line[1000..], b"x\n"]));
            assert_eq!(r.read_line().await.unwrap().len(), READ_BUF - 1);
            assert_eq!(r.read_line().await.unwrap(), b"x");
            // one more byte without the newline is ErrBufferFull
            let long = vec![b'a'; READ_BUF];
            let mut r = LineReader::new(chunks(&[&long, b"\n"]));
            assert!(matches!(r.read_line().await, Err(LineError::TooLong)));
        });
    }

    #[test]
    fn bodies_come_from_the_buffer_first() {
        rt().block_on(async {
            let mut r = LineReader::new(chunks(&[b"PUT k 5\nab", b"cde", b"STATS\n"]));
            assert_eq!(r.read_line().await.unwrap(), b"PUT k 5");
            let mut body = [0u8; 5];
            assert_eq!(r.read(&mut body).await.unwrap(), 2);
            assert_eq!(r.read(&mut body[2..]).await.unwrap(), 3);
            assert_eq!(&body, b"abcde");
            assert_eq!(r.read_line().await.unwrap(), b"STATS");
            assert!(matches!(r.read(&mut body).await, Err(LineError::Eof)));
        });
    }

    #[test]
    fn wait_hangup_keeps_pipelined_bytes() {
        rt().block_on(async {
            let mut r = LineReader::new(chunks(&[b"SLOT b\n", b"DONE b\n", b"STATS\n"]));
            assert_eq!(r.read_line().await.unwrap(), b"SLOT b");
            r.wait_hangup().await;
            assert_eq!(r.read_line().await.unwrap(), b"DONE b");
            assert_eq!(r.read_line().await.unwrap(), b"STATS");
            assert!(matches!(r.read_line().await, Err(LineError::Eof)));
        });
    }

    /// Q-PROTO-5: 4 500 one-byte answers into the 4 096-byte writer, then a hang-up, put exactly
    /// 4 096 bytes on the wire. Tokio's `BufWriter` would have sent 4 095.
    #[test]
    fn go_writer_fills_to_the_brim() {
        rt().block_on(async {
            let mut out = Vec::new();
            let mut w = GoBufWriter::new(&mut out);
            for _ in 0..4500 {
                w.write(b"\n").await.unwrap();
            }
            assert_eq!(w.buffered(), 4500 - 4096);
            drop(w);
            assert_eq!(out.len(), 4096);

            // 35-byte answers: the buffer splits one of them at the 4 096 boundary
            let mut out = Vec::new();
            let mut w = GoBufWriter::new(&mut out);
            for _ in 0..200 {
                w.write(&[b'x'; 35]).await.unwrap();
            }
            drop(w);
            assert_eq!(out.len(), 4096);

            // a write larger than the buffer, into an empty buffer, goes straight through;
            // into a non-empty one it tops the buffer up first
            let mut out = Vec::new();
            let mut w = GoBufWriter::new(&mut out);
            w.write(&[1u8; 5000]).await.unwrap();
            assert_eq!(w.buffered(), 0);
            w.write(&[2u8; 10]).await.unwrap();
            w.write(&[3u8; 5000]).await.unwrap();
            assert_eq!(w.buffered(), 5010 - 4096);
            w.flush().await.unwrap();
            drop(w);
            assert_eq!(out.len(), 10_010);
        });
    }
}
