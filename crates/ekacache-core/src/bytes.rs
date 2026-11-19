//! Byte-string helpers (`implementation/02-core.md §bytes`).
//!
//! Free functions on `&[u8]` with the exact semantics of the `std::string` / `base.cc` helpers
//! legacy uses (`Split`, `Trim`, `ReplaceAll`, `ParseUint`, `SplitWhitespace`, `HexEncode`), so
//! that no `bstr` dependency is needed and every edge case is pinned here.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

/// The bytes of an `OsStr` (Unix).
#[must_use]
pub fn os_bytes(s: &OsStr) -> &[u8] {
    s.as_bytes()
}

/// An `OsString` from bytes (Unix).
#[must_use]
pub fn os_string(b: impl Into<Vec<u8>>) -> OsString {
    OsString::from_vec(b.into())
}

/// A byte string as a file-system path the way `path.c_str()` passes it to the kernel: cut at
/// the first NUL. Rust's `std::fs` rejects interior NULs, legacy silently truncates.
#[must_use]
pub fn os_path(p: &[u8]) -> &Path {
    let end = p.iter().position(|&c| c == 0).unwrap_or(p.len());
    Path::new(OsStr::from_bytes(&p[..end]))
}

/// `std::string::find(needle, from)`: an empty needle matches at `from` when `from <= len`.
#[must_use]
pub fn find_from(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from > hay.len() {
        return None;
    }
    if needle.is_empty() {
        return Some(from);
    }
    if needle.len() > hay.len() - from {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// `std::string::find(needle)`.
#[must_use]
pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    find_from(hay, needle, 0)
}

/// `std::string::find(char, from)`.
#[must_use]
pub fn find_byte_from(hay: &[u8], b: u8, from: usize) -> Option<usize> {
    hay.get(from..)?
        .iter()
        .position(|&c| c == b)
        .map(|p| p + from)
}

/// `std::string::rfind(needle)`: an empty needle matches at `len`.
#[must_use]
pub fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(hay.len());
    }
    if needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).rposition(|w| w == needle)
}

/// `std::string::contains(needle)`.
#[must_use]
pub fn contains(hay: &[u8], needle: &[u8]) -> bool {
    find(hay, needle).is_some()
}

/// Split at the first occurrence of `b`.
#[must_use]
pub fn split_once(s: &[u8], b: u8) -> Option<(&[u8], &[u8])> {
    let i = s.iter().position(|&c| c == b)?;
    Some((&s[..i], &s[i + 1..]))
}

/// `ReplaceAll` (`base.cc`): every non-overlapping occurrence, left to right; scanning resumes
/// after the inserted replacement. An empty `from` returns the text unchanged.
#[must_use]
pub fn replace_all(text: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() {
        return text.to_vec();
    }
    let mut out = Vec::with_capacity(text.len());
    let mut pos = 0;
    while let Some(hit) = find_from(text, from, pos) {
        out.extend_from_slice(&text[pos..hit]);
        out.extend_from_slice(to);
        pos = hit + from.len();
    }
    out.extend_from_slice(&text[pos..]);
    out
}

/// `Split(text, sep, keep_empty)` (`base.cc`). With `keep_empty == false` empty pieces are
/// dropped; with `true` an empty text gives one empty piece.
#[must_use]
pub fn split(text: &[u8], sep: u8, keep_empty: bool) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    while start <= text.len() {
        let end = find_byte_from(text, sep, start).unwrap_or(text.len());
        if keep_empty || end > start {
            out.push(&text[start..end]);
        }
        start = end + 1;
    }
    out
}

/// `SplitWhitespace` (`base.cc`): separators are space, tab and newline only.
#[must_use]
pub fn split_whitespace(text: &[u8]) -> Vec<&[u8]> {
    text.split(|&c| c == b' ' || c == b'\t' || c == b'\n')
        .filter(|w| !w.is_empty())
        .collect()
}

/// `Trim` (`base.cc`): spaces and tabs at both ends.
#[must_use]
pub fn trim(mut s: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = s {
        s = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = s {
        s = rest;
    }
    s
}

/// `ParseUint(text, 10)` (`base.cc`): `Trim`, then `std::from_chars` over the whole rest. No
/// sign is accepted, at least one digit is required, overflow fails.
#[must_use]
pub fn parse_uint(text: &[u8]) -> Option<u64> {
    let t = trim(text);
    if t.is_empty() {
        return None;
    }
    let mut v: u64 = 0;
    for &c in t {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add(u64::from(c - b'0'))?;
    }
    Some(v)
}

/// `ParseUint(text, 8)`: octal, same rules.
#[must_use]
pub fn parse_uint_octal(text: &[u8]) -> Option<u64> {
    let t = trim(text);
    if t.is_empty() {
        return None;
    }
    let mut v: u64 = 0;
    for &c in t {
        if !(b'0'..=b'7').contains(&c) {
            return None;
        }
        v = v.checked_mul(8)?.checked_add(u64::from(c - b'0'))?;
    }
    Some(v)
}

/// ASCII decimal of an unsigned number (`std::to_string`, `std::format("{}")`).
#[must_use]
pub fn itoa(n: u64) -> Vec<u8> {
    n.to_string().into_bytes()
}

/// Lowercase hex, two characters per byte (`HexEncode`).
#[must_use]
pub fn hex_encode(bytes: &[u8]) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[usize::from(b >> 4)]);
        out.push(HEX[usize::from(b & 15)]);
    }
    out
}

/// Concatenate byte slices.
#[must_use]
pub fn concat(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(parts.iter().map(|p| p.len()).sum());
    for p in parts {
        out.extend_from_slice(p);
    }
    out
}

/// `Join(parts, sep)` (`base.cc`): note that legacy skips the separator while the output is
/// still empty, so leading empty parts vanish.
#[must_use]
pub fn join<T: AsRef<[u8]>>(parts: &[T], sep: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in parts {
        if !out.is_empty() {
            out.extend_from_slice(sep);
        }
        out.extend_from_slice(p.as_ref());
    }
    out
}

/// `std::isspace` in the C locale.
#[must_use]
pub const fn is_c_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_edges() {
        assert_eq!(find(b"abc", b""), Some(0));
        assert_eq!(find_from(b"abc", b"", 3), Some(3));
        assert_eq!(find_from(b"abc", b"", 4), None);
        assert_eq!(find(b"aaa", b"aa"), Some(0));
        assert_eq!(find_from(b"aaa", b"aa", 1), Some(1));
        assert_eq!(find_from(b"aaa", b"aa", 2), None);
        assert_eq!(rfind(b"/nix/store", b"/"), Some(4));
        assert_eq!(rfind(b"abc", b""), Some(3));
    }

    #[test]
    fn replace_all_edges() {
        assert_eq!(replace_all(b"aaaa", b"aa", b"b"), b"bb");
        assert_eq!(replace_all(b"aaa", b"aa", b"b"), b"ba");
        assert_eq!(replace_all(b"abc", b"", b"x"), b"abc");
        // the replacement is not rescanned
        assert_eq!(replace_all(b"aa", b"a", b"aa"), b"aaaa");
    }

    #[test]
    fn split_edges() {
        assert_eq!(split(b"", b' ', false), Vec::<&[u8]>::new());
        assert_eq!(split(b"", b' ', true), vec![&b""[..]]);
        assert_eq!(split(b" a  b ", b' ', false), vec![&b"a"[..], b"b"]);
        assert_eq!(split(b"a::b:", b':', true), vec![&b"a"[..], b"", b"b", b""]);
    }

    #[test]
    fn trim_and_parse() {
        assert_eq!(trim(b" \t x \t"), b"x");
        assert_eq!(parse_uint(b" 42\t"), Some(42));
        assert_eq!(parse_uint(b"+1"), None);
        assert_eq!(parse_uint(b""), None);
        assert_eq!(parse_uint(b"1 2"), None);
        assert_eq!(parse_uint(b"18446744073709551615"), Some(u64::MAX));
        assert_eq!(parse_uint(b"18446744073709551616"), None);
        assert_eq!(parse_uint_octal(b"0755"), Some(0o755));
        assert_eq!(parse_uint_octal(b"8"), None);
    }

    #[test]
    fn whitespace_and_hex() {
        assert_eq!(
            split_whitespace(b" a\tb\nc\rd "),
            vec![&b"a"[..], b"b", b"c\rd"]
        );
        assert_eq!(hex_encode(&[0, 0xab, 0xff]), b"00abff");
        assert_eq!(join(&[&b""[..], b"a", b"b"], b","), b"a,b");
    }
}
