//! Replay of `spec/slots.qnt` (module `slots`) ITF traces against [`jigd::slots::Slots`]
//! (`behaviors/jigd-slots.md` §State, §Operations, §Per-connection bookkeeping;
//! implementation/03 §SLOT with cancellation; the model has adopted Q-SLOT-1 on:
//! `CANCEL_ABANDONED = true`).
//!
//! Traces are generated locally and not committed (`/tests/itf/*/` is ignored):
//!
//! ```text
//! quint run --main=slots spec/slots.qnt --mbt --out-itf='tests/itf/slots/t{seq}.itf.json' \
//!     --n-traces=200 --max-steps=60 --max-samples=200 --seed=1 --invariant=safety
//! ```
//!
//! With no traces the test passes and says so. Each trace drives one `Slots::new(LIMIT)` and a
//! driver standing in for `serve()`'s per-connection bookkeeping (the tokens a connection owns,
//! its queued ticket, whether its peer is up). After every step the pool's tokens, per-build
//! counts and queue (waiter ids mapped back to connections), and every connection's phase, peer
//! and tokens, must equal the model's.
//!
//! Where the driver and the model differ in form only:
//!  - grants are observed by polling every queued ticket after each `release`, which is when
//!    the model's `wake` runs; a peer that closes later always finds its waiter still queued, so
//!    `cancel` must return true (the race where `grant` wins is the server's, not the pool's);
//!  - `drainOne` releases one build of the model's choosing: the build is the one whose count
//!    in the model's `connHeld` dropped (the action's own `b` shadows the step's `b` in
//!    `mbt::nondetPicks`).

use std::collections::{BTreeMap, HashMap};

use jig_testkit::itf::{self, State, Trace, Value};
use jigd::slots::{Acquire, Slots, Ticket};
use tokio::sync::oneshot::error::TryRecvError;

/// The model's `LIMIT`.
const LIMIT: i64 = 2;

type Res<T> = Result<T, String>;

fn e(x: itf::ItfError) -> String {
    x.0
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Idle,
    Blocked(Vec<u8>),
    Draining,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MConn {
    phase: Phase,
    up: bool,
    held: BTreeMap<Vec<u8>, i64>,
}

/// The observable state, as the model has it and as the driver rebuilds it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Model {
    out: i64,
    held: BTreeMap<Vec<u8>, i64>,
    /// `(conn, build)` in queue order
    waiting: Vec<(i64, Vec<u8>)>,
    conns: BTreeMap<i64, MConn>,
}

fn counts(v: &Value) -> Res<BTreeMap<Vec<u8>, i64>> {
    v.map()
        .map_err(e)?
        .iter()
        .map(|(k, n)| Ok((k.str().map_err(e)?.as_bytes().to_vec(), n.int().map_err(e)?)))
        .collect()
}

fn by_conn<'a>(st: &'a State, var: &str) -> Res<BTreeMap<i64, &'a Value>> {
    st.var(var)
        .map_err(e)?
        .map()
        .map_err(e)?
        .iter()
        .map(|(k, v)| Ok((k.int().map_err(e)?, v)))
        .collect()
}

impl Model {
    fn parse(st: &State) -> Res<Model> {
        let waiting = st
            .var("waiting")
            .map_err(e)?
            .elems()
            .map_err(e)?
            .iter()
            .map(|w| {
                Ok((
                    w.field("conn").map_err(e)?.int().map_err(e)?,
                    w.field("build")
                        .map_err(e)?
                        .str()
                        .map_err(e)?
                        .as_bytes()
                        .to_vec(),
                ))
            })
            .collect::<Res<_>>()?;
        let phases = by_conn(st, "conn")?;
        let ups = by_conn(st, "peerUp")?;
        let helds = by_conn(st, "connHeld")?;
        let mut conns = BTreeMap::new();
        for (c, p) in phases {
            let phase = match p.variant().map_err(e)? {
                ("Idle", _) => Phase::Idle,
                ("Blocked", b) => Phase::Blocked(b.str().map_err(e)?.as_bytes().to_vec()),
                ("Draining", _) => Phase::Draining,
                ("Closed", _) => Phase::Closed,
                (tag, _) => return Err(format!("conn {c}: unknown phase {tag}")),
            };
            let up = ups
                .get(&c)
                .ok_or(format!("conn {c} has no peerUp"))?
                .bool()
                .map_err(e)?;
            let held = counts(helds.get(&c).ok_or(format!("conn {c} has no connHeld"))?)?;
            conns.insert(c, MConn { phase, up, held });
        }
        Ok(Model {
            out: st.var("out").map_err(e)?.int().map_err(e)?,
            held: counts(st.var("held").map_err(e)?)?,
            waiting,
            conns,
        })
    }
}

/// One connection as `serve()` keeps it.
#[derive(Debug)]
struct Conn {
    phase: Phase,
    up: bool,
    held: BTreeMap<Vec<u8>, i64>,
    ticket: Option<Ticket>,
}

struct Driver {
    slots: Slots,
    conns: BTreeMap<i64, Conn>,
    /// waiter id -> connection
    owner: HashMap<u64, i64>,
}

fn inc(m: &mut BTreeMap<Vec<u8>, i64>, b: &[u8]) {
    *m.entry(b.to_vec()).or_insert(0) += 1;
}

fn dec(m: &mut BTreeMap<Vec<u8>, i64>, b: &[u8]) {
    if let Some(n) = m.get_mut(b) {
        *n -= 1;
        if *n <= 0 {
            m.remove(b);
        }
    }
}

impl Driver {
    fn new(init: &Model) -> Driver {
        let conns = init
            .conns
            .keys()
            .map(|&c| {
                let conn = Conn {
                    phase: Phase::Idle,
                    up: true,
                    held: BTreeMap::new(),
                    ticket: None,
                };
                (c, conn)
            })
            .collect();
        Driver {
            slots: Slots::new(LIMIT),
            conns,
            owner: HashMap::new(),
        }
    }

    fn observe(&self) -> Res<Model> {
        let snap = self.slots.snapshot();
        let waiting = snap
            .waiting
            .into_iter()
            .map(|(id, b)| match self.owner.get(&id) {
                Some(&c) => Ok((c, b)),
                None => Err(format!("waiter {id} belongs to no connection")),
            })
            .collect::<Res<_>>()?;
        let conns = self
            .conns
            .iter()
            .map(|(&c, k)| {
                let conn = MConn {
                    phase: k.phase.clone(),
                    up: k.up,
                    held: k.held.clone(),
                };
                (c, conn)
            })
            .collect();
        Ok(Model {
            out: snap.out,
            held: snap.held,
            waiting,
            conns,
        })
    }

    fn conn(&mut self, c: i64) -> Res<&mut Conn> {
        self.conns.get_mut(&c).ok_or(format!("no connection {c}"))
    }

    /// The model's `wake`: every waiter `grant` served returns from `Acquire`.
    fn collect(&mut self) -> Res<()> {
        for (&c, k) in &mut self.conns {
            let Some(t) = k.ticket.as_mut() else { continue };
            match t.ready.try_recv() {
                Ok(()) => {
                    let Phase::Blocked(b) = &k.phase else {
                        return Err(format!("conn {c} has a ticket but is {:?}", k.phase));
                    };
                    inc(&mut k.held, &b.clone());
                    self.owner.remove(&t.id);
                    k.ticket = None;
                    k.phase = if k.up { Phase::Idle } else { Phase::Draining };
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Closed) => return Err(format!("conn {c}: waiter dropped")),
            }
        }
        Ok(())
    }

    /// Release one token of `b` owned by `c`, then wake whoever `grant` served.
    fn release(&mut self, c: i64, b: &[u8]) -> Res<()> {
        dec(&mut self.conn(c)?.held, b);
        self.slots.release(b);
        self.collect()
    }

    fn step(&mut self, action: &str, st: &State, pre: &Model, post: &Model) -> Res<()> {
        let pick_c = st.pick("c").map_err(e)?;
        let c = pick_c.ok_or("no conn picked")?.int().map_err(e)?;
        let pick_b = st.pick("b").map_err(e)?;
        let b = match pick_b {
            Some(v) => v.str().map_err(e)?.as_bytes().to_vec(),
            None => Vec::new(),
        };
        let k = self.conn(c)?;
        match action {
            "requestSlot" => {
                if k.phase != Phase::Idle || !k.up {
                    return Err(format!("conn {c} cannot ask: {:?} up={}", k.phase, k.up));
                }
                match self.slots.acquire(&b) {
                    Acquire::Granted => inc(&mut self.conn(c)?.held, &b),
                    Acquire::Queued(t) => {
                        self.owner.insert(t.id, c);
                        let k = self.conn(c)?;
                        k.ticket = Some(t);
                        k.phase = Phase::Blocked(b);
                    }
                }
            }
            "done" => {
                if k.phase != Phase::Idle || !k.up {
                    return Err(format!("conn {c} cannot DONE: {:?} up={}", k.phase, k.up));
                }
                // Q-SLOT-2: an unheld DONE changes nothing
                if k.held.get(&b).copied().unwrap_or(0) > 0 {
                    self.release(c, &b)?;
                }
            }
            "peerCloses" => {
                k.up = false;
                match k.phase {
                    Phase::Idle => k.phase = Phase::Draining,
                    Phase::Blocked(_) => {
                        let t = k
                            .ticket
                            .take()
                            .ok_or(format!("conn {c} blocked, no ticket"))?;
                        k.phase = Phase::Draining;
                        self.owner.remove(&t.id);
                        // adopted Q-SLOT-1: the waiter leaves the queue holding nothing
                        if !self.slots.cancel(t.id) {
                            return Err(format!("conn {c}: waiter {} was not queued", t.id));
                        }
                    }
                    Phase::Draining | Phase::Closed => {}
                }
            }
            "drainOne" => {
                if k.phase != Phase::Draining {
                    return Err(format!("conn {c} is not draining: {:?}", k.phase));
                }
                if k.held.is_empty() {
                    k.phase = Phase::Closed;
                } else {
                    let b = drained(pre, post, c)?;
                    if !k.held.contains_key(&b) {
                        return Err(format!("conn {c} holds no {}", String::from_utf8_lossy(&b)));
                    }
                    self.release(c, &b)?;
                }
            }
            "reconnect" => {
                if k.phase != Phase::Closed {
                    return Err(format!("conn {c} is not closed: {:?}", k.phase));
                }
                k.phase = Phase::Idle;
                k.up = true;
                k.held.clear();
            }
            a => return Err(format!("unknown action {a}")),
        }
        Ok(())
    }
}

/// The build whose token the model's `drainOne` released for `c`.
fn drained(pre: &Model, post: &Model, c: i64) -> Res<Vec<u8>> {
    let (Some(a), Some(z)) = (pre.conns.get(&c), post.conns.get(&c)) else {
        return Err(format!("conn {c} missing from the model"));
    };
    let mut gone = a
        .held
        .iter()
        .filter(|(b, n)| z.held.get(*b).copied().unwrap_or(0) == **n - 1)
        .map(|(b, _)| b.clone());
    match (gone.next(), gone.next()) {
        (Some(b), None) => Ok(b),
        _ => Err(format!(
            "conn {c}: cannot tell the drained build from {:?} -> {:?}",
            a.held, z.held
        )),
    }
}

fn replay(trace: &Trace) -> Res<usize> {
    let Some(first) = trace.states.first() else {
        return Err("empty trace".into());
    };
    if first.action().map_err(e)? != "init" {
        return Err("trace does not start with init".into());
    }
    let mut pre = Model::parse(first)?;
    let mut d = Driver::new(&pre);
    let got = d.observe()?;
    if got != pre {
        return Err(format!("state 0 (init): got {got:?}\nwant {pre:?}"));
    }
    for (i, st) in trace.states.iter().enumerate().skip(1) {
        let post = Model::parse(st)?;
        let action = st.action().map_err(e)?.to_owned();
        d.step(&action, st, &pre, &post)
            .map_err(|x| format!("state {i} ({action}): {x}"))?;
        let got = d.observe()?;
        if got != post {
            return Err(format!("state {i} ({action}): got {got:?}\nwant {post:?}"));
        }
        pre = post;
    }
    Ok(trace.states.len())
}

#[test]
fn slots_traces_replay() {
    let files = itf::trace_files("slots");
    if files.is_empty() {
        eprintln!(
            "note: no traces under tests/itf/slots/; generate them with quint (see this file's header). Skipping."
        );
        return;
    }
    let mut actions: BTreeMap<String, usize> = BTreeMap::new();
    let mut states = 0;
    let mut queued = 0;
    for path in &files {
        let trace = Trace::read(path).unwrap_or_else(|x| panic!("{x}"));
        for st in &trace.states {
            *actions
                .entry(st.action().unwrap_or("?").to_owned())
                .or_default() += 1;
            if st
                .var("waiting")
                .and_then(|w| w.elems().map(<[Value]>::len))
                .unwrap_or(0)
                > 0
            {
                queued += 1;
            }
        }
        states += replay(&trace).unwrap_or_else(|x| panic!("{}: {x}", path.display()));
    }
    // the traces must exercise the queue, or the replay proves little
    assert!(queued > 0, "no trace ever queued a waiter");
    eprintln!(
        "replayed {} traces, {states} states ({queued} with waiters): {actions:?}",
        files.len()
    );
}
