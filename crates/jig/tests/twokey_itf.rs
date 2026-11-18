//! Replay of `spec/twokey.qnt` ITF traces against jig's two-key engine (`jig::twokey`,
//! `jig_core::manifest`; `behaviors/jig-keys.md` §Manifest; implementation/12 §L4).
//!
//! Traces are generated locally and not committed (`/tests/itf/*/` is ignored):
//!
//! ```text
//! quint run spec/twokey.qnt --mbt --out-itf='tests/itf/twokey/t{seq}.itf.json' \
//!     --n-traces=300 --max-steps=30 --max-samples=300 --seed=1 --invariant=sound
//! ```
//!
//! With no traces the test passes and says so.
//!
//! The model has two builds with their own file systems but the same cwd and primary source, so
//! the same k1. The driver keeps one real directory (the "build tree") and, before every jig
//! operation of build `b`, makes its files equal to the model's `fs[b]`: content `v` is a file
//! holding `v\n`, content 0 is no file. The compiler is a fake that reads that directory and
//! follows the model's `compile` (search `x.h` in `a/` then `b/`, then include `y.h`; `y.h = 3`
//! is a deterministic failure, a missing header is a `MissingInput` failure). Everything between
//! is the real jig code:
//!
//!  - `invoke`: `twokey::find_result` sends `GET m/<k1>` to a scripted daemon (a socket pair
//!    whose peer is preloaded with `MISS` or the stored, framed manifest) and validates the
//!    manifest against the tree; a hit additionally needs the daemon to hold the artifact under
//!    the k2 it returns, as in cc mode's `o/<k2>` lookup;
//!  - `finish`: `twokey::build_manifest` over the compiler's reads and missed lookups, stored
//!    unless the failure is a `MissingInput` one (CompileAndStore).
//!
//! After every step the daemon's manifest (as text, against the text the model's lines must
//! format to), its artifacts (by real k2), each build's pending compile and its last result
//! must equal the model's. The model's invariants are then checked on the real state: `sound`
//! (whatever jig handed back equals a fresh compile) and `entriesSound` (every stored artifact
//! whose manifest validates on a build's tree is what that tree compiles to, with validation
//! done by `jig_core::manifest::validate`). The witnesses `witnessReplay` and
//! `witnessCachedFailure` must each be reached by some trace.
//!
//! Differences in form only: the daemon keeps, next to each artifact, the manifest text it was
//! stored with (the model keys artifacts by the manifest itself); `entriesSound` needs it.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use jig::client::Client;
use jig::twokey;
use jig_core::env::Env;
use jig_core::hasher::hash_of;
use jig_core::keys::{RequestKey, ResultKey, Tool};
use jig_core::manifest;
use jig_core::store_path::Store;
use jig_testkit::itf::{self, State, Trace, Value};

type Res<T> = Result<T, String>;

const PATHS: [&str; 3] = ["a/x.h", "b/x.h", "y.h"];
const BUILDS: [i64; 2] = [1, 2];

fn e(x: itf::ItfError) -> String {
    x.0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Outcome {
    status: i64,
    out: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Compile {
    res: Outcome,
    read: Vec<String>,
    missed: Vec<String>,
    missing_input: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Last {
    valid: bool,
    got: Outcome,
    expected: Outcome,
    replayed: bool,
}

const NO_LAST: Last = Last {
    valid: false,
    got: Outcome { status: 0, out: 0 },
    expected: Outcome { status: 0, out: 0 },
    replayed: false,
};

type Fs = BTreeMap<String, i64>;
/// A manifest line as the model has it: `(path, id)`, id -1 = `!path`.
type Line = (String, i64);

/// The model's state, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Model {
    fs: BTreeMap<i64, Fs>,
    manifest: Option<Vec<Line>>,
    artifacts: Vec<(Vec<Line>, Outcome)>,
    busy: BTreeMap<i64, Option<Compile>>,
    last: BTreeMap<i64, Last>,
}

fn int(v: &Value, f: &str) -> Res<i64> {
    v.field(f).map_err(e)?.int().map_err(e)
}

fn outcome(v: &Value) -> Res<Outcome> {
    Ok(Outcome {
        status: int(v, "status")?,
        out: int(v, "out")?,
    })
}

fn strs(v: &Value) -> Res<Vec<String>> {
    v.elems()
        .map_err(e)?
        .iter()
        .map(|s| Ok(s.str().map_err(e)?.to_owned()))
        .collect()
}

fn lines(v: &Value) -> Res<Vec<Line>> {
    v.elems()
        .map_err(e)?
        .iter()
        .map(|l| {
            Ok((
                l.field("path").map_err(e)?.str().map_err(e)?.to_owned(),
                int(l, "id")?,
            ))
        })
        .collect()
}

fn fs_of(v: &Value) -> Res<Fs> {
    v.map()
        .map_err(e)?
        .iter()
        .map(|(p, c)| Ok((p.str().map_err(e)?.to_owned(), c.int().map_err(e)?)))
        .collect()
}

fn per_build<T>(st: &State, var: &str, f: impl Fn(&Value) -> Res<T>) -> Res<BTreeMap<i64, T>> {
    st.var(var)
        .map_err(e)?
        .map()
        .map_err(e)?
        .iter()
        .map(|(b, v)| Ok((b.int().map_err(e)?, f(v)?)))
        .collect()
}

impl Model {
    fn parse(st: &State) -> Res<Model> {
        let has = st.var("hasManifest").map_err(e)?.bool().map_err(e)?;
        let m = lines(st.var("manifest").map_err(e)?)?;
        let artifacts = st
            .var("artifacts")
            .map_err(e)?
            .map()
            .map_err(e)?
            .iter()
            .map(|(m, r)| Ok((lines(m)?, outcome(r)?)))
            .collect::<Res<Vec<_>>>()?;
        let busy = per_build(st, "pending", |p| {
            if !p.field("busy").map_err(e)?.bool().map_err(e)? {
                return Ok(None);
            }
            let c = p.field("c").map_err(e)?;
            Ok(Some(Compile {
                res: outcome(c.field("res").map_err(e)?)?,
                read: strs(c.field("read").map_err(e)?)?,
                missed: strs(c.field("missed").map_err(e)?)?,
                missing_input: c.field("missingInput").map_err(e)?.bool().map_err(e)?,
            }))
        })?;
        let last = per_build(st, "last", |l| {
            Ok(Last {
                valid: l.field("valid").map_err(e)?.bool().map_err(e)?,
                got: outcome(l.field("got").map_err(e)?)?,
                expected: outcome(l.field("expected").map_err(e)?)?,
                replayed: l.field("replayed").map_err(e)?.bool().map_err(e)?,
            })
        })?;
        Ok(Model {
            fs: per_build(st, "fs", fs_of)?,
            manifest: has.then_some(m),
            artifacts,
            busy,
            last,
        })
    }
}

/// The build tree and the fake compiler.
struct Tree {
    root: PathBuf,
}

impl Tree {
    fn path(&self, p: &str) -> PathBuf {
        self.root.join(p)
    }

    fn bytes(&self, p: &str) -> Vec<u8> {
        self.path(p).as_os_str().as_encoded_bytes().to_vec()
    }

    /// Make the directory hold `fs`.
    fn materialize(&self, fs: &Fs) -> Res<()> {
        for p in PATHS {
            let c = fs.get(p).copied().ok_or(format!("fs has no {p}"))?;
            let path = self.path(p);
            if c == 0 {
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(x) if x.kind() == std::io::ErrorKind::NotFound => {}
                    Err(x) => return Err(format!("rm {}: {x}", path.display())),
                }
            } else {
                std::fs::write(&path, format!("{c}\n"))
                    .map_err(|x| format!("write {}: {x}", path.display()))?;
            }
        }
        Ok(())
    }

    /// What the file holds now, 0 when it does not exist.
    fn content(&self, p: &str) -> Res<i64> {
        match std::fs::read_to_string(self.path(p)) {
            Ok(s) => s.trim().parse().map_err(|x| format!("{p}: {x}")),
            Err(x) if x.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(x) => Err(format!("{p}: {x}")),
        }
    }

    /// The compiler, run on the directory as it is.
    fn compile(&self) -> Res<Compile> {
        let fail = |out, read: &[&str], missed: &[&str], missing_input| Compile {
            res: Outcome { status: 1, out },
            read: read.iter().map(|s| (*s).to_owned()).collect(),
            missed: missed.iter().map(|s| (*s).to_owned()).collect(),
            missing_input,
        };
        let (x_path, skipped): (&str, &[&str]) = if self.content("a/x.h")? != 0 {
            ("a/x.h", &[])
        } else if self.content("b/x.h")? != 0 {
            ("b/x.h", &["a/x.h"])
        } else {
            return Ok(fail(0, &[], &["a/x.h", "b/x.h"], true));
        };
        let x = self.content(x_path)?;
        let y = self.content("y.h")?;
        let mut missed = skipped.to_vec();
        Ok(match y {
            0 => {
                missed.push("y.h");
                fail(0, &[x_path], &missed, true)
            }
            3 => fail(100 + x, &[x_path, "y.h"], &missed, false),
            _ => Compile {
                res: Outcome {
                    status: 0,
                    out: 10 * x + y,
                },
                read: vec![x_path.to_owned(), "y.h".to_owned()],
                missed: missed.iter().map(|s| (*s).to_owned()).collect(),
                missing_input: false,
            },
        })
    }
}

/// The daemon: `m/<k1>`, and the artifacts by k2 (with the manifest text they were stored
/// under, see the header).
#[derive(Default)]
struct Daemon {
    manifest: Option<Vec<u8>>,
    artifacts: HashMap<Vec<u8>, (Outcome, Vec<u8>)>,
}

struct Driver {
    tree: Tree,
    /// files holding content `v`, to compute the id a model line stands for
    ids_dir: PathBuf,
    store: Store,
    k1: RequestKey,
    primary: Vec<u8>,
    daemon: Daemon,
    fs: BTreeMap<i64, Fs>,
    busy: BTreeMap<i64, Option<Compile>>,
    last: BTreeMap<i64, Last>,
}

impl Driver {
    fn new(dir: &Path, init: &Model) -> Res<Driver> {
        let root = dir.join("build/source");
        std::fs::create_dir_all(root.join("a")).map_err(|x| x.to_string())?;
        std::fs::create_dir_all(root.join("b")).map_err(|x| x.to_string())?;
        let ids_dir = dir.join("ids");
        std::fs::create_dir_all(&ids_dir).map_err(|x| x.to_string())?;
        let main = root.join("main.c");
        std::fs::write(&main, "#include \"x.h\"\n").map_err(|x| x.to_string())?;
        Ok(Driver {
            tree: Tree { root },
            ids_dir,
            store: Store::from_env(&Env::default()),
            k1: RequestKey::new(Tool::Cc, &hash_of(b"twokey k1")),
            primary: main.as_os_str().as_encoded_bytes().to_vec(),
            daemon: Daemon::default(),
            fs: init.fs.clone(),
            busy: BUILDS.iter().map(|&b| (b, None)).collect(),
            last: BUILDS.iter().map(|&b| (b, NO_LAST)).collect(),
        })
    }

    /// The manifest text the model's lines stand for.
    fn text_of(&self, m: &[Line]) -> Res<Vec<u8>> {
        let mut text = Vec::new();
        for (p, id) in m {
            let path = self.tree.bytes(p);
            if *id == -1 {
                text.push(b'!');
                text.extend_from_slice(&self.store.key(&path));
            } else {
                let f = self.ids_dir.join(id.to_string());
                std::fs::write(&f, format!("{id}\n")).map_err(|x| x.to_string())?;
                let f = f.as_os_str().as_encoded_bytes().to_vec();
                let cid = self.store.input_id(&f).ok_or("no id")?;
                text.extend_from_slice(&self.store.key(&path));
                text.push(b'\t');
                text.extend_from_slice(&cid);
            }
            text.push(b'\n');
        }
        Ok(text)
    }

    fn k2_of(&self, text: &[u8]) -> ResultKey {
        let lines: Vec<&[u8]> = text
            .split(|&c| c == b'\n')
            .filter(|l| !l.is_empty())
            .collect();
        manifest::result_key_of(self.k1.text(), &lines)
    }

    /// A client whose daemon has `GET m/<k1>`'s reply already queued.
    fn client(&self) -> Res<(Client, UnixStream)> {
        let (s, mut peer) = UnixStream::pair().map_err(|x| x.to_string())?;
        match &self.daemon.manifest {
            None => peer.write_all(b"MISS\n"),
            Some(text) => {
                let wire = jig_core::frame::encode(text).map_err(|x| format!("frame: {x:?}"))?;
                peer.write_all(format!("OK {}\n", wire.len()).as_bytes())
                    .and_then(|()| peer.write_all(&wire))
            }
        }
        .map_err(|x| x.to_string())?;
        Ok((Client::from_fd(OwnedFd::from(s), 1), peer))
    }

    fn fs(&self, b: i64) -> Res<&Fs> {
        self.fs.get(&b).ok_or(format!("no build {b}"))
    }

    fn invoke(&mut self, b: i64) -> Res<()> {
        if self.busy.get(&b).is_some_and(Option::is_some) {
            return Err(format!("build {b} is busy"));
        }
        self.tree.materialize(self.fs(b)?)?;
        let (mut client, mut peer) = self.client()?;
        let found = twokey::find_result(&mut client, &self.store, &self.k1);
        drop(client);
        let mut sent = Vec::new();
        peer.read_to_end(&mut sent).map_err(|x| x.to_string())?;
        let mut want = b"GET m/".to_vec();
        want.extend_from_slice(self.k1.text());
        want.push(b'\n');
        if sent != want {
            return Err(format!(
                "the client sent {:?}",
                String::from_utf8_lossy(&sent)
            ));
        }
        let hit = found
            .ok()
            .and_then(|k2| self.daemon.artifacts.get(k2.text()).map(|(r, _)| *r));
        let c = self.tree.compile()?;
        if let Some(got) = hit {
            // the replayed result, against what compiling now gives (`sound`)
            let last = Last {
                valid: true,
                got,
                expected: c.res,
                replayed: true,
            };
            self.last.insert(b, last);
        } else {
            self.busy.insert(b, Some(c));
        }
        Ok(())
    }

    fn finish(&mut self, b: i64) -> Res<()> {
        let c = self
            .busy
            .insert(b, None)
            .flatten()
            .ok_or(format!("build {b} is not compiling"))?;
        self.tree.materialize(self.fs(b)?)?;
        let read: Vec<Vec<u8>> = c.read.iter().map(|p| self.tree.bytes(p)).collect();
        let missed: Vec<Vec<u8>> = c.missed.iter().map(|p| self.tree.bytes(p)).collect();
        let store = !(c.res.status != 0 && c.missing_input);
        if store {
            let (mut client, _peer) = self.client()?;
            let m = twokey::build_manifest(
                &mut client,
                &self.store,
                &self.k1,
                &read,
                &self.primary,
                &missed,
            )
            .map_err(|u| format!("unstorable: {u:?}"))?;
            if self.k2_of(&m.text) != m.result_key {
                return Err("k2 is not the hash of the manifest lines".into());
            }
            self.daemon
                .artifacts
                .insert(m.result_key.text().to_vec(), (c.res, m.text.clone()));
            self.daemon.manifest = Some(m.text);
        }
        self.last.insert(
            b,
            Last {
                valid: true,
                got: c.res,
                expected: c.res,
                replayed: false,
            },
        );
        Ok(())
    }

    fn step(&mut self, action: &str, st: &State, pre: &Model, post: &Model) -> Res<()> {
        let pick_int = |name: &str| -> Res<i64> {
            st.pick(name)
                .map_err(e)?
                .ok_or(format!("{action}: no {name} picked"))?
                .int()
                .map_err(e)
        };
        match action {
            "edit" => {
                let b = pick_int("b")?;
                let p = st
                    .pick("p")
                    .map_err(e)?
                    .ok_or("no p picked")?
                    .str()
                    .map_err(e)?
                    .to_owned();
                let v = pick_int("v")?;
                self.fs
                    .get_mut(&b)
                    .ok_or(format!("no build {b}"))?
                    .insert(p, v);
            }
            "invoke" => self.invoke(pick_int("b")?)?,
            "finish" => self.finish(pick_int("b")?)?,
            "evictManifest" => self.daemon.manifest = None,
            "evictArtifact" => {
                // the model picks the manifest `m` whose artifact goes; the pick is the one
                // entry of `pre.artifacts` missing from `post.artifacts`
                let gone: Vec<&Vec<Line>> = pre
                    .artifacts
                    .iter()
                    .map(|(m, _)| m)
                    .filter(|m| !post.artifacts.iter().any(|(n, _)| n == *m))
                    .collect();
                let [m] = gone.as_slice() else {
                    return Err(format!("cannot tell the evicted artifact: {gone:?}"));
                };
                let k2 = self.k2_of(&self.text_of(m)?);
                if self.daemon.artifacts.remove(k2.text()).is_none() {
                    return Err("the evicted artifact was not stored".into());
                }
            }
            a => return Err(format!("unknown action {a}")),
        }
        Ok(())
    }

    fn compare(&self, want: &Model) -> Res<()> {
        if self.fs != want.fs {
            return Err(format!("fs: {:?} vs {:?}", self.fs, want.fs));
        }
        let want_manifest = want
            .manifest
            .as_ref()
            .map(|m| self.text_of(m))
            .transpose()?;
        if self.daemon.manifest != want_manifest {
            return Err(format!(
                "m/<k1>: {:?}\nwant {:?}",
                self.daemon.manifest.as_deref().map(String::from_utf8_lossy),
                want_manifest.as_deref().map(String::from_utf8_lossy)
            ));
        }
        let mut want_artifacts = HashMap::new();
        for (m, r) in &want.artifacts {
            let text = self.text_of(m)?;
            want_artifacts.insert(self.k2_of(&text).text().to_vec(), (*r, text));
        }
        if self.daemon.artifacts != want_artifacts {
            return Err(format!(
                "artifacts: {:?}\nwant {:?}",
                self.daemon.artifacts, want_artifacts
            ));
        }
        if self.busy != want.busy {
            return Err(format!("pending: {:?}\nwant {:?}", self.busy, want.busy));
        }
        if self.last != want.last {
            return Err(format!("last: {:?}\nwant {:?}", self.last, want.last));
        }
        Ok(())
    }

    /// `sound` and `entriesSound`, on the real state. Leaves the tree holding build 2's files.
    fn invariants(&self) -> Res<()> {
        for (b, l) in &self.last {
            if l.valid && l.got != l.expected {
                return Err(format!(
                    "sound: build {b} got {:?}, compiling gives {:?}",
                    l.got, l.expected
                ));
            }
        }
        for b in BUILDS {
            self.tree.materialize(self.fs(b)?)?;
            let fresh = self.tree.compile()?.res;
            for (k2, (r, text)) in &self.daemon.artifacts {
                let valid =
                    manifest::validate(&self.store, &self.k1, text, &mut |_: &[&[u8]]| Vec::new());
                if valid.as_ref().map(ResultKey::text) == Ok(k2.as_slice()) && fresh != *r {
                    return Err(format!(
                        "entriesSound: build {b} validates {} but compiles to {fresh:?}, stored {r:?}",
                        String::from_utf8_lossy(text)
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct Seen {
    replay: bool,
    cached_failure: bool,
}

fn replay(trace: &Trace, seen: &mut Seen) -> Res<usize> {
    let Some(first) = trace.states.first() else {
        return Err("empty trace".into());
    };
    if first.action().map_err(e)? != "init" {
        return Err("trace does not start with init".into());
    }
    let dir = tempfile::Builder::new()
        .prefix("twokey-itf")
        .tempdir()
        .map_err(|x| x.to_string())?;
    let mut pre = Model::parse(first)?;
    let mut d = Driver::new(dir.path(), &pre)?;
    d.compare(&pre)
        .map_err(|x| format!("state 0 (init): {x}"))?;
    for (i, st) in trace.states.iter().enumerate().skip(1) {
        let post = Model::parse(st)?;
        let action = st.action().map_err(e)?.to_owned();
        d.step(&action, st, &pre, &post)
            .and_then(|()| d.compare(&post))
            .and_then(|()| d.invariants())
            .map_err(|x| format!("state {i} ({action}): {x}"))?;
        for l in d.last.values() {
            seen.replay |= l.replayed;
            seen.cached_failure |= l.replayed && l.got.status != 0;
        }
        pre = post;
    }
    Ok(trace.states.len())
}

#[test]
fn twokey_traces_replay() {
    let files = itf::trace_files("twokey");
    if files.is_empty() {
        eprintln!(
            "note: no traces under tests/itf/twokey/; generate them with quint (see this file's header). Skipping."
        );
        return;
    }
    let mut actions: BTreeMap<String, usize> = BTreeMap::new();
    let mut states = 0;
    let mut seen = Seen::default();
    for path in &files {
        let trace = Trace::read(path).unwrap_or_else(|x| panic!("{x}"));
        for st in &trace.states {
            *actions
                .entry(st.action().unwrap_or("?").to_owned())
                .or_default() += 1;
        }
        states += replay(&trace, &mut seen).unwrap_or_else(|x| panic!("{}: {x}", path.display()));
    }
    eprintln!(
        "replayed {} traces, {states} states: {actions:?}; witnessReplay reached: {}, witnessCachedFailure reached: {}",
        files.len(),
        seen.replay,
        seen.cached_failure
    );
    // implementation/12 §L4.3: both witnesses are reachable, so some trace must show them
    assert!(seen.replay, "no trace replayed a hit (witnessReplay)");
    assert!(
        seen.cached_failure,
        "no trace replayed a cached failure (witnessCachedFailure)"
    );
}
