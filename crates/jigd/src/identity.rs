//! File identities for `IDS` (`behaviors/jigd-identities.md` §The identity value,
//! §Configuration, §Algorithm, §The memo; `implementation/03-jigd-server.md` §Identities).
//!
//! An identity is `C:` + the first 128 bits of BLAKE3 over the file, in lowercase hex, for a
//! regular file that really lies under the store, and `""` for anything else. Errors are never
//! logged and never memoised. The memo is keyed by the path as asked, guarded by
//! (inode, size, mtime ns) of the opened file, and never pruned (Q-ID-1 kept).
//!
//! Q-ID-2: the file is hashed as a stream, not read into memory first. The bytes hashed (exactly
//! `st_size` of them, a shorter file gives `""`) and the result are the same as Go's.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::sync::{PoisonError, RwLock};

use crate::gofs;
use crate::gopath;

/// `C:` and 32 hex digits.
pub const ID_LEN: usize = 34;

/// What guards a memo entry (Q-ID-3: no device, no ctime).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    ino: u64,
    size: i64,
    mtime_ns: i64,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    stamp: Stamp,
    id: [u8; ID_LEN],
}

/// `Identities`: the store prefixes and the memo.
#[derive(Debug)]
pub struct Identities {
    /// `Clean(storeDir) + "/"`
    store: Vec<u8>,
    /// `Clean(EvalSymlinks(storeDir)) + "/"`, or `Clean(storeDir) + "/"` if that failed;
    /// resolved once (Q-ID-5)
    real_store: Vec<u8>,
    known: RwLock<HashMap<Vec<u8>, Entry>>,
}

impl Identities {
    /// `NewIdentities(storeDir)`.
    pub fn new(store_dir: &[u8]) -> Identities {
        let real = gofs::eval_symlinks(store_dir).unwrap_or_else(|| store_dir.to_vec());
        let mut store = gopath::clean(store_dir);
        store.push(b'/');
        let mut real_store = gopath::clean(&real);
        real_store.push(b'/');
        Identities {
            store,
            real_store,
            known: RwLock::new(HashMap::new()),
        }
    }

    /// The configured prefix, with its trailing slash.
    pub fn store(&self) -> &[u8] {
        &self.store
    }

    /// `Of(path)`: the identity of a regular file under the store, `""` otherwise.
    pub fn of(&self, path: &[u8]) -> Vec<u8> {
        self.lookup(path).map(|id| id.to_vec()).unwrap_or_default()
    }

    fn lookup(&self, path: &[u8]) -> Option<[u8; ID_LEN]> {
        if !path.starts_with(&self.store) || gopath::clean(path) != path {
            return None;
        }
        // store trees hold symlinks, also ones a hostile build pointed out of the store
        let real = gofs::eval_symlinks(path)?;
        if !real.starts_with(&self.real_store) {
            return None;
        }
        // type and location are checked on the opened descriptor: a build owns directories
        // under its $out and can swap one for a symlink between any two path lookups
        let f = File::open(gofs::nul_free(&real)?).ok()?;
        let meta = f.metadata().ok()?;
        if !meta.file_type().is_file() || !fd_under(&f, &self.real_store) {
            return None;
        }
        let stamp = Stamp {
            ino: meta.ino(),
            size: meta.size() as i64,
            mtime_ns: meta
                .mtime()
                .wrapping_mul(1_000_000_000)
                .wrapping_add(meta.mtime_nsec()),
        };
        let memo = self
            .known
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(path)
            .copied();
        if let Some(e) = memo
            && e.stamp == stamp
        {
            return Some(e.id);
        }
        // exactly st_size bytes: a file that grew is hashed up to the stat size, one that
        // shrank reads short and has no identity
        let want = u64::try_from(stamp.size).ok()?;
        let (digest, n) = jig_core::hasher::hash_reader(&mut (&f).take(want)).ok()?;
        if n != want {
            return None;
        }
        let mut id = [0u8; ID_LEN];
        id[..2].copy_from_slice(b"C:");
        id[2..].copy_from_slice(&digest.to_hex());
        // Q-ID-4: concurrent misses both hash, the last write wins
        self.known
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path.to_vec(), Entry { stamp, id });
        Some(id)
    }

    /// `Len()`: memo entries, reported as `ids=` in STATS.
    pub fn len(&self) -> usize {
        self.known
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// `fdUnder`: by the kernel's account (`/proc/self/fd/N`), the open file lives under `dir`
/// (which has a trailing slash).
#[cfg(target_os = "linux")]
fn fd_under(f: &File, dir: &[u8]) -> bool {
    use std::os::fd::AsRawFd;
    match std::fs::read_link(format!("/proc/self/fd/{}", f.as_raw_fd())) {
        Ok(at) => at.as_os_str().as_encoded_bytes().starts_with(dir),
        Err(_) => false,
    }
}

/// `peer_other.go`: no descriptor check off Linux (Q-DAEMON-8).
#[cfg(not(target_os = "linux"))]
fn fd_under(_f: &File, _dir: &[u8]) -> bool {
    true
}
