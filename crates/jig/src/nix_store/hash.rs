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

    /// `§Test vectors` `### SHA-256 [test]`.
    #[test]
    fn sha256_vectors() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(&[b'a'; 1000])),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }

    /// `§Test vectors` `### Nix32`.
    #[test]
    fn nix32_vectors() {
        assert_eq!(
            s(&nix32(&sha256(b"abc"))),
            "1b8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s"
        );
        assert_eq!(s(&nix32(&[0x01])), "01");
        assert_eq!(s(&nix32(&[0x1f])), "0z");
        assert_eq!(s(&nix32(&[0x20])), "10");
        assert_eq!(s(&nix32(&[0x00, 0x01])), "0080");
        assert_eq!(s(&nix32(&[1, 2, 3, 4, 5])), "0l2060h1");
        assert_eq!(s(&nix32(&[0; 20])), "0".repeat(32));
        assert_eq!(nix32(&[0; 32]).len(), 52);
        assert_eq!(nix32(b""), b"");
    }

    /// `§Test vectors` `### Fixed output path [test]`, with the intermediate values.
    #[test]
    fn fixed_output_path_vector() {
        let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let inner = format!("fixed:out:sha256:{empty}:");
        assert_eq!(
            hex(&sha256(inner.as_bytes())),
            "25c9b18deb1a768db5c09793ae411acb62ba194c8f876ac206a070bbebef4342"
        );
        let fp = b"output:out:sha256:25c9b18deb1a768db5c09793ae411acb62ba194c8f876ac206a070bbebef4342:/nix/store:e";
        let digest = sha256(fp);
        assert_eq!(
            hex(&digest),
            "85a06ae4ab58fc6b51acbdc61a0994917340181cb4d1d30afee0600292245f5b"
        );
        assert_eq!(
            hex(&fold20(&digest)),
            "3171b9ee55b89c69c388e29d1a0994917340181c"
        );
        assert_eq!(
            s(&nix32(&fold20(&digest))),
            "3hc40wwijh4im7g2i31nk75qappbjw9i"
        );
        assert_eq!(
            s(&fixed_output_path(
                b"/nix/store",
                b"e",
                b"sha256",
                empty.as_bytes()
            )),
            "/nix/store/3hc40wwijh4im7g2i31nk75qappbjw9i-e"
        );
    }

    /// `§Test vectors` `### Text paths [bin]` and their fingerprints.
    #[test]
    fn text_path_vectors() {
        let tp = |name: &str, refs: &[&str], contents: &str| {
            String::from_utf8(text_path(
                b"/nix/store",
                name.as_bytes(),
                refs,
                contents.as_bytes(),
            ))
            .unwrap()
        };
        let a = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a";
        let b = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b";
        assert_eq!(
            tp("hello", &[], ""),
            "/nix/store/56wfjivf3kjkp4zhj4xz7722hgg01bdn-hello"
        );
        assert_eq!(
            tp("hello.txt", &[], "hello"),
            "/nix/store/q790zdjk75hm2cn42nh77pqw4gbv1b88-hello.txt"
        );
        assert_eq!(
            tp("hello.txt", &[b, a], "hello"),
            "/nix/store/ib8bp90l2c4w69hgv1c1s3xd4rifpd1f-hello.txt"
        );
        assert_eq!(
            tp("hello.txt", &[a], "hello"),
            "/nix/store/wyzya2mj8n7qzknk6pzcw6ls86nxhcir-hello.txt"
        );
        // Q-NIX-5: the duplicate is kept
        assert_eq!(
            tp("hello.txt", &[a, a], "hello"),
            "/nix/store/0f7wh5bl25mnb8di6vr02zm0pl7r4sf9-hello.txt"
        );
        assert_eq!(
            tp("", &[], ""),
            "/nix/store/9ip48nkc9rfy0a4yaw98lp6gipqlib1a-"
        );
        let fp = b"text:sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824:/nix/store:hello.txt";
        assert_eq!(
            hex(&sha256(fp)),
            "df8731676f5b0b48f2d420b351613953b60fd2c1d72a81b04c47d43b52c1e481"
        );
        assert_eq!(
            hex(&fold20(&sha256(fp))),
            "08adb0d7231cdf73a015c43251613953b60fd2c1"
        );
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
