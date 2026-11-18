//! Store path computation (`behaviors/jig-nix-store.md §Store path computation`: SHA-256, Nix32,
//! XOR-fold to 20 bytes, MakeStorePath, text paths, fixed-output paths).
//!
//! Nothing here validates names, references, `algo` or `hex`: they are used verbatim
//! (Q-NIX-16).

use jig_core::bytes::hex_encode;
use sha2::{Digest, Sha256};

/// The Nix32 alphabet (no `e`, `o`, `t`, `u`).
const NIX32_ALPHABET: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

/// Bytes in a store path's hash part before Nix32 encoding.
const STORE_PATH_HASH_BYTES: usize = 20;

/// FIPS 180-4 SHA-256.
#[must_use]
pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// `Nix32(B)` (`§Nix32`): `B` read as one little-endian integer, written as exactly
/// `floor((8n - 1) / 5) + 1` base-32 digits, most significant first.
#[must_use]
pub fn nix32(bytes: &[u8]) -> Vec<u8> {
    if bytes.is_empty() {
        // Q-NIX-15: unreachable (only 20-byte folds are encoded); the C++ length underflows
        return Vec::new();
    }
    let len = (bytes.len() * 8 - 1) / 5 + 1;
    let byte_at = |i: usize| bytes.get(i).map_or(0u32, |&b| u32::from(b));
    (0..len)
        .map(|i| {
            let bit = (len - 1 - i) * 5;
            let pair = byte_at(bit / 8) | (byte_at(bit / 8 + 1) << 8);
            NIX32_ALPHABET[((pair >> (bit % 8)) & 0x1f) as usize]
        })
        .collect()
}

/// `fold(d)` (`§XOR-fold to 20 bytes`): `o[i mod 20] ^= d[i]`.
#[must_use]
pub fn fold20(digest: &[u8]) -> [u8; STORE_PATH_HASH_BYTES] {
    let mut out = [0u8; STORE_PATH_HASH_BYTES];
    for (i, &b) in digest.iter().enumerate() {
        out[i % STORE_PATH_HASH_BYTES] ^= b;
    }
    out
}

/// `MakeStorePath(store_dir, type, inner, name)` (`§MakeStorePath`):
/// `store_dir "/" Nix32(fold(SHA256(type ":sha256:" hex(SHA256(inner)) ":" store_dir ":" name))) "-" name`.
#[must_use]
pub fn make_store_path(store_dir: &[u8], kind: &[u8], inner: &[u8], name: &[u8]) -> Vec<u8> {
    let fingerprint = [
        kind,
        b":sha256:",
        &hex_encode(&sha256(inner)),
        b":",
        store_dir,
        b":",
        name,
    ]
    .concat();
    [
        store_dir,
        b"/",
        &nix32(&fold20(&sha256(&fingerprint))),
        b"-",
        name,
    ]
    .concat()
}

/// `TextPath(name, refs, contents)` under `store_dir` (`§Text paths`): type `text` followed by
/// `:<ref>` for each reference in bytewise order.
#[must_use]
pub fn text_path<R: AsRef<[u8]>>(
    store_dir: &[u8],
    name: &[u8],
    refs: &[R],
    contents: &[u8],
) -> Vec<u8> {
    let mut kind = b"text".to_vec();
    for r in sorted_refs(refs) {
        kind.push(b':');
        kind.extend_from_slice(r);
    }
    make_store_path(store_dir, &kind, contents, name)
}

/// References in ascending bytewise order, duplicates kept (Q-NIX-5). Shared by the offline
/// type string and the AddToStore reference list.
#[must_use]
pub fn sorted_refs<R: AsRef<[u8]>>(refs: &[R]) -> Vec<&[u8]> {
    let mut v: Vec<&[u8]> = refs.iter().map(AsRef::as_ref).collect();
    // Q-NIX-5: sorted, not deduplicated
    v.sort_unstable();
    v
}

/// `FixedOutputPath(store_dir, name, algo, hex)` (`§Fixed-output paths (flat)`): type
/// `output:out`, inner fingerprint `fixed:out:<algo>:<hex>:` (with the trailing `:`). The path is
/// derived through SHA-256 whatever `algo` says (Q-NIX-16).
#[must_use]
pub fn fixed_output_path(store_dir: &[u8], name: &[u8], algo: &[u8], hex: &[u8]) -> Vec<u8> {
    let inner = [b"fixed:out:", algo, b":", hex, b":"].concat();
    make_store_path(store_dir, b"output:out", &inner, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        String::from_utf8(hex_encode(b)).unwrap()
    }

    fn s(b: &[u8]) -> &str {
        std::str::from_utf8(b).unwrap()
    }

    fn vectors() -> jig_testkit::vectors::VectorFile {
        jig_testkit::vectors::VectorFile::load("jig-nix-store")
    }

    /// The hash part of a store path.
    fn hash_part(path: &str) -> &str {
        let base = path.rsplit('/').next().unwrap();
        &base[..32]
    }

    /// `§Test vectors` `### SHA-256 [test]`: the `sha256` rows of
    /// `tests/vectors/jig-nix-store.toml`.
    #[test]
    fn sha256_vectors() {
        for v in vectors().of_kind("sha256", 3) {
            let input = match v.opt_bytes("repeat") {
                Some(r) => r.repeat(v.usize("count")),
                None => v.bytes("input").to_vec(),
            };
            assert_eq!(hex(&sha256(&input)), v.str("hex"), "{}", v.label());
        }
    }

    /// `§Test vectors` `### Nix32`.
    #[test]
    fn nix32_vectors() {
        for v in vectors().of_kind("nix32", 7) {
            let bytes = match v.opt_bytes("sha256_of") {
                Some(t) => sha256(t).to_vec(),
                None => v.hex("bytes_hex"),
            };
            assert_eq!(s(&nix32(&bytes)), v.str("nix32"), "{}", v.label());
        }
        assert_eq!(nix32(&[0; 32]).len(), 52);
        assert_eq!(nix32(b""), b"");
    }

    /// `§Test vectors` `### Fixed output path [test]`, with the intermediate values.
    #[test]
    fn fixed_output_path_vector() {
        let file = vectors();
        let v = file.one("fixed-output-path");
        let inner_hash = hex(&sha256(v.bytes("inner")));
        let fp = v.str("fingerprint");
        assert!(fp.contains(&format!(":sha256:{inner_hash}:")), "{fp}");
        let digest = sha256(fp.as_bytes());
        assert_eq!(hex(&digest), v.str("fingerprint_sha256"));
        assert_eq!(hex(&fold20(&digest)), v.str("fold"));
        assert_eq!(s(&nix32(&fold20(&digest))), v.str("fold_nix32"));
        assert_eq!(
            s(&fixed_output_path(
                v.bytes("store"),
                v.bytes("name"),
                v.bytes("algo"),
                v.bytes("hex")
            )),
            v.str("path")
        );
    }

    /// `§Test vectors` `### Text paths [bin]` and their fingerprints.
    #[test]
    fn text_path_vectors() {
        let file = vectors();
        for v in file.of_kind("text-path", 6) {
            let refs = v.byte_strs("refs");
            assert_eq!(
                s(&text_path(
                    b"/nix/store",
                    v.bytes("name"),
                    &refs,
                    v.bytes("stdin")
                )),
                v.str("path"),
                "{}",
                v.label()
            );
        }
        for v in file.of_kind("text-fingerprint", 2) {
            let digest = sha256(v.bytes("fingerprint"));
            if let Some(want) = v.opt_str("sha256") {
                assert_eq!(hex(&digest), want, "{}", v.label());
            }
            if let Some(want) = v.opt_str("fold") {
                assert_eq!(hex(&fold20(&digest)), want, "{}", v.label());
            }
            assert_eq!(
                s(&nix32(&fold20(&digest))),
                hash_part(v.str("path")),
                "{}",
                v.label()
            );
        }
    }

    /// `§Test vectors` `### ATerm [test]`: the text path's inner hash is SHA256 of the ATerm.
    #[test]
    fn aterm_inner_hash() {
        let file = vectors();
        let v = file.one("aterm");
        assert_eq!(hex(&sha256(v.bytes("aterm"))), v.str("inner_sha256"));
    }

    #[test]
    fn refs_sort_bytewise_unsigned() {
        let refs: [&[u8]; 4] = [b"b", b"\xff", b"a", b"b"];
        assert_eq!(
            sorted_refs(&refs),
            [&b"a"[..], &b"b"[..], &b"b"[..], &b"\xff"[..]]
        );
    }
}
