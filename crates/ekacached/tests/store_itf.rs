//! Replay of `spec/store.qnt` ITF traces against the real store (implementation/04 §Tests,
//! implementation/12 §Model-based tests; the model has the adopted flags on: KICK_AT_START,
//! DROP_EMPTY_PACKS, FSYNC_ON_SEAL).
//!
//! Traces are generated locally and not committed (`/tests/itf/*/` is ignored):
//!
//! ```text
//! quint run spec/store.qnt --mbt --out-itf='tests/itf/store/t{seq}.itf.json' \
//!     --n-traces=100 --max-steps=60 --max-samples=100 --seed=1
//! ```
//!
//! With no traces the test passes and says so. Each trace drives one store in two temporary
//! tier directories with the model's parameters (pack limit 24, budgets 30 and 45, 1-byte keys,
//! touchEvery 2) and a test clock (model time `t` is `BASE + t` seconds). The demoter is manual:
//! `demoteStep` and `demoteCommit` are the two halves of `moveDown`. After every step the
//! store's index, packs, totals, active pack, pending kick and in-flight copy, and the files on
//! disk (names, sizes, hint entries as a set, the value bytes behind every index entry), must
//! equal the model's.
//!
//! Where the store and the model legitimately differ:
//!  - the model's `init` is not an `OpenStore`, so the kick a real open makes (adopted Q-STORE-5)
//!    is consumed before the first comparison;
//!  - the model does not move a file's mtime when the kernel does (appends, truncation), so
//!    before every restart the files' mtimes are set to the model's (slower tier first: a pack
//!    that is hard-linked into two tiers shares one inode, and the fast tier's copy is loaded);
//!  - adopted Q-STORE-2 rule 1 deletes an empty active pack at `Close`, where the model seals it
//!    and drops it at the next open; the deleted id is then reused by the store but not by the
//!    model, so model pack ids are mapped to real ones (both sequences are increasing).
//!
//! Witnesses (12 §L4.3), evaluated on the replayed model states: `witnessEviction` (a pack was
//! deleted from the last tier) and `witnessDemoted` (a pack left tier 0) must be reached;
//! `witnessPowerLossFallback` (after a power loss an index entry is not the newest record of its
//! key) and a violation of `idleWithinBudgetEvenAfterStart` must not be, since FSYNC_ON_SEAL and
//! KICK_AT_START are adopted.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ekacache_testkit::itf::{self, State, Trace, Value};
use ekacached::log::Logger;
use ekacached::store::{
    DemoteStep, DemoterMode, HINT_HEADER, Prepared, Snapshot, Store, StoreConfig, TestClock, Tier,
};

const BASE: i64 = 1_700_000_000_000_000_000;
const SCALE: i64 = 1_000_000_000;
const PACK_LIMIT: i64 = 24;
const BUDGET: [i64; 2] = [30, 45];
const TOUCH_EVERY: i64 = 2;
const REC_HEADER: i64 = 8;
const KLEN: i64 = 1;

type Res<T> = Result<T, String>;

fn e(x: itf::ItfError) -> String {
    x.0
}

#[derive(Debug, Clone)]
struct MRec {
    key: String,
    vlen: i64,
    off: i64,
    ver: i64,
}

#[derive(Debug, Clone)]
struct MFile {
    recs: Vec<MRec>,
    tail: i64,
    has_hint: bool,
    hint: BTreeSet<(Vec<u8>, i64, i64)>,
    mtime: i64,
}

impl MFile {
    fn size(&self) -> i64 {
        self.recs
            .iter()
            .map(|r| REC_HEADER + KLEN + r.vlen)
            .sum::<i64>()
            + self.tail
    }
}

#[derive(Debug, Clone, Copy)]
struct MLoc {
    pack: i64,
    off: i64,
    len: i64,
}

#[derive(Debug, Clone, Copy)]
struct MPack {
    tier: i64,
    size: i64,
    live: i64,
    used: i64,
    touched: i64,
}

/// The model variables a replay needs.
#[derive(Debug, Clone)]
struct Model {
    files: BTreeMap<(i64, i64), MFile>,
    index: BTreeMap<String, MLoc>,
    packs: BTreeMap<i64, MPack>,
    active: i64,
    totals: Vec<i64>,
    demoting: bool,
    copying: i64,
    now: i64,
    next_ver: i64,
}

fn int(v: &Value, f: &str) -> Res<i64> {
    v.field(f).and_then(Value::int).map_err(e)
}

impl Model {
    fn parse(s: &State) -> Res<Model> {
        let mut files = BTreeMap::new();
        for (k, f) in s.var("files").and_then(Value::map).map_err(e)? {
            let kt = k.elems().map_err(e)?;
            let key = (kt[0].int().map_err(e)?, kt[1].int().map_err(e)?);
            let mut recs = Vec::new();
            for r in f.field("recs").and_then(Value::elems).map_err(e)? {
                recs.push(MRec {
                    key: r.field("key").and_then(Value::str).map_err(e)?.to_owned(),
                    vlen: int(r, "vlen")?,
                    off: int(r, "off")?,
                    ver: int(r, "ver")?,
                });
            }
            let mut hint = BTreeSet::new();
            for h in f.field("hint").and_then(Value::elems).map_err(e)? {
                let k = h
                    .field("key")
                    .and_then(Value::str)
                    .map_err(e)?
                    .as_bytes()
                    .to_vec();
                hint.insert((k, int(h, "vlen")?, int(h, "off")?));
            }
            files.insert(
                key,
                MFile {
                    recs,
                    tail: int(f, "tail")?,
                    has_hint: f.field("hasHint").and_then(Value::bool).map_err(e)?,
                    hint,
                    mtime: int(f, "mtime")?,
                },
            );
        }
        let mut index = BTreeMap::new();
        for (k, l) in s.var("index").and_then(Value::map).map_err(e)? {
            index.insert(
                k.str().map_err(e)?.to_owned(),
                MLoc {
                    pack: int(l, "pack")?,
                    off: int(l, "off")?,
                    len: int(l, "len")?,
                },
            );
        }
        let mut packs = BTreeMap::new();
        for (k, p) in s.var("packs").and_then(Value::map).map_err(e)? {
            packs.insert(
                k.int().map_err(e)?,
                MPack {
                    tier: int(p, "tier")?,
                    size: int(p, "size")?,
                    live: int(p, "live")?,
                    used: int(p, "used")?,
                    touched: int(p, "touched")?,
                },
            );
        }
        let totals = s
            .var("totals")
            .and_then(Value::elems)
            .map_err(e)?
            .iter()
            .map(Value::int)
            .collect::<Result<_, _>>()
            .map_err(e)?;
        Ok(Model {
            files,
            index,
            packs,
            active: s.var("active").and_then(Value::int).map_err(e)?,
            totals,
            demoting: s.var("demoting").and_then(Value::bool).map_err(e)?,
            copying: s.var("copying").and_then(Value::int).map_err(e)?,
            now: s.var("now").and_then(Value::int).map_err(e)?,
            next_ver: s.var("nextVer").and_then(Value::int).map_err(e)?,
        })
    }

    /// The version of the record an index entry points at.
    fn ver_of(&self, key: &str, l: MLoc) -> Res<i64> {
        let p = self
            .packs
            .get(&l.pack)
            .ok_or_else(|| format!("index points at unknown pack {}", l.pack))?;
        let f = self
            .files
            .get(&(p.tier, l.pack))
            .ok_or_else(|| format!("no file for pack {}", l.pack))?;
        f.recs
            .iter()
            .find(|r| r.key == key && r.off + REC_HEADER + KLEN == l.off && r.vlen == l.len)
            .map(|r| r.ver)
            .ok_or_else(|| format!("no record for {key} at {}", l.off))
    }
}

/// Which of the model's witnesses hold in a state.
#[derive(Debug, Default, Clone, Copy)]
struct Witnesses {
    eviction: usize,
    demoted: usize,
    power_loss_fallback: usize,
    over_budget_after_start: usize,
}

impl Witnesses {
    fn add(&mut self, st: &State) -> Res<()> {
        let m = Model::parse(st)?;
        let max_id = st.var("maxId").and_then(Value::int).map_err(e)?;
        let power_lost = st.var("powerLost").and_then(Value::bool).map_err(e)?;
        let n = i64::try_from(m.packs.len()).map_err(|x| x.to_string())?;
        // not(witnessEviction) = not(maxId < 4 or |packs| >= maxId - 1)
        if max_id >= 4 && n < max_id - 1 {
            self.eviction += 1;
        }
        if m.packs.values().any(|p| p.tier != 0) {
            self.demoted += 1;
        }
        // newestWinsRaw: no pack newer than an entry's holds a record of its key
        let newest = m.index.iter().all(|(k, l)| {
            m.packs
                .iter()
                .filter(|(id, _)| **id > l.pack)
                .all(|(id, p)| {
                    m.files
                        .get(&(p.tier, *id))
                        .is_none_or(|f| f.recs.iter().all(|r| r.key != *k))
                })
        });
        if power_lost && !m.index.is_empty() && !newest {
            self.power_loss_fallback += 1;
        }
        if !m.demoting && m.copying == -1 {
            for (t, budget) in BUDGET.iter().enumerate() {
                let t64 = i64::try_from(t).map_err(|x| x.to_string())?;
                let victim = m
                    .packs
                    .iter()
                    .any(|(id, p)| p.tier == t64 && *id != m.active);
                if *budget > 0 && m.totals[t] > *budget && victim {
                    self.over_budget_after_start += 1;
                }
            }
        }
        Ok(())
    }
}

/// The bytes of version `ver` of a value.
fn value(ver: i64, vlen: i64) -> Vec<u8> {
    (0..vlen).map(|i| (ver * 31 + i * 7 + 1) as u8).collect()
}

fn rt(t: i64) -> i64 {
    BASE + t * SCALE
}

struct Replay {
    dirs: [tempfile::TempDir; 2],
    clock: Arc<TestClock>,
    store: Option<Store>,
    prepared: Option<Prepared>,
    ids: BTreeMap<i64, u32>,
}

impl Replay {
    fn new() -> Res<Replay> {
        let dirs = [
            tempfile::tempdir().map_err(|x| x.to_string())?,
            tempfile::tempdir().map_err(|x| x.to_string())?,
        ];
        let mut r = Replay {
            dirs,
            clock: Arc::new(TestClock::fixed(rt(0))),
            store: None,
            prepared: None,
            ids: BTreeMap::new(),
        };
        r.open()?;
        // model `init` is not an OpenStore: consume the real open's kick (nothing to demote)
        let step = r.store()?.demote_step();
        if !matches!(step, DemoteStep::Idle) {
            return Err(format!("first demote step on an empty store: {step:?}"));
        }
        r.ids.insert(1, 1);
        Ok(r)
    }

    fn dir(&self, t: i64) -> &Path {
        self.dirs[usize::try_from(t).unwrap_or(0)].path()
    }

    fn open(&mut self) -> Res<()> {
        let tiers = (0..2)
            .map(|t| Tier {
                dir: self.dir(t).as_os_str().as_encoded_bytes().to_vec(),
                budget: BUDGET[usize::try_from(t).unwrap_or(0)],
            })
            .collect();
        let mut cfg = StoreConfig::new(tiers);
        cfg.pack_limit = PACK_LIMIT;
        cfg.touch_every_ns = TOUCH_EVERY * SCALE;
        cfg.clock = self.clock.clone();
        cfg.logger = Logger::capture().0;
        cfg.demoter = DemoterMode::Manual;
        self.store = Some(Store::open(cfg).map_err(|x| format!("open: {x}"))?);
        Ok(())
    }

    fn store(&self) -> Res<&Store> {
        self.store.as_ref().ok_or_else(|| "no store".to_string())
    }

    fn real(&self, id: i64) -> Res<u32> {
        self.ids
            .get(&id)
            .copied()
            .ok_or_else(|| format!("model pack {id} has no real id"))
    }

    fn path(&self, t: i64, real: u32, ext: &str) -> PathBuf {
        self.dir(t).join(format!("{real:06}.{ext}"))
    }

    /// Give every file the model's mtime, slower tier first.
    fn set_mtimes(&self, m: &Model) -> Res<()> {
        for ((t, id), f) in m.files.iter().rev() {
            let Ok(real) = self.real(*id) else { continue };
            let path = self.path(*t, real, "pack");
            if path.exists() {
                ekacached::gofs::set_mtime(path.as_os_str().as_encoded_bytes(), rt(f.mtime))
                    .map_err(|x| x.to_string())?;
            }
        }
        Ok(())
    }

    fn restart_done(&mut self, post: &Model) -> Res<()> {
        let snap = self.store()?.snapshot();
        self.ids.insert(post.active, snap.active);
        Ok(())
    }

    fn step(&mut self, pre: &Model, post: &Model, st: &State) -> Res<()> {
        let action = st.action().map_err(e)?;
        match action {
            "doPut" => {
                let k = st
                    .pick("k")
                    .map_err(e)?
                    .ok_or("doPut without k")?
                    .str()
                    .map_err(e)?
                    .to_owned();
                let v = st
                    .pick("v")
                    .map_err(e)?
                    .ok_or("doPut without v")?
                    .int()
                    .map_err(e)?;
                let body = value(pre.next_ver, v);
                self.store()?
                    .put_reader(k.as_bytes(), v, &mut &body[..])
                    .map_err(|x| format!("put: {x}"))?;
                if post.active != pre.active {
                    let real = self.store()?.snapshot().active;
                    self.ids.insert(post.active, real);
                }
            }
            "doGet" => {
                let k = st
                    .pick("k")
                    .map_err(e)?
                    .ok_or("doGet without k")?
                    .str()
                    .map_err(e)?
                    .to_owned();
                let l = pre.index[&k];
                let want = value(pre.ver_of(&k, l)?, l.len);
                let mut r = self
                    .store()?
                    .get(k.as_bytes())
                    .ok_or_else(|| format!("get {k}: miss"))?;
                let mut got = Vec::new();
                std::io::Read::read_to_end(&mut r, &mut got).map_err(|x| x.to_string())?;
                if got != want {
                    return Err(format!("get {k}: {got:?}, want {want:?}"));
                }
            }
            "tick" => self.clock.set(rt(post.now)),
            "demoteStep" => {
                let step = self.store()?.demote_step();
                match step {
                    DemoteStep::Idle if !post.demoting => {}
                    DemoteStep::Copied(p)
                        if post.copying != -1 && p.id() == self.real(post.copying)? =>
                    {
                        self.prepared = Some(p);
                    }
                    DemoteStep::Evicted(id) => {
                        let gone: Vec<i64> = pre
                            .packs
                            .keys()
                            .filter(|k| !post.packs.contains_key(k))
                            .copied()
                            .collect();
                        if gone.len() != 1 || self.real(gone[0])? != id {
                            return Err(format!("evicted {id}, model evicted {gone:?}"));
                        }
                    }
                    other => {
                        return Err(format!(
                            "demote step {other:?}, model copying={} demoting={}",
                            post.copying, post.demoting
                        ));
                    }
                }
            }
            "demoteCommit" => {
                let p = self.prepared.take().ok_or("commit without a copy")?;
                self.store()?.demote_commit(p);
            }
            "crashAndRestart" => {
                let power = st
                    .pick("power")
                    .map_err(e)?
                    .ok_or("no power pick")?
                    .bool()
                    .map_err(e)?;
                let keep = st
                    .pick("keepActive")
                    .map_err(e)?
                    .ok_or("no keepActive pick")?
                    .int()
                    .map_err(e)?;
                let torn = st
                    .pick("torn")
                    .map_err(e)?
                    .ok_or("no torn pick")?
                    .int()
                    .map_err(e)?;
                // kill -9: nothing sealed, an in-flight copy stays in both tiers
                self.prepared = None;
                self.store = None;
                let af = &pre.files[&(0, pre.active)];
                let path = self.path(0, self.real(pre.active)?, "pack");
                let f = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .map_err(|x| x.to_string())?;
                let mut len = af.size() - af.tail;
                if power {
                    len = af
                        .recs
                        .iter()
                        .take(usize::try_from(keep).unwrap_or(0))
                        .map(|r| REC_HEADER + KLEN + r.vlen)
                        .sum();
                }
                f.set_len(len as u64).map_err(|x| x.to_string())?;
                f.write_all_at(&vec![0u8; usize::try_from(torn).unwrap_or(0)], len as u64)
                    .map_err(|x| x.to_string())?;
                drop(f);
                self.set_mtimes(pre)?;
                self.open()?;
                self.restart_done(post)?;
            }
            "cleanRestart" => {
                self.store()?.close().map_err(|x| format!("close: {x}"))?;
                self.store = None;
                self.set_mtimes(pre)?;
                self.open()?;
                self.restart_done(post)?;
            }
            other => return Err(format!("unknown action {other}")),
        }
        Ok(())
    }

    fn compare(&self, m: &Model) -> Res<()> {
        let store = self.store()?;
        let snap: Snapshot = store.snapshot();
        // packs
        let want_ids: BTreeSet<u32> = m
            .packs
            .keys()
            .map(|id| self.real(*id))
            .collect::<Res<_>>()?;
        let got_ids: BTreeSet<u32> = snap.packs.keys().copied().collect();
        if want_ids != got_ids {
            return Err(format!(
                "packs {got_ids:?}, model {want_ids:?} (map {:?})",
                self.ids
            ));
        }
        for (id, mp) in &m.packs {
            let p = &snap.packs[&self.real(*id)?];
            let got = (p.tier as i64, p.size, p.live, p.used, p.touched);
            let want = (mp.tier, mp.size, mp.live, rt(mp.used), rt(mp.touched));
            if got != want {
                return Err(format!(
                    "pack {id}: (tier,size,live,used,touched) {got:?}, model {want:?}"
                ));
            }
        }
        // index
        let mut want_index = BTreeMap::new();
        for (k, l) in &m.index {
            want_index.insert(k.as_bytes().to_vec(), (self.real(l.pack)?, l.off, l.len));
        }
        let got_index: BTreeMap<Vec<u8>, (u32, i64, i64)> = snap
            .index
            .iter()
            .map(|(k, l)| (k.clone(), (l.pack, i64::from(l.off), i64::from(l.len))))
            .collect();
        if want_index != got_index {
            return Err(format!("index {got_index:?}, model {want_index:?}"));
        }
        if snap.totals != m.totals {
            return Err(format!("totals {:?}, model {:?}", snap.totals, m.totals));
        }
        if snap.active != self.real(m.active)? {
            return Err(format!("active {}, model {}", snap.active, m.active));
        }
        if store.kick_pending() != m.demoting {
            return Err(format!(
                "kick pending {}, model demoting {}",
                store.kick_pending(),
                m.demoting
            ));
        }
        let copying = self.prepared.as_ref().map(Prepared::id);
        let want_copying = if m.copying == -1 {
            None
        } else {
            Some(self.real(m.copying)?)
        };
        if copying != want_copying {
            return Err(format!("copying {copying:?}, model {want_copying:?}"));
        }
        // disk
        for t in 0..2i64 {
            let mut want = BTreeSet::new();
            for ((ft, id), f) in &m.files {
                if *ft != t {
                    continue;
                }
                let real = self.real(*id)?;
                want.insert(format!("{real:06}.pack"));
                let path = self.path(t, real, "pack");
                let size = std::fs::metadata(&path)
                    .map_err(|x| format!("{}: {x}", path.display()))?
                    .len();
                if size as i64 != f.size() {
                    return Err(format!(
                        "{}: size {size}, model {}",
                        path.display(),
                        f.size()
                    ));
                }
                if f.has_hint {
                    want.insert(format!("{real:06}.hint"));
                    let hint =
                        std::fs::read(self.path(t, real, "hint")).map_err(|x| x.to_string())?;
                    let got = parse_hint(&hint)?;
                    if got != f.hint {
                        return Err(format!(
                            "hint of {real:06} in tier {t}: {got:?}, model {:?}",
                            f.hint
                        ));
                    }
                }
            }
            let got: BTreeSet<String> = std::fs::read_dir(self.dir(t))
                .map_err(|x| x.to_string())?
                .filter_map(Result::ok)
                .map(|d| d.file_name().to_string_lossy().into_owned())
                .collect();
            if got != want {
                return Err(format!("tier {t} holds {got:?}, model {want:?}"));
            }
        }
        // the bytes behind every index entry are the version the model says
        for (k, l) in &m.index {
            let want = value(m.ver_of(k, *l)?, l.len);
            let p = &snap.packs[&self.real(l.pack)?];
            let path = PathBuf::from(String::from_utf8_lossy(&p.path).into_owned());
            let f = std::fs::File::open(&path).map_err(|x| x.to_string())?;
            let mut got = vec![0u8; usize::try_from(l.len).unwrap_or(0)];
            f.read_exact_at(&mut got, l.off as u64)
                .map_err(|x| format!("{}: {x}", path.display()))?;
            if got != want {
                return Err(format!("value of {k}: {got:?}, model {want:?}"));
            }
        }
        Ok(())
    }
}

fn parse_hint(b: &[u8]) -> Res<BTreeSet<(Vec<u8>, i64, i64)>> {
    let mut out = BTreeSet::new();
    let mut at = 0;
    while at < b.len() {
        if at + HINT_HEADER > b.len() {
            return Err("hint ends mid-header".into());
        }
        let klen = u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) as usize;
        let vlen = u32::from_le_bytes([b[at + 4], b[at + 5], b[at + 6], b[at + 7]]);
        let mut off = [0u8; 8];
        off.copy_from_slice(&b[at + 8..at + 16]);
        let Some(key) = b.get(at + HINT_HEADER..at + HINT_HEADER + klen) else {
            return Err("hint ends mid-key".into());
        };
        if !out.insert((
            key.to_vec(),
            i64::from(vlen),
            u64::from_le_bytes(off) as i64,
        )) {
            return Err(format!("hint lists {key:?} twice"));
        }
        at += HINT_HEADER + klen;
    }
    Ok(out)
}

fn replay(trace: &Trace) -> Res<usize> {
    let first = trace.states.first().ok_or("empty trace")?;
    if first.action().map_err(e)? != "init" {
        return Err("trace does not start with init".into());
    }
    let mut r = Replay::new()?;
    let mut pre = Model::parse(first)?;
    r.compare(&pre)
        .map_err(|x| format!("state 0 (init): {x}"))?;
    for (i, st) in trace.states.iter().enumerate().skip(1) {
        let post = Model::parse(st)?;
        let action = st.action().map_err(e)?.to_owned();
        r.step(&pre, &post, st)
            .map_err(|x| format!("state {i} ({action}): {x}"))?;
        r.compare(&post)
            .map_err(|x| format!("state {i} ({action}): {x}"))?;
        pre = post;
    }
    Ok(trace.states.len())
}

#[test]
fn store_traces_replay() {
    let files = itf::trace_files("store");
    if files.is_empty() {
        eprintln!(
            "note: no traces under tests/itf/store/; generate them with quint (see this file's header). Skipping."
        );
        return;
    }
    let mut actions: BTreeMap<String, usize> = BTreeMap::new();
    let mut states = 0;
    let mut w = Witnesses::default();
    for path in &files {
        let trace = Trace::read(path).unwrap_or_else(|x| panic!("{x}"));
        for st in &trace.states {
            *actions
                .entry(st.action().unwrap_or("?").to_owned())
                .or_default() += 1;
            w.add(st)
                .unwrap_or_else(|x| panic!("{}: {x}", path.display()));
        }
        states += replay(&trace).unwrap_or_else(|x| panic!("{}: {x}", path.display()));
    }
    eprintln!(
        "replayed {} traces, {states} states: {actions:?}; witnesses {w:?}",
        files.len()
    );
    assert!(w.eviction > 0, "witnessEviction never reached");
    assert!(w.demoted > 0, "witnessDemoted never reached");
    assert_eq!(w.power_loss_fallback, 0, "witnessPowerLossFallback reached");
    assert_eq!(
        w.over_budget_after_start, 0,
        "idleWithinBudgetEvenAfterStart violated"
    );
}
