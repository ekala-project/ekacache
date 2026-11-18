//! Store behaviour beyond the ported Go tests (`jigd-store.md`; implementation/04 §Tests): the
//! adopted changes (Q-STORE-1 sorted hints, Q-STORE-2 empty packs, Q-STORE-5 kick at start,
//! Q-STORE-7 copy path), the loader quirks (Q-STORE-4, Q-STORE-9, duplicate ids), recency, put
//! validation and error texts, and the manual demoter the Quint replay drives.

// helpers outside #[test] fns: a panic is the failure report
#![allow(clippy::unwrap_used)]

use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jigd::log::{LogCapture, Logger};
use jigd::store::{
    Body, DemoteStep, DemoterMode, HINT_HEADER, MAX_VALUE, Store, StoreConfig, TestClock, Tier,
    validate_put,
};

fn tier(dir: &Path, budget: i64) -> Tier {
    Tier {
        dir: dir.as_os_str().as_encoded_bytes().to_vec(),
        budget,
    }
}

fn config(tiers: Vec<Tier>, pack_limit: i64) -> (StoreConfig, LogCapture) {
    let (logger, cap) = Logger::capture();
    let mut cfg = StoreConfig::new(tiers);
    cfg.pack_limit = pack_limit;
    cfg.logger = logger;
    (cfg, cap)
}

fn putb(s: &Store, key: &[u8], v: &[u8]) {
    s.put_reader(key, v.len() as i64, &mut &v[..]).expect("put");
}

fn read(s: &Store, key: &[u8]) -> Option<Vec<u8>> {
    let mut r = s.get(key)?;
    let mut b = Vec::new();
    r.read_to_end(&mut b).expect("read value");
    Some(b)
}

fn record(key: &[u8], val: &[u8]) -> Vec<u8> {
    let mut r = Vec::new();
    r.extend_from_slice(&(key.len() as u32).to_le_bytes());
    r.extend_from_slice(&(val.len() as u32).to_le_bytes());
    r.extend_from_slice(key);
    r.extend_from_slice(val);
    r
}

fn hint_entry(key: &[u8], vlen: u32, off: u64) -> Vec<u8> {
    let mut r = Vec::new();
    r.extend_from_slice(&(key.len() as u32).to_le_bytes());
    r.extend_from_slice(&vlen.to_le_bytes());
    r.extend_from_slice(&off.to_le_bytes());
    r.extend_from_slice(key);
    r
}

/// `(record offset, key, vlen)` of every entry of a hint file.
fn parse_hint(b: &[u8]) -> Vec<(u64, Vec<u8>, u32)> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + HINT_HEADER <= b.len() {
        let klen = u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as usize;
        let vlen = u32::from_le_bytes(b[at + 4..at + 8].try_into().unwrap());
        let off = u64::from_le_bytes(b[at + 8..at + 16].try_into().unwrap());
        out.push((off, b[at + 16..at + 16 + klen].to_vec(), vlen));
        at += HINT_HEADER + klen;
    }
    assert_eq!(at, b.len(), "hint ends mid-entry");
    out
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

fn p(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

// adopted Q-STORE-1: a hint lists its entries by record offset, whatever the index order; only
// the newest record of a key is listed
#[test]
fn seal_writes_hint_sorted_by_offset() {
    let dir = tempfile::tempdir().unwrap();
    let (cfg, _cap) = config(vec![tier(dir.path(), 0)], 1 << 20);
    let s = Store::open(cfg).unwrap();
    for i in 0..300u32 {
        putb(
            &s,
            format!("k/{}", i * 7919 % 300).as_bytes(),
            &i.to_le_bytes(),
        );
    }
    for i in (0..300u32).step_by(3) {
        putb(&s, format!("k/{i}").as_bytes(), b"again");
    }
    s.close().unwrap();
    let pack = std::fs::read(p(dir.path(), "000001.pack")).unwrap();
    let hint = parse_hint(&std::fs::read(p(dir.path(), "000001.hint")).unwrap());
    assert_eq!(hint.len(), 300);
    assert!(
        hint.windows(2).all(|w| w[0].0 < w[1].0),
        "hint not sorted by offset"
    );
    for (off, key, vlen) in &hint {
        let off = *off as usize;
        assert_eq!(&pack[off..off + 4], &(key.len() as u32).to_le_bytes());
        assert_eq!(&pack[off + 4..off + 8], &vlen.to_le_bytes());
        assert_eq!(&pack[off + 8..off + 8 + key.len()], &key[..]);
    }
    assert!(
        !p(dir.path(), "000001.hint.tmp").exists(),
        "temp hint left behind"
    );
}

// Go writes hints in map order: any order loads the same
#[test]
fn loads_unsorted_hint() {
    let dir = tempfile::tempdir().unwrap();
    let recs = [
        (&b"a"[..], &b"one"[..]),
        (b"bb", b"two"),
        (b"a", b"three"),
        (b"c", b""),
    ];
    let mut pack = Vec::new();
    let mut offs = Vec::new();
    for (k, v) in recs {
        offs.push(pack.len() as u64);
        pack.extend_from_slice(&record(k, v));
    }
    std::fs::write(p(dir.path(), "000001.pack"), &pack).unwrap();
    // newest record of each key, in a scrambled order
    let mut hint = hint_entry(b"c", 0, offs[3]);
    hint.extend_from_slice(&hint_entry(b"a", 5, offs[2]));
    hint.extend_from_slice(&hint_entry(b"bb", 3, offs[1]));
    std::fs::write(p(dir.path(), "000001.hint"), &hint).unwrap();
    let (cfg, _cap) = config(vec![tier(dir.path(), 0)], 1 << 20);
    let s = Store::open(cfg).unwrap();
    assert_eq!(read(&s, b"a").as_deref(), Some(&b"three"[..]));
    assert_eq!(read(&s, b"bb").as_deref(), Some(&b"two"[..]));
    assert_eq!(read(&s, b"c").as_deref(), Some(&b""[..]));
    let snap = s.snapshot();
    assert_eq!(
        (snap.packs[&1].size, snap.packs[&1].live),
        (pack.len() as i64, 8)
    );
    assert_eq!(snap.active, 2);
    // a hinted pack is not rewritten
    assert_eq!(std::fs::read(p(dir.path(), "000001.hint")).unwrap(), hint);
}

// a hint that ends mid-entry loads the entries before the cut (Q-STORE-15)
#[test]
fn short_hint_stops_silently() {
    let dir = tempfile::tempdir().unwrap();
    let mut pack = record(b"x", b"1");
    pack.extend_from_slice(&record(b"y", b"2"));
    std::fs::write(p(dir.path(), "000003.pack"), &pack).unwrap();
    let mut hint = hint_entry(b"x", 1, 0);
    hint.extend_from_slice(&hint_entry(b"y", 1, 10)[..HINT_HEADER]);
    std::fs::write(p(dir.path(), "000003.hint"), &hint).unwrap();
    let (cfg, cap) = config(vec![tier(dir.path(), 0)], 1 << 20);
    let s = Store::open(cfg).unwrap();
    assert_eq!(read(&s, b"x").as_deref(), Some(&b"1"[..]));
    assert!(read(&s, b"y").is_none());
    assert!(cap.lines().is_empty(), "{:?}", cap.strings());
    assert_eq!(s.snapshot().active, 4);
}

// adopted Q-STORE-2: packs with no bytes are removed at open, with their hints, in any tier;
// their ids still count for the next one. An empty active pack is deleted at close
#[test]
fn empty_packs_are_dropped() {
    let (hot, cold) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(p(hot.path(), "000001.pack"), b"").unwrap(); // unsealed, empty
    std::fs::write(p(hot.path(), "000002.pack"), record(b"k", b"v")).unwrap();
    std::fs::write(p(cold.path(), "000007.pack"), b"").unwrap(); // sealed, empty
    std::fs::write(p(cold.path(), "000007.hint"), b"").unwrap();
    let (cfg, cap) = config(vec![tier(hot.path(), 0), tier(cold.path(), 0)], 1 << 20);
    let s = Store::open(cfg).unwrap();
    assert_eq!(
        cap.strings(),
        ["removed empty pack 000001", "removed empty pack 000007"]
    );
    assert_eq!(
        names(hot.path()),
        ["000002.hint", "000002.pack", "000008.pack"]
    );
    assert!(names(cold.path()).is_empty());
    let snap = s.snapshot();
    assert_eq!(snap.active, 8);
    assert_eq!(snap.packs.keys().copied().collect::<Vec<_>>(), [2, 8]);
    assert_eq!(s.stats().to_string(), "keys=1 packs=2 bytes=10 live=1");
    s.close().unwrap();
    // the active pack never got a byte: it is deleted, not sealed
    assert_eq!(names(hot.path()), ["000002.hint", "000002.pack"]);
    // so the next start reuses its id (behaviors/jigd-store.md Q-STORE-2 says ids are never
    // reused; rule 1 leaves no trace of the deleted id to count)
    let (cfg, cap) = config(vec![tier(hot.path(), 0), tier(cold.path(), 0)], 1 << 20);
    let s = Store::open(cfg).unwrap();
    assert!(cap.lines().is_empty());
    assert_eq!(s.snapshot().active, 3);
}

// adopted Q-STORE-5: a store over budget at start is trimmed without waiting for a Put
#[test]
fn over_budget_at_start_is_trimmed() {
    let dir = tempfile::tempdir().unwrap();
    for id in 1..=3u32 {
        std::fs::write(
            p(dir.path(), &format!("{id:06}.pack")),
            record(format!("k{id}").as_bytes(), &[0u8; 100]),
        )
        .unwrap();
    }
    let (cfg, cap) = config(vec![tier(dir.path(), 250)], 1 << 20);
    let s = Store::open(cfg).unwrap();
    s.wait();
    let snap = s.snapshot();
    assert_eq!(snap.packs.keys().copied().collect::<Vec<_>>(), [2, 3, 4]);
    assert_eq!(cap.strings(), ["evicted pack 000001 (0 MiB, 0 MiB live)"]);
    assert!(read(&s, b"k1").is_none() && read(&s, b"k2").is_some());
}

// two tiers holding the same id (a demotion interrupted between link and cleanup): the fast
// tier's copy wins, the slow tier's pack and its hint are deleted
#[test]
fn duplicate_id_in_slower_tier_is_deleted() {
    let (hot, cold) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let pack = record(b"k", b"v");
    std::fs::write(p(hot.path(), "000004.pack"), &pack).unwrap();
    std::fs::write(p(hot.path(), "000004.hint"), hint_entry(b"k", 1, 0)).unwrap();
    std::fs::write(p(cold.path(), "000004.pack"), &pack).unwrap();
    std::fs::write(p(cold.path(), "000004.hint"), hint_entry(b"k", 1, 0)).unwrap();
    let (cfg, _cap) = config(vec![tier(hot.path(), 0), tier(cold.path(), 0)], 1 << 20);
    let s = Store::open(cfg).unwrap();
    assert!(names(cold.path()).is_empty());
    assert_eq!(s.snapshot().packs[&4].tier, 0);
    assert_eq!(read(&s, b"k").as_deref(), Some(&b"v"[..]));
}

// Q-STORE-4: a pack that cannot be loaded is logged and skipped, and its id does not count;
// Q-STORE-9: names parse loosely but `load` opens the canonical name; Q-STORE-3: seven digits
// are ignored silently
#[test]
fn loader_quirks() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(p(dir.path(), "000009.pack")).unwrap();
    std::fs::write(p(dir.path(), "42.pack"), record(b"loose", b"1")).unwrap();
    std::fs::write(p(dir.path(), "1000000.pack"), record(b"seven", b"1")).unwrap();
    let (cfg, cap) = config(vec![tier(dir.path(), 0)], 1 << 20);
    let s = Store::open(cfg).unwrap();
    let path = |n: &str| p(dir.path(), n).display().to_string();
    assert_eq!(
        cap.strings(),
        [
            format!(
                "pack 000009: open {}: is a directory (skipped)",
                path("000009.pack")
            ),
            format!(
                "pack 000042: open {}: no such file or directory (skipped)",
                path("000042.pack")
            ),
        ]
    );
    assert!(read(&s, b"loose").is_none() && read(&s, b"seven").is_none());
    assert_eq!(s.snapshot().active, 1);
    assert!(p(dir.path(), "42.pack").exists() && p(dir.path(), "1000000.pack").exists());
}

// Q-STORE-9: a stray name for a loaded id is a "duplicate": it goes, and so does the live
// pack's hint; the next start scans the pack instead
#[test]
fn stray_name_deletes_live_hint() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(p(dir.path(), "000005.pack"), record(b"k", b"v")).unwrap();
    std::fs::write(p(dir.path(), "000005.hint"), hint_entry(b"k", 1, 0)).unwrap();
    std::fs::write(p(dir.path(), "5.pack"), b"junk").unwrap();
    let (cfg, cap) = config(vec![tier(dir.path(), 0)], 1 << 20);
    let s = Store::open(cfg).unwrap();
    assert!(cap.lines().is_empty());
    assert_eq!(read(&s, b"k").as_deref(), Some(&b"v"[..]));
    assert_eq!(names(dir.path()), ["000005.pack", "000006.pack"]);
}

// recency: `used` moves on every Get, the mtime at most every touchEvery
#[test]
fn get_touches_mtime_at_most_every_touch_every() {
    let dir = tempfile::tempdir().unwrap();
    let base: i64 = 1_700_000_000_000_000_000;
    let clock = Arc::new(TestClock::fixed(base));
    let (mut cfg, _cap) = config(vec![tier(dir.path(), 0)], 1 << 20);
    cfg.clock = clock.clone();
    cfg.touch_every_ns = 10;
    let s = Store::open(cfg).unwrap();
    putb(&s, b"k", b"v");
    let mtime = || {
        let m = std::fs::metadata(p(dir.path(), "000001.pack")).unwrap();
        m.mtime() * 1_000_000_000 + m.mtime_nsec()
    };
    let before = mtime();
    clock.set(base + 10);
    read(&s, b"k");
    assert_eq!(s.snapshot().packs[&1].used, base + 10);
    assert_eq!(mtime(), before, "touched at exactly touchEvery");
    clock.set(base + 11);
    read(&s, b"k");
    assert_eq!(mtime(), base + 11);
    clock.set(base + 20);
    read(&s, b"k");
    assert_eq!(s.snapshot().packs[&1].used, base + 20);
    assert_eq!(mtime(), base + 11, "touched again within touchEvery");
}

#[test]
fn put_validation() {
    assert!(validate_put(b"k", 0).is_ok());
    assert!(validate_put(b"k", MAX_VALUE).is_ok(), "2<<30 is allowed");
    assert!(validate_put(&[b'x'; 4096], 1).is_ok());
    for (key, size) in [
        (&b""[..], 1),
        (&[b'x'; 4097][..], 1),
        (b"a b", 1),
        (b"a\nb", 1),
        (b"k", -1),
        (b"k", MAX_VALUE + 1),
    ] {
        assert_eq!(
            validate_put(key, size).map_err(|e| e.to_string()),
            Err("bad put".into()),
            "{key:?} {size}"
        );
    }
    // a key that is not UTF-8 is fine
    assert!(validate_put(b"\xff\xfe", 1).is_ok());
}

// the body errors Go's io.ReadFull and io.CopyN give, before anything is written
#[test]
fn put_body_errors() {
    let dir = tempfile::tempdir().unwrap();
    let (cfg, _cap) = config(vec![tier(dir.path(), 0)], 1 << 20);
    let s = Store::open(cfg).unwrap();
    let err = |size: i64, body: &[u8]| {
        s.put_reader(b"k", size, &mut &body[..])
            .unwrap_err()
            .to_string()
    };
    assert_eq!(err(5, b""), "EOF");
    assert_eq!(err(5, b"abc"), "unexpected EOF");
    assert_eq!(err(4 << 20, b"abc"), "EOF");
    assert_eq!(s.snapshot().packs[&1].size, 0);
    // a spool shorter than announced is a short copy: Q-DAEMON-12's `<nil>`
    let (mut spool, name) = s.create_spool().unwrap();
    std::io::Write::write_all(&mut spool, b"abc").unwrap();
    let res = s.put(b"k", 5, Body::Spool { file: spool, name });
    assert_eq!(
        res.map_err(|e| e.to_string()),
        Err("put k: wrote 3 of 5: <nil>".into())
    );
    assert_eq!(s.snapshot().packs[&1].size, 0, "torn record not truncated");
    assert!(
        std::fs::read_dir(dir.path()).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp"))
    );
}

// adopted Q-STORE-7: the copy path (no hard link) gives the same files and leaves no temp files
#[test]
fn demotion_by_copy() {
    let (hot, cold) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (mut cfg, cap) = config(vec![tier(hot.path(), 300), tier(cold.path(), 0)], 200);
    cfg.force_copy = true;
    let s = Store::open(cfg).unwrap();
    for i in 0..3u8 {
        putb(&s, &[b'k', b'0' + i], &[i; 150]);
    }
    s.wait();
    let snap = s.snapshot();
    assert_eq!(snap.packs[&1].tier, 1);
    assert_eq!(snap.packs[&2].tier, 1);
    assert_eq!(
        names(cold.path()),
        ["000001.hint", "000001.pack", "000002.hint", "000002.pack"]
    );
    assert_eq!(
        std::fs::read(p(cold.path(), "000001.pack")).unwrap(),
        record(b"k0", &[0; 150])
    );
    assert_eq!(read(&s, b"k0").as_deref(), Some(&[0u8; 150][..]));
    let cold_dir = cold.path().display().to_string();
    assert_eq!(
        cap.strings()[0],
        format!("demoted pack 000001 to {cold_dir} (0 MiB, 0 MiB live)")
    );
    // the copied pack keeps the source's recency in its mtime
    let used = snap.packs[&1].used;
    let m = std::fs::metadata(p(cold.path(), "000001.pack")).unwrap();
    assert_eq!(m.mtime() * 1_000_000_000 + m.mtime_nsec(), used);
}

// the split demotion the model replay drives: the copy happens without the lock, the switch is
// a separate step; reads go to the old file until then
#[test]
fn manual_demoter_steps() {
    let (hot, cold) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (mut cfg, _cap) = config(vec![tier(hot.path(), 30), tier(cold.path(), 0)], 24);
    cfg.demoter = DemoterMode::Manual;
    let s = Store::open(cfg).unwrap();
    assert!(s.kick_pending(), "adopted Q-STORE-5: kicked at open");
    assert!(matches!(s.demote_step(), DemoteStep::Idle));
    assert!(!s.kick_pending());
    putb(&s, b"a", &[1; 9]); // 18 bytes in pack 1
    assert!(!s.kick_pending(), "18 <= 30");
    putb(&s, b"b", &[2; 9]); // rotates: pack 2, tier 0 holds 36
    assert!(s.kick_pending());
    let DemoteStep::Copied(prep) = s.demote_step() else {
        panic!("expected a copy")
    };
    assert_eq!(prep.id(), 1);
    assert!(p(hot.path(), "000001.pack").exists() && p(cold.path(), "000001.pack").exists());
    assert_eq!(s.snapshot().packs[&1].tier, 0);
    assert!(s.kick_pending(), "the loop is still running");
    s.demote_commit(prep);
    assert_eq!(s.snapshot().packs[&1].tier, 1);
    assert!(!p(hot.path(), "000001.pack").exists());
    assert_eq!(s.snapshot().totals, [18, 18]);
    assert!(matches!(s.demote_step(), DemoteStep::Idle));
    assert!(!s.kick_pending());
    assert_eq!(read(&s, b"a").as_deref(), Some(&[1u8; 9][..]));
}
