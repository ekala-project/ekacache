//! Host-wide compiler slots (`behaviors/jigd-slots.md` §State, §Operations, §Per-connection
//! bookkeeping; model `spec/slots.qnt`, module `slots`; `implementation/03-jigd-server.md`
//! §SLOT with cancellation).
//!
//! `acquire`, `release` and `grant` are the Go code (`slots.go`) line by line: an immediate grant
//! needs a free token **and** an empty queue, and `grant` serves the waiter whose build holds the
//! fewest tokens, scanning left to right with a strict `<` (FIFO among equals) and re-reading the
//! counts on every iteration.
//!
//! Adopted Q-SLOT-1: a queued waiter has an id, and the connection that sees its peer hang up
//! calls [`Slots::cancel`], which removes the waiter under the lock (no token, no `OK`). If the
//! grant took the lock first, `cancel` returns false and the token is the connection's, as in Go.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, MutexGuard, PoisonError};

use tokio::sync::oneshot;

/// The slot pool: `limit` tokens shared by every connection.
#[derive(Debug)]
pub struct Slots {
    limit: i64,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// tokens handed out
    out: i64,
    /// tokens per build; a key is deleted when its count drops to 0 or below
    held: HashMap<Vec<u8>, i64>,
    /// arrival order
    waiting: Vec<Waiter>,
    next_id: u64,
}

#[derive(Debug)]
struct Waiter {
    id: u64,
    build: Vec<u8>,
    ready: oneshot::Sender<()>,
}

/// The outcome of [`Slots::acquire`].
#[derive(Debug)]
pub enum Acquire {
    /// A token was free and nobody was queued: the caller holds it now.
    Granted,
    /// Queued. The token is the caller's once `ready` fires (`grant` counted it already), or
    /// after [`Slots::cancel`] returned false.
    Queued(Ticket),
}

/// A queued request.
#[derive(Debug)]
pub struct Ticket {
    pub id: u64,
    pub ready: oneshot::Receiver<()>,
}

/// The state, for tests and the model replay.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SlotsSnapshot {
    pub out: i64,
    pub held: BTreeMap<Vec<u8>, i64>,
    /// `(waiter id, build)` in queue order
    pub waiting: Vec<(u64, Vec<u8>)>,
}

impl Slots {
    /// `NewSlots(limit)`.
    pub fn new(limit: i64) -> Slots {
        Slots {
            limit,
            state: Mutex::new(State::default()),
        }
    }

    pub fn limit(&self) -> i64 {
        self.limit
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `Acquire(build)` up to the point where Go blocks.
    pub fn acquire(&self, build: &[u8]) -> Acquire {
        let mut s = self.lock();
        if s.out < self.limit && s.waiting.is_empty() {
            s.out += 1;
            *s.held.entry(build.to_vec()).or_insert(0) += 1;
            return Acquire::Granted;
        }
        let (tx, rx) = oneshot::channel();
        s.next_id += 1;
        let id = s.next_id;
        s.waiting.push(Waiter {
            id,
            build: build.to_vec(),
            ready: tx,
        });
        Acquire::Queued(Ticket { id, ready: rx })
    }

    /// `Release(build)`: no guard; the caller only releases tokens it holds.
    pub fn release(&self, build: &[u8]) {
        let mut s = self.lock();
        s.out -= 1;
        let gone = match s.held.get_mut(build) {
            Some(n) => {
                *n -= 1;
                *n <= 0
            }
            // Go: `held[build]--` on a missing key stores -1, then deletes it
            None => false,
        };
        if gone {
            s.held.remove(build);
        }
        self.grant(&mut s);
    }

    /// Adopted Q-SLOT-1: remove a queued waiter whose peer has gone. True if it was still
    /// queued (it holds nothing); false if `grant` already handed it a token.
    pub fn cancel(&self, id: u64) -> bool {
        let mut s = self.lock();
        match s.waiting.iter().position(|w| w.id == id) {
            Some(i) => {
                s.waiting.remove(i);
                true
            }
            None => false,
        }
    }

    /// `grant()`, with the lock held.
    fn grant(&self, s: &mut State) {
        while s.out < self.limit && !s.waiting.is_empty() {
            let count = |s: &State, b: &[u8]| s.held.get(b).copied().unwrap_or(0);
            let mut best = 0;
            for i in 0..s.waiting.len() {
                if count(s, &s.waiting[i].build) < count(s, &s.waiting[best].build) {
                    best = i;
                }
            }
            let w = s.waiting.remove(best);
            s.out += 1;
            *s.held.entry(w.build.clone()).or_insert(0) += 1;
            if w.ready.send(()).is_err() {
                // The waiter's connection task is gone without cancelling (it cannot happen in
                // the server, which always cancels first). Go would grant it and release it
                // when the `OK` write fails; release it at once instead, so no token leaks.
                s.out -= 1;
                if let Some(n) = s.held.get_mut(&w.build) {
                    *n -= 1;
                    if *n <= 0 {
                        s.held.remove(&w.build);
                    }
                }
            }
        }
    }

    /// `Stats()`: `(out, len(waiting))`.
    pub fn stats(&self) -> (i64, usize) {
        let s = self.lock();
        (s.out, s.waiting.len())
    }

    pub fn snapshot(&self) -> SlotsSnapshot {
        let s = self.lock();
        SlotsSnapshot {
            out: s.out,
            held: s.held.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            waiting: s.waiting.iter().map(|w| (w.id, w.build.clone())).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued(a: Acquire) -> Ticket {
        match a {
            Acquire::Queued(t) => t,
            Acquire::Granted => panic!("granted at once"),
        }
    }

    /// `TestSlotsFewestHeldFirst`: with the pool exhausted, the build holding fewer tokens is
    /// woken first, whatever the arrival order.
    #[test]
    fn fewest_held_first() {
        let s = Slots::new(2);
        assert!(matches!(s.acquire(b"big"), Acquire::Granted));
        assert!(matches!(s.acquire(b"big"), Acquire::Granted));
        let mut big = queued(s.acquire(b"big"));
        let mut small = queued(s.acquire(b"small"));
        assert_eq!(s.stats(), (2, 2));
        s.release(b"big");
        assert!(small.ready.try_recv().is_ok(), "small is woken first");
        assert!(big.ready.try_recv().is_err());
        s.release(b"big");
        assert!(big.ready.try_recv().is_ok());
        assert_eq!(s.stats(), (2, 0));
    }

    #[test]
    fn fifo_among_equals_and_immediate_grant_needs_empty_queue() {
        let s = Slots::new(1);
        assert!(matches!(s.acquire(b"a"), Acquire::Granted));
        let mut b = queued(s.acquire(b"b"));
        let mut c = queued(s.acquire(b"c"));
        s.release(b"a");
        assert!(b.ready.try_recv().is_ok());
        assert!(c.ready.try_recv().is_err());
        assert_eq!(
            s.snapshot(),
            SlotsSnapshot {
                out: 1,
                held: [(b"b".to_vec(), 1)].into(),
                waiting: vec![(c.id, b"c".to_vec())],
            }
        );
        s.release(b"b");
        assert!(c.ready.try_recv().is_ok());
        s.release(b"c");
        assert_eq!(s.snapshot(), SlotsSnapshot::default());
    }

    #[test]
    fn cancel_removes_a_waiter_without_a_token() {
        let s = Slots::new(1);
        assert!(matches!(s.acquire(b"a"), Acquire::Granted));
        let b = queued(s.acquire(b"b"));
        let mut c = queued(s.acquire(b"c"));
        assert!(s.cancel(b.id));
        assert!(!s.cancel(b.id));
        assert_eq!(s.stats(), (1, 1));
        s.release(b"a");
        assert!(c.ready.try_recv().is_ok());
        // the grant won: cancel is too late, the token is the caller's
        assert!(!s.cancel(c.id));
        assert_eq!(s.stats(), (1, 0));
    }

    #[test]
    fn a_dropped_waiter_does_not_leak_a_token() {
        let s = Slots::new(1);
        assert!(matches!(s.acquire(b"a"), Acquire::Granted));
        drop(queued(s.acquire(b"b")));
        let mut c = queued(s.acquire(b"c"));
        s.release(b"a");
        assert!(c.ready.try_recv().is_ok());
        assert_eq!(s.stats(), (1, 0));
        assert_eq!(s.snapshot().held, [(b"c".to_vec(), 1)].into());
    }
}
