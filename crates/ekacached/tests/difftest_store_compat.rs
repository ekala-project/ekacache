//! On-disk compatibility with the Go daemon's store (implementation/04 §Tests: "opens a
//! Go-written store"; `jigd-store.md` §Pack file format, §Hint file format, §Startup).
//!
//! Needs the Go jigd binary: `JIG_LEGACY_JIGD=/path/to/jigd`. Without it each test passes and
//! says it skipped.
//!
//! - Go writes, Rust reads: the Go daemon fills a store over several runs (clean stops, a run
//!   with no PUTs that leaves an empty pack, a `kill -9` that leaves an unsealed pack, then a torn
//!   record appended to it), and its oldest pack is moved to a second, cold tier as a demotion
//!   would. Two copies of the directories are opened, one by the Go daemon and one by the Rust
//!   store. Both must serve every acknowledged value, report the same `keys`, `bytes` and
//!   `live`, and leave the same pack sizes and hint entries (as sets: Go writes hints in map
//!   order, Rust by offset). Adopted Q-STORE-2: Rust drops the empty pack, so its `packs` is
//!   one less.
//! - Rust writes, Go reads: the Rust store fills small packs over two tiers (sorted hints, a
//!   spooled value, demotion), crashes with an unsealed active pack, and gets a torn record
//!   appended. The Go daemon and a reopened Rust store must agree in the same way.

// helpers outside #[test] fns: a panic is the failure report
#![allow(clippy::unwrap_used)]
// pack and hint names are compared byte for byte, as the daemons do
#![allow(clippy::case_sensitive_file_extension_comparisons)]

use ekacache_testkit::busy::CommandBusyExt as _;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ekacached::log::Logger;
use ekacached::store::{HINT_HEADER, Stats, Store, StoreConfig, Tier};
use rustix::process::{Pid, Signal};

fn legacy_jigd() -> Option<PathBuf> {
    match std::env::var_os("JIG_LEGACY_JIGD") {
        Some(p) if !p.is_empty() => Some(PathBuf::from(p)),
        _ => {
            eprintln!("note: JIG_LEGACY_JIGD is not set; skipping the Go store compatibility test");
            None
        }
    }
}

/// A running Go daemon on a socket in `root`, store in `root/cache/jigd/packs` (+ `cold`).
struct GoJigd {
    child: Child,
    sock: PathBuf,
}

impl GoJigd {
    fn start(bin: &Path, root: &Path, cold: Option<&Path>) -> GoJigd {
        let sock = root.join("sock");
        let mut cmd = Command::new(bin);
        cmd.arg(&sock)
            .env_clear()
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("JIGD_SLOTS", "2")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if let Some(c) = cold {
            cmd.env("JIGD_COLD", c);
        }
        let child = cmd.spawn_busy().expect("start Go jigd");
        let d = GoJigd { child, sock };
        let deadline = Instant::now() + Duration::from_secs(20);
        while UnixStream::connect(&d.sock).is_err() {
            assert!(Instant::now() < deadline, "Go jigd did not start listening");
            std::thread::sleep(Duration::from_millis(10));
        }
        d
    }

    fn conn(&self) -> Conn {
        let s = UnixStream::connect(&self.sock).expect("connect");
        Conn {
            r: BufReader::new(s.try_clone().expect("clone")),
            w: s,
        }
    }

    /// Send `sig`, wait for the exit, return what the daemon logged. SIGTERM must exit 0.
    fn signal(&mut self, sig: Signal) -> String {
        let pid = Pid::from_child(&self.child);
        rustix::process::kill_process(pid, sig).expect("kill");
        let status = self.child.wait().expect("wait");
        let mut err = String::new();
        if let Some(mut e) = self.child.stderr.take() {
            let _ = e.read_to_string(&mut err);
        }
        if sig == Signal::TERM {
            assert!(status.success(), "Go jigd exit {status:?}: {err}");
        }
        err
    }
}

struct Conn {
    r: BufReader<UnixStream>,
    w: UnixStream,
}

impl Conn {
    fn line(&mut self) -> String {
        let mut l = String::new();
        self.r.read_line(&mut l).expect("read reply");
        l
    }

    fn put(&mut self, key: &[u8], val: &[u8]) {
        let mut req = b"PUT ".to_vec();
        req.extend_from_slice(key);
        req.extend_from_slice(format!(" {}\n", val.len()).as_bytes());
        self.w.write_all(&req).unwrap();
        self.w.write_all(val).unwrap();
        assert_eq!(self.line(), "OK\n", "PUT {key:?}");
    }

    fn get(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        let mut req = b"GET ".to_vec();
        req.extend_from_slice(key);
        req.push(b'\n');
        self.w.write_all(&req).unwrap();
        let l = self.line();
        if l == "MISS\n" {
            return None;
        }
        let n: usize = l
            .strip_prefix("OK ")
            .and_then(|s| s.trim_end().parse().ok())
            .unwrap_or_else(|| panic!("GET reply {l:?}"));
        let mut v = vec![0u8; n];
        self.r.read_exact(&mut v).unwrap();
        Some(v)
    }

    /// `keys`, `packs`, `bytes`, `live` from STATS.
    fn stats(&mut self) -> Stats {
        self.w.write_all(b"STATS\n").unwrap();
        let l = self.line();
        let field = |name: &str| -> i64 {
            l.split_whitespace()
                .find_map(|f| f.strip_prefix(name).and_then(|v| v.strip_prefix('=')))
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| panic!("STATS {l:?} has no {name}"))
        };
        Stats {
            keys: field("keys") as usize,
            packs: field("packs") as usize,
            bytes: field("bytes"),
            live: field("live"),
        }
    }
}

fn val(seed: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (seed.wrapping_mul(131).wrapping_add(i.wrapping_mul(7)) % 251) as u8)
        .collect()
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let ty = e.file_type().unwrap();
        if ty.is_dir() {
            copy_dir(&e.path(), &to.join(e.file_name()));
        } else if ty.is_file() {
            std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }
}

fn bytes_path(p: &Path) -> Vec<u8> {
    p.as_os_str().as_encoded_bytes().to_vec()
}

fn open_rust(tiers: &[&Path]) -> Store {
    let mut cfg = StoreConfig::new(
        tiers
            .iter()
            .map(|d| Tier {
                dir: bytes_path(d),
                budget: 0,
            })
            .collect(),
    );
    cfg.logger = Logger::capture().0;
    Store::open(cfg).expect("open the store")
}

/// name -> (size, hint entries `(key, vlen, record offset)` sorted)
type Listing = BTreeMap<String, (u64, Vec<(Vec<u8>, u32, u64)>)>;

/// Every pack and hint in a directory.
fn listing(dir: &Path) -> Listing {
    let mut out = BTreeMap::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let name = e.file_name().to_string_lossy().into_owned();
        let b = std::fs::read(e.path()).unwrap();
        let mut entries = Vec::new();
        if name.ends_with(".hint") {
            let mut at = 0;
            while at < b.len() {
                let klen = u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as usize;
                let vlen = u32::from_le_bytes(b[at + 4..at + 8].try_into().unwrap());
                let off = u64::from_le_bytes(b[at + 8..at + 16].try_into().unwrap());
                entries.push((
                    b[at + HINT_HEADER..at + HINT_HEADER + klen].to_vec(),
                    vlen,
                    off,
                ));
                at += HINT_HEADER + klen;
            }
            entries.sort();
        }
        out.insert(name, (b.len() as u64, entries));
    }
    out
}

fn rust_get(s: &Store, key: &[u8]) -> Option<Vec<u8>> {
    let mut r = s.get(key)?;
    let mut v = Vec::new();
    r.read_to_end(&mut v).unwrap();
    Some(v)
}

/// Both readers serve `want` and agree on the stats and the files they leave.
fn compare(
    go: &mut GoJigd,
    rust: &Store,
    want: &BTreeMap<Vec<u8>, Vec<u8>>,
    empty_packs: usize,
    dirs: [(&Path, &Path); 2],
) {
    let mut c = go.conn();
    let gs = c.stats();
    let rs = rust.stats();
    assert_eq!(
        (rs.keys, rs.bytes, rs.live),
        (gs.keys, gs.bytes, gs.live),
        "stats: rust {rs}, go {gs}"
    );
    assert_eq!(
        rs.packs + empty_packs,
        gs.packs,
        "packs: rust {rs}, go {gs}"
    );
    assert_eq!(rs.keys, want.len());
    for (k, v) in want {
        assert_eq!(c.get(k).as_ref(), Some(v), "Go GET {k:?}");
        assert_eq!(rust_get(rust, k).as_ref(), Some(v), "Rust GET {k:?}");
    }
    assert_eq!(c.get(b"never/put"), None);
    for (go_dir, rust_dir) in dirs {
        let (mut g, r) = (listing(go_dir), listing(rust_dir));
        // adopted Q-STORE-2: the empty packs Go keeps (and their empty hints) are gone in Rust
        g.retain(|name, (size, _)| {
            !(*size == 0 && name.ends_with(".pack") && !r.contains_key(name))
        });
        g.retain(|name, (size, _)| {
            !(*size == 0 && name.ends_with(".hint") && !r.contains_key(name))
        });
        assert_eq!(
            g,
            r,
            "files in {} vs {}",
            go_dir.display(),
            rust_dir.display()
        );
    }
}

#[test]
fn go_written_store_opens_in_rust() {
    let Some(bin) = legacy_jigd() else { return };
    let root = tempfile::tempdir().unwrap();
    let hot = root.path().join("cache/jigd/packs");
    let mut want: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut put = |c: &mut Conn, k: &[u8], v: Vec<u8>| {
        c.put(k, &v);
        want.insert(k.to_vec(), v);
    };
    // run 1: pack 1, sealed by SIGTERM
    let mut d = GoJigd::start(&bin, root.path(), None);
    let mut c = d.conn();
    for i in 0..40 {
        put(&mut c, format!("o/1/{i}").as_bytes(), val(i, 100 + i * 13));
    }
    put(&mut c, b"m/k", b"old".to_vec());
    drop(c);
    d.signal(Signal::TERM);
    // run 2: pack 2, with an overwrite of a pack-1 key, a spooled value, an empty value and a
    // key that is not UTF-8
    let mut d = GoJigd::start(&bin, root.path(), None);
    let mut c = d.conn();
    put(&mut c, b"m/k", b"new".to_vec());
    put(&mut c, b"big", val(7, (4 << 20) + 321));
    put(&mut c, b"empty", Vec::new());
    put(&mut c, b"k/\xff\xfe", b"bytes".to_vec());
    for i in 0..20 {
        put(&mut c, format!("o/2/{i}").as_bytes(), val(1000 + i, 50));
    }
    drop(c);
    d.signal(Signal::TERM);
    // run 3: no PUTs, leaves an empty pack 3 and its empty hint
    let mut d = GoJigd::start(&bin, root.path(), None);
    d.signal(Signal::TERM);
    // run 4: pack 4, never sealed (kill -9), overwrites inside the unsealed pack
    let mut d = GoJigd::start(&bin, root.path(), None);
    let mut c = d.conn();
    for i in 0..30 {
        put(
            &mut c,
            format!("o/4/{}", i % 10).as_bytes(),
            val(2000 + i, 30 + i),
        );
    }
    put(&mut c, b"o/1/0", b"replaced in pack 4".to_vec());
    drop(c);
    d.signal(Signal::KILL);
    assert!(
        hot.join("000003.pack").exists()
            && std::fs::metadata(hot.join("000003.pack")).unwrap().len() == 0
    );
    assert!(
        !hot.join("000004.hint").exists(),
        "pack 4 should be unsealed"
    );
    // a torn record at the end of the unsealed pack
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(hot.join("000004.pack"))
        .unwrap();
    f.write_all(&[5, 0, 0, 0, 9, 0, 0, 0, b'o', b'/']).unwrap();
    drop(f);
    // pack 1 moved to a cold tier, as the demoter would have
    let cold = root.path().join("cold");
    std::fs::create_dir(&cold).unwrap();
    for ext in ["pack", "hint"] {
        std::fs::rename(
            hot.join(format!("000001.{ext}")),
            cold.join(format!("000001.{ext}")),
        )
        .unwrap();
    }
    // two copies: one for each reader
    let (go_root, rust_root) = (root.path().join("go"), root.path().join("rust"));
    for r in [&go_root, &rust_root] {
        copy_dir(&hot, &r.join("cache/jigd/packs"));
        copy_dir(&cold, &r.join("cold"));
    }
    let mut go = GoJigd::start(&bin, &go_root, Some(&go_root.join("cold")));
    let rust = open_rust(&[&rust_root.join("cache/jigd/packs"), &rust_root.join("cold")]);
    compare(
        &mut go,
        &rust,
        &want,
        1,
        [
            (
                &go_root.join("cache/jigd/packs"),
                &rust_root.join("cache/jigd/packs"),
            ),
            (&go_root.join("cold"), &rust_root.join("cold")),
        ],
    );
    let log = go.signal(Signal::TERM);
    assert!(
        log.contains("pack 000004: truncating torn tail at"),
        "{log}"
    );
}

#[test]
fn rust_written_store_opens_in_go() {
    let Some(bin) = legacy_jigd() else { return };
    let root = tempfile::tempdir().unwrap();
    let hot = root.path().join("cache/jigd/packs");
    let cold = root.path().join("cold");
    let mut want: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    {
        let mut cfg = StoreConfig::new(vec![
            Tier {
                dir: bytes_path(&hot),
                budget: 3 * 4096,
            },
            Tier {
                dir: bytes_path(&cold),
                budget: 0,
            },
        ]);
        cfg.pack_limit = 4096;
        cfg.logger = Logger::capture().0;
        let s = Store::open(cfg).unwrap();
        let mut put = |k: &[u8], v: Vec<u8>| {
            s.put_reader(k, v.len() as i64, &mut &v[..]).unwrap();
            want.insert(k.to_vec(), v);
        };
        for i in 0..150 {
            put(format!("o/{}", i % 110).as_bytes(), val(i, 100 + i % 200));
        }
        put(b"big", val(3, (4 << 20) + 99));
        put(b"empty", Vec::new());
        put(b"k/\xff", b"bytes".to_vec());
        for i in 0..5 {
            put(format!("tail/{i}").as_bytes(), val(500 + i, 40));
        }
        s.wait();
        let active = s.snapshot().active;
        assert!(
            s.snapshot().packs.values().any(|p| p.tier == 1),
            "nothing was demoted"
        );
        // crash: the active pack is left unsealed, then a record is torn
        drop(s);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(hot.join(format!("{active:06}.pack")))
            .unwrap();
        f.write_all(&[3, 0, 0, 0]).unwrap();
    }
    let (go_root, rust_root) = (root.path().join("go"), root.path().join("rust"));
    for r in [&go_root, &rust_root] {
        copy_dir(&hot, &r.join("cache/jigd/packs"));
        copy_dir(&cold, &r.join("cold"));
    }
    let mut go = GoJigd::start(&bin, &go_root, Some(&go_root.join("cold")));
    let rust = open_rust(&[&rust_root.join("cache/jigd/packs"), &rust_root.join("cold")]);
    compare(
        &mut go,
        &rust,
        &want,
        0,
        [
            (
                &go_root.join("cache/jigd/packs"),
                &rust_root.join("cache/jigd/packs"),
            ),
            (&go_root.join("cold"), &rust_root.join("cold")),
        ],
    );
    let log = go.signal(Signal::TERM);
    assert!(log.contains("truncating torn tail at"), "{log}");
}
