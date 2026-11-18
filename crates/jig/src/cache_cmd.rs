//! `jig cache …` and `jig slot …` (`behaviors/jig-cache-commands.md`, `main.cc:37-74`,
//! `dir_mode.cc`). Phase P4.
//!
//! - `jig cache get|put <key> <file>`: plain blobs (§jig cache).
//! - `jig cache put-dir|get-dir <key> <dir>`: a tree as `b/<hash>` blobs plus a listing
//!   (§Directory trees).
//! - `jig slot <program> [args…]` (§jig slot).
//!
//! Adopted here: Q-CLIENT-2 (`jig cache put` exits 1 when the PUT is not acknowledged, and every
//! failed PUT is reported as `jig: cache put <key> failed`), Q-DIR-1 / Q-KEY-1 (path mode stores
//! and restores text unchanged: `Store::stored` / `Store::restored`). Everything else is
//! reproduced, including Q-DIR-2 (readdir order), Q-DIR-3 (double read), Q-DIR-4 (aborts),
//! Q-DIR-5 (no path sanitising), Q-DIR-6 and Q-DIR-7.

use std::fs::{FileType, ReadDir};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use jig_core::bytes::{os_bytes, os_path, parse_uint_octal, split, split_whitespace};
use jig_core::fspath;
use jig_core::hasher::hash_of;
use jig_core::store_path::Store;

use crate::client::Client;
use crate::process::{self, Capture};
use crate::{Ctx, log, slot as slot_guard, sys, util};

/// Files per pipelined `HAS`/`PUT` or `GET` round (`kBatch`).
const BATCH: usize = 256;
/// The mode of a listing line whose mode field does not parse (`kDefaultMode`).
const DEFAULT_MODE: u64 = 0o644;
/// The permission bits a listing records (`kPermBits`): setuid, setgid and sticky are dropped.
const PERM_BITS: u32 = 0o777;
const SLOT_USAGE: &[u8] = b"usage: jig slot <program> [args...]\n";

/// `RunCacheMode(args, socket)` (§jig cache): exactly three arguments.
pub fn run(args: &[Vec<u8>], ctx: &mut Ctx) -> i32 {
    // Q-DIR-6 kept: a wrong argument count exits 1 without a message
    let [verb, key, path] = args else {
        return 1;
    };
    match verb.as_slice() {
        b"put-dir" => return put_dir(&ctx.socket, &ctx.store, key, path),
        b"get-dir" => return get_dir(&ctx.socket, &ctx.store, key, path),
        _ => {}
    }
    let mut client = Client::new();
    if !client.connect(&ctx.socket) {
        return 2;
    }
    if verb == b"put" {
        let Some(data) = util::read_file(path) else {
            return 1;
        };
        if client.put(key, &data) {
            return 0;
        }
        // ADOPTED Q-CLIENT-2: a refused or unanswered PUT is reported and exits 1
        log::put_failed(key);
        return 1;
    }
    // Q-DIR-6 kept: any other verb, typos included, is `get`
    match client.get(key) {
        Some(blob) if util::write_file(path, &blob) => 0,
        _ => 1,
    }
}

/// `RunSlotMode(args, socket)` (§jig slot): run the program holding a jigd job slot.
pub fn slot(args: &[Vec<u8>], ctx: &mut Ctx) -> i32 {
    let Some((program, rest)) = args.split_first() else {
        let _ = std::io::stderr().write_all(SLOT_USAGE);
        return 2;
    };
    let mut client = Client::new();
    slot_guard::hold(&mut client, &ctx.socket, &mut ctx.env, |_, env| {
        // Q-DIR-7 kept: a failed exec is the child's silent `_exit(127)`
        process::run(program, rest, Capture::Inherit, env).status
    })
}

fn blob_key(hash_hex: &[u8]) -> Vec<u8> {
    let mut k = b"b/".to_vec();
    k.extend_from_slice(hash_hex);
    k
}

/// Q-DIR-4 kept: where legacy's throwing `std::filesystem` calls terminate the process. The
/// libstdc++ `terminate called after throwing…` text is not reproduced (flagged in
/// `implementation/13-rollout.md`).
fn filesystem_error() -> ! {
    std::process::abort()
}

/// An entry of the walk: its path (`<dir>/…`, built with `operator/`) and its `readdir` type.
struct Dent {
    path: Vec<u8>,
    kind: FileType,
}

/// `recursive_directory_iterator(root)` with no options (§put-dir step 2): pre-order, each
/// directory's entries in `readdir` order (Q-DIR-2 kept: not sorted), symlinks to directories
/// listed but not descended into. Like libstdc++'s `increment`, a subdirectory is opened when the
/// walk moves past its entry. Failing to open or read any directory, the root included, aborts
/// (Q-DIR-4 kept).
struct Walk {
    stack: Vec<(Vec<u8>, ReadDir)>,
    descend: Option<Vec<u8>>,
}

impl Walk {
    fn new(root: &[u8]) -> Self {
        let Ok(top) = std::fs::read_dir(os_path(root)) else {
            filesystem_error(); // Q-DIR-4
        };
        Self {
            stack: vec![(root.to_vec(), top)],
            descend: None,
        }
    }

    fn next(&mut self) -> Option<Dent> {
        if let Some(dir) = self.descend.take() {
            let Ok(sub) = std::fs::read_dir(os_path(&dir)) else {
                filesystem_error(); // Q-DIR-4
            };
            self.stack.push((dir, sub));
        }
        loop {
            let (dir, entries) = self.stack.last_mut()?;
            match entries.next() {
                None => {
                    self.stack.pop();
                }
                Some(Err(_)) => filesystem_error(), // Q-DIR-4
                Some(Ok(entry)) => {
                    let path = fspath::join(dir, os_bytes(&entry.file_name()));
                    // `d_type`, or `lstat` when the filesystem does not report it
                    let Ok(kind) = entry.file_type() else {
                        filesystem_error(); // Q-DIR-4
                    };
                    if kind.is_dir() {
                        self.descend = Some(path.clone());
                    }
                    return Some(Dent { path, kind });
                }
            }
        }
    }
}

/// `PutDir(socket, key, dir)` (§put-dir).
fn put_dir(socket: &[u8], store: &Store, key: &[u8], dir: &[u8]) -> i32 {
    let mut client = Client::new();
    if !client.connect(socket) {
        return 2;
    }
    let mut up = Uploader {
        client: &mut client,
        store,
        uploaded: 0,
        reported: false,
    };
    let mut listing = Vec::new();
    let mut pending: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut files: usize = 0;
    let mut walk = Walk::new(dir);
    while let Some(dent) = walk.next() {
        let rel = fspath::relative(&dent.path, dir);
        if dent.kind.is_symlink() {
            let Ok(target) = std::fs::read_link(os_path(&dent.path)) else {
                filesystem_error(); // Q-DIR-4
            };
            listing.extend_from_slice(b"l 0 ");
            listing.extend_from_slice(os_bytes(target.as_os_str()));
            listing.push(b'\t');
            listing.extend_from_slice(&rel);
            listing.push(b'\n');
            continue;
        }
        if !dent.kind.is_file() {
            continue;
        }
        let Some(data) = util::read_file(&dent.path) else {
            continue;
        };
        // `stat` with its result ignored: a zeroed `struct stat` gives mode 0
        let mode = std::fs::metadata(os_path(&dent.path)).map_or(0, |m| m.mode() & PERM_BITS);
        let hash = hash_of(&store.stored(&data)).hex();
        listing.extend_from_slice(format!("f {mode:o} ").as_bytes());
        listing.extend_from_slice(&hash);
        listing.push(b'\t');
        listing.extend_from_slice(&rel);
        listing.push(b'\n');
        pending.push((blob_key(&hash), dent.path));
        files += 1;
        if pending.len() == BATCH {
            up.upload(&pending);
            pending.clear();
        }
    }
    up.upload(&pending);
    let uploaded = up.uploaded;
    if !client.put(key, &listing) {
        log::put_failed(key); // ADOPTED Q-CLIENT-2
    }
    let mut msg = b"jig: put-dir ".to_vec();
    msg.extend_from_slice(key);
    msg.extend_from_slice(format!(": {files} files, {uploaded} new\n").as_bytes());
    let _ = std::io::stderr().write_all(&msg);
    0
}

/// `Upload(cache, pending)` (§put-dir): the blobs the daemon lacks.
struct Uploader<'a> {
    client: &'a mut Client,
    store: &'a Store,
    /// PUTs sent, the `new` of the summary (duplicates count twice).
    uploaded: usize,
    /// Whether a failed blob PUT was already reported: the connection is dead after the first.
    reported: bool,
}

impl Uploader<'_> {
    fn upload(&mut self, pending: &[(Vec<u8>, Vec<u8>)]) {
        let keys: Vec<&[u8]> = pending.iter().map(|(k, _)| k.as_slice()).collect();
        let have = self.client.has_many(&keys);
        let mut puts: Vec<(&[u8], Vec<u8>)> = Vec::new();
        for ((blob_key, file), have) in pending.iter().zip(have) {
            if have {
                continue;
            }
            // Q-DIR-3 kept: the file is read again; the listing's hash is from the first read
            if let Some(data) = util::read_file(file) {
                puts.push((blob_key, self.store.stored(&data).into_owned()));
            }
        }
        let stored = self.client.put_many(&puts);
        if let Some((failed, _)) = puts.get(stored)
            && !self.reported
        {
            log::put_failed(failed); // ADOPTED Q-CLIENT-2
            self.reported = true;
        }
        self.uploaded += puts.len();
    }
}

/// One parsed listing line (`Entry`).
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    kind: u8,
    mode: u64,
    /// The blob hash (`f`) or the link target (`l`).
    reference: Vec<u8>,
    rel: Vec<u8>,
}

/// `ParseListing(text)` (§get-dir step 3): lines without a tab, or whose part before the first
/// tab is not three words, are skipped; a mode that is not octal is `0644`.
fn parse_listing(text: &[u8]) -> Vec<Entry> {
    split(text, b'\n', false)
        .into_iter()
        .filter_map(|line| {
            let tab = line.iter().position(|&c| c == b'\t')?;
            let [kind, mode, reference] = split_whitespace(&line[..tab])[..] else {
                return None;
            };
            Some(Entry {
                kind: *kind.first()?,
                mode: parse_uint_octal(mode).unwrap_or(DEFAULT_MODE),
                reference: reference.to_vec(),
                rel: line[tab + 1..].to_vec(),
            })
        })
        .collect()
}

/// `create_directories(p, ignored)`.
fn create_directories(p: &[u8]) {
    let _ = std::fs::create_dir_all(os_path(p));
}

/// `chmod(path, static_cast<unsigned>(mode))`, result ignored: all 12 low bits as parsed.
fn chmod(path: &[u8], mode: u64) {
    #[allow(clippy::cast_possible_truncation)] // `static_cast<unsigned>`
    let mode = mode as u32;
    let _ = std::fs::set_permissions(os_path(path), std::fs::Permissions::from_mode(mode));
}

/// `GetDir(socket, key, dir)` (§get-dir).
fn get_dir(socket: &[u8], store: &Store, key: &[u8], dir: &[u8]) -> i32 {
    let mut client = Client::new();
    if !client.connect(socket) {
        return 2;
    }
    let Some(listing) = client.get(key) else {
        return 1;
    };
    let mut entries = parse_listing(&listing);
    // blobs are evicted independently of listings: a partial tree must not pass for a warm one
    let mut written: Vec<Vec<u8>> = Vec::new();
    for entry in entries.iter().filter(|e| e.kind == b'l') {
        // Q-DIR-5 kept: `root / rel`, so an absolute `rel` replaces the root, `..` is followed
        let dest = fspath::join(dir, &entry.rel);
        create_directories(&fspath::parent_path(&dest));
        sys::remove(&dest);
        let _ = std::os::unix::fs::symlink(os_path(&entry.reference), os_path(&dest));
        written.push(dest); // recorded even when creating it failed
    }
    entries.retain(|e| e.kind == b'f');
    let mut missing: usize = 0;
    for batch in entries.chunks(BATCH) {
        let keys: Vec<Vec<u8>> = batch.iter().map(|e| blob_key(&e.reference)).collect();
        let blobs = client.get_many(&keys);
        for (entry, blob) in batch.iter().zip(blobs) {
            let dest = fspath::join(dir, &entry.rel);
            create_directories(&fspath::parent_path(&dest));
            match blob {
                Some(blob) if util::write_file(&dest, &store.restored(&blob)) => {
                    chmod(&dest, entry.mode);
                    written.push(dest);
                }
                _ => missing += 1,
            }
        }
    }
    let mut msg = b"jig: get-dir ".to_vec();
    msg.extend_from_slice(key);
    msg.extend_from_slice(format!(": {} files, {missing} missing\n", entries.len()).as_bytes());
    let _ = std::io::stderr().write_all(&msg);
    if missing != 0 {
        // a pre-existing file that was overwritten is deleted, not restored; directories stay
        for path in &written {
            sys::remove(path);
        }
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: u8, mode: u64, reference: &str, rel: &str) -> Entry {
        Entry {
            kind,
            mode,
            reference: reference.into(),
            rel: rel.into(),
        }
    }

    #[test]
    fn listing_parse() {
        let text = b"f 755 abc\tbin/tool\n\nl 0 /nix/store/x-y/lib\tlib\n\
            f 9z9 h\tbad mode\nf 4755 h2\t a\tb \nno tab here\nf 644\tshort\n\
            f 1 2 3\tfour words\nd 700 x\tdir\n  l\t0\tt\tsp\n";
        assert_eq!(
            parse_listing(text),
            vec![
                entry(b'f', 0o755, "abc", "bin/tool"),
                entry(b'l', 0, "/nix/store/x-y/lib", "lib"),
                entry(b'f', 0o644, "h", "bad mode"),
                entry(b'f', 0o4755, "h2", " a\tb "),
                entry(b'd', 0o700, "x", "dir"),
                // `  l\t0\tt\tsp`: one word before the first tab, skipped
            ]
        );
        assert!(parse_listing(b"").is_empty());
    }

    #[test]
    fn walk_is_preorder_and_skips_dir_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/b/f"), "").unwrap();
        std::fs::write(root.join("top"), "").unwrap();
        std::os::unix::fs::symlink("a", root.join("link")).unwrap();
        let root_b = format!("{}/", root.display()).into_bytes();
        let mut walk = Walk::new(&root_b);
        let mut seen = Vec::new();
        while let Some(d) = walk.next() {
            let rel = String::from_utf8(fspath::relative(&d.path, &root_b)).unwrap();
            seen.push((rel, d.kind.is_dir(), d.kind.is_symlink()));
        }
        let pos = |name: &str| seen.iter().position(|(r, ..)| r == name).unwrap();
        assert_eq!(seen.len(), 5, "{seen:?}");
        assert!(pos("a") < pos("a/b") && pos("a/b") < pos("a/b/f"));
        assert_eq!(pos("a/b") + 1, pos("a/b/f"));
        assert!(seen[pos("link")].2);
        // the root's trailing slash does not double up
        assert!(seen.iter().all(|(r, ..)| !r.starts_with('/')));
    }

    #[test]
    fn join_semantics_of_rel() {
        // Q-DIR-5 kept: `root / rel` with an absolute rel discards the root
        assert_eq!(fspath::join(b"/out", b"/etc/x"), b"/etc/x");
        assert_eq!(fspath::join(b"/out", b"../x"), b"/out/../x");
        assert_eq!(fspath::join(b"out/", b"a/b"), b"out/a/b");
    }

    #[test]
    fn chmod_applies_all_twelve_bits() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, "").unwrap();
        let pb = p.as_os_str().as_encoded_bytes();
        chmod(pb, 0o1640);
        let m = std::fs::metadata(&p).unwrap().mode();
        assert_eq!(m & 0o7777, 0o1640);
        // bits above the twelve are truncated to `unsigned` and then ignored by the kernel
        chmod(pb, 0o600 | (1 << 40));
        assert_eq!(std::fs::metadata(&p).unwrap().mode() & 0o7777, 0o600);
    }
}
