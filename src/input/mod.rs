//! Input correctness: event classes, coalescing, held-state projection and release-all
//! cancellation. Implements §2 of `docs/NATIVE_CLIENT_PLAN.md` in full.
//!
//! # Shape
//!
//! ```text
//!   viewer thread ──submit()──▶ [ one queue, one order ] ──▶ writer thread ──▶ Link
//!         │                            §2.1–2.4                  §2.5, §2.6
//!         └──request_release_all()──▶ 1-slot cancellation flag ──────┘
//! ```
//!
//! - **One queue, one order** (§2.1). Keyboard and mouse share it, or ordering between a modifier
//!   press and a click is lost and shift-click breaks.
//! - **Coalescing is a correctness requirement, not an optimisation** (§5.1): the device
//!   acknowledges roughly 83 absolute mouse reports per second and overload silently corrupts
//!   rather than applying backpressure.
//! - **The writer is the sole serialization point** (§2.6). It applies the epoch check in the same
//!   loop iteration as the write, so there is no window in which a stale event can slip past a
//!   cancellation.
//! - **Cancellation invalidates, it does not merely outrank** (§2.6). Triggers never enqueue
//!   anything, which is how a release stays schedulable when the queue is full.
//! - **Overflow is an explicit failure** (§2.8), never a silent drop of a transition.
//!
//! # Not in Stage 1
//!
//! Reconnect (§2.7) is Stage 2. The writer's link is boxed so a transport can be swapped under it
//! later without changing this API. Blocking, paced admission for scripts (§2.9) is not
//! implemented either; [`Producer::submit`] is the viewer's non-blocking policy.

mod held;
mod queue;
mod shared;
mod stats;
mod writer;

#[doc(hidden)]
pub mod testing;

pub use stats::{ReleaseOutcome, ReleaseReason, ReleaseRecord, Stats};

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::input::queue::EntryKind;
use crate::input::shared::{lock, Shared};
use crate::input::stats::Counters;
use crate::link::Link;
use crate::proto::report::{HidKey, ABS_MAX};

/// One input event as the viewer produces it (§2.2).
///
/// The epoch an event was produced under is attached inside [`Producer::submit`], so callers
/// cannot forge or stale one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A key transition. **Barrier**: never dropped, never merged, never reordered (§2.2). Host
    /// auto-repeat must be discarded by the caller (§2.5); a repeat that slips through is
    /// absorbed as a no-op by the held-state tracker.
    Key { key: HidKey, down: bool },
    /// A mouse button transition — one bit from [`crate::proto::button`]. **Barrier.**
    Button { button: u8, down: bool },
    /// Absolute pointer state, already in `0..=4095`; the viewer maps pixels through
    /// [`crate::proto::abs_coord`] (§3.4). May be **replaced** by a newer value (§2.2). Values
    /// above [`ABS_MAX`] are clamped here as well, because an overshoot past 8191 silently jumps
    /// the pointer to the top-left corner of a live console.
    PointerAbs { x: u16, y: u16 },
    /// Relative pointer motion. An accumulating delta: **summed**, never replaced (§2.2), and
    /// split across reports at flush rather than clamped (§2.3).
    PointerRel { dx: i32, dy: i32 },
    /// Wheel delta. An accumulating delta, same rules as [`Event::PointerRel`].
    Wheel { delta: i32 },
}

/// Tuning for the input path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// The queue bound, counted in **barrier entries** (§2.8). Motion entries are bounded
    /// implicitly: at most one coalesced run can sit between two barriers, so the whole queue is
    /// bounded by `2 * max_barriers + 1`. A value of 0 is treated as 1.
    pub max_barriers: usize,
    /// How long each [`Link::transact`] waits for its reply. The measured ack round trip is
    /// 17 ms for a mouse report and 4.15 ms for a keyboard report (§5.1).
    pub transact_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_barriers: 256,
            transact_timeout: Duration::from_millis(100),
        }
    }
}

/// Why a submission was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SubmitError {
    /// Input is disengaged after a cancellation and stays that way until the user deliberately
    /// re-grabs and [`Producer::engage`] succeeds (§2.6, §2.8).
    #[error("input is disengaged; re-engage after the release-all is acknowledged")]
    Disengaged,
    /// Coalescing could not make room for a transition. The session has failed: a cancellation
    /// has been triggered, and input is disengaged until deliberate recapture (§2.8).
    #[error("input queue overflowed; the session was cancelled and must be re-engaged")]
    Overflow,
    /// The transport is gone (§2.6.1). Stage 1 has no reconnect (§2.7), so submissions are
    /// refused rather than queued into a link that will never drain. Reported rather than
    /// silently accepted, so input cannot wedge invisibly.
    #[error("link is down; input cannot be delivered")]
    LinkDown,
    /// The writer is shutting down (§2.6). The final release-all is the last thing that will be
    /// written, so anything submitted from here on could only land after it; and once the writer
    /// has exited nobody drains the queue at all. Permanent: there is no recapture after a
    /// shutdown.
    #[error("the input path is shutting down; no further input will be delivered")]
    ShuttingDown,
}

/// Why re-engagement was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EngageError {
    /// The writer has not yet acknowledged the requested epoch. A new session must not emit
    /// input until it observes `acked_epoch >= requested_epoch` (§2.6).
    #[error("release-all not acknowledged yet (requested {requested}, acked {acked})")]
    NotYetAcked { requested: u64, acked: u64 },
    /// A shutdown has begun (§2.6). Recapture is closed for good: the shutdown's own release-all
    /// is the last thing the writer will write, and re-engaging behind it is what would let a
    /// key-down be written after it.
    #[error("the input path is shutting down; it cannot be re-engaged")]
    ShuttingDown,
}

/// The producer half: cloneable, `Send + Sync`, non-blocking (§2.9, viewer policy).
#[derive(Debug, Clone)]
pub struct Producer {
    shared: Arc<Shared>,
}

/// Owns the writer thread. Dropping it stops the writer; [`WriterHandle::shutdown`] stops it
/// cleanly, which is a §2.6 release-all trigger.
#[derive(Debug)]
pub struct WriterHandle {
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}

/// Start the input path: one queue, one writer thread, one link (§2.1, §4.1).
///
/// The writer is a dedicated `std::thread` because every report is a blocking acknowledged round
/// trip and nothing else in the program may wait on it (§5.1, §2.9).
pub fn spawn<L: Link + 'static>(link: L, config: Config) -> (Producer, WriterHandle) {
    let shared = Arc::new(Shared::new(config));
    let writer = writer::Writer::new(Arc::clone(&shared), Box::new(link), config.transact_timeout);
    // `std::thread::spawn` rather than `Builder::spawn`: the only failure is the OS refusing a
    // thread, which is not a condition this API can meaningfully report, and the signature is
    // fixed as infallible.
    let join = std::thread::spawn(move || writer.run());
    (
        Producer {
            shared: Arc::clone(&shared),
        },
        WriterHandle {
            shared,
            join: Some(join),
        },
    )
}

impl Producer {
    /// Submit one event. Never blocks on the writer and never waits on the device (§2.9).
    ///
    /// Motion coalesces into the pending run (§2.2); a transition is enqueued as a barrier, which
    /// flushes everything ahead of it and starts a fresh accumulation (§2.3).
    ///
    /// Errors are the four ways input can be refused, and all of them are visible rather than
    /// silent: see [`SubmitError`].
    pub fn submit(&self, event: Event) -> Result<(), SubmitError> {
        let now = Instant::now();
        let counters = &self.shared.counters;
        let overflowed = {
            let mut q = lock(&self.shared.queue);
            // Checked first, and under the same lock the shutdown trigger takes, so a producer
            // can never observe the post-shutdown epoch without also observing that the path is
            // closed.
            if self.shared.shutting_down.load(Ordering::SeqCst) {
                return Err(SubmitError::ShuttingDown);
            }
            if !self.shared.engaged.load(Ordering::SeqCst) {
                return Err(SubmitError::Disengaged);
            }
            if self.shared.link_down.load(Ordering::SeqCst) {
                return Err(SubmitError::LinkDown);
            }
            // Read under the lock, so the epoch cannot be bumped between the read and the push.
            let epoch = self.shared.requested_epoch.load(Ordering::SeqCst);
            match event {
                Event::PointerAbs { x, y } => {
                    q.push_abs(x.min(ABS_MAX), y.min(ABS_MAX), epoch, now, counters);
                    false
                }
                Event::PointerRel { dx, dy } => {
                    q.push_rel(dx, dy, epoch, now, counters);
                    false
                }
                Event::Wheel { delta } => {
                    q.push_wheel(delta, epoch, now, counters);
                    false
                }
                Event::Key { key, down } => q
                    .push_barrier(EntryKind::Key { key, down }, epoch, now, counters)
                    .is_err(),
                Event::Button { button, down } => q
                    .push_barrier(
                        EntryKind::Button { mask: button, down },
                        epoch,
                        now,
                        counters,
                    )
                    .is_err(),
            }
        };

        if overflowed {
            // §2.8: the session has failed. Do not drop the transition and continue — trigger
            // cancellation, surface it, and require deliberate recapture.
            Counters::bump(&counters.overflows);
            self.shared.trigger(ReleaseReason::Overflow);
            return Err(SubmitError::Overflow);
        }
        self.shared.wake.notify_all();
        Ok(())
    }

    /// Request a release-all (§2.6): focus loss, capture release, an explicit binding, or
    /// shutdown. Bumps `requested_epoch`, sets the 1-slot cancellation flag, disengages input and
    /// wakes the writer. It enqueues nothing and it never blocks on the writer or the device,
    /// which is what keeps a release schedulable when the queue is full (§2.8).
    ///
    /// Repeated or concurrent calls coalesce into one release sequence.
    pub fn request_release_all(&self, reason: ReleaseReason) {
        self.shared.trigger(reason);
    }

    /// Re-enable submission after a cancellation — the user's deliberate re-grab (§2.6, §2.8).
    ///
    /// Succeeds only once `acked_epoch >= requested_epoch`, and never after a shutdown has begun.
    /// That acknowledgement is a **local
    /// barrier**: it means the writer discarded all stale work and either submitted the release
    /// or recorded it as unsent. It does not mean the device received anything (§2.6.1) — check
    /// [`Stats::last_release`] for the outcome and surface an [`ReleaseOutcome::Unsent`] to the
    /// user, since the target may still be holding keys.
    pub fn engage(&self) -> Result<(), EngageError> {
        // Serialised against the triggers, which mutate the same two atomics under this lock.
        let _q = lock(&self.shared.queue);
        if self.shared.shutting_down.load(Ordering::SeqCst) {
            return Err(EngageError::ShuttingDown);
        }
        let requested = self.shared.requested_epoch.load(Ordering::SeqCst);
        let acked = self.shared.acked_epoch.load(Ordering::SeqCst);
        if acked < requested {
            return Err(EngageError::NotYetAcked { requested, acked });
        }
        self.shared.engaged.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Whether [`Producer::submit`] is currently accepted.
    pub fn is_engaged(&self) -> bool {
        self.shared.engaged.load(Ordering::SeqCst)
    }

    /// Block until the epoch gate [`Producer::engage`] applies is satisfied — `acked_epoch >=
    /// requested_epoch` — or `timeout` elapses; returns whether it is. For scripts, which use
    /// blocking admission (§2.9), and for tests, which use it as a rendezvous rather than
    /// sleeping. A shutdown satisfies the gate too, and `engage` then fails with
    /// [`EngageError::ShuttingDown`]: this waits for the release, it does not promise a session.
    pub fn wait_until_engageable(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut q = lock(&self.shared.queue);
        loop {
            if self.shared.acked_epoch.load(Ordering::SeqCst)
                >= self.shared.requested_epoch.load(Ordering::SeqCst)
            {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (guard, _) = self
                .shared
                .wake
                .wait_timeout(q, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            q = guard;
        }
    }

    /// A snapshot of the instrumentation §2.8 requires: queue depth, time-in-queue, coalescing
    /// rate, overflow events, and the release-all outcome (§2.6.1). Cheap: atomics plus one small
    /// mutex.
    pub fn stats(&self) -> Stats {
        let c = &self.shared.counters;
        let load = |a: &AtomicU64| a.load(Ordering::SeqCst);
        let load_usize = |a: &AtomicUsize| a.load(Ordering::SeqCst);
        Stats {
            queue_depth: load_usize(&c.queue_depth),
            max_queue_depth: load_usize(&c.max_queue_depth),
            barrier_depth: load_usize(&c.barrier_depth),
            max_barrier_depth: load_usize(&c.max_barrier_depth),
            abs_replaced: load(&c.abs_replaced),
            rel_summed: load(&c.rel_summed),
            wheel_summed: load(&c.wheel_summed),
            kb_reports: load(&c.kb_reports),
            abs_reports: load(&c.abs_reports),
            rel_reports: load(&c.rel_reports),
            rel_truncated: load(&c.rel_truncated),
            device_errors: load(&c.device_errors),
            timeouts: load(&c.timeouts),
            overflows: load(&c.overflows),
            cancellations: load(&c.cancellations),
            stale_discarded: load(&c.stale_discarded),
            last_release: *lock(&self.shared.last_release),
            link_down: self.shared.link_down.load(Ordering::SeqCst),
            degraded: self.shared.degraded.load(Ordering::SeqCst),
            acked_epoch: load(&self.shared.acked_epoch),
            requested_epoch: load(&self.shared.requested_epoch),
            max_time_in_queue: Duration::from_nanos(load(&c.max_time_in_queue_ns)),
            last_time_in_queue: Duration::from_nanos(load(&c.last_time_in_queue_ns)),
        }
    }

    /// The configuration this path was spawned with.
    pub fn config(&self) -> Config {
        self.shared.config
    }
}

impl WriterHandle {
    /// Clean shutdown, which is a §2.6 release-all trigger — and the **last** one: nothing may be
    /// written after its release, so [`Producer::engage`] and [`Producer::submit`] fail from the
    /// moment it begins and keep failing afterwards, when nobody is draining the queue at all.
    ///
    /// Closes the path, waits for the writer to finish the release for the epoch **this call**
    /// requested, joins the thread and returns that release's outcome.
    /// [`ReleaseOutcome::Unsent`] means the frames never reached the transport and **the target
    /// may still be holding keys** (§2.6.1) — say so rather than hiding it behind cleared local
    /// state.
    ///
    /// The wait is the join: the writer exits only once it has run the sequence that clears the
    /// flag this call set, so a returned join is `acked_epoch >= E`. Joining rather than waiting on
    /// the epoch directly is deliberate — a writer that panicked mid-sequence would never advance
    /// the epoch, and this must report `Unsent` rather than hang. Both facts are checked below.
    pub fn shutdown(mut self) -> ReleaseOutcome {
        let epoch = self.shared.begin_shutdown();
        let joined = match self.join.take() {
            Some(join) => join.join().is_ok(),
            None => false,
        };
        let acked = self.shared.acked_epoch.load(Ordering::SeqCst);
        match (joined, *lock(&self.shared.last_release)) {
            // A later trigger can coalesce into this sequence, or follow it before the writer
            // exits; either way a record at `>= epoch` describes a release-all written after this
            // call asked for one, which is the fact §2.6.1 asks to be reported.
            (true, Some(record)) if acked >= epoch && record.epoch >= epoch => record.outcome,
            // A writer that panicked or never ran wrote nothing.
            _ => ReleaseOutcome::Unsent,
        }
    }

    /// The same snapshot [`Producer::stats`] returns.
    pub fn stats(&self) -> Stats {
        Producer {
            shared: Arc::clone(&self.shared),
        }
        .stats()
    }
}

impl Drop for WriterHandle {
    /// A handle dropped without [`WriterHandle::shutdown`] still asks for a release-all and stops
    /// the writer, but does not join: a drop must not block on a wedged transport. It closes the
    /// path exactly as `shutdown` does, so a producer that survives the handle cannot re-engage
    /// behind the release either.
    fn drop(&mut self) {
        if self.join.is_some() {
            self.shared.begin_shutdown();
        }
    }
}
