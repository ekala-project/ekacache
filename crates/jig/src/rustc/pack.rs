//! The `o/<k2>` blob of rustc mode (`behaviors/jig-rustc-mode.md §Output blob`,
//! `rustc_mode.cc:83-123`): one `<name> <octal mode> <size>\n<bytes>` record per output.

use std::os::unix::fs::PermissionsExt;

use jig_core::bytes::{os_path, parse_uint, parse_uint_octal, replace_all, split};
use jig_core::fspath;

use crate::util::{read_file, write_file};

/// The permission bits a record carries (`kPermissionBits`).
const PERMISSION_BITS: u32 = 0o777;

/// `perms::mask`: what `std::filesystem::permissions` keeps of the mode it is given.
const PERMS_MASK: u64 = 0o7777;

/// `PackFiles(paths, stem)`: a record per readable path, in order; an unreadable path is
/// skipped. The name is the basename with every `stem` replaced by `@` (unchanged for an empty
/// stem); the mode is `permissions & 0777` (`0777` when `stat` fails, `perms::unknown`). Bytes
/// are stored as they are (Q-RS-2: no `MaskOut`). No readable path gives an empty blob, which
/// is stored and replayed like any other (Q-RS-4).
#[must_use]
pub fn pack_files<P: AsRef<[u8]>>(paths: &[P], stem: &[u8]) -> Vec<u8> {
    let mut blob = Vec::new();
    for path in paths {
        let path = path.as_ref();
        let Some(data) = read_file(path) else {
            continue;
        };
        let mode = std::fs::metadata(os_path(path)).map_or(PERMISSION_BITS, |m| {
            m.permissions().mode() & PERMISSION_BITS
        });
        blob.extend_from_slice(&replace_all(fspath::filename(path), stem, b"@"));
        blob.extend_from_slice(format!(" {mode:o} {}\n", data.len()).as_bytes());
        blob.extend_from_slice(&data);
    }
    blob
}

/// `UnpackFiles(blob, dir, stem)`: write each record to `dir / ReplaceAll(name, "@", stem)`
/// (`WriteFile`: temp file and rename), then set its mode exactly, ignoring errors. `false` at
/// the first malformed record or failed write; files written before it stay. An empty blob is
/// `true`.
///
/// Q-RS-8: every `@` becomes the stem, also one that was in the original name; with an empty
/// stem `@` is removed.
#[must_use]
pub fn unpack_files(mut blob: &[u8], dir: &[u8], stem: &[u8]) -> bool {
    while !blob.is_empty() {
        let Some(newline) = blob.iter().position(|&c| c == b'\n') else {
            return false;
        };
        let head = split(&blob[..newline], b' ', false);
        let [name, mode, size] = head[..] else {
            return false;
        };
        let (Some(mode), Some(size)) = (parse_uint_octal(mode), parse_uint(size)) else {
            return false;
        };
        let body = &blob[newline + 1..];
        let Some(size) = usize::try_from(size).ok().filter(|&s| s <= body.len()) else {
            return false;
        };
        let out = fspath::join(dir, &replace_all(name, b"@", stem));
        if !write_file(&out, &body[..size]) {
            return false;
        }
        // `fs::permissions(out, perms(mode), ec)`: chmod(mode & perms::mask), errors ignored
        let bits = u32::try_from(mode & PERMS_MASK).unwrap_or(0);
        let _ = std::fs::set_permissions(os_path(&out), std::fs::Permissions::from_mode(bits));
        blob = &body[size..];
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn b(p: &Path) -> Vec<u8> {
        p.as_os_str().as_encoded_bytes().to_vec()
    }

    fn mode_of(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn pack_format() {
        let dir = tempfile::tempdir().unwrap();
        let rlib = dir.path().join("libfoo-abcd.rlib");
        std::fs::write(&rlib, "hello").unwrap();
        std::fs::set_permissions(&rlib, std::fs::Permissions::from_mode(0o644)).unwrap();
        let d = dir.path().join("foo-abcd.d");
        std::fs::write(&d, "").unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o4755)).unwrap();
        let blob = pack_files(&[b(&rlib), b(&dir.path().join("gone")), b(&d)], b"-abcd");
        assert_eq!(blob, b"libfoo@.rlib 644 5\nhellofoo@.d 755 0\n");
        // no stem: names unchanged
        let blob = pack_files(&[b(&rlib)], b"");
        assert_eq!(blob, b"libfoo-abcd.rlib 644 5\nhello");
        // the stem is replaced wherever it occurs
        let twice = dir.path().join("x-a-a");
        std::fs::write(&twice, "z").unwrap();
        std::fs::set_permissions(&twice, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(pack_files(&[b(&twice)], b"-a"), b"x@@ 600 1\nz");
        // Q-RS-4: nothing readable is an empty blob
        assert!(pack_files(&[b(&dir.path().join("gone"))], b"").is_empty());
        assert!(pack_files(&[b(dir.path())], b"").is_empty());
    }

    #[test]
    fn round_trip_with_stem_and_modes() {
        let src = tempfile::tempdir().unwrap();
        let a = src.path().join("libfoo-1.rlib");
        std::fs::write(&a, b"\x00\x01rlib\n").unwrap();
        std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o640)).unwrap();
        let c = src.path().join("foo-1.d");
        std::fs::write(&c, "dep\n").unwrap();
        std::fs::set_permissions(&c, std::fs::Permissions::from_mode(0o755)).unwrap();
        let blob = pack_files(&[b(&a), b(&c)], b"-1");

        let dst = tempfile::tempdir().unwrap();
        // an existing file is replaced, and its mode set exactly
        std::fs::write(dst.path().join("libfoo-2.rlib"), "old").unwrap();
        std::fs::set_permissions(
            dst.path().join("libfoo-2.rlib"),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        assert!(unpack_files(&blob, &b(dst.path()), b"-2"));
        let a2 = dst.path().join("libfoo-2.rlib");
        assert_eq!(std::fs::read(&a2).unwrap(), b"\x00\x01rlib\n");
        assert_eq!(mode_of(&a2), 0o640);
        assert_eq!(std::fs::read(dst.path().join("foo-2.d")).unwrap(), b"dep\n");
        assert_eq!(mode_of(&dst.path().join("foo-2.d")), 0o755);
        // Q-RS-4: an empty blob unpacks to nothing, successfully
        assert!(unpack_files(b"", b"/nonexistent", b""));
    }

    #[test]
    fn unpack_modes() {
        let dst = tempfile::tempdir().unwrap();
        let dir = b(dst.path());
        // the setuid bit passes (perms::mask is 07777); higher bits are masked off
        assert!(unpack_files(b"s 4755 0\nt 1000640 0\n", &dir, b""));
        assert_eq!(mode_of(&dst.path().join("s")), 0o4755);
        assert_eq!(mode_of(&dst.path().join("t")), 0o640);
        // a tab around a number is trimmed by ParseUint
        assert!(unpack_files(b"u 600\t 2\nab", &dir, b""));
        assert_eq!(std::fs::read(dst.path().join("u")).unwrap(), b"ab");
        // the header is split on single spaces, empty fields dropped
        assert!(unpack_files(b"  v  644   1 \nq", &dir, b""));
        assert_eq!(std::fs::read(dst.path().join("v")).unwrap(), b"q");
    }

    /// Q-RS-8: every `@` becomes the stem, one in the original name too.
    #[test]
    fn q_rs_8_at_rewriting() {
        let src = tempfile::tempdir().unwrap();
        let f = src.path().join("we@ird-1.rlib");
        std::fs::write(&f, "x").unwrap();
        let blob = pack_files(&[b(&f)], b"-1");
        assert!(blob.starts_with(b"we@ird@.rlib "));
        let dst = tempfile::tempdir().unwrap();
        assert!(unpack_files(&blob, &b(dst.path()), b"-2"));
        assert!(dst.path().join("we-2ird-2.rlib").exists());
        assert!(unpack_files(&blob, &b(dst.path()), b""));
        assert!(dst.path().join("weird.rlib").exists());
    }

    /// Each failure branch of `UnpackFiles`; records before the bad one stay written.
    #[test]
    fn unpack_failures() {
        let dst = tempfile::tempdir().unwrap();
        let dir = b(dst.path());
        for bad in [
            &b"a 644 1"[..],                  // no newline
            b"a 644\nx",                      // two fields
            b"a 644 1 2\nx",                  // four fields
            b"a 648 1\nx",                    // not octal
            b"a -644 1\nx",                   // a sign
            b"a 644 x\nx",                    // not decimal
            b"a 644 2\nx",                    // size beyond the blob
            b"a 644 99999999999999999999\nx", // overflow
            b"no/such/dir/a 644 1\nx",        // the write fails
        ] {
            assert!(
                !unpack_files(bad, &dir, b""),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        assert!(!unpack_files(b"first 644 1\n1second 644\n", &dir, b""));
        assert_eq!(std::fs::read(dst.path().join("first")).unwrap(), b"1");
        // a name with `/` is joined under the directory as it is
        std::fs::create_dir(dst.path().join("sub")).unwrap();
        assert!(unpack_files(b"sub/f 644 1\nz", &dir, b""));
        assert_eq!(std::fs::read(dst.path().join("sub/f")).unwrap(), b"z");
    }
}
