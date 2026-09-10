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
use std::time::Duration;

use crate::input::held::HeldState;
use crate::input::queue::{Entry, EntryKind, MotionRun};
use crate::input::shared::{lock, Shared};
use crate::input::stats::{Counters, ReleaseOutcome, ReleaseRecord};
use crate::input::ReleaseReason;
use crate::link::{Link, LinkError};
use crate::proto::cmd;
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
    /// Boxed rather than generic so Stage 2's reconnect (§2.7) can swap the transport under the
    /// running writer without changing this type.
    link: Box<dyn Link>,
    timeout: Duration,
    held: HeldState,
    mode: PointerMode,
}

impl Writer {
    pub(crate) fn new(shared: Arc<Shared>, link: Box<dyn Link>, timeout: Duration) -> Self {
        Self {
            shared,
            link,
            timeout,
            held: HeldState::new(),
            mode: PointerMode::Rel,
        }
    }

    /// The writer's loop, in the order §2.6 specifies for every iteration.
    pub(crate) fn run(mut self) {
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

    /// Block until there is an entry, a cancellation, or shutdown. Returns `None` when the caller
    /// should re-run the checks at the top of the loop.
    fn next_entry(&self) -> Option<Entry> {
        let mut q = lock(&self.shared.queue);
        loop {
            if self.shared.cancel.load(Ordering::SeqCst)
                || self.shared.shutting_down.load(Ordering::SeqCst)
            {
                return None;
            }
            if let Some(entry) = q.pop(&self.shared.counters) {
                return Some(entry);
            }
            q = self
                .shared
                .wake
                .wait(q)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
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
        if self.shared.link_down.load(Ordering::SeqCst) {
            // The transport is already known dead; further transacts would only block. §2.6.1:
            // record it as unsent and advance the ack anyway.
            return ReleaseOutcome::Unsent;
        }
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

    /// One frame, one transact, wait for the reply (§5.1).    /// One frame, one transact, wait for the reply (§5.1).
    ///
    /// - `Device` — the frame was rejected but the link is healthy (§3.1): count it and continue.
    /// - `Timeout` — the frame was written and nothing answered: count it, flag degraded, and
    ///   continue. The device may still have acted on it (§2.6.1).
    /// - `Down` / `Io` — the transport is gone: mark the link down and stop writing.
    fn write_frame(&mut self, command: u8, payload: &[u8]) -> Result<(), Down> {
        match self.link.transact(command, payload, self.timeout) {
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
                self.shared.link_down.store(true, Ordering::SeqCst);
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
