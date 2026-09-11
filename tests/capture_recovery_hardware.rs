//! Capture recovery against the real dongle: §6.1 S2-1 and S2-4 (plan §6.1, §12 Stage 2).
//!
//! ```text
//! cargo test --features hardware --test capture_recovery_hardware -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `/dev/video4` is the NanoKVM-USB node on this desk. `/dev/video0`–`3` belong to unrelated
//! hardware and are never opened here. Nothing in this file sends input of any kind; the target
//! is only ever watched.
//!
//! §12 "verify by consequence, not by acknowledgement": every assertion is on an observed frame,
//! an observed node, or an observed mapping — never on an ioctl returning success. `restart()`
//! returning `Ok` proves nothing; frames arriving after it does.

#![cfg(feature = "hardware")]

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use nanokvm::capture::{
    CaptureError, CompressedFrame, FrameSource, Pipeline, PipelineConfig, PipelineState,
    SourceOpener, V4l2Source,
};
use nanokvm::discovery::{Constraints, DiscoveringOpener, NodeKind, NodeResolver};

/// `00-common.md`: kill every process you start. H-B2 spawns `scripts/usb-replug.py` and then
/// asserts its way to the bottom of the test, so the child is spawned through a `Drop` guard
/// rather than a bare `Child`, which does not kill on drop. `tests/capture_recovery.rs` owns the
/// test that the guard works.
#[path = "support/child_guard.rs"]
mod child_guard;

/// The V4L2 node admits one streaming opener at a time, and the harness runs tests in parallel
/// by default, so every test in this binary holds this lock while it owns the device. Test
/// *binaries* must still be run one at a time.
static DEVICE: Mutex<()> = Mutex::new(());

/// The dongle's video node on this desk (CLAUDE.md): the capture node of the `345f:2133`
/// interface under the dongle's own `1a40:0101` hub, as `VIDIOC_QUERYCAP` decides it — never the
/// metadata sibling, and never anything on bus 5, which is the user's dock.
///
/// H-B1 opens this name directly: it neither takes the device away nor expects it back, so the
/// name cannot move under it, and a run on a renumbered desk fails with this comment as the
/// explanation. **H-B2 reopens by identity instead** (§8, C6) — a replug is exactly when the
/// kernel can hand the node back under a different name — and uses this only as the expectation
/// it prints.
const NODE: &str = "/dev/video4";
/// The primary path (§6): the target outputs 1080p.
const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const FPS: u32 = 60;
/// How long after a reopen the frames have to settle at the negotiated size (H-B2), **derived
/// from the config the test actually runs with** rather than written down.
///
/// A hand-written constant here was wrong twice over: it said 5 s and justified it as "the 500 ms
/// grace plus a restart", which understates the real arithmetic by three seconds because the
/// watchdog doubles its wait between restarts and then escalates. Worse, it was wrong *silently* —
/// raising `format_mismatch_grace` or `format_mismatch_restart_limit` would have broken the
/// assertion with no sign of why. So it is computed, and printed next to the config it came from.
///
/// The watchdog's whole ladder, in order:
///
/// - restart *k* fires after `grace * 2^(k-1)` of mismatched frames, so a full budget of `limit`
///   restarts costs `grace * (2^limit - 1)` — 3.5 s at the shipped 500 ms and 3;
/// - each restart costs the stream ~82 ms (8.7 ms for the `restart()` itself, ~73 ms to the next
///   frame — C4);
/// - one more `grace` passes before the watchdog decides the budget is spent;
/// - it then escalates **once** to a full reopen — the pipeline's `reopen_backoff` wait plus an
///   open of a device that has just re-enumerated — and gets one fresh budget, so the whole of
///   the above happens a second time before the mismatch is accepted.
///
/// The frame gate (`format_mismatch_frames`, 12) adds 200 ms at 60 fps and is inside the
/// rounding. `MARGIN` is slack for a loaded machine, not part of the derivation.
fn settle_window(config: &PipelineConfig) -> Duration {
    /// Slack on top of the derived worst case.
    const MARGIN: Duration = Duration::from_secs(1);
    /// C4: `restart()` 8.7 ms, ~73 ms to the first frame after it.
    const RESTART_COST: Duration = Duration::from_millis(82);
    /// A generous allowance for the `open()` itself: `S_FMT`, `S_PARM`, `REQBUFS`, four `mmap`s
    /// and `QBUF`s and a `STREAMON`, each a USB control transfer to a freshly enumerated device.
    const OPEN_COST: Duration = Duration::from_secs(1);

    let Some(grace) = config.format_mismatch_grace else {
        // The watchdog is off, so nothing is going to fix a stuck stream and there is no ladder
        // to wait out. Any bound is arbitrary; keep the old one.
        return Duration::from_secs(5) + MARGIN;
    };
    let limit = config.format_mismatch_restart_limit;
    let budget = grace.saturating_mul(2u32.saturating_pow(limit).saturating_sub(1))
        + RESTART_COST.saturating_mul(limit)
        + grace;
    let escalation = config.reopen_backoff + OPEN_COST;
    budget.saturating_mul(2) + escalation + MARGIN
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Poll `cond` until it holds, or fail with `label`. The deadline bounds a hang; the assertion is
/// on the condition.
fn wait_for(label: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {timeout:?} waiting for {label}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The node a [`FrameSource`] says it is streaming from: `V4l2Source::describe` is
/// `"V4L2 /dev/videoN MJPG WxH @F fps, B mmap buffers"`, and the second word is the path. Used
/// because H-B2's opener resolves the node itself, so the test learns which one only by asking
/// what was opened — which is the point: the name is an outcome, not an input.
fn node_of(describe: &str) -> String {
    describe
        .split_whitespace()
        .nth(1)
        .unwrap_or_else(|| panic!("no node in the source description {describe:?}"))
        .to_string()
}

/// What the frames after the reopen actually looked like (§6/A6).
///
/// The stats alone cannot answer this: `last_resolution` is a sample, and the failure measured on
/// 2026-09-11 — a reopened stream stuck in the device's power-on 640x480 while `S_FMT` had
/// committed 1920x1080 — is about *every* frame after the reopen, not the last one.
#[derive(Default)]
struct PostReopen {
    /// Frames after the reopen whose SOF dimensions were not the negotiated ones.
    mismatched: u64,
    /// Frames after the reopen at the negotiated size.
    matched: u64,
    /// When the first post-reopen frame arrived, and the first one at the negotiated size.
    first_frame: Option<Instant>,
    first_match: Option<Instant>,
    /// Every distinct size seen after the reopen, in order of first appearance.
    sizes: Vec<(u32, u32)>,
}

/// A [`FrameSource`] that records the SOF dimensions of every frame, and delegates everything
/// else — `negotiated_dimensions` included, so the pipeline's format watchdog is the one under
/// test rather than one this wrapper switched off.
struct WatchedSource {
    inner: Box<dyn FrameSource>,
    /// `None` for the first open: only the frames after the reopen are being characterised.
    post: Option<Arc<Mutex<PostReopen>>>,
    negotiated: (u32, u32),
}

impl FrameSource for WatchedSource {
    fn next_frame(&mut self) -> Result<CompressedFrame, CaptureError> {
        let frame = self.inner.next_frame()?;
        if let Some(post) = &self.post {
            let dims = (frame.width, frame.height);
            let mut p = lock(post);
            if p.first_frame.is_none() {
                p.first_frame = Some(Instant::now());
            }
            if !p.sizes.contains(&dims) {
                p.sizes.push(dims);
            }
            if dims == self.negotiated {
                p.matched += 1;
                if p.first_match.is_none() {
                    p.first_match = Some(Instant::now());
                }
            } else {
                p.mismatched += 1;
            }
        }
        Ok(frame)
    }

    fn restart(&mut self) -> Result<(), CaptureError> {
        self.inner.restart()
    }

    fn negotiated_dimensions(&self) -> Option<(u32, u32)> {
        self.inner.negotiated_dimensions()
    }

    fn describe(&self) -> String {
        self.inner.describe()
    }
}

/// A [`SourceOpener`] that records the node each open actually landed on, wraps what it opened in
/// a [`WatchedSource`], and delegates everything else.
struct RecordingOpener {
    inner: DiscoveringOpener,
    opened: Arc<Mutex<Vec<String>>>,
    post: Arc<Mutex<PostReopen>>,
}

impl SourceOpener for RecordingOpener {
    fn open(&mut self) -> Result<Box<dyn FrameSource>, CaptureError> {
        let source = self.inner.open()?;
        let mut opened = lock(&self.opened);
        opened.push(node_of(&source.describe()));
        // The first open is the control; everything from the reopen on is what H-B2 measures.
        let post = (opened.len() > 1).then(|| Arc::clone(&self.post));
        drop(opened);
        Ok(Box::new(WatchedSource {
            inner: source,
            post,
            negotiated: (WIDTH, HEIGHT),
        }))
    }

    fn describe(&self) -> String {
        self.inner.describe()
    }
}

/// How many mappings of `node` this process holds, straight out of `/proc/self/maps`.
///
/// The `v4l` crate's `Arena` mmaps one region per buffer and unmaps them in its `Drop`. A reopen
/// that leaked them would show here and nowhere else — the counters would look perfect while the
/// process quietly accumulated a 4-buffer arena per replug (§6.1 S2-2: "no leaked buffers").
fn mappings_of(node: &str) -> usize {
    std::fs::read_to_string("/proc/self/maps")
        .map(|maps| maps.lines().filter(|l| l.contains(node)).count())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------------------------
// H-B1 — the pipeline's stream restart, on the real device
// ---------------------------------------------------------------------------------------------

/// What the wrapper below recorded, for the test to assert on once the pipeline gives it back.
#[derive(Default)]
struct Observed {
    frames_before: u64,
    frames_after: u64,
    /// `captured_at` going backwards, which §5.5 says cannot happen on this clock.
    monotonic_violations: u64,
    /// Frames whose SOF header disagreed with the negotiated mode.
    wrong_dimensions: u64,
    stall_began: Option<Instant>,
    restart_called: Option<Instant>,
    restart_returned: Option<Instant>,
    first_frame_after_restart: Option<Instant>,
    /// `V4l2Source::stream_rebuilds` after the restart: the B2 rebuild really ran.
    rebuilds_after: u64,
    restart_error: Option<String>,
}

/// A real [`V4l2Source`] that stops handing frames on after `stall_after` of them, until the
/// pipeline restarts it.
///
/// This is the only honest way to exercise the *pipeline's* stall response against hardware. The
/// device will not stall on demand, and the captured video is the target's only feedback channel
/// so the cable cannot be pulled (CLAUDE.md) — but the pipeline cannot tell a consumer that has
/// stopped dequeuing from a device that has stopped producing, which is precisely the condition
/// §6.1's "capture error or stall" row covers. The restart underneath is the genuine article:
/// [`V4l2Source::restart`], `STREAMOFF`, re-prime, `STREAMON`.
struct StallingSource {
    inner: V4l2Source,
    obs: Arc<Mutex<Observed>>,
    stall_after: u64,
    frames: u64,
    stalling: bool,
    healed: bool,
    last_captured: Duration,
}

impl FrameSource for StallingSource {
    fn next_frame(&mut self) -> Result<CompressedFrame, CaptureError> {
        if self.stalling {
            // A stall with no error to explain it: exactly what `restart_after` is for. Paced,
            // so it costs the wall clock a real dequeue timeout costs.
            std::thread::sleep(Duration::from_millis(5));
            return Err(CaptureError::Timeout(Duration::from_millis(5)));
        }
        let frame = self.inner.next_frame()?;
        self.frames += 1;
        {
            let mut obs = lock(&self.obs);
            if self.healed {
                obs.frames_after += 1;
                if obs.first_frame_after_restart.is_none() {
                    obs.first_frame_after_restart = Some(Instant::now());
                }
            } else {
                obs.frames_before += 1;
            }
            if frame.captured_at < self.last_captured {
                obs.monotonic_violations += 1;
            }
            if (frame.width, frame.height) != (WIDTH, HEIGHT) {
                obs.wrong_dimensions += 1;
            }
        }
        self.last_captured = frame.captured_at;
        if !self.healed && self.frames >= self.stall_after {
            self.stalling = true;
            lock(&self.obs).stall_began = Some(Instant::now());
        }
        Ok(frame)
    }

    fn restart(&mut self) -> Result<(), CaptureError> {
        lock(&self.obs).restart_called = Some(Instant::now());
        let result = self.inner.restart();
        let mut obs = lock(&self.obs);
        obs.restart_returned = Some(Instant::now());
        obs.rebuilds_after = self.inner.stream_rebuilds();
        match &result {
            Ok(()) => {
                self.stalling = false;
                self.healed = true;
            }
            Err(e) => obs.restart_error = Some(e.to_string()),
        }
        result
    }

    fn negotiated_dimensions(&self) -> Option<(u32, u32)> {
        self.inner.negotiated_dimensions()
    }

    fn describe(&self) -> String {
        format!("stalling wrapper over {}", self.inner.describe())
    }
}

/// An opener that hands over one [`StallingSource`] and then refuses, so this test exercises the
/// restart path and never the reopen path.
struct StallingOpener {
    obs: Arc<Mutex<Observed>>,
    stall_after: u64,
    given: bool,
}

impl SourceOpener for StallingOpener {
    fn open(&mut self) -> Result<Box<dyn FrameSource>, CaptureError> {
        if self.given {
            return Err(CaptureError::Disconnected);
        }
        self.given = true;
        let inner = V4l2Source::open(Path::new(NODE), WIDTH, HEIGHT, FPS)?;
        Ok(Box::new(StallingSource {
            inner,
            obs: Arc::clone(&self.obs),
            stall_after: self.stall_after,
            frames: 0,
            stalling: false,
            healed: false,
            last_captured: Duration::ZERO,
        }))
    }

    fn describe(&self) -> String {
        format!("stalling opener on {NODE}")
    }
}

/// **H-B1.** A stall on the real device is answered with a real `STREAMOFF`/`STREAMON`, and
/// frames come back (§6.1 "capture error or stall — attempt restart").
///
/// The number this exists to produce is the **cost of a restart**: the wall clock from
/// `restart()` returning to the next frame landing. That is what says whether
/// `restart_after = 2 s` is a sane default for a target that is rebooting for a minute.
#[test]
#[ignore = "needs the NanoKVM-USB dongle on /dev/video4"]
fn hb1_a_stalled_stream_is_restarted_and_frames_come_back() {
    let _device = DEVICE.lock().unwrap_or_else(|e| e.into_inner());
    let obs = Arc::new(Mutex::new(Observed::default()));
    let opener = Box::new(StallingOpener {
        obs: Arc::clone(&obs),
        stall_after: 60,
        given: false,
    });
    let config = PipelineConfig {
        restart_after: Some(Duration::from_millis(200)),
        stall_after: Duration::from_millis(100),
        ..PipelineConfig::default()
    };
    let mappings_before = mappings_of(NODE);
    let h = Pipeline::start_with_opener(opener, config);
    let out = h.output();

    wait_for(
        "60 frames before the stall",
        Duration::from_secs(20),
        || lock(&obs).frames_before >= 60,
    );
    // The renderer's last image, held across the stall (§6.1 S1-2).
    wait_for("a decoded frame", Duration::from_secs(20), || {
        out.is_pending()
    });

    wait_for(
        "the pipeline to notice the stall",
        Duration::from_secs(10),
        || h.state() == PipelineState::Stalled,
    );
    wait_for("the stream restart", Duration::from_secs(10), || {
        h.stats().stream_restarts >= 1
    });
    wait_for(
        "30 frames after the restart",
        Duration::from_secs(20),
        || lock(&obs).frames_after >= 30,
    );
    wait_for("Running again", Duration::from_secs(10), || {
        h.state() == PipelineState::Running
    });

    let st = h.stats();
    let o = lock(&obs);
    let stall_to_restart = o
        .restart_called
        .zip(o.stall_began)
        .map(|(r, s)| r.saturating_duration_since(s));
    let restart_cost = o
        .restart_returned
        .zip(o.restart_called)
        .map(|(done, called)| done.saturating_duration_since(called));
    let restart_to_frame = o
        .first_frame_after_restart
        .zip(o.restart_returned)
        .map(|(f, done)| f.saturating_duration_since(done));

    println!("-- H-B1: pipeline stream restart on {NODE} --");
    println!("  frames before the stall: {}", o.frames_before);
    println!(
        "  stall noticed -> restart() called: {stall_to_restart:?} (restart_after was 200 ms)"
    );
    println!("  restart() call -> return (STREAMOFF, re-prime, STREAMON): {restart_cost:?}");
    println!("  restart() return -> first frame: {restart_to_frame:?}");
    println!("  frames after the restart: {}", o.frames_after);
    println!(
        "  V4l2Source::stream_rebuilds after the restart: {}",
        o.rebuilds_after
    );
    println!(
        "  pipeline: stream_restarts {} disconnects {} reopens {} capture_errors {}",
        st.stream_restarts, st.disconnects, st.reopens, st.capture_errors
    );
    println!("  bytesused: {}", st.bytes);
    println!("  last_resolution: {:?}", st.last_resolution);

    assert_eq!(o.restart_error, None, "the restart itself failed");
    assert_eq!(
        o.monotonic_violations, 0,
        "captured_at went backwards across the restart"
    );
    assert_eq!(
        o.wrong_dimensions, 0,
        "a frame's SOF header disagreed with the negotiated mode"
    );
    assert_eq!(
        o.rebuilds_after, 1,
        "the B2 rebuild did not run exactly once"
    );
    assert_eq!(st.disconnects, 0, "a stall was reported as a disconnection");
    assert_eq!(st.reopens, 0, "a stall caused a reopen");
    assert!(st.stream_restarts >= 1);
    assert_eq!(st.last_resolution, Some((WIDTH, HEIGHT)));
    drop(o);
    h.stop();

    // The arena went back where it came from.
    assert_eq!(
        mappings_of(NODE),
        mappings_before,
        "the restart leaked {NODE} mappings"
    );
}

// ---------------------------------------------------------------------------------------------
// H-B2 — replug of the video device (§6.1 S2-4)
// ---------------------------------------------------------------------------------------------

/// **H-B2.** The kernel takes `/dev/video4` away and gives it back; the pipeline must reopen it
/// on its own, with no leaked buffers and without stopping (§6.1 S2-2/S2-4).
///
/// The replug is performed by `scripts/usb-replug.py` (slice D1), which resolves the dongle
/// through sysfs, refuses to touch anything that is not on its `345f:2133`/`1a86:55d3`/`1a40:0101`
/// allowlist, and exits 0 only once every affected node is back. The method — `reset` or
/// `rebind` — comes from `NANOKVM_REPLUG_METHOD` so the orchestrator can select whichever one
/// actually takes the node away on this hardware: a driver that implements `pre_reset`/
/// `post_reset` can survive a `reset` in place, and if it does, this test says so rather than
/// pretending the recovery was exercised.
///
/// Assertions are all on consequences: a node that went away, a frame that arrived, a mapping
/// count that came back to where it started.
#[test]
#[ignore = "replugs the dongle: needs scripts/usb-replug.py and the orchestrator's go-ahead"]
fn hb2_a_replugged_device_is_reopened_with_no_leaked_buffers() {
    let _device = DEVICE.lock().unwrap_or_else(|e| e.into_inner());
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = root.join("scripts/usb-replug.py");
    assert!(
        script.exists(),
        "{} is missing; H-B2 cannot replug the device without it (slice D1)",
        script.display()
    );
    let method = std::env::var("NANOKVM_REPLUG_METHOD").unwrap_or_else(|_| "reset".to_string());

    // §8, not a name: the replug is exactly when the kernel can hand the node back as something
    // else, so every open here resolves the dongle by USB identity the way `src/main.rs` does.
    // The wrapper records what that resolved to, because the leak check below has to count
    // mappings of the node actually opened rather than of a constant.
    let opened: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let post: Arc<Mutex<PostReopen>> = Arc::new(Mutex::new(PostReopen::default()));
    let opener = Box::new(RecordingOpener {
        inner: DiscoveringOpener::new(
            NodeResolver::real(NodeKind::Video, Constraints::default()),
            WIDTH,
            HEIGHT,
            FPS,
        ),
        opened: Arc::clone(&opened),
        post: Arc::clone(&post),
    });
    let config = PipelineConfig::default();
    let settle = settle_window(&config);
    println!(
        "-- H-B2: replug of the §8-resolved video node with --method {method} --\n  \
         reopen backoff {:?} doubling to {:?}, restart_after {:?}\n  \
         format watchdog: {} mismatched frame(s) and a {:?} grace, at most {} restart(s) per \
         budget, one escalation reopen\n  \
         settle window derived from that config: {settle:?}",
        config.reopen_backoff,
        config.reopen_max_backoff,
        config.restart_after,
        config.format_mismatch_frames,
        config.format_mismatch_grace,
        config.format_mismatch_restart_limit
    );
    let h = Pipeline::start_with_opener(opener, config);
    let out = h.output();

    wait_for(
        "30 frames before the replug",
        Duration::from_secs(20),
        || h.stats().frames_captured >= 30,
    );
    wait_for("a decoded frame", Duration::from_secs(20), || {
        out.is_pending()
    });
    let before = h.stats();
    assert_eq!(before.last_resolution, Some((WIDTH, HEIGHT)));
    let node_before = lock(&opened)
        .last()
        .cloned()
        .expect("the pipeline opened something to have produced those frames");
    println!("  §8 resolved the video node to {node_before} for the first open");
    if node_before != NODE {
        println!(
            "  note: that is not {NODE}, which is what this desk usually calls it (CLAUDE.md). \
             Not a failure — resolving is what makes the name an outcome rather than an input."
        );
    }
    let mappings_before = mappings_of(&node_before);
    assert!(
        mappings_before > 0,
        "no {node_before} mappings while streaming; the leak check would be vacuous"
    );
    println!("  {node_before} mappings while streaming: {mappings_before}");

    // The script prints its own timeline on our stdout; it exits 0 only when the node is back.
    // Through a `ChildGuard`: every assertion below this line unwinds past the `wait()` at the
    // bottom, and a `usb-replug.py` left running unsupervised is the worst thing this file could
    // leak. The most likely failure of the lot — "the node never went away with this method" —
    // is precisely the one that used to leak it.
    let mut replug = child_guard::ChildGuard::spawn(
        "scripts/usb-replug.py",
        Command::new("python3")
            .arg("scripts/usb-replug.py")
            .arg("video")
            .arg("--method")
            .arg(&method)
            .arg("--video")
            .arg(&node_before)
            .current_dir(root),
    )
    .expect("spawning scripts/usb-replug.py");

    // The device goes away. If it does not, say so plainly: that is a finding about the method,
    // not a failure of the recovery under test.
    //
    // Nothing here watches for `PipelineState::Stopped`. Only `stop()` and a failed thread spawn
    // can set it (`pipeline.rs`), neither of which can happen between here and the `stop()` at
    // the bottom, so a poll for it would assert nothing. What actually proves the pipeline did
    // not give up on the device is the 30 frames after the reopen, below.
    let mut errors_during_the_gap: Vec<String> = Vec::new();
    let disconnect_deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < disconnect_deadline && h.stats().disconnects == 0 {
        std::thread::sleep(Duration::from_millis(5));
    }
    let st = h.stats();
    assert!(
        st.disconnects >= 1,
        "{node_before} never went away with --method {method}; the pipeline saw no disconnection. \
         Try the other method, and record that this one leaves the driver bound in place."
    );
    let gone_at = st.disconnected_since.unwrap_or_else(Instant::now);
    println!("  disconnect observed: {:?}", st.last_error);
    assert!(
        matches!(
            st.state,
            PipelineState::Disconnected | PipelineState::Reconnecting
        ) || st.reopens > 0,
        "state during the gap was {:?}",
        st.state
    );

    // Frames must survive the gap: the renderer keeps the last image (§6.1 S1-2).
    assert!(
        out.is_pending(),
        "the last decoded frame was discarded during the gap"
    );

    // The device comes back and the pipeline reopens it, on its own.
    let reopen_deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < reopen_deadline && h.stats().reopens == 0 {
        let st = h.stats();
        if let Some(e) = st.last_error {
            if !errors_during_the_gap.contains(&e) {
                errors_during_the_gap.push(e);
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let st = h.stats();
    assert_eq!(
        st.reopens, 1,
        "the device was not reopened exactly once: {st:?}"
    );

    // **The assertion that would have caught the 2026-09-11 bug.** The reopen can succeed —
    // node rediscovered, `S_FMT` MJPG 1920x1080, `S_PARM` 60 fps, "reopened after N attempt(s)"
    // — and still leave the *stream* in the device's power-on 640x480 with the UVC commit lost,
    // for as long as the client runs. A6 keeps the SOF header as the authority, so the only
    // honest check is on the frames: they must settle at the negotiated size, and they must do
    // it within a bound. The bound is `settle_window(&config)`, derived from the watchdog's own
    // ladder rather than asserted — see that function for the arithmetic. On the 2026-09-11 live
    // run the whole recovery took ~1.2 s of it.
    let settle_deadline = Instant::now() + settle;
    while Instant::now() < settle_deadline && lock(&post).first_match.is_none() {
        std::thread::sleep(Duration::from_millis(5));
    }
    {
        // A fresh snapshot: the watchdog may have fired during the wait above.
        let st = h.stats();
        let p = lock(&post);
        println!(
            "  after the reopen: {} frame(s) at the negotiated {WIDTH}x{HEIGHT}, {} at another \
             size; sizes seen {:?}",
            p.matched, p.mismatched, p.sizes
        );
        println!(
            "  format-mismatch restarts: {} (escalation reopens {}, accepted {})",
            st.format_mismatch_restarts, st.format_mismatch_reopens, st.format_mismatch_accepted
        );
        match (p.first_frame, p.first_match) {
            (Some(first), Some(matched)) => println!(
                "  first post-reopen frame to first {WIDTH}x{HEIGHT} frame: {:.3?}",
                matched.saturating_duration_since(first)
            ),
            _ => println!("  no {WIDTH}x{HEIGHT} frame arrived after the reopen"),
        }
        assert!(
            p.first_match.is_some(),
            "no frame at the negotiated {WIDTH}x{HEIGHT} within {settle:?} of the reopen: {} \
             frame(s) arrived, all at {:?}. The reopen brought the node back but left the stream \
             in the wrong mode, and the format watchdog did not recover it (it made {} \
             restart(s), escalated to {} reopen(s) and accepted the mismatch {} time(s)). The \
             window is the whole watchdog ladder plus margin, so an acceptance inside it means \
             the watchdog has given up rather than that it is still trying.",
            p.mismatched,
            p.sizes,
            st.format_mismatch_restarts,
            st.format_mismatch_reopens,
            st.format_mismatch_accepted
        );
    }
    let settled_at = lock(&post).matched;

    let frames_at_reopen = st.frames_captured;
    wait_for(
        "30 frames after the reopen",
        Duration::from_secs(30),
        || h.stats().frames_captured >= frames_at_reopen + 30,
    );
    let back_at = Instant::now();
    let st = h.stats();

    // Settled means settled: every frame from the first correct one on is correct too. A stream
    // that flickers back into 640x480 is the same failure, more slowly.
    {
        let p = lock(&post);
        let after_settling = p.matched - settled_at;
        println!(
            "  frames since settling: {after_settling} at {WIDTH}x{HEIGHT}, {} mismatched in \
             total over the whole post-reopen window",
            p.mismatched
        );
        assert!(
            after_settling >= 20,
            "only {after_settling} frame(s) at the negotiated size after settling"
        );
        assert_eq!(
            p.sizes.last(),
            Some(&(WIDTH, HEIGHT)),
            "the stream left the negotiated size again after settling; sizes seen {:?}",
            p.sizes
        );
    }

    let status = replug.wait().expect("waiting for scripts/usb-replug.py");
    assert!(
        status.success(),
        "scripts/usb-replug.py exited with {status}; the node did not come back"
    );

    println!(
        "  disconnect -> 30 frames again: {:?}",
        back_at.saturating_duration_since(gone_at)
    );
    println!(
        "  reopen attempts {} for {} reopen(s); disconnects {}",
        st.reopen_attempts, st.reopens, st.disconnects
    );
    println!("  errors seen while the node was away (the shapes the backoff has to tolerate):");
    for e in &errors_during_the_gap {
        println!("    {e}");
    }
    println!("  stream restarts during the gap: {}", st.stream_restarts);
    println!("  bytesused after recovery: {}", st.bytes);

    assert_eq!(
        st.last_resolution,
        Some((WIDTH, HEIGHT)),
        "the frames after the replug are not 1920x1080: {st:?}"
    );
    assert_ne!(
        st.state,
        PipelineState::Stopped,
        "the pipeline stopped instead of recovering: {st:?}"
    );
    assert_eq!(
        st.disconnected_since, None,
        "disconnected_since outlived the reopen: {st:?}"
    );

    // The node the *reopen* landed on, which is not necessarily the one it started on: the kernel
    // hands back the lowest free name, and this test is the one place that can observe it move.
    let node_after = lock(&opened)
        .last()
        .cloned()
        .expect("the reopen opened something");
    let renumbered = node_after != node_before;
    if renumbered {
        println!(
            "  the node was renumbered by the replug: {node_before} -> {node_after}. That is the \
             C6 case this test exists to survive, and it survived it."
        );
    }
    let mappings_after = mappings_of(&node_after);
    println!(
        "  {node_after} mappings after recovery: {mappings_after} (was {mappings_before} on \
         {node_before})"
    );
    assert_eq!(
        mappings_after, mappings_before,
        "the reopen leaked {node_after} mappings: {mappings_before} before, {mappings_after} after"
    );
    // **Both names, whenever they differ.** Comparing one count against the other is blind in
    // exactly the C6 case: an arena still mapped on the old `/dev/videoN` contributes nothing to
    // `mappings_of` for the new name, so a leak of the whole old arena reads as "same count,
    // nothing leaked" and §6.1 S2-2 goes unverified precisely where it is interesting. The old
    // node must hold nothing — it no longer exists, and `release_source()` runs before any
    // backoff (`pipeline.rs`) — and the new one must hold what the pipeline holds, which the
    // assertion above already pins.
    if renumbered {
        let stale = mappings_of(&node_before);
        println!("  {node_before} mappings after recovery: {stale} (must be zero)");
        assert_eq!(
            stale, 0,
            "the reopen left {stale} mapping(s) on {node_before}, the node it moved off: the \
             arena was not returned before the device went away"
        );
    }

    h.stop();
    assert_eq!(
        mappings_of(&node_after),
        0,
        "stopping the pipeline left {node_after} mappings behind"
    );
    if renumbered {
        assert_eq!(
            mappings_of(&node_before),
            0,
            "stopping the pipeline left mappings behind on {node_before}, the node it started on"
        );
    }
}
