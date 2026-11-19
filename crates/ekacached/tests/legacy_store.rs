//! Port of `legacy/jigd/store_test.go` (`jigd-store.md`; implementation/04 §Tests).
//!
//! One Rust test per Go test, same scenario and assertions, snake_case names. Scaled: the Go
//! tests fill 256 MiB packs with 1 MiB values; these use a 256 KiB pack limit and 1 KiB values,
//! which keeps the ratio (`per = pack_limit / len(big)` = 256 values to a pack that holds a few
//! less, so the last values of each run spill into the next pack exactly as in Go). The spool
//! test keeps the real `SPOOL_MIN`. Where an adopted change (`implementation/00-adopted.md`)
//! alters an observable the Go test relied on, the line says `// ADOPTED Q-...`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ekacached::log::{LogCapture, Logger};
use ekacached::store::{REC_HEADER, SPOOL_MIN, Store, StoreConfig, TestClock, Tier};

/// Scaled `packLimit`.
const PACK_LIMIT: i64 = 256 << 10;
/// Scaled `1<<20` value.
const BIG: usize = 1 << 10;
/// `per := packLimit / len(big)`
const PER: usize = PACK_LIMIT as usize / BIG;

fn tier(dir: &Path, budget: i64) -> Tier {
    Tier {
        dir: dir.as_os_str().as_encoded_bytes().to_vec(),
        budget,
    }
}

fn config(tiers: Vec<Tier>) -> (StoreConfig, LogCapture) {
    let (logger, cap) = Logger::capture();
    let mut cfg = StoreConfig::new(tiers);
    cfg.pack_limit = PACK_LIMIT;
    cfg.logger = logger;
    (cfg, cap)
}

fn open(tiers: Vec<Tier>) -> Store {
    Store::open(config(tiers).0).expect("open store")
}

fn putb(s: &Store, key: &str, v: &[u8]) -> Result<(), ekacached::goerr::GoErr> {
    s.put_reader(key.as_bytes(), v.len() as i64, &mut &v[..])
}

fn read(s: &Store, key: &str) -> Option<Vec<u8>> {
    let mut r = s.get(key.as_bytes())?;
    let mut b = Vec::new();
    r.read_to_end(&mut b).expect("read value");
    Some(b)
}

fn big() -> Vec<u8> {
    vec![1u8; BIG]
}

fn path_of(s: &Store, id: u32) -> PathBuf {
    let snap = s.snapshot();
    PathBuf::from(String::from_utf8(snap.packs[&id].path.clone()).expect("utf-8 temp path"))
}

fn active_path(s: &Store) -> PathBuf {
    path_of(s, s.snapshot().active)
}

fn now_ns() -> i64 {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after 1970");
    i64::try_from(d.as_nanos()).expect("time fits i64")
}

const HOUR_NS: i64 = 3_600_000_000_000;

#[test]
fn put_get_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(vec![tier(dir.path(), 0)]);
    putb(&s, "o/aa", b"one").unwrap();
    putb(&s, "o/bb", b"two").unwrap();
    putb(&s, "o/aa", b"three").unwrap(); // overwrite: newest wins
    assert_eq!(read(&s, "o/aa").as_deref(), Some(&b"three"[..]));
    assert!(read(&s, "o/zz").is_none(), "expected miss");
    // crash without Close: next open scans the unsealed pack
    let s2 = open(vec![tier(dir.path(), 0)]);
    assert_eq!(
        read(&s2, "o/aa").as_deref(),
        Some(&b"three"[..]),
        "after scan"
    );
    s2.close().unwrap();
    // and after a clean close it loads from hints
    let s3 = open(vec![tier(dir.path(), 0)]);
    assert_eq!(
        read(&s3, "o/bb").as_deref(),
        Some(&b"two"[..]),
        "after hints"
    );
    drop(s);
}

// values past spoolMin go through a temp file. A body shorter than announced leaves no record
#[test]
fn put_spooled() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(vec![tier(dir.path(), 0)]);
    let big = vec![7u8; SPOOL_MIN as usize + 123];
    putb(&s, "big", &big).unwrap();
    assert!(
        s.put_reader(b"short", big.len() as i64, &mut &big[..100])
            .is_err(),
        "short body accepted"
    );
    putb(&s, "after", b"ok").unwrap();
    assert_eq!(read(&s, "big").map(|b| b == big), Some(true), "big");
    assert!(
        read(&s, "short").is_none() && read(&s, "after").as_deref() == Some(&b"ok"[..]),
        "torn put corrupted the pack"
    );
    let left: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().as_encoded_bytes().ends_with(b".tmp"))
        .collect();
    assert!(left.is_empty(), "spool files left: {left:?}");
}

#[test]
fn torn_tail() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(vec![tier(dir.path(), 0)]);
    putb(&s, "k", b"value").unwrap();
    let name = active_path(&s);
    // half a record appended
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&name)
        .unwrap();
    f.write_all(&[9, 0, 0, 0, 200, 0, 0, 0, b'x']).unwrap();
    drop(f);
    let (cfg, cap) = config(vec![tier(dir.path(), 0)]);
    let s2 = Store::open(cfg).unwrap();
    assert_eq!(read(&s2, "k").as_deref(), Some(&b"value"[..]));
    let size = std::fs::metadata(&name).unwrap().len();
    assert_eq!(size, (REC_HEADER + 1 + 5) as u64, "not truncated");
    // the line the Go loader logs (not asserted by the Go test)
    assert!(
        cap.strings()
            .contains(&"pack 000001: truncating torn tail at 14 (was 23)".to_string()),
        "{:?}",
        cap.strings()
    );
    drop(s);
}

#[test]
fn evict_keeps_read_pack() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(vec![tier(dir.path(), 2 * PACK_LIMIT + PACK_LIMIT / 2)]);
    let big = big();
    // two full packs, pack 1 read from, then a third: pack 2 goes, not pack 1
    for i in 0..2 * PER {
        putb(&s, &format!("o/{i}"), &big).unwrap();
    }
    read(&s, "o/0");
    for i in 2 * PER..7 * PER / 2 {
        putb(&s, &format!("o/{i}"), &big).unwrap();
    }
    s.wait();
    assert!(
        read(&s, "o/0").is_some(),
        "pack 1 was read last and got evicted"
    );
    assert!(
        read(&s, &format!("o/{}", PER + 1)).is_none(),
        "pack 2 was never read and survived"
    );
}

// recency is the pack's mtime: a pack read before a restart outlives one that was not
#[test]
fn evict_recency_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::system_offset(0));
    let (mut cfg, _cap) = config(vec![tier(dir.path(), 0)]);
    cfg.clock = clock.clone();
    let s = Store::open(cfg).unwrap();
    let big = big();
    for i in 0..2 * PER {
        putb(&s, &format!("o/{i}"), &big).unwrap();
    }
    clock.set_offset(HOUR_NS);
    read(&s, "o/0"); // pack 1 read later than pack 2 was written, pack 2 never read
    s.close().unwrap();

    let s2 = open(vec![tier(dir.path(), 2 * PACK_LIMIT + PACK_LIMIT / 2)]);
    let snap = s2.snapshot();
    let (p1, p2) = (snap.packs[&1].used, snap.packs[&2].used);
    assert!(
        p1 > p2,
        "after restart pack 1 used={p1} not newer than pack 2 used={p2}"
    );
    for i in 2 * PER..7 * PER / 2 {
        putb(&s2, &format!("o/{i}"), &big).unwrap();
    }
    s2.wait();
    assert!(
        read(&s2, "o/0").is_some(),
        "pack 1 was read before the restart and got evicted"
    );
    assert!(
        read(&s2, &format!("o/{}", PER + 1)).is_none(),
        "pack 2 was never read and survived"
    );
}

// a pack whose entries were mostly overwritten goes before a fully live one, however recent
#[test]
fn evict_superseded_first() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(vec![tier(dir.path(), 2 * PACK_LIMIT + PACK_LIMIT / 2)]);
    let big = big();
    for i in 0..PER {
        // pack 1: o/0..per
        putb(&s, &format!("o/{i}"), &big).unwrap();
    }
    for i in 0..PER {
        // pack 2: the same keys again, pack 1 is now dead weight
        putb(&s, &format!("o/{i}"), &big).unwrap();
    }
    read(&s, "o/0"); // served from pack 2; pack 1 gets no reads but make it "recent" anyway
    s.set_used(1, now_ns() + HOUR_NS);
    for i in PER..5 * PER / 2 {
        // pack 3 and a half: over budget
        putb(&s, &format!("o/{i}"), &big).unwrap();
    }
    s.wait();
    assert!(
        !dir.path().join("000001.pack").exists(),
        "superseded pack 1 still on disk"
    );
    assert!(
        read(&s, "o/0").is_some(),
        "live pack 2 evicted instead of dead pack 1"
    );
}

// over the fast budget a pack moves to the cold dir and is still served, over the cold budget it
// is gone. A restart finds cold packs
#[test]
fn tiers() {
    let (hot, cold) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    // room for the active pack plus one sealed pack in each tier
    let s = open(vec![
        tier(hot.path(), 2 * PACK_LIMIT),
        tier(cold.path(), PACK_LIMIT + PACK_LIMIT / 2),
    ]);
    let big = big();
    let puts = |from: usize, to: usize| {
        for i in from..to {
            putb(&s, &format!("o/{i}"), &big).unwrap();
        }
        s.wait();
    };
    puts(0, 5 * PER / 2); // packs 1, 2 sealed, 3 half: 1 goes cold
    assert!(
        cold.path().join("000001.pack").exists(),
        "pack 1 not demoted"
    );
    assert!(!hot.path().join("000001.pack").exists(), "pack 1 still hot");
    assert!(
        read(&s, "o/0").is_some(),
        "o/0 not served from the cold tier"
    );
    puts(5 * PER / 2, 7 * PER / 2); // 3 sealed, 4 half: 2 goes cold, cold over budget, 2 was read less recently than 1
    assert!(
        read(&s, &format!("o/{}", PER + 1)).is_none(),
        "pack 2 survived the cold budget"
    );
    assert!(
        read(&s, "o/0").is_some(),
        "pack 1 (read) dropped instead of pack 2"
    );
    s.close().unwrap();
    let s2 = open(vec![tier(hot.path(), 0), tier(cold.path(), 0)]);
    assert!(
        read(&s2, "o/0").is_some() && s2.snapshot().packs[&1].tier == 1,
        "cold pack 1 not found after restart"
    );
}

// a reader handed out by Get keeps working while its pack is evicted underneath it
#[test]
fn get_survives_eviction() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(vec![tier(dir.path(), PACK_LIMIT + PACK_LIMIT / 2)]);
    let big = big();
    for i in 0..PER {
        putb(&s, &format!("o/{i}"), &big).unwrap();
    }
    let mut r = s.get(b"o/0").expect("o/0"); // pack 1, about to be evicted
    for i in PER..5 * PER / 2 {
        putb(&s, &format!("o/{i}"), &big).unwrap();
    }
    s.wait();
    assert!(
        !dir.path().join("000001.pack").exists(),
        "pack 1 still on disk"
    );
    let mut got = Vec::new();
    let res = r.read_to_end(&mut got);
    assert!(
        res.is_ok() && got == big,
        "in-flight read broke: {res:?}, {} bytes",
        got.len()
    );
}

// a Put racing with Close (connections outlive the listener) must not panic
#[test]
fn put_after_close() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(vec![tier(dir.path(), 1)]);
    putb(&s, "k", b"v").unwrap();
    s.close().unwrap();
    let res = putb(&s, "k2", b"v");
    // (the Go test only checks for a panic; the error text is the one jigd sends)
    assert_eq!(
        res.map_err(|e| e.to_string()),
        Err("store closed".to_string())
    );
}

// after a restart the copy in the newer (hot) pack wins over a demoted older one
#[test]
fn reopen_newest_across_tiers() {
    let (hot, cold) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let s = open(vec![
        tier(hot.path(), PACK_LIMIT + PACK_LIMIT / 2),
        tier(cold.path(), 0),
    ]);
    let big = big();
    putb(&s, "m/k", b"old").unwrap();
    for i in 0..PER {
        putb(&s, &format!("o/{i}"), &big).unwrap();
    }
    putb(&s, "m/k", b"new").unwrap(); // pack 2
    for i in PER..2 * PER {
        putb(&s, &format!("p/{i}"), &big).unwrap();
    }
    s.wait();
    assert!(
        cold.path().join("000001.pack").exists(),
        "pack 1 not demoted"
    );
    s.close().unwrap();
    let s2 = open(vec![tier(hot.path(), 0), tier(cold.path(), 0)]);
    let snap = s2.snapshot();
    assert_eq!(
        read(&s2, "m/k").as_deref(),
        Some(&b"new"[..]),
        "from pack {}",
        snap.index[&b"m/k"[..]].pack
    );
    assert!(
        snap.packs[&2].live >= snap.packs[&1].live,
        "hot pack accounted as superseded: live {} vs cold {}",
        snap.packs[&2].live,
        snap.packs[&1].live
    );
}

// a sealed pack shorter than its hint claims (writes lost in a crash) must not index records
// past EOF: Get would promise bytes it cannot send
#[test]
fn hint_past_eof() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(vec![tier(dir.path(), 0)]);
    putb(&s, "a", b"first").unwrap();
    putb(&s, "b", b"second").unwrap();
    let name = active_path(&s);
    s.close().unwrap();
    let f = std::fs::OpenOptions::new().write(true).open(&name).unwrap();
    f.set_len((REC_HEADER + 1 + 5 + REC_HEADER + 1 + 2) as u64)
        .unwrap(); // "b" torn mid-value
    drop(f);
    let s2 = open(vec![tier(dir.path(), 0)]);
    assert_eq!(read(&s2, "a").as_deref(), Some(&b"first"[..]), "a");
    if let Some(mut r) = s2.get(b"b") {
        let mut got = Vec::new();
        let _ = r.read_to_end(&mut got);
        panic!(
            "b indexed past EOF: promised {}, readable {}",
            r.len(),
            got.len()
        );
    }
}

#[test]
fn evict_oldest_pack() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(vec![tier(dir.path(), PACK_LIMIT + PACK_LIMIT / 2)]);
    let big = big();
    let n = PACK_LIMIT as usize * 5 / 2 / BIG;
    // ~2.5 packs worth: the first pack must go
    for i in 0..n {
        putb(&s, &format!("o/{i}"), &big).unwrap();
    }
    s.wait();
    assert!(
        read(&s, "o/0").is_none(),
        "o/0 should have been evicted with pack 1"
    );
    assert!(
        read(&s, &format!("o/{}", n - 1)).is_some(),
        "newest entry missing"
    );
    assert!(
        !dir.path().join("000001.pack").exists(),
        "pack 1 still on disk"
    );
}
