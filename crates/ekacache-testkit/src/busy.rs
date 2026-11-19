//! Running a program the test has just written or copied.
//!
//! Tests run in parallel threads. If one thread forks while another still holds a freshly
//! written executable open for writing, the child inherits that fd until it execs, and an exec
//! of the file in the meantime fails with `ETXTBSY`. The window is short, so retrying the spawn
//! is the usual fix.

use std::io;
use std::process::{Child, Command, ExitStatus, Output};
use std::time::Duration;

const TRIES: u32 = 100;

fn retry<T>(mut f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut left = TRIES;
    loop {
        match f() {
            Err(e) if e.kind() == io::ErrorKind::ExecutableFileBusy && left > 1 => {
                left -= 1;
                std::thread::sleep(Duration::from_millis(10));
            }
            r => return r,
        }
    }
}

/// `Command::{output, spawn, status}` that retry while the program is `ETXTBSY`.
pub trait CommandBusyExt {
    /// [`Command::output`], retried on `ETXTBSY`.
    ///
    /// # Errors
    /// As [`Command::output`].
    fn output_busy(&mut self) -> io::Result<Output>;
    /// [`Command::spawn`], retried on `ETXTBSY`.
    ///
    /// # Errors
    /// As [`Command::spawn`].
    fn spawn_busy(&mut self) -> io::Result<Child>;
    /// [`Command::status`], retried on `ETXTBSY`.
    ///
    /// # Errors
    /// As [`Command::status`].
    fn status_busy(&mut self) -> io::Result<ExitStatus>;
}

impl CommandBusyExt for Command {
    fn output_busy(&mut self) -> io::Result<Output> {
        retry(|| self.output())
    }
    fn spawn_busy(&mut self) -> io::Result<Child> {
        retry(|| self.spawn())
    }
    fn status_busy(&mut self) -> io::Result<ExitStatus> {
        retry(|| self.status())
    }
}
