//! Non-blocking input submission with ordered delivery and cancellation.
//!
//! One writer owns the transport and held-key state. Producers coalesce adjacent motion
//! while preserving key and button transitions. Release-all advances a cancellation epoch
//! so queued input from an old capture session cannot reach the target afterward.
//!
//! [`spawn`] uses one link. [`spawn_with_source`] supports reconnect, discards stale input,
//! and leaves the producer disengaged until the user captures again.

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
use crate::link::{Link, LinkError, LinkSource};
use crate::proto::frame::DeviceInfo;
use crate::proto::report::{HidKey, ABS_MAX};

/// One input event as the viewer produces it.
///
/// The epoch an event was produced under is attached inside [`Producer::submit`], so callers
/// cannot forge or stale one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A key transition. Barrier: never dropped, never merged, never reordered. Host
    /// auto-repeat must be discarded by the caller; a repeat that slips through is
    /// absorbed as a no-op by the held-state tracker.
    Key { key: HidKey, down: bool },
    /// A mouse button transition — one bit from [`crate::proto::button`]. Barrier.
    Button { button: u8, down: bool },
    /// Absolute pointer state, already in `0..=4095`; the viewer maps pixels through
    /// [`crate::proto::abs_coord`]. May be replaced by a newer value. Values
    /// above [`ABS_MAX`] are clamped here as well, because an overshoot past 8191 silently jumps
    /// the pointer to the top-left corner of a live console.
    PointerAbs { x: u16, y: u16 },
    /// Relative pointer motion. An accumulating delta: summed, never replaced, and
    /// split across reports at flush rather than clamped.
    PointerRel { dx: i32, dy: i32 },
    /// Wheel delta. An accumulating delta, same rules as [`Event::PointerRel`].
    Wheel { delta: i32 },
}

/// Tuning for the input path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// The queue bound, counted in barrier entries. Motion entries are bounded
    /// implicitly: at most one coalesced run can sit between two barriers, so the whole queue is
    /// bounded by `2 * max_barriers + 1`. A value of 0 is treated as 1.
    pub max_barriers: usize,
    /// How long each [`Link::transact`] waits for its reply. The measured ack round trip is
    /// 17 ms for a mouse report and 4.15 ms for a keyboard report.
    pub transact_timeout: Duration,
    /// Reconnect tuning. Ignored by [`spawn`], which has nothing to reconnect to.
    pub reconnect: ReconnectConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_barriers: 256,
            transact_timeout: Duration::from_millis(100),
            reconnect: ReconnectConfig::default(),
        }
    }
}

/// How hard, and how often, the writer tries to replace a failed transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectConfig {
    /// The wait before the first attempt after a failure, doubling on each rejection.
    ///
    /// Nothing is gained by trying sooner: a replug takes the kernel some hundreds of
    /// milliseconds to enumerate, and an attempt made before the node is back is an attempt
    /// wasted. A value below 1 ms is floored to 1 ms, because a zero backoff is a spin.
    pub initial_backoff: Duration,
    /// The ceiling the doubling stops at. It is also the worst case for how long a link that came
    /// back stays unnoticed, which is why it is seconds rather than minutes.
    pub max_backoff: Duration,
    /// How long the `GET_INFO` that proves a new link is real may take.
    ///
    /// The measured round trip is 3.98 ms, so this is three orders of magnitude of slack —
    /// it exists to bound a chip that is enumerated but not answering, not to pace anything.
    pub get_info_timeout: Duration,
}

impl Default for ReconnectConfig {
    /// Reconnect delays start at 250 ms and double to 4 s; commissioning has a 500 ms deadline.
    ///
    /// Recorded resets took about 0.4 s for serial and 1.4 s for the hub. This schedule
    /// retries near both recovery times without polling continuously during longer outages.
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(4),
            get_info_timeout: Duration::from_millis(500),
        }
    }
}

/// Why a submission was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SubmitError {
    /// Input is disengaged after a cancellation and stays that way until the user deliberately
    /// re-grabs and [`Producer::engage`] succeeds.
    #[error("input is disengaged; re-engage after the release-all is acknowledged")]
    Disengaged,
    /// Coalescing could not make room for a transition. The session has failed: a cancellation
    /// has been triggered, and input is disengaged until deliberate recapture.
    #[error("input queue overflowed; the session was cancelled and must be re-engaged")]
    Overflow,
    /// The transport is unavailable. Submissions are refused. A source-backed writer
    /// clears this condition after commissioning a replacement; a single-link writer cannot.
    #[error("link is down; input cannot be delivered")]
    LinkDown,
    /// The writer is shutting down. The final release-all is the last thing that will be
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
    /// input until it observes `acked_epoch >= requested_epoch`.
    #[error("release-all not acknowledged yet (requested {requested}, acked {acked})")]
    NotYetAcked { requested: u64, acked: u64 },
    /// A shutdown has begun. Recapture is closed for good: the shutdown's own release-all
    /// is the last thing the writer will write, and re-engaging behind it is what would let a
    /// key-down be written after it.
    #[error("the input path is shutting down; it cannot be re-engaged")]
    ShuttingDown,
}

/// The producer half: cloneable, `Send + Sync`, non-blocking.
#[derive(Debug, Clone)]
pub struct Producer {
    shared: Arc<Shared>,
}

/// Owns the writer thread. Drop stops it; [`WriterHandle::shutdown`] also returns
/// the final release outcome.
#[derive(Debug)]
pub struct WriterHandle {
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}

/// Start the input path: one queue, one writer thread, one link.
///
/// The writer is a dedicated `std::thread` because every report is a blocking acknowledged round
/// trip and nothing else in the program may wait on it.
pub fn spawn<L: Link + 'static>(link: L, config: Config) -> (Producer, WriterHandle) {
    let source = OneLink {
        link: Some(Box::new(link) as Box<dyn Link>),
    };
    start(Box::new(source), false, config)
}

/// Start input with a reopenable transport source.
///
/// Every new link is resynchronised, stale work discarded, held input released, and
/// device information refreshed. Failures retry with exponential backoff. The producer
/// remains disengaged until recapture. Startup uses the same commissioning sequence;
/// [`Producer::wait_for_link`] lets callers wait for its result.
pub fn spawn_with_source(source: Box<dyn LinkSource>, config: Config) -> (Producer, WriterHandle) {
    start(source, true, config)
}

/// The body both entry points share.
fn start(source: Box<dyn LinkSource>, reconnect: bool, config: Config) -> (Producer, WriterHandle) {
    let shared = Arc::new(Shared::new(config));
    if reconnect {
        // There is no link yet, and `submit` must say so rather than queue into nothing. The
        // writer clears this the moment its first `GET_INFO` answers.
        shared.mark_link_down();
    }
    let writer = writer::Writer::new(Arc::clone(&shared), source, reconnect, config);
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

/// The [`LinkSource`] behind [`spawn`]: it yields the one link it was given, once, and refuses
/// for ever after.
///
/// It exists so both entry points build the same writer over the same seam. The writer is told
/// separately not to reconnect, because retrying against a source that can never succeed would
/// spin a thread for the life of the process and would not be the behaviour `spawn`
/// promises — after a failure the link stays down and entries are discarded, and
/// `Stats::reconnect_attempts` stays 0.
struct OneLink {
    link: Option<Box<dyn Link>>,
}

impl LinkSource for OneLink {
    fn open(&mut self) -> Result<Box<dyn Link>, LinkError> {
        self.link
            .take()
            .ok_or_else(|| LinkError::Down("this path was started with one fixed link".into()))
    }
    fn describe(&self) -> String {
        "one fixed link, no reconnect".to_string()
    }
}

impl Producer {
    /// Submit one event. Never blocks on the writer and never waits on the device.
    ///
    /// Motion coalesces into the pending run; a transition is enqueued as a barrier, which
    /// flushes everything ahead of it and starts a fresh accumulation.
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
            // Losing a transition could leave input held, so overflow cancels the whole session.
            Counters::bump(&counters.overflows);
            self.shared.trigger(ReleaseReason::Overflow);
            return Err(SubmitError::Overflow);
        }
        self.shared.wake.notify_all();
        Ok(())
    }

    /// Request fresh device information without blocking or enqueuing input.
    ///
    /// Returns the generation observed atomically with the request. Wait for
    /// [`Stats::device_info_generation`] to advance, then check [`Stats::device_info_stale`]
    /// before using [`Stats::device_info`]. The generation advances on failure too.
    /// Concurrent requests coalesce and are serviced by the writer between reports.
    #[must_use = "the returned generation is the baseline a caller waits on"]
    pub fn refresh_device_info(&self) -> u64 {
        self.shared.request_info_refresh()
    }

    /// Request a release-all: focus loss, capture release, an explicit binding, or
    /// shutdown. Bumps `requested_epoch`, sets the 1-slot cancellation flag, disengages input and
    /// wakes the writer. It enqueues nothing and it never blocks on the writer or the device,
    /// which is what keeps a release schedulable when the queue is full.
    ///
    /// Repeated or concurrent calls coalesce into one release sequence.
    pub fn request_release_all(&self, reason: ReleaseReason) {
        self.shared.trigger(reason);
    }

    /// Re-enable submission after a cancellation — the user's deliberate re-grab.
    ///
    /// Succeeds only once `acked_epoch >= requested_epoch`, and never after a shutdown has begun.
    /// That acknowledgement is a local
    /// barrier: it means the writer discarded all stale work and either submitted the release
    /// or recorded it as unsent. It does not mean the device received anything — check
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

    /// Wait for `acked_epoch >= requested_epoch`, returning `false` on timeout.
    ///
    /// This only waits for the release barrier. Shutdown can satisfy it while
    /// [`Producer::engage`] still returns [`EngageError::ShuttingDown`].
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

    /// Block until a link has been commissioned, returning what its `GET_INFO` reported.
    ///
    /// This is how a caller keeps "fail fast when the device is not there" while the writer owns
    /// the opening: [`spawn_with_source`] reports a startup failure through [`Stats`] instead of
    /// returning an error, and this turns that back into one.
    ///
    /// [`SubmitError::LinkDown`] means the timeout elapsed with no link, not that reconnecting has
    /// stopped — the writer keeps trying. [`SubmitError::ShuttingDown`] means it has stopped for
    /// good. On a path started with [`spawn`] no `GET_INFO` is ever sent, so this always times
    /// out; it is meaningful only for [`spawn_with_source`].
    pub fn wait_for_link(&self, timeout: Duration) -> Result<DeviceInfo, SubmitError> {
        let deadline = Instant::now() + timeout;
        let mut q = lock(&self.shared.queue);
        loop {
            if self.shared.shutting_down.load(Ordering::SeqCst) {
                return Err(SubmitError::ShuttingDown);
            }
            // One read of `link_state`, for the reason `stats` gives: "up" and "this is what it
            // said" must come from the same critical section or this can answer with the previous
            // link's `DeviceInfo`.
            {
                let state = lock(&self.shared.link_state);
                if !state.down {
                    if let Some(info) = state.device_info {
                        return Ok(info);
                    }
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(SubmitError::LinkDown);
            }
            let (guard, _) = self
                .shared
                .wake
                .wait_timeout(q, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            q = guard;
        }
    }

    /// Snapshot queue, delivery, cancellation, and link state.
    ///
    /// Counters are sampled independently. Link status, outage time, and device information
    /// are sampled under one lock so their relationships remain consistent.
    pub fn stats(&self) -> Stats {
        let c = &self.shared.counters;
        let load = |a: &AtomicU64| a.load(Ordering::SeqCst);
        let load_usize = |a: &AtomicUsize| a.load(Ordering::SeqCst);
        let link_state = *lock(&self.shared.link_state);
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
            // From `link_state`, not from the `link_down` atomic beside it: the atomic is the fast
            // path `submit` reads, and reading it here would let this snapshot straddle a
            // transition the writer makes in one critical section.
            link_down: link_state.down,
            reconnects: load(&c.reconnects),
            reconnect_attempts: load(&c.reconnect_attempts),
            down_since: link_state.down_since,
            device_info: link_state.device_info,
            device_info_generation: link_state.info_generation,
            device_info_stale: link_state.info_stale,
            queue_discarded_on_reconnect: load(&c.queue_discarded_on_reconnect),
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
    /// Close input admission, finish the final release, and join the writer.
    ///
    /// Returns the release outcome for this shutdown epoch or a later coalesced epoch.
    /// [`ReleaseOutcome::Unsent`] means the target may still hold keys. Joining also detects
    /// a writer panic, which otherwise could leave a wait for acknowledgement blocked.
    pub fn shutdown(mut self) -> ReleaseOutcome {
        let epoch = self.shared.begin_shutdown();
        let joined = match self.join.take() {
            Some(join) => join.join().is_ok(),
            None => false,
        };
        let acked = self.shared.acked_epoch.load(Ordering::SeqCst);
        match (joined, *lock(&self.shared.last_release)) {
            // A later coalesced epoch still represents a release requested after shutdown began.
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
    /// path as `shutdown` does, so a producer that survives the handle cannot re-engage
    /// behind the release either.
    fn drop(&mut self) {
        if self.join.is_some() {
            self.shared.begin_shutdown();
        }
    }
}
