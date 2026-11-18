//! Key types and server-side prefixes (`behaviors/jig-keys.md §Key types and prefixes`), and
//! `KeyEnvironment` (`behaviors/jig-keys.md §k1 field sequences`, `cc_mode.cc:556-580`).
//!
//! The functions in `slot` are the **only** place the prefixes `m/ o/ e/ d/ r/ b/ go/a/ go/o/`
//! and `rs/` are spelled.

use crate::hasher::Digest;

/// Which front end produced a key. Keeps C and rustc entries apart in one server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Cc,
    Rustc,
}

/// k1: what the build system asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestKey {
    text: Vec<u8>,
}

impl RequestKey {
    /// `<hex>` for cc, `rs/<hex>` for rustc (`keys.h:35`).
    #[must_use]
    pub fn new(tool: Tool, digest: &Digest) -> Self {
        let mut text = Vec::with_capacity(35);
        if tool == Tool::Rustc {
            text.extend_from_slice(b"rs/");
        }
        text.extend_from_slice(&digest.to_hex());
        Self { text }
    }

    #[must_use]
    pub fn text(&self) -> &[u8] {
        &self.text
    }
}

/// k2: k1 plus the manifest. Always the bare hex.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultKey {
    text: Vec<u8>,
}

impl ResultKey {
    #[must_use]
    pub fn new(digest: &Digest) -> Self {
        Self { text: digest.hex() }
    }

    #[must_use]
    pub fn text(&self) -> &[u8] {
        &self.text
    }
}

/// Server-side key strings.
pub mod slot {
    use super::{RequestKey, ResultKey};

    fn prefixed(prefix: &[u8], rest: &[u8]) -> Vec<u8> {
        let mut k = Vec::with_capacity(prefix.len() + rest.len());
        k.extend_from_slice(prefix);
        k.extend_from_slice(rest);
        k
    }

    /// `m/<k1-text>`: the manifest.
    #[must_use]
    pub fn manifest(k1: &RequestKey) -> Vec<u8> {
        prefixed(b"m/", k1.text())
    }
    /// `o/<k2>`: the object (cc) or the packed outputs (rustc).
    #[must_use]
    pub fn object(k2: &ResultKey) -> Vec<u8> {
        prefixed(b"o/", k2.text())
    }
    /// `e/<k2>`: captured stderr.
    #[must_use]
    pub fn stderr(k2: &ResultKey) -> Vec<u8> {
        prefixed(b"e/", k2.text())
    }
    /// `d/<k2>`: the depfile.
    #[must_use]
    pub fn depfile(k2: &ResultKey) -> Vec<u8> {
        prefixed(b"d/", k2.text())
    }
    /// `r/<k2>`: the exit status of a cached failure.
    #[must_use]
    pub fn exit_status(k2: &ResultKey) -> Vec<u8> {
        prefixed(b"r/", k2.text())
    }
    /// `b/<hex>`: one `put-dir` file blob.
    #[must_use]
    pub fn blob(hex: &[u8]) -> Vec<u8> {
        prefixed(b"b/", hex)
    }
    /// `go/a/<hex>`: Go action id → output id.
    #[must_use]
    pub fn go_action(hex: &[u8]) -> Vec<u8> {
        prefixed(b"go/a/", hex)
    }
    /// `go/o/<hex>`: Go output id → bytes.
    #[must_use]
    pub fn go_output(hex: &[u8]) -> Vec<u8> {
        prefixed(b"go/o/", hex)
    }
}

/// Names `KeyEnvironment` always keeps.
const COMPILER_VARS: [&[u8]; 6] = [
    b"CPATH",
    b"C_INCLUDE_PATH",
    b"CPLUS_INCLUDE_PATH",
    b"OBJC_INCLUDE_PATH",
    b"GCC_EXEC_PREFIX",
    b"COMPILER_PATH",
];

/// `NIX_*` names that differ between two runs of one build, or say nothing about its outputs.
const VOLATILE: [&[u8]; 10] = [
    b"NIX_BUILD_TOP",
    b"NIX_BUILD_CORES",
    b"NIX_LOG_FD",
    b"NIX_ATTRS_JSON_FILE",
    b"NIX_ATTRS_SH_FILE",
    b"NIX_SSL_CERT_FILE",
    b"NIX_REMOTE",
    b"NIX_PATH",
    b"NIX_PROFILES",
    b"NIX_USER_PROFILE_DIR",
];

/// `KeyEnvironment(entries, links)`: keep the compiler variables, and for links every `NIX_*`
/// that is not volatile; sort the whole `NAME=value` entries bytewise; join each followed by
/// `\n`.
#[must_use]
pub fn key_environment<I, E>(environ_entries: I, links: bool) -> Vec<u8>
where
    I: IntoIterator<Item = E>,
    E: AsRef<[u8]>,
{
    let mut kept: Vec<Vec<u8>> = Vec::new();
    for entry in environ_entries {
        let entry = entry.as_ref();
        let name = entry
            .iter()
            .position(|&c| c == b'=')
            .map_or(entry, |i| &entry[..i]);
        let wrapper = links && name.starts_with(b"NIX_") && !VOLATILE.contains(&name);
        if wrapper || COMPILER_VARS.contains(&name) {
            kept.push(entry.to_vec());
        }
    }
    kept.sort();
    let mut out = Vec::new();
    for e in kept {
        out.extend_from_slice(&e);
        out.push(b'\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hasher::Hasher;

    #[test]
    fn prefixes() {
        let d = Hasher::new().finish();
        let k1 = RequestKey::new(Tool::Rustc, &d);
        assert_eq!(k1.text(), b"rs/af1349b9f5f9a1a6a0404dea36dcc949");
        assert_eq!(
            slot::manifest(&k1),
            b"m/rs/af1349b9f5f9a1a6a0404dea36dcc949"
        );
        let k2 = ResultKey::new(&d);
        assert_eq!(slot::object(&k2), b"o/af1349b9f5f9a1a6a0404dea36dcc949");
        assert_eq!(
            slot::exit_status(&k2),
            b"r/af1349b9f5f9a1a6a0404dea36dcc949"
        );
        assert_eq!(slot::go_action(b"ab"), b"go/a/ab");
    }

    #[test]
    fn key_environment_sorting() {
        let env = [
            "NIX_LDFLAGS_X=2",
            "PATH=/bin",
            "NIX_LDFLAGS=1",
            "C_INCLUDE_PATH=/c",
            "CPATH=/p",
            "NIX_BUILD_TOP=/build",
        ];
        assert_eq!(
            key_environment(env, false),
            b"CPATH=/p\nC_INCLUDE_PATH=/c\n"
        );
        assert_eq!(
            key_environment(env, true),
            b"CPATH=/p\nC_INCLUDE_PATH=/c\nNIX_LDFLAGS=1\nNIX_LDFLAGS_X=2\n"
        );
        assert_eq!(key_environment(["X=1"], true), b"");
    }
}
