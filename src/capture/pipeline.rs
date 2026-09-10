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
//! **The capture thread survives everything except disconnection.** §6.1 S1-2 is architectural:
//! frames stopping must neither crash nor hang, and a stall is not evidence of signal loss (A5).
//! [`CaptureError::Timeout`] and [`CaptureError::Io`] are counted and the loop continues;
//! [`CaptureError::Disconnected`] records the state and exits, because recovery is Stage 2.
//! A frame that fails to decode is counted and skipped; the decode thread never exits over one.
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
//! would leave the pipeline claiming `Running` forever. A panic is therefore recorded as
//! [`PipelineState::Disconnected`] with the panic message, exactly like a real `ENODEV`.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::v4l2::BytesUsedStats;
use super::{
    lock_or_recover, CaptureError, CompressedFrame, DecodedFrame, Decoder, FrameSource, Slot,
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
    /// The device reported `ENODEV`. The capture thread has exited; Stage 2 owns rediscovery.
    Disconnected,
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
}

impl Default for PipelineConfig {
    fn default() -> Self {
        PipelineConfig {
            stall_after: Duration::from_millis(500),
            decode_wait: Duration::from_millis(100),
            decode_delay: None,
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
    pub last_error: Option<String>,
    /// Host-clock instant at which the last frame was captured, for staleness.
    pub last_frame_at: Option<Instant>,
    /// The last frame's V4L2 timestamp on `CLOCK_MONOTONIC` (§5.5). Comparable with
    /// [`super::v4l2::now_monotonic`] to get a capture-to-dequeue age — **not** a latency.
    pub last_captured_at: Option<Duration>,
    /// Compressed frame sizes seen (§1.3: instrument `bytesused` in Stage 1).
    pub bytes: BytesUsedStats,
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
    pub fn start(source: Box<dyn FrameSource>, config: PipelineConfig) -> PipelineHandle {
        let compressed: Arc<Slot<CompressedFrame>> = Arc::new(Slot::new());
        let output: Arc<Slot<DecodedFrame>> = Arc::new(Slot::new());
        let shared = Arc::new(Shared::new(config.stall_after));

        let describe = source.describe();
        log::info!("capture pipeline starting on {describe}");

        let capture = {
            let compressed = Arc::clone(&compressed);
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("nanokvm-capture".to_string())
                .spawn(move || capture_loop(source, &compressed, &shared))
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
    const STEP: Duration = Duration::from_millis(2);
    let deadline = Instant::now() + nap;
    while Instant::now() < deadline {
        if shared.stopping.load(Ordering::SeqCst) || compressed.is_closed() {
            return;
        }
        std::thread::sleep(STEP.min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// Capture thread body (§4.1 row 1).
///
/// The loop body runs under `catch_unwind`: the `v4l` crate's destructors panic on a failed
/// teardown ioctl and they run on this thread, and §6.1 S1-1 forbids a capture failure from
/// taking anything else down. See the module docs.
fn capture_loop(
    source: Box<dyn FrameSource>,
    compressed: &Arc<Slot<CompressedFrame>>,
    shared: &Arc<Shared>,
) {
    // `AssertUnwindSafe` because the state that could be observed after an unwind is exactly the
    // state this function is here to manage: the source (dropped and never used again on this
    // path) and the shared counters (plain integers behind a mutex whose poisoning this module
    // already recovers from — see `capture::lock_or_recover`).
    let mut source = source;
    let mut consecutive_errors: u32 = 0;
    let mut last_kind: Option<(u8, i32)> = None;

    loop {
        if shared.stopping.load(Ordering::SeqCst) || compressed.is_closed() {
            break;
        }

        let step = catch_unwind(AssertUnwindSafe(|| {
            capture_step(
                source.as_mut(),
                compressed,
                shared,
                &mut consecutive_errors,
                &mut last_kind,
            )
        }));

        match step {
            Ok(Step::Continue) => {}
            Ok(Step::Exit) => break,
            Err(payload) => {
                let msg = panic_message(&payload);
                log::error!("capture: capture thread panicked: {msg}");
                {
                    let mut inner = lock_or_recover(&shared.inner);
                    inner.state = PipelineState::Disconnected;
                    inner.last_error = Some(format!("capture thread panicked: {msg}"));
                }
                compressed.close();
                break;
            }
        }
    }
    log::info!("capture thread finished");
}

/// What the capture loop should do next.
enum Step {
    Continue,
    Exit,
}

/// One pass of the capture loop: take a frame, or handle the failure to.
fn capture_step(
    source: &mut dyn FrameSource,
    compressed: &Arc<Slot<CompressedFrame>>,
    shared: &Arc<Shared>,
    consecutive_errors: &mut u32,
    last_kind: &mut Option<(u8, i32)>,
) -> Step {
    match source.next_frame() {
        Ok(frame) => {
            *consecutive_errors = 0;
            *last_kind = None;
            {
                let mut inner = lock_or_recover(&shared.inner);
                inner.frames_captured += 1;
                inner.last_frame_at = Some(Instant::now());
                inner.last_captured_at = Some(frame.captured_at);
                // `bytesused` is exactly the owned copy's length (§1.3, A19: instrument it).
                inner
                    .bytes
                    .record(frame.jpeg.len().min(u32::MAX as usize) as u32);
            }
            // Replace-on-full: the displaced frame is an obsolete one nobody will see
            // (§5.2, "drop before decoding"). Its allocation is released here.
            if compressed.put(frame).is_some() && !compressed.is_closed() {
                lock_or_recover(&shared.inner).dropped_pre_decode += 1;
            }
            Step::Continue
        }
        Err(CaptureError::Disconnected) => {
            log::error!("capture: device disconnected; capture thread exiting");
            {
                let mut inner = lock_or_recover(&shared.inner);
                inner.state = PipelineState::Disconnected;
                inner.last_error = Some(CaptureError::Disconnected.to_string());
            }
            // Wake the decode thread; leave the output slot alone so the renderer can still
            // present the last image it holds (§6.1 S1-2: preserve the last image).
            compressed.close();
            Step::Exit
        }
        Err(e) => {
            // A stall or an I/O error. Count it, say what it was, back off, keep polling —
            // §6.1 S1-2 is that frames stopping neither crashes nor hangs, and A5 forbids
            // turning this into a claim about the signal.
            *consecutive_errors = consecutive_errors.saturating_add(1);
            let kind = error_kind(&e);
            let is_new_kind = *last_kind != Some(kind);
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
                *last_kind = Some(kind);
                log::debug!("capture: {t}");
            }
            let nap = if *consecutive_errors > BACKOFF_ESCALATE_AFTER {
                BACKOFF_SLOW
            } else {
                BACKOFF
            };
            backoff(shared, compressed, nap);
            Step::Continue
        }
    }
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
            state,
        }
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
    /// Returns once both have exited. Prompt in every case, including after a disconnection:
    /// closing the slots wakes any waiter, and the capture thread checks for shutdown on each
    /// pass, so the longest wait is one source `next_frame` timeout.
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
