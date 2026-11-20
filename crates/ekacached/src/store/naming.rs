//! Pack and hint file names (`ecd-store.md` §Directory layout and naming, §OpenStore: the
//! `Sscanf("%06d.pack")` table, Q-STORE-3, Q-STORE-9).

use std::os::unix::ffi::OsStrExt;

use crate::gopath;

/// `fmt.Sprintf("%06d.%s", id, ext)`: at least 6 digits, more for ids above 999 999
/// (Q-STORE-3).
pub(crate) fn file_name(id: u32, ext: &str) -> Vec<u8> {
    format!("{id:06}.{ext}").into_bytes()
}

/// `packName(dir, id, ext)`: `filepath.Join(dir, "%06d.<ext>")`.
pub(crate) fn pack_path(dir: &[u8], id: u32, ext: &str) -> Vec<u8> {
    gopath::join(&[dir, &file_name(id, ext)])
}

/// `filepath.Glob(dir + "/*.pack")`, sorted: every directory entry whose name ends in `.pack`,
/// as `filepath.Join(dir, name)` together with the name, in bytewise order. An unreadable
/// directory yields nothing (Glob ignores I/O errors).
///
/// Glob would also interpret metacharacters in `dir` itself; ecd's tier directories are taken
/// literally here (the Go behaviour for a directory name containing `*?[\` is not specified
/// and is not reproduced).
pub(crate) fn glob_packs(dir: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let Ok(rd) = std::fs::read_dir(ekacache_core::bytes::os_path(dir)) else {
        return Vec::new();
    };
    let mut names: Vec<Vec<u8>> = rd
        .map_while(Result::ok)
        .map(|e| e.file_name().as_bytes().to_vec())
        .filter(|n| n.ends_with(b".pack"))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|n| (gopath::join(&[dir, &n]), n))
        .collect()
}

/// `fmt.Sscanf(name, "%06d.pack", &id)` with a `uint32` target, as Go 1.25 implements it:
/// leading Unicode white space is skipped (a newline fails), then 1 to 6 ASCII digits are read,
/// then the literal `.pack` must follow; anything after it is ignored. `None` = Sscanf failed
/// (the file is skipped silently).
pub fn scanf_pack_id(name: &[u8]) -> Option<u32> {
    let mut rest = name;
    // SkipSpace (the verb's width does not count the skipped runes)
    loop {
        let Some((r, w)) = decode_rune(rest) else {
            break;
        };
        if r == '\r' && rest.get(w) == Some(&b'\n') {
            return None; // "\r\n" is a newline, and Sscanf's newlines are not space
        }
        if r == '\n' {
            return None;
        }
        if !is_go_space(r) {
            break;
        }
        rest = &rest[w..];
    }
    let digits = rest
        .iter()
        .take(6)
        .take_while(|b| b.is_ascii_digit())
        .count();
    if digits == 0 {
        return None;
    }
    let id = rest[..digits]
        .iter()
        .fold(0u32, |acc, b| acc * 10 + u32::from(b - b'0'));
    rest[digits..].starts_with(b".pack").then_some(id)
}

/// One UTF-8 rune the way Go's `readRune` decodes it: an invalid sequence is U+FFFD, 1 byte.
fn decode_rune(b: &[u8]) -> Option<(char, usize)> {
    let first = *b.first()?;
    if first < 0x80 {
        return Some((char::from(first), 1));
    }
    let len = match first {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return Some((char::REPLACEMENT_CHARACTER, 1)),
    };
    match b.get(..len).map(std::str::from_utf8) {
        Some(Ok(s)) => s.chars().next().map(|c| (c, len)),
        _ => Some((char::REPLACEMENT_CHARACTER, 1)),
    }
}

/// fmt's `isSpace`: the Unicode White_Space ranges.
fn is_go_space(r: char) -> bool {
    matches!(
        u32::from(r),
        0x09..=0x0d | 0x20 | 0x85 | 0xa0 | 0x1680 | 0x2000..=0x200a | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ecd-store.md §OpenStore, one assertion per table row (Q-STORE-9).
    #[test]
    fn sscanf_table() {
        assert_eq!(scanf_pack_id(b"000042.pack"), Some(42));
        assert_eq!(scanf_pack_id(b"42.pack"), Some(42));
        assert_eq!(scanf_pack_id(b" 00042.pack"), Some(42));
        assert_eq!(scanf_pack_id(b"000042.pack.pack"), Some(42));
        assert_eq!(scanf_pack_id(b"999999.pack"), Some(999_999));
        assert_eq!(scanf_pack_id(b"1000000.pack"), None);
        assert_eq!(scanf_pack_id(b"0000042.pack"), None);
        assert_eq!(scanf_pack_id(b"+00042.pack"), None);
        assert_eq!(scanf_pack_id(b"-00001.pack"), None);
        assert_eq!(scanf_pack_id(b"abc.pack"), None);
    }

    // The same Sscanf rules beyond the table, checked against Go 1.25.
    #[test]
    fn sscanf_edges() {
        // skipped space does not count toward the width
        assert_eq!(scanf_pack_id(b" 000042.pack"), Some(42));
        assert_eq!(scanf_pack_id(b"\t\x0b 000001.pack"), Some(1));
        assert_eq!(
            scanf_pack_id("\u{a0}\u{3000}000007.pack".as_bytes()),
            Some(7)
        );
        assert_eq!(scanf_pack_id(b"\r000007.pack"), Some(7));
        assert_eq!(scanf_pack_id(b"\n000007.pack"), None);
        assert_eq!(scanf_pack_id(b"\r\n000007.pack"), None);
        assert_eq!(scanf_pack_id(b"\xff000007.pack"), None);
        assert_eq!(scanf_pack_id(b".pack"), None);
        assert_eq!(scanf_pack_id(b"000000.pack"), Some(0));
        assert_eq!(scanf_pack_id(b"000042.packed"), Some(42));
        assert_eq!(scanf_pack_id(b"000042.pac"), None);
        assert_eq!(scanf_pack_id(b"42x.pack"), None);
    }

    #[test]
    fn names() {
        assert_eq!(file_name(1, "pack"), b"000001.pack");
        assert_eq!(file_name(1_000_000, "hint"), b"1000000.hint");
        assert_eq!(
            pack_path(b"/c//packs/", 42, "hint.tmp"),
            b"/c/packs/000042.hint.tmp"
        );
    }
}
