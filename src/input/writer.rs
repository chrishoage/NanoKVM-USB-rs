//! The single writer thread: the sole serialization point (§2.6).
//!
//! One queue, one order, one consumer (§2.1). Every report leaves as exactly one
//! [`Link::transact`] and the writer waits for the reply before starting the next: the
//! acknowledged round trip is the real rate limit (17 ms for a mouse report, 4.15 ms for a
//! keyboard report) and **overload provides no backpressure — it silently corrupts** (§5.1).
//!
//! The epoch check is applied here and nowhere else. It is not a filter that producers or a
//! monitor apply concurrently; it is applied by the one thread that writes, in the same loop
//! iteration as the write, and the cancellation sequence runs to completion in that thread. There
//! is no window because there is no concurrency at the boundary (§2.6).
//!
//! Cancellation happens **between** transacts, never inside one. The writer's atomic unit is one
//! complete frame, because the CH9329 has no inter-byte timeout and a torn frame consumes the
//! following command as its remainder (§5.1, §2.6). **The frame is the unit of the check, not the
//! queue entry.** One entry can be many frames — a coalesced motion run splits across as many
//! reports as its accumulation needs (§2.3) — so the flag is re-read after *every* transact. The
//! rest of an entry abandoned this way carries an epoch `<= E` and is stale by definition: the
//! cancellation sequence would have drained it had it still been queued.
//!
//! # Why a coalesced run is capped at [`REL_RUN_MAX`] per axis
//!
//! §2.3's "saturate and split, never clamp" governs the **report** range of ±127, and that rule
//! is kept: nothing is lost inside a run that fits the cap. The cap is a separate, higher bound on
//! the *run*, and it exists because an uncapped run is a denial of service against the release-all:
//!
//! - A run longer than 32 reports (~0.35 s at the measured 91 relative reports per second, §5.1)
//!   cannot arise from a user moving a mouse. It arises when the writer is stalled and motion
//!   piles up behind it — which is precisely the §2.8 situation in which a release-all is most
//!   likely to be wanted, and least affordable to delay.
//! - Relative motion cannot address a coordinate in any case (§3.4): the deltas are consumed by
//!   whatever pointer acceleration the target applies, so beyond a screen's width the accumulated
//!   figure has no meaning to preserve. [`REL_RUN_MAX`] is the full span of the absolute
//!   coordinate range, which is the largest displacement that could mean anything.
//! - Between the two checks the abandoned motion is bounded twice over: the per-frame check ends
//!   a run at the first frame boundary after a trigger, and the cap bounds the run even with no
//!   trigger at all.
//!
//! Truncation is counted (`Stats::rel_truncated`) rather than silent, per §2.8's "observable and
//! rare, not merely handled".

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::input::held::HeldState;
use crate::input::queue::{Entry, EntryKind, MotionRun};
use crate::input::shared::{lock, Shared};
use crate::input::stats::{Counters, ReleaseOutcome, ReleaseRecord};
use crate::input::{ReconnectConfig, ReleaseReason};
use crate::link::{Link, LinkError, LinkSource};
use crate::proto::cmd;
use crate::proto::frame::DeviceInfo;
use crate::proto::report::{KeyboardReport, MouseAbsReport, MouseRelReport, MOUSE_RELEASE_ALL};

/// The transport failed on this frame. Nothing further may be written (§2.6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Down;

/// The cap on one coalesced motion run's accumulated delta, per axis (§2.3, §3.4).
///
/// The same 4095 as [`crate::proto::report::ABS_MAX`], and for the same reason: it is the full
/// span of the coordinate range, so it is the largest displacement that could still mean anything.
/// At ±127 a report this is at most 33 reports per axis. See the module documentation.
const REL_RUN_MAX: i32 = crate::proto::report::ABS_MAX as i32;

/// The failsafe interval at which the writer re-checks `shutting_down` while an open is in flight
/// on the helper thread ([`Writer::open_bounded`]).
///
/// It is a failsafe and not the mechanism: a shutdown notifies the same condvar, so the ordinary
/// case wakes at once. It exists because "a shutdown is bounded by this constant" is a property
/// that should not depend on every future trigger remembering to notify.
const OPEN_POLL: Duration = Duration::from_millis(5);

/// How long the writer parks in [`Writer::next_entry`] before asking the transport whether it is
/// still there ([`Link::is_down`], §2.7).
///
/// It bounds the one wait that used to be unbounded, and with it the time an *idle* writer takes
/// to notice a link that hung up with nothing queued — measured on the dongle as never, because
/// only a failed write set `link_down`. 100 ms is a tenth of the second §2.8 wants that fact
/// visible within, and costs an idle thread ten mutex acquisitions a second; a submission or a
/// trigger still wakes it immediately, so this is a floor on *discovery*, never on latency.
///
/// It is emphatically not a keepalive: nothing is written. The device acknowledges anything
/// (§3.4, A17), so traffic would prove nothing that the transport's own read side does not
/// already know.
const LINK_HEALTH_POLL: Duration = Duration::from_millis(100);

/// Which mouse device the writer last addressed, tracked from the last motion flushed.
///
/// The plan fixes the report shapes but not this bookkeeping: a button transition and a
/// wheel-only run carry no position of their own, so they are emitted against whichever device
/// the session is using. Initial mode is `Rel`; any flushed `PointerAbs` makes it `Abs`, any
/// flushed `PointerRel` makes it `Rel`, and the cancellation sequence puts it back to `Rel`
/// because the position it held belonged to the session that just ended (§2.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PointerMode {
    Rel,
    Abs { x: u16, y: u16 },
}

pub(crate) struct Writer {
    shared: Arc<Shared>,
    /// Where a replacement transport comes from (§2.7). Consulted once at startup and again after
    /// every transport failure — or, when `reconnect` is false, only once ever.
    ///
    /// `None` only while an open is in flight on the helper thread, and permanently after a
    /// shutdown abandoned one there — see [`Writer::open_bounded`].
    source: Option<Box<dyn LinkSource>>,
    /// [`LinkSource::describe`], taken once at construction.
    ///
    /// It is a constant — the path, or the discovery rule — and the writer must still be able to
    /// name its source in a log line while the source itself is away on the helper thread that is
    /// opening a port.
    source_desc: String,
    /// The transport in service, or `None` between a failure and the link that replaces it.
    /// Boxed rather than generic so the transport can be swapped under the running writer.
    link: Option<Box<dyn Link>>,
    /// Whether a transport failure is repaired (§2.7) or terminal.
    ///
    /// False for [`crate::input::spawn`], which is Stage 1's entry point: its source holds exactly
    /// one link and can never produce another, so entering a retry loop against it would spin a
    /// thread for ever and would not be the Stage 1 behaviour its callers and tests pin.
    reconnect: bool,
    timeout: Duration,
    reconnect_cfg: ReconnectConfig,
    held: HeldState,
    mode: PointerMode,
}

/// Why one attempt at commissioning a link was rejected (§2.7 step 3). Every variant means the
/// same thing to the caller — drop this link and open another — and differs only in the log line.
#[derive(Debug)]
enum Rejected {
    Resync(LinkError),
    ReleaseAll,
    GetInfo(LinkError),
    Parse(crate::proto::frame::ParseError),
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Rejected::Resync(e) => write!(f, "the resynchronisation preamble failed: {e}"),
            Rejected::ReleaseAll => write!(f, "the release-all could not be written"),
            Rejected::GetInfo(e) => write!(f, "GET_INFO did not answer: {e}"),
            Rejected::Parse(e) => write!(f, "the GET_INFO reply was unusable: {e}"),
        }
    }
}

impl Writer {
    pub(crate) fn new(
        shared: Arc<Shared>,
        source: Box<dyn LinkSource>,
        reconnect: bool,
        config: crate::input::Config,
    ) -> Self {
        Self {
            shared,
            source_desc: source.describe(),
            source: Some(source),
            link: None,
            reconnect,
            timeout: config.transact_timeout,
            reconnect_cfg: config.reconnect,
            held: HeldState::new(),
            mode: PointerMode::Rel,
        }
    }

    /// The writer's loop, in the order §2.6 specifies for every iteration.
    pub(crate) fn run(mut self) {
        self.acquire_first_link();
        loop {
            // (1) Check the cancellation flag before dequeuing or writing anything.
            if self.shared.cancel.load(Ordering::SeqCst) {
                // (2) Run the sequence to completion, without interleaving any other write.
                self.cancellation_sequence();
                continue;
            }
            if self.should_exit() {
                return;
            }
            // §2.7. Placed *after* the cancellation check so the `LinkDown` release that a failure
            // triggers is completed first — §2.6.1's "a failed transport must still advance the
            // ack" — and only then is a replacement sought.
            if self.reconnect && self.shared.link_down.load(Ordering::SeqCst) {
                self.acquire(false);
                continue;
            }
            // A fresh `GET_INFO` was asked for (§12 Stage 4c). Serviced **here**, between frames
            // and on this thread, for the same reason every write is: the writer is the sole
            // serialization point (§2.6), and a second thread transacting on the link would
            // interleave two frames on a chip with no inter-byte timeout (§5.1). `continue` rather
            // than falling through, so a cancellation that arrived while the reply was in flight
            // is seen at the top of the loop before anything is dequeued.
            if self.shared.refresh_info.load(Ordering::SeqCst) {
                self.refresh_device_info();
                continue;
            }
            // (3) Dequeue one entry.
            let Some(entry) = self.next_entry() else {
                continue;
            };
            self.shared
                .counters
                .record_time_in_queue(entry.enqueued.elapsed());
            // Re-check its epoch against `acked_epoch`, discard if stale. Strict `<`: the current
            // session's own events carry exactly `acked_epoch` (see `shared`).
            if entry.epoch < self.shared.acked_epoch.load(Ordering::SeqCst) {
                Counters::bump(&self.shared.counters.stale_discarded);
                continue;
            }
            if self.shared.link_down.load(Ordering::SeqCst) {
                // Stage 1 has no reconnect (§2.7). Nothing more is written on a dead link;
                // entries are discarded so the queue cannot grow without bound and shutdown can
                // still join.
                Counters::bump(&self.shared.counters.stale_discarded);
                continue;
            }
            if self.write_entry(entry).is_err() {
                // §2.6.1: a failed transport must still advance the ack, with outcome `unsent`,
                // or a dead cable wedges input permanently.
                self.shared.trigger(ReleaseReason::LinkDown);
            }
        }
    }

    /// Whether the writer may exit.
    ///
    /// Only when nothing is pending: `shutting_down` alone is **not** enough, because the shutdown
    /// that set it also owes a release-all. The two are set in one critical section
    /// (`Shared::begin_shutdown`), so reading them both under the queue lock makes it impossible to
    /// observe the request to stop without also observing the release it owes — the window that
    /// let a shutdown exit with a key still held.
    fn should_exit(&self) -> bool {
        let _q = lock(&self.shared.queue);
        self.shared.shutting_down.load(Ordering::SeqCst)
            && !self.shared.cancel.load(Ordering::SeqCst)
    }

    /// Whether the rest of the entry in progress must be abandoned: a cancellation is pending, or
    /// the writer is shutting down. Read between frames, never inside one (§2.6).
    fn cancelled(&self) -> bool {
        self.shared.cancel.load(Ordering::SeqCst)
            || self.shared.shutting_down.load(Ordering::SeqCst)
    }

    /// Block until there is an entry, a cancellation, a shutdown, a `GET_INFO` refresh request —
    /// or the transport reports itself gone. Returns `None` when the caller should re-run the
    /// checks at the top of the loop.
    ///
    /// **The wait is bounded** ([`LINK_HEALTH_POLL`]), and every wake asks the link whether it is
    /// still there. Without that this is where an idle writer sits for ever: nothing is queued, so
    /// nothing fails, so `link_down` — which only a failed write used to set — stays clear on a
    /// cable that has been out of its socket for half a minute (§2.7, measured). The check is a
    /// question, not a write: see [`Link::is_down`].
    fn next_entry(&self) -> Option<Entry> {
        loop {
            {
                let mut q = lock(&self.shared.queue);
                if self.shared.cancel.load(Ordering::SeqCst)
                    || self.shared.shutting_down.load(Ordering::SeqCst)
                    // §12 Stage 4c. Without this an **idle** writer never services a refresh: it
                    // parks in this loop, and the loop only leaves it for an entry or a failure.
                    // The request is a wake-up like any other, and the check belongs where the
                    // other two are — under the queue lock every trigger also takes, so a flag set
                    // between the test and the wait cannot lose its wake-up.
                    || self.shared.refresh_info.load(Ordering::SeqCst)
                {
                    return None;
                }
                if let Some(entry) = q.pop(&self.shared.counters) {
                    return Some(entry);
                }
                let _unused = self
                    .shared
                    .wake
                    .wait_timeout(q, LINK_HEALTH_POLL)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            // Off the queue lock, because `mark_link_down` takes the link-state mutex and no path
            // in this file holds both.
            if self.note_link_lost() {
                return None;
            }
        }
    }

    /// Ask the transport whether it has failed, and record it if it has (§2.6.1, §2.7).
    ///
    /// `true` only on the transition, so the caller is sent back to the top of the loop — where a
    /// reconnecting writer drops the stale link and seeks a replacement, and a Stage 1 writer
    /// parks again with the failure now visible in [`crate::input::Stats`]. Reporting it once is
    /// what keeps an idle writer that cannot reconnect from spinning on a dead link.
    ///
    /// Dropping the fd is the other half of why this matters, and it belongs to
    /// [`Writer::acquire`]: while the writer holds a hung-up `/dev/ttyACM1`, `acm_port_destruct`
    /// cannot run, the tty index is not freed, and the dongle re-enumerates as `/dev/ttyACM2` —
    /// measured. `acquire` clears `self.link` first thing, before any backoff, for exactly that
    /// reason.
    ///
    /// **Which means a Stage 1 (`spawn`) writer never drops it**, because `acquire` is only
    /// reached when `reconnect` is set, and this is deliberate rather than an oversight. Freeing
    /// the tty index is a §2.7 concern — it exists so that the *reopen* finds the device — and a
    /// `spawn`ed writer has no source to reopen from: its link was handed to it and there will
    /// never be another. Clearing it there would destroy the caller's transport while the writer
    /// is still running, on a path where nothing can use the space that frees. C1 asks only that
    /// the outage be recorded and the ack advanced so input is not wedged for good, and that
    /// happens below whether or not the fd goes. It is one more reason `main.rs` builds its writer
    /// with `spawn_with_source`.
    fn note_link_lost(&self) -> bool {
        if self.shared.link_down.load(Ordering::SeqCst) {
            // Already known and already reported; the loop is not owed another trip.
            return false;
        }
        if !self.link.as_ref().is_some_and(|link| link.is_down()) {
            return false;
        }
        log::warn!(
            "the transport reported itself down with nothing queued; no write was needed to \
             discover it"
        );
        self.shared.mark_link_down();
        // §2.6.1, and the same two steps a failed write takes: the session ended with the cable,
        // so it is released and the ack advances. The release itself will be recorded `Unsent` —
        // there is nothing left to send it on — which is precisely the warning the viewer turns
        // into "the target may still be holding keys". Going through the ordinary trigger is what
        // keeps this from being a second path: `run` services the cancellation first and only then
        // looks for a replacement, exactly as it does after a write failure.
        self.shared.trigger(ReleaseReason::LinkDown);
        true
    }

    // ---------------------------------------------------------------- cancellation

    /// The §2.6 cancellation sequence, run to completion in this thread.
    fn cancellation_sequence(&mut self) {
        let (epoch, reason) = {
            let mut q = lock(&self.shared.queue);
            // Latch `requested_epoch` as E, then drain and discard every queued event with
            // epoch <= E, writing none of them.
            let epoch = self.shared.requested_epoch.load(Ordering::SeqCst);
            let reason = q
                .pending_reason
                .take()
                .unwrap_or(ReleaseReason::UserRequested);
            q.drain_upto(epoch, &self.shared.counters);
            (epoch, reason)
        };

        let outcome = self.write_release_all();

        // Clear the tracked state: physical held set with its per-key statuses, modifier mask,
        // button mask (§2.5, §2.6).
        self.held.clear();
        // …and the pointer mode with it. The absolute position was this session's, and the next
        // session may open with a `Button` before any motion: emitting that as an absolute report
        // would click at the *previous* session's coordinates on a live console. With no known
        // position the mode is `Rel`, whose zero-motion report clicks wherever the pointer is.
        self.mode = PointerMode::Rel;

        *lock(&self.shared.last_release) = Some(ReleaseRecord {
            epoch,
            reason,
            outcome,
        });
        Counters::bump(&self.shared.counters.cancellations);

        {
            let _q = lock(&self.shared.queue);
            self.shared.acked_epoch.store(epoch, Ordering::SeqCst);
            // Clear the flag — unless a trigger arrived after the latch, in which case its
            // request is not covered by this sequence and must run its own. The plan says
            // "clear the flag"; clearing unconditionally would strand that trigger with
            // `requested_epoch > acked_epoch` and no flag set, and `engage` would never succeed.
            if self.shared.requested_epoch.load(Ordering::SeqCst) == epoch {
                self.shared.cancel.store(false, Ordering::SeqCst);
            }
        }
        self.shared.wake.notify_all();
    }

    /// Synthesize and write the release-all reports directly, not through the queue (§2.6).
    ///
    /// Order: zeroed keyboard report, then — when the session is addressing the absolute mouse
    /// device — an absolute report at the last known position with all buttons up, then
    /// [`MOUSE_RELEASE_ALL`]. The chip presents relative and absolute mice as separate HID
    /// devices, so releasing one does not release the other; the plan requires both.
    ///
    /// The caller resets the pointer mode to `Rel` afterwards: the position this report carries
    /// belonged to the session being cancelled, and carrying it into the next one turns that
    /// session's first `Button` into a click at a stale coordinate.
    fn write_release_all(&mut self) -> ReleaseOutcome {
        if self.link.is_none() || self.shared.link_down.load(Ordering::SeqCst) {
            // The transport is already known dead; further transacts would only block. §2.6.1:
            // record it as unsent and advance the ack anyway.
            return ReleaseOutcome::Unsent;
        }
        self.emit_release_all()
    }

    /// The release frames themselves, with no health check in front of them.
    ///
    /// Split out for §2.7 step 2, which writes a release-all on a link that has just been opened
    /// while `link_down` is — correctly — still set: the link is not in service until `GET_INFO`
    /// has proved it (step 3d), and this write is part of that proof.
    fn emit_release_all(&mut self) -> ReleaseOutcome {
        let kb = KeyboardReport::RELEASE_ALL.payload();
        if self.write_frame(cmd::SEND_KB_GENERAL_DATA, &kb).is_err() {
            return ReleaseOutcome::Unsent;
        }
        if let PointerMode::Abs { x, y } = self.mode {
            let abs = MouseAbsReport {
                buttons: 0,
                x,
                y,
                wheel: 0,
            }
            .payload();
            if self.write_frame(cmd::SEND_MS_ABS_DATA, &abs).is_err() {
                return ReleaseOutcome::Unsent;
            }
        }
        let rel = MOUSE_RELEASE_ALL.payload();
        if self.write_frame(cmd::SEND_MS_REL_DATA, &rel).is_err() {
            return ReleaseOutcome::Unsent;
        }
        ReleaseOutcome::Submitted
    }

    // ---------------------------------------------------------------- reconnect (§2.7)

    /// Get the writer its first transport.
    ///
    /// Two shapes, because the two entry points promise different things:
    ///
    /// - [`crate::input::spawn`] hands over a link its caller has already opened and proved with
    ///   its own `GET_INFO`. There is nothing to resynchronise and no queue to discard, so the
    ///   link is taken from the source and used as it is — Stage 1's behaviour, unchanged.
    /// - [`crate::input::spawn_with_source`] owns the opening, so the first link goes through the
    ///   full §2.7 sequence like every later one. A release-all at startup is harmless and §2.7
    ///   wants the target's HID state resynchronised, so it is not special-cased; and the
    ///   `GET_INFO` that ends the sequence is what turns a startup failure into a reported
    ///   condition (`Stats::link_down`, `Producer::wait_for_link`) rather than a panic.
    fn acquire_first_link(&mut self) {
        if self.reconnect {
            self.acquire(true);
            return;
        }
        // Not `open_bounded`: `spawn`'s source hands over a link its caller already opened, so this
        // call cannot block, and routing it through the helper thread would let a shutdown racing
        // startup drop a perfectly good link and report its release `Unsent` — a change to Stage 1
        // behaviour for no gain.
        match self.source.as_mut().map(|s| s.open()) {
            Some(Ok(link)) => self.link = Some(link),
            Some(Err(e)) => {
                log::error!("no transport at startup: {e}");
                self.shared.mark_link_down();
            }
            None => {
                log::error!("no transport at startup: no source");
                self.shared.mark_link_down();
            }
        }
    }

    /// [`LinkSource::open`] on a helper thread, so that the writer's wait for it is bounded by a
    /// flag check rather than by the transport.
    ///
    /// `None` means a shutdown was requested while the open was in flight and the caller must stop
    /// reconnecting; anything else is what the source answered.
    ///
    /// **Why this is not a plain call.** Opening a serial port is `open(2)` plus a `tcsetattr`,
    /// which on a device that has just re-enumerated is a `SET_LINE_CODING` control transfer whose
    /// kernel bound is seconds. The writer cannot interrupt that, and it is what
    /// [`crate::input::WriterHandle::shutdown`] joins on: without this, a shutdown asked for while
    /// a port is opening waits out the open, on a program that is holding someone else's console.
    ///
    /// So the open happens on a thread that owns the source for the duration, and the writer waits
    /// on the condvar every trigger already notifies — with [`OPEN_POLL`] as a failsafe, so the
    /// bound holds even if a wake-up is missed. When the writer gives up on it, the channel's
    /// receiver goes with it: the helper's `send` then fails, and **the link it opened is dropped
    /// right there, never having been written to**. That is the property shutdown needs — nothing
    /// is left behind that could put bytes on the port after `shutdown` returns.
    ///
    /// The source is not returned in that case, so `self.source` stays `None` and no further open
    /// is possible. That is correct rather than lossy: the only path here is a shutdown, which is
    /// a one-way door (`Shared::shutting_down` is never cleared).
    fn open_bounded(&mut self) -> Option<Result<Box<dyn Link>, LinkError>> {
        let Some(mut source) = self.source.take() else {
            // Only reachable after a shutdown abandoned the source, and the caller returns at once.
            return None;
        };
        let shared = Arc::clone(&self.shared);
        let (tx, rx) = std::sync::mpsc::channel();
        // `std::thread::spawn` rather than `Builder::spawn` for the reason `input::start` gives:
        // the only failure is the OS refusing a thread, and there is nothing useful to do with it
        // here — a `Builder` error would also drop the source it had already moved into the closure.
        std::thread::spawn(move || {
            let opened = source.open();
            {
                // Sent under the lock the writer waits on, so the notify below cannot land in the
                // window between the writer's `try_recv` and its `wait_timeout`.
                let _q = lock(&shared.queue);
                // On `Err` the writer has gone and this drops the link, unwritten to.
                let _ = tx.send((source, opened));
            }
            shared.wake.notify_all();
        });

        loop {
            let q = lock(&self.shared.queue);
            match rx.try_recv() {
                Ok((source, opened)) => {
                    self.source = Some(source);
                    return Some(opened);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // The helper panicked. Report it as a failed attempt; the source went with it,
                    // so every later attempt returns `None` and the writer stops reconnecting.
                    return Some(Err(LinkError::Down(format!(
                        "the thread opening {} died",
                        self.source_desc
                    ))));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
            if self.shared.shutting_down.load(Ordering::SeqCst) {
                return None;
            }
            let _unused = self
                .shared
                .wake
                .wait_timeout(q, OPEN_POLL)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    /// Loop until a link has been commissioned, or a shutdown asks the writer to stop (§2.7).
    ///
    /// `initial` distinguishes the first link from a replacement in two places only: the first
    /// attempt goes out immediately rather than after a backoff, and success does not count as a
    /// *re*-connect.
    fn acquire(&mut self, initial: bool) {
        // Whatever is left of the old transport goes now, before anything waits: its reader thread
        // and its port handle have no business outliving the decision to replace it.
        self.link = None;
        if !initial {
            log::warn!(
                "serial link down; reconnecting via {} (backoff {:?}..{:?})",
                self.source_desc,
                self.reconnect_cfg.initial_backoff,
                self.reconnect_cfg.max_backoff
            );
        }
        // A zero backoff would be a spin, so it is floored rather than trusted; the doubling is
        // saturating for the same reason.
        let floor = Duration::from_millis(1);
        let max = self.reconnect_cfg.max_backoff.max(floor);
        let mut backoff = self.reconnect_cfg.initial_backoff.max(floor).min(max);
        let mut attempt = 0u64;
        let mut wait_first = !initial;

        loop {
            if wait_first && !self.backoff_wait(backoff) {
                return;
            }
            wait_first = true;
            if self.shared.shutting_down.load(Ordering::SeqCst) {
                return;
            }
            attempt += 1;
            Counters::bump(&self.shared.counters.reconnect_attempts);
            match self.open_bounded() {
                // A shutdown landed while the open was in flight. The link it may still produce is
                // dropped by the helper thread without ever being written to; the ordinary
                // shutdown path in `run` then records the release it owes as `Unsent`, because
                // there is no transport to send it on.
                None => return,
                Some(Ok(link)) => {
                    self.link = Some(link);
                    if self.commission(initial, attempt) {
                        return;
                    }
                    // Not accepted. The link is dropped here rather than kept "just in case": a
                    // transport that cannot answer `GET_INFO` is not one to send keystrokes down.
                    self.link = None;
                }
                Some(Err(e)) => log::warn!("attempt {attempt} to open {}: {e}", self.source_desc),
            }
            backoff = backoff.saturating_mul(2).min(max);
        }
    }

    /// Wait up to `backoff` for something to change. `false` means a shutdown was requested and
    /// the caller must stop reconnecting.
    ///
    /// The wait is on the condvar rather than a sleep, so a shutdown is noticed at once instead of
    /// after up to `max_backoff` — the difference between an exit that feels instant and one that
    /// hangs for four seconds on a machine driving someone else's console.
    ///
    /// A cancellation trigger arriving here is **serviced here** (§2.6.1). Its release is `Unsent`
    /// on a dead link and that is recorded, but the ack must still advance: otherwise a reconnect
    /// that never succeeds leaves `requested_epoch` permanently above `acked_epoch`, `engage`
    /// never succeeds again, and a dead cable has wedged input for good — the exact failure
    /// §2.6.1 forbids.
    fn backoff_wait(&mut self, backoff: Duration) -> bool {
        let deadline = Instant::now() + backoff;
        loop {
            if self.shared.shutting_down.load(Ordering::SeqCst) {
                return false;
            }
            if self.shared.cancel.load(Ordering::SeqCst) {
                self.cancellation_sequence();
                continue;
            }
            // §12 Stage 4c: a refresh asked for while the writer is hunting for a link is answered
            // here, without a transport — `refresh_device_info` sees `link_down` and records that
            // nobody could ask. Otherwise a caller waiting on the generation waits out the whole
            // reconnect ladder, which for a cable that is not coming back is for ever.
            if self.shared.refresh_info.load(Ordering::SeqCst) {
                self.refresh_device_info();
                continue;
            }
            let q = lock(&self.shared.queue);
            // Re-read under the lock, which every trigger also takes: without this the flag could
            // be set between the checks above and the wait below, and the wake-up lost.
            if self.shared.shutting_down.load(Ordering::SeqCst)
                || self.shared.cancel.load(Ordering::SeqCst)
                || self.shared.refresh_info.load(Ordering::SeqCst)
            {
                continue;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return true;
            };
            let _unused = self
                .shared
                .wake
                .wait_timeout(q, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    /// The §2.7 sequence on a link that has just been opened, run as a cancellation and as one
    /// uninterrupted unit on this thread. `true` means the link was accepted.
    ///
    /// It is a cancellation because it must be: a producer is allowed to re-engage while the link
    /// is down (`engage` checks only the ack), so without raising an epoch here its events would
    /// be written on the new link — a replay of input produced against a session that no longer
    /// exists, which is exactly what §2.7 step 1 forbids.
    ///
    /// The sequence is deliberately the plan's order, with the preamble in front of it:
    ///
    /// | | | |
    /// | --- | --- | --- |
    /// | a | `resync` | finish any frame the chip is still waiting on, **before** the release-all, so the release-all is not itself eaten as the remainder (§5.1) |
    /// | b | drain | never replay (§2.7 step 1) |
    /// | c | release-all | the target's HID state after a link failure is unknown (§2.7 step 2) |
    /// | d | `GET_INFO` | the proof: a matched reply means the parser is realigned and a CH9329 is there (§2.7 step 3) |
    ///
    /// Failure at any step rejects the link. The epoch is still acknowledged, with outcome
    /// `Unsent`, for the §2.6.1 reason spelled out on [`Writer::backoff_wait`].
    ///
    /// The producer stays **disengaged** afterwards: §2.8 requires deliberate recapture, and the
    /// viewer's next grab is the right moment for input to resume.
    fn commission(&mut self, initial: bool, attempt: u64) -> bool {
        let (epoch, pending) = self.shared.begin_reconnect();
        // A trigger that landed while the link was being opened has no sequence of its own left to
        // run: this one covers its epoch. Its reason is folded in rather than stranded — kept when
        // it outranks `Reconnected` (only `Shutdown` does), and otherwise subsumed by it, since a
        // release that went out because the cable came back is better described by the cable than
        // by the focus loss that coincided with it. Leaving it in the slot would mislabel the next,
        // unrelated release.
        let reason = match pending {
            Some(r) if r.severity() > ReleaseReason::Reconnected.severity() => r,
            _ => ReleaseReason::Reconnected,
        };
        let result = self.commission_steps(epoch);

        // The tracked state goes whatever happened: the held set and the pointer position belonged
        // to a session that ended with the previous cable (§2.6).
        self.held.clear();
        self.mode = PointerMode::Rel;

        let outcome = match &result {
            Ok(_) => ReleaseOutcome::Submitted,
            Err(rejected) => {
                log::warn!("attempt {attempt} rejected: {rejected}");
                ReleaseOutcome::Unsent
            }
        };
        *lock(&self.shared.last_release) = Some(ReleaseRecord {
            epoch,
            reason,
            outcome,
        });
        Counters::bump(&self.shared.counters.cancellations);

        if let Ok(info) = result {
            let down_for = self.shared.mark_link_up(info);
            if !initial {
                Counters::bump(&self.shared.counters.reconnects);
            }
            // `DeviceInfo`'s own `Display`, so the lock bits read the same here as in every
            // command that prints them: `caps=on`, never `caps=true`.
            log::info!(
                "{} on {}: {info} (attempt {attempt}, down for {:?})",
                if initial { "link up" } else { "reconnected" },
                self.source_desc,
                down_for.unwrap_or_default()
            );
        }

        {
            let _q = lock(&self.shared.queue);
            self.shared.acked_epoch.store(epoch, Ordering::SeqCst);
            // B3, exactly as in `cancellation_sequence`: a trigger that arrived after the latch —
            // a shutdown between steps a and d, say — is not covered by this sequence and gets its
            // own, on the link this one just commissioned.
            if self.shared.requested_epoch.load(Ordering::SeqCst) == epoch {
                self.shared.cancel.store(false, Ordering::SeqCst);
            }
        }
        self.shared.wake.notify_all();
        outcome == ReleaseOutcome::Submitted
    }

    /// Steps a to d of [`Writer::commission`]. Separated so the bookkeeping around them runs on
    /// every path, including every failure.
    fn commission_steps(&mut self, epoch: u64) -> Result<DeviceInfo, Rejected> {
        let Some(link) = self.link.as_mut() else {
            return Err(Rejected::ReleaseAll);
        };
        // (a) §5.1: the chip may still be mid-frame from a torn write on the previous link.
        link.resync().map_err(Rejected::Resync)?;

        // (b) §2.7 step 1. Everything queued carries an epoch below this one, so the drain is
        // total; it is done explicitly, and counted, because "never replay" is the requirement and
        // relying on the epoch check to notice later would leave the entries sitting in the queue.
        let discarded = {
            let mut q = lock(&self.shared.queue);
            q.drain_upto(epoch, &self.shared.counters)
        };
        if discarded > 0 {
            self.shared
                .counters
                .queue_discarded_on_reconnect
                .fetch_add(discarded as u64, Ordering::SeqCst);
            log::info!("discarded {discarded} queued entries rather than replaying them (§2.7)");
        }

        // (c) §2.7 step 2.
        if self.emit_release_all() == ReleaseOutcome::Unsent {
            return Err(Rejected::ReleaseAll);
        }

        // (d) §2.7 step 3, and the proof the link is real. A timeout, a transport failure or a
        // short payload all mean the same thing here: this is not a link to send keystrokes down.
        let timeout = self.reconnect_cfg.get_info_timeout;
        let link = self.link.as_mut().ok_or(Rejected::ReleaseAll)?;
        let reply = link
            .transact(cmd::GET_INFO, &[], timeout)
            .map_err(Rejected::GetInfo)?;
        DeviceInfo::parse(&reply.data).map_err(Rejected::Parse)
    }

    /// Ask the device for its current `GET_INFO` and publish it (§12 Stage 4c).
    ///
    /// The flag is cleared **first**, so a request that arrives while this transaction is in
    /// flight is not swallowed by the clear: it survives as a second request and is serviced on
    /// the next pass. The generation is advanced on every completed attempt, successful or not, so
    /// a caller waiting for it cannot be wedged by a device that will not answer — it sees the
    /// generation move and `device_info` unchanged, which is the honest report of "asked, and this
    /// is still the newest answer there is".
    ///
    /// **A failed transaction here does not end the session.** This is a read-only convenience —
    /// no keystroke of the user's depends on it — and one timed-out `GET_INFO` is not evidence
    /// that the transport is gone: the chip pushes unsolicited frames, and a reply that did not
    /// arrive in time is a degraded link rather than a dead one. Escalating it to a
    /// `ReleaseReason::LinkDown` cancellation would take a user's capture away on behalf of a menu
    /// item that only wanted to know whether CapsLock was on. What it does instead is record the
    /// failure, which makes the reading **stale** and makes the paste refuse rather than type
    /// through an unknown lock state (D2, `chrome::paste`); if the link really has failed, the
    /// next write finds it and §2.6.1 runs on the path that owns that decision.
    fn refresh_device_info(&mut self) {
        self.shared.refresh_info.store(false, Ordering::SeqCst);
        if self.shared.link_down.load(Ordering::SeqCst) {
            self.shared.note_info_refresh_failed();
            return;
        }
        let timeout = self.reconnect_cfg.get_info_timeout;
        let Some(link) = self.link.as_mut() else {
            self.shared.note_info_refresh_failed();
            return;
        };
        match link.transact(cmd::GET_INFO, &[], timeout) {
            Ok(reply) => match DeviceInfo::parse(&reply.data) {
                Ok(info) => {
                    log::debug!("device info refreshed on request: {info}");
                    self.shared.publish_device_info(info);
                }
                Err(e) => {
                    log::warn!("the refreshed GET_INFO reply was unusable: {e}");
                    self.shared.note_info_refresh_failed();
                }
            },
            Err(e) => {
                log::warn!(
                    "the refresh GET_INFO did not answer: {e}. The session is left engaged; the \
                     reading is stale, which is what a paste refuses on"
                );
                self.shared.note_info_refresh_failed();
            }
        }
    }

    // ---------------------------------------------------------------- report emission

    fn write_entry(&mut self, entry: Entry) -> Result<(), Down> {
        match entry.kind {
            EntryKind::Motion(run) => self.flush_motion(run),
            EntryKind::Key { key, down } => {
                // Every keyboard state change that changes the report is one transact. Keyboard
                // reports are never coalesced (§2.3).
                match self.held.apply_key(key, down) {
                    Some(report) => self.write_frame(cmd::SEND_KB_GENERAL_DATA, &report.payload()),
                    None => Ok(()),
                }
            }
            EntryKind::Button { mask, down } => match self.held.apply_button(mask, down) {
                Some(buttons) => self.write_buttons(buttons),
                None => Ok(()),
            },
        }
    }

    /// A button transition carries no position of its own: in `Abs` mode it goes out as an
    /// absolute report at the last known position with the new mask, in `Rel` mode as a
    /// zero-motion relative report with the new mask.
    fn write_buttons(&mut self, buttons: u8) -> Result<(), Down> {
        match self.mode {
            PointerMode::Abs { x, y } => {
                let p = MouseAbsReport {
                    buttons,
                    x,
                    y,
                    wheel: 0,
                }
                .payload();
                self.write_frame(cmd::SEND_MS_ABS_DATA, &p)
            }
            PointerMode::Rel => {
                let p = MouseRelReport {
                    buttons,
                    dx: 0,
                    dy: 0,
                    wheel: 0,
                }
                .payload();
                self.write_frame(cmd::SEND_MS_REL_DATA, &p)
            }
        }
    }

    /// Flush one coalesced motion run (§2.3).
    ///
    /// Accumulated deltas **saturate at the report range and split across multiple reports**
    /// rather than clamping and losing the remainder: 300 goes out as 127 + 127 + 46. The run as
    /// a whole is capped at [`REL_RUN_MAX`] per axis first — see the module documentation.
    ///
    /// A run that carries both an absolute position and relative deltas cannot arise from a
    /// viewer that picks one pointer mode, and the plan does not say what it means. The chosen
    /// order is position first, then the deltas — the position is state and the deltas are
    /// movement from it — which leaves the mode `Rel`, since that is the last motion flushed.
    ///
    /// Every report is one [`Link::transact`], and the cancellation flag is re-read after each of
    /// them (§2.6: the check is between *frames*). When it is set the rest of the run is abandoned
    /// and the loop above enters the cancellation sequence; the abandoned motion carries an epoch
    /// `<= E` and would have been drained had it still been queued.
    fn flush_motion(&mut self, mut run: MotionRun) -> Result<(), Down> {
        self.cap_run(&mut run);
        if run.is_empty() {
            // Nothing observable: a zero-delta submission produces no frame.
            return Ok(());
        }
        let buttons = self.held.buttons();
        let has_rel = run.dx != 0 || run.dy != 0;
        let mut wheel = run.wheel;

        if let Some((x, y)) = run.abs {
            self.mode = PointerMode::Abs { x, y };
            // The wheel rides with the absolute report unless relative deltas follow, in which
            // case it rides with those instead so it is not counted twice.
            loop {
                let w = if has_rel { 0 } else { take_step(&mut wheel) };
                let p = MouseAbsReport {
                    buttons,
                    x,
                    y,
                    wheel: w,
                }
                .payload();
                if !self.write_step(cmd::SEND_MS_ABS_DATA, &p)? {
                    return Ok(());
                }
                if has_rel || wheel == 0 {
                    break;
                }
            }
        }

        if has_rel {
            self.mode = PointerMode::Rel;
            let (mut dx, mut dy) = (run.dx, run.dy);
            while dx != 0 || dy != 0 || wheel != 0 {
                let p = MouseRelReport {
                    buttons,
                    dx: take_step(&mut dx),
                    dy: take_step(&mut dy),
                    wheel: take_step(&mut wheel),
                }
                .payload();
                if !self.write_step(cmd::SEND_MS_REL_DATA, &p)? {
                    return Ok(());
                }
            }
            return Ok(());
        }

        if run.abs.is_none() {
            // Wheel-only run: a relative report with zero motion in `Rel` mode, an absolute
            // report at the last position in `Abs` mode. The mode is unchanged: a wheel event is
            // not motion, so it does not re-target the pointer device.
            while wheel != 0 {
                let w = take_step(&mut wheel);
                let (command, p) = match self.mode {
                    PointerMode::Abs { x, y } => (
                        cmd::SEND_MS_ABS_DATA,
                        MouseAbsReport {
                            buttons,
                            x,
                            y,
                            wheel: w,
                        }
                        .payload()
                        .to_vec(),
                    ),
                    PointerMode::Rel => (
                        cmd::SEND_MS_REL_DATA,
                        MouseRelReport {
                            buttons,
                            dx: 0,
                            dy: 0,
                            wheel: w,
                        }
                        .payload()
                        .to_vec(),
                    ),
                };
                if !self.write_step(command, &p)? {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Clamp a run's accumulated relative motion and wheel to ±[`REL_RUN_MAX`] per axis, counting
    /// each axis it had to clamp. Saturating, never wrapping. The rationale is in the module
    /// documentation: the report-level "split, never clamp" rule of §2.3 is untouched, and this
    /// bound exists so that a stalled writer cannot owe an unbounded run ahead of a release-all.
    fn cap_run(&self, run: &mut MotionRun) {
        let mut truncated = 0u64;
        for axis in [&mut run.dx, &mut run.dy, &mut run.wheel] {
            let capped = (*axis).clamp(-REL_RUN_MAX, REL_RUN_MAX);
            if capped != *axis {
                truncated += 1;
                *axis = capped;
            }
        }
        if truncated > 0 {
            self.shared
                .counters
                .rel_truncated
                .fetch_add(truncated, Ordering::SeqCst);
        }
    }

    /// One frame of a multi-frame entry, followed by the §2.6 between-frames check.
    ///
    /// `Ok(false)` means a cancellation (or a shutdown) is pending and the rest of this entry must
    /// be abandoned so the release-all can go out at the next frame boundary.
    fn write_step(&mut self, command: u8, payload: &[u8]) -> Result<bool, Down> {
        self.write_frame(command, payload)?;
        Ok(!self.cancelled())
    }

    /// One frame, one transact, wait for the reply (§5.1).
    ///
    /// - `Device` — the frame was rejected but the link is healthy (§3.1): count it and continue.
    /// - `Timeout` — the frame was written and nothing answered: count it, flag degraded, and
    ///   continue. The device may still have acted on it (§2.6.1).
    /// - `Down` / `Io` — the transport is gone: mark the link down and stop writing.
    fn write_frame(&mut self, command: u8, payload: &[u8]) -> Result<(), Down> {
        let Some(link) = self.link.as_mut() else {
            // Between a failure and its replacement there is nothing to write to (§2.7).
            return Err(Down);
        };
        match link.transact(command, payload, self.timeout) {
            Ok(_) => {
                self.count_written(command);
                Ok(())
            }
            Err(LinkError::Device { .. }) => {
                Counters::bump(&self.shared.counters.device_errors);
                self.count_written(command);
                Ok(())
            }
            Err(LinkError::Timeout { .. }) => {
                Counters::bump(&self.shared.counters.timeouts);
                self.shared.degraded.store(true, Ordering::SeqCst);
                self.count_written(command);
                Ok(())
            }
            Err(LinkError::Down(_)) | Err(LinkError::Io(_)) => {
                self.shared.mark_link_down();
                Err(Down)
            }
        }
    }

    /// Count a frame that reached the transport. A device error or a missing reply still means
    /// the bytes went out; only a transport failure means they did not (§2.6.1).
    fn count_written(&self, command: u8) {
        let c = &self.shared.counters;
        match command {
            cmd::SEND_KB_GENERAL_DATA => Counters::bump(&c.kb_reports),
            cmd::SEND_MS_ABS_DATA => Counters::bump(&c.abs_reports),
            cmd::SEND_MS_REL_DATA => Counters::bump(&c.rel_reports),
            _ => {}
        }
    }
}

/// Take up to one report's worth of an accumulated delta, leaving the remainder behind.
///
/// Saturates at ±127 — the report range, and never `-128`, which is outside
/// [`MouseRelReport::DELTA_MAX`] — so the caller loops until the accumulator is empty. Saturate
/// and split, never clamp and lose (§2.3).
fn take_step(remaining: &mut i32) -> i8 {
    let max = MouseRelReport::DELTA_MAX;
    let step = (*remaining).clamp(-max, max);
    *remaining -= step;
    step as i8
}

#[cfg(test)]
mod tests {
    use super::take_step;

    #[test]
    fn a_delta_of_300_splits_into_127_127_46() {
        let mut d = 300;
        let mut steps = Vec::new();
        while d != 0 {
            steps.push(take_step(&mut d));
        }
        assert_eq!(steps, vec![127, 127, 46]);
    }

    #[test]
    fn a_negative_delta_splits_in_the_same_direction() {
        let mut d = -300;
        let mut steps = Vec::new();
        while d != 0 {
            steps.push(take_step(&mut d));
        }
        assert_eq!(steps, vec![-127, -127, -46]);
        assert_eq!(steps.iter().map(|s| i32::from(*s)).sum::<i32>(), -300);
    }
}
