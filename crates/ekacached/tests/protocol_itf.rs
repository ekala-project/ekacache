//! Replay of `spec/jigd_protocol.qnt` ITF traces against the Rust jigd over its unix socket
//! (`behaviors/jigd-protocol.md`; implementation/12 §L4). Each trace runs one in-process server
//! (`common::InProc`, `LIMIT` = 1 slot) and one real connection per model connection.
//!
//! Traces are generated locally and not committed (`/tests/itf/*/` is ignored). The model's own
//! `step` ends a trace after about five steps (most requests it picks are malformed, and a
//! malformed request ends the connection), so a second set comes from
//! `tests/itf-wrappers/jigd_protocol_long.qnt`, whose `longStep` takes a subset of `step`:
//!
//! ```text
//! quint run spec/jigd_protocol.qnt --mbt --out-itf='tests/itf/jigd_protocol/t{seq}.itf.json' \
//!     --n-traces=300 --max-steps=40 --max-samples=300 --seed=1 --invariant=safety
//! quint run tests/itf-wrappers/jigd_protocol_long.qnt --step=longStep --mbt \
//!     --out-itf='tests/itf/jigd_protocol_long/t{seq}.itf.json' --n-traces=200 --max-steps=60 \
//!     --max-samples=200 --seed=1 --invariant=safety
//! ```
//!
//! With no traces the test passes and says so.
//!
//! What is compared: every reply and every hang-up. When the model's `serveOne(c)` answers, the
//! real connection must answer the same (`OK`, `OK <len>` and the value that was PUT, `MISS`,
//! `1`/`0`, `n` IDS lines, one STATS line); when the model hangs up, the real one must close with
//! no reply byte; when the model blocks in SLOT, nothing may arrive; at `granted(c)` the blocked
//! connection must get its `OK`. After every step the slot pool's tokens out and waiters must
//! come to what the model implies (below).
//!
//! Where the replay and the model differ in form only:
//!  - a request's bytes are written when the model's `serveOne` reads it, not at `send`. A real
//!    server reads whatever is in the socket, so bytes written at `send` would be served early.
//!    Pipelining itself is covered by `serve.rs`. Requests still in the model's inbox when the
//!    connection goes are never written;
//!  - `evict(k)` is a new key generation: model key `k` is the real key `k#<gen>`, and evicting
//!    bumps the generation, so later requests see a key that is absent, as after an eviction;
//!  - the pool grants a freed token at once, where the model's `granted` may come later. So the
//!    real pool has `min(LIMIT, out + blocked)` tokens out and the rest of the blocked
//!    connections waiting, where `out` and `blocked` are the model's;
//!  - the model leaves the order in which two blocked connections are granted open (slots.qnt
//!    pins it). When the model grants the one the pool did not, the trace stops there and is
//!    counted, since nothing after it can be compared.
//!
//! Request bytes: `PUT k#g <len>` + `value-<v>`; `PutBadLen` = `PUT k#g x`; `PutBadKey` = a
//! 5 000-byte key (over `MAX_KEY`); `PutShortBody` = `PUT k#g 10` + 3 bytes, then a half close;
//! `IDS n` + `n` paths that do not exist; `IdsBadCount` = `IDS -1`; `Unknown` = `FOO`;
//! `TooLong` = 65 536 bytes and no newline.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use common::{Client, End, InProc, PATIENCE, eventually, show};
use ekacache_testkit::itf::{self, State, Trace, Value};

/// The model's `LIMIT`.
const LIMIT: i64 = 1;
/// How long a blocked SLOT is watched for a wrong answer.
const BLOCKED_WAIT: Duration = Duration::from_millis(20);

type Res<T> = Result<T, String>;

fn e(x: itf::ItfError) -> String {
    x.0
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

fn conn_var<'a>(st: &'a State, var: &str, c: i64) -> Res<&'a Value> {
    by_conn(st, var)?
        .get(&c)
        .copied()
        .ok_or(format!("{var} has no conn {c}"))
}

/// `Reading`, `InSlot` or `Gone`.
fn cstate(st: &State, c: i64) -> Res<String> {
    Ok(conn_var(st, "cstate", c)?
        .variant()
        .map_err(e)?
        .0
        .to_owned())
}

/// The model's tokens out and number of connections blocked in SLOT.
fn pool(st: &State) -> Res<(i64, i64)> {
    let out = st.var("out").map_err(e)?.int().map_err(e)?;
    let mut blocked = 0;
    for c in by_conn(st, "cstate")?.keys() {
        if cstate(st, *c)? == "InSlot" {
            blocked += 1;
        }
    }
    Ok((out, blocked))
}

fn value_of(v: i64) -> Vec<u8> {
    format!("value-{v}").into_bytes()
}

/// What a replay saw, for the coverage report.
#[derive(Default)]
struct Seen {
    states: usize,
    requests: BTreeMap<String, usize>,
    hits: usize,
    misses_after_evict: usize,
    blocked: usize,
    grants: usize,
    grant_order_stops: usize,
}

struct Replay {
    srv: InProc,
    conns: HashMap<i64, Client>,
    generation: HashMap<String, u32>,
    evicted: HashMap<String, bool>,
}

impl Replay {
    fn new(init: &State) -> Res<Replay> {
        let srv = InProc::start(LIMIT);
        let mut conns = HashMap::new();
        for c in by_conn(init, "cstate")?.keys() {
            conns.insert(*c, srv.connect());
        }
        Ok(Replay {
            srv,
            conns,
            generation: HashMap::new(),
            evicted: HashMap::new(),
        })
    }

    fn key(&self, k: &str) -> Vec<u8> {
        format!("{k}#{}", self.generation.get(k).copied().unwrap_or(0)).into_bytes()
    }

    fn conn(&mut self, c: i64) -> Res<&mut Client> {
        self.conns
            .get_mut(&c)
            .ok_or(format!("conn {c} is already closed"))
    }

    /// The bytes of request `r`, and whether the client half-closes after them.
    fn encode(&self, r: &Value) -> Res<(String, Vec<u8>, bool)> {
        let (tag, arg) = r.variant().map_err(e)?;
        let mut out = Vec::new();
        let mut half = false;
        match tag {
            "Get" | "Has" => {
                out.extend_from_slice(if tag == "Get" { b"GET " } else { b"HAS " });
                out.extend_from_slice(&self.key(arg.str().map_err(e)?));
                out.push(b'\n');
            }
            "Put" => {
                let kv = arg.elems().map_err(e)?;
                let [k, v] = kv else {
                    return Err(format!("Put payload {arg:?}"));
                };
                out = common::put_req(
                    &self.key(k.str().map_err(e)?),
                    &value_of(v.int().map_err(e)?),
                );
            }
            "PutBadLen" => {
                out.extend_from_slice(b"PUT ");
                out.extend_from_slice(&self.key(arg.str().map_err(e)?));
                out.extend_from_slice(b" x\n");
            }
            "PutBadKey" => {
                out.extend_from_slice(b"PUT ");
                out.extend(std::iter::repeat_n(b'k', 5000));
                out.extend_from_slice(b" 1\n");
            }
            "PutShortBody" => {
                out.extend_from_slice(b"PUT ");
                out.extend_from_slice(&self.key(arg.str().map_err(e)?));
                out.extend_from_slice(b" 10\nabc");
                half = true;
            }
            "Ids" => {
                let n = arg.int().map_err(e)?;
                out.extend_from_slice(format!("IDS {n}\n").as_bytes());
                for i in 0..n {
                    out.extend_from_slice(format!("/nonexistent/jig-itf/{i}\n").as_bytes());
                }
            }
            "IdsBadCount" => out.extend_from_slice(b"IDS -1\n"),
            "Slot" | "Done" => {
                out.extend_from_slice(if tag == "Slot" { b"SLOT " } else { b"DONE " });
                out.extend_from_slice(arg.str().map_err(e)?.as_bytes());
                out.push(b'\n');
            }
            "Stats" => out.extend_from_slice(b"STATS\n"),
            "Unknown" => out.extend_from_slice(b"FOO\n"),
            "TooLong" => out.extend(std::iter::repeat_n(b'a', 65536)),
            t => return Err(format!("unknown request {t}")),
        }
        Ok((tag.to_owned(), out, half))
    }

    /// Read the reply the model gave, `resp`, from connection `c`.
    fn expect(&mut self, c: i64, resp: &Value, req: &Value, seen: &mut Seen) -> Res<()> {
        let (tag, arg) = resp.variant().map_err(e)?;
        let req_key = match req.variant().map_err(e)? {
            ("Get", k) => Some(k.str().map_err(e)?.to_owned()),
            _ => None,
        };
        let cl = self.conn(c)?;
        match tag {
            "ROk" => want_line(cl, "OK\\n"),
            "RMiss" => {
                if req_key.is_some_and(|k| self.evicted.get(&k).copied().unwrap_or(false)) {
                    seen.misses_after_evict += 1;
                }
                want_line(self.conn(c)?, "MISS\\n")
            }
            "RHas" => want_line(
                cl,
                if arg.bool().map_err(e)? {
                    "1\\n"
                } else {
                    "0\\n"
                },
            ),
            "RVal" => {
                let val = value_of(arg.int().map_err(e)?);
                want_line(cl, &show(format!("OK {}\n", val.len()).as_bytes()))?;
                let got = cl
                    .try_exact(val.len())
                    .ok_or(format!("GET body cut short: {:?}", cl.end()))?;
                if got != val {
                    return Err(format!("GET body {} want {}", show(&got), show(&val)));
                }
                seen.hits += 1;
                Ok(())
            }
            "RIds" => {
                for i in 0..arg.int().map_err(e)? {
                    cl.try_line()
                        .ok_or(format!("IDS line {i} missing: {:?}", cl.end()))?;
                }
                Ok(())
            }
            "RStats" => {
                let l = cl
                    .try_line()
                    .ok_or(format!("no STATS line: {:?}", cl.end()))?;
                if l.len() < 2 {
                    return Err(format!("STATS line {}", show(&l)));
                }
                Ok(())
            }
            t => Err(format!("unknown reply {t}")),
        }
    }

    /// The model's `serveOne(c)`: write its next request, then check the answer.
    fn serve(&mut self, c: i64, pre: &State, post: &State, seen: &mut Seen) -> Res<()> {
        let inbox = conn_var(pre, "inbox", c)?.elems().map_err(e)?;
        let req = inbox.first().ok_or("serveOne with an empty inbox")?;
        let (tag, bytes, half) = self.encode(req)?;
        *seen.requests.entry(tag.clone()).or_default() += 1;
        let cl = self.conn(c)?;
        cl.send(&bytes);
        if half {
            cl.half_close();
        }
        match cstate(post, c)?.as_str() {
            "Gone" => {
                let (rest, end) = cl.rest();
                if !rest.is_empty() || !matches!(end, End::Eof | End::Reset) {
                    return Err(format!(
                        "{tag}: want a hang-up with no reply, got {} and {end:?}",
                        show(&rest)
                    ));
                }
                self.conns.remove(&c);
                Ok(())
            }
            "InSlot" => {
                seen.blocked += 1;
                let got = cl.within(BLOCKED_WAIT);
                if !got.is_empty() || cl.end().is_some() {
                    return Err(format!(
                        "SLOT should block, got {} and {:?}",
                        show(&got),
                        cl.end()
                    ));
                }
                Ok(())
            }
            _ => {
                let answered = conn_var(post, "answered", c)?.elems().map_err(e)?;
                let resp = answered.last().ok_or("no answer in the model")?;
                self.expect(c, resp, req, seen)
                    .map_err(|x| format!("{tag}: {x}"))
            }
        }
    }

    /// The model's `granted(c)`. False if the pool granted the other blocked connection.
    fn granted(&mut self, c: i64, pre: &State) -> Res<bool> {
        let others: Vec<i64> = by_conn(pre, "cstate")?
            .keys()
            .copied()
            .filter(|&d| d != c)
            .filter(|&d| cstate(pre, d).is_ok_and(|s| s == "InSlot"))
            .collect();
        let deadline = Instant::now() + PATIENCE;
        loop {
            let got = self.conn(c)?.within(Duration::from_millis(2));
            if !got.is_empty() {
                if got != b"OK\n" {
                    return Err(format!("granted SLOT answered {}", show(&got)));
                }
                return Ok(true);
            }
            for d in &others {
                if !self.conn(*d)?.within(Duration::from_millis(2)).is_empty() {
                    return Ok(false);
                }
            }
            if Instant::now() > deadline {
                return Err(format!(
                    "granted SLOT never answered: {:?}",
                    self.conn(c)?.end()
                ));
            }
        }
    }

    /// The pool must come to `min(LIMIT, out + blocked)` out and the rest waiting.
    fn check_pool(&self, st: &State) -> Res<()> {
        let (out, blocked) = pool(st)?;
        let real_out = LIMIT.min(out + blocked);
        let waiting = usize::try_from(blocked - (real_out - out)).map_err(|x| x.to_string())?;
        let want = (real_out, waiting);
        if eventually(|| self.srv.slots() == want) {
            Ok(())
        } else {
            Err(format!(
                "slot pool {:?}, want {want:?} (model out {out}, {blocked} blocked)",
                self.srv.slots()
            ))
        }
    }

    fn step(&mut self, action: &str, pre: &State, post: &State, seen: &mut Seen) -> Res<bool> {
        let c = || -> Res<i64> {
            post.pick("c")
                .map_err(e)?
                .ok_or("no c pick")?
                .int()
                .map_err(e)
        };
        match action {
            "send" => {}
            "serveOne" => self.serve(c()?, pre, post, seen)?,
            "granted" => {
                seen.grants += 1;
                if !self.granted(c()?, pre)? {
                    seen.grant_order_stops += 1;
                    return Ok(false);
                }
            }
            "disconnect" => {
                self.conns.remove(&c()?);
            }
            "evict" => {
                let k = post
                    .pick("k")
                    .map_err(e)?
                    .ok_or("no k pick")?
                    .str()
                    .map_err(e)?
                    .to_owned();
                *self.generation.entry(k.clone()).or_default() += 1;
                self.evicted.insert(k, true);
            }
            a => return Err(format!("unknown action {a}")),
        }
        // a later PUT makes the key present again
        if action == "serveOne"
            && let Some(Ok(("Put", p))) = conn_var(pre, "inbox", c()?)?
                .elems()
                .map_err(e)?
                .first()
                .map(Value::variant)
            && let Some(k) = p
                .elems()
                .ok()
                .and_then(|x| x.first())
                .and_then(|k| k.str().ok())
        {
            self.evicted.insert(k.to_owned(), false);
        }
        self.check_pool(post)?;
        Ok(true)
    }
}

fn want_line(cl: &mut Client, want: &str) -> Res<()> {
    let l = cl
        .try_line()
        .ok_or(format!("no reply line, want {want}: {:?}", cl.end()))?;
    if show(&l) == want {
        Ok(())
    } else {
        Err(format!("reply {} want {want}", show(&l)))
    }
}

fn replay(trace: &Trace, seen: &mut Seen) -> Res<()> {
    let Some(first) = trace.states.first() else {
        return Err("empty trace".into());
    };
    if first.action().map_err(e)? != "init" {
        return Err("trace does not start with init".into());
    }
    let mut r = Replay::new(first)?;
    seen.states += 1;
    for (i, w) in trace.states.windows(2).enumerate() {
        let [pre, post] = w else { unreachable!() };
        let action = post.action().map_err(e)?.to_owned();
        let go_on = r
            .step(&action, pre, post, seen)
            .map_err(|x| format!("state {} ({action}): {x}", i + 1))?;
        if !go_on {
            return Ok(());
        }
        seen.states += 1;
    }
    Ok(())
}

fn run(model: &str) {
    let files = itf::trace_files(model);
    if files.is_empty() {
        eprintln!(
            "note: no traces under tests/itf/{model}/; generate them with quint (see this file's header). Skipping."
        );
        return;
    }
    let mut seen = Seen::default();
    let mut actions: BTreeMap<String, usize> = BTreeMap::new();
    for path in &files {
        let trace = Trace::read(path).unwrap_or_else(|x| panic!("{x}"));
        for st in &trace.states {
            *actions
                .entry(st.action().unwrap_or("?").to_owned())
                .or_default() += 1;
        }
        replay(&trace, &mut seen).unwrap_or_else(|x| panic!("{}: {x}", path.display()));
    }
    eprintln!(
        "{model}: replayed {} traces, {} states; actions {actions:?}; requests served {:?}; \
         {} GET hits, {} misses after evict, {} blocked SLOTs, {} grants ({} stopped on grant order)",
        files.len(),
        seen.states,
        seen.requests,
        seen.hits,
        seen.misses_after_evict,
        seen.blocked,
        seen.grants,
        seen.grant_order_stops,
    );
}

#[test]
fn protocol_traces_replay() {
    run("jigd_protocol");
}

#[test]
fn protocol_long_traces_replay() {
    run("jigd_protocol_long");
}
