//! Go-shaped error values, so that log lines carry the Go daemon's error text byte for byte
//! (`ecd-daemon.md` §Logging; `implementation/03-ecd-server.md` §Logging and error text).
//!
//! Rust's `io::Error` prints `No such file or directory (os error 2)`; Go prints
//! `open /x: no such file or directory`. Each variant mirrors one Go error type and renders
//! with Go's lowercase `syscall.Errno` strings (`goerr_table.rs`, generated from Go 1.25).

use std::fmt;
use std::io;

use crate::goerr_table::ERRNO;

/// One Go error value. `text()` is what Go's `%v` prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoErr {
    /// `syscall.Errno`.
    Errno(i32),
    /// `*os.SyscallError`: `<syscall>: <err>`.
    Syscall { name: &'static str, errno: i32 },
    /// `*fs.PathError`: `<op> <path>: <err>`.
    Path {
        op: &'static str,
        path: Vec<u8>,
        err: Box<GoErr>,
    },
    /// `*os.LinkError` (link, rename): `<op> <old> <new>: <err>`.
    Link {
        op: &'static str,
        old: Vec<u8>,
        new: Vec<u8>,
        err: Box<GoErr>,
    },
    /// `*net.OpError`: `<op> <net>[ <source>][->| ]<addr>: <err>`.
    Op {
        op: &'static str,
        net: &'static str,
        source: Option<Vec<u8>>,
        addr: Option<Vec<u8>>,
        err: Box<GoErr>,
    },
    /// `errors.New` / `fmt.Errorf` text, including the sentinels `EOF`, `unexpected EOF`,
    /// `bad put` and `store closed`.
    Msg(Vec<u8>),
}

/// Go's text for an errno: the `syscall` table entry, or `errno <n>`.
pub fn errno_text(errno: i32) -> String {
    // darwin/amd64's table is darwin/arm64's without 106 (`gen/goerrno/darwin.sh`)
    #[cfg(all(target_vendor = "apple", target_arch = "x86_64"))]
    if errno == 106 {
        return format!("errno {errno}");
    }
    match usize::try_from(errno).ok().and_then(|i| ERRNO.get(i)) {
        Some(s) if !s.is_empty() => (*s).to_owned(),
        _ => format!("errno {errno}"),
    }
}

impl GoErr {
    pub fn msg(text: impl Into<Vec<u8>>) -> GoErr {
        GoErr::Msg(text.into())
    }

    /// `io.EOF`.
    pub fn eof() -> GoErr {
        GoErr::msg("EOF")
    }

    /// `io.ErrUnexpectedEOF`.
    pub fn unexpected_eof() -> GoErr {
        GoErr::msg("unexpected EOF")
    }

    /// The inner error of a wrapper: the errno if the I/O error has one, else its text.
    pub fn from_io(e: &io::Error) -> GoErr {
        match e.raw_os_error() {
            Some(errno) => GoErr::Errno(errno),
            None => match e.kind() {
                io::ErrorKind::UnexpectedEof => GoErr::unexpected_eof(),
                _ => GoErr::Msg(e.to_string().into_bytes()),
            },
        }
    }

    /// `&PathError{Op: op, Path: path, Err: e}`.
    pub fn path(op: &'static str, path: &[u8], e: &io::Error) -> GoErr {
        GoErr::Path {
            op,
            path: path.to_vec(),
            err: Box::new(GoErr::from_io(e)),
        }
    }

    /// `&PathError{Op: op, Path: path, Err: errno}`.
    pub fn path_errno(op: &'static str, path: &[u8], errno: i32) -> GoErr {
        GoErr::Path {
            op,
            path: path.to_vec(),
            err: Box::new(GoErr::Errno(errno)),
        }
    }

    /// `&LinkError{Op: op, Old: old, New: new, Err: e}`.
    pub fn link(op: &'static str, old: &[u8], new: &[u8], e: &io::Error) -> GoErr {
        GoErr::Link {
            op,
            old: old.to_vec(),
            new: new.to_vec(),
            err: Box::new(GoErr::from_io(e)),
        }
    }

    /// The errno at the bottom of the wrapper chain, if any.
    pub fn errno(&self) -> Option<i32> {
        match self {
            GoErr::Errno(e) | GoErr::Syscall { errno: e, .. } => Some(*e),
            GoErr::Path { err, .. } | GoErr::Link { err, .. } | GoErr::Op { err, .. } => {
                err.errno()
            }
            GoErr::Msg(_) => None,
        }
    }

    /// Go's `err.Error()` as bytes (paths are not necessarily UTF-8).
    pub fn text(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.write_text(&mut out);
        out
    }

    fn write_text(&self, out: &mut Vec<u8>) {
        match self {
            GoErr::Errno(e) => out.extend_from_slice(errno_text(*e).as_bytes()),
            GoErr::Syscall { name, errno } => {
                out.extend_from_slice(name.as_bytes());
                out.extend_from_slice(b": ");
                out.extend_from_slice(errno_text(*errno).as_bytes());
            }
            GoErr::Path { op, path, err } => {
                out.extend_from_slice(op.as_bytes());
                out.push(b' ');
                out.extend_from_slice(path);
                out.extend_from_slice(b": ");
                err.write_text(out);
            }
            GoErr::Link { op, old, new, err } => {
                out.extend_from_slice(op.as_bytes());
                out.push(b' ');
                out.extend_from_slice(old);
                out.push(b' ');
                out.extend_from_slice(new);
                out.extend_from_slice(b": ");
                err.write_text(out);
            }
            GoErr::Op {
                op,
                net,
                source,
                addr,
                err,
            } => {
                out.extend_from_slice(op.as_bytes());
                if !net.is_empty() {
                    out.push(b' ');
                    out.extend_from_slice(net.as_bytes());
                }
                if let Some(s) = source {
                    out.push(b' ');
                    out.extend_from_slice(s);
                }
                if let Some(a) = addr {
                    out.extend_from_slice(if source.is_some() { b"->" } else { b" " });
                    out.extend_from_slice(a);
                }
                out.extend_from_slice(b": ");
                err.write_text(out);
            }
            GoErr::Msg(m) => out.extend_from_slice(m),
        }
    }
}

impl fmt::Display for GoErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.text()))
    }
}

impl std::error::Error for GoErr {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno_strings_are_go_s() {
        assert_eq!(errno_text(2), "no such file or directory");
        assert_eq!(errno_text(17), "file exists");
        assert_eq!(errno_text(28), "no space left on device");
        assert_eq!(errno_text(libc::EADDRINUSE), "address already in use");
        assert_eq!(errno_text(13), "permission denied");
        assert_eq!(errno_text(0), "errno 0");
        // a hole in the table: Linux has no errno 41 (darwin's is EPROTOTYPE)
        #[cfg(target_os = "linux")]
        assert_eq!(errno_text(41), "errno 41");
        #[cfg(target_vendor = "apple")]
        assert_eq!(errno_text(41), "protocol wrong type for socket");
        assert_eq!(errno_text(4000), "errno 4000");
        assert_eq!(errno_text(-1), "errno -1");
    }

    #[test]
    fn wrappers_render_like_go() {
        let open = GoErr::path_errno("open", b"/x/000001.pack", 17);
        assert_eq!(open.text(), b"open /x/000001.pack: file exists");
        let listen = GoErr::Op {
            op: "listen",
            net: "unix",
            source: None,
            addr: Some(b"/run/s".to_vec()),
            err: Box::new(GoErr::Syscall {
                name: "bind",
                errno: libc::EADDRINUSE,
            }),
        };
        assert_eq!(
            listen.text(),
            b"listen unix /run/s: bind: address already in use"
        );
        let read = GoErr::Op {
            op: "read",
            net: "unix",
            source: Some(b"/run/s".to_vec()),
            addr: Some(b"@".to_vec()),
            err: Box::new(GoErr::Errno(libc::ECONNRESET)),
        };
        assert_eq!(
            read.text(),
            b"read unix /run/s->@: connection reset by peer"
        );
        let rename = GoErr::Link {
            op: "rename",
            old: b"a.tmp".to_vec(),
            new: b"a".to_vec(),
            err: Box::new(GoErr::Errno(2)),
        };
        assert_eq!(rename.text(), b"rename a.tmp a: no such file or directory");
        let copy = GoErr::Path {
            op: "write",
            path: b"/p".to_vec(),
            err: Box::new(GoErr::Syscall {
                name: "copy_file_range",
                errno: 28,
            }),
        };
        assert_eq!(
            copy.text(),
            b"write /p: copy_file_range: no space left on device"
        );
        assert_eq!(copy.errno(), Some(28));
    }
}
