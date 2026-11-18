//! C++ `std::filesystem::path` lexical semantics (POSIX, libstdc++) on byte strings.
//!
//! Specified by `behaviors/jig-reloc-fixup.md §Conventions used in this document` (path
//! operations) and `behaviors/jig-keys.md §NormalizePathArg` step 4. Rust's `Path` normalises
//! differently (trailing `/`, `..` past the root, empty components), so cc, rustc and
//! reloc-fixup use these functions instead.

use std::cmp::Ordering;

/// The *elements* of a path: `"/"` if it begins with `/`; each maximal run of non-`/` bytes;
/// then a final `""` if the path ends with `/` and has at least one non-root element.
#[must_use]
pub fn elements(p: &[u8]) -> Vec<&[u8]> {
    let mut out: Vec<&[u8]> = Vec::new();
    if p.is_empty() {
        return out;
    }
    if p[0] == b'/' {
        out.push(b"/");
    }
    let mut non_root = false;
    for run in p.split(|&c| c == b'/') {
        if !run.is_empty() {
            out.push(run);
            non_root = true;
        }
    }
    if non_root && p.last() == Some(&b'/') {
        out.push(b"");
    }
    out
}

/// `operator/` (`join(a, b)`): `b` if it is absolute; else `a + "/" + b` when `a` is non-empty
/// and does not end with `/`; else `a + b`. So `join("a", "")` is `"a/"`.
#[must_use]
pub fn join(a: &[u8], b: &[u8]) -> Vec<u8> {
    if b.first() == Some(&b'/') {
        return b.to_vec();
    }
    let mut out = a.to_vec();
    if !a.is_empty() && a.last() != Some(&b'/') {
        out.push(b'/');
    }
    out.extend_from_slice(b);
    out
}

/// `lexically_normal()`.
///
/// `/a/b/..` → `/a/`, `/x/lib/.` → `/x/lib/`, `a/..` → `.`, `a/../../b` → `../b`,
/// `//a//b//` → `/a/b/`, `""` → `""`.
#[must_use]
pub fn normalize(p: &[u8]) -> Vec<u8> {
    if p.is_empty() {
        return Vec::new();
    }
    // libstdc++ returns a root-directory-only path as it is: `//` stays `//`
    if p.iter().all(|&c| c == b'/') {
        return p.to_vec();
    }
    let abs = p[0] == b'/';
    let mut stack: Vec<&[u8]> = Vec::new();
    let mut trail = false;
    let runs = p.split(|&c| c == b'/').filter(|r| !r.is_empty());
    let tail: &[&[u8]] = if p.last() == Some(&b'/') { &[b""] } else { &[] };
    for c in runs.chain(tail.iter().copied()) {
        match c {
            b"." | b"" => trail = true,
            b".." => match stack.last() {
                Some(top) if *top != b".." => {
                    stack.pop();
                    trail = true;
                }
                None if abs => {}
                _ => {
                    stack.push(b"..");
                    trail = false;
                }
            },
            _ => {
                stack.push(c);
                trail = false;
            }
        }
    }
    if stack.last() == Some(&&b".."[..]) {
        trail = false;
    }
    let mut out = Vec::with_capacity(p.len());
    if abs {
        out.push(b'/');
    }
    for (i, c) in stack.iter().enumerate() {
        if i > 0 {
            out.push(b'/');
        }
        out.extend_from_slice(c);
    }
    if trail && !stack.is_empty() {
        out.push(b'/');
    }
    if out.is_empty() {
        out.push(b'.');
    }
    out
}

/// `lexically_relative(p, base)`; neither argument is normalised.
#[must_use]
pub fn relative(p: &[u8], base: &[u8]) -> Vec<u8> {
    let pa = p.first() == Some(&b'/');
    let ba = base.first() == Some(&b'/');
    if pa != ba {
        return Vec::new();
    }
    let pe = elements(p);
    let be = elements(base);
    let mut i = 0;
    while i < pe.len() && i < be.len() && pe[i] == be[i] {
        i += 1;
    }
    if i == pe.len() && i == be.len() {
        return b".".to_vec();
    }
    let mut n: i64 = 0;
    for e in &be[i..] {
        match *e {
            b".." => n -= 1,
            b"" | b"." => {}
            _ => n += 1,
        }
    }
    if n == 0 && (i >= pe.len() || pe[i].is_empty()) {
        return b".".to_vec();
    }
    if n < 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for _ in 0..n {
        out = join(&out, b"..");
    }
    for e in &pe[i..] {
        out = join(&out, e);
    }
    out
}

/// `filename()`: the last element if it is a non-root, non-empty element; otherwise `""`.
#[must_use]
pub fn filename(p: &[u8]) -> &[u8] {
    match elements(p).last() {
        Some(&e) if e != b"/" => e,
        _ => b"",
    }
}

/// `parent_path()`: `p` without its last element and the separator run before it
/// (`/a` → `/`, `a` → `""`, `a/b/` → `a/b`, `/` → `/`).
#[must_use]
pub fn parent_path(p: &[u8]) -> Vec<u8> {
    let els = elements(p);
    let has_relative = els.iter().any(|e| *e != b"/");
    if !has_relative {
        return p.to_vec();
    }
    // byte offset where the last element starts
    let end = if p.last() == Some(&b'/') {
        p.len() // the trailing "" element starts at the end
    } else {
        p.iter().rposition(|&c| c == b'/').map_or(0, |i| i + 1)
    };
    let mut cut = end;
    while cut > 0 && p[cut - 1] == b'/' {
        cut -= 1;
    }
    if cut == 0 && p[0] == b'/' {
        return b"/".to_vec();
    }
    p[..cut].to_vec()
}

/// `extension()`: from the last `.` of the filename, unless the filename is `.`/`..` or the dot
/// is its first byte.
#[must_use]
pub fn extension(p: &[u8]) -> &[u8] {
    let f = filename(p);
    if f == b"." || f == b".." {
        return b"";
    }
    match f.iter().rposition(|&c| c == b'.') {
        Some(0) | None => b"",
        Some(i) => &f[i..],
    }
}

/// `stem()`: the filename without its extension.
#[must_use]
pub fn stem(p: &[u8]) -> &[u8] {
    let f = filename(p);
    let e = extension(p);
    &f[..f.len() - e.len()]
}

/// Path comparison (`operator<=>`): lexicographic over element lists, elements compared
/// bytewise; a proper prefix sorts first. `/a//b == /a/b`, `/a/b != /a/b/`.
#[must_use]
pub fn compare(a: &[u8], b: &[u8]) -> Ordering {
    // libstdc++ compares the root directory first: a relative path sorts before an absolute one
    is_absolute(a)
        .cmp(&is_absolute(b))
        .then_with(|| elements(a).cmp(&elements(b)))
}

/// Is `p` absolute (`is_absolute()` on POSIX: begins with `/`).
#[must_use]
pub fn is_absolute(p: &[u8]) -> bool {
    p.first() == Some(&b'/')
}

/// `std::filesystem::absolute(p)` against a given current directory: `p` itself if absolute,
/// else `cwd / p`. `None` for an empty `p` (libstdc++ reports `invalid_argument`).
#[must_use]
pub fn absolute_in(p: &[u8], cwd: &[u8]) -> Option<Vec<u8>> {
    if p.is_empty() {
        return None;
    }
    if is_absolute(p) {
        return Some(p.to_vec());
    }
    Some(join(cwd, p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: Vec<u8>) -> String {
        String::from_utf8(v).unwrap()
    }

    #[test]
    fn normalize_examples() {
        for (i, o) in [
            ("/a/b/..", "/a/"),
            ("/x/lib/.", "/x/lib/"),
            ("a/..", "."),
            ("a/../../b", "../b"),
            ("//a//b//", "/a/b/"),
            ("", ""),
            ("./include//sub/../", "include/"),
            ("./", "."),
            (".", "."),
            ("a/", "a/"),
            ("/", "/"),
            ("//x", "/x"),
            ("x/..", "."),
            ("a/b/../..", "."),
            ("../..", "../.."),
            ("/..", "/"),
            ("/../a", "/a"),
            ("../a/../..", "../.."),
            (".././..", "../.."),
            ("a/b/../", "a/"),
            ("/a/b/../..", "/"),
            ("a/./", "a/"),
            ("../", ".."),
            ("x/../", "."),
            ("//", "//"),
            ("///", "///"),
            ("../.", ".."),
            ("./..", ".."),
            ("a/../../", ".."),
            ("a/b/c/../../..", "."),
        ] {
            assert_eq!(s(normalize(i.as_bytes())), o, "normalize({i:?})");
        }
    }

    #[test]
    fn relative_examples() {
        assert_eq!(s(relative(b"/a/b/c/", b"/a/x")), "../b/c/");
        assert_eq!(s(relative(b"/a/b", b"/a/./b")), "../b");
        assert_eq!(s(relative(b"a", b"/a")), "");
        assert_eq!(s(relative(b"/a/b", b"/a/b")), ".");
        assert_eq!(s(relative(b"/a/b/", b"/a/b")), ".");
        assert_eq!(s(relative(b"/a", b"/a/b/../..")), "");
    }

    #[test]
    fn join_filename_parent() {
        assert_eq!(s(join(b"a", b"")), "a/");
        assert_eq!(s(join(b"a/", b"b")), "a/b");
        assert_eq!(s(join(b"", b"b")), "b");
        assert_eq!(s(join(b"a", b"/b")), "/b");
        assert_eq!(filename(b"a/b"), b"b");
        assert_eq!(filename(b"a/"), b"");
        assert_eq!(filename(b"/"), b"");
        assert_eq!(filename(b"."), b".");
        assert_eq!(s(parent_path(b"/a")), "/");
        assert_eq!(s(parent_path(b"a")), "");
        assert_eq!(s(parent_path(b"a/b/")), "a/b");
        assert_eq!(s(parent_path(b"/")), "/");
        assert_eq!(s(parent_path(b"/usr/bin/gcc")), "/usr/bin");
        assert_eq!(s(parent_path(b"a//b")), "a");
        assert_eq!(s(parent_path(b"//a")), "/");
        assert_eq!(s(parent_path(b"")), "");
        assert_eq!(extension(b"a/b.tar.gz"), b".gz");
        assert_eq!(extension(b".hidden"), b"");
        assert_eq!(stem(b"x/foo.c"), b"foo");
    }

    #[test]
    fn comparison() {
        assert_eq!(compare(b"/a/b/c", b"/a/b-c"), Ordering::Less);
        assert_eq!(compare(b"/a//b", b"/a/b"), Ordering::Equal);
        assert_ne!(compare(b"/a/b", b"/a/b/"), Ordering::Equal);
        assert_eq!(compare(b"/a", b"/a/b"), Ordering::Less);
    }
}
