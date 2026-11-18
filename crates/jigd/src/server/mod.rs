//! The socket server: the accept loop and one task per connection (`behaviors/jigd-protocol.md`
//! all of it, `jigd-slots.md` §Per-connection bookkeeping, `jigd-daemon.md` §Signals and shutdown
//! (Q-DAEMON-5 kept); `implementation/03-jigd-server.md` §Connection task, §Commands, §SLOT with
//! cancellation).
//!
//! A connection reads one line, dispatches it on verb and field count
//! (`jig_core::wire::parse_request`), writes the reply into the Go-sized output buffer and
//! flushes after every request that succeeded. Every failure is a hang-up: no error replies,
//! and only a failed PUT logs. When the connection ends, its slot tokens are released (in
//! sorted build order; Go's map order is random, Q-SLOT-4).
//!
//! Adopted here: HELLO (only as the first request, H11) and Q-SLOT-1 (a queued SLOT whose peer
//! hangs up is dequeued). Store calls and IDS hashing run on the blocking pool.

pub mod bufio;

use std::collections::BTreeMap;
use std::io;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use jig_core::wire::{self, Request};
use tokio::io::AsyncWriteExt;
use tokio::net::unix::{ReadHalf, WriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;

use crate::goerr::GoErr;
use crate::identity::Identities;
use crate::log::Logger;
use crate::peer::Auth;
use crate::slots::{Acquire, Slots};
use crate::store::{Body, SPOOL_MIN, Store, ValueReader, validate_put};
use bufio::{GoBufWriter, LineError, LineReader};

/// GET bodies and spooled PUT bodies move in chunks of this size.
const CHUNK: usize = 256 << 10;
/// IDS paths already buffered are hashed together, up to this many per blocking call.
const IDS_BATCH: usize = 256;

/// Everything a connection needs: the daemon's globals in Go (`store`, `idents`, `slots`, the
/// counters).
pub struct Ctx {
    pub store: Store,
    pub idents: Identities,
    pub slots: Slots,
    pub auth: Auth,
    pub log: Logger,
    gets: AtomicI64,
    hits: AtomicI64,
    puts: AtomicI64,
}

impl Ctx {
    pub fn new(store: Store, idents: Identities, slots: Slots, auth: Auth, log: Logger) -> Ctx {
        Ctx {
            store,
            idents,
            slots,
            auth,
            log,
            gets: AtomicI64::new(0),
            hits: AtomicI64::new(0),
            puts: AtomicI64::new(0),
        }
    }

    /// The STATS reply: each part read separately, not a snapshot (Q-PROTO-10).
    pub fn stats_line(&self) -> Vec<u8> {
        let gets = self.gets.load(Ordering::Relaxed);
        let hits = self.hits.load(Ordering::Relaxed);
        let puts = self.puts.load(Ordering::Relaxed);
        let ids = self.idents.len();
        let (out, waiting) = self.slots.stats();
        let store = self.store.stats();
        format!(
            "gets={gets} hits={hits} puts={puts} ids={ids} slots={out} waiting={waiting} {store}\n"
        )
        .into_bytes()
    }
}

/// Accept until `stop` is notified or accept fails. Go retries `ECONNABORTED` and `EINTR`
/// inside `Accept`; any other error ends the loop, and the daemon then shuts down cleanly
/// (Q-DAEMON-5 kept).
pub async fn accept_loop(listener: &UnixListener, ctx: &Arc<Ctx>, stop: &Notify) {
    loop {
        let res = tokio::select! {
            biased;
            () = stop.notified() => return,
            r = listener.accept() => r,
        };
        match res {
            Ok((stream, _)) => {
                tokio::spawn(serve_conn(stream, Arc::clone(ctx)));
            }
            Err(e) if matches!(e.raw_os_error(), Some(libc::ECONNABORTED | libc::EINTR)) => {}
            Err(_) => return,
        }
    }
}

/// `serve(conn)`: the peer check, the request loop, then the release of every token the
/// connection still holds, then the close.
pub async fn serve_conn(mut stream: UnixStream, ctx: Arc<Ctx>) {
    if !ctx.auth.allowed(&stream) {
        return;
    }
    // borrowed halves: dropping them does not `shutdown(SHUT_WR)` first, so the peer sees the
    // same close as from Go
    let (r, w) = stream.split();
    let mut conn = Conn {
        ctx: &ctx,
        rd: LineReader::new(r),
        wr: GoBufWriter::new(w),
        held: BTreeMap::new(),
        line: Vec::new(),
    };
    let Hangup = conn.run().await;
    for (build, n) in std::mem::take(&mut conn.held) {
        for _ in 0..n {
            ctx.slots.release(&build);
        }
    }
}

/// The end of a connection. Every path out of the request loop is a hang-up.
struct Hangup;

impl From<io::Error> for Hangup {
    fn from(_: io::Error) -> Hangup {
        Hangup
    }
}

impl From<LineError> for Hangup {
    fn from(_: LineError) -> Hangup {
        Hangup
    }
}

struct Conn<'a> {
    ctx: &'a Arc<Ctx>,
    rd: LineReader<ReadHalf<'a>>,
    wr: GoBufWriter<WriteHalf<'a>>,
    /// serve()'s `held`: tokens per build this connection owns (sorted for the release order)
    held: BTreeMap<Vec<u8>, u64>,
    line: Vec<u8>,
}

/// Run `f` on the blocking pool. `None` only if it panicked, which the caller treats as a
/// hang-up.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

impl Conn<'_> {
    async fn run(&mut self) -> Hangup {
        let mut first = true;
        loop {
            if let Err(h) = self.request(first).await {
                return h;
            }
            first = false;
        }
    }

    /// One request: read, dispatch, reply, flush.
    async fn request(&mut self, first: bool) -> Result<(), Hangup> {
        let line = self.rd.read_line().await?;
        self.line.clear();
        self.line.extend_from_slice(line);
        let line = std::mem::take(&mut self.line);
        let res = self.dispatch(&line, first).await;
        self.line = line;
        res?;
        self.wr.flush().await?;
        Ok(())
    }

    async fn dispatch(&mut self, line: &[u8], first: bool) -> Result<(), Hangup> {
        let ctx = self.ctx;
        match wire::parse_request(line).ok_or(Hangup)? {
            Request::Hello(n) => {
                // adopted HELLO: only as the first request (H11)
                if !first {
                    return Err(Hangup);
                }
                let ver = n.min(wire::PROTOCOL_MAX);
                self.wr.write(format!("OK {ver}\n").as_bytes()).await?;
            }
            Request::Get(key) => self.get(key).await?,
            Request::Has(key) => {
                // Q-PROTO-1: the same recency side effect as a GET hit, no counters
                let (c, k) = (Arc::clone(ctx), key.to_vec());
                let found = blocking(move || c.store.get(&k).is_some())
                    .await
                    .ok_or(Hangup)?;
                self.wr.write(if found { b"1\n" } else { b"0\n" }).await?;
            }
            Request::Put(key, size) => {
                // Q-PROTO-2: counted once the length parsed, whatever happens next
                ctx.puts.fetch_add(1, Ordering::Relaxed);
                if let Err(e) = self.put(key, size).await {
                    // the stream position is unknown after a failed body read: hang up
                    let mut msg = b"put ".to_vec();
                    msg.extend_from_slice(key);
                    msg.extend_from_slice(b": ");
                    msg.extend_from_slice(&e.text());
                    ctx.log.log(&msg);
                    return Err(Hangup);
                }
                self.wr.write(b"OK\n").await?;
            }
            // Q-PROTO-6: `IDS 0` writes nothing
            Request::Ids(n) => self.ids(n).await?,
            Request::Slot(build) => {
                if !self.slot(build).await {
                    return Err(Hangup);
                }
                *self.held.entry(build.to_vec()).or_insert(0) += 1;
                self.wr.write(b"OK\n").await?;
            }
            Request::Done(build) => {
                // Q-SLOT-2: an unheld DONE is answered OK and changes nothing
                if let Some(n) = self.held.get_mut(build) {
                    *n -= 1;
                    if *n == 0 {
                        self.held.remove(build);
                    }
                    ctx.slots.release(build);
                }
                self.wr.write(b"OK\n").await?;
            }
            Request::Stats => {
                let line = ctx.stats_line();
                self.wr.write(&line).await?;
            }
        }
        Ok(())
    }

    /// `get`: count, look up, `MISS`, or count the hit, flush the header and copy the value
    /// past the buffer. A short copy hangs up after a partial body (Q-PROTO-8).
    async fn get(&mut self, key: &[u8]) -> Result<(), Hangup> {
        let ctx = self.ctx;
        ctx.gets.fetch_add(1, Ordering::Relaxed);
        let (c, k) = (Arc::clone(ctx), key.to_vec());
        // the lookup and the first chunk in one trip to the blocking pool
        let found = blocking(move || {
            c.store.get(&k).map(|mut v| {
                let mut buf = Vec::new();
                let res = read_chunk(&mut v, &mut buf);
                (v, buf, res)
            })
        })
        .await
        .ok_or(Hangup)?;
        let Some((mut val, mut buf, mut res)) = found else {
            self.wr.write(b"MISS\n").await?;
            return Ok(());
        };
        ctx.hits.fetch_add(1, Ordering::Relaxed);
        self.wr
            .write(format!("OK {}\n", val.len()).as_bytes())
            .await?;
        self.wr.flush().await?;
        let sock = self.wr.get_mut();
        loop {
            sock.write_all(&buf).await?;
            if res.is_err() {
                return Err(Hangup);
            }
            if val.remaining() == 0 {
                return Ok(());
            }
            if buf.is_empty() {
                // the pack is shorter than the index says: Go's io.Copy comes up short
                return Err(Hangup);
            }
            (val, buf, res) = blocking(move || {
                let res = read_chunk(&mut val, &mut buf);
                (val, buf, res)
            })
            .await
            .ok_or(Hangup)?;
        }
    }

    /// `Store.Put` from the connection: validate, read the body (memory below 4 MiB, else an
    /// unlinked spool in tier 0), then append on the blocking pool.
    async fn put(&mut self, key: &[u8], size: i64) -> Result<(), GoErr> {
        validate_put(key, size)?;
        let body = self.read_body(size).await?;
        let (c, k) = (Arc::clone(self.ctx), key.to_vec());
        blocking(move || c.store.put(&k, size, body))
            .await
            .unwrap_or_else(|| Err(GoErr::msg("put panicked")))
    }

    /// The body as Go's `Put` reads it: `io.ReadFull` into memory below `SPOOL_MIN` (`EOF` if
    /// nothing came, `unexpected EOF` if part did), else `io.CopyN` into the spool (`EOF` if it
    /// ends early). Bytes already buffered after the request line come first.
    async fn read_body(&mut self, size: i64) -> Result<Body, GoErr> {
        let len = usize::try_from(size).unwrap_or(0);
        if size < SPOOL_MIN {
            let mut buf = vec![0u8; len];
            let mut done = 0;
            while done < len {
                match self.rd.read(&mut buf[done..]).await {
                    Ok(n) => done += n,
                    Err(LineError::Io(e)) => return Err(self.read_error(&e)),
                    Err(_) if done == 0 => return Err(GoErr::eof()),
                    Err(_) => return Err(GoErr::unexpected_eof()),
                }
            }
            return Ok(Body::Mem(buf));
        }
        let c = Arc::clone(self.ctx);
        let (mut file, name) = blocking(move || c.store.create_spool())
            .await
            .unwrap_or_else(|| Err(GoErr::msg("spool panicked")))?;
        let mut left = len;
        let mut chunk = vec![0u8; CHUNK.min(left)];
        while left > 0 {
            let want = chunk.len().min(left);
            let n = match self.rd.read(&mut chunk[..want]).await {
                Ok(n) => n,
                Err(LineError::Io(e)) => return Err(self.read_error(&e)),
                Err(_) => return Err(GoErr::eof()),
            };
            let res;
            (file, chunk, res) = blocking(move || {
                let res = std::io::Write::write_all(&mut file, &chunk[..n]);
                (file, chunk, res)
            })
            .await
            .ok_or_else(|| GoErr::msg("spool panicked"))?;
            res.map_err(|e| GoErr::path("write", &name, &e))?;
            left -= n;
        }
        Ok(Body::Spool { file, name })
    }

    /// The `*net.OpError` of a failed socket read: `read unix <local>-><peer>: read: <errno>`.
    fn read_error(&self, e: &io::Error) -> GoErr {
        let stream: &UnixStream = self.rd.get_ref().as_ref();
        let local = rustix::net::getsockname(stream)
            .ok()
            .map(|a| go_unix_name(&a));
        let peer = rustix::net::getpeername(stream)
            .ok()
            .flatten()
            .map(|a| go_unix_name(&a));
        GoErr::Op {
            op: "read",
            net: "unix",
            source: local,
            addr: Some(peer.unwrap_or_else(|| b"@".to_vec())),
            err: Box::new(match e.raw_os_error() {
                Some(errno) => GoErr::Syscall {
                    name: "read",
                    errno,
                },
                None => GoErr::from_io(e),
            }),
        }
    }

    /// `ids`: for each path line in order, its identity and `\n`. Answers go into the output
    /// buffer as they are computed and leave in 4 096-byte chunks; a failed path line hangs up
    /// and drops what is still buffered (Q-PROTO-5). Lines already in the input buffer are
    /// hashed in one blocking call: the socket sees the same reads and writes as in Go, where
    /// each answer is written before the next line is read.
    async fn ids(&mut self, mut left: usize) -> Result<(), Hangup> {
        while left > 0 {
            let mut batch = vec![self.rd.read_line().await?.to_vec()];
            left -= 1;
            let mut failed = false;
            while left > 0 && batch.len() < IDS_BATCH {
                match self.rd.buffered_line() {
                    Some(Ok(l)) => {
                        batch.push(l.to_vec());
                        left -= 1;
                    }
                    Some(Err(_)) => {
                        failed = true;
                        break;
                    }
                    None => break,
                }
            }
            let c = Arc::clone(self.ctx);
            let answers = blocking(move || {
                batch
                    .iter()
                    .map(|p| {
                        let mut a = c.idents.of(p);
                        a.push(b'\n');
                        a
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .ok_or(Hangup)?;
            for a in answers {
                self.wr.write(&a).await?;
            }
            if failed {
                return Err(Hangup);
            }
        }
        Ok(())
    }

    /// `slots.Acquire(build)`. True once the connection holds the token; false if the peer hung
    /// up while queued and the waiter was removed (adopted Q-SLOT-1).
    async fn slot(&mut self, build: &[u8]) -> bool {
        let mut ticket = match self.ctx.slots.acquire(build) {
            Acquire::Granted => return true,
            Acquire::Queued(t) => t,
        };
        tokio::select! {
            biased;
            r = &mut ticket.ready => r.is_ok(),
            () = self.rd.wait_hangup() => {
                // the grant may have won the race for the lock: then the token is ours, and
                // the Go path follows (the OK write fails, the cleanup releases it)
                !self.ctx.slots.cancel(ticket.id)
            }
        }
    }
}

/// Fill `buf` with the next bytes of the value, up to `CHUNK`; empty at the end of the value or
/// of the pack file.
fn read_chunk(v: &mut ValueReader, buf: &mut Vec<u8>) -> io::Result<()> {
    let want = usize::try_from(v.remaining())
        .unwrap_or(usize::MAX)
        .min(CHUNK);
    buf.resize(want, 0);
    let mut done = 0;
    while done < want {
        match v.read(&mut buf[done..]) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                buf.truncate(done);
                return Err(e);
            }
        }
    }
    buf.truncate(done);
    Ok(())
}

/// A Unix socket address as Go's `syscall.anyToSockaddr` names it: the path up to its first NUL,
/// or `@` and the abstract name up to its first NUL, or `@` for an unnamed socket.
fn go_unix_name(addr: &rustix::net::SocketAddrAny) -> Vec<u8> {
    let Ok(unix) = rustix::net::SocketAddrUnix::try_from(addr.clone()) else {
        return b"@".to_vec();
    };
    let upto_nul = |b: &[u8]| b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())].to_vec();
    if let Some(p) = unix.path_bytes() {
        return upto_nul(p);
    }
    let mut out = b"@".to_vec();
    if let Some(a) = unix.abstract_name() {
        out.extend_from_slice(&upto_nul(a));
    }
    out
}
