//! Go's `path/filepath` lexical functions on Unix (`Clean`, `Join`, `Dir`, `Base`), on bytes.
//!
//! jigd builds every file name the way the Go daemon did (`jigd-store.md` §Directory layout and
//! naming, `jigd-daemon.md` §Environment, `jigd-identities.md` §Path resolution), and those
//! names show up in log lines and error text, so the lexical rules must be Go's, not
//! `std::path`'s or C++ `std::filesystem`'s (`ekacache_core::fspath`).

/// `filepath.Clean`: the shortest path equivalent to `path` by purely lexical processing.
pub fn clean(path: &[u8]) -> Vec<u8> {
    if path.is_empty() {
        return b".".to_vec();
    }
    let rooted = path[0] == b'/';
    let n = path.len();
    let mut out: Vec<u8> = Vec::with_capacity(n);
    let (mut r, mut dotdot) = (0, 0);
    if rooted {
        out.push(b'/');
        r = 1;
        dotdot = 1;
    }
    while r < n {
        if path[r] == b'/' {
            // empty path element
            r += 1;
        } else if path[r] == b'.' && (r + 1 == n || path[r + 1] == b'/') {
            // . element
            r += 1;
        } else if path[r] == b'.' && path[r + 1] == b'.' && (r + 2 == n || path[r + 2] == b'/') {
            // .. element: remove to the last /
            r += 2;
            if out.len() > dotdot {
                let mut w = out.len() - 1;
                while w > dotdot && out[w] != b'/' {
                    w -= 1;
                }
                out.truncate(w);
            } else if !rooted {
                // cannot backtrack, but not rooted, so append .. element
                if !out.is_empty() {
                    out.push(b'/');
                }
                out.extend_from_slice(b"..");
                dotdot = out.len();
            }
        } else {
            // real path element; add a slash if needed
            if (rooted && out.len() != 1) || (!rooted && !out.is_empty()) {
                out.push(b'/');
            }
            while r < n && path[r] != b'/' {
                out.push(path[r]);
                r += 1;
            }
        }
    }
    if out.is_empty() {
        return b".".to_vec();
    }
    out
}

/// `filepath.Join`: the non-empty elements joined with `/`, then cleaned; `""` if all are empty.
pub fn join(elems: &[&[u8]]) -> Vec<u8> {
    let Some(first) = elems.iter().position(|e| !e.is_empty()) else {
        return Vec::new();
    };
    let mut joined = Vec::new();
    for (i, e) in elems[first..].iter().enumerate() {
        if i > 0 {
            joined.push(b'/');
        }
        joined.extend_from_slice(e);
    }
    clean(&joined)
}

/// `filepath.Dir`: all but the last element, cleaned.
pub fn dir(path: &[u8]) -> Vec<u8> {
    let cut = path.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1);
    clean(&path[..cut])
}

/// `filepath.Base`: the last element, trailing slashes removed; `"."` for `""`, `"/"` for all
/// slashes.
pub fn base(path: &[u8]) -> Vec<u8> {
    if path.is_empty() {
        return b".".to_vec();
    }
    let mut end = path.len();
    while end > 0 && path[end - 1] == b'/' {
        end -= 1;
    }
    if end == 0 {
        return b"/".to_vec();
    }
    let start = path[..end]
        .iter()
        .rposition(|&b| b == b'/')
        .map_or(0, |i| i + 1);
    path[start..end].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Go's path/filepath clean tests (path_test.go cleantests, Unix subset).
    #[test]
    fn clean_matches_go() {
        let cases: &[(&str, &str)] = &[
            ("abc", "abc"),
            ("abc/def", "abc/def"),
            ("a/b/c", "a/b/c"),
            (".", "."),
            ("..", ".."),
            ("../..", "../.."),
            ("../../abc", "../../abc"),
            ("/abc", "/abc"),
            ("/", "/"),
            ("", "."),
            ("abc/", "abc"),
            ("abc/def/", "abc/def"),
            ("a/b/c/", "a/b/c"),
            ("./", "."),
            ("../", ".."),
            ("../../", "../.."),
            ("/abc/", "/abc"),
            ("abc//def//ghi", "abc/def/ghi"),
            ("//abc", "/abc"),
            ("///abc", "/abc"),
            ("//abc//", "/abc"),
            ("abc//", "abc"),
            ("abc/./def", "abc/def"),
            ("/./abc/def", "/abc/def"),
            ("abc/.", "abc"),
            ("abc/def/ghi/../jkl", "abc/def/jkl"),
            ("abc/def/../ghi/../jkl", "abc/jkl"),
            ("abc/def/..", "abc"),
            ("abc/def/../..", "."),
            ("/abc/def/../..", "/"),
            ("abc/def/../../..", ".."),
            ("/abc/def/../../..", "/"),
            ("abc/def/../../../ghi/jkl/../../../mno", "../../mno"),
            ("/../abc", "/abc"),
            ("a/../b:/../../c", "../c"),
            ("abc/./../def", "def"),
            ("abc//./../def", "def"),
            ("abc/../../././../def", "../../def"),
        ];
        for (input, want) in cases {
            assert_eq!(clean(input.as_bytes()), want.as_bytes(), "clean({input:?})");
        }
    }

    #[test]
    fn join_matches_go() {
        assert_eq!(join(&[b"a", b"b", b"c"]), b"a/b/c");
        assert_eq!(join(&[b"", b"b"]), b"b");
        assert_eq!(join(&[b"a", b""]), b"a");
        assert_eq!(join(&[b"", b""]), b"");
        assert_eq!(join(&[b"/cache/", b"jigd", b"packs"]), b"/cache/jigd/packs");
        assert_eq!(
            join(&[b"/cold//x/", b"000001.pack"]),
            b"/cold/x/000001.pack"
        );
        assert_eq!(join(&[b"a/", b"/b"]), b"a/b");
    }

    #[test]
    fn dir_and_base_match_go() {
        assert_eq!(dir(b"/run/jigd/socket"), b"/run/jigd");
        assert_eq!(dir(b"socket"), b".");
        assert_eq!(dir(b"/socket"), b"/");
        assert_eq!(dir(b"a//b//"), b"a/b");
        assert_eq!(dir(b""), b".");
        assert_eq!(base(b"/a/b/000001.pack"), b"000001.pack");
        assert_eq!(base(b"/a/b/"), b"b");
        assert_eq!(base(b"///"), b"/");
        assert_eq!(base(b""), b".");
    }
}
