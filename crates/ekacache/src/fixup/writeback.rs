//! A7: write-back (`behaviors/jig-reloc-fixup.md §A7`; `fixup_mode.cc` `WriteBack`,
//! `base.cc` `WriteFile`).
//!
//! The image goes to `<dir>/.jig<pid>.<name>` and is renamed over the file, so the result is a
//! new inode (Q-FIX-18, kept). The reported errno is whatever the C library's `errno` holds when
//! legacy formats the message: the last call that **failed**, among the temp-file write, the
//! cleanup `remove` of the temp file, the `rename`, and the final `chmod`. After a failed open
//! that is usually the cleanup's `ENOENT` (Q-FIX-4).

use std::fs::Permissions;
use std::io::Write;
use std::os::fd::IntoRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use ekacache_core::bytes::{concat, os_path};
use ekacache_core::fspath;

use crate::sys::strerror;

use super::output::Out;

/// `errno` as legacy sees it: updated by every failing call, untouched by successful ones.
struct Errno(i32);

impl Errno {
    fn record<T>(&mut self, r: std::io::Result<T>) -> Option<T> {
        match r {
            Ok(v) => Some(v),
            Err(e) => {
                self.0 = e.raw_os_error().unwrap_or(0);
                None
            }
        }
    }
}

/// glibc's `remove(3)`: `unlink`, and `rmdir` if that failed with `EISDIR`.
fn remove(tmp: &[u8], errno: &mut Errno) {
    let r = std::fs::remove_file(os_path(tmp));
    let is_dir = matches!(&r, Err(e) if e.raw_os_error() == Some(libc::EISDIR));
    errno.record(r);
    if is_dir {
        errno.record(std::fs::remove_dir(os_path(tmp)));
    }
}

/// `WriteFile`: write the temp file (create/truncate, mode 0666 & ~umask), close it, rename it
/// over `path`. A failed write or close removes the temp file; a failed rename leaves it.
fn write_file(path: &[u8], data: &[u8], errno: &mut Errno) -> bool {
    let mut name = format!(".jig{}.", std::process::id()).into_bytes();
    name.extend_from_slice(fspath::filename(path));
    let tmp = fspath::join(&fspath::parent_path(path), &name);
    let opened = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(os_path(&tmp));
    let written = match errno.record(opened) {
        Some(mut f) => {
            let wrote = errno.record(f.write_all(data)).is_some();
            // close explicitly: legacy's ofstream reports a failing close (ENOSPC on NFS…)
            let fd = f.into_raw_fd();
            // SAFETY: `fd` was just released from the `File`, so it is open and owned here.
            #[allow(unsafe_code)]
            let closed = unsafe { libc::close(fd) } == 0;
            if !closed {
                errno.0 = crate::sys::errno();
            }
            wrote && closed
        }
        None => false,
    };
    if !written {
        remove(&tmp, errno);
        return false;
    }
    errno
        .record(std::fs::rename(os_path(&tmp), os_path(path)))
        .is_some()
}

/// `S_IWUSR` as a `u32` (`mode_t` is `u16` on macOS).
const OWNER_WRITE: u32 = 0o200;

/// `WriteBack(path, bytes)`. On failure the message is written here; the caller counts the
/// error.
pub fn write_back(out: &mut Out, path: &[u8], bytes: &[u8]) -> bool {
    let p = os_path(path);
    let mut errno = Errno(0);
    let mode = errno.record(std::fs::metadata(p)).map(|m| m.mode());
    // the file may be read-only (installed 0444): writable for the moment
    if let Some(m) = mode {
        errno.record(std::fs::set_permissions(
            p,
            Permissions::from_mode(m | OWNER_WRITE),
        ));
    }
    let written = write_file(path, bytes, &mut errno);
    if let Some(m) = mode {
        errno.record(std::fs::set_permissions(p, Permissions::from_mode(m)));
    }
    if !written {
        // Q-FIX-4: errno after the cleanup, usually ENOENT after a failed open
        out.err(concat(&[
            path,
            b": cannot write back: ",
            &strerror(errno.0),
        ]));
    }
    written
}
