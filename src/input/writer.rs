//! Serialised input delivery, held-state tracking, and reconnect.
//!
//! Cancellation is checked between reports, including reports split from a motion run.
//! Each cancellation advances local acknowledgement even when its release cannot be sent.
//! This keeps recapture from waiting forever on a disconnected device.
//!
//! Reconnect discards queued input, resynchronises the bridge, releases held keys, and
//! refreshes device information. Input remains disengaged until explicitly captured.

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

/// The transport failed on this frame. Nothing further may be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Down;

/// The cap on one coalesced motion run's accumulated delta, per axis.
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

/// Idle interval for checking [`Link::is_down`] without writing to the device.
/// Submissions and cancellation wake the writer immediately; idle health checks
/// otherwise occur every 100 ms.
const LINK_HEALTH_POLL: Duration = Duration::from_millis(100);

/// Last active pointer mode. Button and wheel reports use it because they carry no
/// position. Cancellation resets it to relative mode to avoid reusing a stale position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PointerMode {
    Rel,
    Abs { x: u16, y: u16 },
}

pub(crate) struct Writer {
    shared: Arc<Shared>,
    /// Where a replacement transport comes from. Consulted once at startup and again after
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
    /// Whether failed transports can be replaced. Single-link writers cannot reopen
    /// and must not retry an exhausted source.
    reconnect: bool,
    timeout: Duration,
    reconnect_cfg: ReconnectConfig,
    held: HeldState,
    mode: PointerMode,
}

/// Why one attempt at commissioning a link was rejected. Every variant means the
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

    /// Run delivery, checking cancellation before reconnect or queued input.
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
            // Finish the pending release before acquiring a replacement link.
            if self.reconnect && self.shared.link_down.load(Ordering::SeqCst) {
                self.acquire(false);
                continue;
            }
            // A fresh `GET_INFO` was asked for. Serviced here, between frames
            // and on this thread, for the same reason every write is: the writer is the sole
            // serialization point, and a second thread transacting on the link would
            // interleave two frames on a chip with no inter-byte timeout. `continue` rather
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
            // session's own events carry `acked_epoch` (see `shared`).
            if entry.epoch < self.shared.acked_epoch.load(Ordering::SeqCst) {
                Counters::bump(&self.shared.counters.stale_discarded);
                continue;
            }
            if self.shared.link_down.load(Ordering::SeqCst) {
                // A fixed-link writer cannot recover. Discard queued work while preserving
                // bounded memory and shutdown.
                Counters::bump(&self.shared.counters.stale_discarded);
                continue;
            }
            if self.write_entry(entry).is_err() {
                // Advance local acknowledgement even when release cannot be sent, or recapture wedges.
                self.shared.trigger(ReleaseReason::LinkDown);
            }
        }
    }

    /// Whether the writer may exit.
    ///
    /// Only when nothing is pending: `shutting_down` alone is not enough, because the shutdown
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
    /// the writer is shutting down. Read between frames, never inside one.
    fn cancelled(&self) -> bool {
        self.shared.cancel.load(Ordering::SeqCst)
            || self.shared.shutting_down.load(Ordering::SeqCst)
    }

    /// Block until there is an entry, a cancellation, a shutdown, a `GET_INFO` refresh request —
    /// or the transport reports itself gone. Returns `None` when the caller should re-run the
    /// checks at the top of the loop.
    ///
    /// The wait is bounded ([`LINK_HEALTH_POLL`]), and every wake asks the link whether it is
    /// still there. Without that this is where an idle writer sits for ever: nothing is queued, so
    /// nothing fails, so `link_down` — which only a failed write used to set — stays clear on a
    /// cable that has been out of its socket for half a minute. The check is a
    /// question, not a write: see [`Link::is_down`].
    fn next_entry(&self) -> Option<Entry> {
        loop {
            {
                let mut q = lock(&self.shared.queue);
                if self.shared.cancel.load(Ordering::SeqCst)
                    || self.shared.shutting_down.load(Ordering::SeqCst)
                    // Check refresh requests under the queue lock so an idle writer cannot lose the wake-up.
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

    /// Record a newly detected transport failure. Returns `true` only for the transition.
    ///
    /// Reconnect drops the old descriptor in `acquire` before retrying; a held descriptor
    /// can reserve the device's former node number. Single-link writers retain their link
    /// and park after recording the failure.
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
        // Use the ordinary cancellation path so idle link loss advances the release epoch
        // and reports an unsent release just like write-driven loss.
        self.shared.trigger(ReleaseReason::LinkDown);
        true
    }

    // ---------------------------------------------------------------- cancellation

    /// Complete a latched cancellation epoch on the writer thread.
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
        // button mask.
        self.held.clear();
        // Reset pointer mode so a button before the next motion cannot use old coordinates.
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
            // A newer trigger needs another release. Clearing its flag here would leave
            // `requested_epoch > acked_epoch` with no work scheduled.
            if self.shared.requested_epoch.load(Ordering::SeqCst) == epoch {
                self.shared.cancel.store(false, Ordering::SeqCst);
            }
        }
        self.shared.wake.notify_all();
    }

    /// Write zeroed keyboard and mouse reports without using the queue.
    ///
    /// Absolute mode also releases buttons at the last position: the bridge exposes
    /// absolute and relative mice separately, so releasing one cannot release the other.
    fn write_release_all(&mut self) -> ReleaseOutcome {
        if self.link.is_none() || self.shared.link_down.load(Ordering::SeqCst) {
            // A dead transport cannot carry the release; report it unsent and advance the epoch.
            return ReleaseOutcome::Unsent;
        }
        self.emit_release_all()
    }

    /// Write release reports without checking link health.
    ///
    /// Commissioning calls this while the new link is still marked down, before `GET_INFO`
    /// confirms it is ready for input.
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

    // ---------------------------------------------------------------- reconnect

    /// Acquire the initial transport.
    ///
    /// `spawn` accepts its caller's already-initialized link. `spawn_with_source` commissions
    /// the first link through the same recovery sequence as later replacements.
    fn acquire_first_link(&mut self) {
        if self.reconnect {
            self.acquire(true);
            return;
        }
        // Not `open_bounded`: `spawn`'s source hands over a link its caller already opened, so this
        // call cannot block, and routing it through the helper thread would let a shutdown racing
        // startup drop a perfectly good link and report its release `Unsent` — a change to behaviour for no gain.
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

    /// Open a transport on a helper thread while polling cancellation and shutdown.
    ///
    /// Serial configuration may block inside the kernel. `None` means shutdown abandoned
    /// the open; the helper then drops its result without sending input. The source is
    /// not returned after abandonment because shutdown permanently closes admission.
    fn open_bounded(&mut self) -> Option<Result<Box<dyn Link>, LinkError>> {
        let Some(mut source) = self.source.take() else {
            // Only reachable after a shutdown abandoned the source, and the caller returns at once.
            return None;
        };
        let shared = Arc::clone(&self.shared);
        let (tx, rx) = std::sync::mpsc::channel();
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

    /// Retry commissioning until a link is ready or shutdown is requested. The initial
    /// attempt is immediate and does not increment the reconnect count.
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

    /// Wait up to `backoff`; return `false` on shutdown.
    ///
    /// The condition variable wakes promptly for cancellation. A release during an outage
    /// is recorded as unsent and still acknowledged, even if the device never returns.
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
            // Complete refresh attempts during reconnect too, so callers receive a stale result
            // instead of waiting indefinitely for the device.
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

    /// Commission a newly opened link as one cancellation sequence.
    ///
    /// Resynchronise the parser, discard old input, release held keys, then query device
    /// information. Any failed step rejects the link. The epoch is acknowledged even on
    /// failure, and the producer stays disengaged until recapture. Returns success.
    fn commission(&mut self, initial: bool, attempt: u64) -> bool {
        let (epoch, pending) = self.shared.begin_reconnect();
        // Fold pending triggers into this epoch so they cannot mislabel a later release.
        let reason = match pending {
            Some(r) if r.severity() > ReleaseReason::Reconnected.severity() => r,
            _ => ReleaseReason::Reconnected,
        };
        let result = self.commission_steps(epoch);

        // The old session's held state and pointer position are invalid after reconnect.
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
            // Triggers arriving after the latched epoch need their own release on the new link.
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
        // The bridge may still be waiting for bytes from a torn frame.
        link.resync().map_err(Rejected::Resync)?;

        // Discard stale entries now so they cannot be replayed or occupy queue space.
        let discarded = {
            let mut q = lock(&self.shared.queue);
            q.drain_upto(epoch, &self.shared.counters)
        };
        if discarded > 0 {
            self.shared
                .counters
                .queue_discarded_on_reconnect
                .fetch_add(discarded as u64, Ordering::SeqCst);
            log::info!("discarded {discarded} queued entries rather than replaying them");
        }

        if self.emit_release_all() == ReleaseOutcome::Unsent {
            return Err(Rejected::ReleaseAll);
        }

        // Only a valid device-info reply establishes that the link is ready for input.
        let timeout = self.reconnect_cfg.get_info_timeout;
        let link = self.link.as_mut().ok_or(Rejected::ReleaseAll)?;
        let reply = link
            .transact(cmd::GET_INFO, &[], timeout)
            .map_err(Rejected::GetInfo)?;
        DeviceInfo::parse(&reply.data).map_err(Rejected::Parse)
    }

    /// Refresh device information between input reports.
    ///
    /// Clear the flag before the transaction so new requests survive. Every attempt
    /// advances the generation; failure marks cached information stale. A query timeout
    /// alone does not cancel capture: transport reads and writes detect actual link loss.
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
                // reports are never coalesced.
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

    /// Flush a coalesced motion run, checking cancellation after every report.
    ///
    /// Each axis is first bounded by [`REL_RUN_MAX`], then split into report-sized deltas.
    /// If absolute and relative motion coexist, send the position before the deltas and
    /// leave the active mode relative. Cancellation abandons the remaining run.
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

    /// Bound each accumulated axis to ±[`REL_RUN_MAX`] and count truncations.
    /// This limits backlog after a stalled writer; bounded deltas still split across reports.
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

    /// Send one report and check cancellation.
    ///
    /// `Ok(false)` means the remaining entry must be abandoned to service release-all.
    fn write_step(&mut self, command: u8, payload: &[u8]) -> Result<bool, Down> {
        self.write_frame(command, payload)?;
        Ok(!self.cancelled())
    }

    /// One frame, one transact, wait for the reply.
    ///
    /// - `Device` — the frame was rejected but the link is healthy: count it and continue.
    /// - `Timeout` — the frame was written and nothing answered: count it, flag degraded, and
    ///   continue. The device may still have acted on it.
    /// - `Down` / `Io` — the transport is gone: mark the link down and stop writing.
    fn write_frame(&mut self, command: u8, payload: &[u8]) -> Result<(), Down> {
        let Some(link) = self.link.as_mut() else {
            // Between a failure and its replacement there is nothing to write to.
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
    /// the bytes went out; only a transport failure means they did not.
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
/// and split, never clamp and lose.
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
