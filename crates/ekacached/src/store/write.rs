//! Writing: `Put`, `rotate`, `seal`, and the active pack at `Close` (`ecd-store.md` §Writing,
//! §Close; adopted Q-STORE-1, Q-STORE-2, Q-STORE-7; Q-STORE-11, Q-STORE-12 kept).

use std::fs::File;
use std::io::{BufWriter, Read, Write};

use super::pack::{Loc, Pack};
use super::record::{REC_HEADER, push_hint_entry, record_head};
use super::{Inner, MAX_KEY, MAX_VALUE, SPOOL_MIN, Shared};
use crate::goerr::GoErr;
use crate::gofs;

/// A PUT body, read before the store lock is taken.
#[derive(Debug)]
pub enum Body {
    /// below `SPOOL_MIN`
    Mem(Vec<u8>),
    /// an unlinked `put-*.tmp` in tier 0 holding the body from offset 0, and its name
    Spool { file: File, name: Vec<u8> },
}

/// The `bad put` check, done before any body byte is read: key empty, longer than 4096, or
/// containing `\n` or a space; size negative or above `2<<30`.
pub fn validate_put(key: &[u8], size: i64) -> Result<(), GoErr> {
    if key.is_empty()
        || key.len() > MAX_KEY
        || key.iter().any(|&b| b == b'\n' || b == b' ')
        || !(0..=MAX_VALUE).contains(&size)
    {
        return Err(GoErr::msg("bad put"));
    }
    Ok(())
}

pub(crate) fn create_spool(s: &Shared) -> Result<(File, Vec<u8>), GoErr> {
    let (file, name) = gofs::create_temp(&s.tiers[0].dir, b"put-", b".tmp")?;
    // unlinked straight away: nothing is left behind whatever happens next (error ignored)
    let _ = gofs::remove(&name);
    Ok((file, name))
}

pub(crate) fn read_body(s: &Shared, size: i64, r: &mut impl Read) -> Result<Body, GoErr> {
    if size < SPOOL_MIN {
        let mut buf = vec![0u8; usize::try_from(size).unwrap_or(0)];
        gofs::read_full(r, &mut buf)?;
        return Ok(Body::Mem(buf));
    }
    let (mut file, name) = create_spool(s)?;
    // io.CopyN: a body that ends early is `EOF`, whatever was copied
    let mut buf = vec![0u8; 32 * 1024];
    let mut left = size as u64;
    while left > 0 {
        let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
        let n = match r.read(&mut buf[..want]) {
            Ok(0) => return Err(GoErr::eof()),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(GoErr::from_io(&e)),
        };
        file.write_all(&buf[..n])
            .map_err(|e| GoErr::path("write", &name, &e))?;
        left -= n as u64;
    }
    Ok(Body::Spool { file, name })
}

/// `Put`: validate, then under the write lock rotate if needed, append the record at the
/// active pack's size, and account for it.
pub(crate) fn put(s: &Shared, key: &[u8], size: i64, body: Body) -> Result<(), GoErr> {
    validate_put(key, size)?;
    let mut inner = s.write();
    if inner.closed {
        return Err(GoErr::msg("store closed"));
    }
    let rec_len = REC_HEADER as i64 + key.len() as i64 + size;
    let Some(active) = inner.active else {
        return Err(GoErr::msg("store closed"));
    };
    let active_size = inner.packs.get(&active).map_or(0, |p| p.size);
    // a record that does not fit goes into an empty pack on its own
    if active_size + rec_len > s.pack_limit && active_size > 0 {
        rotate(s, &mut inner, active.wrapping_add(1))?;
    }
    let Some(id) = inner.active else {
        return Err(GoErr::msg("store closed"));
    };
    let Some(p) = inner.packs.get(&id) else {
        return Err(GoErr::msg("store closed"));
    };
    let (file, path, at) = (std::sync::Arc::clone(&p.file), p.path.clone(), p.size);
    // the active pack is only written here, under the write lock; positional writes at
    // `size` leave no shared cursor (Go seeks, then writes: the same bytes)
    let head = record_head(key, size as u32);
    if let (_, Err(e)) = gofs::write_all_at(&file, &path, &head, at as u64) {
        let _ = file.set_len(at as u64);
        return Err(e);
    }
    let body_at = at as u64 + head.len() as u64;
    let (n, res) = match body {
        Body::Mem(buf) => gofs::write_all_at(&file, &path, &buf, body_at),
        Body::Spool { file: spool, name } => {
            gofs::copy_file(&file, &path, body_at, &spool, &name, 0)
        }
    };
    if res.is_err() || n != size as u64 {
        let _ = file.set_len(at as u64);
        let mut msg = b"put ".to_vec();
        msg.extend_from_slice(key);
        msg.extend_from_slice(format!(": wrote {n} of {size}: ").as_bytes());
        // Q-DAEMON-12: a short copy without an error prints Go's nil error
        msg.extend_from_slice(&res.err().map_or_else(|| b"<nil>".to_vec(), |e| e.text()));
        return Err(GoErr::Msg(msg));
    }
    if let Some(old) = inner.index.get(key).copied()
        && let Some(op) = inner.packs.get_mut(&old.pack)
    {
        op.live -= i64::from(old.len);
    }
    let off = (at as u32)
        .wrapping_add(REC_HEADER as u32)
        .wrapping_add(key.len() as u32);
    inner.index.insert(
        key.into(),
        Loc {
            pack: id,
            off,
            len: size as u32,
        },
    );
    if let Some(p) = inner.packs.get_mut(&id) {
        p.size += rec_len;
        p.live += size;
    }
    inner.totals[0] += rec_len;
    let budget = s.tiers[0].budget;
    if budget > 0 && inner.totals[0] > budget {
        s.kick();
    }
    Ok(())
}

/// `rotate(id)`: seal the active pack (an error changes nothing), then create the new one in
/// tier 0 with `O_EXCL` (Q-STORE-3, Q-STORE-4, Q-STORE-11: an existing file fails this).
pub(crate) fn rotate(s: &Shared, inner: &mut Inner, id: u32) -> Result<(), GoErr> {
    if let Some(active) = inner.active {
        seal(s, inner, active)?;
    }
    let path = s.name(0, id, "pack");
    let file = gofs::create_excl(&path, 0o644)?;
    // being written counts as recent
    let now = s.now();
    inner.packs.insert(id, Pack::new(0, path, file, 0, now));
    inner.active = Some(id);
    Ok(())
}

/// `seal(p)`: write the hint for a pack that will not grow any more. Adopted Q-STORE-1: the
/// entries are sorted by record offset. Adopted Q-STORE-7: fsync the pack, write
/// `%06d.hint.tmp`, flush, fsync it, close, rename it to `%06d.hint`, fsync the directory.
/// Q-STORE-12 kept: the entries come from a walk of the whole index under the write lock.
pub(crate) fn seal(s: &Shared, inner: &Inner, id: u32) -> Result<(), GoErr> {
    let Some(p) = inner.packs.get(&id) else {
        return Ok(());
    };
    let tier = p.tier;
    gofs::fsync(&p.file, &p.path)?;
    let mut entries: Vec<(u64, &[u8], u32)> = inner
        .index
        .iter()
        .filter(|(_, l)| l.pack == id)
        .map(|(k, l)| {
            let rec_off = l
                .off
                .wrapping_sub(REC_HEADER as u32)
                .wrapping_sub(k.len() as u32);
            (u64::from(rec_off), &k[..], l.len)
        })
        .collect();
    entries.sort_unstable();
    let tmp = s.name(tier, id, "hint.tmp");
    let f = gofs::create(&tmp)?;
    let mut w = BufWriter::with_capacity(1 << 20, &f);
    let mut entry = Vec::with_capacity(64);
    for (rec_off, key, vlen) in entries {
        entry.clear();
        push_hint_entry(&mut entry, key, vlen, rec_off);
        // a write error is reported once, as Go's sticky bufio error would be at Flush; the
        // temp file is left behind
        w.write_all(&entry)
            .map_err(|e| GoErr::path("write", &tmp, &e))?;
    }
    w.flush().map_err(|e| GoErr::path("write", &tmp, &e))?;
    drop(w);
    gofs::fsync(&f, &tmp)?;
    drop(f);
    let hint = s.name(tier, id, "hint");
    gofs::rename(&tmp, &hint)?;
    gofs::fsync_dir(&s.tiers[tier].dir)
}

/// The end of `Close`: seal the active pack, or (adopted Q-STORE-2) delete it if it is empty.
pub(crate) fn close_active(s: &Shared) -> Result<(), GoErr> {
    let mut inner = s.write();
    let Some(active) = inner.active else {
        return Ok(());
    };
    match inner.packs.get(&active) {
        Some(p) if p.size == 0 => {
            let _ = gofs::remove(&p.path);
            inner.packs.remove(&active);
            inner.active = None;
            Ok(())
        }
        Some(_) => seal(s, &inner, active),
        None => Ok(()),
    }
}
