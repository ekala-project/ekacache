//! In-memory pack and index entry (`ecd-store.md` §In-memory state).

use std::fs::File;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

/// Where a key's current value lives: `off` is the offset of the VALUE in the pack, not of
/// the record. All three are `u32` as in Go, with Go's wrapping arithmetic where it is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Loc {
    pub pack: u32,
    pub off: u32,
    pub len: u32,
}

/// One pack file the store knows about. Lives in `Inner::packs` and changes only under the
/// store's write lock, except `used`/`touched`, which `get` moves under the read lock.
#[derive(Debug)]
pub(crate) struct Pack {
    /// index into the tier list
    pub tier: usize,
    /// `%06d.pack` in the tier's directory (`s.name(p, "pack")`)
    pub path: Vec<u8>,
    /// An `Arc` so a reader handed out by `get` keeps the descriptor alive after demotion or
    /// eviction (Go relies on the finalizer, `store.go:51-52`).
    pub file: Arc<File>,
    pub size: i64,
    /// bytes of values the index still points at
    pub live: i64,
    /// Unix ns of the last read served from this pack
    pub used: AtomicI64,
    /// what the file's mtime says, moved at most every touchEvery
    pub touched: AtomicI64,
}

impl Pack {
    pub fn new(tier: usize, path: Vec<u8>, file: File, size: i64, stamp: i64) -> Pack {
        Pack {
            tier,
            path,
            file: Arc::new(file),
            size,
            live: 0,
            used: AtomicI64::new(stamp),
            touched: AtomicI64::new(stamp),
        }
    }

    pub fn used(&self) -> i64 {
        self.used.load(Ordering::SeqCst)
    }

    /// `p.live*2 < p.size`: strictly more than half dead. A 0-byte pack is never stale.
    pub fn stale(&self) -> bool {
        self.live.wrapping_mul(2) < self.size
    }
}
