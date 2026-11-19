//! File and argument helpers shared by the modes (`base.cc`): `ReadFile`, `WriteFile`, `OnPath`,
//! `TokenizeGnu`, `ExpandResponseFiles`.

use ekacache_core::bytes::{is_c_space, os_path, split};
use ekacache_core::fspath;

use crate::process::ProcEnv;
use crate::sys;

/// `ReadFile(path)`: the whole file, or `None` when it cannot be opened or a read fails (a
/// directory opens and then fails with `EISDIR`).
#[must_use]
pub fn read_file(path: &[u8]) -> Option<Vec<u8>> {
    std::fs::read(os_path(path)).ok()
}

/// `WriteFile(path, data)` (`base.cc:89-101`): write `<parent>/.jig<pid>.<filename>`, then rename
/// it over `path`. A failed write removes the temp file and returns `false`; a failed rename
/// returns `false` and leaves the temp file (legacy does not remove it either).
#[must_use]
pub fn write_file(path: &[u8], data: &[u8]) -> bool {
    write_file_errno(path, data).is_ok()
}

/// `WriteFile`, and on failure the `errno` legacy's `jig: cannot write <path>: <strerror>`
/// message reports: after a failed open or write, `fs::remove(tmp)` runs first, so a failed
/// `remove(3)` (usually `ENOENT`: the temp file was never created) replaces the write's error;
/// after a failed rename, rename's.
pub fn write_file_errno(path: &[u8], data: &[u8]) -> Result<(), i32> {
    use std::io::Write;
    let mut name = format!(".jig{}.", std::process::id()).into_bytes();
    name.extend_from_slice(fspath::filename(path));
    let tmp = fspath::join(&fspath::parent_path(path), &name);
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(os_path(&tmp))
        .and_then(|mut f| f.write_all(data));
    if let Err(e) = written {
        let write_errno = e.raw_os_error().unwrap_or(libc::EIO);
        return Err(if sys::remove(&tmp) {
            write_errno
        } else {
            sys::errno()
        });
    }
    std::fs::rename(os_path(&tmp), os_path(path)).map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))
}

/// `OnPath(name)`: for a name without `/`, the first `<dir>/<name>` that exists for a non-empty
/// piece of `$PATH`; otherwise (or when none exists) the name itself.
#[must_use]
pub fn on_path(name: &[u8], env: &ProcEnv) -> Vec<u8> {
    if !name.contains(&b'/') {
        for dir in split(env.get_or("PATH", b""), b':', false) {
            let candidate = fspath::join(dir, name);
            if std::fs::metadata(os_path(&candidate)).is_ok() {
                return candidate;
            }
        }
    }
    name.to_vec()
}

/// `TokenizeGnu` (`base.cc:150-175`): response-file words as clang reads them. A backslash makes
/// the next byte literal (a trailing one is kept), quotes group, unquoted C white space
/// separates.
#[must_use]
pub fn tokenize_gnu(text: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut word = Vec::new();
    let mut in_word = false;
    let mut quote = 0u8;
    let mut i = 0;
    while i < text.len() {
        let mut c = text[i];
        if c == b'\\' && i + 1 < text.len() {
            i += 1;
            c = text[i];
        } else if quote != 0 && c == quote {
            quote = 0;
            i += 1;
            continue;
        } else if quote == 0 && (c == b'\'' || c == b'"') {
            quote = c;
            in_word = true;
            i += 1;
            continue;
        } else if quote == 0 && is_c_space(c) {
            if in_word {
                out.push(std::mem::take(&mut word));
            }
            in_word = false;
            i += 1;
            continue;
        }
        word.push(c);
        in_word = true;
        i += 1;
    }
    if in_word {
        out.push(word);
    }
    out
}

/// `ExpandResponseFiles(args)`: each `@file` that names a readable file is replaced by its
/// words; any other `@word` stays (not recursive).
#[must_use]
pub fn expand_response_files<A: AsRef<[u8]>>(args: &[A]) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(args.len());
    for arg in args {
        let arg = arg.as_ref();
        match arg.strip_prefix(b"@").and_then(read_file) {
            Some(body) => out.extend(tokenize_gnu(&body)),
            None => out.push(arg.to_vec()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ekacache_core::env::Env;

    fn words(t: &str) -> Vec<String> {
        tokenize_gnu(t.as_bytes())
            .into_iter()
            .map(|w| String::from_utf8(w).unwrap())
            .collect()
    }

    #[test]
    fn gnu_tokenizer() {
        assert_eq!(words("a b\tc\n d"), ["a", "b", "c", "d"]);
        assert_eq!(words(r#"-I"a b" 'c d'e"#), ["-Ia b", "c de"]);
        assert_eq!(words(r"a\ b c\\d e\"), ["a b", r"c\d", r"e\"]);
        assert_eq!(words(r#""" '' x"#), ["", "", "x"]);
        assert_eq!(words(r#""a\"b""#), [r#"a"b"#]);
        assert_eq!(words("\x0b\x0c\r"), Vec::<String>::new());
        assert_eq!(words("'unterminated x"), ["unterminated x"]);
    }

    #[test]
    fn response_files() {
        let dir = tempfile::tempdir().unwrap();
        let rsp = dir.path().join("r");
        std::fs::write(&rsp, "-c 'x y.c'\n").unwrap();
        let at = format!("@{}", rsp.display());
        let got = expand_response_files(&[&b"-O2"[..], at.as_bytes(), b"@/nonexistent", b"@"]);
        assert_eq!(
            got,
            vec![
                b"-O2".to_vec(),
                b"-c".to_vec(),
                b"x y.c".to_vec(),
                b"@/nonexistent".to_vec(),
                b"@".to_vec()
            ]
        );
    }

    #[test]
    fn write_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("out.o");
        let pb = p.as_os_str().as_encoded_bytes();
        assert!(write_file(pb, b"one"));
        assert!(write_file(pb, b"two"));
        assert_eq!(read_file(pb).as_deref(), Some(&b"two"[..]));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        assert!(!write_file(b"/nonexistent/dir/x", b""));
        assert_eq!(
            write_file_errno(b"/nonexistent/dir/x", b""),
            Err(libc::ENOENT)
        );
        // the target is a non-empty directory: the rename fails, the temp file stays
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("f"), "").unwrap();
        let sb = sub.as_os_str().as_encoded_bytes();
        assert!(matches!(
            write_file_errno(sb, b"x"),
            Err(libc::EISDIR | libc::ENOTEMPTY | libc::EEXIST)
        ));
        assert_eq!(read_file(b"/nonexistent"), None);
        assert_eq!(read_file(dir.path().as_os_str().as_encoded_bytes()), None);
    }

    #[test]
    fn path_lookup() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tool"), "").unwrap();
        let d = dir.path().to_str().unwrap();
        let env = ProcEnv::new(Env::from_pairs(&[("PATH", format!("::/nonexistent:{d}"))]));
        assert_eq!(on_path(b"tool", &env), format!("{d}/tool").into_bytes());
        assert_eq!(on_path(b"nope", &env), b"nope");
        assert_eq!(on_path(b"./tool", &env), b"./tool");
    }
}
