//! Independent audio capture and playback workers.
//!
//! A bounded period ring separates the device and host clocks. Each side reports opening,
//! failure, and recovery independently; audio failure does not stop keyboard or video.
//! Stop waits for a bounded interval and detaches an unresponsive worker.

pub mod alsa;
pub mod pcm;
pub mod ring;
pub mod tone;

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub use pcm::{PcmSink, PcmSinkOpener, PcmSource, PcmSourceOpener};
pub use ring::{
    DriftAction, DriftPolicy, PeriodRing, PopOutcome, PushOutcome, RingConfig, RingCounts,
    CHANNELS, SAMPLE_RATE,
};

/// Why audio is not running, in the words the title and the log use.
///
/// Every variant names the device it is about, because "audio failed" with no node in it is
/// unactionable: the capture card and the playback sink fail for different reasons and are fixed
/// in different places.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioError {
    /// Discovery could not name a card for this dongle. Carries discovery's own explanation.
    NoCard(String),
    /// Something else holds the card. On a PipeWire desk that is PipeWire itself, but only while
    /// something is recording from its node — so this is a condition that comes and goes.
    Busy { device: String, why: String },
    /// The device was there and is not any more: unplugged, unbound, or the daemon behind
    /// `default` restarted.
    Gone { device: String, why: String },
    /// The device is there and would not open in the one format it has.
    Open { device: String, why: String },
    /// A read or a write failed for some other reason.
    Io { device: String, why: String },
}

impl AudioError {
    /// The device this is about, for the log line and the title.
    pub fn device(&self) -> &str {
        match self {
            AudioError::NoCard(_) => "(no card)",
            AudioError::Busy { device, .. }
            | AudioError::Gone { device, .. }
            | AudioError::Open { device, .. }
            | AudioError::Io { device, .. } => device,
        }
    }

    /// The coarse kind, which is what "log this once" is keyed on together with the message. Two
    /// different messages of the same kind are two different conditions; the same message twice
    /// is one condition observed twice.
    pub fn kind(&self) -> &'static str {
        match self {
            AudioError::NoCard(_) => "no card",
            AudioError::Busy { .. } => "busy",
            AudioError::Gone { .. } => "gone",
            AudioError::Open { .. } => "open failed",
            AudioError::Io { .. } => "I/O error",
        }
    }
}

impl std::error::Error for AudioError {}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AudioError::NoCard(why) => write!(f, "no sound card: {why}"),
            AudioError::Busy { device, why } => {
                write!(f, "{device} is busy: {why}. Another client holds the card")
            }
            AudioError::Gone { device, why } => write!(f, "{device} is gone: {why}"),
            AudioError::Open { device, why } => write!(f, "cannot open {device}: {why}"),
            AudioError::Io { device, why } => write!(f, "{device}: {why}"),
        }
    }
}

/// Which half of the audio path a condition is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioSide {
    /// The dongle's card.
    Capture,
    /// ALSA `default`.
    Playback,
}

impl AudioSide {
    /// The two sides in the order everything here reports them: capture first.
    ///
    /// The order is part of the contract rather than an accident of iteration. Both sides can be
    /// in a condition at once — an unplugged dongle and a restarted PipeWire are one event on
    /// the recorded test setup — and a title that alternated between the two four times a second would be
    /// unreadable. Capture first because it is the one the user can usually do something about.
    pub const ALL: [AudioSide; 2] = [AudioSide::Capture, AudioSide::Playback];

    fn index(self) -> usize {
        match self {
            AudioSide::Capture => 0,
            AudioSide::Playback => 1,
        }
    }
}

impl fmt::Display for AudioSide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AudioSide::Capture => "capture",
            AudioSide::Playback => "playback",
        })
    }
}

/// One surfaced audio condition: which side, what happened, and since when.
#[derive(Clone, Debug)]
pub struct AudioCondition {
    pub side: AudioSide,
    pub kind: &'static str,
    pub message: String,
    pub since: Instant,
}

impl fmt::Display for AudioCondition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.side, self.message)
    }
}

/// Everything the audio path has counted, and the one condition it is currently in.
///
/// Shared by `Arc` between the two threads and whatever is surfacing them. The counters are
/// atomics rather than a lock because the title reads them four times a second and neither thread
/// may ever wait on a reader.
#[derive(Debug, Default)]
pub struct AudioStats {
    periods_captured: AtomicU64,
    /// Real captured periods written to the sink. Silence is not one of them, and neither is a
    /// muted period — see [`AudioSnapshot::periods_silent`] and [`AudioSnapshot::periods_muted`].
    periods_played: AtomicU64,
    periods_silent: AtomicU64,
    periods_muted: AtomicU64,
    pushed: AtomicU64,
    popped: AtomicU64,
    overruns: AtomicU64,
    underruns: AtomicU64,
    drift_drops: AtomicU64,
    drift_inserts: AtomicU64,
    capture_opens: AtomicU64,
    playback_opens: AtomicU64,
    /// Count of newly reported conditions, excluding repeated identical failures.
    conditions_logged: AtomicU64,
    /// How many times a side went from "in a condition" back to working, which is how
    /// many "recovered" lines were logged. A test can assert there was no recovery where the
    /// log would only show one.
    recoveries_logged: AtomicU64,
    /// One slot per side, not one between them. Both sides can be failing at once — a dongle
    /// that was unplugged and a `default` whose daemon restarted are one event on the recorded test setup — and
    /// a single slot makes each side's retry overwrite the other's condition, so every retry
    /// looks new, every retry logs, and `conditions_logged` counts retries rather than
    /// conditions. Keyed by `AudioSide::index`.
    conditions: Mutex<[Option<AudioCondition>; 2]>,
    /// Start time of each side's current open, cleared when its first period moves.
    /// Snapshot readers use this to expose a worker stuck in open or first I/O.
    opening: Mutex<[Option<Instant>; 2]>,
    /// How long a side may stay in [`AudioStats::opening`] before it is surfaced as a condition,
    /// in milliseconds; `0` disables the supervision.
    ///
    /// [`AudioHandle::spawn`] sets it to [`AudioConfig::reopen_backoff_cap`] — the longest this
    /// module ever waits for anything, so an open still outstanding after it is by this module's
    /// own standard stuck. It is a number rather than a `Duration` only so that [`AudioStats`]
    /// can keep its derived `Default`.
    opening_limit_ms: AtomicU64,
}

impl AudioStats {
    /// Record a condition on `side`. Returns `true` if it is new for that side — which is the
    /// caller's cue to log it, and the only time it ever does.
    ///
    /// "New" is by kind and message, against that side's own entry. A card that is absent stays
    /// absent across hundreds of retries and produces the same two values every time, so it is
    /// logged once and then counted in silence; a card that goes from absent to busy is a
    /// different condition and says so. The other side's condition is never consulted and never
    /// disturbed.
    pub fn report(&self, side: AudioSide, error: &AudioError) -> bool {
        let mut held = self.conditions.lock().unwrap_or_else(|e| e.into_inner());
        let slot = &mut held[side.index()];
        let message = error.to_string();
        let same = slot
            .as_ref()
            .is_some_and(|c| c.kind == error.kind() && c.message == message);
        if same {
            return false;
        }
        *slot = Some(AudioCondition {
            side,
            kind: error.kind(),
            message,
            since: Instant::now(),
        });
        self.conditions_logged.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Clear one side's condition because it is working again. Returns `true` if there was one to
    /// clear, so recovery is logged once too.
    ///
    /// Called after the first period that moved, never after a successful open. An
    /// open that succeeds and then fails on its first read is not a recovery: on a card that does
    /// that every cycle — which is what a half-dead USB device looks like — treating the open as
    /// the recovery produces a "recovered" line and a warning twice a second for the rest of the
    /// session.
    pub fn recovered(&self, side: AudioSide) -> bool {
        let mut held = self.conditions.lock().unwrap_or_else(|e| e.into_inner());
        if held[side.index()].take().is_some() {
            self.recoveries_logged.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        false
    }

    /// The condition to show, if any: capture's if both sides have one, per
    /// [`AudioSide::ALL`], so the title is a function of the state and not of which thread
    /// retried last.
    pub fn condition(&self) -> Option<AudioCondition> {
        let held = self.conditions.lock().unwrap_or_else(|e| e.into_inner());
        AudioSide::ALL
            .iter()
            .find_map(|side| held[side.index()].clone())
    }

    /// One side's condition, whatever the other side is doing.
    pub fn condition_of(&self, side: AudioSide) -> Option<AudioCondition> {
        self.conditions.lock().unwrap_or_else(|e| e.into_inner())[side.index()].clone()
    }

    /// How long a side may stay [`AudioStats::opening`] before it becomes a condition.
    ///
    /// Set once by [`AudioHandle::spawn`]. Zero — the default — disables the supervision, which
    /// is what a bare `AudioStats::default()` in a unit test wants.
    pub fn set_opening_limit(&self, limit: Duration) {
        self.opening_limit_ms.store(
            limit.as_millis().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
    }

    /// Mark that `side` is about to call into its opener.
    ///
    /// Called on the thread, immediately before the call that can block for ever. That is the
    /// point of it: the thread that is stuck cannot report itself, so it leaves a timestamp
    /// behind first and a *reader* decides when the silence has gone on too long.
    pub fn opening(&self, side: AudioSide) {
        let mut held = self.opening.lock().unwrap_or_else(|e| e.into_inner());
        held[side.index()] = Some(Instant::now());
    }

    /// Mark that `side` is no longer opening — it moved a period, or it failed and has a
    /// condition of its own to show. Idempotent.
    pub fn opened(&self, side: AudioSide) {
        let mut held = self.opening.lock().unwrap_or_else(|e| e.into_inner());
        held[side.index()] = None;
    }

    /// Which sides are currently between "about to open" and "moved a period", keyed by
    /// `AudioSide::index`.
    pub fn opening_sides(&self) -> [bool; 2] {
        let held = self.opening.lock().unwrap_or_else(|e| e.into_inner());
        [held[0].is_some(), held[1].is_some()]
    }

    /// Turn an open that has not come back within the limit into an ordinary condition.
    ///
    /// Called from [`AudioStats::snapshot`], which is to say from whoever is surfacing audio —
    /// the title four times a second, or the stats line. That is deliberate: the only thread that
    /// knows an open is outstanding is the one blocked inside it.
    ///
    /// The message names the limit and never the elapsed time, so it is the same string on
    /// every call and [`AudioStats::report`]'s dedup logs it once.
    fn supervise_opening(&self) {
        let limit = self.opening_limit_ms.load(Ordering::Relaxed);
        if limit == 0 {
            return;
        }
        let limit = Duration::from_millis(limit);
        // The guard is dropped before `report`, which takes the other lock: two locks, never
        // held at once, in one order only.
        let overdue: Vec<AudioSide> = {
            let held = self.opening.lock().unwrap_or_else(|e| e.into_inner());
            AudioSide::ALL
                .into_iter()
                .filter(|side| held[side.index()].is_some_and(|since| since.elapsed() >= limit))
                .collect()
        };
        for side in overdue {
            let error = AudioError::Open {
                device: side.to_string(),
                why: format!(
                    "the open has not returned in {limit:?}. It is inside a device call that has \
                     not come back; nothing is waiting on it and video and input are unaffected"
                ),
            };
            if self.report(side, &error) {
                log::warn!("audio: {error}");
            }
        }
    }

    /// A consistent-enough copy of the counters for one title or one stats line.
    pub fn snapshot(&self) -> AudioSnapshot {
        self.supervise_opening();
        AudioSnapshot {
            periods_captured: self.periods_captured.load(Ordering::Relaxed),
            periods_played: self.periods_played.load(Ordering::Relaxed),
            periods_silent: self.periods_silent.load(Ordering::Relaxed),
            periods_muted: self.periods_muted.load(Ordering::Relaxed),
            counts: RingCounts {
                pushed: self.pushed.load(Ordering::Relaxed),
                popped: self.popped.load(Ordering::Relaxed),
                overruns: self.overruns.load(Ordering::Relaxed),
                underruns: self.underruns.load(Ordering::Relaxed),
                drift_drops: self.drift_drops.load(Ordering::Relaxed),
                drift_inserts: self.drift_inserts.load(Ordering::Relaxed),
            },
            capture_opens: self.capture_opens.load(Ordering::Relaxed),
            playback_opens: self.playback_opens.load(Ordering::Relaxed),
            conditions_logged: self.conditions_logged.load(Ordering::Relaxed),
            recoveries_logged: self.recoveries_logged.load(Ordering::Relaxed),
            condition: self.condition(),
            opening: self.opening_sides(),
        }
    }

    /// Publish ring counters monotonically. Both workers snapshot under the ring lock
    /// then publish outside it; `fetch_max` prevents reversed publication order from
    /// moving a counter backward.
    fn absorb(&self, counts: RingCounts) {
        for (slot, value) in [
            (&self.pushed, counts.pushed),
            (&self.popped, counts.popped),
            (&self.overruns, counts.overruns),
            (&self.underruns, counts.underruns),
            (&self.drift_drops, counts.drift_drops),
            (&self.drift_inserts, counts.drift_inserts),
        ] {
            slot.fetch_max(value, Ordering::Relaxed);
        }
    }
}

/// A copy of [`AudioStats`] taken at one moment.
#[derive(Clone, Debug)]
pub struct AudioSnapshot {
    pub periods_captured: u64,
    /// Real captured periods written to the sink.
    pub periods_played: u64,
    /// Periods of silence written because the ring had nothing to give (an underrun or a drift
    /// insert). Counted apart from [`AudioSnapshot::periods_played`] because it is the opposite
    /// of audio: a session that "played" 30 000 periods of which 29 000 were silence is a broken
    /// one, and a single counter says it went perfectly.
    pub periods_silent: u64,
    /// Periods written as silence because the output was muted. Not a failure, and not audio
    /// either, so it is neither of the two above.
    pub periods_muted: u64,
    pub counts: RingCounts,
    pub capture_opens: u64,
    pub playback_opens: u64,
    pub conditions_logged: u64,
    pub recoveries_logged: u64,
    pub condition: Option<AudioCondition>,
    /// Which sides are between "about to open" and "moved their first period", keyed by
    /// `AudioSide::index`. See [`AudioSnapshot::opening_side`].
    pub opening: [bool; 2],
}

impl AudioSnapshot {
    /// The side to say is opening, if any — capture first, for the same reason
    /// [`AudioStats::condition`] prefers it: a surface that alternated between the two four times
    /// a second would be unreadable.
    ///
    /// A side that is *both* opening and in a condition shows the condition: the condition is the
    /// more specific statement, and past the supervision limit it is what the opening itself
    /// turned into.
    pub fn opening_side(&self) -> Option<AudioSide> {
        if self.condition.is_some() {
            return None;
        }
        AudioSide::ALL
            .into_iter()
            .find(|side| self.opening[side.index()])
    }
}

/// What the audio threads need to know.
#[derive(Clone, Copy, Debug)]
pub struct AudioConfig {
    pub ring: RingConfig,
    /// Periods in the ALSA device buffer on each side. Two is the minimum ALSA accepts; four
    /// leaves the kernel a period of slack either side of a scheduling hiccup without adding
    /// meaningfully to the depth.
    pub device_periods: usize,
    /// How long to wait before retrying an open that failed. Long enough that an absent dongle
    /// does not spin, short enough that a replug is picked up while the user is still watching
    /// (the same order as the capture pipeline's reopen backoff).
    pub reopen_backoff: Duration,
    /// The longest [`reopen_delay`] will grow to while a device stays *absent*.
    ///
    /// A dongle that is not plugged in is not coming back this second, and the resolution behind
    /// each retry reads `/sys` — cheap, but not free, and repeated for ever at a fixed 500 ms it
    /// is a periodic background task for a device nobody has. Doubling up to this cap keeps a
    /// replug noticed within ten seconds at worst while an overnight session with no dongle costs
    /// nothing.
    pub reopen_backoff_cap: Duration,
    /// How long [`AudioHandle::stop`] waits for a thread before detaching it.
    ///
    /// A bound, not an expectation: both threads normally stop within one slice of
    /// `sleep_until_stopped`. It exists because the thing on the other side of a device call is
    /// a driver, and `main.rs` drops audio on every shutdown path — including the one after
    /// the release-all, which must not be delayed by a wedged sound card.
    pub stop_deadline: Duration,
    /// Periods to accumulate before playback begins, on every open.
    ///
    /// Must exceed [`AudioConfig::device_periods`], whose initial writes do not block.
    /// The default six-period prefill leaves two periods after the four-period device
    /// buffer fills, preventing startup from being miscounted as clock drift.
    pub prefill_periods: usize,
}

impl Default for AudioConfig {
    fn default() -> Self {
        let ring = RingConfig::default();
        AudioConfig {
            ring,
            device_periods: DEVICE_PERIODS,
            reopen_backoff: Duration::from_millis(500),
            reopen_backoff_cap: Duration::from_secs(10),
            stop_deadline: Duration::from_millis(250),
            // Two periods deeper than the device buffer this prefill drains into, so two periods
            // are left in the ring when the sink starts pacing — off the low-water mark, with
            // room to move in either direction. See the field's doc for the measurement.
            prefill_periods: (DEVICE_PERIODS + 2).min(ring.periods.saturating_sub(1)),
        }
    }
}

/// Periods in the ALSA device buffer on each side, for [`AudioConfig::default`].
///
/// Named rather than inline because [`AudioConfig::prefill_periods`] is derived from it: the two
/// are a pair, and a change to one that is not a change to the other reintroduces the startup
/// drift inserts that doc records.
const DEVICE_PERIODS: usize = 4;

/// How long to wait before the next open attempt, after `absences` consecutive absent opens.
///
/// Pure, so the schedule is a thing with a test rather than an arithmetic expression buried in a
/// retry loop. `base` for the first retry, doubling from there, clamped at `cap`; `absences` is
/// reset by the next open that succeeds, so a device that comes and goes is always retried
/// promptly the first time.
///
/// Only *absence* escalates — [`AudioError::NoCard`] and [`AudioError::Gone`]. A busy card is
/// somebody else's recording that will end, and an open that failed on its parameters is about to
/// fail the same way again; neither is a device that is not there, and neither should push the
/// retry out to ten seconds.
pub fn reopen_delay(base: Duration, cap: Duration, absences: u32) -> Duration {
    let doublings = absences.saturating_sub(1).min(31);
    base.saturating_mul(1u32 << doublings).min(cap)
}

/// Whether this failure means "the device is not there", which is what [`reopen_delay`] backs off
/// on.
fn is_absence(error: &AudioError) -> bool {
    matches!(error, AudioError::NoCard(_) | AudioError::Gone { .. })
}

/// The ring, plus the one condvar the playback thread waits on.
///
/// The lock is held only for the push or the pop — never across a device call — so a stalled
/// sound card on one side cannot block the other.
#[derive(Debug)]
struct SharedRing {
    ring: Mutex<PeriodRing>,
    filled: Condvar,
    closed: AtomicBool,
}

impl SharedRing {
    fn new(config: RingConfig) -> Self {
        SharedRing {
            ring: Mutex::new(PeriodRing::new(config)),
            filled: Condvar::new(),
            closed: AtomicBool::new(false),
        }
    }

    fn push(&self, period: Vec<i16>) -> (PushOutcome, RingCounts) {
        let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        let outcome = ring.push(period);
        let counts = ring.counts();
        drop(ring);
        self.filled.notify_one();
        (outcome, counts)
    }

    /// The next period, waiting up to `timeout` for one to arrive.
    ///
    /// Returns `None` only when the ring has been closed — the shutdown path. Everything else is
    /// a period: an empty ring after the timeout is [`PopOutcome::Underran`] and silence, because
    /// the playback device must be fed on time or it underruns itself.
    fn pop(&self, timeout: Duration) -> Option<(Vec<i16>, PopOutcome, RingCounts)> {
        let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        if ring.is_empty() && !self.closed.load(Ordering::SeqCst) {
            let (guard, _) = self
                .filled
                .wait_timeout_while(ring, timeout, |r| {
                    r.is_empty() && !self.closed.load(Ordering::SeqCst)
                })
                .unwrap_or_else(|e| e.into_inner());
            ring = guard;
        }
        if self.closed.load(Ordering::SeqCst) && ring.is_empty() {
            return None;
        }
        let (period, outcome) = ring.pop();
        let counts = ring.counts();
        Some((period, outcome, counts))
    }

    /// Wait for `periods` to accumulate, or for `deadline` to pass, or for a shutdown.
    ///
    /// Returns the level it gave up at, which the caller logs when it is short: a prefill that
    /// timed out means the capture side is not producing, and that is worth seeing next to the
    /// silence the user is about to hear.
    fn prime(&self, periods: usize, deadline: Duration) -> usize {
        let ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        let (ring, _) = self
            .filled
            .wait_timeout_while(ring, deadline, |r| {
                r.len() < periods && !self.closed.load(Ordering::SeqCst)
            })
            .unwrap_or_else(|e| e.into_inner());
        ring.len()
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.filled.notify_all();
    }
}

/// The running audio path: two threads, a ring and the counters.
///
/// Dropping it stops both threads; [`AudioHandle::stop`] does the same and waits for them, which
/// is what the viewer's shutdown wants so the log lines land before the process exits.
pub struct AudioHandle {
    config: AudioConfig,
    stats: Arc<AudioStats>,
    muted: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    shared: Arc<SharedRing>,
    /// Named, because a thread that will not stop is reported by name and then detached.
    threads: Vec<(&'static str, JoinHandle<()>)>,
}

impl AudioHandle {
    /// Start capture and playback workers without opening devices on the caller's thread.
    /// Each worker reports its own opening, failure, and recovery state.
    pub fn spawn(
        mut source: Box<dyn PcmSourceOpener>,
        mut sink: Box<dyn PcmSinkOpener>,
        config: AudioConfig,
    ) -> AudioHandle {
        let stats = Arc::new(AudioStats::default());
        // A blocked opener cannot report its own delay. Use the maximum retry delay
        // as the supervision limit.
        stats.set_opening_limit(config.reopen_backoff_cap);
        let muted = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(SharedRing::new(config.ring));

        // The devices these openers produce are handed the same flag, so a blocking device call
        // has something to check: `PcmSourceOpener::on_stop_flag`'s docs say why that is the only
        // way a bounded shutdown can be honest rather than a detach.
        source.on_stop_flag(Arc::clone(&stop));
        sink.on_stop_flag(Arc::clone(&stop));

        // A failed audio thread start must not prevent the other side or viewer from running.
        let threads = vec![
            spawn_named(
                "nanokvm-audio-capture",
                capture_loop(
                    source,
                    Arc::clone(&shared),
                    Arc::clone(&stats),
                    Arc::clone(&stop),
                    config,
                ),
            ),
            spawn_named(
                "nanokvm-audio-play",
                playback_loop(
                    sink,
                    Arc::clone(&shared),
                    Arc::clone(&stats),
                    Arc::clone(&stop),
                    Arc::clone(&muted),
                    config,
                ),
            ),
        ];

        AudioHandle {
            config,
            stats,
            muted,
            stop,
            shared,
            threads: threads.into_iter().flatten().collect(),
        }
    }

    pub fn config(&self) -> AudioConfig {
        self.config
    }

    pub fn stats(&self) -> Arc<AudioStats> {
        Arc::clone(&self.stats)
    }

    pub fn snapshot(&self) -> AudioSnapshot {
        self.stats.snapshot()
    }

    /// Shared mute flag, applied by playback at the next period.
    pub fn mute_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.muted)
    }

    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    /// Stop audio and wait at most [`AudioConfig::stop_deadline`]. Idempotent.
    ///
    /// Close wakes ring waiters. Workers normally stop within a polling slice; a driver
    /// call that exceeds the deadline is detached with a warning.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.shared.close();
        let deadline = Instant::now() + self.config.stop_deadline;
        for (name, handle) in self.threads.drain(..) {
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(2));
            }
            if !handle.is_finished() {
                log::warn!(
                    "audio: the {name} thread did not stop in {:?}; detaching. It is inside a \
                     device call that has not returned; nothing else is waiting on it",
                    self.config.stop_deadline
                );
                continue; // dropping the handle detaches the thread.
            }
            if handle.join().is_err() {
                log::error!("the {name} thread panicked");
            }
        }
    }
}

impl Drop for AudioHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

impl fmt::Debug for AudioHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AudioHandle")
            .field("config", &self.config)
            .field("muted", &self.muted())
            .field("threads", &self.threads.len())
            .finish()
    }
}

fn spawn_named(
    name: &'static str,
    body: impl FnOnce() + Send + 'static,
) -> Option<(&'static str, JoinHandle<()>)> {
    match std::thread::Builder::new()
        .name(name.to_string())
        .spawn(body)
    {
        Ok(handle) => Some((name, handle)),
        // Report thread-start failure locally so audio cannot abort the viewer.
        Err(e) => {
            log::warn!("audio: cannot spawn {name} ({e}); continuing without audio");
            None
        }
    }
}

/// The capture side: open, read periods into the ring, and on any failure report once and reopen.
fn capture_loop(
    mut opener: Box<dyn PcmSourceOpener>,
    shared: Arc<SharedRing>,
    stats: Arc<AudioStats>,
    stop: Arc<AtomicBool>,
    config: AudioConfig,
) -> impl FnOnce() + Send + 'static {
    move || {
        let samples = config.ring.period_samples();
        let mut absences = 0u32;
        while !stop.load(Ordering::SeqCst) {
            // Record the start before entering a blocking open; clear it when a period moves.
            stats.opening(AudioSide::Capture);
            let mut source = match opener.open() {
                Ok(source) => {
                    stats.capture_opens.fetch_add(1, Ordering::Relaxed);
                    absences = 0;
                    log::info!("audio: capturing from {}", source.describe());
                    source
                }
                Err(e) => {
                    // Not opening any more: this side has a condition of its own to show, and
                    // leaving the mark set would have the supervisor raise a second one beside it.
                    stats.opened(AudioSide::Capture);
                    if is_absence(&e) {
                        absences = absences.saturating_add(1);
                    }
                    let delay =
                        reopen_delay(config.reopen_backoff, config.reopen_backoff_cap, absences);
                    if stats.report(AudioSide::Capture, &e) {
                        log::warn!(
                            "audio: {e}. Video and input are unaffected; retrying, the next \
                             attempt in {delay:?}"
                        );
                    }
                    sleep_until_stopped(&stop, delay);
                    continue;
                }
            };

            // The open is not the recovery: a card that opens and then fails its first read is a
            // half-dead device, and calling the open a recovery would log one twice a second for
            // the rest of the session. The first period that arrives is the recovery
            // (`AudioStats::recovered`).
            let mut working = false;
            while !stop.load(Ordering::SeqCst) {
                let mut period = vec![0i16; samples];
                match source.read_period(&mut period) {
                    Ok(()) => {
                        if !working {
                            working = true;
                            stats.opened(AudioSide::Capture);
                            if stats.recovered(AudioSide::Capture) {
                                log::info!("audio: capture recovered on {}", source.describe());
                            }
                        }
                        stats.periods_captured.fetch_add(1, Ordering::Relaxed);
                        let (_, counts) = shared.push(period);
                        stats.absorb(counts);
                    }
                    Err(e) => {
                        // A device call that failed *because* we are stopping is the shutdown
                        // path, not a condition: reporting it would put a warning in the log of
                        // every clean exit.
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                        stats.opened(AudioSide::Capture);
                        if stats.report(AudioSide::Capture, &e) {
                            log::warn!("audio: {e}. Video and input are unaffected; reopening");
                        }
                        break;
                    }
                }
            }
            // Dropped here, before the backoff, so a card that came back is not held closed by a
            // handle to the one that went away.
            drop(source);
            if !stop.load(Ordering::SeqCst) {
                sleep_until_stopped(&stop, config.reopen_backoff);
            }
        }
        log::debug!("audio: capture thread finished");
    }
}

/// The playback side: open, write whatever the ring hands over, and on any failure report once
/// and reopen.
fn playback_loop(
    mut opener: Box<dyn PcmSinkOpener>,
    shared: Arc<SharedRing>,
    stats: Arc<AudioStats>,
    stop: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    config: AudioConfig,
) -> impl FnOnce() + Send + 'static {
    move || {
        // One period of patience before falling back to silence, not one ring depth. The
        // device's own buffer is `device_periods` periods (40 ms at the default), so a thread
        // that waited the ring's 80 ms for a period that is not coming would let the sound card
        // run dry first — an XRUN, which is a click and an ALSA state to recover from, in place
        // of a counted period of silence that costs nothing. Floored at 3 ms so a tiny test
        // configuration still waits rather than spins.
        let wait =
            (config.ring.depth() / config.ring.periods.max(1) as u32).max(Duration::from_millis(3));
        let mut absences = 0u32;
        while !stop.load(Ordering::SeqCst) {
            // Record opening before the driver call so a blocked open remains visible.
            stats.opening(AudioSide::Playback);
            let mut sink = match opener.open() {
                Ok(sink) => {
                    stats.playback_opens.fetch_add(1, Ordering::Relaxed);
                    absences = 0;
                    log::info!("audio: playing to {}", sink.describe());
                    sink
                }
                Err(e) => {
                    stats.opened(AudioSide::Playback);
                    if is_absence(&e) {
                        absences = absences.saturating_add(1);
                    }
                    let delay =
                        reopen_delay(config.reopen_backoff, config.reopen_backoff_cap, absences);
                    if stats.report(AudioSide::Playback, &e) {
                        log::warn!(
                            "audio: {e}. Video and input are unaffected; retrying, the next \
                             attempt in {delay:?}"
                        );
                    }
                    sleep_until_stopped(&stop, delay);
                    continue;
                }
            };

            // Prime before the first write of this session, so the drift policy has a cushion to
            // measure movement against rather than a level pinned at zero (see
            // `AudioConfig::prefill_periods`). Bounded, because a card that produces nothing must
            // not wedge the playback thread — it must write silence and let the condition show.
            if config.prefill_periods > 0 {
                let deadline = config.ring.depth() * 4;
                let level = shared.prime(config.prefill_periods, deadline);
                if level < config.prefill_periods {
                    log::debug!(
                        "audio: playback primed with {level} of {} periods within {deadline:?}; \
                         starting anyway",
                        config.prefill_periods
                    );
                }
            }

            let mut working = false;
            while !stop.load(Ordering::SeqCst) {
                let Some((period, outcome, counts)) = shared.pop(wait) else {
                    break; // closed: shutdown.
                };
                stats.absorb(counts);
                // Mute silences the *output* and keeps the ring flowing, so unmuting resumes on
                // live audio rather than on a backlog, and an overrun is not an artefact of
                // having been muted.
                let is_muted = muted.load(Ordering::Relaxed);
                let written = if is_muted {
                    sink.write_period(&config.ring.silence())
                } else {
                    sink.write_period(&period)
                };
                match written {
                    Ok(()) => {
                        // Three counters, because they are three different things to a listener:
                        // audio, a gap where audio should have been, and a mute they asked for.
                        let counter = match (is_muted, outcome) {
                            (true, _) => &stats.periods_muted,
                            (false, PopOutcome::Period) => &stats.periods_played,
                            (false, _) => &stats.periods_silent,
                        };
                        counter.fetch_add(1, Ordering::Relaxed);
                        if !working {
                            working = true;
                            stats.opened(AudioSide::Playback);
                            if stats.recovered(AudioSide::Playback) {
                                log::info!("audio: playback recovered on {}", sink.describe());
                            }
                        }
                    }
                    Err(e) => {
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                        stats.opened(AudioSide::Playback);
                        if stats.report(AudioSide::Playback, &e) {
                            log::warn!("audio: {e}. Video and input are unaffected; reopening");
                        }
                        break;
                    }
                }
            }
            drop(sink);
            if !stop.load(Ordering::SeqCst) {
                sleep_until_stopped(&stop, config.reopen_backoff);
            }
        }
        log::debug!("audio: playback thread finished");
    }
}

/// Sleep, but wake for a shutdown. Checked in slices so `stop()` never waits a whole backoff.
fn sleep_until_stopped(stop: &AtomicBool, total: Duration) {
    const SLICE: Duration = Duration::from_millis(20);
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(SLICE.min(deadline.saturating_duration_since(Instant::now())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_failure_is_a_condition_once_however_often_it_happens() {
        let stats = AudioStats::default();
        let e = AudioError::Busy {
            device: "hw:8".to_string(),
            why: "Device or resource busy".to_string(),
        };
        assert!(stats.report(AudioSide::Capture, &e));
        for _ in 0..100 {
            assert!(!stats.report(AudioSide::Capture, &e));
        }
        assert_eq!(stats.snapshot().conditions_logged, 1);
    }

    #[test]
    fn a_different_failure_is_a_different_condition() {
        let stats = AudioStats::default();
        let busy = AudioError::Busy {
            device: "hw:8".to_string(),
            why: "busy".to_string(),
        };
        let gone = AudioError::Gone {
            device: "hw:8".to_string(),
            why: "No such device".to_string(),
        };
        assert!(stats.report(AudioSide::Capture, &busy));
        assert!(stats.report(AudioSide::Capture, &gone));
        assert!(!stats.report(AudioSide::Capture, &gone));
        assert_eq!(stats.snapshot().conditions_logged, 2);
        assert_eq!(stats.condition().map(|c| c.kind), Some("gone"));
    }

    /// Two sides failing at once are two conditions, each logged once. One slot between them
    /// makes every retry look new: the count then measures retries, and the title alternates
    /// between the two at the rate the threads happen to loop at.
    #[test]
    fn each_side_keeps_its_own_condition_and_neither_displaces_the_other() {
        let stats = AudioStats::default();
        let capture = AudioError::NoCard("the dongle is not plugged in".to_string());
        let playback = AudioError::Gone {
            device: "default".to_string(),
            why: "the daemon went away".to_string(),
        };
        for _ in 0..50 {
            stats.report(AudioSide::Capture, &capture);
            stats.report(AudioSide::Playback, &playback);
        }
        assert_eq!(stats.snapshot().conditions_logged, 2);
        assert_eq!(
            stats.condition_of(AudioSide::Playback).map(|c| c.kind),
            Some("gone")
        );
        // The aggregate is capture's while both are set, whichever reported last.
        assert_eq!(stats.condition().map(|c| c.side), Some(AudioSide::Capture));
        assert!(stats.recovered(AudioSide::Capture));
        assert_eq!(stats.condition().map(|c| c.side), Some(AudioSide::Playback));
        assert_eq!(stats.snapshot().recoveries_logged, 1);
    }

    /// Out-of-order counter publication must remain monotonic.
    #[test]
    fn interleaved_publications_never_walk_a_counter_backwards() {
        let stats = Arc::new(AudioStats::default());
        let stop = Arc::new(AtomicBool::new(false));
        let writers: Vec<_> = [1u64, 2]
            .into_iter()
            .map(|step| {
                let stats = Arc::clone(&stats);
                std::thread::spawn(move || {
                    for n in (0..2_000).step_by(step as usize) {
                        stats.absorb(RingCounts {
                            pushed: n,
                            popped: n,
                            overruns: n,
                            underruns: n,
                            drift_drops: n,
                            drift_inserts: n,
                        });
                    }
                })
            })
            .collect();

        let reader = {
            let stats = Arc::clone(&stats);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut last = RingCounts::default();
                while !stop.load(Ordering::Relaxed) {
                    let now = stats.snapshot().counts;
                    for (before, after) in [
                        (last.pushed, now.pushed),
                        (last.popped, now.popped),
                        (last.overruns, now.overruns),
                        (last.underruns, now.underruns),
                        (last.drift_drops, now.drift_drops),
                        (last.drift_inserts, now.drift_inserts),
                    ] {
                        assert!(after >= before, "a counter went from {before} to {after}");
                    }
                    last = now;
                }
            })
        };

        for w in writers {
            w.join().expect("a writer panicked");
        }
        stop.store(true, Ordering::Relaxed);
        reader
            .join()
            .expect("the reader saw a counter go backwards");
    }

    /// Absent-device retries grow to the configured cap.
    #[test]
    fn the_retry_schedule_doubles_while_a_device_stays_absent_and_stops_at_the_cap() {
        let base = Duration::from_millis(500);
        let cap = Duration::from_secs(10);
        let delays: Vec<Duration> = (0..8).map(|n| reopen_delay(base, cap, n)).collect();
        assert_eq!(
            delays,
            vec![
                Duration::from_millis(500), // nothing absent yet
                Duration::from_millis(500),
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                cap,
                cap,
            ]
        );
        // However long it has been absent, and whatever the arithmetic would overflow into.
        assert_eq!(reopen_delay(base, cap, u32::MAX), cap);
    }

    /// Only absence escalates. A busy card is somebody else's recording that will end, and an
    /// open that failed on its parameters will fail the same way again — neither is a device that
    /// is not there, and backing either off to ten seconds would make the client slow to notice a
    /// card it could have had.
    #[test]
    fn only_an_absent_device_backs_off() {
        assert!(is_absence(&AudioError::NoCard("no card".to_string())));
        assert!(is_absence(&AudioError::Gone {
            device: "hw:8".to_string(),
            why: "No such device".to_string(),
        }));
        for present in [
            AudioError::Busy {
                device: "hw:8".to_string(),
                why: "busy".to_string(),
            },
            AudioError::Open {
                device: "default".to_string(),
                why: "no such pcm".to_string(),
            },
            AudioError::Io {
                device: "default".to_string(),
                why: "write failed".to_string(),
            },
        ] {
            assert!(!is_absence(&present), "{present}");
        }
    }

    #[test]
    fn one_sides_recovery_does_not_clear_the_others_condition() {
        let stats = AudioStats::default();
        let e = AudioError::Gone {
            device: "default".to_string(),
            why: "the daemon went away".to_string(),
        };
        assert!(stats.report(AudioSide::Playback, &e));
        assert!(!stats.recovered(AudioSide::Capture));
        assert!(stats.condition().is_some());
        assert!(stats.recovered(AudioSide::Playback));
        assert!(stats.condition().is_none());
    }

    #[test]
    fn every_error_names_the_device_it_is_about() {
        for e in [
            AudioError::Busy {
                device: "hw:8".to_string(),
                why: "busy".to_string(),
            },
            AudioError::Gone {
                device: "hw:8".to_string(),
                why: "gone".to_string(),
            },
            AudioError::Open {
                device: "default".to_string(),
                why: "no such pcm".to_string(),
            },
            AudioError::Io {
                device: "default".to_string(),
                why: "write failed".to_string(),
            },
        ] {
            let text = e.to_string();
            assert!(text.contains(e.device()), "{text} must name {}", e.device());
            assert!(text.len() > e.device().len(), "{text} must say why");
        }
    }

    /// Measured 2026-09-11 on the recorded test setup: with `prefill_periods == device_periods` the prefill is
    /// consumed entirely by the sound card's own buffer, the ring is left empty on the low-water
    /// mark, and the drift policy inserts periods of silence for a clock difference that has not
    /// happened — two of them in the first second, every run. The relation, not the two numbers,
    /// is what keeps that from coming back.
    #[test]
    fn the_prefill_is_deeper_than_the_device_buffer_it_drains_into() {
        let config = AudioConfig::default();
        assert!(
            config.prefill_periods > config.device_periods,
            "a prefill of {} cannot survive a device buffer of {}: the first {} writes do not \
             block, so the ring is drained to {} and sits on the low-water mark",
            config.prefill_periods,
            config.device_periods,
            config.device_periods,
            config.prefill_periods.saturating_sub(config.device_periods),
        );
        assert!(
            config.prefill_periods < config.ring.periods,
            "a prefill of {} cannot be reached in a ring of {}: playback would wait out its whole \
             prime deadline on every open",
            config.prefill_periods,
            config.ring.periods,
        );
    }
}
