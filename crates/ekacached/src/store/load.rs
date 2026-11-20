//! Startup: `OpenStore` and `load` (`ecd-store.md` §Startup: OpenStore and load, §Each start
//! makes a new pack; quirks Q-STORE-2 (adopted), Q-STORE-4, Q-STORE-6, Q-STORE-9, Q-STORE-14,
//! Q-STORE-15).

use std::fs::File;
use std::io::{BufReader, Read};

use super::naming;
use super::pack::{Loc, Pack};
use super::record::{HINT_HEADER, REC_HEADER, parse_hint_header, parse_record_header};
use super::{Inner, Shared, write};
use crate::goerr::GoErr;
use crate::gofs;

/// Load every tier (fast tier first, names in bytewise order), drop empty packs, rotate to a
/// new active pack. The caller starts the demoter and kicks it.
pub(crate) fn open_store(s: &Shared) -> Result<(), GoErr> {
    let mut inner = s.write();
    let mut last: u32 = 0;
    for t in 0..s.tiers.len() {
        gofs::mkdir_all(&s.tiers[t].dir, 0o755)?;
        for (path, name) in naming::glob_packs(&s.tiers[t].dir) {
            // Q-STORE-9: Sscanf is loose (`42.pack`, ` 00042.pack`, `000042.pack.pack`), and
            // Q-STORE-3: 7-digit ids do not parse and are skipped silently
            let Some(id) = naming::scanf_pack_id(&name) else {
                continue;
            };
            if inner.packs.contains_key(&id) {
                // a leftover of an interrupted demotion (or a stray name, Q-STORE-9): delete
                // this copy and the hint named after the id; errors ignored
                let _ = gofs::remove(&path);
                let _ = gofs::remove(&s.name(t, id, "hint"));
                continue;
            }
            if let Err(e) = load(s, &mut inner, t, id) {
                // Q-STORE-4: whatever load registered before failing stays, `last` is not
                // updated
                let mut msg = format!("pack {id:06}: ").into_bytes();
                msg.extend_from_slice(&e.text());
                msg.extend_from_slice(b" (skipped)");
                s.log.log(&msg);
                continue;
            }
            last = last.max(id);
        }
    }
    // adopted Q-STORE-2: every loaded pack with no bytes goes, with its hint, in any tier;
    // `last` still counts it, so ids stay monotonic
    let empty: Vec<u32> = inner
        .packs
        .iter()
        .filter(|(_, p)| p.size == 0)
        .map(|(id, _)| *id)
        .collect();
    for id in empty {
        if let Some(p) = inner.packs.remove(&id) {
            let _ = gofs::remove(&p.path);
            let _ = gofs::remove(&s.name(p.tier, id, "hint"));
            s.log.logf(format_args!("removed empty pack {id:06}"));
        }
    }
    write::rotate(s, &mut inner, last.wrapping_add(1))
}

/// `load(tier, id)`: register the pack, then index it from its hint if it has one, else by
/// scanning its records (truncating a torn tail and sealing it).
fn load(s: &Shared, inner: &mut Inner, tier: usize, id: u32) -> Result<(), GoErr> {
    let path = s.name(tier, id, "pack");
    // Q-STORE-14: sealed packs are opened read-write too
    let file = gofs::open_rdwr(&path)?;
    let (size, mtime) = gofs::stat(&file, &path)?;
    // recency survives restarts through the mtime
    inner
        .packs
        .insert(id, Pack::new(tier, path.clone(), file, size, mtime));
    if let Ok(hint) = File::open(ekacache_core::bytes::os_path(&s.name(tier, id, "hint"))) {
        load_hint(inner, id, size, hint);
        inner.totals[tier] += size;
        // a hinted pack is never truncated or rewritten
        return Ok(());
    }
    // unsealed (it was the active pack when the daemon stopped): scan, truncate a torn tail
    let Some(file) = inner.packs.get(&id).map(|p| std::sync::Arc::clone(&p.file)) else {
        return Ok(());
    };
    let off = scan(inner, id, size, &file);
    let mut size = size;
    if off != size {
        s.log.logf(format_args!(
            "pack {id:06}: truncating torn tail at {off} (was {size})"
        ));
        // on error the pack stays registered with its records but uncounted (Q-STORE-4)
        gofs::truncate(&file, &path, off)?;
        size = off;
        if let Some(p) = inner.packs.get_mut(&id) {
            p.size = off;
        }
    }
    inner.totals[tier] += size;
    write::seal(s, inner, id)
}

/// The hint loop. A short read or an I/O error ends it silently (Q-STORE-15); an entry whose
/// record would end past the pack's size at open is skipped.
fn load_hint(inner: &mut Inner, id: u32, size: i64, hint: File) {
    let mut r = BufReader::with_capacity(1 << 20, hint);
    loop {
        let mut hdr = [0u8; HINT_HEADER];
        if r.read_exact(&mut hdr).is_err() {
            break;
        }
        let (klen, vlen, off) = parse_hint_header(&hdr);
        let Some(key) = read_key(&mut r, klen) else {
            break;
        };
        // pack and hint are not fsynced in Go: after a crash the hint can outlive pack writes.
        // Go's int64 arithmetic, wrapping included.
        let end = (off as i64)
            .wrapping_add(REC_HEADER as i64)
            .wrapping_add(i64::from(klen))
            .wrapping_add(i64::from(vlen));
        if end > size {
            continue;
        }
        let loc = Loc {
            pack: id,
            off: (off as u32)
                .wrapping_add(REC_HEADER as u32)
                .wrapping_add(klen),
            len: vlen,
        };
        add(inner, id, key, loc);
    }
}

/// The scan of an unsealed pack: index complete records in order, return the offset after the
/// last complete one. A run of zero bytes scans as empty-key records (Q-STORE-6).
fn scan(inner: &mut Inner, id: u32, size: i64, file: &File) -> i64 {
    let mut r = BufReader::with_capacity(1 << 20, file);
    let mut off: i64 = 0;
    loop {
        let mut hdr = [0u8; REC_HEADER];
        if r.read_exact(&mut hdr).is_err() {
            break;
        }
        let (klen, vlen) = parse_record_header(hdr);
        let Some(key) = read_key(&mut r, klen) else {
            break;
        };
        // Go discards vlen bytes through bufio and stops when the file ends first; the file
        // size does not change under us, so check the bound and seek instead of reading
        let end = off + REC_HEADER as i64 + i64::from(klen) + i64::from(vlen);
        if end > size || r.seek_relative(i64::from(vlen)).is_err() {
            break;
        }
        let loc = Loc {
            pack: id,
            off: (off as u32)
                .wrapping_add(REC_HEADER as u32)
                .wrapping_add(klen),
            len: vlen,
        };
        add(inner, id, key, loc);
        off = end;
    }
    off
}

/// Read a `klen`-byte key, or `None` on a short read. Go allocates `klen` bytes up front (up
/// to 4 GiB from a corrupt length, Q-STORE-6); this buffer only grows with the bytes actually
/// there, so a length beyond the end of the file is the same short read without the
/// allocation (the one deliberate deviation in the loader, implementation/04 §open_store).
fn read_key(r: &mut impl Read, klen: u32) -> Option<Vec<u8>> {
    let mut key = Vec::with_capacity((klen as usize).min(64 * 1024));
    match r.take(u64::from(klen)).read_to_end(&mut key) {
        Ok(n) if n == klen as usize => Some(key),
        _ => None,
    }
}

/// load()'s add: newest pack id wins whatever the load order; replaced bytes leave the old
/// pack's `live`.
fn add(inner: &mut Inner, id: u32, key: Vec<u8>, l: Loc) {
    if let Some(old) = inner.index.get(key.as_slice()).copied() {
        if old.pack > l.pack {
            return;
        }
        if let Some(p) = inner.packs.get_mut(&old.pack) {
            p.live -= i64::from(old.len);
        }
    }
    inner.index.insert(key.into_boxed_slice(), l);
    if let Some(p) = inner.packs.get_mut(&id) {
        p.live += i64::from(l.len);
    }
}
