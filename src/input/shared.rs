//! State shared between every `Producer` and the one writer thread (§2.6).
//!
//! # The epoch scheme, stated precisely
//!
//! - An event submitted while `requested_epoch == R` carries epoch `R`.
//! - A cancellation trigger bumps `requested_epoch` to `E = R + 1`, sets the 1-slot flag, and
//!   disengages the producer. Every event already queued therefore carries `<= E - 1`.
//! - The writer latches `E`, drains every entry with epoch `<= E` (§2.6 step 2), writes the
//!   release, and sets `acked_epoch = E`.
//! - `engage` succeeds only once `acked_epoch >= requested_epoch`, after which new events carry
//!   epoch `E` again.
//!
//! Hence the writer's step-3 staleness test is **strict**: `entry.epoch < acked_epoch`. Using
//! `<=` there would discard every event of the newly engaged session, since those legitimately
//! carry exactly `acked_epoch`. The drain in step 2 keeps the plan's `<= E` because at that
//! instant no entry can carry `E` yet.
//!
//! # Shutdown is a one-way door
//!
//! A shutdown is a cancellation trigger like any other, with one extra property: **no session may
//! follow it**. `shutting_down` is set in the *same* critical section that bumps `requested_epoch`
//! and sets the flag, so no producer can observe the epoch advance without also observing that
//! recapture is closed. Without that, a producer following §2.8's recapture loop (`submit` fails,
//! `engage()`, `submit`) re-engages the instant the writer acks the shutdown epoch and its events
//! are written *after* the final release-all — the writer then exits and the target is left holding
//! a key. `engage` and `submit` therefore fail permanently once it is set, and the writer exits
//! only when it is set **and** no cancellation is pending, which is the one release it still owes.
//!
//! # Why the mutex is taken by the triggers
//!
//! The cancellation flag is an `AtomicBool` so any thread can read it without a lock, but every
//! *mutation* of `requested_epoch`, the flag and `engaged` happens under the queue mutex. That
//! closes the lost-wakeup window between the writer's "flag is clear" check and its
//! `Condvar::wait`, and it makes `engage` and the triggers mutually exclusive. The lock is held
//! for a handful of instructions and **never across a `Link::transact`**, so `request_release_all`
//! still never blocks on the writer or on the device (§2.6: triggers never block).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Instant;

use crate::input::queue::QueueState;
use crate::input::stats::{Counters, ReleaseRecord};
use crate::input::{Config, ReleaseReason};
use crate::proto::frame::DeviceInfo;

/// Lock a mutex, recovering from poisoning.
///
/// Poisoning means some thread panicked while holding this lock. Every critical section here is
/// a handful of infallible operations on a `VecDeque` and some counters, so the guarded state
/// cannot be left half-updated; and a poisoned lock that propagated would wedge input on a
/// machine that is driving someone else's console, which is exactly the failure §2.6.1 forbids.
/// So the poison is discarded rather than unwrapped.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What is known about the transport: whether it is up, since when, and what it last said.
///
/// **All three live under one mutex, and the mutex is what [`crate::input::Stats`] reads**, so a
/// snapshot cannot straddle a link transition. `down` duplicates `Shared::link_down` on purpose:
/// `link_down` is read by every `submit` and must stay a lock-free atomic, but a reader that took
/// the atomic and this mutex separately could see `down == true` with no `down_since`, or a live
/// link still carrying one — the two are written in one order and were read in the other. The
/// writers below hold this mutex **across** the atomic store, so the pair is only ever observed
/// before or after a transition, never inside one.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LinkState {
    /// The transport has failed and no replacement has been commissioned (§2.6.1). The mirror of
    /// `Shared::link_down` that a consistent snapshot is taken from.
    pub(crate) down: bool,
    /// When the current outage began (§2.7: the log line says how long the link was down).
    /// `Some` exactly while `down` is true.
    pub(crate) down_since: Option<Instant>,
    /// The last `GET_INFO` that answered (§2.7 step 3d).
    pub(crate) device_info: Option<DeviceInfo>,
}

/// Everything the producers and the writer both touch.
#[derive(Debug)]
pub(crate) struct Shared {
    pub(crate) queue: Mutex<QueueState>,
    /// Signalled on submission, on any cancellation trigger and on shutdown.
    pub(crate) wake: Condvar,
    /// §2.6: incremented by any cancellation trigger.
    pub(crate) requested_epoch: AtomicU64,
    /// §2.6: advanced only by the writer.
    pub(crate) acked_epoch: AtomicU64,
    /// §2.6: the 1-slot cancellation flag. Setting an already-set flag is a no-op, which is how
    /// concurrent triggers coalesce into one sequence — and how a release stays schedulable when
    /// the event queue is full, since nothing is enqueued.
    pub(crate) cancel: AtomicBool,
    /// Whether `submit` is accepted (§2.6 last paragraph, §2.8 "require deliberate recapture").
    pub(crate) engaged: AtomicBool,
    /// The transport has failed under the writer, and no replacement has been commissioned yet
    /// (§2.6.1). Cleared only by a completed §2.7 sequence, and only by the writer.
    pub(crate) link_down: AtomicBool,
    /// [`LinkState`]: when this outage began, and the last `GET_INFO` that answered.
    pub(crate) link_state: Mutex<LinkState>,
    /// At least one transact timed out.
    pub(crate) degraded: AtomicBool,
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
            degraded: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            counters: Counters::default(),
            last_release: Mutex::new(None),
            config,
        }
    }

    /// A cancellation trigger (§2.6): bump `requested_epoch`, set the flag, disengage, wake the
    /// writer. It enqueues nothing and it does not wait for the writer.
    pub(crate) fn trigger(&self, reason: ReleaseReason) {
        let _ = self.raise(ReasonSlot::Note(reason), false);
    }

    /// The writer's own trigger for the §2.7 sequence, raised on a link that has just been opened.
    /// Returns the epoch it requested and the reason it took over, if any.
    ///
    /// It is a cancellation like any other — the epoch advances, the flag is set and the producer
    /// is disengaged, so a producer that re-engaged while the link was down (which `engage` allows,
    /// since it only checks the ack) is disengaged again and its events are drained rather than
    /// written on the new link.
    ///
    /// It differs from [`Shared::trigger`] in what it does with the one-slot reason: it **takes**
    /// whatever is there rather than adding to it. Both halves of that matter, and both are in the
    /// same critical section as the epoch bump:
    ///
    /// - *Taking* it: a trigger that landed while the writer was inside `LinkSource::open` — after
    ///   the last flag check and before this call — has its reason sitting in the slot with no
    ///   sequence of its own left to run, because this sequence covers its epoch and then clears
    ///   the flag. Leaving it there would strand it, and the next unrelated release would inherit
    ///   it and be mislabelled.
    /// - *Not adding* to it: the slot is left free for a trigger that arrives **during** the
    ///   sequence — a shutdown between steps 3a and 3e — so that trigger's own sequence is
    ///   labelled with its own reason. The writer supplies `Reconnected` itself.
    pub(crate) fn begin_reconnect(&self) -> (u64, Option<ReleaseReason>) {
        self.raise(ReasonSlot::Take, false)
    }

    /// Mark the transport failed, remembering when, if this is the transition into an outage.
    /// Called by the writer on a transport failure and once at startup by
    /// [`crate::input::spawn_with_source`], which has no link yet.
    ///
    /// The atomic is stored **while the mutex is held**, so the two halves of the transition are
    /// one critical section to anyone reading a snapshot — see [`LinkState`].
    pub(crate) fn mark_link_down(&self) {
        let mut state = lock(&self.link_state);
        if state.down_since.is_none() {
            state.down_since = Some(Instant::now());
        }
        state.down = true;
        self.link_down.store(true, Ordering::SeqCst);
    }

    /// Record an accepted link (§2.7 step 3e) and return how long the outage lasted. One critical
    /// section, for the reason on [`LinkState`].
    pub(crate) fn mark_link_up(&self, info: DeviceInfo) -> Option<std::time::Duration> {
        let mut state = lock(&self.link_state);
        let down_for = state.down_since.take().map(|t| t.elapsed());
        state.device_info = Some(info);
        state.down = false;
        self.link_down.store(false, Ordering::SeqCst);
        down_for
    }

    /// The shutdown trigger. Identical to [`Shared::trigger`], except that `shutting_down` is set
    /// **inside the same critical section**, so the epoch bump and the closing of recapture are
    /// indivisible to anyone holding the queue lock — which is every producer path. Returns the
    /// epoch `E` this call requested, so the caller can wait for exactly its own release.
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
            // Input stays disengaged until the user deliberately re-grabs (§2.8).
            self.engaged.store(false, Ordering::SeqCst);
            (epoch, taken)
        };
        self.wake.notify_all();
        raised
    }
}

/// What a trigger does with the queue's one-slot pending reason (§2.6).
///
/// The two are exclusive by construction, which is the point of naming them: an ordinary trigger
/// adds a reason for a sequence that has not run yet, while the writer's reconnect *is* the
/// sequence and so takes what is there. See [`Shared::begin_reconnect`].
enum ReasonSlot {
    /// Coalesce this reason into the slot, most severe wins (`QueueState::note_reason`).
    Note(ReleaseReason),
    /// Empty the slot and hand its contents to the caller, which is about to run the sequence
    /// that covers it.
    Take,
}
