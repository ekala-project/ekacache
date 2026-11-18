//! The offline store (`behaviors/jig-nix-store.md §Modes of operation`): with
//! `JIG_NIX_STORE_OFFLINE` non-empty no socket is opened, text paths are computed locally under
//! `STORE_DIR`, and `submit` succeeds without doing anything.

use jig_core::store_path::STORE_DIR;

use super::hash::text_path;

/// `TextPath(name, refs, contents)` under the build-time store directory.
#[must_use]
pub fn add_text<R: AsRef<[u8]>>(name: &[u8], refs: &[R], contents: &[u8]) -> Vec<u8> {
    text_path(STORE_DIR, name, refs, contents)
}

/// `submit` checks nothing and succeeds (Q-NIX-14).
#[must_use]
pub fn submit() -> bool {
    // Q-NIX-14: offline submit always succeeds
    true
}
