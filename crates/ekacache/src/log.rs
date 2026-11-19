//! `$JIG_LOG` and `$JIG_LOG_ARGS` (`behaviors/jig-dispatch.md §Logging`, `base.cc:293-328`,
//! `cc_mode.cc:434-443`).
//!
//! Each line is appended with one `write` to a file opened `O_WRONLY|O_APPEND|O_CREAT`, mode
//! `0666 & ~umask` (`std::ofstream(path, std::ios::app)`). A file that cannot be opened is
//! skipped silently.

use std::io::Write;
use std::time::Instant;

use ekacache_core::bytes::{join, os_path};

use crate::process::ProcEnv;

/// `Outcome` (`keys.h`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Hit,
    HitFail,
    MissStored,
    MissStoredFail,
    MissFail,
    MissUnstored,
    PlainCompile,
    PlainLink,
    PlainNoSource,
    PlainQuery,
    PlainNoSocket,
}

impl Outcome {
    /// `OutcomeName`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Hit => "cached",
            Self::HitFail => "cached-error",
            Self::MissStored => "compiled",
            Self::MissStoredFail => "compiled-error",
            Self::MissFail => "failed",
            Self::MissUnstored => "compiled-unstored",
            Self::PlainCompile => "uncacheable",
            Self::PlainLink => "linked",
            Self::PlainNoSource => "uncacheable-nosrc",
            Self::PlainQuery => "query",
            Self::PlainNoSocket => "no-daemon",
        }
    }
}

/// Wall time since a mode started (`Stopwatch`).
#[derive(Clone, Copy, Debug)]
pub struct Stopwatch(Instant);

impl Stopwatch {
    #[must_use]
    pub fn start() -> Self {
        Self(Instant::now())
    }

    #[must_use]
    pub fn elapsed_ms(&self) -> f64 {
        self.0.elapsed().as_secs_f64() * 1000.0
    }
}

/// `std::format("{:.0f}", ms)`: both Rust and C++ round the exact binary value, half to even.
#[must_use]
pub fn format_ms(ms: f64) -> String {
    format!("{ms:.0}")
}

/// Append `line` to the file at `path` with one `write`.
fn append(path: &[u8], line: &[u8]) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(os_path(path))
    {
        let _ = f.write_all(line);
    }
}

/// `LogOutcome(tool, outcome, subject, clock)`: `<tool>\t<outcome>\t<subject>\t<ms>\n` to
/// `$JIG_LOG` when set and non-empty. jig never changes `JIG_LOG`, so reading it per call is the
/// same as legacy's read-once `static`.
pub fn outcome(env: &ProcEnv, tool: &str, outcome: Outcome, subject: &[u8], clock: &Stopwatch) {
    let Some(path) = env.non_empty("JIG_LOG") else {
        return;
    };
    let mut line = Vec::with_capacity(subject.len() + 40);
    line.extend_from_slice(tool.as_bytes());
    line.push(b'\t');
    line.extend_from_slice(outcome.name().as_bytes());
    line.push(b'\t');
    line.extend_from_slice(subject);
    line.push(b'\t');
    line.extend_from_slice(format_ms(clock.elapsed_ms()).as_bytes());
    line.push(b'\n');
    append(path, &line);
}

/// `LogArgs` (cc only): `<outcome>\t<user args joined by " ">\n` to `$JIG_LOG_ARGS` when set and
/// non-empty, except for queries.
pub fn args<A: AsRef<[u8]>>(env: &ProcEnv, outcome: Outcome, user_args: &[A]) {
    let Some(path) = env.non_empty("JIG_LOG_ARGS") else {
        return;
    };
    if outcome == Outcome::PlainQuery {
        return;
    }
    let mut line = outcome.name().as_bytes().to_vec();
    line.push(b'\t');
    line.extend_from_slice(&join(user_args, b" "));
    line.push(b'\n');
    append(path, &line);
}

/// ADOPTED Q-CLIENT-2: `jig: cache put <key> failed`, on stderr.
pub fn put_failed(key: &[u8]) {
    let mut msg = b"jig: cache put ".to_vec();
    msg.extend_from_slice(key);
    msg.extend_from_slice(b" failed\n");
    let _ = std::io::stderr().write_all(&msg);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ekacache_core::env::Env;

    #[test]
    fn rounding_matches_cxx_format() {
        assert_eq!(format_ms(0.5), "0");
        assert_eq!(format_ms(1.5), "2");
        assert_eq!(format_ms(2.5), "2");
        assert_eq!(format_ms(2.500_000_1), "3");
        assert_eq!(format_ms(0.0), "0");
        assert_eq!(format_ms(1234.4), "1234");
    }

    #[test]
    fn names() {
        assert_eq!(Outcome::MissUnstored.name(), "compiled-unstored");
        assert_eq!(Outcome::PlainNoSocket.name(), "no-daemon");
    }

    #[test]
    fn lines() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let la = dir.path().join("args");
        let env = ProcEnv::new(Env::from_pairs(&[
            ("JIG_LOG", log.to_str().unwrap()),
            ("JIG_LOG_ARGS", la.to_str().unwrap()),
        ]));
        let clock = Stopwatch::start();
        outcome(&env, "cc", Outcome::Hit, b"a.c", &clock);
        outcome(&env, "rustc", Outcome::MissStored, b"lib.rs", &clock);
        let text = std::fs::read_to_string(&log).unwrap();
        let rows: Vec<Vec<&str>> = text.lines().map(|l| l.split('\t').collect()).collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][..3], ["cc", "cached", "a.c"]);
        assert_eq!(rows[1][..3], ["rustc", "compiled", "lib.rs"]);
        assert!(rows[0][3].parse::<u64>().is_ok());
        args(&env, Outcome::PlainQuery, &[b"-v"]);
        args(&env, Outcome::PlainLink, &[&b"-o"[..], b"x", b"a.o"]);
        assert_eq!(std::fs::read(&la).unwrap(), b"linked\t-o x a.o\n");
        // unset or empty: nothing
        let env = ProcEnv::new(Env::from_pairs(&[("JIG_LOG", "")]));
        outcome(&env, "cc", Outcome::Hit, b"a.c", &clock);
    }
}
