//! Shared admission, cancellation, and link state.
//!
//! The cancellation epoch and admission state change under one lock. A release drains
//! events through its latched epoch; fresh-session events carry the acknowledged epoch.
//! A newer cancellation keeps the flag armed until its own release completes.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Instant;

use crate::input::queue::QueueState;
use crate::input::stats::{Counters, ReleaseRecord};
use crate::input::{Config, ReleaseReason};
use crate::proto::frame::DeviceInfo;

/// Recover poisoned locks so a worker panic cannot prevent input cancellation.
/// Critical sections contain only counter and queue updates.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What is known about the transport: whether it is up, since when, and what it last said.
///
/// All three live under one mutex, and the mutex is what [`crate::input::Stats`] reads, so a
/// snapshot cannot straddle a link transition. `down` duplicates `Shared::link_down` on purpose:
/// `link_down` is read by every `submit` and must stay a lock-free atomic, but a reader that took
/// the atomic and this mutex separately could see `down == true` with no `down_since`, or a live
/// link still carrying one — the two are written in one order and were read in the other. The
/// writers below hold this mutex across the atomic store, so the pair is only ever observed
/// before or after a transition, never inside one.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LinkState {
    /// The transport has failed and no replacement has been commissioned. The mirror of
    /// `Shared::link_down` that a consistent snapshot is taken from.
    pub(crate) down: bool,
    /// When the current outage began.
    /// `Some` while `down` is true.
    pub(crate) down_since: Option<Instant>,
    /// The last `GET_INFO` that answered.
    pub(crate) device_info: Option<DeviceInfo>,
    /// Generation of the last commissioning or refresh attempt. Callers compare it with
    /// the generation returned by their refresh request to establish freshness.
    pub(crate) info_generation: u64,
    /// The most recent information request failed or had no link. Cached lock bits must
    /// not be treated as current until a later successful reading clears this flag.
    pub(crate) info_stale: bool,
}

/// Everything the producers and the writer both touch.
#[derive(Debug)]
pub(crate) struct Shared {
    pub(crate) queue: Mutex<QueueState>,
    /// Signalled on submission, on any cancellation trigger and on shutdown.
    pub(crate) wake: Condvar,
    /// Epoch requested by cancellation triggers.
    pub(crate) requested_epoch: AtomicU64,
    /// Epoch acknowledged by the writer.
    pub(crate) acked_epoch: AtomicU64,
    /// Coalesced cancellation flag, independent of queue capacity so a full queue can
    /// still be released.
    pub(crate) cancel: AtomicBool,
    /// Whether `submit` is accepted.
    pub(crate) engaged: AtomicBool,
    /// No usable transport. Cleared by the writer after successful commissioning.
    pub(crate) link_down: AtomicBool,
    /// [`LinkState`]: when this outage began, and the last `GET_INFO` that answered.
    pub(crate) link_state: Mutex<LinkState>,
    /// At least one transact timed out.
    pub(crate) degraded: AtomicBool,
    /// Coalesced information-refresh request. Only the writer services it, preventing
    /// concurrent transactions from interleaving bytes on the serial link.
    pub(crate) refresh_info: AtomicBool,
    /// Set by `WriterHandle::shutdown` and by dropping the handle, always in the same critical
    /// section as the trigger that accompanies it. It closes `engage` and `submit` for good, and
    /// the writer exits once it is set and no cancellation is pending. Never cleared: after a
    /// shutdown nobody drains the queue, so accepting input again would wedge it invisibly.
    pub(crate) shutting_down: AtomicBool,
    pub(crate) counters: Counters,
    pub(crate) last_release: Mutex<Option<ReleaseRecord>>,
    pub(crate) config: Config,
}

impl Shared {
    pub(crate) fn new(config: Config) -> Self {
        Self {
            queue: Mutex::new(QueueState::new(config.max_barriers)),
            wake: Condvar::new(),
            // Epochs start equal, so a fresh session is engaged and its events (epoch 0) are not
            // stale against `acked_epoch == 0` under the strict test above.
            requested_epoch: AtomicU64::new(0),
            acked_epoch: AtomicU64::new(0),
            cancel: AtomicBool::new(false),
            engaged: AtomicBool::new(true),
            link_down: AtomicBool::new(false),
            link_state: Mutex::new(LinkState::default()),
            refresh_info: AtomicBool::new(false),
            degraded: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            counters: Counters::default(),
            last_release: Mutex::new(None),
            config,
        }
    }

    /// A cancellation trigger: bump `requested_epoch`, set the flag, disengage, wake the
    /// writer. It enqueues nothing and it does not wait for the writer.
    pub(crate) fn trigger(&self, reason: ReleaseReason) {
        let _ = self.raise(ReasonSlot::Note(reason), false);
    }

    /// Begin commissioning by advancing the epoch and disengaging input.
    ///
    /// Returns the new epoch and takes any pending reason it covers. The reason slot is
    /// left free for a trigger arriving during commissioning, which needs its own release.
    pub(crate) fn begin_reconnect(&self) -> (u64, Option<ReleaseReason>) {
        self.raise(ReasonSlot::Take, false)
    }

    /// Mark the transport failed, remembering when, if this is the transition into an outage.
    /// Called by the writer on a transport failure and once at startup by
    /// [`crate::input::spawn_with_source`], which has no link yet.
    ///
    /// The atomic is stored while the mutex is held, so the two halves of the transition are
    /// one critical section to anyone reading a snapshot — see [`LinkState`].
    pub(crate) fn mark_link_down(&self) {
        let mut state = lock(&self.link_state);
        if state.down_since.is_none() {
            state.down_since = Some(Instant::now());
        }
        state.down = true;
        self.link_down.store(true, Ordering::SeqCst);
    }

    /// Record an accepted link and return how long the outage lasted. One critical
    /// section, for the reason on [`LinkState`].
    pub(crate) fn mark_link_up(&self, info: DeviceInfo) -> Option<std::time::Duration> {
        let mut state = lock(&self.link_state);
        let down_for = state.down_since.take().map(|t| t.elapsed());
        state.device_info = Some(info);
        state.info_generation = state.info_generation.wrapping_add(1);
        state.info_stale = false;
        state.down = false;
        self.link_down.store(false, Ordering::SeqCst);
        down_for
    }

    /// Publish a `GET_INFO` reading taken outside a commissioning. One critical
    /// section, like [`Shared::mark_link_up`], so a snapshot never shows a new generation with the
    /// old reading.
    pub(crate) fn publish_device_info(&self, info: DeviceInfo) {
        let mut state = lock(&self.link_state);
        state.device_info = Some(info);
        state.info_generation = state.info_generation.wrapping_add(1);
        state.info_stale = false;
    }

    /// Ask the writer for a fresh `GET_INFO`, and return the generation the request is *against*.
    ///
    /// The read and the store are in one critical section, for the same reason as every other
    /// pair here: a caller that set the flag and then read the generation separately could have
    /// the writer answer in between, and would then be waiting for a number that had already
    /// moved. For the viewer's paste that is a 3 s wait ending in "the device did not answer
    /// GET_INFO" about a device that answered in milliseconds. Both writers of the generation
    /// ([`Shared::publish_device_info`], [`Shared::note_info_refresh_failed`]) take this mutex, so
    /// the number returned is one that any answer to *this* request is greater than.
    pub(crate) fn request_info_refresh(&self) -> u64 {
        let generation = {
            let state = lock(&self.link_state);
            self.refresh_info.store(true, Ordering::SeqCst);
            state.info_generation
        };
        self.wake.notify_all();
        generation
    }

    /// Advance the refresh generation and mark cached information stale after a failed attempt.
    pub(crate) fn note_info_refresh_failed(&self) {
        let mut state = lock(&self.link_state);
        state.info_generation = state.info_generation.wrapping_add(1);
        state.info_stale = true;
    }

    /// The shutdown trigger. Identical to [`Shared::trigger`], except that `shutting_down` is set
    /// inside the same critical section, so the epoch bump and the closing of recapture are
    /// indivisible to anyone holding the queue lock — which is every producer path. Returns the
    /// epoch `E` this call requested, so the caller can wait for its own release.
    pub(crate) fn begin_shutdown(&self) -> u64 {
        self.raise(ReasonSlot::Note(ReleaseReason::Shutdown), true)
            .0
    }

    /// The body every trigger shares. Returns the epoch this trigger requested and, for
    /// [`ReasonSlot::Take`], the reason it took out of the slot.
    fn raise(&self, slot: ReasonSlot, shutting_down: bool) -> (u64, Option<ReleaseReason>) {
        let raised = {
            let mut q = lock(&self.queue);
            if shutting_down {
                self.shutting_down.store(true, Ordering::SeqCst);
            }
            let epoch = self.requested_epoch.fetch_add(1, Ordering::SeqCst) + 1;
            let taken = match slot {
                ReasonSlot::Note(reason) => {
                    q.note_reason(reason);
                    None
                }
                ReasonSlot::Take => q.pending_reason.take(),
            };
            self.cancel.store(true, Ordering::SeqCst);
            // Input stays disengaged until the user re-grabs.
            self.engaged.store(false, Ordering::SeqCst);
            (epoch, taken)
        };
        self.wake.notify_all();
        raised
    }
}

/// Whether a trigger adds a pending reason or commissioning takes the existing one.
enum ReasonSlot {
    /// Coalesce this reason into the slot, most severe wins (`QueueState::note_reason`).
    Note(ReleaseReason),
    /// Empty the slot and hand its contents to the caller, which is about to run the sequence
    /// that covers it.
    Take,
}
