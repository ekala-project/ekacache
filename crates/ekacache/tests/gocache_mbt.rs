//! Model-based test: `spec/gocache.qnt` traces replayed against two real `jig gocacheprog`
//! processes sharing one fake jigd (`implementation/08-jig-gocache-and-cache-cmds.md §Part 2
//! §Tests`, `behaviors/jig-gocacheprog.md §Concurrency model`).
//!
//! The model's steps are single daemon requests, so the fake jigd holds each process's
//! connection before `PUT go/a/` (between `putStart` and `putFinish`) and before `GET go/o/`
//! (between `getStart` and `getFinish`); evictions are done in the fake (jigd has no delete).
//! After every step the daemon contents, each process's state, `lastHit` and `lastErr` must
//! equal the model's, and `hitIsConsistent`, `noTornBodies` and `pointersAnnounced` are checked
//! on the real side. Model action `a<n>` is the ActionID `sha256("action a<n>")`, body `b` is
//! `b * 700` bytes of `'0' + b`, and its OutputID (model `100 + b`) is the body's SHA-256, as
//! Go computes it. A torn put sends one byte less than its `BodySize`.
//!
//! The traces in `fixtures/gocache/itf/` were generated with quint 0.33.1 and picked to cover
//! hits, misses, a body evicted during a get, torn puts, both evictions, concurrent puts and a
//! repointed action:
//!
//! ```text
//! quint run --mbt --invariant=safety --out-itf='t{seq}.itf.json' --n-traces=200 \
//!   --max-samples=200 --max-steps=40 --seed=0x5eed spec/gocache.qnt
//! ```
//!
//! `$JIG_GOCACHE_ITF_DIR` names a directory of more `*.itf.json` traces to replay.
#![allow(
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::many_single_char_names
)]

mod gocache_support;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

use gocache_support::{FakeJigd, HANDSHAKE, JIG, Run, b64, hex};

const TIMEOUT: Duration = Duration::from_secs(20);

fn sha(b: &[u8]) -> Vec<u8> {
    Sha256::digest(b).to_vec()
}

fn action_id(a: &str) -> Vec<u8> {
    sha(format!("action {a}").as_bytes())
}

fn body(b: i64) -> Vec<u8> {
    let n = usize::try_from(b).unwrap();
    vec![b'0' + u8::try_from(b).unwrap(); n * 700]
}

/// The real-side values of the model's constants.
struct World {
    actions: BTreeMap<Vec<u8>, String>,
    /// OutputID → model body `b`.
    outputs: BTreeMap<Vec<u8>, i64>,
}

impl World {
    fn new() -> Self {
        Self {
            actions: ["a1", "a2"]
                .iter()
                .map(|a| (action_id(a), (*a).to_owned()))
                .collect(),
            outputs: (1..=3).map(|b| (sha(&body(b)), b)).collect(),
        }
    }

    fn oid_of(&self, output_id: &[u8]) -> i64 {
        100 + self.outputs[output_id]
    }

    fn output_id(oid: i64) -> Vec<u8> {
        sha(&body(oid - 100))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PState {
    Idle,
    PutBodySent { act: String, out: i64 },
    GetAwaitBody { act: String, out: i64 },
}

struct Proc {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    conn: usize,
    dir: PathBuf,
    next_id: i64,
    state: PState,
}

impl Proc {
    fn start(fake: &FakeJigd, conn: usize, dir: PathBuf) -> Self {
        let mut cmd = Run::new(OsStr::new(JIG), Some(&fake.sock))
            .env("GOCACHEPROG_DIR", dir.as_os_str())
            .command();
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = cmd.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        let p = Self {
            child,
            stdin,
            lines,
            conn,
            dir,
            next_id: 1,
            state: PState::Idle,
        };
        assert_eq!(format!("{}\n", p.recv()), HANDSHAKE);
        fake.wait_conns(conn + 1);
        p.hold(fake);
        p
    }

    fn hold(&self, fake: &FakeJigd) {
        fake.hold(self.conn, "PUT go/a/");
        fake.hold(self.conn, "GET go/o/");
    }

    fn send(&mut self, line: &str) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        self.stdin
            .write_all(line.replace("@ID@", &id.to_string()).as_bytes())
            .unwrap();
        self.stdin.flush().unwrap();
        id
    }

    fn recv(&self) -> String {
        match self.lines.recv_timeout(TIMEOUT) {
            Ok(l) => l,
            Err(e) => panic!("no reply from process {}: {e}", self.conn + 1),
        }
    }

    /// Wait until the process is held before a request starting with `prefix`, or replies.
    fn held_or_reply(&self, fake: &FakeJigd, prefix: &str) -> Option<String> {
        let t = Instant::now();
        loop {
            if let Some(req) = fake.blocked(self.conn) {
                assert!(req.starts_with(prefix.as_bytes()), "{req:?}");
                return None;
            }
            match self.lines.recv_timeout(Duration::from_millis(2)) {
                Ok(l) => return Some(l),
                Err(RecvTimeoutError::Timeout) => {}
                Err(e) => panic!("process {} ended: {e}", self.conn + 1),
            }
            assert!(t.elapsed() < TIMEOUT, "process {} is stuck", self.conn + 1);
        }
    }

    fn disk_path(&self, output_id: &[u8]) -> PathBuf {
        self.dir.join(hex(output_id))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct LastHit {
    valid: bool,
    act: String,
    out: i64,
    body: i64,
}

fn big(v: &Value) -> i64 {
    v["#bigint"].as_str().unwrap().parse().unwrap()
}

fn pairs(v: &Value) -> impl Iterator<Item = (&Value, &Value)> {
    v["#map"]
        .as_array()
        .unwrap()
        .iter()
        .map(|kv| (&kv[0], &kv[1]))
}

fn pick<'a>(s: &'a Value, name: &str) -> &'a Value {
    let p = &s["mbt::nondetPicks"][name];
    assert_eq!(p["tag"], "Some", "{name}");
    &p["value"]
}

/// What the model's state says, in the replay's terms.
#[derive(Debug, PartialEq, Eq)]
struct View {
    kv: BTreeMap<String, i64>,
    objs: BTreeMap<i64, i64>,
    procs: Vec<PState>,
    last_hit: LastHit,
    last_err: bool,
}

fn model_view(s: &Value) -> View {
    let procs = pairs(&s["proc"])
        .map(|(p, st)| {
            let p = big(p);
            let st = match st["tag"].as_str().unwrap() {
                "Idle" => PState::Idle,
                "PutBodySent" => PState::PutBodySent {
                    act: st["value"]["act"].as_str().unwrap().to_owned(),
                    out: big(&st["value"]["out"]),
                },
                "GetAwaitBody" => PState::GetAwaitBody {
                    act: st["value"]["act"].as_str().unwrap().to_owned(),
                    out: big(&st["value"]["out"]),
                },
                t => panic!("state {t}"),
            };
            (p, st)
        })
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .collect();
    let h = &s["lastHit"];
    View {
        kv: pairs(&s["kv"])
            .map(|(a, o)| (a.as_str().unwrap().to_owned(), big(o)))
            .collect(),
        objs: pairs(&s["objs"]).map(|(o, b)| (big(o), big(b))).collect(),
        procs,
        last_hit: LastHit {
            valid: h["valid"].as_bool().unwrap(),
            act: h["act"].as_str().unwrap().to_owned(),
            out: big(&h["out"]),
            body: big(&h["body"]),
        },
        last_err: s["lastErr"].as_bool().unwrap(),
    }
}

struct Replay<'w> {
    world: &'w World,
    fake: FakeJigd,
    procs: Vec<Proc>,
    last_hit: LastHit,
    last_err: bool,
    /// OutputIDs a put named per action (the model's ghost `announced`).
    announced: BTreeMap<String, BTreeSet<Vec<u8>>>,
    _tmp: tempfile::TempDir,
}

impl<'w> Replay<'w> {
    fn new(world: &'w World) -> Self {
        let fake = FakeJigd::start();
        let tmp = tempfile::tempdir().unwrap();
        let procs = (0..2)
            .map(|c| Proc::start(&fake, c, tmp.path().join(format!("dp{}", c + 1))))
            .collect();
        Self {
            world,
            fake,
            procs,
            last_hit: LastHit::default(),
            last_err: false,
            announced: BTreeMap::new(),
            _tmp: tmp,
        }
    }

    fn step(&mut self, s: &Value) {
        let act = s["mbt::actionTaken"].as_str().unwrap();
        let p = || usize::try_from(big(pick(s, "p"))).unwrap() - 1;
        let a = || pick(s, "a").as_str().unwrap().to_owned();
        let b = || big(pick(s, "b"));
        match act {
            "putStart" => {
                let (i, a, b) = (p(), a(), b());
                let torn = pick(s, "torn").as_bool().unwrap();
                let data = body(b);
                let output = sha(&data);
                let size = data.len() + usize::from(torn);
                let line = format!(
                    "{{\"ID\":@ID@,\"Command\":\"put\",\"ActionID\":\"{}\",\"OutputID\":\"{}\",\"BodySize\":{size}}}\n\n\"{}\"\n",
                    b64(&action_id(&a)),
                    b64(&output),
                    b64(&data)
                );
                let pr = &mut self.procs[i];
                assert_eq!(pr.state, PState::Idle);
                let id = pr.send(&line);
                if torn {
                    // refused before anything is sent to jigd
                    assert_eq!(
                        pr.recv(),
                        format!(
                            "{{\"ID\":{id},\"Err\":\"body: got {} of {size} bytes\"}}",
                            data.len()
                        )
                    );
                    self.last_err = true;
                } else {
                    // the body is stored; the pointer PUT waits
                    assert_eq!(pr.held_or_reply(&self.fake, "PUT go/a/"), None);
                    pr.state = PState::PutBodySent {
                        act: a.clone(),
                        out: self.world.oid_of(&output),
                    };
                    self.announced.entry(a).or_default().insert(output);
                    self.last_err = false;
                }
            }
            "putFinish" => {
                let pr = &mut self.procs[p()];
                let PState::PutBodySent { out, .. } = pr.state.clone() else {
                    panic!("putFinish of {:?}", pr.state)
                };
                self.fake.release(pr.conn);
                let reply = pr.recv();
                pr.hold(&self.fake);
                let output = World::output_id(out);
                let dp = pr.disk_path(&output);
                let id = pr.next_id - 1;
                assert_eq!(
                    reply,
                    format!("{{\"ID\":{id},\"DiskPath\":\"{}\"}}", dp.display())
                );
                assert_eq!(std::fs::read(&dp).unwrap(), body(out - 100));
                pr.state = PState::Idle;
            }
            "getStart" => {
                let a = a();
                let pr = &mut self.procs[p()];
                assert_eq!(pr.state, PState::Idle);
                let id = pr.send(&format!(
                    "{{\"ID\":@ID@,\"Command\":\"get\",\"ActionID\":\"{}\"}}\n\n",
                    b64(&action_id(&a))
                ));
                match pr.held_or_reply(&self.fake, "GET go/o/") {
                    None => {
                        let req = self.fake.blocked(pr.conn).unwrap();
                        let output = &req[b"GET go/o/".len()..];
                        let out = self
                            .world
                            .outputs
                            .iter()
                            .find(|(o, _)| hex(o).as_bytes() == output)
                            .map(|(o, _)| self.world.oid_of(o))
                            .unwrap();
                        pr.state = PState::GetAwaitBody { act: a, out };
                    }
                    Some(reply) => {
                        assert_eq!(reply, format!("{{\"ID\":{id},\"Miss\":true}}"));
                        self.last_hit.valid = false;
                    }
                }
            }
            "getFinish" => {
                let pr = &mut self.procs[p()];
                let PState::GetAwaitBody { act, out } = pr.state.clone() else {
                    panic!("getFinish of {:?}", pr.state)
                };
                self.fake.release(pr.conn);
                let reply = pr.recv();
                pr.hold(&self.fake);
                pr.state = PState::Idle;
                let id = pr.next_id - 1;
                let v: Value = serde_json::from_str(&reply).unwrap();
                assert_eq!(v["ID"], id);
                if v.get("Miss").is_some() {
                    assert_eq!(reply, format!("{{\"ID\":{id},\"Miss\":true}}"));
                    self.last_hit.valid = false;
                } else {
                    let output = World::output_id(out);
                    let data = body(out - 100);
                    let dp = pr.disk_path(&output);
                    assert_eq!(
                        reply,
                        format!(
                            "{{\"ID\":{id},\"OutputID\":\"{}\",\"Size\":{},\"DiskPath\":\"{}\"}}",
                            b64(&output),
                            data.len(),
                            dp.display()
                        )
                    );
                    let got = std::fs::read(&dp).unwrap();
                    let b = self.world.outputs[&sha(&got)];
                    self.last_hit = LastHit {
                        valid: true,
                        act,
                        out,
                        body: b,
                    };
                }
            }
            "evictAction" => {
                let key = format!("go/a/{}", hex(&action_id(&a())));
                self.fake.evict(key.as_bytes());
            }
            "evictObject" => {
                let key = format!("go/o/{}", hex(&sha(&body(b()))));
                self.fake.evict(key.as_bytes());
            }
            other => panic!("action {other}"),
        }
    }

    /// The daemon contents and the replay state, in the model's terms; checks the invariants
    /// on the real side.
    fn view(&self) -> View {
        let mut kv = BTreeMap::new();
        let mut objs = BTreeMap::new();
        for (k, v) in self.fake.kv() {
            let k = String::from_utf8(k).unwrap();
            if let Some(a) = k.strip_prefix("go/a/") {
                let act = self
                    .world
                    .actions
                    .iter()
                    .find(|(id, _)| hex(id) == a)
                    .map(|(_, n)| n.clone())
                    .unwrap();
                // pointersAnnounced
                assert!(self.announced[&act].contains(&v), "{act} → {}", hex(&v));
                kv.insert(act, self.world.oid_of(&v));
            } else {
                let o = k.strip_prefix("go/o/").unwrap();
                // noTornBodies: the body hashes to its key
                assert_eq!(hex(&sha(&v)), o);
                objs.insert(self.world.oid_of(&sha(&v)), self.world.outputs[&sha(&v)]);
            }
        }
        // hitIsConsistent
        if self.last_hit.valid {
            let output = World::output_id(self.last_hit.out);
            assert!(self.announced[&self.last_hit.act].contains(&output));
            assert_eq!(self.last_hit.out, 100 + self.last_hit.body);
        }
        View {
            kv,
            objs,
            procs: self.procs.iter().map(|p| p.state.clone()).collect(),
            last_hit: self.last_hit.clone(),
            last_err: self.last_err,
        }
    }

    fn finish(mut self) {
        for pr in &mut self.procs {
            self.fake.release(pr.conn);
            if pr.state != PState::Idle {
                // the reply to the request left waiting
                pr.recv();
            }
            let id = pr.send("{\"ID\":@ID@,\"Command\":\"close\"}\n\n");
            assert_eq!(pr.recv(), format!("{{\"ID\":{id}}}"));
            assert_eq!(pr.child.wait().unwrap().code(), Some(0));
        }
    }
}

/// `lastHit` compares fully only while valid (the model keeps stale fields otherwise).
fn comparable(mut v: View) -> View {
    if !v.last_hit.valid {
        v.last_hit = LastHit::default();
    }
    v
}

/// Replay one trace; the set of actions it took.
fn replay(world: &World, path: &Path) -> BTreeSet<String> {
    let trace: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let states = trace["states"].as_array().unwrap();
    assert_eq!(states[0]["mbt::actionTaken"], "init");
    let mut r = Replay::new(world);
    let mut seen = BTreeSet::new();
    for (n, s) in states.iter().enumerate().skip(1) {
        let act = s["mbt::actionTaken"].as_str().unwrap().to_owned();
        r.step(s);
        let model = model_view(s);
        assert_eq!(
            comparable(r.view()),
            comparable(model_view(s)),
            "{} step {n} ({act})",
            path.display()
        );
        if act == "getFinish" {
            seen.insert(format!("getFinish {}", model.last_hit.valid));
        }
        if act == "putStart" && model.last_err {
            seen.insert("torn".to_owned());
        }
        seen.insert(act);
    }
    r.finish();
    seen
}

fn traces(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().ends_with(".itf.json"))
        .collect();
    v.sort();
    v
}

#[test]
fn quint_traces() {
    let world = World::new();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gocache/itf");
    let mut seen = BTreeSet::new();
    for t in traces(&dir) {
        seen.extend(replay(&world, &t));
    }
    for want in [
        "putStart",
        "putFinish",
        "getStart",
        "getFinish true",
        "getFinish false",
        "torn",
        "evictAction",
        "evictObject",
    ] {
        assert!(seen.contains(want), "no {want} in the traces: {seen:?}");
    }
    if let Some(more) = std::env::var_os("JIG_GOCACHE_ITF_DIR") {
        for t in traces(Path::new(&more)) {
            replay(&world, &t);
        }
    }
}
