//! An explicit snapshot of the process environment.
//!
//! Legacy reads variables with `getenv` wherever it needs them. The Rust port captures the
//! environment once (`implementation/02-core.md §store_path`) so functions stay pure for tests and
//! the difftest harness can give both implementations the same environment. Lookups follow
//! `getenv`: the first entry with a matching name wins.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

/// `NAME=value` entries in `environ` order.
#[derive(Clone, Debug, Default)]
pub struct Env {
    entries: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Env {
    /// The current process environment, in `environ` order.
    ///
    /// `std::env::vars_os` skips entries without `=` (which only a malformed `execve` can
    /// create); every other entry round-trips exactly.
    #[must_use]
    pub fn capture() -> Self {
        Self {
            entries: std::env::vars_os()
                .map(|(k, v)| (k.into_vec(), v.into_vec()))
                .collect(),
        }
    }

    /// An environment made of the given pairs, in order (tests).
    #[must_use]
    pub fn from_pairs<K: AsRef<[u8]>, V: AsRef<[u8]>>(pairs: &[(K, V)]) -> Self {
        Self {
            entries: pairs
                .iter()
                .map(|(k, v)| (k.as_ref().to_vec(), v.as_ref().to_vec()))
                .collect(),
        }
    }

    /// `getenv(name)`: `None` when unset; `Some(b"")` when set to the empty string.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.entries
            .iter()
            .find(|(k, _)| k == name.as_bytes())
            .map(|(_, v)| v.as_slice())
    }

    /// `Env(name, fallback)` (`base.cc`): the value, or the fallback when unset.
    #[must_use]
    pub fn get_or<'a>(&'a self, name: &str, fallback: &'a [u8]) -> &'a [u8] {
        self.get(name).unwrap_or(fallback)
    }

    /// Set (and not empty).
    #[must_use]
    pub fn non_empty(&self, name: &str) -> Option<&[u8]> {
        self.get(name).filter(|v| !v.is_empty())
    }

    /// The raw `NAME=value` entries in `environ` order (`EnvironEntries` in `cc_mode.cc`).
    pub fn raw_entries(&self) -> impl Iterator<Item = Vec<u8>> + '_ {
        self.entries.iter().map(|(k, v)| {
            let mut e = Vec::with_capacity(k.len() + 1 + v.len());
            e.extend_from_slice(k);
            e.push(b'=');
            e.extend_from_slice(v);
            e
        })
    }

    /// The `(name, value)` pairs in order.
    #[must_use]
    pub fn pairs(&self) -> &[(Vec<u8>, Vec<u8>)] {
        &self.entries
    }

    /// Set a variable in the snapshot (tests and child-environment bookkeeping).
    pub fn set(&mut self, name: &str, value: impl Into<Vec<u8>>) {
        let value = value.into();
        if let Some(e) = self.entries.iter_mut().find(|(k, _)| k == name.as_bytes()) {
            e.1 = value;
        } else {
            self.entries.push((name.as_bytes().to_vec(), value));
        }
    }

    /// Remove every entry named `name`.
    pub fn remove(&mut self, name: &str) {
        self.entries.retain(|(k, _)| k != name.as_bytes());
    }

    /// As `OsString` pairs, for `Command::envs`.
    #[must_use]
    pub fn to_os_pairs(&self) -> Vec<(OsString, OsString)> {
        self.entries
            .iter()
            .map(|(k, v)| (OsString::from_vec(k.clone()), OsString::from_vec(v.clone())))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_match_wins_and_empty_is_set() {
        let e = Env::from_pairs(&[("A", "1"), ("B", ""), ("A", "2")]);
        assert_eq!(e.get("A"), Some(&b"1"[..]));
        assert_eq!(e.get("B"), Some(&b""[..]));
        assert_eq!(e.non_empty("B"), None);
        assert_eq!(e.get("C"), None);
        assert_eq!(e.get_or("C", b"x"), b"x");
        assert_eq!(
            e.raw_entries().collect::<Vec<_>>(),
            vec![b"A=1".to_vec(), b"B=".to_vec(), b"A=2".to_vec()]
        );
    }
}
