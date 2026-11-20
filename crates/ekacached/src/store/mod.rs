//! The bitcask pack store: append-only packs, an in-memory index, hint files for sealed packs,
//! tiers with byte budgets, the demoter, and crash recovery.
//!
//! Specified by `behaviors/ecd-store.md` (all of it) and `spec/store.qnt`; plan
//! `implementation/04-ecd-store.md`. Adopted changes implemented here: Q-STORE-1 (hints sorted
//! by record offset), Q-STORE-2 (empty packs dropped at open and close), Q-STORE-5 (demoter
//! kicked at open), Q-STORE-7 (fsync on seal and demotion).
//!
//! The store is synchronous (`std::fs`, `std::sync`); the server calls it from blocking-pool
//! threads. Locking mirrors Go's `mu sync.RWMutex`: `get` takes the read lock, `put`, seal,
//! victim choice and the demotion switch take the write lock.

mod clock;
mod demote;
mod load;
mod naming;
mod pack;
mod read;
mod record;
mod write;

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::thread::JoinHandle;

pub use clock::{Clock, SystemClock, TestClock};
pub use demote::Prepared;
pub use naming::scanf_pack_id;
pub use pack::Loc;
pub use read::ValueReader;
pub use record::{HINT_HEADER, REC_HEADER};
pub use write::{Body, validate_put};

use crate::goerr::GoErr;
use crate::log::Logger;
use pack::Pack;

/// Seal the active pack before a record would take it past this size (256 MiB).
pub const PACK_LIMIT: i64 = 256 << 20;
/// A PUT body of at least this size is spooled to a temp file instead of RAM (4 MiB).
pub const SPOOL_MIN: i64 = 4 << 20;
/// A read moves its pack's mtime at most this often (1 minute).
pub const TOUCH_EVERY_NS: i64 = 60_000_000_000;
/// Longest key a PUT may use.
pub const MAX_KEY: usize = 4096;
/// Largest value a PUT may carry (inclusive).
pub const MAX_VALUE: i64 = 2 << 30;

/// A directory of packs with a byte budget (`<= 0` = unbounded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier {
    pub dir: Vec<u8>,
    pub budget: i64,
}

/// How the demoter runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemoterMode {
    /// A background thread fed by kicks (the daemon).
    Thread,
    /// No thread: a kick only sets a flag, and the caller drives demotion with
    /// [`Store::demote_step`] / [`Store::demote_commit`] (the Quint replay).
    Manual,
}

/// Everything `Store::open` needs. The defaults are the Go constants; tests shrink them.
#[derive(Clone)]
pub struct StoreConfig {
    /// `tiers[0]` takes the writes; the last tier deletes.
    pub tiers: Vec<Tier>,
    pub pack_limit: i64,
    pub touch_every_ns: i64,
    pub clock: Arc<dyn Clock>,
    pub logger: Logger,
    pub demoter: DemoterMode,
    /// Never hard-link when demoting: always take the copy path (tests).
    pub force_copy: bool,
}

impl fmt::Debug for StoreConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreConfig")
            .field("tiers", &self.tiers)
            .field("pack_limit", &self.pack_limit)
            .field("touch_every_ns", &self.touch_every_ns)
            .field("demoter", &self.demoter)
            .field("force_copy", &self.force_copy)
            .finish_non_exhaustive()
    }
}

impl StoreConfig {
    pub fn new(tiers: Vec<Tier>) -> StoreConfig {
        StoreConfig {
            tiers,
            pack_limit: PACK_LIMIT,
            touch_every_ns: TOUCH_EVERY_NS,
            clock: Arc::new(SystemClock),
            logger: Logger::stderr(),
            demoter: DemoterMode::Thread,
            force_copy: false,
        }
    }
}

/// `Stats()`: `keys=%d packs=%d bytes=%d live=%d`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    pub keys: usize,
    pub packs: usize,
    pub bytes: i64,
    pub live: i64,
}

impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "keys={} packs={} bytes={} live={}",
            self.keys, self.packs, self.bytes, self.live
        )
    }
}

/// The store's mutable state, behind `Shared::inner` (Go's fields guarded by `mu`).
#[derive(Debug, Default)]
pub(crate) struct Inner {
    pub index: HashMap<Box<[u8]>, Loc>,
    /// A `BTreeMap` so that iteration is deterministic (Go's map order is random).
    pub packs: BTreeMap<u32, Pack>,
    /// `tiers[t].total`: the sizes of the packs in tier t, the active pack included
    pub totals: Vec<i64>,
    /// `None` only while `open` has not rotated yet
    pub active: Option<u32>,
    pub closed: bool,
}

enum Kicker {
    /// the capacity-1 channel to the demoter thread; `None` once closed
    Thread(Mutex<Option<SyncSender<()>>>),
    /// a pending kick, consumed by the caller
    Manual(AtomicBool),
}

pub(crate) struct Shared {
    pub tiers: Vec<Tier>,
    pub pack_limit: i64,
    pub touch_every_ns: i64,
    pub clock: Arc<dyn Clock>,
    pub log: Logger,
    pub force_copy: bool,
    inner: RwLock<Inner>,
    kicker: Kicker,
}

impl Shared {
    pub fn read(&self) -> RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn write(&self) -> RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn now(&self) -> i64 {
        self.clock.now_ns()
    }

    /// `kick()`: a non-blocking send on the 1-slot channel, so kicks coalesce.
    pub fn kick(&self) {
        match &self.kicker {
            Kicker::Thread(tx) => {
                if let Some(tx) = tx.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
                    let _ = tx.try_send(());
                }
            }
            Kicker::Manual(pending) => pending.store(true, Ordering::SeqCst),
        }
    }

    /// `s.name(p, ext)` for a pack id in a tier.
    pub fn name(&self, tier: usize, id: u32, ext: &str) -> Vec<u8> {
        naming::pack_path(&self.tiers[tier].dir, id, ext)
    }
}

/// The pack store. `get`, `put` and `stats` may be called from many threads at once.
pub struct Store {
    shared: Arc<Shared>,
    demoter: Mutex<Option<JoinHandle<()>>>,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("tiers", &self.shared.tiers)
            .finish_non_exhaustive()
    }
}

/// What one manual demoter iteration did (Quint `demoteStep`).
#[derive(Debug)]
pub enum DemoteStep {
    /// no victim: the demoter loop ends (the pending kick is consumed)
    Idle,
    /// the victim was deleted from the last tier
    Evicted(u32),
    /// the victim's files are in the next tier; [`Store::demote_commit`] switches to them
    Copied(demote::Prepared),
    /// moveDown failed and was logged; the loop ends
    Failed(u32),
}

/// A copy of the store's state for tests and the model replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub index: BTreeMap<Vec<u8>, Loc>,
    pub packs: BTreeMap<u32, PackSnapshot>,
    pub totals: Vec<i64>,
    pub active: u32,
    pub closed: bool,
}

/// One pack in a [`Snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackSnapshot {
    pub tier: usize,
    pub path: Vec<u8>,
    pub size: i64,
    pub live: i64,
    pub used: i64,
    pub touched: i64,
}

impl Store {
    /// `OpenStore`: load every tier, then start a new active pack and the demoter.
    pub fn open(cfg: StoreConfig) -> Result<Store, GoErr> {
        let (kicker, rx) = match cfg.demoter {
            DemoterMode::Thread => {
                let (tx, rx) = std::sync::mpsc::sync_channel(1);
                (Kicker::Thread(Mutex::new(Some(tx))), Some(rx))
            }
            DemoterMode::Manual => (Kicker::Manual(AtomicBool::new(false)), None),
        };
        let ntiers = cfg.tiers.len();
        let shared = Arc::new(Shared {
            tiers: cfg.tiers,
            pack_limit: cfg.pack_limit,
            touch_every_ns: cfg.touch_every_ns,
            clock: cfg.clock,
            log: cfg.logger,
            force_copy: cfg.force_copy,
            inner: RwLock::new(Inner {
                totals: vec![0; ntiers],
                ..Inner::default()
            }),
            kicker,
        });
        load::open_store(&shared)?;
        let demoter = rx.map(|rx| {
            let s = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("ecd-demoter".into())
                .spawn(move || demote::demoter(&s, &rx))
        });
        let demoter = match demoter {
            Some(Ok(h)) => Some(h),
            Some(Err(e)) => return Err(GoErr::Msg(format!("demoter thread: {e}").into_bytes())),
            None => None,
        };
        // adopted Q-STORE-5: kick once, so a store that is over budget at start is trimmed
        // without waiting for a Put
        shared.kick();
        Ok(Store {
            shared,
            demoter: Mutex::new(demoter),
        })
    }

    /// `Get`: a reader over the value, or `None`. Moves the pack's recency (and, at most
    /// every touchEvery, its mtime). Also serves HAS. Works after `close`.
    pub fn get(&self, key: &[u8]) -> Option<ValueReader> {
        read::get(&self.shared, key)
    }

    /// `Put` with a body already read (see [`Store::read_body`]).
    pub fn put(&self, key: &[u8], size: i64, body: Body) -> Result<(), GoErr> {
        write::put(&self.shared, key, size, body)
    }

    /// Go's `Store.Put(key, size, r)` in one call: validate, read the body (into memory, or a
    /// spool file from `SPOOL_MIN` on), then append.
    pub fn put_reader(
        &self,
        key: &[u8],
        size: i64,
        r: &mut impl std::io::Read,
    ) -> Result<(), GoErr> {
        validate_put(key, size)?;
        let body = self.read_body(size, r)?;
        self.put(key, size, body)
    }

    /// Read a PUT body of `size` bytes the way Go's `Put` does before taking the lock:
    /// `io.ReadFull` into memory below `SPOOL_MIN`, else `io.CopyN` into an unlinked
    /// `put-*.tmp` in tier 0.
    pub fn read_body(&self, size: i64, r: &mut impl std::io::Read) -> Result<Body, GoErr> {
        write::read_body(&self.shared, size, r)
    }

    /// An unlinked `put-*.tmp` spool file in tier 0 (`os.CreateTemp` + `os.Remove`), and its
    /// name, for a caller that fills a large body itself.
    pub fn create_spool(&self) -> Result<(std::fs::File, Vec<u8>), GoErr> {
        write::create_spool(&self.shared)
    }

    pub fn stats(&self) -> Stats {
        let inner = self.shared.read();
        Stats {
            keys: inner.index.len(),
            packs: inner.packs.len(),
            bytes: inner.totals.iter().sum(),
            live: inner.packs.values().map(|p| p.live).sum(),
        }
    }

    /// The tiers as configured.
    pub fn tiers(&self) -> &[Tier] {
        &self.shared.tiers
    }

    /// `Close`: refuse further Puts, let the demoter finish every pending demotion, then seal
    /// the active pack (adopted Q-STORE-2: delete it instead if it is empty). Descriptors stay
    /// open and `get` keeps working. A second call does nothing.
    pub fn close(&self) -> Result<(), GoErr> {
        {
            let mut inner = self.shared.write();
            if inner.closed {
                return Ok(());
            }
            inner.closed = true;
        }
        match &self.shared.kicker {
            Kicker::Thread(tx) => {
                tx.lock().unwrap_or_else(PoisonError::into_inner).take();
            }
            Kicker::Manual(pending) => {
                if pending.swap(false, Ordering::SeqCst) {
                    demote::run_loop(&self.shared);
                }
            }
        }
        if let Some(h) = self
            .demoter
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = h.join();
        }
        write::close_active(&self.shared)
    }

    /// `Wait` (tests): block until no tier has a victim.
    pub fn wait(&self) {
        loop {
            let busy = demote::victim(&self.shared, &self.shared.write()).is_some();
            if !busy {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Manual demoter: is a kick pending? (Quint `demoting`.)
    pub fn kick_pending(&self) -> bool {
        match &self.shared.kicker {
            Kicker::Manual(p) => p.load(Ordering::SeqCst),
            Kicker::Thread(_) => false,
        }
    }

    /// Manual demoter: one iteration of the demoter's inner loop. Eviction completes here; a
    /// demotion stops after the unlocked copy and waits for [`Store::demote_commit`].
    pub fn demote_step(&self) -> DemoteStep {
        let step = demote::step(&self.shared);
        if matches!(step, DemoteStep::Idle | DemoteStep::Failed(_))
            && let Kicker::Manual(p) = &self.shared.kicker
        {
            p.store(false, Ordering::SeqCst);
        }
        step
    }

    /// Manual demoter: the locked switch of a demotion started by `demote_step`.
    pub fn demote_commit(&self, prepared: demote::Prepared) {
        demote::commit(&self.shared, prepared);
    }

    /// Manual demoter: run the demoter loop until no victim is left (what the thread does on a
    /// kick), consuming the pending kick.
    pub fn run_demoter(&self) {
        if let Kicker::Manual(p) = &self.shared.kicker {
            p.store(false, Ordering::SeqCst);
        }
        demote::run_loop(&self.shared);
    }

    /// The state as the model sees it (tests).
    pub fn snapshot(&self) -> Snapshot {
        let inner = self.shared.read();
        Snapshot {
            index: inner.index.iter().map(|(k, l)| (k.to_vec(), *l)).collect(),
            packs: inner
                .packs
                .iter()
                .map(|(id, p)| {
                    let snap = PackSnapshot {
                        tier: p.tier,
                        path: p.path.clone(),
                        size: p.size,
                        live: p.live,
                        used: p.used(),
                        touched: p.touched.load(Ordering::SeqCst),
                    };
                    (*id, snap)
                })
                .collect(),
            totals: inner.totals.clone(),
            active: inner.active.unwrap_or(0),
            closed: inner.closed,
        }
    }

    /// Overwrite a pack's `used` (tests: Go's `s.packs[1].used.Store(...)`).
    pub fn set_used(&self, id: u32, ns: i64) {
        if let Some(p) = self.shared.read().packs.get(&id) {
            p.used.store(ns, Ordering::SeqCst);
        }
    }
}

impl Drop for Store {
    /// Dropping without `close` is a crash as far as the disk is concerned: nothing is sealed.
    /// The demoter thread is only told to stop (after the work it already has).
    fn drop(&mut self) {
        if let Kicker::Thread(tx) = &self.shared.kicker {
            tx.lock().unwrap_or_else(PoisonError::into_inner).take();
        }
    }
}
