//! Instrumentation for the input path.
//!
//! §2.8 requires queue depth, time-in-queue, coalescing rate and overflow events to be
//! instrumented "regardless — the goal is that overflow is observable and rare, not merely
//! handled", and §2.6.1 requires the release-all *outcome* to be reported where the user can see
//! it. This module is that reporting surface.
//!
//! All counters are plain atomics. Ordering is `SeqCst` throughout: the input path runs at
//! roughly 100 reports per second (§5.1), so the cost is irrelevant and a single ordering is one
//! less thing to get subtly wrong.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

/// Why a release-all (§2.6) was requested.
///
/// The trigger list is §2.6's, minus serial reconnect, which is Stage 2 (§2.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReleaseReason {
    /// The viewer window lost focus. niri does not deactivate the shortcut inhibitor on focus
    /// loss, so this is client-driven (§12, Stage 1).
    FocusLost,
    /// Pointer or keyboard capture was released.
    CaptureReleased,
    /// The user's explicit "release all" binding.
    UserRequested,
    /// The bounded queue could not admit a transition (§2.8).
    Overflow,
    /// The transport failed under the writer (§2.6.1).
    LinkDown,
    /// Clean shutdown (§2.6).
    Shutdown,
}

impl ReleaseReason {
    /// How consequential this reason is, used to pick one reason when concurrent triggers coalesce
    /// into a single sequence (§2.6).
    ///
    /// `Shutdown` outranks everything because nothing follows it; `LinkDown` outranks the rest
    /// because it is the one that means the release may never have reached the device (§2.6.1);
    /// `Overflow` outranks the ordinary triggers because it is a session failure rather than a
    /// user action (§2.8).
    pub(crate) fn severity(self) -> u8 {
        match self {
            ReleaseReason::Shutdown => 3,
            ReleaseReason::LinkDown => 2,
            ReleaseReason::Overflow => 1,
            ReleaseReason::FocusLost
            | ReleaseReason::CaptureReleased
            | ReleaseReason::UserRequested => 0,
        }
    }
}

/// What happened to the release-all reports, as distinct from what the device did with them.
///
/// §2.6.1: an advanced `acked_epoch` means the writer discarded all stale work and either
/// submitted the release to the transport or recorded it as unsent. It never means the device
/// received anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReleaseOutcome {
    /// Every release frame was handed to the link. The device may still have ignored them.
    Submitted,
    /// A transport failure stopped the sequence. **The target may still be holding keys** —
    /// §2.6.1 says to surface this rather than hide it behind cleared local state.
    Unsent,
}

/// The record of the most recent completed cancellation sequence (§2.6, §2.6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleaseRecord {
    /// The epoch latched by the writer, which is also the value written to `acked_epoch`.
    pub epoch: u64,
    /// The reason of this sequence. Concurrent triggers coalesce into one sequence (§2.6); the
    /// most severe reason is kept, ties going to the first (`QueueState::note_reason`).
    pub reason: ReleaseReason,
    /// Whether the frames reached the transport (§2.6.1).
    pub outcome: ReleaseOutcome,
}

/// A cheap snapshot of the input path's instrumentation (§2.8, §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Entries currently queued: coalesced motion runs plus barriers (§2.3).
    pub queue_depth: usize,
    /// High-water mark of `queue_depth`.
    pub max_queue_depth: usize,
    /// Barrier entries currently queued. This is the quantity `max_barriers` bounds (§2.8).
    pub barrier_depth: usize,
    /// High-water mark of `barrier_depth`.
    pub max_barrier_depth: usize,
    /// Absolute positions that replaced a newer-pending one inside a motion run (§2.2).
    pub abs_replaced: u64,
    /// Relative motion events summed into an existing run rather than starting one (§2.2).
    pub rel_summed: u64,
    /// Wheel deltas summed into an existing run (§2.2).
    pub wheel_summed: u64,
    /// `SEND_KB_GENERAL_DATA` frames handed to the link.
    pub kb_reports: u64,
    /// `SEND_MS_ABS_DATA` frames handed to the link.
    pub abs_reports: u64,
    /// `SEND_MS_REL_DATA` frames handed to the link.
    pub rel_reports: u64,
    /// Axes of a coalesced motion run whose accumulated delta exceeded the per-run cap and was
    /// clamped to it — counted once per axis per flushed run. See the module documentation of the
    /// writer for why the cap exists; a non-zero value means the writer was stalled long enough
    /// for more than a screen's worth of relative motion to pile up behind it.
    pub rel_truncated: u64,
    /// Replies of the form `cmd | 0xC0` (§3.1). The link is healthy; the frame was rejected.
    pub device_errors: u64,
    /// Transacts that were written but never answered within the configured timeout.
    pub timeouts: u64,
    /// Submissions refused because coalescing could not make room (§2.8).
    pub overflows: u64,
    /// Completed cancellation sequences (§2.6).
    pub cancellations: u64,
    /// Queue entries the writer dequeued and discarded without writing: stale against
    /// `acked_epoch` at the §2.6 step-3 re-check, or arriving after the link went down.
    pub stale_discarded: u64,
    /// The most recent completed release-all (§2.6.1).
    pub last_release: Option<ReleaseRecord>,
    /// The transport has failed. No reconnect in Stage 1 (§2.7 is Stage 2).
    pub link_down: bool,
    /// At least one transact timed out. The link may still be usable but is not keeping up.
    pub degraded: bool,
    /// `acked_epoch` — advanced only by the writer (§2.6).
    pub acked_epoch: u64,
    /// `requested_epoch` — bumped by every cancellation trigger (§2.6).
    pub requested_epoch: u64,
    /// Longest time any entry spent between submission and dequeue (§2.8).
    pub max_time_in_queue: Duration,
    /// Time the most recently dequeued entry spent in the queue.
    pub last_time_in_queue: Duration,
}

/// The live counters behind [`Stats`]. Internal; read through `Producer::stats`.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub(crate) queue_depth: AtomicUsize,
    pub(crate) max_queue_depth: AtomicUsize,
    pub(crate) barrier_depth: AtomicUsize,
    pub(crate) max_barrier_depth: AtomicUsize,
    pub(crate) abs_replaced: AtomicU64,
    pub(crate) rel_summed: AtomicU64,
    pub(crate) wheel_summed: AtomicU64,
    pub(crate) kb_reports: AtomicU64,
    pub(crate) abs_reports: AtomicU64,
    pub(crate) rel_reports: AtomicU64,
    pub(crate) rel_truncated: AtomicU64,
    pub(crate) device_errors: AtomicU64,
    pub(crate) timeouts: AtomicU64,
    pub(crate) overflows: AtomicU64,
    pub(crate) cancellations: AtomicU64,
    pub(crate) stale_discarded: AtomicU64,
    pub(crate) max_time_in_queue_ns: AtomicU64,
    pub(crate) last_time_in_queue_ns: AtomicU64,
}

impl Counters {
    pub(crate) fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::SeqCst);
    }

    /// Record a depth sample and keep the high-water mark.
    pub(crate) fn set_depth(&self, depth: usize, barriers: usize) {
        self.queue_depth.store(depth, Ordering::SeqCst);
        self.max_queue_depth.fetch_max(depth, Ordering::SeqCst);
        self.barrier_depth.store(barriers, Ordering::SeqCst);
        self.max_barrier_depth.fetch_max(barriers, Ordering::SeqCst);
    }

    /// Record how long a dequeued entry waited (§2.8 "time-in-queue").
    pub(crate) fn record_time_in_queue(&self, waited: Duration) {
        let ns = u64::try_from(waited.as_nanos()).unwrap_or(u64::MAX);
        self.last_time_in_queue_ns.store(ns, Ordering::SeqCst);
        self.max_time_in_queue_ns.fetch_max(ns, Ordering::SeqCst);
    }
}
