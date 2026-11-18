//! The two-key engine over a jigd connection (`behaviors/jig-keys.md §Manifest`,
//! `manifest.cc:94-178`, `spec/twokey.qnt`).
//!
//! The manifest logic itself (k2, `!absent` lines, ADOPTED Q-KEY-3) is `jig_core::manifest`; this
//! module supplies the daemon's `IDS` answers and the `m/<k1>` lookup.

use jig_core::keys::{RequestKey, ResultKey, slot};
use jig_core::manifest::{self, EnvLookup, Manifest, Unstorable};
use jig_core::store_path::Store;

use crate::client::Client;

/// `FindResult(cache, k1)`: `GET m/<k1>`; no value (miss, I/O error, bad frame) is `new-key`,
/// otherwise `ValidateManifest` gives k2 or `inputs-changed:<path>` / `appeared:<path>`.
pub fn find_result(
    client: &mut Client,
    store: &Store,
    k1: &RequestKey,
) -> Result<ResultKey, Vec<u8>> {
    let Some(text) = client.get(&slot::manifest(k1)) else {
        return Err(b"new-key".to_vec());
    };
    manifest::validate(store, k1, &text, &mut |paths: &[&[u8]]| {
        client.identities(paths)
    })
}

/// `BuildManifest` after a compile, identities prefetched from the daemon. ADOPTED Q-KEY-3: an
/// input without an identity makes the result unstorable.
pub fn build_manifest<I: AsRef<[u8]>, A: AsRef<[u8]>>(
    client: &mut Client,
    store: &Store,
    k1: &RequestKey,
    inputs: &[I],
    primary_source: &[u8],
    absent: &[A],
) -> Result<Manifest, Unstorable> {
    manifest::build(
        store,
        k1,
        inputs,
        primary_source,
        absent,
        &mut |paths: &[&[u8]]| client.identities(paths),
    )
}

/// `PrefetchIdentities` for the front ends' toolchain and stdlib ids.
pub fn prefetch_identities<P: AsRef<[u8]>>(client: &mut Client, store: &Store, paths: &[P]) {
    manifest::prefetch_identities(store, paths, &mut |p: &[&[u8]]| client.identities(p));
}

/// `find_result` for rustc with ADOPTED Q-RS-3 env-dep entries (`behaviors/jig-rustc-mode.md
/// §Q-RS-3`): in an `rs/` manifest each `$env:<NAME>\t<id>` line is checked against `env(NAME)`
/// and a mismatch is `env-changed:<NAME>` (`manifest::validate_with_env`).
pub fn find_result_with_env(
    client: &mut Client,
    store: &Store,
    k1: &RequestKey,
    env: &EnvLookup<'_>,
) -> Result<ResultKey, Vec<u8>> {
    let Some(text) = client.get(&slot::manifest(k1)) else {
        return Err(b"new-key".to_vec());
    };
    manifest::validate_with_env(store, k1, &text, env, &mut |paths: &[&[u8]]| {
        client.identities(paths)
    })
}

/// `build_manifest` with ADOPTED Q-RS-3 env-dep lines (`manifest::env_line`), emitted after the
/// input lines and before the `!` lines (`manifest::build_with_env`).
pub fn build_manifest_with_env<I: AsRef<[u8]>, A: AsRef<[u8]>, L: AsRef<[u8]>>(
    client: &mut Client,
    store: &Store,
    k1: &RequestKey,
    inputs: &[I],
    primary_source: &[u8],
    absent: &[A],
    env_lines: &[L],
) -> Result<Manifest, Unstorable> {
    manifest::build_with_env(
        store,
        k1,
        inputs,
        primary_source,
        absent,
        env_lines,
        &mut |paths: &[&[u8]]| client.identities(paths),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use jig_core::env::Env;
    use jig_core::hasher::hash_of;
    use jig_core::keys::Tool;
    use std::io::Write;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn find_result_paths() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.h");
        std::fs::write(&a, "int a;\n").unwrap();
        let a = a.as_os_str().as_encoded_bytes().to_vec();
        let gone = dir.path().join("gone.h");
        let gone = gone.as_os_str().as_encoded_bytes().to_vec();
        let store = Store::from_env(&Env::default());
        let k1 = RequestKey::new(Tool::Cc, &hash_of(b"k1"));

        let (s, mut peer) = UnixStream::pair().unwrap();
        let mut c = Client::from_fd(OwnedFd::from(s), 1);
        let m = build_manifest(&mut c, &store, &k1, &[&a], b"", &[&gone]).unwrap();
        // a hit: the stored manifest still matches
        let wire = jig_core::frame::encode(&m.text).unwrap();
        peer.write_all(format!("OK {}\n", wire.len()).as_bytes())
            .unwrap();
        peer.write_all(&wire).unwrap();
        assert_eq!(find_result(&mut c, &store, &k1), Ok(m.result_key.clone()));
        // a miss
        peer.write_all(b"MISS\n").unwrap();
        assert_eq!(find_result(&mut c, &store, &k1), Err(b"new-key".to_vec()));
        // the absent path appeared
        std::fs::write(os(&gone), "").unwrap();
        peer.write_all(format!("OK {}\n", wire.len()).as_bytes())
            .unwrap();
        peer.write_all(&wire).unwrap();
        let mut why = b"appeared:".to_vec();
        why.extend_from_slice(&gone);
        assert_eq!(find_result(&mut c, &store, &k1), Err(why));
        // an input changed
        std::fs::remove_file(os(&gone)).unwrap();
        std::fs::write(os(&a), "int b;\n").unwrap();
        peer.write_all(format!("OK {}\n", wire.len()).as_bytes())
            .unwrap();
        peer.write_all(&wire).unwrap();
        let mut why = b"inputs-changed:".to_vec();
        why.extend_from_slice(&a);
        assert_eq!(find_result(&mut c, &store, &k1), Err(why));
        // ADOPTED Q-KEY-3: an unreadable input is not storable
        std::fs::remove_file(os(&a)).unwrap();
        assert_eq!(
            build_manifest(&mut c, &store, &k1, &[&a], b"", &[] as &[&[u8]]),
            Err(Unstorable::Unreadable(store.key(&a)))
        );
    }

    fn os(b: &[u8]) -> &std::path::Path {
        jig_core::bytes::os_path(b)
    }

    #[test]
    fn env_dep_wrappers() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("lib.rs");
        std::fs::write(&a, "mod m;\n").unwrap();
        let a = a.as_os_str().as_encoded_bytes().to_vec();
        let store = Store::from_env(&Env::default());
        let k1 = RequestKey::new(Tool::Rustc, &hash_of(b"k1"));
        let (s, mut peer) = UnixStream::pair().unwrap();
        let mut c = Client::from_fd(OwnedFd::from(s), 1);
        let lines = [manifest::env_line(b"JIG_T", Some(b"1"))];
        let m = build_manifest_with_env(&mut c, &store, &k1, &[&a], b"", &[] as &[&[u8]], &lines)
            .unwrap();
        let mut want = store.key(&a);
        want.push(b'\t');
        want.extend_from_slice(&store.input_id(&a).unwrap());
        want.push(b'\n');
        want.extend_from_slice(&lines[0]);
        want.push(b'\n');
        assert_eq!(m.text, want);
        let wire = jig_core::frame::encode(&m.text).unwrap();
        let mut reply = || {
            peer.write_all(format!("OK {}\n", wire.len()).as_bytes())
                .unwrap();
            peer.write_all(&wire).unwrap();
        };
        reply();
        let same = |n: &[u8]| (n == b"JIG_T").then(|| b"1".to_vec());
        assert_eq!(
            find_result_with_env(&mut c, &store, &k1, &same),
            Ok(m.result_key.clone())
        );
        reply();
        let changed = |n: &[u8]| (n == b"JIG_T").then(|| b"2".to_vec());
        assert_eq!(
            find_result_with_env(&mut c, &store, &k1, &changed),
            Err(b"env-changed:JIG_T".to_vec())
        );
        reply();
        assert_eq!(
            find_result_with_env(&mut c, &store, &k1, &|_: &[u8]| None),
            Err(b"env-changed:JIG_T".to_vec())
        );
        // the plain find_result does not interpret `$env:` lines: an ordinary input line
        reply();
        assert_eq!(
            find_result(&mut c, &store, &k1),
            Err(b"inputs-changed:$env:JIG_T".to_vec())
        );
    }
}
