//! A2: the tree walk and `own_lib_dirs` (`behaviors/jig-reloc-fixup.md §A2`;
//! `fixup_mode.cc` `RunFixupMode`, libstdc++ `recursive_directory_iterator` with
//! `skip_permission_denied`).
//!
//! Order is depth-first pre-order with each directory's entries in `readdir(3)` order; directory
//! symlinks are not followed. Errors follow the iterator's two flavours:
//!
//! - The constructor takes an error code: if `<prefix>` cannot be opened, or its first `readdir`
//!   fails, the walk yields nothing (Q-FIX-17).
//! - Every later step is the throwing `operator++`: a subdirectory that cannot be opened, or a
//!   `readdir` that fails, aborts the process (Q-FIX-10), except with `EACCES`, which skips the
//!   directory (or ends it) silently.
//! - The loop body judges each entry with the throwing `is_symlink()` / `is_regular_file()`.
//!   They use the `readdir` type and only `lstat` when it is unknown; an `lstat` error other
//!   than not-found aborts.

use std::cmp::Ordering;
use std::fs::{DirEntry, ReadDir};
use std::os::unix::ffi::OsStrExt;

use ekacache_core::bytes::{contains, os_path};
use ekacache_core::fspath;

use super::output::abort_now;

/// The result of A2.
#[derive(Default)]
pub struct Tree {
    /// Regular, non-symlink files in walk order.
    pub files: Vec<Vec<u8>>,
    /// Directories holding a `*.so*` or `*.dylib`, deduplicated and sorted by path comparison.
    pub own_lib_dirs: Vec<Vec<u8>>,
}

// macOS's `DirEntry` holds a whole `dirent` (about 1 KiB); a `Step` lives for one `readdir`
#[allow(clippy::large_enum_variant)]
enum Step {
    Entry(DirEntry),
    End,
    Fail,
}

/// One `readdir`. `EACCES` ends the directory (`skip_permission_denied`).
fn advance(rd: &mut ReadDir) -> Step {
    match rd.next() {
        Some(Ok(e)) => Step::Entry(e),
        None => Step::End,
        Some(Err(e)) if e.raw_os_error() == Some(libc::EACCES) => Step::End,
        Some(Err(_)) => Step::Fail,
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Kind {
    File,
    Dir,
    Other,
}

/// `is_symlink()` / `is_regular_file()` of the loop body, and whether the iterator recurses.
fn kind(entry: &DirEntry) -> Kind {
    match entry.file_type() {
        Ok(t) if t.is_file() => Kind::File,
        Ok(t) if t.is_dir() => Kind::Dir,
        Ok(_) => Kind::Other,
        // an unknown d_type and a failing lstat: not-found is "neither", anything else throws
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => Kind::Other,
        // Q-FIX-10: the throwing `symlink_status` behind `is_symlink()`
        Err(_) => abort_now(),
    }
}

/// Walk `prefix` (A2).
pub fn walk(prefix: &[u8]) -> Tree {
    let mut tree = Tree::default();
    let Ok(mut top) = std::fs::read_dir(os_path(prefix)) else {
        // Q-FIX-17: a missing or unreadable prefix yields nothing, without an error
        return tree;
    };
    let mut entry = match advance(&mut top) {
        Step::Entry(e) => e,
        Step::End | Step::Fail => return tree,
    };
    let mut lib_dirs: Vec<Vec<u8>> = Vec::new();
    let mut stack: Vec<(Vec<u8>, ReadDir)> = vec![(prefix.to_vec(), top)];
    loop {
        let parent = stack.last().map(|(p, _)| p.as_slice()).unwrap_or_default();
        let path = fspath::join(parent, entry.file_name().as_bytes());
        let k = kind(&entry);
        if k == Kind::File {
            let name = fspath::filename(&path);
            let dir = fspath::parent_path(&path);
            // Q-FIX-6: substring tests
            if (contains(name, b".so") || name.ends_with(b".dylib")) && !contains(&dir, b"/debug") {
                lib_dirs.push(dir);
            }
            tree.files.push(path.clone());
        }
        if k == Kind::Dir {
            match std::fs::read_dir(os_path(&path)) {
                Ok(rd) => stack.push((path, rd)),
                Err(e) if e.raw_os_error() == Some(libc::EACCES) => {}
                // Q-FIX-10: the throwing `operator++`
                Err(_) => abort_now(),
            }
        }
        loop {
            let Some((_, rd)) = stack.last_mut() else {
                tree.own_lib_dirs = sorted_set(lib_dirs);
                return tree;
            };
            match advance(rd) {
                Step::Entry(e) => {
                    entry = e;
                    break;
                }
                Step::End => {
                    stack.pop();
                }
                // Q-FIX-10: the throwing `operator++`
                Step::Fail => abort_now(),
            }
        }
    }
}

/// `std::set<fs::path>` in iteration order: sorted by path comparison, and of several
/// equivalent spellings the first inserted is kept.
fn sorted_set(mut v: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    v.sort_by(|a, b| fspath::compare(a, b)); // stable: the first inserted stays first
    v.dedup_by(|later, first| fspath::compare(later, first) == Ordering::Equal);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_order_is_path_order() {
        let v: Vec<Vec<u8>> = [&b"/a/b-c"[..], b"/a/b/c", b"/a/b-c", b"/a//b/c", b"/a"]
            .iter()
            .map(|s| s.to_vec())
            .collect();
        let got = sorted_set(v);
        assert_eq!(
            got,
            [b"/a".to_vec(), b"/a/b/c".to_vec(), b"/a/b-c".to_vec()]
        );
    }

    #[test]
    fn walk_collects_regular_files_and_lib_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().as_os_str().as_encoded_bytes().to_vec();
        let p = |rel: &str| {
            let mut v = root.clone();
            v.push(b'/');
            v.extend_from_slice(rel.as_bytes());
            v
        };
        for d in ["lib", "lib/debug", "x/debug-y", "bin"] {
            std::fs::create_dir_all(os_path(&p(d))).unwrap();
        }
        for f in [
            "lib/libz.so.1",
            "lib/debug/libq.so",
            "x/debug-y/a.so",
            "bin/tool",
            "bin/x.sock",
        ] {
            std::fs::write(os_path(&p(f)), b"").unwrap();
        }
        std::os::unix::fs::symlink("libz.so.1", os_path(&p("lib/libz.so"))).unwrap();
        std::os::unix::fs::symlink("lib", os_path(&p("libdir"))).unwrap();
        let t = walk(&root);
        let mut files = t.files.clone();
        files.sort();
        let mut want: Vec<Vec<u8>> = [
            "lib/libz.so.1",
            "lib/debug/libq.so",
            "x/debug-y/a.so",
            "bin/tool",
            "bin/x.sock",
        ]
        .iter()
        .map(|f| p(f))
        .collect();
        want.sort();
        assert_eq!(files, want);
        // `/debug` anywhere in the parent excludes; `x.sock` contains `.so` (Q-FIX-6)
        assert_eq!(t.own_lib_dirs, [p("bin"), p("lib")]);
        assert!(walk(&p("missing")).files.is_empty());
        assert!(walk(&p("bin/tool")).files.is_empty());
    }
}
