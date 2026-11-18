//! stdout and stderr the way legacy's `std::println` writes them (`behaviors/jig-reloc-fixup.md
//! §Errors and messages`, last paragraph; Q-FIX-10).
//!
//! Legacy prints with `std::println`, which formats a whole line and hands it to one `fwrite` on
//! glibc's `stdout` or `stderr`:
//!
//! - **stderr** is unbuffered: every line is one `write(2)` as it happens.
//! - **stdout** is line buffered on a terminal and fully buffered otherwise, with a buffer of
//!   `st_blksize` bytes (`BUFSIZ` = 8192 if that is 0 or larger). Lines reach the file only when
//!   the buffer overflows and at exit. When the process aborts (Q-FIX-10), whatever is still
//!   buffered is lost. [`Stdout`] reproduces glibc's `_IO_new_file_xsputn` so the bytes that
//!   reach fd 1 before an abort are the same.
//! - A failed `fwrite` makes `std::println` throw, which ends in `std::terminate` and SIGABRT.
//!   A write error on fd 2, or on fd 1 while the buffer is being emptied mid-run, therefore
//!   aborts. A failed flush at exit is ignored.
//!
//! Writes go to the file descriptors directly: Rust's `std::io::Stdout` is line buffered and
//! both std handles silently swallow `EBADF`, which legacy does not.

/// glibc's `BUFSIZ`.
const BUFSIZ: usize = 8192;

/// Below this glibc does not keep writes block aligned (`_IO_new_file_xsputn`).
const ALIGN_MIN: usize = 128;

/// The process ends here, as legacy does through an uncaught C++ exception (Q-FIX-10).
///
/// Legacy's libstdc++ prints `terminate called after throwing an instance of …` and `what(): …`
/// before `abort()`. That text is not reproduced (the flag on the abort text, shared with
/// Q-DRV-10, is open): the process dies by SIGABRT with nothing more on stderr. Buffered stdout
/// is deliberately not flushed.
pub fn abort_now() -> ! {
    std::process::abort()
}

/// `write(2)` of all of `data` to `fd`, `false` on the first error (glibc's
/// `_IO_new_file_write` does not retry `EINTR` either).
fn write_fd(fd: libc::c_int, mut data: &[u8]) -> bool {
    while !data.is_empty() {
        // SAFETY: `data` is a valid, initialised buffer of `data.len()` bytes for the duration
        // of the call; `write` does not retain the pointer.
        #[allow(unsafe_code)]
        let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        let Ok(n) = usize::try_from(n) else {
            return false;
        };
        data = data.get(n..).unwrap_or_default();
    }
    true
}

/// How glibc buffers fd 1, decided at the first write (`_IO_file_doallocate`).
#[derive(Clone, Copy)]
enum Mode {
    Line,
    Full(usize),
}

fn stdout_mode() -> Mode {
    // SAFETY: `fstat` writes into the zeroed `stat` we own; `isatty` only takes a descriptor.
    #[allow(unsafe_code)]
    let (rc, st, tty) = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        let rc = libc::fstat(1, &raw mut st);
        let tty = rc == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFCHR && libc::isatty(1) == 1;
        (rc, st, tty)
    };
    if tty {
        return Mode::Line;
    }
    let blk = usize::try_from(st.st_blksize).unwrap_or(0);
    if rc == 0 && blk > 0 && blk < BUFSIZ {
        Mode::Full(blk)
    } else {
        Mode::Full(BUFSIZ)
    }
}

/// glibc's `stdout` (see the module comment).
#[derive(Default)]
pub struct Stdout {
    buf: Vec<u8>,
    /// `None` until the first write allocates the buffer.
    mode: Option<Mode>,
}

impl Stdout {
    /// `fwrite(data)` of one whole line.
    fn fwrite(&mut self, data: &[u8]) {
        let allocated = self.mode.is_some();
        let mode = *self.mode.get_or_insert_with(stdout_mode);
        let cap = match mode {
            Mode::Line => {
                // every line ends in '\n', which flushes a line-buffered stream
                if !write_fd(1, data) {
                    abort_now();
                }
                return;
            }
            Mode::Full(cap) => cap,
        };
        // the space left in the buffer; none before the first write has allocated it
        let space = if allocated {
            cap.saturating_sub(self.buf.len())
        } else {
            0
        };
        let (now, rest) = data.split_at(space.min(data.len()));
        self.buf.extend_from_slice(now);
        if rest.is_empty() {
            return;
        }
        // flush the full buffer, write whole blocks directly, buffer the remainder
        if !self.buf.is_empty() && !write_fd(1, &self.buf) {
            abort_now();
        }
        self.buf.clear();
        let tail = if cap >= ALIGN_MIN {
            rest.len().checked_rem(cap).unwrap_or(0)
        } else {
            0
        };
        let (direct, keep) = rest.split_at(rest.len().saturating_sub(tail));
        if !direct.is_empty() && !write_fd(1, direct) {
            abort_now();
        }
        self.buf.extend_from_slice(keep);
    }

    /// `exit`'s flush. Errors are ignored.
    pub fn flush(&mut self) {
        if !self.buf.is_empty() {
            let _ = write_fd(1, &self.buf);
            self.buf.clear();
        }
    }
}

/// The two streams of the mode.
#[derive(Default)]
pub struct Out {
    stdout: Stdout,
}

impl Out {
    /// One line (a `'\n'` is appended) to stdout.
    pub fn line(&mut self, mut line: Vec<u8>) {
        line.push(b'\n');
        self.stdout.fwrite(&line);
    }

    /// One line (a `'\n'` is appended) to stderr, written at once. A write error aborts, as the
    /// throwing `std::println(stderr, …)` does.
    // a method although stderr is unbuffered: every message of the mode goes through `Out`
    #[allow(clippy::unused_self)]
    pub fn err(&mut self, mut line: Vec<u8>) {
        line.push(b'\n');
        if !write_fd(2, &line) {
            abort_now();
        }
    }

    /// The flush at normal exit.
    pub fn flush(&mut self) {
        self.stdout.flush();
    }
}
