//! Budgets, victims, demotion and eviction (`ecd-store.md` §Budgets, victims, demotion and
//! eviction; adopted Q-STORE-7 for the fsyncs; Q-STORE-8, Q-STORE-10, Q-STORE-12, Q-STORE-13
//! kept).
//!
//! `moveDown` to a slower tier is split into the unlocked copy (`begin`) and the locked switch
//! (`commit`), exactly where `spec/store.qnt` splits `demoteStep` and `demoteCommit`.

use std::fs::File;
use std::sync::Arc;
use std::sync::mpsc::Receiver;

use super::{DemoteStep, Inner, Shared};
use crate::goerr::GoErr;
use crate::gofs;

/// A demotion whose files are in place in the next tier, waiting for the switch.
#[derive(Debug)]
pub struct Prepared {
    id: u32,
    from: usize,
    file: File,
    dest: Vec<u8>,
}

impl Prepared {
    /// The pack being demoted.
    pub fn id(&self) -> u32 {
        self.id
    }
}

enum Begun {
    Evicted,
    Copied(Prepared),
}

/// The demoter thread: for each kick, demote until no tier has a victim. Ends when the store
/// drops the sender, after the kicks already queued.
pub(crate) fn demoter(s: &Arc<Shared>, rx: &Receiver<()>) {
    while rx.recv().is_ok() {
        run_loop(s);
    }
}

/// The demoter's inner loop: victim under the lock, move without it; on a failure log
/// `pack %06d: <err>` and stop (the next kick retries).
pub(crate) fn run_loop(s: &Shared) {
    loop {
        let v = victim(s, &s.write());
        let Some(id) = v else {
            break;
        };
        if let Err(e) = move_down(s, id) {
            log_failure(s, id, &e);
            break;
        }
    }
}

/// One iteration for the manual demoter.
pub(crate) fn step(s: &Shared) -> DemoteStep {
    let v = victim(s, &s.write());
    let Some(id) = v else {
        return DemoteStep::Idle;
    };
    match begin(s, id) {
        Ok(Begun::Evicted) => DemoteStep::Evicted(id),
        Ok(Begun::Copied(p)) => DemoteStep::Copied(p),
        Err(e) => {
            log_failure(s, id, &e);
            DemoteStep::Failed(id)
        }
    }
}

fn log_failure(s: &Shared, id: u32, e: &GoErr) {
    let mut msg = format!("pack {id:06}: ").into_bytes();
    msg.extend_from_slice(&e.text());
    s.log.log(&msg);
}

/// `victim()`: in the first tier that is over its (positive) budget and has a pack other than
/// the active one, the pack that sorts first by: stale first, then `used` ascending, then id.
/// Called with the lock held.
pub(crate) fn victim(s: &Shared, inner: &Inner) -> Option<u32> {
    for (t, tier) in s.tiers.iter().enumerate() {
        if tier.budget <= 0 || inner.totals[t] <= tier.budget {
            continue;
        }
        let best = inner
            .packs
            .iter()
            .filter(|(id, p)| p.tier == t && Some(**id) != inner.active)
            .min_by_key(|(id, p)| (!p.stale(), p.used(), **id));
        if let Some((id, _)) = best {
            return Some(*id);
        }
    }
    None
}

/// `moveDown(p)`: evict from the last tier, else demote to the next one.
fn move_down(s: &Shared, id: u32) -> Result<(), GoErr> {
    match begin(s, id)? {
        Begun::Evicted => Ok(()),
        Begun::Copied(p) => {
            commit(s, p);
            Ok(())
        }
    }
}

fn begin(s: &Shared, id: u32) -> Result<Begun, GoErr> {
    let Some((from, used)) = s.read().packs.get(&id).map(|p| (p.tier, p.used())) else {
        return Ok(Begun::Evicted);
    };
    if from + 1 == s.tiers.len() {
        // all under the lock: drop the index entries (a walk of the whole index, Q-STORE-12;
        // older records of these keys are not re-indexed, Q-STORE-8), the files, the pack
        let mut inner = s.write();
        inner.index.retain(|_, l| l.pack != id);
        let _ = gofs::remove(&s.name(from, id, "pack"));
        let _ = gofs::remove(&s.name(from, id, "hint"));
        if let Some(p) = inner.packs.remove(&id) {
            inner.totals[from] -= p.size;
            s.log.logf(format_args!(
                "evicted pack {id:06} ({} MiB, {} MiB live)",
                p.size >> 20,
                p.live >> 20
            ));
        }
        return Ok(Begun::Evicted);
    }
    // without the lock: reads keep coming from the old file until the switch
    for ext in ["pack", "hint"] {
        move_file(s, &s.name(from, id, ext), &s.name(from + 1, id, ext))?;
    }
    // adopted Q-STORE-7: the destination entries are durable before the source goes
    gofs::fsync_dir(&s.tiers[from + 1].dir)?;
    let dest = s.name(from + 1, id, "pack");
    let file = gofs::open_rdwr(&dest)?;
    let _ = gofs::set_mtime(&dest, used);
    Ok(Begun::Copied(Prepared {
        id,
        from,
        file,
        dest,
    }))
}

/// The locked switch: the pack now lives in the next tier; the old files go.
pub(crate) fn commit(s: &Shared, prepared: Prepared) {
    let Prepared {
        id,
        from,
        file,
        dest,
    } = prepared;
    let mut inner = s.write();
    let Some(p) = inner.packs.get_mut(&id) else {
        return;
    };
    p.file = Arc::new(file);
    p.tier = from + 1;
    p.path = dest;
    let (size, live) = (p.size, p.live);
    inner.totals[from] -= size;
    inner.totals[from + 1] += size;
    let _ = gofs::remove(&s.name(from, id, "pack"));
    let _ = gofs::remove(&s.name(from, id, "hint"));
    let mut msg = format!("demoted pack {id:06} to ").into_bytes();
    msg.extend_from_slice(&s.tiers[from + 1].dir);
    msg.extend_from_slice(format!(" ({} MiB, {} MiB live)", size >> 20, live >> 20).as_bytes());
    s.log.log(&msg);
}

/// `moveFile(from, to)`: hard-link, where an existing destination counts as success
/// (Q-STORE-10); else copy to `<to>.tmp` and rename (the source stays until the caller removes
/// it). Adopted Q-STORE-7: the copy is fsynced before the rename.
fn move_file(s: &Shared, from: &[u8], to: &[u8]) -> Result<(), GoErr> {
    if !s.force_copy {
        match gofs::link(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if gofs::is_exist(&e) => return Ok(()),
            Err(_) => {}
        }
    }
    let src = gofs::open(from)?;
    let mut tmp = to.to_vec();
    tmp.extend_from_slice(b".tmp");
    let dst = gofs::create(&tmp)?;
    let (_, res) = gofs::copy_file(&dst, &tmp, 0, &src, from, 0);
    if let Err(e) = res.and_then(|()| gofs::fsync(&dst, &tmp)) {
        drop(dst);
        let _ = gofs::remove(&tmp);
        return Err(e);
    }
    drop(dst);
    gofs::rename(&tmp, to)
}
