//! `WithPackageLibDirs` (`behaviors/ec-driver-policy.md` A13): `cc -print-search-dirs` output
//! with the package's dependency `-L` dirs on the `libraries:` line, for tools that search
//! instead of link (libtool on mingw).

use ekacache_core::bytes::find_byte_from;

/// Append `:<dir>` for each joined `-L<dir>` of `ldflags` to every `libraries:` line
/// (Q-DRV-12: the separate `-L dir` contributes nothing). Every line, the last one included,
/// ends in `\n`.
#[must_use]
pub fn with_package_lib_dirs<F: AsRef<[u8]>>(search_dirs: &[u8], ldflags: &[F]) -> Vec<u8> {
    let mut extra = Vec::new();
    for flag in ldflags {
        let flag = flag.as_ref();
        if flag.starts_with(b"-L") && flag.len() > 2 {
            extra.push(b':');
            extra.extend_from_slice(&flag[2..]);
        }
    }
    let mut out = Vec::with_capacity(search_dirs.len() + extra.len() + 1);
    let mut pos = 0;
    while pos < search_dirs.len() {
        let eol = find_byte_from(search_dirs, b'\n', pos).unwrap_or(search_dirs.len());
        let line = &search_dirs[pos..eol];
        out.extend_from_slice(line);
        if line.starts_with(b"libraries:") {
            out.extend_from_slice(&extra);
        }
        out.push(b'\n');
        pos = eol + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Beyond TV-20 and TV-34, which `tests/driver_vectors.rs` reads from
    /// `tests/vectors/ec-driver-policy.toml`.
    #[test]
    fn crlf_has_no_libraries_line() {
        assert_eq!(with_package_lib_dirs(b"\r\n", &["-L/x"]), b"\r\n");
    }
}
