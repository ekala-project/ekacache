//! The `std::filesystem` calls reloc-fixup makes that touch the disk, with libstdc++'s error
//! semantics (`behaviors/jig-reloc-fixup.md §Conventions` `rel(p)`, A5.5; Q-FIX-10).
//!
//! - `fs::exists(p)` (throwing) is `true` for an existing path, `false` for `ENOENT`/`ENOTDIR`,
//!   and throws, so the process aborts, for any other `stat` error (`EACCES`, `ELOOP`,
//!   `ENAMETOOLONG`, …).
//! - `fs::relative(p, base)` is `weakly_canonical(p).lexically_relative(weakly_canonical(base))`
//!   and throws (aborts) when either canonicalisation fails.
//!
//! These differ from the lexical operations in `jig_core::fspath` in that they need the file
//! system; nothing else in jig needs `weakly_canonical` yet, so they live here.

use jig_core::bytes::os_path;
use jig_core::fspath;

use super::output::abort_now;

/// `fs::status(p, ec)` reduced to what `exists` needs: `Ok(exists)` when the status is known,
/// `Err(())` otherwise. `EOVERFLOW` gives `file_type::unknown`, which is known and exists.
fn status(p: &[u8]) -> Result<bool, ()> {
    match std::fs::metadata(os_path(p)) {
        Ok(_) => Ok(true),
        Err(e) => match e.raw_os_error() {
            Some(libc::ENOENT | libc::ENOTDIR) => Ok(false),
            Some(libc::EOVERFLOW) => Ok(true),
            _ => Err(()),
        },
    }
}

/// The throwing `fs::exists(p)`: an unknown status aborts (Q-FIX-10).
pub fn exists_or_abort(p: &[u8]) -> bool {
    match status(p) {
        Ok(e) => e,
        // Q-FIX-10: `fs::exists` without an error code throws on EACCES/ELOOP/…
        Err(()) => abort_now(),
    }
}

/// `fs::canonical(p, ec)` (libstdc++ with `realpath`): `realpath(absolute(p))`. libstdc++ falls
/// back to resolving by hand when `realpath` fails with `ENAMETOOLONG`; that fallback is not
/// reproduced (the path is then treated as an error).
fn canonical(p: &[u8]) -> Option<Vec<u8>> {
    let cwd = if fspath::is_absolute(p) {
        Vec::new()
    } else {
        std::env::current_dir()
            .ok()?
            .into_os_string()
            .into_encoded_bytes()
    };
    let pa = fspath::absolute_in(p, &cwd)?;
    std::fs::canonicalize(os_path(&pa))
        .ok()
        .map(|r| r.into_os_string().into_encoded_bytes())
}

/// `fs::weakly_canonical(p, ec)` (libstdc++ `fs_ops.cc`): the canonical form of the longest
/// leading part of `p` that exists, the remaining elements appended, lexically normalised.
/// `None` where libstdc++ sets the error code.
pub fn weakly_canonical(p: &[u8]) -> Option<Vec<u8>> {
    if status(p).ok()? {
        return canonical(p);
    }
    let els = fspath::elements(p);
    let mut result: Vec<u8> = Vec::new();
    let mut rest = els.as_slice();
    while let Some((first, tail)) = rest.split_first() {
        let tmp = fspath::join(&result, first);
        if !status(&tmp).ok()? {
            break;
        }
        result = tmp;
        rest = tail;
    }
    if !result.is_empty() {
        result = canonical(&result)?;
    }
    for e in rest {
        result = fspath::join(&result, e);
    }
    Some(fspath::normalize(&result))
}

/// The throwing `fs::relative(p, base)`; an error aborts (Q-FIX-10). This is `rel(p)` of the
/// spec.
pub fn relative_or_abort(p: &[u8], base: &[u8]) -> Vec<u8> {
    // Q-FIX-10: `fs::relative` throws when `weakly_canonical` fails
    let Some(a) = weakly_canonical(p) else {
        abort_now()
    };
    let Some(b) = weakly_canonical(base) else {
        abort_now()
    };
    fspath::relative(&a, &b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(p: &std::path::Path) -> Vec<u8> {
        p.as_os_str().as_encoded_bytes().to_vec()
    }

    #[test]
    fn weakly_canonical_resolves_the_existing_part() {
        let dir = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir(real.join("d")).unwrap();
        std::os::unix::fs::symlink(real.join("d"), real.join("l")).unwrap();
        let mut r = b(&real);
        r.extend_from_slice(b"/d");
        let mut l = b(&real);
        l.extend_from_slice(b"/l");
        assert_eq!(weakly_canonical(&l).unwrap(), r);
        let mut l_missing = l.clone();
        l_missing.extend_from_slice(b"/x/../y/");
        let mut want = r.clone();
        want.extend_from_slice(b"/y/");
        assert_eq!(weakly_canonical(&l_missing).unwrap(), want);
        assert_eq!(weakly_canonical(b"").unwrap(), b"");
        let mut f = b(&real);
        f.extend_from_slice(b"/d/f");
        std::fs::write(os_path(&f), b"").unwrap();
        assert_eq!(relative_or_abort(&f, &l), b"f");
        assert!(exists_or_abort(&f));
        let mut under_file = f.clone();
        under_file.extend_from_slice(b"/x");
        assert!(!exists_or_abort(&under_file)); // ENOTDIR
    }
}
