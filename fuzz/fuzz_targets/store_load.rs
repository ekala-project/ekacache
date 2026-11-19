//! `Store::open` over random pack and hint files in two tiers: no panic, and the loaded index is
//! sound (spec/store.qnt `indexSound`): every entry points inside a known pack, and `get`
//! returns exactly its bytes. Then `close`.
#![no_main]

use std::sync::Arc;

use arbitrary::Arbitrary;
use ekacached::log::Logger;
use ekacached::store::{DemoterMode, Store, StoreConfig, TestClock, Tier};
use libfuzzer_sys::fuzz_target;

#[derive(Debug, Arbitrary)]
struct File {
    tier: bool,
    id: u8,
    kind: u8,
    bytes: Vec<u8>,
}

fuzz_target!(|files: Vec<File>| {
    let root = tempfile::tempdir().expect("temp dir");
    let dirs = [root.path().join("t0"), root.path().join("t1")];
    for d in &dirs {
        std::fs::create_dir(d).expect("tier dir");
    }
    for f in files.iter().take(8) {
        let ext = match f.kind % 4 {
            0 | 1 => "pack",
            2 => "hint",
            _ => "hint.tmp",
        };
        let name = format!("{:06}.{ext}", f.id % 5);
        let _ = std::fs::write(dirs[usize::from(f.tier)].join(name), &f.bytes);
    }
    let tiers = dirs
        .iter()
        .map(|d| Tier {
            dir: d.as_os_str().as_encoded_bytes().to_vec(),
            budget: 1 << 20,
        })
        .collect();
    let mut cfg = StoreConfig::new(tiers);
    cfg.pack_limit = 1 << 16;
    cfg.clock = Arc::new(TestClock::fixed(1_700_000_000 * 1_000_000_000));
    cfg.logger = Logger::capture().0;
    cfg.demoter = DemoterMode::Manual;
    let Ok(store) = Store::open(cfg) else { return };
    let snap = store.snapshot();
    for (key, loc) in &snap.index {
        let pack = snap
            .packs
            .get(&loc.pack)
            .expect("an index entry names a known pack");
        assert!(
            i64::from(loc.off) + i64::from(loc.len) <= pack.size,
            "entry past the end of its pack"
        );
        let mut value = store.get(key).expect("an indexed key reads");
        assert_eq!(value.len(), u64::from(loc.len));
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut value, &mut buf).expect("read the value");
        assert_eq!(buf.len() as u64, u64::from(loc.len));
    }
    let _ = store.close();
});
