//! `IDS` identities (`behaviors/jigd-identities.md`): `legacy/jigd/identity_test.go` ported, and
//! the edges of the algorithm (store prefix, `Clean`, symlinks, file types, the memo).

// helpers outside #[test] fns: a panic is the failure report
#![allow(clippy::unwrap_used)]

use std::os::unix::fs::symlink;
use std::path::Path;

use jigd::identity::Identities;

const ABC: &[u8] = b"C:6437b3ac38465133ffb63b75273a8db5";
const EMPTY: &[u8] = b"C:af1349b9f5f9a1a6a0404dea36dcc949";

fn b(p: &Path) -> Vec<u8> {
    p.as_os_str().as_encoded_bytes().to_vec()
}

fn s(p: &[u8]) -> String {
    String::from_utf8_lossy(p).into_owned()
}

/// `behaviors/jigd-identities.md §The identity value` test vectors, from
/// `tests/vectors/jigd-identities.toml` (`implementation/12-testing.md §L1: spec vectors`).
#[test]
fn identity_value_vectors() {
    let file = jig_testkit::vectors::VectorFile::load("jigd-identities");
    let store = tempfile::tempdir().unwrap();
    let ids = Identities::new(&b(store.path()));
    for (i, v) in file.of_kind("identity", 2).into_iter().enumerate() {
        let path = store.path().join(format!("f{i}"));
        std::fs::write(&path, v.bytes("contents")).unwrap();
        let got = ids.of(&b(&path));
        assert_eq!(s(&got), v.str("identity"), "{}", v.label());
        assert_eq!(got.len(), 34, "{}", v.label());
    }
    // the L2 ports below spell the same two values
    let all: Vec<&str> = file.vectors.iter().map(|v| v.str("identity")).collect();
    assert_eq!(all, [s(ABC), s(EMPTY)]);
}

/// `TestIdentityMatchesJigAndTracksRewrites`.
#[test]
fn identity_matches_jig_and_tracks_rewrites() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("h.h");
    std::fs::write(&path, b"abc").unwrap();
    let ids = Identities::new(&b(dir.path()));
    // jig: "C:" + Digest(HashOf("abc")).hex(), BLAKE3 truncated to 128 bits
    assert_eq!(s(&ids.of(&b(&path))), s(ABC));
    assert_eq!(s(&ids.of(&b(&path))), s(ABC), "memoised lookup");
    assert_eq!(ids.len(), 1);
    std::fs::write(&path, b"abcd").unwrap();
    let got = ids.of(&b(&path));
    assert!(
        got != ABC && !got.is_empty(),
        "rewrite not noticed: {}",
        s(&got)
    );
    assert_eq!(ids.len(), 1);
    assert!(ids.of(&b(&dir.path().join("missing"))).is_empty());
    assert!(
        ids.of(&b(dir.path())).is_empty(),
        "unreadable paths must yield empty identities"
    );
}

/// `TestIdentityOnlyUnderStore`: the socket is open to every sandboxed build, so IDS must not be
/// a read oracle for the host.
#[test]
fn identity_only_under_store() {
    let store = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (st, hm) = (store.path(), home.path());
    let secret = hm.join("id_ed25519");
    std::fs::write(&secret, b"key").unwrap();
    std::fs::write(st.join("h.h"), b"abc").unwrap();
    // a build's output may hold symlinks pointing anywhere
    symlink(&secret, st.join("link")).unwrap();
    symlink(hm, st.join("dir")).unwrap();
    symlink(st.join("h.h"), st.join("inside")).unwrap();
    let ids = Identities::new(&b(st));
    let mut dotdot = b(st);
    dotdot.extend_from_slice(b"/../");
    dotdot.extend_from_slice(hm.file_name().unwrap().as_encoded_bytes());
    dotdot.extend_from_slice(b"/id_ed25519");
    let mut sibling = b(st);
    sibling.extend_from_slice(b"x/h.h");
    for p in [
        b(&secret),
        dotdot,
        sibling,
        b(st),
        b(&st.join("link")),
        b(&st.join("dir").join("id_ed25519")),
    ] {
        assert!(ids.of(&p).is_empty(), "{}: leaked", s(&p));
    }
    assert!(
        !ids.of(&b(&st.join("h.h"))).is_empty(),
        "store path not identified"
    );
    assert!(
        !ids.of(&b(&st.join("inside"))).is_empty(),
        "store path not identified"
    );
}

/// Q-ID-6/Q-ID-7 territory: the path as asked must be clean and under the configured prefix;
/// the resolved one under the resolved store.
#[test]
fn lexical_checks() {
    let store = tempfile::tempdir().unwrap();
    let st = store.path();
    std::fs::create_dir(st.join("sub")).unwrap();
    std::fs::write(st.join("sub/f"), b"abc").unwrap();
    std::fs::write(st.join("empty"), b"").unwrap();
    let ids = Identities::new(&b(st));
    let good = b(&st.join("sub/f"));
    assert_eq!(ids.of(&good), ABC);
    assert_eq!(ids.of(&b(&st.join("empty"))), EMPTY);
    let root = s(&b(st));
    for bad in [
        format!("{root}//sub/f"),
        format!("{root}/./sub/f"),
        format!("{root}/sub/../sub/f"),
        format!("{root}/sub/f/"),
        format!("{root}/sub"),
        format!("{root}/sub/f/x"),
        "sub/f".to_owned(),
        String::new(),
    ] {
        assert!(ids.of(bad.as_bytes()).is_empty(), "{bad:?}");
    }
    // a NUL cannot be in a path the kernel sees
    let mut nul = good.clone();
    nul.extend_from_slice(b"\0x");
    assert!(ids.of(&nul).is_empty());
    // the store configured with a trailing slash or unclean is cleaned
    let ids = Identities::new(format!("{root}/sub/..//").as_bytes());
    assert_eq!(ids.store(), format!("{root}/").as_bytes());
    assert_eq!(ids.of(&good), ABC);
}

/// A store reached through a symlink: paths are asked under the configured name and resolve
/// under the real one (Q-ID-5).
#[test]
fn store_behind_a_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::write(real.join("f"), b"abc").unwrap();
    symlink(&real, tmp.path().join("store")).unwrap();
    symlink("f", real.join("rel")).unwrap();
    symlink("../real/f", real.join("up")).unwrap();
    symlink("../elsewhere", real.join("out")).unwrap();
    std::fs::write(tmp.path().join("elsewhere"), b"abc").unwrap();
    symlink("loop", real.join("loop")).unwrap();
    symlink("nothing", real.join("dangling")).unwrap();
    let ids = Identities::new(&b(&tmp.path().join("store")));
    let st = tmp.path().join("store");
    assert_eq!(ids.of(&b(&st.join("f"))), ABC);
    assert_eq!(ids.of(&b(&st.join("rel"))), ABC);
    assert_eq!(ids.of(&b(&st.join("up"))), ABC);
    for name in ["out", "loop", "dangling"] {
        assert!(ids.of(&b(&st.join(name))).is_empty(), "{name}");
    }
    // the real name is not the configured prefix
    assert!(ids.of(&b(&real.join("f"))).is_empty());
    assert_eq!(ids.len(), 3);
}

/// Only regular files have identities, and nothing that is not one is memoised.
#[test]
fn file_types() {
    let store = tempfile::tempdir().unwrap();
    let st = store.path();
    std::os::unix::net::UnixListener::bind(st.join("sock")).unwrap();
    std::fs::create_dir(st.join("d")).unwrap();
    symlink("/dev/null", st.join("null")).unwrap();
    let ids = Identities::new(&b(st));
    for name in ["sock", "d", "null"] {
        assert!(ids.of(&b(&st.join(name))).is_empty(), "{name}");
    }
    assert_eq!(ids.len(), 0);
}

/// The memo is guarded by inode, size and mtime: a replacement file with the same size and a
/// restored mtime is still noticed through its inode.
#[test]
fn memo_stamp() {
    let store = tempfile::tempdir().unwrap();
    let st = store.path();
    let f = st.join("f");
    std::fs::write(&f, b"abc").unwrap();
    let ids = Identities::new(&b(st));
    assert_eq!(ids.of(&b(&f)), ABC);
    let mtime = std::fs::metadata(&f).unwrap().modified().unwrap();
    let tmp = st.join("f.new");
    std::fs::write(&tmp, b"xyz").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&tmp)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    std::fs::rename(&tmp, &f).unwrap();
    let got = ids.of(&b(&f));
    assert!(got != ABC && got.len() == 34, "{}", s(&got));
    // the memo is never pruned: one entry per path ever identified
    assert_eq!(ids.len(), 1);
}
