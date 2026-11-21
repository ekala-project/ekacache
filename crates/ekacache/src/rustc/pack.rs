//! The `o/<k2>` blob of rustc mode (`behaviors/ec-rustc-mode.md §Output blob`,
//! `rustc_mode.cc:83-123`): one `<name> <octal mode> <size>\n<bytes>` record per output.

use std::os::unix::fs::PermissionsExt;

use ekacache_core::bytes::{os_path, parse_uint, parse_uint_octal, replace_all, split};
use ekacache_core::fspath;

use crate::util::{read_file, write_file};

/// The permission bits a record carries (`kPermissionBits`).
const PERMISSION_BITS: u32 = 0o777;

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
/// (`WriteFile`: temp file and rename), then set its mode, ignoring errors. `false` at the
/// first malformed record or failed write; files written before it stay. An empty blob is
/// `true`.
///
/// ADOPTED Q-RS-11: a record whose name, after the `@` rewriting, is not a single path component
/// (empty, `.`, `..`, or containing `/` or NUL) is malformed, and the mode is masked to `0777`
/// (legacy kept `mode & 07777`), so a planted blob cannot write outside `dir` or restore a
/// setuid, setgid or sticky file.
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
        let name = replace_all(name, b"@", stem);
        // ADOPTED Q-RS-11: one path component under `dir`, nothing else
        if !is_plain_name(&name) {
            return false;
        }
        let out = fspath::join(dir, &name);
        if !write_file(&out, &body[..size]) {
            return false;
        }
        // chmod, errors ignored. ADOPTED Q-RS-11: `& 0777` (legacy: `& perms::mask`, 07777)
        let bits = u32::try_from(mode & u64::from(PERMISSION_BITS)).unwrap_or(0);
        let _ = std::fs::set_permissions(os_path(&out), std::fs::Permissions::from_mode(bits));
        blob = &body[size..];
    }
    true
}

/// ADOPTED Q-RS-11: a restorable name is one non-empty path component other than `.` and `..`.
fn is_plain_name(name: &[u8]) -> bool {
    !matches!(name, b"" | b"." | b"..") && !name.iter().any(|&c| c == b'/' || c == 0)
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

    /// Whether this host lets the tests set a setuid bit (the Nix build sandbox does not).
    fn setuid_allowed(dir: &Path) -> bool {
        let probe = dir.join(".setuid-probe");
        std::fs::write(&probe, "").unwrap();
        let ok = std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o4755)).is_ok()
            && mode_of(&probe) == 0o4755;
        std::fs::remove_file(&probe).unwrap();
        ok
    }

    #[test]
    fn pack_format() {
        let dir = tempfile::tempdir().unwrap();
        let rlib = dir.path().join("libfoo-abcd.rlib");
        std::fs::write(&rlib, "hello").unwrap();
        std::fs::set_permissions(&rlib, std::fs::Permissions::from_mode(0o644)).unwrap();
        let d = dir.path().join("foo-abcd.d");
        std::fs::write(&d, "").unwrap();
        // the setuid bit is masked off on the way in, where the host allows setting it
        let d_mode = if setuid_allowed(dir.path()) {
            0o4755
        } else {
            0o755
        };
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(d_mode)).unwrap();
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
        // ADOPTED Q-RS-11: setuid, setgid, sticky and higher bits are masked off
        assert!(unpack_files(b"s 7755 0\nt 1000640 0\n", &dir, b""));
        assert_eq!(mode_of(&dst.path().join("s")), 0o755);
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
    }

    /// ADOPTED Q-RS-11: a name that is not one path component is a malformed record, and
    /// nothing is written outside the directory.
    #[test]
    fn adopted_q_rs_9_hostile_names() {
        let root = tempfile::tempdir().unwrap();
        let dst = root.path().join("out");
        std::fs::create_dir(&dst).unwrap();
        std::fs::create_dir(dst.join("sub")).unwrap();
        let dir = b(&dst);
        let abs = root.path().join("abs");
        let mut absolute = b(&abs);
        absolute.extend_from_slice(b" 644 1\nz");
        for (bad, stem) in [
            (absolute, &b""[..]),
            (b"../x 644 1\nz".to_vec(), b""),
            (b"sub/f 644 1\nz".to_vec(), b""),
            (b".. 644 1\nz".to_vec(), b""),
            (b". 644 1\nz".to_vec(), b""),
            (b"a\0b 644 1\nz".to_vec(), b""),
            // empty, `..` or with `/` only after the `@` rewriting
            (b"@ 644 1\nz".to_vec(), b""),
            (b"@ 644 1\nz".to_vec(), b".."),
            (b"@x 644 1\nz".to_vec(), b"../"),
            // a good record first is written; the bad one after it is not
            (b"ok 644 1\n1../y 644 1\nz".to_vec(), b""),
        ] {
            assert!(
                !unpack_files(&bad, &dir, stem),
                "{:?}",
                String::from_utf8_lossy(&bad)
            );
        }
        assert!(!abs.exists());
        assert!(!root.path().join("x").exists());
        assert!(!root.path().join("y").exists());
        assert!(!dst.join("sub/f").exists());
        let mut left: Vec<_> = std::fs::read_dir(&dst)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        left.sort();
        assert_eq!(left, ["ok", "sub"]);
        assert!(std::fs::read_dir(dst.join("sub")).unwrap().next().is_none());
        let mut above: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        above.sort();
        assert_eq!(above, ["out"]);
    }
}
