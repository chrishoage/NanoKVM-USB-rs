//! The video handoff: **at most one pending owned frame** (§5.2).
//!
//! §5.2 states the requirement and explicitly leaves the data structure open: "whether that is a
//! mutex-guarded slot, a rendezvous channel, or a capacity-1 channel with replace-on-full is an
//! implementation choice". This is the mutex-guarded slot, chosen because it gives the producer
//! the displaced value back — which is what lets the pipeline count a drop *and* recycle the
//! displaced frame's allocation instead of freeing and reallocating an 8 MB RGBA buffer.
//!
//! Anything that can hold two frames accumulates staleness by design, so [`Slot::put`] never
//! blocks and never queues: it replaces.
//!
//! Lock poisoning: the module policy in `capture::lock_or_recover`. A `Slot` guards an
//! `Option<T>` and three
//! counters; there is no invariant that an unwind can break, and refusing to serve frames
//! because some other thread panicked would violate §6.1 S1-1.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use super::lock_or_recover;

struct State<T> {
    value: Option<T>,
    closed: bool,
    dropped: u64,
}

/// A one-deep replace-on-full handoff between two threads (§5.2).
///
/// Share it as an `Arc<Slot<T>>`: `&self` on every method, so producer and consumer hold the
/// same object.
pub struct Slot<T> {
    state: Mutex<State<T>>,
    ready: Condvar,
}

impl<T> Slot<T> {
    /// An empty, open slot.
    pub fn new() -> Self {
        Slot {
            state: Mutex::new(State {
                value: None,
                closed: false,
                dropped: 0,
            }),
            ready: Condvar::new(),
        }
    }

    /// Put `v` in the slot, displacing whatever was pending.
    ///
    /// Returns the displaced value, if any. That return is not incidental: the caller counts it
    /// as a drop (§5.5 tracks "frames dropped pre-decode" and "post-decode" separately) and can
    /// reuse its allocation for the next frame.
    ///
    /// On a closed slot the value is handed straight back — the consumer is gone, so accepting
    /// it would only leak staleness. That is *not* counted as a drop, because nothing was
    /// displaced; [`Slot::is_closed`] is how a producer distinguishes the two.
    ///
    /// Never blocks.
    pub fn put(&self, v: T) -> Option<T> {
        let mut st = lock_or_recover(&self.state);
        if st.closed {
            return Some(v);
        }
        let displaced = st.value.replace(v);
        if displaced.is_some() {
            st.dropped += 1;
        }
        drop(st);
        // Exactly one waiter can take the value, but notify_all keeps this correct if a second
        // consumer is ever added.
        self.ready.notify_all();
        displaced
    }

    /// Take the pending value if there is one. Never blocks. This is the renderer's call:
    /// "render takes whatever is pending — no catch-up, no queue drain" (§5.2).
    pub fn take(&self) -> Option<T> {
        lock_or_recover(&self.state).value.take()
    }

    /// Take the pending value, waiting up to `timeout` for one to arrive.
    ///
    /// Returns `None` on timeout **and** on close. A caller that must tell them apart checks
    /// [`Slot::is_closed`]; the decode thread does exactly that to decide between looping and
    /// exiting.
    pub fn wait_take(&self, timeout: Duration) -> Option<T> {
        let deadline = Instant::now().checked_add(timeout);
        let mut st = lock_or_recover(&self.state);
        loop {
            if let Some(v) = st.value.take() {
                return Some(v);
            }
            if st.closed {
                return None;
            }
            let remaining = match deadline {
                // `checked_add` overflowed: an effectively infinite timeout was asked for.
                None => timeout,
                Some(d) => match d.checked_duration_since(Instant::now()) {
                    Some(r) if !r.is_zero() => r,
                    _ => return None,
                },
            };
            // Spurious wakeups are handled by looping on the deadline rather than trusting
            // `WaitTimeoutResult`.
            let (guard, _) = self
                .ready
                .wait_timeout(st, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            st = guard;
        }
    }

    /// Close the slot: every waiter wakes and returns `None`, and further [`Slot::put`]s hand
    /// the value straight back. Used for shutdown (`PipelineHandle::stop`) and by the capture
    /// thread when the device disconnects, so the decode thread does not sit in a `wait_take`
    /// nobody will ever satisfy.
    ///
    /// Idempotent. Any value still pending is dropped here.
    pub fn close(&self) {
        {
            let mut st = lock_or_recover(&self.state);
            st.closed = true;
            st.value = None;
        }
        self.ready.notify_all();
    }

    /// Whether [`Slot::close`] has been called.
    pub fn is_closed(&self) -> bool {
        lock_or_recover(&self.state).closed
    }

    /// How many values have been displaced by [`Slot::put`] over this slot's lifetime — the
    /// drop counter §5.5 asks to track and report.
    pub fn dropped(&self) -> u64 {
        lock_or_recover(&self.state).dropped
    }

    /// Whether a value is pending right now. Advisory: it can change the instant it returns.
    pub fn is_pending(&self) -> bool {
        lock_or_recover(&self.state).value.is_some()
    }
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn put_on_an_empty_slot_displaces_nothing() {
        let s: Slot<u32> = Slot::new();
        assert_eq!(s.put(1), None);
        assert_eq!(s.dropped(), 0);
    }

    #[test]
    fn put_on_a_full_slot_displaces_and_counts() {
        let s: Slot<u32> = Slot::new();
        assert_eq!(s.put(1), None);
        assert_eq!(
            s.put(2),
            Some(1),
            "the OLD value comes back, not the new one"
        );
        assert_eq!(s.put(3), Some(2));
        assert_eq!(s.dropped(), 2);
        assert_eq!(s.take(), Some(3), "the newest value is what is pending");
    }

    #[test]
    fn take_empties_the_slot() {
        let s: Slot<u32> = Slot::new();
        s.put(9);
        assert!(s.is_pending());
        assert_eq!(s.take(), Some(9));
        assert!(!s.is_pending());
        assert_eq!(s.take(), None);
        assert_eq!(s.dropped(), 0);
    }

    #[test]
    fn wait_take_returns_immediately_when_a_value_is_already_pending() {
        let s: Slot<u32> = Slot::new();
        s.put(4);
        let t = Instant::now();
        assert_eq!(s.wait_take(Duration::from_secs(30)), Some(4));
        assert!(t.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_waiter_wakes_on_put() {
        let s = Arc::new(Slot::<u32>::new());
        let (armed_tx, armed_rx) = mpsc::channel();
        let (got_tx, got_rx) = mpsc::channel();
        let s2 = Arc::clone(&s);
        let h = thread::spawn(move || {
            armed_tx.send(()).expect("armed");
            let v = s2.wait_take(Duration::from_secs(30));
            got_tx.send(v).expect("got");
        });
        // Rendezvous: the waiter says it is about to wait. The 30 s timeout is a deadlock
        // guard, not the synchronisation — the assertion below is on the value, not on time.
        armed_rx.recv().expect("waiter armed");
        s.put(42);
        assert_eq!(
            got_rx.recv_timeout(Duration::from_secs(10)),
            Ok(Some(42)),
            "waiter did not wake on put"
        );
        h.join().expect("join");
    }

    #[test]
    fn a_waiter_wakes_on_close() {
        let s = Arc::new(Slot::<u32>::new());
        let (armed_tx, armed_rx) = mpsc::channel();
        let (got_tx, got_rx) = mpsc::channel();
        let s2 = Arc::clone(&s);
        let h = thread::spawn(move || {
            armed_tx.send(()).expect("armed");
            let t = Instant::now();
            let v = s2.wait_take(Duration::from_secs(300));
            got_tx.send((v, t.elapsed())).expect("got");
        });
        armed_rx.recv().expect("waiter armed");
        s.close();
        let (v, elapsed) = got_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("waiter did not wake on close");
        assert_eq!(v, None);
        assert!(
            elapsed < Duration::from_secs(10),
            "woke only after {elapsed:?}, i.e. not because of close()"
        );
        h.join().expect("join");
    }

    #[test]
    fn wait_take_times_out_and_says_so() {
        let s: Slot<u32> = Slot::new();
        let t = Instant::now();
        assert_eq!(s.wait_take(Duration::from_millis(30)), None);
        assert!(t.elapsed() >= Duration::from_millis(25));
        assert!(!s.is_closed(), "a timeout must not look like a close");
    }

    #[test]
    fn put_on_a_closed_slot_hands_the_value_back_uncounted() {
        let s: Slot<u32> = Slot::new();
        s.close();
        assert_eq!(s.put(5), Some(5));
        assert_eq!(s.dropped(), 0);
        assert!(s.is_closed());
        assert_eq!(s.take(), None);
    }

    #[test]
    fn close_is_idempotent_and_drops_the_pending_value() {
        let s: Slot<u32> = Slot::new();
        s.put(1);
        s.close();
        s.close();
        assert_eq!(s.take(), None);
    }

    /// §5.2's real invariant: a frame is delivered exactly once or dropped exactly once, never
    /// both and never twice. If `put` ever duplicated a value the renderer could show the same
    /// frame twice while a newer one was freed.
    #[test]
    fn no_value_is_ever_duplicated_or_lost() {
        const N: u32 = 5_000;
        let s = Arc::new(Slot::<u32>::new());
        let s2 = Arc::clone(&s);

        let consumer = thread::spawn(move || {
            let mut taken: Vec<u32> = Vec::new();
            while !s2.is_closed() {
                if let Some(v) = s2.wait_take(Duration::from_millis(20)) {
                    taken.push(v);
                }
            }
            taken
        });

        let mut displaced: Vec<u32> = Vec::new();
        for v in 1..=N {
            if let Some(old) = s.put(v) {
                displaced.push(old);
            }
        }
        // Rendezvous rather than sleep: wait for the consumer to drain the last value, so
        // `close()` discards nothing and the accounting below is exact.
        let deadline = Instant::now() + Duration::from_secs(10);
        while s.is_pending() {
            assert!(Instant::now() < deadline, "consumer never drained the slot");
            thread::yield_now();
        }
        s.close();
        let mut taken = consumer.join().expect("consumer");

        assert_eq!(
            taken.len() + displaced.len(),
            N as usize,
            "values were duplicated or lost: {} taken, {} displaced",
            taken.len(),
            displaced.len()
        );
        // Values come out in order and never repeat: a consumer never sees an older value than
        // one it has already seen.
        let mut sorted = taken.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), taken.len(), "a value was delivered twice");
        taken.sort_unstable();
        let mut all: Vec<u32> = taken;
        all.extend_from_slice(&displaced);
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), N as usize, "a value appeared in both lists");
        assert_eq!(s.dropped(), displaced.len() as u64);
    }

    #[test]
    fn a_consumer_never_sees_an_older_value_after_a_newer_one() {
        let s = Arc::new(Slot::<u32>::new());
        let s2 = Arc::clone(&s);
        let (armed_tx, armed_rx) = mpsc::channel();
        let consumer = thread::spawn(move || {
            armed_tx.send(()).expect("armed");
            let mut last = 0u32;
            let mut seen = 0usize;
            while !s2.is_closed() || s2.is_pending() {
                if let Some(v) = s2.wait_take(Duration::from_millis(20)) {
                    assert!(v > last, "slot handed back {v} after {last}");
                    last = v;
                    seen += 1;
                }
            }
            seen
        });
        armed_rx.recv().expect("armed");
        for v in 1..=2_000u32 {
            s.put(v);
        }
        s.close();
        let seen = consumer.join().expect("consumer");
        assert!(seen > 0, "consumer saw nothing at all");
    }
}
