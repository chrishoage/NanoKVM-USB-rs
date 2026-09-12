//! Input counters and observable release outcomes.
//!
//! Cancellation acknowledgement advances local state even when the link is down. The
//! separate release outcome tells the viewer whether the target may still hold keys.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::proto::frame::DeviceInfo;

/// Reason a release-all was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReleaseReason {
    /// The viewer window lost focus. niri does not deactivate the shortcut inhibitor on focus
    /// loss, so this is client-driven.
    FocusLost,
    /// Pointer or keyboard capture was released.
    CaptureReleased,
    /// The user's explicit "release all" binding.
    UserRequested,
    /// The bounded queue could not admit a transition.
    Overflow,
    /// The transport failed under the writer.
    LinkDown,
    /// A replacement link was commissioned and its unknown HID state released.
    Reconnected,
    /// Clean shutdown.
    Shutdown,
}

impl ReleaseReason {
    /// Severity used when cancellation triggers coalesce.
    ///
    /// Shutdown outranks link loss and reconnect, then overflow, then ordinary releases.
    /// Reconnect takes pending reasons separately but uses this ordering to preserve shutdown.
    pub(crate) fn severity(self) -> u8 {
        match self {
            ReleaseReason::Shutdown => 3,
            ReleaseReason::LinkDown | ReleaseReason::Reconnected => 2,
            ReleaseReason::Overflow => 1,
            ReleaseReason::FocusLost
            | ReleaseReason::CaptureReleased
            | ReleaseReason::UserRequested => 0,
        }
    }
}

/// Transport outcome of release-all. Advancing the local epoch does not establish
/// that the target received the release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReleaseOutcome {
    /// Every release frame was handed to the link. The device may still have ignored them.
    Submitted,
    /// Transport failure interrupted release; the target may still hold keys.
    Unsent,
}

/// The record of the most recent completed cancellation sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleaseRecord {
    /// The epoch latched by the writer, which is also the value written to `acked_epoch`.
    pub epoch: u64,
    /// The reason of this sequence. Concurrent triggers coalesce into one sequence; the
    /// most severe reason is kept, ties going to the first (`QueueState::note_reason`).
    pub reason: ReleaseReason,
    /// Whether the frames reached the transport.
    pub outcome: ReleaseOutcome,
}

/// A cheap snapshot of the input path's instrumentation.
///
/// Not `Eq`: [`Stats::device_info`] carries the device's firmware version as an `f32`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stats {
    /// Entries currently queued: coalesced motion runs plus barriers.
    pub queue_depth: usize,
    /// High-water mark of `queue_depth`.
    pub max_queue_depth: usize,
    /// Barrier entries currently queued. This is the quantity `max_barriers` bounds.
    pub barrier_depth: usize,
    /// High-water mark of `barrier_depth`.
    pub max_barrier_depth: usize,
    /// Absolute positions that replaced a newer-pending one inside a motion run.
    pub abs_replaced: u64,
    /// Relative motion events summed into an existing run rather than starting one.
    pub rel_summed: u64,
    /// Wheel deltas summed into an existing run.
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
    /// Replies of the form `cmd | 0xC0`. The link is healthy; the frame was rejected.
    pub device_errors: u64,
    /// Transacts that were written but never answered within the configured timeout.
    pub timeouts: u64,
    /// Submissions refused because coalescing could not make room.
    pub overflows: u64,
    /// Completed cancellation sequences.
    pub cancellations: u64,
    /// Entries discarded as stale or received after link loss.
    pub stale_discarded: u64,
    /// The most recent completed release-all.
    pub last_release: Option<ReleaseRecord>,
    /// The transport has failed and no replacement has been commissioned yet.
    /// True from startup until the first link is accepted on a path started with
    /// [`crate::input::spawn_with_source`].
    ///
    /// This and [`Stats::down_since`]/[`Stats::device_info`] are read together under one lock, so
    /// `link_down == true` always comes with a `down_since`, and a link that is up never carries
    /// one. Rendering "down for {:?}" from the pair needs no defensive `unwrap_or_default`.
    pub link_down: bool,
    /// Links accepted after a failure. The initial one is not counted: it did not
    /// replace anything, so `reconnects == 0` on a healthy run that never lost the cable.
    pub reconnects: u64,
    /// Commissioning attempts, including initial startup and failed device-info checks.
    /// A successful startup has one attempt and zero reconnects.
    pub reconnect_attempts: u64,
    /// When the current outage began, or `None` while a link is in service. Render it as an
    /// elapsed time: `down_since.map(|t| t.elapsed())`.
    pub down_since: Option<Instant>,
    /// The last successful `GET_INFO`, including the initial one. It is the proof
    /// that the reply parser is realigned and a CH9329 is answering, so it is `Some` when
    /// a link has been accepted at least once — the version and lock bits are a by-product.
    pub device_info: Option<DeviceInfo>,
    /// Generation of the latest device-info attempt. Compare against the generation
    /// returned by a refresh request, then check [`Stats::device_info_stale`].
    pub device_info_generation: u64,
    /// The latest information request failed. Treat cached lock bits as unknown.
    pub device_info_stale: bool,
    /// Queued entries discarded at reconnect to prevent stale input from being replayed.
    pub queue_discarded_on_reconnect: u64,
    /// At least one transact timed out. The link may still be usable but is not keeping up.
    pub degraded: bool,
    /// `acked_epoch` — advanced only by the writer.
    pub acked_epoch: u64,
    /// `requested_epoch` — bumped by every cancellation trigger.
    pub requested_epoch: u64,
    /// Longest time any entry spent between submission and dequeue.
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
    pub(crate) reconnects: AtomicU64,
    pub(crate) reconnect_attempts: AtomicU64,
    pub(crate) queue_discarded_on_reconnect: AtomicU64,
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

    /// Record how long a dequeued entry waited.
    pub(crate) fn record_time_in_queue(&self, waited: Duration) {
        let ns = u64::try_from(waited.as_nanos()).unwrap_or(u64::MAX);
        self.last_time_in_queue_ns.store(ns, Ordering::SeqCst);
        self.max_time_in_queue_ns.fetch_max(ns, Ordering::SeqCst);
    }
}
