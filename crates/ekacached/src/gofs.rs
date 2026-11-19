//! The Go `os` file operations jigd uses, with Go's semantics and Go's error values
//! (`jigd-store.md` §Directory layout and naming, §Seal, §moveDown; `jigd-daemon.md` §Startup).
//!
//! Every function takes the path as bytes, exactly as the Go code passed it, because the path
//! is part of the error text that ends up in a log line.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use ekacache_core::bytes::os_path;
use rustix::fs::{AtFlags, CWD, Timestamps, UTIME_OMIT};
use rustix::time::Timespec;

use crate::goerr::GoErr;

const ENOENT: i32 = 2;
const EEXIST: i32 = 17;
const ENOTDIR: i32 = 20;

fn p(path: &[u8]) -> &Path {
    os_path(path)
}

/// `os.MkdirAll(path, perm)`, including its error paths (`mkdir <dir>: <err>`, where `<dir>`
/// may be a parent).
pub fn mkdir_all(path: &[u8], mode: u32) -> Result<(), GoErr> {
    // fast path: stop with success or error if the path exists
    match std::fs::metadata(p(path)) {
        Ok(m) if m.is_dir() => return Ok(()),
        Ok(_) => return Err(GoErr::path_errno("mkdir", path, ENOTDIR)),
        Err(_) => {}
    }
    // the parent: strip trailing slashes, then the last element
    let mut i = path.len();
    while i > 0 && path[i - 1] == b'/' {
        i -= 1;
    }
    while i > 0 && path[i - 1] != b'/' {
        i -= 1;
    }
    let parent = &path[..i.saturating_sub(1)];
    if !parent.is_empty() {
        mkdir_all(parent, mode)?;
    }
    match std::fs::DirBuilder::new().mode(mode).create(p(path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            // "foo/." and friends: fine if it is a directory now
            if std::fs::symlink_metadata(p(path)).is_ok_and(|m| m.is_dir()) {
                return Ok(());
            }
            Err(GoErr::path("mkdir", path, &e))
        }
    }
}

/// `os.OpenFile(path, O_RDWR, 0)`.
pub fn open_rdwr(path: &[u8]) -> Result<File, GoErr> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(p(path))
        .map_err(|e| GoErr::path("open", path, &e))
}

/// `os.Open(path)` (read-only).
pub fn open(path: &[u8]) -> Result<File, GoErr> {
    File::open(p(path)).map_err(|e| GoErr::path("open", path, &e))
}

/// `os.OpenFile(path, O_RDWR|O_CREATE|O_EXCL, mode)`.
pub fn create_excl(path: &[u8], mode: u32) -> Result<File, GoErr> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(p(path))
        .map_err(|e| GoErr::path("open", path, &e))
}

/// `os.Create(path)`: `O_RDWR|O_CREATE|O_TRUNC`, mode `0666` before the umask.
pub fn create(path: &[u8]) -> Result<File, GoErr> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o666)
        .open(p(path))
        .map_err(|e| GoErr::path("open", path, &e))
}

/// `os.Remove(path)`: unlink, or rmdir if that fails.
pub fn remove(path: &[u8]) -> Result<(), GoErr> {
    let Err(e) = std::fs::remove_file(p(path)) else {
        return Ok(());
    };
    let Err(e1) = std::fs::remove_dir(p(path)) else {
        return Ok(());
    };
    let err = if e1.raw_os_error() == Some(ENOTDIR) {
        e
    } else {
        e1
    };
    Err(GoErr::path("remove", path, &err))
}

/// `os.Link(old, new)`.
pub fn link(old: &[u8], new: &[u8]) -> Result<(), GoErr> {
    std::fs::hard_link(p(old), p(new)).map_err(|e| GoErr::link("link", old, new, &e))
}

/// `os.Rename(old, new)`.
pub fn rename(old: &[u8], new: &[u8]) -> Result<(), GoErr> {
    std::fs::rename(p(old), p(new)).map_err(|e| GoErr::link("rename", old, new, &e))
}

/// `os.Chtimes(path, time.Time{}, mtime)`: sets the mtime only (atime `UTIME_OMIT`), as Go 1.25
/// does for a zero atime (`jigd-store.md` §Recency).
pub fn set_mtime(path: &[u8], mtime_ns: i64) -> Result<(), GoErr> {
    let times = Timestamps {
        last_access: Timespec {
            tv_sec: 0,
            tv_nsec: UTIME_OMIT,
        },
        last_modification: timespec(mtime_ns),
    };
    rustix::fs::utimensat(CWD, p(path), &times, AtFlags::empty())
        .map_err(|e| GoErr::path_errno("chtimes", path, e.raw_os_error()))
}

fn timespec(ns: i64) -> Timespec {
    Timespec {
        tv_sec: ns.div_euclid(1_000_000_000),
        tv_nsec: ns.rem_euclid(1_000_000_000) as _,
    }
}

/// The mtime of an open file in Unix nanoseconds (`info.ModTime().UnixNano()`), and its size.
pub fn stat(file: &File, path: &[u8]) -> Result<(i64, i64), GoErr> {
    let m = file.metadata().map_err(|e| GoErr::path("stat", path, &e))?;
    let mtime = m
        .mtime()
        .saturating_mul(1_000_000_000)
        .saturating_add(m.mtime_nsec());
    Ok((m.size() as i64, mtime))
}

/// `(*File).Truncate(size)`.
pub fn truncate(file: &File, path: &[u8], size: i64) -> Result<(), GoErr> {
    file.set_len(size as u64)
        .map_err(|e| GoErr::path("truncate", path, &e))
}

/// `(*File).Sync()`. On macOS `sync_all` is `fcntl(F_FULLFSYNC)`, as Go's `Sync` is there: plain
/// `fsync` does not flush the drive cache, and the adopted Q-STORE-7 intends a sealed pack to
/// survive power loss (P9 decision 3, `implementation/11-macos.md`; not yet confirmed).
pub fn fsync(file: &File, path: &[u8]) -> Result<(), GoErr> {
    file.sync_all().map_err(|e| GoErr::path("sync", path, &e))
}

/// Open a directory and fsync it (the adopted Q-STORE-7 directory sync; Go never did this, so
/// the error text follows the `os` style: `open <dir>: …` or `sync <dir>: …`).
pub fn fsync_dir(dir: &[u8]) -> Result<(), GoErr> {
    let d = File::open(p(dir)).map_err(|e| GoErr::path("open", dir, &e))?;
    fsync(&d, dir)
}

/// `pwrite` all of `buf` at `off`, the way Go's `(*File).Write` reports a failure: the bytes
/// written before the error, and `write <path>: <errno>`.
pub fn write_all_at(file: &File, path: &[u8], buf: &[u8], off: u64) -> (u64, Result<(), GoErr>) {
    let mut done = 0usize;
    while done < buf.len() {
        match file.write_at(&buf[done..], off + done as u64) {
            Ok(0) => {
                return (
                    done as u64,
                    Err(GoErr::path("write", path, &io::ErrorKind::WriteZero.into())),
                );
            }
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return (done as u64, Err(GoErr::path("write", path, &e))),
        }
    }
    (done as u64, Ok(()))
}

/// `dst.ReadFrom(src)` for two files (what `io.Copy(dst, src)` does): `copy_file_range` from
/// `src_off` to `dst_off` until `src` is at EOF, or Go's generic read/write fallback when the
/// kernel cannot do it at all. Returns the bytes copied and the error, worded as Go wraps it:
/// `write <dst>: copy_file_range: <errno>` from the fast path, plain `read <src>: …` /
/// `write <dst>: …` from the fallback.
#[cfg(target_os = "linux")]
pub fn copy_file(
    dst: &File,
    dst_path: &[u8],
    mut dst_off: u64,
    src: &File,
    src_path: &[u8],
    mut src_off: u64,
) -> (u64, Result<(), GoErr>) {
    // internal/poll.maxCopyFileRangeRound
    const ROUND: usize = 0x7fff_f000;
    let mut written = 0u64;
    loop {
        match rustix::fs::copy_file_range(src, Some(&mut src_off), dst, Some(&mut dst_off), ROUND) {
            // Go: a first round of 0 bytes may be a silent failure (old kernels), so it falls
            // back to the generic copy, which then finds EOF too
            Ok(0) if written == 0 => {
                return generic_copy(dst, dst_path, dst_off, src, src_path, src_off);
            }
            Ok(0) => return (written, Ok(())),
            Ok(n) => written += n as u64,
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => {
                let errno = e.raw_os_error();
                let unsupported = [
                    rustix::io::Errno::NOSYS,
                    rustix::io::Errno::XDEV,
                    rustix::io::Errno::INVAL,
                    rustix::io::Errno::OPNOTSUPP,
                    rustix::io::Errno::PERM,
                    rustix::io::Errno::IO,
                ]
                .contains(&e);
                if unsupported {
                    // Go falls back to read/write from the current offsets and reports only
                    // the fallback's byte count
                    return generic_copy(dst, dst_path, dst_off, src, src_path, src_off);
                }
                let err = GoErr::Path {
                    op: "write",
                    path: dst_path.to_vec(),
                    err: Box::new(GoErr::Syscall {
                        name: "copy_file_range",
                        errno,
                    }),
                };
                return (written, Err(err));
            }
        }
    }
}

/// `dst.ReadFrom(src)` off Linux: Go's darwin build has no `copy_file_range` (`zero_copy_stub.go`),
/// so `io.Copy` is the generic read/write loop.
#[cfg(not(target_os = "linux"))]
pub fn copy_file(
    dst: &File,
    dst_path: &[u8],
    dst_off: u64,
    src: &File,
    src_path: &[u8],
    src_off: u64,
) -> (u64, Result<(), GoErr>) {
    generic_copy(dst, dst_path, dst_off, src, src_path, src_off)
}

fn generic_copy(
    dst: &File,
    dst_path: &[u8],
    mut dst_off: u64,
    src: &File,
    src_path: &[u8],
    mut src_off: u64,
) -> (u64, Result<(), GoErr>) {
    let mut buf = vec![0u8; 32 * 1024];
    let mut written = 0u64;
    loop {
        let n = match src.read_at(&mut buf, src_off) {
            Ok(0) => return (written, Ok(())),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return (written, Err(GoErr::path("read", src_path, &e))),
        };
        src_off += n as u64;
        let (w, res) = write_all_at(dst, dst_path, &buf[..n], dst_off);
        written += w;
        dst_off += w;
        if res.is_err() {
            return (written, res);
        }
    }
}

/// `os.CreateTemp(dir, prefix + "*" + suffix)`: a new `0600` file named
/// `<dir>/<prefix><random decimal><suffix>`, retrying on a name clash. Returns the file and its
/// name.
pub fn create_temp(dir: &[u8], prefix: &[u8], suffix: &[u8]) -> Result<(File, Vec<u8>), GoErr> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    // Go's joinPath: no cleaning, a slash only if dir does not end in one
    let mut base = dir.to_vec();
    if !base.is_empty() && !base.ends_with(b"/") {
        base.push(b'/');
    }
    base.extend_from_slice(prefix);
    for _ in 0..10_000 {
        let mut name = base.clone();
        name.extend_from_slice(
            next_random(SEQ.fetch_add(1, Ordering::Relaxed))
                .to_string()
                .as_bytes(),
        );
        name.extend_from_slice(suffix);
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(p(&name))
        {
            Ok(f) => return Ok((f, name)),
            Err(e) if e.raw_os_error() == Some(EEXIST) => {}
            Err(e) => return Err(GoErr::path("open", &name, &e)),
        }
    }
    let mut pattern = base;
    pattern.push(b'*');
    pattern.extend_from_slice(suffix);
    Err(GoErr::Path {
        op: "createtemp",
        path: pattern,
        err: Box::new(GoErr::msg("file already exists")),
    })
}

fn next_random(seq: u64) -> u32 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    // splitmix64 over time, pid and a sequence number
    let mut z = t ^ (u64::from(std::process::id()) << 32) ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (z ^ (z >> 31)) as u32
}

/// `io.ReadFull`: `EOF` if nothing was read, `unexpected EOF` if only part was.
pub fn read_full(r: &mut impl Read, buf: &mut [u8]) -> Result<(), GoErr> {
    let mut done = 0;
    while done < buf.len() {
        match r.read(&mut buf[done..]) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(GoErr::from_io(&e)),
        }
    }
    match done {
        _ if done == buf.len() => Ok(()),
        0 => Err(GoErr::eof()),
        _ => Err(GoErr::unexpected_eof()),
    }
}

/// Is this the "no such file" errno?
pub fn is_not_exist(e: &GoErr) -> bool {
    e.errno() == Some(ENOENT)
}

/// Is this the "file exists" errno (`os.IsExist`)?
pub fn is_exist(e: &GoErr) -> bool {
    e.errno() == Some(EEXIST)
}

/// A path for a syscall, or `None` if it holds a NUL byte (Go's `syscall.ByteSliceFromString`
/// fails with `EINVAL` there, where `os_path` would cut the path short).
pub fn nul_free(path: &[u8]) -> Option<&Path> {
    if path.contains(&0) {
        return None;
    }
    Some(p(path))
}

/// The index of the last `/` in `dest` at or after `from` (Go's backwards scan that stops at
/// the volume).
fn last_slash(dest: &[u8], from: usize) -> Option<usize> {
    dest.get(from..)
        .and_then(|tail| tail.iter().rposition(|&c| c == b'/'))
        .map(|i| i + from)
}

/// `filepath.EvalSymlinks` on Unix (`walkSymlinks` in Go 1.25's `path/filepath/symlink.go`),
/// ported literally: components are resolved one at a time with `Lstat` and `Readlink`, at
/// most 255 links are followed, and a non-directory with components left after it is
/// `ENOTDIR`. The result is `Clean`ed. `None` on any error (`jigd-identities.md` §Algorithm
/// step 2 needs only success or failure). This is not `fs::canonicalize`, which treats `..` and
/// dangling links differently.
pub fn eval_symlinks(path: &[u8]) -> Option<Vec<u8>> {
    let mut path = path.to_vec();
    let mut vol_len = usize::from(path.first() == Some(&b'/'));
    let mut vol = path[..vol_len].to_vec();
    let mut dest = vol.clone();
    let mut links_walked = 0;
    let mut start = vol_len;
    while start < path.len() {
        while start < path.len() && path[start] == b'/' {
            start += 1;
        }
        let mut end = start;
        while end < path.len() && path[end] != b'/' {
            end += 1;
        }
        if end == start {
            // no more path components
            break;
        }
        let comp = &path[start..end];
        if comp == b"." {
            start = end;
            continue;
        }
        if comp == b".." {
            // back up to the previous component if possible (vol_len counts a leading slash)
            match last_slash(&dest, vol_len) {
                Some(r) if &dest[r + 1..] != b".." => dest.truncate(r),
                // no slash after the volume, or dest ends in a ".." we had to keep: keep this
                _ => {
                    if dest.len() > vol_len {
                        dest.push(b'/');
                    }
                    dest.extend_from_slice(b"..");
                }
            }
            start = end;
            continue;
        }
        // an ordinary component
        if !dest.is_empty() && dest.last() != Some(&b'/') {
            dest.push(b'/');
        }
        dest.extend_from_slice(comp);
        let meta = std::fs::symlink_metadata(nul_free(&dest)?).ok()?;
        if !meta.file_type().is_symlink() {
            if !meta.is_dir() && end < path.len() {
                return None; // ENOTDIR
            }
            start = end;
            continue;
        }
        links_walked += 1;
        if links_walked > 255 {
            return None; // "EvalSymlinks: too many links"
        }
        let link = std::fs::read_link(nul_free(&dest)?).ok()?;
        let link = link.as_os_str().as_encoded_bytes();
        let mut next = link.to_vec();
        next.extend_from_slice(&path[end..]);
        path = next;
        if link.first() == Some(&b'/') {
            // a symlink to an absolute path
            dest = b"/".to_vec();
            vol = b"/".to_vec();
            vol_len = 1;
            start = 1;
        } else {
            // a symlink to a relative path: replace the last component of dest
            match last_slash(&dest, vol_len) {
                Some(r) => dest.truncate(r),
                None => dest.clone_from(&vol),
            }
            start = 0;
        }
    }
    Some(crate::gopath::clean(&dest))
}
