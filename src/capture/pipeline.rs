//! The capture and decode threads (§4.1, §5.2).
//!
//! §4.1's table, the two rows this module owns:
//!
//! | Thread | Owns | Handoff out |
//! | --- | --- | --- |
//! | Capture | V4L2 dequeue, copy out, requeue | one pending owned frame |
//! | Decode | MJPEG → RGBA, one thread (§1.3) | one pending decoded frame |
//!
//! Render is somebody else's thread; it takes from [`PipelineHandle::output`] and never queues
//! or drains (§5.2). Both handoffs are [`Slot`]s, so each carries at most one pending frame and
//! a new frame displaces an unconsumed old one rather than queueing behind it. Displacement is
//! counted, separately for each stage, because §5.5 asks for "frames dropped pre-decode" and
//! "frames dropped post-decode" as distinct numbers — one means the decoder is behind, the
//! other means the renderer is.
//!
//! **The capture thread survives everything, disconnection included.** §6.1 S1-2 is
//! architectural: frames stopping must neither crash nor hang, and a stall is not evidence of
//! signal loss (A5). [`CaptureError::Timeout`] and [`CaptureError::Io`] are counted and the loop
//! continues. A frame that fails to decode is counted and skipped; the decode thread never exits
//! over one.
//!
//! **Stage 2 adds the two recoveries §6.1's table asks for, and keeps them distinct.**
//!
//! | Condition | Evidence | What this module does |
//! | --- | --- | --- |
//! | Device disconnected | `ENODEV`, `POLLHUP`/`POLLNVAL`, or a panicking `v4l` destructor | drop the source, then reopen through the [`SourceOpener`] on an exponential backoff (S2-4) |
//! | Capture stall | no frame for [`PipelineConfig::restart_after`], with or without an error | [`FrameSource::restart`] on the same device, repeated while the stall lasts (S2-1) |
//! | Frames in the wrong mode | the SOF dimensions disagree with what `S_FMT` negotiated, for [`PipelineConfig::format_mismatch_frames`] consecutive frames *and* [`PipelineConfig::format_mismatch_grace`] | [`FrameSource::restart`] — a fresh `STREAMON` re-commits the format — at most [`PipelineConfig::format_mismatch_restart_limit`] times, then **one** reopen through the [`SourceOpener`] with a fresh budget, then the SOF dimensions are accepted |
//!
//! **The third row is a measured failure, not a hypothetical.** On 2026-09-11 a live viewer run
//! through a real USB reset reopened the node correctly — rediscovered, `S_FMT` MJPG 1920x1080,
//! `S_PARM` 60 fps — and then streamed 640x480 for the remaining two minutes of the run, with the
//! V4L2 `ERROR` flag on the first buffer as the only tell. The dongle powers up in 640x480 and
//! the reopen's UVC probe/commit was lost on it; `G_FMT` went on reporting 1920x1080 because that
//! is the driver's opinion, not the device's. The *stream* was stuck, not the device: a fresh
//! open by `v4l2-ctl` immediately afterwards got 1920x1080 from frame one. So the response is a
//! `restart()` — `STREAMOFF`, re-prime, `STREAMON`, which re-commits the format — and the
//! watchdog that calls it is bounded, because A6 keeps the SOF header as the authority on the
//! size and a unit that does *not* rescale internally (this one does — C4, §6 item 3) would
//! legitimately deliver a different size forever and must not be restarted in a loop.
//!
//! **`restart()` first, and a reopen only if it does not work.** A restart is strictly less than
//! the reopen that had just failed — no `S_FMT`, no `S_PARM`, the same fd — so it is fair to ask
//! why the weaker remedy goes first. Because it is the one that was measured to work: the second
//! live run of 2026-09-11 (17:45:55–17:45:56) shows the reopen coming up at 640x480, the watchdog
//! restarting the stream after 516.7 ms, and the very next frame at 1920x1080. One restart, fixed.
//! The likely mechanism is timing rather than strength — the reopen lands about half a second
//! after re-enumeration, while the device is still settling, and a `STREAMON` half a second later
//! re-commits successfully — which is exactly why doing the *same* reopen again, later, is the
//! right escalation when the restarts do not take: once the restart budget is spent the watchdog
//! takes one full reopen through the [`SourceOpener`] (counted in
//! [`PipelineStats::format_mismatch_reopens`]), re-arms itself with one fresh restart budget, and
//! accepts only if that budget is spent too. The whole escalation runs at most once per run of
//! mismatched frames, so a device that never reaches the negotiated mode ends in acceptance and
//! never in a reopen loop.
//!
//! Four properties of the disconnect path are load-bearing rather than incidental:
//!
//! - **The decode thread and the output slot are never touched.** The compressed slot is *not*
//!   closed, so the renderer keeps presenting the last decoded image for the whole gap — §6.1
//!   S1-2's "the last image is preserved", now across a real unplug.
//! - **The source is dropped under `catch_unwind`**, before any reopen. The `v4l` destructors
//!   panic on a teardown ioctl that fails with anything but `ENODEV` (`capture::v4l2` module
//!   docs item 4), and a panic that escaped here would leave the mmap buffers mapped and the
//!   pipeline claiming `Running` forever.
//! - **The backoff is interruptible.** `stop()` during a five-second wait returns as promptly as
//!   `stop()` on a healthy pipeline; the wait polls the stop flag rather than sleeping through it.
//! - **The open itself is not waited on.** [`SourceOpener::open`] cannot be interrupted — for
//!   `V4l2Source` it is a chain of ioctls, each a USB control transfer to a device that has just
//!   re-enumerated, and the kernel's timeout on one of those is seconds — so it runs on a
//!   short-lived helper thread that the capture thread waits on but abandons on shutdown. See
//!   [`PipelineHandle::stop`] for the bound this buys and the residual it leaves.
//!
//! **A stall response that fails is still a stall.** [`FrameSource::restart`] is called under its
//! own `catch_unwind`, because `V4l2Source::restart` drops the old stream and those destructors
//! panic (v4l2 module docs item 4) — and a panic there means the *restart* failed, not that the
//! device is gone. A failed restart, panicking or not, is counted as a capture error and the loop
//! keeps polling the same source, which is what §6.1 S1-2 requires of a stall and what
//! `V4l2Source` is built for: it comes back from a failed rebuild marked broken, so the next
//! `next_frame` tries the rebuild again.
//!
//! [`Pipeline::start`] keeps its Stage 1 contract exactly — one source, no reopen, the thread
//! exits on disconnection — because a caller that handed over a device it opened itself has
//! given the pipeline nothing to reopen. [`Pipeline::start_with_opener`] is the recovering entry
//! point.
//!
//! **Surviving an error is not the same as tolerating it for free.** A source can fail
//! *instantly* and repeatedly — `capture::v4l2` returns `EIO` without waiting when the fd
//! reports `POLLERR`, and a `BadFrame` follows a perfectly prompt dequeue — so the error arm
//! backs off ([`BACKOFF`], escalating to [`BACKOFF_SLOW`] after
//! [`BACKOFF_ESCALATE_AFTER`] consecutive failures) instead of spinning a core. The backoff is
//! interruptible: it wakes on the stop flag, so shutdown stays as prompt as it was.
//!
//! **The capture thread's loop body runs under `catch_unwind`.** The `v4l` crate's `Stream` and
//! `Arena` destructors `panic!` on any teardown ioctl failure other than `ENODEV` (see
//! `capture::v4l2` module docs item 4), and they run on this thread — during a stream rebuild,
//! and when the source is finally dropped. §6.1 S1-1 says a capture failure must never tear the
//! rest of the client down, and a panicking thread whose payload nobody catches until `stop()`
//! would leave the pipeline claiming `Running` forever. A panic out of the *frame* path is
//! therefore recorded as [`PipelineState::Disconnected`] with the panic message, exactly like a
//! real `ENODEV` — the ring is in the state a failed dequeue leaves it in and only a fresh source
//! can be trusted. A panic out of the *restart* path is not: see above.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::v4l2::BytesUsedStats;
use super::{
    lock_or_recover, CaptureError, CompressedFrame, DecodedFrame, Decoder, FrameSource, Slot,
    SourceOpener,
};

/// How the pipeline is doing right now (§6.1: report only what is actually known).
///
/// There is deliberately no `NoSignal`: this hardware cannot report it and pixel content is
/// never evidence of it (A5, §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineState {
    /// Frames are arriving.
    Running,
    /// No frame has arrived for [`PipelineConfig::stall_after`]. The capture stalled — say that,
    /// and do not say the signal is gone (§6.1).
    Stalled,
    /// The device is gone — `ENODEV`, a `POLLHUP`/`POLLNVAL` on the node, or a panicking `v4l`
    /// destructor — and no reopen is in flight.
    ///
    /// For [`Pipeline::start`] this is terminal: the capture thread has exited. For
    /// [`Pipeline::start_with_opener`] it is the brief moment between noticing the loss and
    /// entering the retry loop — the source is being dropped, which on real hardware means
    /// `STREAMOFF`, `munmap` and `REQBUFS(0)`. It is genuinely observable, so it is a state
    /// rather than an implementation detail, but a test that wants "the device went away" should
    /// accept either this or [`PipelineState::Reconnecting`].
    Disconnected,
    /// Disconnected, and a reopen is being retried on the backoff (§6.1: "attempt rediscovery").
    ///
    /// Distinct from [`PipelineState::Disconnected`] because the two mean different things to a
    /// user: one says the device is gone, the other says the client is doing something about it.
    Reconnecting,
    /// [`PipelineHandle::stop`] was called, or the threads could not be started.
    Stopped,
}

/// How long the capture thread sleeps after a survivable error before trying again.
///
/// A source that fails instantly — `EIO` off a sticky `POLLERR`, or an unparseable frame after a
/// prompt dequeue — would otherwise be retried as fast as the CPU allows. 20 ms is longer than a
/// frame period at every advertised rate, so it costs nothing when frames are flowing, and it
/// caps the retry rate at 50 Hz when they are not.
pub const BACKOFF: Duration = Duration::from_millis(20);

/// The backoff after [`BACKOFF_ESCALATE_AFTER`] consecutive errors: a failure that has not
/// cleared in a fifth of a second is not going to clear in the next 20 ms either.
pub const BACKOFF_SLOW: Duration = Duration::from_millis(100);

/// Consecutive survivable errors after which the backoff escalates to [`BACKOFF_SLOW`].
pub const BACKOFF_ESCALATE_AFTER: u32 = 10;

/// The granularity at which every wait on the capture thread notices [`PipelineHandle::stop`].
///
/// Every such wait is a poll on the stop flag rather than one long sleep, so `stop()` is bounded
/// by this and not by whatever the wait was for. 2 ms is 2,500 wake-ups across the 5 s reopen cap
/// and costs nothing measurable; it is well inside the "≤ 50 ms" the design asks for.
const STOP_POLL: Duration = Duration::from_millis(2);

/// Pipeline knobs.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// How long without a frame before [`PipelineHandle::state`] reports
    /// [`PipelineState::Stalled`]. At 60 fps the frame period is 16.7 ms, so the default of
    /// 500 ms is 30 missed frames — long enough not to flap, short enough to be useful.
    pub stall_after: Duration,
    /// How long the decode thread waits on the compressed slot before rechecking for shutdown.
    /// A `close()` wakes it immediately; this only bounds the pathological case.
    pub decode_wait: Duration,
    /// **Test hook.** Sleep this long before each decode, to make the decoder deliberately
    /// slower than the source and force §5.2's drop-before-decode path. `None` in production.
    pub decode_delay: Option<Duration>,
    /// How long without a frame before [`FrameSource::restart`] is called, and then called again
    /// for every further interval the stall lasts. `None` disables restarts entirely (§6.1:
    /// "attempt restart" is a *response* to a stall, not a diagnosis of one).
    ///
    /// The default of 2 s is four times [`PipelineConfig::stall_after`]: reporting a stall is
    /// cheap and should be prompt, while restarting a stream costs a `STREAMOFF`/`STREAMON`
    /// round trip and throws away whatever the driver had queued, so it should only happen once
    /// the stall has outlived any plausible hiccup. It stays cheap when the stall is long, since
    /// it only re-runs a handful of ioctls every 2 s while it lasts.
    ///
    /// The 2 s was to be settled by the target-reboot measurement. It was not: measured on
    /// 2026-09-11, a full reboot of the target **produces no stall at all** — the dongle keeps
    /// emitting 60 fps of a no-signal picture throughout, so this path is never entered
    /// (`docs/STAGE2_FINDINGS.md` C12 and §6 item 2). 2 s therefore remains an argued value for
    /// an event this unit has not yet produced, not a measured one.
    pub restart_after: Option<Duration>,
    /// The first wait before a reopen is attempted after a disconnection, doubling up to
    /// [`PipelineConfig::reopen_max_backoff`].
    ///
    /// 500 ms because that is the shape of the event: a USB re-enumeration takes on the order of
    /// a second or two, so retrying faster only burns `open` calls on a node that is not there,
    /// and retrying slower adds dead time to a recovery the user is watching.
    pub reopen_backoff: Duration,
    /// The cap the reopen backoff doubles up to. 5 s keeps a device that is gone for an hour to
    /// twelve `open` attempts a minute while still recovering within one interval of it
    /// returning.
    pub reopen_max_backoff: Duration,
    /// How long frames may keep arriving at dimensions other than the negotiated ones before
    /// [`FrameSource::restart`] is called to re-commit the format. `None` disables the watchdog.
    ///
    /// **This is one of two gates and both must be passed**; the other is
    /// [`PipelineConfig::format_mismatch_frames`]. The watchdog fires only once that many
    /// *consecutive* mismatched frames have arrived **and** this much time has elapsed since the
    /// first of them.
    ///
    /// **The default is 500 ms.** The failure it exists for is unbounded: on 2026-09-11 a
    /// session stayed at 640x480 for two minutes after a USB reset and would have stayed there
    /// forever. What this gate contributes is patience with a device that is settling — the same
    /// fresh open showed 640x480 at sequence 1 and 1920x1080 by sequence 4 — so the watchdog does
    /// not spend a restart the instant a burst of mismatched frames lands.
    ///
    /// **What the clock is, exactly.** It is wall clock from the first mismatched frame of the
    /// current run, so a gap in which no frame arrives at all does count towards it. That is
    /// deliberate rather than overlooked: the frame gate is what a stall cannot pass — a stalled
    /// stream produces no frames, so it cannot produce
    /// [`PipelineConfig::format_mismatch_frames`] mismatched ones — so time alone can no longer
    /// buy a restart, and the clock does not need to be stopped to make that true. Both are
    /// reset by a frame at the negotiated size, and by every watchdog restart; a *stall* restart
    /// ([`PipelineConfig::restart_after`]) deliberately leaves them alone, so an intermittent
    /// stream stuck in the wrong mode cannot have its evidence wiped out over and over.
    pub format_mismatch_grace: Option<Duration>,
    /// How many *consecutive* mismatched frames must have arrived before the format watchdog may
    /// fire, alongside [`PipelineConfig::format_mismatch_grace`]'s elapsed time. Both, not
    /// either.
    ///
    /// **The default is 12, and the load-bearing part is that it is greater than eight.** A6
    /// bounds the benign transient at "up to eight frames at the previous resolution after an
    /// idle period" — a bound in *frames*, which does not convert into a time without also
    /// fixing the frame rate. A time-only grace of 500 ms sits three times over that transient
    /// at the 60 fps this client negotiates and *under* it at 10 fps, where eight benign frames
    /// take 800 ms; the watchdog would then restart a stream that was about to fix itself, at
    /// the worst possible moment, since the transient happens precisely when a stream has just
    /// come back. A count above A6's bound makes the transient un-triggerable at any frame rate,
    /// which is what "a stream that is merely slow is not restarted for being slow" actually
    /// requires.
    pub format_mismatch_frames: u32,
    /// How many times the format watchdog may restart the stream before it accepts the SOF
    /// dimensions as the truth, per device open.
    ///
    /// **The default is 3.** A restart costs a `STREAMOFF`/`STREAMON` round trip (8.7 ms
    /// measured, ~73 ms to the next frame — C4) and a lost frame or five, so three of them
    /// spread over a doubling backoff is cheap; and the measured failure was fixed by *one*
    /// fresh `STREAMON`, so three is two more chances than the event needs. The bound is the
    /// load-bearing part rather than the number: A6 makes the frame's own header the authority
    /// on its size, so a unit that does not rescale internally would legitimately deliver a
    /// different size forever, and an unbounded watchdog would restart its stream every grace
    /// period for as long as the client ran. When the budget is spent the watchdog escalates
    /// **once** to a full reopen ([`PipelineStats::format_mismatch_reopens`]) and gets one fresh
    /// budget; when that one is spent too, the mismatch is accepted, logged once at warn, and
    /// counted in [`PipelineStats::format_mismatch_accepted`].
    ///
    /// The budget is refilled by **a frame at the negotiated size**, by a reopen, and by that one
    /// escalation, and by nothing else — in particular not by a restart, which would let the
    /// watchdog refill its own budget and defeat the bound.
    pub format_mismatch_restart_limit: u32,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        PipelineConfig {
            stall_after: Duration::from_millis(500),
            decode_wait: Duration::from_millis(100),
            decode_delay: None,
            restart_after: Some(Duration::from_secs(2)),
            reopen_backoff: Duration::from_millis(500),
            reopen_max_backoff: Duration::from_secs(5),
            format_mismatch_grace: Some(Duration::from_millis(500)),
            format_mismatch_frames: 12,
            format_mismatch_restart_limit: 3,
        }
    }
}

/// A snapshot of the pipeline's counters (§5.5: "FPS alone hides exactly the failures §5.2 and
/// §5.4 exist to prevent").
#[derive(Debug, Clone)]
pub struct PipelineStats {
    /// Frames dequeued and copied out.
    pub frames_captured: u64,
    /// Frames decoded to RGBA.
    pub frames_decoded: u64,
    /// Compressed frames displaced before the decoder got to them: the decoder is behind (§5.2
    /// — "decoding an already-obsolete frame spends the most expensive step on output nobody
    /// will see").
    pub dropped_pre_decode: u64,
    /// Decoded frames displaced before the renderer took them: the renderer is behind.
    pub dropped_post_decode: u64,
    /// Frames that failed to decode. Strict mode makes a truncated frame an error rather than a
    /// partial image (§1.3), so this counts frames deliberately not shown.
    pub decode_errors: u64,
    /// Survivable capture errors: timeouts and I/O errors. Not disconnections.
    pub capture_errors: u64,
    /// The most recent error from either thread, for display.
    ///
    /// **Cleared when the pipeline reopens the device**, and only then: a fresh source has none
    /// of the old one's history, and every other field in the snapshot — `state`,
    /// `disconnected_since` — says the failure is over, so leaving the message here would make
    /// one snapshot contradict itself for the rest of the process's life.
    ///
    /// It is deliberately **not** cleared by a frame arriving, which is the other thing that
    /// could be called a recovery. A stream can deliver frames and errors at the same time — a
    /// device that emits one unparseable frame in five is the shape `capture_pipeline`'s
    /// oversized-header test pins — and a field that the next good frame wipes would hide exactly
    /// that. So: "the last thing that went wrong, until the device was reopened", never "what is
    /// wrong right now". A consumer that wants the latter reads `state` and the counters.
    pub last_error: Option<String>,
    /// Host-clock instant at which the last frame was captured, for staleness.
    pub last_frame_at: Option<Instant>,
    /// The last frame's V4L2 timestamp on `CLOCK_MONOTONIC` (§5.5). Comparable with
    /// [`super::v4l2::now_monotonic`] to get a capture-to-dequeue age — **not** a latency.
    pub last_captured_at: Option<Duration>,
    /// Compressed frame sizes seen (§1.3: instrument `bytesused` in Stage 1).
    pub bytes: BytesUsedStats,
    /// The last frame's V4L2 buffer sequence counter. Advisory (A6 says it carries no reliable
    /// tell about a resolution change); it is here so an instrument can print *which* frame a
    /// dimension change was first seen on.
    pub last_sequence: Option<u32>,
    /// Times the device was lost: `ENODEV`, a `POLLHUP`/`POLLNVAL`, or a panic on the capture
    /// thread. Counted once per loss, not once per failed reopen.
    pub disconnects: u64,
    /// Times a [`SourceOpener`] handed back a working source after a loss (§6.1 S2-4).
    pub reopens: u64,
    /// [`SourceOpener::open`] calls made *while recovering*. The initial open is not one: there
    /// was nothing to get back. `reopen_attempts - reopens` is how many times the node was not
    /// ready yet, which is the number that says whether the backoff is tuned sanely.
    pub reopen_attempts: u64,
    /// [`FrameSource::restart`] calls the stall timer made (§6.1 "attempt restart"). Counted on
    /// the attempt, because a restart that failed still happened and still cost the stream.
    pub stream_restarts: u64,
    /// Frames whose SOF dimensions differed from the previous frame's (§6, A6). The first frame
    /// is not a change. This is the local half of S2-3's measurement: the *device's* opinion of
    /// the format is `V4l2Source::query_negotiated_format`, and the two need not agree.
    pub resolution_changes: u64,
    /// The last frame's SOF dimensions, or `None` before the first frame.
    pub last_resolution: Option<(u32, u32)>,
    /// [`FrameSource::restart`] calls the **format watchdog** made: frames kept arriving at
    /// dimensions other than the negotiated ones for [`PipelineConfig::format_mismatch_grace`],
    /// so the stream was restarted to re-commit the format. Counted separately from
    /// [`PipelineStats::stream_restarts`] — they go through the same `restart()` call but they
    /// answer different questions ("no frames at all" against "frames in the wrong mode").
    pub format_mismatch_restarts: u64,
    /// Times the watchdog escalated a spent restart budget to a **full reopen** of the device:
    /// release the source, open it again through the [`SourceOpener`], `S_FMT`/`S_PARM`/
    /// `STREAMON` from scratch. At most one per run of mismatched frames, so this is bounded by
    /// the same thing [`PipelineStats::format_mismatch_accepted`] is. Counted apart from
    /// [`PipelineStats::reopens`] — which it also increments, since it really is a reopen — so
    /// that "the device went away" and "the watchdog gave up on restarts" stay distinguishable.
    pub format_mismatch_reopens: u64,
    /// Times the watchdog spent its restart budget, spent the fresh one the single escalation
    /// reopen bought it, and accepted the SOF dimensions as the truth (A6). Nonzero means the
    /// negotiated format could not be established on this device and what is on screen is what
    /// the device is actually sending.
    pub format_mismatch_accepted: u64,
    /// What the current source's `S_FMT` committed to, or `None` when there is no source or it
    /// negotiated nothing. The other half of the comparison
    /// [`PipelineStats::last_resolution`] is one half of: the two differing while
    /// `format_mismatch_accepted` is nonzero is the state the viewer has to put in the title
    /// (C5 — a condition is surfaced, not counted).
    pub negotiated_dimensions: Option<(u32, u32)>,
    /// When the current disconnection started, or `None` when connected. Cleared the instant a
    /// reopen succeeds, so `state == Running` always comes with `None`.
    pub disconnected_since: Option<Instant>,
    /// The state at the moment of the snapshot.
    pub state: PipelineState,
}

/// Everything both threads and the handle share.
struct Shared {
    inner: Mutex<Inner>,
    started_at: Instant,
    stopping: AtomicBool,
    stall_after: Duration,
}

struct Inner {
    frames_captured: u64,
    frames_decoded: u64,
    dropped_pre_decode: u64,
    dropped_post_decode: u64,
    decode_errors: u64,
    capture_errors: u64,
    last_error: Option<String>,
    last_frame_at: Option<Instant>,
    last_captured_at: Option<Duration>,
    bytes: BytesUsedStats,
    /// The same `bytesused` fold over a window the caller resets, for a per-interval min/max
    /// that a cumulative `min`/`max` cannot give. See [`PipelineHandle::take_bytes_window`].
    bytes_window: BytesUsedStats,
    last_sequence: Option<u32>,
    disconnects: u64,
    reopens: u64,
    reopen_attempts: u64,
    stream_restarts: u64,
    resolution_changes: u64,
    last_resolution: Option<(u32, u32)>,
    format_mismatch_restarts: u64,
    format_mismatch_reopens: u64,
    format_mismatch_accepted: u64,
    negotiated_dimensions: Option<(u32, u32)>,
    disconnected_since: Option<Instant>,
    state: PipelineState,
}

impl Shared {
    fn new(stall_after: Duration) -> Self {
        Shared {
            inner: Mutex::new(Inner {
                frames_captured: 0,
                frames_decoded: 0,
                dropped_pre_decode: 0,
                dropped_post_decode: 0,
                decode_errors: 0,
                capture_errors: 0,
                last_error: None,
                last_frame_at: None,
                last_captured_at: None,
                bytes: BytesUsedStats::default(),
                bytes_window: BytesUsedStats::default(),
                last_sequence: None,
                disconnects: 0,
                reopens: 0,
                reopen_attempts: 0,
                stream_restarts: 0,
                resolution_changes: 0,
                last_resolution: None,
                format_mismatch_restarts: 0,
                format_mismatch_reopens: 0,
                format_mismatch_accepted: 0,
                negotiated_dimensions: None,
                disconnected_since: None,
                state: PipelineState::Running,
            }),
            started_at: Instant::now(),
            stopping: AtomicBool::new(false),
            stall_after,
        }
    }

    /// The state, with staleness folded in. `Stalled` is derived rather than stored so it
    /// becomes true simply by time passing, with no thread having to notice.
    fn state(&self) -> PipelineState {
        let inner = lock_or_recover(&self.inner);
        self.state_of(&inner)
    }

    /// The same derivation against a guard the caller already holds.
    ///
    /// [`PipelineHandle::stats`] needs the state and the counters to describe **one** moment. Two
    /// separate lock acquisitions let a frame land between them, which is how a snapshot ends up
    /// saying `Stalled` next to a `last_frame_at` of "just now".
    fn state_of(&self, inner: &Inner) -> PipelineState {
        match inner.state {
            PipelineState::Running => {
                let since = inner.last_frame_at.unwrap_or(self.started_at);
                if since.elapsed() >= self.stall_after {
                    PipelineState::Stalled
                } else {
                    PipelineState::Running
                }
            }
            other => other,
        }
    }
}

/// The capture pipeline (§4.1). A namespace for [`Pipeline::start`]; the running pipeline is
/// owned by the [`PipelineHandle`] it returns.
pub struct Pipeline;

impl Pipeline {
    /// Start the capture and decode threads and return a handle to them.
    ///
    /// The threads are named `nanokvm-capture` and `nanokvm-decode`, so a stack dump or a
    /// `perf` profile says which is which.
    ///
    /// Infallible by construction: if a thread cannot be spawned the handle comes back with
    /// [`PipelineState::Stopped`] and the reason in [`PipelineStats::last_error`], rather than
    /// panicking inside a library. Callers that care check [`PipelineHandle::state`].
    /// Start the capture and decode threads on one source, with no recovery — the Stage 1
    /// entry point, unchanged.
    ///
    /// The source is wrapped in an opener that yields it once and has nothing after that, and
    /// recovery is off, so a disconnection is terminal exactly as it was in Stage 1: the state
    /// becomes [`PipelineState::Disconnected`], the capture thread exits, and
    /// [`PipelineStats::reopen_attempts`] stays at 0 — nothing was ever attempted, because a
    /// caller who handed over a device it opened itself has given us nothing to reopen. Callers
    /// that want §6.1's rediscovery use [`Pipeline::start_with_opener`].
    pub fn start(source: Box<dyn FrameSource>, config: PipelineConfig) -> PipelineHandle {
        let describe = source.describe();
        let opener = OnceOpener {
            source: Some(source),
            describe: describe.clone(),
        };
        Pipeline::spawn(Box::new(opener), describe, false, config)
    }

    /// Start the pipeline against an opener, with the §6.1 recovery behaviours on
    /// (S2-1, S2-2, S2-4).
    ///
    /// The opener is called once now — on the capture thread, so a device that is not there yet
    /// costs the caller nothing — and again on every reopen after a disconnection. A failure of
    /// that first call is not fatal: it puts the pipeline straight into
    /// [`PipelineState::Reconnecting`], which is the honest state for "the node is not there
    /// yet" and is what makes starting the client before plugging the dongle in work.
    ///
    /// [`PipelineHandle::describe`] reports [`SourceOpener::describe`], because at the moment
    /// this returns there may be no source to ask.
    pub fn start_with_opener(
        opener: Box<dyn SourceOpener>,
        config: PipelineConfig,
    ) -> PipelineHandle {
        let describe = opener.describe();
        Pipeline::spawn(opener, describe, true, config)
    }

    /// The shared body of both entry points. `recover` is the only difference between them.
    ///
    /// Infallible by construction: if a thread cannot be spawned the handle comes back with
    /// [`PipelineState::Stopped`] and the reason in [`PipelineStats::last_error`], rather than
    /// panicking inside a library. Callers that care check [`PipelineHandle::state`].
    ///
    /// The threads are named `nanokvm-capture` and `nanokvm-decode`, so a stack dump or a
    /// `perf` profile says which is which.
    fn spawn(
        opener: Box<dyn SourceOpener>,
        describe: String,
        recover: bool,
        config: PipelineConfig,
    ) -> PipelineHandle {
        let compressed: Arc<Slot<CompressedFrame>> = Arc::new(Slot::new());
        let output: Arc<Slot<DecodedFrame>> = Arc::new(Slot::new());
        let shared = Arc::new(Shared::new(config.stall_after));

        log::info!("capture pipeline starting on {describe}");

        let capture = {
            let compressed = Arc::clone(&compressed);
            let shared = Arc::clone(&shared);
            let capture_state = Capture::new(opener, recover, &config);
            std::thread::Builder::new()
                .name("nanokvm-capture".to_string())
                .spawn(move || capture_loop(capture_state, &compressed, &shared))
        };

        let decode = {
            let compressed = Arc::clone(&compressed);
            let output = Arc::clone(&output);
            let shared = Arc::clone(&shared);
            let config = config.clone();
            std::thread::Builder::new()
                .name("nanokvm-decode".to_string())
                .spawn(move || decode_loop(&compressed, &output, &shared, &config))
        };

        let (capture, decode) = match (capture, decode) {
            (Ok(c), Ok(d)) => (Some(c), Some(d)),
            (c, d) => {
                // One or both failed to spawn. Shut down whatever did start and report it.
                let mut msg = String::from("failed to start pipeline threads");
                if let Err(e) = &c {
                    msg = format!("failed to spawn nanokvm-capture: {e}");
                }
                if let Err(e) = &d {
                    msg = format!("failed to spawn nanokvm-decode: {e}");
                }
                log::error!("{msg}");
                {
                    let mut inner = lock_or_recover(&shared.inner);
                    inner.state = PipelineState::Stopped;
                    inner.last_error = Some(msg);
                }
                shared.stopping.store(true, Ordering::SeqCst);
                compressed.close();
                output.close();
                (c.ok(), d.ok())
            }
        };

        PipelineHandle {
            output,
            compressed,
            shared,
            capture,
            decode,
            describe,
        }
    }
}

/// The [`SourceOpener`] behind [`Pipeline::start`]: one source, then nothing.
///
/// Its `open` after the first is `Disconnected` — truthfully, since it has no device to give —
/// but `Pipeline::start` also runs with recovery off, so the pipeline never asks a second time.
/// The wrapper exists so both entry points run *one* capture loop rather than two that drift.
struct OnceOpener {
    source: Option<Box<dyn FrameSource>>,
    describe: String,
}

impl SourceOpener for OnceOpener {
    fn open(&mut self) -> Result<Box<dyn FrameSource>, CaptureError> {
        self.source.take().ok_or(CaptureError::Disconnected)
    }

    fn describe(&self) -> String {
        self.describe.clone()
    }
}

/// A cheap identity for a [`CaptureError`], so a repeat of the same failure can be recognised
/// without formatting it.
///
/// `BadFrame` and `Config` carry a message that can differ between occurrences; they are treated
/// as one kind each, so `last_error` keeps the first message of a run rather than being rewritten
/// tens of times a second. The counter still moves on every error, which is the number §5.5 asks
/// for; only the display string is left alone.
fn error_kind(e: &CaptureError) -> (u8, i32) {
    match e {
        CaptureError::Disconnected => (0, 0),
        CaptureError::Timeout(_) => (1, 0),
        CaptureError::BadFrame(_) => (2, 0),
        CaptureError::Io(io) => (3, io.raw_os_error().unwrap_or(0)),
        CaptureError::Config(_) => (4, 0),
    }
}

/// Sleep for `nap`, waking early if the pipeline is shutting down.
///
/// The backoff must not make `stop()` any slower than it was, so it is a poll on the stop flag
/// rather than one long sleep.
fn backoff(shared: &Arc<Shared>, compressed: &Arc<Slot<CompressedFrame>>, nap: Duration) {
    let deadline = Instant::now() + nap;
    while Instant::now() < deadline {
        if shared.stopping.load(Ordering::SeqCst) || compressed.is_closed() {
            return;
        }
        std::thread::sleep(STOP_POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// Everything the capture thread owns across an iteration (§4.1 row 1, §6.1 Stage 2).
///
/// A struct rather than a pile of `&mut` parameters because the recovery paths need most of it:
/// the opener outlives every source, the stall timer has to survive a reopen, and the error
/// bookkeeping has to be reset by one.
struct Capture {
    /// Behind an `Arc<Mutex<_>>` because the open runs on a helper thread the capture thread
    /// abandons on shutdown — see [`Capture::open_detached`]. Never contended: at most one open
    /// is ever in flight, and the capture thread does not start another after abandoning one.
    opener: Arc<Mutex<Box<dyn SourceOpener>>>,
    /// `None` between losing a device and opening the next one.
    source: Option<Box<dyn FrameSource>>,
    /// Whether a disconnection is recoverable. False for [`Pipeline::start`] — see its docs.
    recover: bool,
    restart_after: Option<Duration>,
    reopen_backoff: Duration,
    reopen_max_backoff: Duration,
    /// The format watchdog's grace period, or `None` when it is off.
    format_grace: Option<Duration>,
    /// How many consecutive mismatched frames the watchdog needs before it may fire, alongside
    /// [`Capture::format_grace`]. Both gates, not either.
    format_min_frames: u32,
    /// How many watchdog restarts this device open is allowed.
    format_restart_limit: u32,
    /// What the current source negotiated, read once per device open. `None` for a source that
    /// negotiated nothing — every synthetic and scripted one — which leaves the watchdog inert.
    negotiated: Option<(u32, u32)>,
    /// When the current unbroken run of mismatched frames started. Cleared by a frame at the
    /// negotiated size, by a *watchdog* restart, and by a reopen — and deliberately not by a
    /// stall restart.
    mismatch_since: Option<Instant>,
    /// How many frames that run is, which is the gate a stall cannot pass. Cleared with
    /// [`Capture::mismatch_since`] and by the same three things.
    mismatch_frames: u32,
    /// Watchdog restarts spent since the budget was last refilled.
    format_restarts: u32,
    /// Whether this run of mismatched frames has already had its one escalation reopen. Survives
    /// that reopen — which is what bounds the escalation to one per run — and is cleared by a
    /// frame at the negotiated size or by a reopen the watchdog did not ask for.
    format_reopened: bool,
    /// Set between asking for the escalation reopen and the re-arm that follows it, so
    /// [`Capture::arm_format_watch`] can tell that reopen apart from a disconnection's.
    escalating: bool,
    /// Set when both budgets are spent: the SOF dimensions are the truth from here (A6) and the
    /// watchdog is silent until a negotiated-size frame or a reopen re-arms it.
    format_accepted: bool,
    consecutive_errors: u32,
    last_kind: Option<(u8, i32)>,
    /// What the stall timer measures from: the last frame, the last restart, or the last reopen.
    /// Not `last_frame_at`, because after a reopen there may be no frame yet and the timer must
    /// still start from *something* — otherwise a device that has been away for a minute is
    /// restarted the instant it comes back.
    restart_base: Instant,
}

impl Capture {
    fn new(opener: Box<dyn SourceOpener>, recover: bool, config: &PipelineConfig) -> Self {
        Capture {
            opener: Arc::new(Mutex::new(opener)),
            source: None,
            recover,
            restart_after: config.restart_after,
            reopen_backoff: config.reopen_backoff,
            reopen_max_backoff: config.reopen_max_backoff,
            format_grace: config.format_mismatch_grace,
            format_min_frames: config.format_mismatch_frames,
            format_restart_limit: config.format_mismatch_restart_limit,
            negotiated: None,
            mismatch_since: None,
            mismatch_frames: 0,
            format_restarts: 0,
            format_reopened: false,
            escalating: false,
            format_accepted: false,
            consecutive_errors: 0,
            last_kind: None,
            restart_base: Instant::now(),
        }
    }

    /// Drop the current source, absorbing a panicking destructor.
    ///
    /// `capture::v4l2` module docs item 4: `Stream::drop` issues `STREAMOFF` and `Arena::drop`
    /// `munmap`s and `REQBUFS(0)`s, and both `panic!` on any failure other than `ENODEV`. On the
    /// disconnect path the ioctls are being issued against a node that has just gone away, which
    /// is the one case they tolerate — but "tolerates the case we expect" is not a guarantee, and
    /// a panic escaping here would skip the reopen entirely. Absorb it and say so; the mappings
    /// are released by the same destructors either way, which is what H-B2 checks in
    /// `/proc/self/maps`.
    fn release_source(&mut self) {
        let Some(source) = self.source.take() else {
            return;
        };
        if let Err(payload) = catch_unwind(AssertUnwindSafe(move || drop(source))) {
            log::error!(
                "capture: the source's destructor panicked while releasing it: {}",
                panic_message(&payload)
            );
        }
    }

    /// Record the loss of the device (§6.1 disconnect row) and release the source.
    fn lose_device(&mut self, msg: &str, shared: &Arc<Shared>) {
        {
            let mut inner = lock_or_recover(&shared.inner);
            inner.state = PipelineState::Disconnected;
            inner.last_error = Some(msg.to_string());
            inner.disconnects += 1;
            inner.disconnected_since = Some(Instant::now());
            // There is no source, so there is nothing that negotiated anything. Leaving the old
            // value here would let the title go on comparing a frame size against a mode that
            // belongs to a device this pipeline no longer holds.
            inner.negotiated_dimensions = None;
        }
        if self.recover {
            log::warn!("capture: {msg}; the pipeline will try to reopen the device");
        } else {
            log::error!("capture: {msg}; capture thread exiting");
        }
        // Before any reopen: the buffers must go back whatever the destructors think of it.
        self.release_source();
    }

    /// Call [`SourceOpener::open`] somewhere the capture thread can walk away from it.
    ///
    /// An open cannot be interrupted and it is not quick: `V4l2Source::open` is `S_FMT`, `S_PARM`,
    /// `REQBUFS`, four `mmap`s and `QBUF`s and a `STREAMON`, every one a USB control transfer to a
    /// device that has just re-enumerated, where the kernel's control timeout runs to seconds. A
    /// `stop()` that landed mid-open used to wait the whole of it out, because `stop()` joins this
    /// thread. So the open runs on a helper thread and the capture thread waits on a channel,
    /// polling the stop flag every [`STOP_POLL`]; on shutdown it drops the receiver and returns.
    ///
    /// The residual, documented on [`PipelineHandle::stop`]: the abandoned helper still finishes
    /// its open and then drops whatever it produced — on its own thread, under `catch_unwind`,
    /// because those destructors are the panicking kind — shortly after `stop()` has returned.
    /// The device is released there rather than at `stop()`. Bounding the wait instead would give
    /// the same promptness and *also* leave the open running, with no way to know when it ended.
    ///
    /// Returns `None` only when the pipeline is shutting down.
    fn open_detached(
        &mut self,
        compressed: &Arc<Slot<CompressedFrame>>,
        shared: &Arc<Shared>,
    ) -> Option<Result<Box<dyn FrameSource>, CaptureError>> {
        let opener = Arc::clone(&self.opener);
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("nanokvm-open".to_string())
            .spawn(move || {
                // `AssertUnwindSafe`: the only state observable after an unwind is the opener,
                // behind a mutex whose poisoning this module recovers from by policy.
                let opened =
                    match catch_unwind(AssertUnwindSafe(|| lock_or_recover(&opener).open())) {
                        Ok(result) => result,
                        Err(payload) => Err(CaptureError::Config(format!(
                            "opening the device panicked: {}",
                            panic_message(&payload)
                        ))),
                    };
                if let Err(unsent) = tx.send(opened) {
                    // The pipeline stopped while this open was in flight. Nobody else can own
                    // what came back, so it is dropped here — under `catch_unwind`, because a
                    // `V4l2Source`'s destructors panic on a failed teardown ioctl.
                    let _ = catch_unwind(AssertUnwindSafe(move || drop(unsent)));
                }
            });
        if let Err(e) = spawned {
            // No helper thread to be had. Open inline: a `stop()` that has to wait one open out
            // is worse than this pipeline's promise, and better than never opening the device.
            log::warn!("capture: could not spawn the open helper thread ({e}); opening inline");
            return Some(lock_or_recover(&self.opener).open());
        }
        loop {
            match rx.recv_timeout(STOP_POLL) {
                Ok(result) => return Some(result),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if shared.stopping.load(Ordering::SeqCst) || compressed.is_closed() {
                        return None;
                    }
                }
                // The helper cannot end without sending: the panic is caught above. Treat it as
                // a failed attempt rather than trusting the impossible.
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Some(Err(CaptureError::Config(
                        "the open helper thread ended without answering".to_string(),
                    )))
                }
            }
        }
    }

    /// The first open, on the capture thread. Not counted in `reopen_attempts`: nothing has been
    /// lost yet, so there is nothing to get back.
    ///
    /// Returns false only when the pipeline should stop outright, which is the non-recovering
    /// entry point failing to open at all, or a `stop()` that landed during the open.
    fn open_first(
        &mut self,
        compressed: &Arc<Slot<CompressedFrame>>,
        shared: &Arc<Shared>,
    ) -> bool {
        let Some(opened) = self.open_detached(compressed, shared) else {
            return false;
        };
        match opened {
            Ok(source) => {
                log::info!("capture: streaming from {}", source.describe());
                self.source = Some(source);
                self.restart_base = Instant::now();
                self.arm_format_watch(shared);
                true
            }
            Err(e) => {
                let msg = e.to_string();
                {
                    let mut inner = lock_or_recover(&shared.inner);
                    inner.last_error = Some(msg.clone());
                    inner.state = PipelineState::Disconnected;
                    inner.disconnected_since = Some(Instant::now());
                }
                if self.recover {
                    // Not a `disconnects` — the device was never connected. The retry loop takes
                    // it from here, which is what "start the client, then plug the dongle in"
                    // needs.
                    log::warn!("capture: could not open the device ({msg}); retrying");
                    true
                } else {
                    log::error!("capture: could not open the device: {msg}");
                    false
                }
            }
        }
    }

    /// Wait out the backoff and try the opener, until it works or the pipeline stops (§6.1
    /// "attempt rediscovery").
    ///
    /// The wait comes **first**: the device was lost microseconds ago and hammering `open` on a
    /// node mid-re-enumeration answers nothing. The backoff doubles to
    /// [`PipelineConfig::reopen_max_backoff`], and every step of it is interruptible, so `stop()`
    /// is as prompt here as anywhere else.
    ///
    /// Returns false if the pipeline is shutting down.
    fn reconnect(&mut self, compressed: &Arc<Slot<CompressedFrame>>, shared: &Arc<Shared>) -> bool {
        // The cap binds the *first* wait too. A config with `reopen_backoff` above
        // `reopen_max_backoff` is degenerate, but "the cap is the longest wait" should be true
        // without having to read the doubling to find out.
        let mut delay = self.reopen_backoff.min(self.reopen_max_backoff);
        loop {
            if shared.stopping.load(Ordering::SeqCst) || compressed.is_closed() {
                return false;
            }
            lock_or_recover(&shared.inner).state = PipelineState::Reconnecting;
            backoff(shared, compressed, delay);
            if shared.stopping.load(Ordering::SeqCst) || compressed.is_closed() {
                return false;
            }
            let attempt = {
                let mut inner = lock_or_recover(&shared.inner);
                inner.reopen_attempts += 1;
                inner.reopen_attempts
            };
            let Some(opened) = self.open_detached(compressed, shared) else {
                return false;
            };
            match opened {
                Ok(source) => {
                    let describe = source.describe();
                    self.source = Some(source);
                    // A fresh device has none of the old one's error history, and the stall timer
                    // restarts from now rather than from a frame that arrived before the unplug.
                    self.consecutive_errors = 0;
                    self.last_kind = None;
                    self.restart_base = Instant::now();
                    // The watchdog re-arms with the new source's negotiated format and a full
                    // budget: the reopen is exactly the event that produced the stuck stream this
                    // watchdog exists for.
                    self.arm_format_watch(shared);
                    let reopens = {
                        let mut inner = lock_or_recover(&shared.inner);
                        inner.reopens += 1;
                        // `Running` even though no frame has arrived yet: the derived `Stalled`
                        // takes over until one does, which is the honest report — the device is
                        // open and nothing has come out of it yet.
                        inner.state = PipelineState::Running;
                        inner.disconnected_since = None;
                        // The failure this snapshot was reporting is over, and every other field
                        // in the snapshot now says so. Leaving the disconnection message here
                        // would make `stats()` contradict itself for the rest of the process's
                        // life — see [`PipelineStats::last_error`].
                        inner.last_error = None;
                        inner.reopens
                    };
                    log::info!(
                        "capture: reopened after {attempt} attempt(s) (reopen {reopens}): {describe}"
                    );
                    return true;
                }
                Err(e) => {
                    // ENOENT/ENODEV/EBUSY are all "not yet" while a USB device re-enumerates.
                    let text = e.to_string();
                    log::debug!("capture: reopen attempt {attempt} failed: {text}");
                    lock_or_recover(&shared.inner).last_error = Some(text);
                    delay = delay.saturating_mul(2).min(self.reopen_max_backoff);
                }
            }
        }
    }

    /// The stall response (§6.1 "attempt restart"): stop and restart the stream in place.
    ///
    /// The timer is reset *before* the call, so a restart that itself takes a while does not
    /// immediately qualify for another one.
    ///
    /// **A restart that panics is a failed restart, not a lost device.** `V4l2Source::restart`
    /// drops the old stream, and those destructors `panic!` on any teardown ioctl that fails with
    /// something other than `ENODEV` (v4l2 module docs item 4) — which is exactly the state a
    /// genuinely wedged stream is in, and the state the stall timer exists to attack. Letting
    /// that panic reach the loop's `catch_unwind` would make it `Step::Lost`, and on
    /// [`Pipeline::start`] — the entry point `src/main.rs` uses, with `restart_after` on by
    /// default — a survivable stall would become a terminal disconnection with the capture thread
    /// exited, which is precisely what §6.1 S1-1/S1-2 forbid. So the call gets its own
    /// `catch_unwind` and lands where the non-panicking failure already landed: counted, logged,
    /// same source, keep polling. Keeping the source is right rather than merely convenient — a
    /// `V4l2Source` whose rebuild failed comes back marked broken, so its next `next_frame`
    /// retries the rebuild; and a source that really is gone says `Disconnected` on that call.
    fn restart_stream(&mut self, shared: &Arc<Shared>) -> Step {
        self.restart_base = Instant::now();
        let after = self.restart_after.unwrap_or_default();
        if self.source.is_none() {
            return Step::Continue;
        }
        let n = {
            let mut inner = lock_or_recover(&shared.inner);
            inner.stream_restarts += 1;
            inner.stream_restarts
        };
        log::info!("capture: no frame for {after:?}; restarting the stream (restart {n})");
        self.invoke_restart(shared)
    }

    /// Call [`FrameSource::restart`] on the current source and classify what came back.
    ///
    /// The one place either restart path — the stall timer's and the format watchdog's — touches
    /// the source, so both get the same `catch_unwind` and the same three outcomes. They are
    /// counted separately by their callers and handled identically here, which is the point: a
    /// panicking `restart()` is a failed restart and never a lost device, whichever timer asked
    /// for it (C4).
    fn invoke_restart(&mut self, shared: &Arc<Shared>) -> Step {
        // `AssertUnwindSafe`: what an unwind here can leave behind is the source, and the two
        // sources that can panic out of `restart` — `V4l2Source` and a test double — both leave
        // it in the state a failed rebuild leaves it in, which the next `next_frame` handles.
        let outcome = {
            let Some(source) = self.source.as_mut() else {
                return Step::Continue;
            };
            catch_unwind(AssertUnwindSafe(|| source.restart()))
        };
        match outcome {
            // Nothing about the watchdog is touched here, on purpose, and the two callers differ
            // on what they want:
            //
            // - **The negotiated mode is not re-read.** It used to be, "in case the source
            //   renegotiated", and on the only source that can renegotiate anything that was a
            //   no-op: `V4l2Source::format` is set once in `open()` and `restart()` is
            //   `REQBUFS`/`QBUF`/`STREAMON` on the same fd with no `S_FMT` and no `S_PARM`, so it
            //   cannot change. Re-reading it here would therefore have been code only a test
            //   double could exercise — the one shape where a fake can pass where hardware would
            //   not. The negotiated mode changes when the device is opened, and
            //   [`Capture::arm_format_watch`] is where that is picked up.
            // - **The mismatch clock and frame count are not cleared.** The *watchdog's* restart
            //   does clear them, at its own call site, because it wants fresh evidence for its
            //   next decision. The *stall* restart must not: a stream that is both intermittent
            //   and stuck in the wrong mode would otherwise have its evidence wiped out every
            //   time the stall timer fired, and the watchdog would never fire at all.
            Ok(Ok(())) => Step::Continue,
            // The restart discovered what the stall could not tell us apart from: the node is
            // gone. That is the disconnect path, not a failed restart.
            Ok(Err(CaptureError::Disconnected)) => {
                Step::Lost(CaptureError::Disconnected.to_string())
            }
            Ok(Err(e)) => self.failed_restart(format!("stream restart failed: {e}"), shared),
            Err(payload) => self.failed_restart(
                format!("stream restart panicked: {}", panic_message(&payload)),
                shared,
            ),
        }
    }

    /// Re-arm the format watchdog for a freshly opened device: new negotiated mode, clean clock,
    /// full restart budget.
    ///
    /// The one thing a reopen does **not** always clear is [`Capture::format_reopened`]. When the
    /// reopen is the watchdog's own escalation, that flag is what stops the next spent budget
    /// asking for another one: the escalation is bounded at one per run of mismatched frames, so
    /// a device that never reaches the negotiated mode ends in acceptance rather than in a reopen
    /// loop. A reopen the watchdog did not ask for — a real disconnection — is a new device open
    /// and clears it like everything else.
    fn arm_format_watch(&mut self, shared: &Arc<Shared>) {
        self.negotiated = self.source.as_ref().and_then(|s| s.negotiated_dimensions());
        lock_or_recover(&shared.inner).negotiated_dimensions = self.negotiated;
        self.mismatch_since = None;
        self.mismatch_frames = 0;
        self.format_restarts = 0;
        self.format_accepted = false;
        if !self.escalating {
            self.format_reopened = false;
        }
        self.escalating = false;
    }

    /// End the current run of mismatched frames: no clock, no count, a full budget, no
    /// acceptance, and the escalation available again. The only thing that calls this is a frame
    /// that arrived at the negotiated size.
    fn clear_format_run(&mut self) {
        self.mismatch_since = None;
        self.mismatch_frames = 0;
        self.format_restarts = 0;
        self.format_reopened = false;
        self.format_accepted = false;
    }

    /// The format watchdog, run on every delivered frame (§6/A6, C4).
    ///
    /// A6 stands: `dims` came from *this frame's* start-of-frame header and nothing here
    /// overrides it or rescales anything — the decoder and the renderer go on coping with
    /// whatever size arrives. What this asks is a different question: is the **device** in the
    /// mode `S_FMT` committed to? When it is not, and has not been for
    /// [`PipelineConfig::format_mismatch_frames`] consecutive frames *and*
    /// [`PipelineConfig::format_mismatch_grace`] of elapsed time, a `restart()` re-commits it.
    ///
    /// **Two gates, and both are needed.** The frame count is what a benign transient cannot
    /// pass — A6 bounds it at eight frames whatever the frame rate — and it is also what a
    /// *stall* cannot pass, since a stream delivering nothing delivers no mismatched frames
    /// either. The elapsed time is what keeps a device that is merely settling from being
    /// restarted the moment a burst lands.
    ///
    /// **The ladder, once both gates are passed.** `restart()` up to
    /// [`PipelineConfig::format_mismatch_restart_limit`] times on a doubling backoff, then one
    /// full reopen through the [`SourceOpener`] with a fresh restart budget, then acceptance. The
    /// weaker remedy goes first because it is the one that was measured to work (module docs);
    /// the reopen goes second because it is the same action that had already failed once, and the
    /// evidence says its failure was about *when* it happened rather than about what it does.
    /// Acceptance is checked **before** the doubled wait, because the doubling exists to space
    /// restarts out and at that point there is no restart left to space.
    ///
    /// Returns a [`Step`] because a restart can report the node gone — the disconnect path, not
    /// this one's — and because the escalation is itself a request to reopen the device.
    fn watch_format(&mut self, dims: (u32, u32), shared: &Arc<Shared>) -> Step {
        let (Some(grace), Some(negotiated)) = (self.format_grace, self.negotiated) else {
            return Step::Continue;
        };
        if dims == negotiated {
            // The device is in the mode that was asked for. The run of mismatched frames is over,
            // the budget is refilled and the escalation is available again: a frame at the
            // negotiated size is the only thing that does any of that.
            if self.format_accepted {
                log::info!(
                    "capture: frames are back at the negotiated {}x{}; the format watchdog is \
                     armed again",
                    negotiated.0,
                    negotiated.1
                );
            }
            self.clear_format_run();
            return Step::Continue;
        }
        if self.format_accepted {
            return Step::Continue;
        }
        self.mismatch_frames = self.mismatch_frames.saturating_add(1);
        let since = *self.mismatch_since.get_or_insert_with(Instant::now);
        let waited = since.elapsed();
        if self.mismatch_frames < self.format_min_frames || waited < grace {
            return Step::Continue;
        }
        if self.format_restarts >= self.format_restart_limit {
            // The restarts are spent. One escalation is left, and only if there is somewhere to
            // escalate *to*: a non-recovering pipeline (`Pipeline::start`) has a source and no
            // second one, so for it a spent budget is the end of the ladder.
            if self.recover && !self.format_reopened {
                self.format_reopened = true;
                self.escalating = true;
                let n = {
                    let mut inner = lock_or_recover(&shared.inner);
                    inner.format_mismatch_reopens += 1;
                    inner.format_mismatch_reopens
                };
                return Step::Reopen(format!(
                    "frames have been {}x{} through {} stream restart(s) but the negotiated \
                     format is {}x{}; reopening the device to commit it again (format-mismatch \
                     reopen {n})",
                    dims.0, dims.1, self.format_restarts, negotiated.0, negotiated.1
                ));
            }
            // Both budgets spent and the frames are still the wrong size. A6: the SOF header is
            // the authority, so this is now simply what the device sends — accept it, say so
            // once, and stop restarting a stream that may be perfectly correct for a unit that
            // does not rescale internally.
            self.format_accepted = true;
            let accepted = {
                let mut inner = lock_or_recover(&shared.inner);
                inner.format_mismatch_accepted += 1;
                inner.format_mismatch_accepted
            };
            log::warn!(
                "capture: the negotiated format {}x{} could not be established after {} stream \
                 restart(s){}; accepting the {}x{} the device is sending as the truth (A6) and \
                 leaving the stream alone (acceptance {accepted})",
                negotiated.0,
                negotiated.1,
                self.format_restarts,
                if self.format_reopened {
                    " and a reopen"
                } else {
                    ""
                },
                dims.0,
                dims.1
            );
            return Step::Continue;
        }
        // The same doubling shape the reopen path uses: every spent restart doubles the wait
        // before the next one, so a device that answers slowly is given more room each time
        // rather than the same room three times in a row. It gates restarts only — acceptance is
        // decided above, so the last restart is not followed by a doubled wait for nothing.
        let factor = 1u32.checked_shl(self.format_restarts).unwrap_or(u32::MAX);
        if waited < grace.saturating_mul(factor) {
            return Step::Continue;
        }
        let n = {
            let mut inner = lock_or_recover(&shared.inner);
            inner.format_mismatch_restarts += 1;
            inner.format_mismatch_restarts
        };
        log::warn!(
            "capture: frames have been {}x{} for {waited:?} ({} frames) but the negotiated \
             format is {}x{}; restarting the stream to re-commit it (format restart {} of {}, \
             {n} in this run)",
            dims.0,
            dims.1,
            self.mismatch_frames,
            negotiated.0,
            negotiated.1,
            self.format_restarts + 1,
            self.format_restart_limit
        );
        self.format_restarts += 1;
        // The next decision is made on evidence gathered after this restart, not on the evidence
        // that bought it: `invoke_restart` deliberately leaves the clock alone, so the watchdog
        // clears it here and the stall path does not.
        self.mismatch_since = None;
        self.mismatch_frames = 0;
        // A restart is a stream start for the stall timer too, exactly as `restart_stream` makes
        // it: without this the next `step` could fire the stall restart on top of this one.
        self.restart_base = Instant::now();
        self.invoke_restart(shared)
    }

    /// Record a restart that did not work and carry on polling the same source.
    ///
    /// Counted in `capture_errors` rather than in a counter of its own: whichever timer asked —
    /// `stream_restarts` or `format_mismatch_restarts` — has already counted the attempt (a
    /// restart that failed still happened and still cost the stream), and what a failed one
    /// leaves behind is an ordinary survivable capture error.
    fn failed_restart(&mut self, text: String, shared: &Arc<Shared>) -> Step {
        {
            let mut inner = lock_or_recover(&shared.inner);
            inner.capture_errors += 1;
            inner.last_error = Some(text.clone());
        }
        log::warn!("capture: {text}");
        Step::Continue
    }

    /// One pass of the capture loop: restart if the stall has run long enough, else take a frame
    /// or handle the failure to.
    fn step(&mut self, compressed: &Arc<Slot<CompressedFrame>>, shared: &Arc<Shared>) -> Step {
        if let Some(after) = self.restart_after {
            if self.restart_base.elapsed() >= after {
                return self.restart_stream(shared);
            }
        }
        let Some(source) = self.source.as_mut() else {
            return Step::Continue;
        };
        match source.next_frame() {
            Ok(frame) => {
                self.consecutive_errors = 0;
                self.last_kind = None;
                self.restart_base = Instant::now();
                let dims = (frame.width, frame.height);
                let change = {
                    let mut inner = lock_or_recover(&shared.inner);
                    inner.frames_captured += 1;
                    inner.last_frame_at = Some(Instant::now());
                    inner.last_captured_at = Some(frame.captured_at);
                    inner.last_sequence = Some(frame.sequence);
                    // `bytesused` is exactly the owned copy's length (§1.3, A19: instrument it).
                    let len = frame.jpeg.len().min(u32::MAX as usize) as u32;
                    inner.bytes.record(len);
                    inner.bytes_window.record(len);
                    // A6: the SOF header of *this* frame is the only authority on its size, so a
                    // target-side mode change (§6.1 S2-3) becomes visible here and nowhere else.
                    // The first frame establishes the resolution; it is not a change.
                    let previous = inner.last_resolution.replace(dims);
                    match previous {
                        Some(prev) if prev != dims => {
                            inner.resolution_changes += 1;
                            Some((Some(prev), inner.resolution_changes))
                        }
                        Some(_) => None,
                        None => Some((None, 0)),
                    }
                };
                if let Some((previous, n)) = change {
                    match previous {
                        Some(prev) => log::info!(
                            "capture: SOF dimensions changed {}x{} -> {}x{} at sequence {} \
                             ({} bytes, change {n})",
                            prev.0,
                            prev.1,
                            frame.width,
                            frame.height,
                            frame.sequence,
                            frame.jpeg.len()
                        ),
                        None => log::info!(
                            "capture: first frame is {}x{} at sequence {} ({} bytes)",
                            frame.width,
                            frame.height,
                            frame.sequence,
                            frame.jpeg.len()
                        ),
                    }
                }
                // Replace-on-full: the displaced frame is an obsolete one nobody will see
                // (§5.2, "drop before decoding"). Its allocation is released here.
                if compressed.put(frame).is_some() && !compressed.is_closed() {
                    lock_or_recover(&shared.inner).dropped_pre_decode += 1;
                }
                // After the frame has been handed on, not instead of it: a mismatched frame is
                // still a frame, and the renderer shows it while the watchdog works on the
                // device (C4 — the client coping with a size is not this mechanism's business).
                self.watch_format(dims, shared)
            }
            Err(CaptureError::Disconnected) => Step::Lost(CaptureError::Disconnected.to_string()),
            Err(e) => {
                // A stall or an I/O error. Count it, say what it was, back off, keep polling —
                // §6.1 S1-2 is that frames stopping neither crashes nor hangs, and A5 forbids
                // turning this into a claim about the signal.
                self.consecutive_errors = self.consecutive_errors.saturating_add(1);
                let kind = error_kind(&e);
                let is_new_kind = self.last_kind != Some(kind);
                // Formatting happens once per run of identical failures, not once per retry.
                let text = is_new_kind.then(|| e.to_string());
                {
                    let mut inner = lock_or_recover(&shared.inner);
                    inner.capture_errors += 1;
                    if let Some(t) = &text {
                        inner.last_error = Some(t.clone());
                    }
                }
                if let Some(t) = text {
                    self.last_kind = Some(kind);
                    log::debug!("capture: {t}");
                }
                let nap = if self.consecutive_errors > BACKOFF_ESCALATE_AFTER {
                    BACKOFF_SLOW
                } else {
                    BACKOFF
                };
                backoff(shared, compressed, nap);
                Step::Continue
            }
        }
    }
}

/// Capture thread body (§4.1 row 1, §6.1 S2-1/S2-2/S2-4).
///
/// The loop body runs under `catch_unwind`: the `v4l` crate's destructors panic on a failed
/// teardown ioctl and they run on this thread, and §6.1 S1-1 forbids a capture failure from
/// taking anything else down. A panic is treated as a lost device, exactly like `ENODEV`, so a
/// recovering pipeline reopens after one instead of sitting dead. See the module docs.
fn capture_loop(mut cap: Capture, compressed: &Arc<Slot<CompressedFrame>>, shared: &Arc<Shared>) {
    // The first open runs here rather than in `Pipeline::spawn` so that a device which is not
    // there yet costs the caller nothing and lands in the retry loop like any other absence.
    if !cap.open_first(compressed, shared) {
        compressed.close();
        log::info!("capture thread finished");
        return;
    }

    loop {
        if shared.stopping.load(Ordering::SeqCst) || compressed.is_closed() {
            break;
        }
        if cap.source.is_none() && !cap.reconnect(compressed, shared) {
            break;
        }

        // `AssertUnwindSafe` because the state that could be observed after an unwind is exactly
        // the state this loop is here to manage: the source (released below, never used again)
        // and the shared counters (plain integers behind a mutex whose poisoning this module
        // already recovers from — see `capture::lock_or_recover`).
        let outcome = catch_unwind(AssertUnwindSafe(|| cap.step(compressed, shared)));

        let mut reopen = None;
        let lost = match outcome {
            Ok(Step::Continue) => None,
            Ok(Step::Lost(msg)) => Some(msg),
            Ok(Step::Reopen(msg)) => {
                reopen = Some(msg);
                None
            }
            Err(payload) => Some(format!(
                "capture thread panicked: {}",
                panic_message(&payload)
            )),
        };

        if let Some(msg) = reopen {
            // Not `lose_device`: the device never went away, so `disconnects` and
            // `disconnected_since` must not say it did. What this shares with a disconnection is
            // everything after that — the source is released under `catch_unwind` and the loop's
            // own `reconnect` opens the node afresh on the next pass, which is exactly the
            // `S_FMT`/`S_PARM`/`STREAMON` the watchdog is asking for. The state goes to
            // `Reconnecting` in there; for the half-second that takes, the title says the capture
            // device is being reopened, which is true.
            log::warn!("capture: {msg}");
            {
                let mut inner = lock_or_recover(&shared.inner);
                inner.last_error = Some(msg);
                inner.negotiated_dimensions = None;
            }
            cap.release_source();
        }

        if let Some(msg) = lost {
            cap.lose_device(&msg, shared);
            if !cap.recover {
                // Stage 1's terminal behaviour. Wake the decode thread; leave the output slot
                // alone so the renderer can still present the last image it holds (§6.1 S1-2).
                compressed.close();
                break;
            }
            // Recovering: the compressed slot stays open and the decode thread stays alive, so
            // the renderer keeps the last image for the whole gap.
        }
    }
    // On the way out too: a panicking destructor must not turn a clean shutdown into a
    // panicking thread that `stop()` then reports as a failure.
    cap.release_source();
    log::info!("capture thread finished");
}

/// What the capture loop should do next.
enum Step {
    Continue,
    /// The device is gone, with the message to record. Recovery decides what happens next.
    Lost(String),
    /// The device is **there** and is to be opened again anyway, with the reason to record: the
    /// format watchdog's one escalation, once its restarts have failed to commit the negotiated
    /// mode. Deliberately not a [`Step::Lost`] — nothing was lost, so nothing is counted as a
    /// disconnection — but it takes the same road from there: release the source, then the
    /// capture loop's ordinary reconnect, backoff and all.
    Reopen(String),
}

/// The message out of a `catch_unwind` payload, for the two shapes `panic!` produces.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Decode thread body (§4.1 row 2). Exactly one of these (§1.3).
fn decode_loop(
    compressed: &Arc<Slot<CompressedFrame>>,
    output: &Arc<Slot<DecodedFrame>>,
    shared: &Arc<Shared>,
    config: &PipelineConfig,
) {
    let mut decoder = Decoder::new();
    // The reused RGBA buffer (§1.3). It is swapped with whatever `put` displaces, so in steady
    // state with a slow renderer this pipeline decodes without allocating at all.
    let mut scratch = DecodedFrame::empty();

    loop {
        let frame = match compressed.wait_take(config.decode_wait) {
            Some(f) => f,
            None => {
                if compressed.is_closed() || shared.stopping.load(Ordering::SeqCst) {
                    break;
                }
                continue;
            }
        };

        if let Some(delay) = config.decode_delay {
            std::thread::sleep(delay);
        }

        match decoder.decode(&frame, &mut scratch) {
            Ok(()) => {
                lock_or_recover(&shared.inner).frames_decoded += 1;
                let done = std::mem::take(&mut scratch);
                match output.put(done) {
                    Some(displaced) => {
                        if !output.is_closed() {
                            lock_or_recover(&shared.inner).dropped_post_decode += 1;
                        }
                        // Recycle the displaced frame's buffer as the next scratch.
                        scratch = displaced;
                    }
                    None => scratch = DecodedFrame::empty(),
                }
            }
            Err(e) => {
                // Strict mode rejecting a truncated frame is the expected shape here (§1.3).
                // Skip it; never exit over one bad frame.
                let mut inner = lock_or_recover(&shared.inner);
                inner.decode_errors += 1;
                inner.last_error = Some(e.to_string());
                drop(inner);
                log::debug!("decode: sequence {}: {e}", frame.sequence);
            }
        }
    }
    log::info!("decode thread finished");
}

/// A running pipeline (§4.1).
pub struct PipelineHandle {
    output: Arc<Slot<DecodedFrame>>,
    compressed: Arc<Slot<CompressedFrame>>,
    shared: Arc<Shared>,
    capture: Option<JoinHandle<()>>,
    decode: Option<JoinHandle<()>>,
    describe: String,
}

impl PipelineHandle {
    /// The decoded-frame handoff. The render thread clones this `Arc` and calls
    /// [`Slot::take`] or [`Slot::wait_take`]; it takes whatever is pending, with no catch-up and
    /// no queue drain (§5.2).
    pub fn output(&self) -> Arc<Slot<DecodedFrame>> {
        Arc::clone(&self.output)
    }

    /// A snapshot of the counters and the state, taken under a single lock acquisition so
    /// every field describes the same moment.
    pub fn stats(&self) -> PipelineStats {
        let inner = lock_or_recover(&self.shared.inner);
        let state = self.shared.state_of(&inner);
        PipelineStats {
            frames_captured: inner.frames_captured,
            frames_decoded: inner.frames_decoded,
            dropped_pre_decode: inner.dropped_pre_decode,
            dropped_post_decode: inner.dropped_post_decode,
            decode_errors: inner.decode_errors,
            capture_errors: inner.capture_errors,
            last_error: inner.last_error.clone(),
            last_frame_at: inner.last_frame_at,
            last_captured_at: inner.last_captured_at,
            bytes: inner.bytes,
            last_sequence: inner.last_sequence,
            disconnects: inner.disconnects,
            reopens: inner.reopens,
            reopen_attempts: inner.reopen_attempts,
            stream_restarts: inner.stream_restarts,
            resolution_changes: inner.resolution_changes,
            last_resolution: inner.last_resolution,
            format_mismatch_restarts: inner.format_mismatch_restarts,
            format_mismatch_reopens: inner.format_mismatch_reopens,
            format_mismatch_accepted: inner.format_mismatch_accepted,
            negotiated_dimensions: inner.negotiated_dimensions,
            disconnected_since: inner.disconnected_since,
            state,
        }
    }

    /// Take the `bytesused` fold accumulated since the last call, and reset it.
    ///
    /// [`PipelineStats::bytes`] is cumulative, so its `min`/`max` are min and max *ever* and
    /// cannot answer "how big were the frames in the last second" — the question §1.3 and A19
    /// leave open, because every decode margin so far was measured against an idle desktop.
    /// Sampling `bytes.last` at any poll rate below the frame rate would miss frames and quietly
    /// under-report the max, so the window is folded on the capture thread and handed over whole.
    /// `examples/capture-probe.rs` is the caller; nothing else needs it.
    pub fn take_bytes_window(&self) -> BytesUsedStats {
        std::mem::take(&mut lock_or_recover(&self.shared.inner).bytes_window)
    }

    /// How long since the last frame was captured, or `None` if none ever has been.
    ///
    /// This is the number §6.1 S1-2 wants surfaced: "the tool says frames stopped arriving".
    /// It says nothing about the signal (A5).
    pub fn frames_stalled_for(&self) -> Option<Duration> {
        lock_or_recover(&self.shared.inner)
            .last_frame_at
            .map(|t| t.elapsed())
    }

    /// The current state, with staleness derived from the clock (§6.1).
    pub fn state(&self) -> PipelineState {
        self.shared.state()
    }

    /// What the source is, for logs and the window title.
    pub fn describe(&self) -> &str {
        &self.describe
    }

    /// Close both handoffs and join both threads.
    ///
    /// Returns once both have exited. Prompt in every case, including mid-reconnect: closing the
    /// slots wakes any waiter, and every wait on the capture thread is a poll on the stop flag
    /// rather than a sleep. **The bound is one in-flight call into the source, plus
    /// [`STOP_POLL`] (2 ms):** either a `next_frame` — which [`FrameSource`] requires to be
    /// bounded, and which `V4l2Source` bounds by its own dequeue timeout, 250 ms by default — or
    /// a `restart`, measured at 8.7 ms on this hardware. The reopen backoff, up to
    /// [`PipelineConfig::reopen_max_backoff`], does **not** count: it is interruptible.
    ///
    /// [`SourceOpener::open`] does not count either, and that is the one residual worth stating.
    /// An open cannot be interrupted, so it runs on a helper thread (see `Capture::open_detached`)
    /// which `stop()` abandons rather than joins. If `stop()` lands while one is in flight, the
    /// helper finishes the open on its own and immediately drops whatever it got — so a device
    /// opened at that moment is released a little *after* `stop()` returned, not before. Nothing
    /// waits on it, nothing is reported from it, and no frame it produced can reach a consumer.
    pub fn stop(mut self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        self.compressed.close();
        self.output.close();
        for (name, handle) in [
            ("nanokvm-capture", self.capture.take()),
            ("nanokvm-decode", self.decode.take()),
        ] {
            if let Some(h) = handle {
                if h.join().is_err() {
                    log::error!("{name} thread panicked");
                    let mut inner = lock_or_recover(&self.shared.inner);
                    inner.last_error = Some(format!("{name} thread panicked"));
                }
            }
        }
        lock_or_recover(&self.shared.inner).state = PipelineState::Stopped;
        log::info!("capture pipeline stopped");
    }
}

impl Drop for PipelineHandle {
    /// A dropped handle must not leave two threads running against a device nobody owns.
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        self.compressed.close();
        self.output.close();
        if let Some(h) = self.capture.take() {
            let _ = h.join();
        }
        if let Some(h) = self.decode.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::SyntheticSource;

    fn fixture_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/frames/absrange")
    }

    fn source(interval: Duration) -> Box<dyn FrameSource> {
        Box::new(SyntheticSource::from_dir(&fixture_dir(), interval).expect("synthetic source"))
    }

    /// Poll `cond` until it holds or the deadline passes. Not a sleep-based assertion: the
    /// deadline only bounds a hang, and the assertion is on the condition.
    fn wait_for(label: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if cond() {
                return;
            }
            std::thread::yield_now();
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("timed out waiting for {label}");
    }

    #[test]
    fn a_fresh_pipeline_reports_running_with_no_frames_yet() {
        let cfg = PipelineConfig {
            stall_after: Duration::from_secs(30),
            ..PipelineConfig::default()
        };
        let h = Pipeline::start(source(Duration::from_millis(5)), cfg);
        assert!(h.describe().contains("absrange"));
        wait_for("first frame", Duration::from_secs(5), || {
            h.stats().frames_captured > 0
        });
        assert_eq!(h.state(), PipelineState::Running);
        h.stop();
    }

    /// A source that never produces a frame, only timeouts — the §6.1 "capture stalled" shape.
    struct NeverSource;
    impl FrameSource for NeverSource {
        fn next_frame(&mut self) -> Result<CompressedFrame, CaptureError> {
            std::thread::sleep(Duration::from_millis(2));
            Err(CaptureError::Timeout(Duration::from_millis(2)))
        }
        fn describe(&self) -> String {
            "never".to_string()
        }
    }

    #[test]
    fn stall_is_derived_from_the_clock_not_from_a_thread() {
        // A source that produces nothing at all: state must become Stalled without anybody
        // having to notice (§6.1 S1-2), and must never become Disconnected.
        let s = NeverSource;
        let cfg = PipelineConfig {
            stall_after: Duration::from_millis(20),
            ..PipelineConfig::default()
        };
        let h = Pipeline::start(Box::new(s), cfg);
        wait_for("stalled", Duration::from_secs(5), || {
            h.state() == PipelineState::Stalled
        });
        assert!(
            h.frames_stalled_for().is_none(),
            "no frame has ever arrived"
        );
        assert!(
            h.stats().capture_errors > 0,
            "the timeouts were not counted"
        );
        h.stop();
    }

    #[test]
    fn decode_errors_are_counted_and_the_thread_keeps_going() {
        // A corpus of one truncated frame: every decode fails, capture keeps working.
        let good = std::fs::read(fixture_dir().join("abs-0-0.jpg")).expect("fixture");
        let cut = good.len() * 6 / 10;
        let s = SyntheticSource::new(vec![good[..cut].to_vec()], Duration::from_millis(2))
            .expect("source");
        let h = Pipeline::start(Box::new(s), PipelineConfig::default());
        wait_for("some decode errors", Duration::from_secs(5), || {
            h.stats().decode_errors >= 3
        });
        let st = h.stats();
        assert_eq!(st.frames_decoded, 0);
        assert!(st.frames_captured >= 3);
        assert!(st.last_error.is_some());
        h.stop();
    }

    #[test]
    fn bytesused_statistics_are_instrumented() {
        let h = Pipeline::start(source(Duration::from_millis(2)), PipelineConfig::default());
        wait_for("frames", Duration::from_secs(5), || {
            h.stats().frames_captured >= 5
        });
        let b = h.stats().bytes;
        assert!(b.count >= 5);
        assert!(b.min >= 100_000 && b.max <= 300_000, "{b}");
        assert!(b.mean() > 0.0);
        assert!(b.last >= b.min && b.last <= b.max);
        h.stop();
    }

    /// A source that fails instantly and counts the attempts — `capture::v4l2` does exactly
    /// this when the fd reports a sticky `POLLERR`. Without a backoff the capture thread spins
    /// a core while `state()` says `Stalled`.
    struct CountingFail(Arc<std::sync::atomic::AtomicU64>);
    impl FrameSource for CountingFail {
        fn next_frame(&mut self) -> Result<CompressedFrame, CaptureError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Err(CaptureError::Io(std::io::Error::from_raw_os_error(
                libc::EIO,
            )))
        }
        fn describe(&self) -> String {
            "instant-EIO".to_string()
        }
    }

    #[test]
    fn an_instantly_failing_source_is_retried_at_a_bounded_rate() {
        let calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let h = Pipeline::start(
            Box::new(CountingFail(Arc::clone(&calls))),
            PipelineConfig {
                stall_after: Duration::from_millis(50),
                ..PipelineConfig::default()
            },
        );
        std::thread::sleep(Duration::from_millis(200));
        let n = calls.load(Ordering::Relaxed);
        let st = h.stats();
        h.stop();
        // 200 ms at the 20 ms backoff is ~10 attempts. The bound is deliberately loose: what is
        // being asserted is "bounded", not a precise rate.
        assert!(
            n < 200,
            "capture made {n} failing next_frame() calls in 200 ms; the backoff is not working"
        );
        assert!(n >= 2, "the source was barely called at all: {n}");
        assert_eq!(st.capture_errors, n, "every failure must still be counted");
        assert_eq!(st.state, PipelineState::Stalled);
    }

    #[test]
    fn a_run_of_identical_errors_is_formatted_once_but_counted_every_time() {
        let calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let h = Pipeline::start(
            Box::new(CountingFail(Arc::clone(&calls))),
            PipelineConfig::default(),
        );
        wait_for("several errors", Duration::from_secs(5), || {
            h.stats().capture_errors >= 3
        });
        let st = h.stats();
        h.stop();
        assert!(st
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("I/O error")));
        assert!(st.capture_errors >= 3);
    }

    /// §6.1 S1-1 and `capture::v4l2` module docs item 4: the `v4l` crate's destructors panic on
    /// a failed teardown ioctl, on this very thread. A panic must land as `Disconnected` with
    /// the message, not as a silently dead thread whose pipeline still claims `Running`.
    #[test]
    fn a_panicking_source_becomes_disconnected_rather_than_killing_the_thread() {
        struct PanicsOnSecondCall(u32);
        impl FrameSource for PanicsOnSecondCall {
            fn next_frame(&mut self) -> Result<CompressedFrame, CaptureError> {
                self.0 += 1;
                if self.0 >= 2 {
                    panic!("STREAMOFF failed, as the v4l crate would");
                }
                std::thread::sleep(Duration::from_millis(1));
                Err(CaptureError::Timeout(Duration::from_millis(1)))
            }
            fn describe(&self) -> String {
                "panics".to_string()
            }
        }

        let h = Pipeline::start(Box::new(PanicsOnSecondCall(0)), PipelineConfig::default());
        wait_for(
            "disconnected after the panic",
            Duration::from_secs(5),
            || h.state() == PipelineState::Disconnected,
        );
        let st = h.stats();
        assert!(
            st.last_error
                .as_deref()
                .is_some_and(|e| e.contains("panicked") && e.contains("STREAMOFF")),
            "the panic message was lost: {:?}",
            st.last_error
        );
        let t = Instant::now();
        h.stop();
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "stop() hung after a panic"
        );
    }

    #[test]
    fn dropping_the_handle_joins_the_threads() {
        let h = Pipeline::start(source(Duration::from_millis(2)), PipelineConfig::default());
        wait_for("frames", Duration::from_secs(5), || {
            h.stats().frames_captured >= 1
        });
        let t = Instant::now();
        drop(h);
        assert!(t.elapsed() < Duration::from_secs(2), "drop hung");
    }
}
