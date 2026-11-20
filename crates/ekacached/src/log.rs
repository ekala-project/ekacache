//! ecd's log: Go's standard logger with default flags (`ecd-daemon.md` §Logging;
//! `implementation/03-ecd-server.md` §Logging and error text).
//!
//! Each line is `YYYY/MM/DD HH:MM:SS ` in local time, the message, and `\n` if the message does
//! not end in one, written to stderr in one `write`. Messages are bytes: a key or path in a log
//! line need not be UTF-8. Tests swap the sink for a capture.

use std::fmt;
use std::io::Write;
use std::sync::{Arc, Mutex, PoisonError};

type Sink = dyn Fn(&[u8]) + Send + Sync;

/// Where log lines go. Cheap to clone; every clone writes to the same sink.
#[derive(Clone)]
pub struct Logger {
    sink: Arc<Sink>,
}

impl fmt::Debug for Logger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Logger")
    }
}

impl Default for Logger {
    fn default() -> Self {
        Logger::stderr()
    }
}

impl Logger {
    /// Timestamped lines on stderr, serialised like Go's logger.
    pub fn stderr() -> Logger {
        static LOCK: Mutex<()> = Mutex::new(());
        Logger {
            sink: Arc::new(|msg: &[u8]| {
                let line = format_line(now_unix_secs(), msg);
                let _guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
                let _ = std::io::stderr().lock().write_all(&line);
            }),
        }
    }

    /// A logger that collects messages (without timestamps) for a test to inspect.
    pub fn capture() -> (Logger, LogCapture) {
        let lines: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let sink_lines = Arc::clone(&lines);
        let logger = Logger {
            sink: Arc::new(move |msg: &[u8]| {
                sink_lines
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(msg.to_vec());
            }),
        };
        (logger, LogCapture { lines })
    }

    /// One log line (the message without timestamp or trailing newline).
    pub fn log(&self, msg: &[u8]) {
        (self.sink)(msg);
    }

    /// One log line from format arguments (for messages that carry no raw bytes).
    pub fn logf(&self, args: fmt::Arguments<'_>) {
        self.log(fmt::format(args).as_bytes());
    }
}

/// The messages a capture logger received.
#[derive(Clone, Default)]
pub struct LogCapture {
    lines: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl LogCapture {
    pub fn lines(&self) -> Vec<Vec<u8>> {
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The messages as (lossy) strings, for assertions.
    pub fn strings(&self) -> Vec<String> {
        self.lines()
            .iter()
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .collect()
    }

    pub fn clear(&self) {
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

fn now_unix_secs() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

/// `log.Printf` output for one message at `secs`: header, message, newline if missing.
pub fn format_line(secs: i64, msg: &[u8]) -> Vec<u8> {
    let mut line = Vec::with_capacity(20 + msg.len() + 1);
    let (y, mo, d, h, mi, s) = local_time(secs);
    line.extend_from_slice(format!("{y:04}/{mo:02}/{d:02} {h:02}:{mi:02}:{s:02} ").as_bytes());
    line.extend_from_slice(msg);
    if !msg.ends_with(b"\n") {
        line.push(b'\n');
    }
    line
}

/// Local calendar time of a Unix second: (year, month, day, hour, minute, second).
#[allow(unsafe_code)]
fn local_time(secs: i64) -> (i64, i64, i64, i64, i64, i64) {
    let time: libc::time_t = secs;
    // SAFETY: `libc::tm` is a plain C struct; all-zero bytes (including a null `tm_zone`) is a
    // valid value of it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the duration of the call, and `localtime_r` is the
    // reentrant variant that writes only into `tm`.
    let res = unsafe { libc::localtime_r(&raw const time, &raw mut tm) };
    if res.is_null() {
        // out of range for the C library: fall back to UTC arithmetic
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        return (year, month, day, rem / 3600, rem % 3600 / 60, rem % 60);
    }
    (
        i64::from(tm.tm_year) + 1900,
        i64::from(tm.tm_mon) + 1,
        i64::from(tm.tm_mday),
        i64::from(tm.tm_hour),
        i64::from(tm.tm_min),
        i64::from(tm.tm_sec),
    )
}

/// Proleptic Gregorian date of a day count since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_shape_matches_go() {
        let line = format_line(1_700_000_000, b"listening on /s, 4 slots, keys=0");
        let text = String::from_utf8(line).unwrap();
        // 2023/11/14 hh:mm:ss in whatever zone the test runs in
        assert_eq!(
            text.len(),
            20 + "listening on /s, 4 slots, keys=0".len() + 1
        );
        assert!(
            text.ends_with(" listening on /s, 4 slots, keys=0\n"),
            "{text:?}"
        );
        let b = text.as_bytes();
        assert_eq!(
            (b[4], b[7], b[10], b[13], b[16], b[19]),
            (b'/', b'/', b' ', b':', b':', b' ')
        );
        // a message that already ends in a newline gets no second one
        assert!(format_line(0, b"x\n").ends_with(b" x\n"));
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_675), (2023, 11, 14));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn capture_collects_messages() {
        let (logger, cap) = Logger::capture();
        logger.log(b"pack 000001: x");
        logger.logf(format_args!(
            "evicted pack {:06} ({} MiB, {} MiB live)",
            2, 0, 0
        ));
        assert_eq!(
            cap.strings(),
            ["pack 000001: x", "evicted pack 000002 (0 MiB, 0 MiB live)"]
        );
    }
}
