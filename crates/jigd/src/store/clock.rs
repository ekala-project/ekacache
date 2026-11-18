//! The store's clock (`jigd-store.md` §Recency). `used`/`touched` and pack mtimes are Unix
//! nanoseconds; the Go tests swap `Store.now` for a virtual clock
//! (`TestEvictRecencySurvivesRestart`), and the Quint replay drives an integer clock.

use std::sync::atomic::{AtomicI64, Ordering};

/// A source of "now" in Unix nanoseconds.
pub trait Clock: Send + Sync {
    fn now_ns(&self) -> i64;
}

/// The real clock (`time.Now`).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ns(&self) -> i64 {
        match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
            Err(e) => -i64::try_from(e.duration().as_nanos()).unwrap_or(i64::MAX),
        }
    }
}

/// A clock for tests: either fixed at a settable instant, or the system clock plus an offset.
#[derive(Debug, Default)]
pub struct TestClock {
    fixed: AtomicI64,
    offset: AtomicI64,
    follow_system: std::sync::atomic::AtomicBool,
}

impl TestClock {
    /// A clock stopped at `ns`.
    pub fn fixed(ns: i64) -> TestClock {
        TestClock {
            fixed: AtomicI64::new(ns),
            ..TestClock::default()
        }
    }

    /// The system clock shifted by `offset_ns` (changeable later with `set_offset`).
    pub fn system_offset(offset_ns: i64) -> TestClock {
        let c = TestClock {
            offset: AtomicI64::new(offset_ns),
            ..TestClock::default()
        };
        c.follow_system.store(true, Ordering::SeqCst);
        c
    }

    pub fn set(&self, ns: i64) {
        self.follow_system.store(false, Ordering::SeqCst);
        self.fixed.store(ns, Ordering::SeqCst);
    }

    pub fn advance(&self, ns: i64) {
        self.fixed.fetch_add(ns, Ordering::SeqCst);
    }

    pub fn set_offset(&self, offset_ns: i64) {
        self.offset.store(offset_ns, Ordering::SeqCst);
        self.follow_system.store(true, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_ns(&self) -> i64 {
        if self.follow_system.load(Ordering::SeqCst) {
            SystemClock
                .now_ns()
                .saturating_add(self.offset.load(Ordering::SeqCst))
        } else {
            self.fixed.load(Ordering::SeqCst)
        }
    }
}
