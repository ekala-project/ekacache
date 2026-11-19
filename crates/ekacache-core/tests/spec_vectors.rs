//! ekacache-core against the L1 spec vectors (`implementation/12-testing.md §L1: spec vectors`):
//! `tests/vectors/jig-keys.toml` (`behaviors/jig-keys.md` §Digest, §Key, §Composite and
//! §Manifest test vectors) and `tests/vectors/jig-cache-client.toml` (`behaviors/
//! jig-cache-client.md §Value encoding`).
//!
//! An integration test rather than unit tests: ekacache-testkit depends on ekacache-core, so only here do
//! both link the same ekacache-core.
#![allow(clippy::unwrap_used)]

use ekacache_core::env::Env;
use ekacache_core::hasher::{Hasher, hash_of};
use ekacache_core::keys::{RequestKey, Tool};
use ekacache_core::manifest::{Unstorable, build, result_key_of, validate};
use ekacache_core::store_path::{STORE_DIR, Store};
use ekacache_testkit::vectors::{Vector, VectorFile};

fn keys() -> VectorFile {
    VectorFile::load("jig-keys")
}

fn s(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// The vectors spell `/nix/store`; some hold only for the default `JIG_STORE_DIR`.
fn default_store_dir() -> bool {
    STORE_DIR == b"/nix/store"
}

/// `/nix/store` in `p` replaced by the configured store dir.
fn in_store(p: &[u8]) -> Vec<u8> {
    ekacache_core::bytes::replace_all(p, b"/nix/store", STORE_DIR)
}

fn store(mode: Option<&str>) -> Store {
    match mode {
        None | Some("path") => Store::from_env(&Env::default()),
        Some("content") => Store::from_env(&Env::from_pairs(&[("JIG_STORE_IDENTITY", "content")])),
        Some(m) => panic!("unknown mode {m}"),
    }
}

fn no_daemon() -> impl FnMut(&[&[u8]]) -> Vec<Vec<u8>> {
    |_: &[&[u8]]| Vec::new()
}

#[test]
fn digest_vectors() {
    for v in keys().of_kind("digest", 8) {
        let got = if let Some(input) = v.opt_bytes("hash_of") {
            hash_of(input)
        } else if let Some(byte) = v.opt_bytes("repeat") {
            Hasher::new().field(&byte.repeat(v.usize("count"))).finish()
        } else {
            let mut h = Hasher::new();
            for f in v.strs("fields") {
                h.field(f.as_bytes());
            }
            h.finish()
        };
        assert_eq!(got.to_string(), v.str("digest"), "{}", v.label());
        if let Some(framed) = v.opt_bytes("framed") {
            assert_eq!(
                hash_of(framed).to_string(),
                v.str("digest"),
                "{}",
                v.label()
            );
        }
    }
}

#[test]
fn key_vectors() {
    for v in keys().of_kind("key", 43) {
        if v.opt_bool("default_store_only") == Some(true) && !default_store_dir() {
            continue;
        }
        let st = store(v.opt_str("mode"));
        assert_eq!(s(&st.key(v.bytes("arg"))), v.str("key"), "{}", v.label());
    }
}

#[test]
fn composite_vector() {
    let file = keys();
    let v = file.one("composite");
    let mut h = Hasher::new();
    for f in v.strs("fields") {
        h.field(f.as_bytes());
    }
    let d = h.finish();
    assert_eq!(s(RequestKey::new(Tool::Cc, &d).text()), v.str("k1"));
    assert_eq!(s(RequestKey::new(Tool::Rustc, &d).text()), v.str("rs_text"));
}

#[test]
fn pure_hasher_manifest_vectors() {
    for v in keys().of_kind("manifest-k2", 3) {
        let lines = v.byte_strs("lines");
        assert_eq!(
            s(result_key_of(v.bytes("k1_text"), &lines).text()),
            v.str("k2"),
            "{}",
            v.label()
        );
    }
}

/// The `/tmp/jigprobe/t` scenario. The expected k2 hashes these literal paths, so the test
/// must use exactly that directory.
#[test]
fn manifest_scenario() {
    let file = keys();
    let v = file.one("manifest-scenario");
    let t = std::path::Path::new(v.str("root"));
    let _ = std::fs::remove_dir_all(t);
    std::fs::create_dir_all(t).unwrap();
    for (name, contents) in v.strs("files").into_iter().zip(v.strs("contents")) {
        std::fs::write(t.join(name), contents).unwrap();
    }
    let inputs = v.byte_strs("inputs");
    let absent: Vec<Vec<u8>> = v.byte_strs("absent").iter().map(|p| in_store(p)).collect();
    let primary = v.bytes("primary_source");
    let k1 = RequestKey::new(Tool::Cc, &hash_of(v.bytes("k1_of")));
    assert_eq!(s(k1.text()), v.str("k1_text"));
    let unstorable = v.bytes("unstorable");
    let readable: Vec<_> = inputs
        .iter()
        .filter(|i| i.as_slice() != unstorable)
        .cloned()
        .collect();
    assert_eq!(readable.len() + 1, inputs.len());
    let build_in =
        |st: &Store, inputs: &[Vec<u8>]| build(st, &k1, inputs, primary, &absent, &mut no_daemon());
    for mode in ["path", "content"] {
        let st = store(Some(mode));
        // ADOPTED Q-KEY-3: the unreadable input makes the manifest unstorable
        assert_eq!(
            build_in(&st, &inputs),
            Err(Unstorable::Unreadable(unstorable.to_vec())),
            "{mode}"
        );
        let m = build_in(&st, &readable).unwrap();
        assert_eq!(s(&m.text), v.str("text"), "{mode}");
        assert_eq!(s(m.result_key.text()), v.str("k2"), "{mode}");
        assert_eq!(
            validate(&st, &k1, &m.text, &mut no_daemon()),
            Ok(m.result_key.clone()),
            "{mode}"
        );
        let junk = [&m.text[..], v.bytes("junk")].concat();
        assert_eq!(
            validate(&st, &k1, &junk, &mut no_daemon()),
            Ok(m.result_key.clone()),
            "{mode}"
        );
    }
    let st = store(None);
    let m = build_in(&st, &readable).unwrap();
    let appear = t.join(v.str("appear"));
    std::fs::create_dir_all(appear.parent().unwrap()).unwrap();
    std::fs::write(&appear, b"").unwrap();
    assert_eq!(
        validate(&st, &k1, &m.text, &mut no_daemon()),
        Err(v.bytes("appeared").to_vec())
    );
    std::fs::remove_dir_all(appear.parent().unwrap()).unwrap();
    std::fs::write(t.join(v.str("edit")), v.bytes("edit_contents")).unwrap();
    assert_eq!(
        validate(&st, &k1, &m.text, &mut no_daemon()),
        Err(v.bytes("changed").to_vec())
    );
    let _ = std::fs::remove_dir_all(t);
}

/// `ValidateManifest` of a stored text alone, with the scenario's k1.
#[test]
fn manifest_validate_vectors() {
    let file = keys();
    let k1_of = file.one("manifest-scenario").bytes("k1_of");
    let k1 = RequestKey::new(Tool::Cc, &hash_of(k1_of));
    for v in file.of_kind("manifest-validate", 2) {
        let st = store(v.opt_str("mode"));
        let got = validate(&st, &k1, &in_store(v.bytes("text")), &mut no_daemon());
        if let Some(err) = v.opt_bytes("error") {
            assert_eq!(got, Err(in_store(err)), "{}", v.label());
        } else {
            let k2 = got.unwrap();
            if v.opt_bool("default_store_only") != Some(true) || default_store_dir() {
                assert_eq!(s(k2.text()), v.str("k2"), "{}", v.label());
            }
        }
    }
}

fn frame_vector(v: &Vector) {
    let wire = v.hex("wire");
    assert_eq!(
        ekacache_core::frame::decode(&wire).as_deref(),
        Some(v.bytes("value")),
        "{}",
        v.label()
    );
    let exact = v.str("exact_with_zstd");
    let linked = zstd::zstd_safe::version_number();
    let linked = format!("{}.{}.{}", linked / 10000, linked / 100 % 100, linked % 100);
    let encoded = ekacache_core::frame::encode(v.bytes("value")).unwrap();
    if linked == exact {
        assert_eq!(encoded, wire, "{}", v.label());
    }
    assert_eq!(
        ekacache_core::frame::decode(&encoded).as_deref(),
        Some(v.bytes("value"))
    );
}

#[test]
fn value_encoding_vectors() {
    for v in VectorFile::load("jig-cache-client").of_kind("frame", 1) {
        frame_vector(v);
    }
}
