//! Capture/decode integration with synthetic sources and real JPEG fixtures.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nanokvm::capture::{
    CaptureError, CompressedFrame, DecodedFrame, FrameSource, Pipeline, PipelineConfig,
    PipelineState, Slot, SyntheticSource, MAX_HEIGHT, MAX_WIDTH,
};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/frames/absrange")
}

/// Two fixture frames whose bytes differ, so "the held frame did not change" is a real
/// assertion rather than a tautology.
fn two_distinct_fixtures() -> (Vec<u8>, Vec<u8>) {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(fixture_dir())
        .expect("fixture dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jpg"))
        .collect();
    paths.sort();
    let a = std::fs::read(&paths[0]).expect("fixture");
    let b = paths
        .iter()
        .map(|p| std::fs::read(p).expect("fixture"))
        .find(|b| *b != a)
        .expect("two distinct fixtures");
    (a, b)
}

fn synthetic(interval: Duration) -> SyntheticSource {
    SyntheticSource::from_dir(&fixture_dir(), interval).expect("synthetic source")
}

/// Corrupt the SOF dimensions without breaking JPEG marker framing.
fn patch_sof(bytes: &mut [u8], height: u16, width: u16) {
    let mut at = 2usize; // past SOI
    loop {
        assert_eq!(bytes[at], 0xFF, "lost the marker stream at {at}");
        let marker = bytes[at + 1];
        let len = u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]) as usize;
        let is_sof =
            (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC;
        if is_sof {
            bytes[at + 5..at + 7].copy_from_slice(&height.to_be_bytes());
            bytes[at + 7..at + 9].copy_from_slice(&width.to_be_bytes());
            return;
        }
        at += 2 + len;
    }
}

/// Poll `cond` until it holds. Panics on the deadline so a hang is a test failure rather than a
/// hung suite.
fn wait_for(label: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Take from the output slot, waiting for a frame to appear.
fn wait_frame(out: &Arc<Slot<DecodedFrame>>, timeout: Duration) -> DecodedFrame {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(f) = out.wait_take(Duration::from_millis(20)) {
            return f;
        }
        assert!(
            Instant::now() < deadline,
            "no decoded frame within {timeout:?}"
        );
    }
}

fn checksum(bytes: &[u8]) -> u64 {
    // FNV-1a. Cheap, and sensitive to a single changed byte anywhere in 8 MB.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Use JPEG header dimensions so negotiated metadata cannot mask a wrong-sized frame.
#[test]
fn frames_flow_end_to_end_at_the_captured_resolution() {
    let h = Pipeline::start(
        Box::new(synthetic(Duration::from_millis(5))),
        PipelineConfig::default(),
    );
    let out = h.output();

    let first = wait_frame(&out, Duration::from_secs(10));
    assert_eq!((first.width, first.height), (1920, 1080));
    assert_eq!(first.rgba.len(), 1920 * 1080 * 4);

    // Several frames, each newer than the last.
    let mut last_seq = first.sequence;
    for _ in 0..3 {
        let f = wait_frame(&out, Duration::from_secs(10));
        assert_eq!((f.width, f.height), (1920, 1080));
        assert!(
            f.sequence > last_seq,
            "sequence went backwards: {} after {last_seq}",
            f.sequence
        );
        assert!(f.captured_at > Duration::ZERO);
        last_seq = f.sequence;
    }

    let stats = h.stats();
    assert!(stats.frames_captured >= 4);
    assert!(stats.frames_decoded >= 4);
    assert_eq!(stats.decode_errors, 0);
    assert_eq!(stats.state, PipelineState::Running);
    assert!(stats.bytes.count >= 4, "bytesused was not instrumented");
    assert!(stats.last_captured_at.is_some());
    assert!(h.frames_stalled_for().is_some());
    h.stop();
}

/// The source reuses one backing buffer. A queued frame must own its bytes before
/// the next capture overwrites that buffer.
#[test]
fn a_taken_frame_is_unaffected_by_the_source_overwriting_its_buffer() {
    let (a, b) = two_distinct_fixtures();
    assert_ne!(
        a, b,
        "the fixtures must differ for this test to mean anything"
    );

    let source = SyntheticSource::new(vec![a, b], Duration::from_millis(2)).expect("source");
    let h = Pipeline::start(Box::new(source), PipelineConfig::default());
    let out = h.output();

    let held = wait_frame(&out, Duration::from_secs(10));
    let held_sum = checksum(&held.rgba);
    let held_seq = held.sequence;
    let captured_when_taken = h.stats().frames_captured;

    // Let the source overwrite its internal buffer many times, alternating between two frames
    // whose pixels differ.
    wait_for(
        "the ring to be overwritten repeatedly",
        Duration::from_secs(10),
        || h.stats().frames_captured >= captured_when_taken + 8,
    );

    assert_eq!(
        checksum(&held.rgba),
        held_sum,
        "the held frame's pixels changed after the source reused its buffer"
    );
    assert_eq!(held.sequence, held_seq);
    assert_eq!(held.rgba.len(), 1920 * 1080 * 4);

    // And prove the two fixtures really do decode to different pixels, so the checksum above
    // could have detected corruption.
    let mut other = wait_frame(&out, Duration::from_secs(10));
    let deadline = Instant::now() + Duration::from_secs(10);
    while checksum(&other.rgba) == held_sum {
        assert!(
            Instant::now() < deadline,
            "never saw a frame with different pixels; the corpus may be uniform"
        );
        other = wait_frame(&out, Duration::from_secs(10));
    }
    h.stop();
}

/// Slow decoding must discard obsolete compressed frames before spending work on them.
#[test]
fn a_slow_decoder_drops_pre_decode_and_never_delivers_an_older_frame() {
    let config = PipelineConfig {
        decode_delay: Some(Duration::from_millis(15)),
        ..PipelineConfig::default()
    };
    // Source ~1000 fps against a ~66 fps decoder: pre-decode drops are structural, not a race.
    let h = Pipeline::start(Box::new(synthetic(Duration::from_millis(1))), config);
    let out = h.output();

    let mut seqs: Vec<u32> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while seqs.len() < 8 {
        assert!(Instant::now() < deadline, "decoder never produced 8 frames");
        if let Some(f) = out.wait_take(Duration::from_millis(50)) {
            if let Some(&last) = seqs.last() {
                assert!(
                    f.sequence > last,
                    "the output slot handed back sequence {} after {last}",
                    f.sequence
                );
            }
            seqs.push(f.sequence);
        }
    }

    let stats = h.stats();
    assert!(
        stats.dropped_pre_decode > 0,
        "no frames were dropped pre-decode: {stats:?}"
    );
    assert!(
        stats.dropped_pre_decode > stats.frames_decoded,
        "with a 15x slower decoder most frames should be dropped pre-decode: {stats:?}"
    );
    // The renderer here is eager, so post-decode drops stay bounded by the number of decodes;
    // they must not track the source rate.
    assert!(
        stats.dropped_post_decode <= stats.frames_decoded,
        "post-decode drops outgrew decodes: {stats:?}"
    );
    assert!(
        stats.dropped_post_decode < stats.dropped_pre_decode,
        "{stats:?}"
    );

    // The last frame delivered is the newest decoded one, never an older one.
    let last_delivered = *seqs.last().expect("frames");
    assert_eq!(
        seqs.iter().copied().max(),
        Some(last_delivered),
        "an older frame was delivered after a newer one"
    );
    h.stop();
}

/// Paced dequeue timeouts simulate a stalled source without a hot error loop.
/// The pipeline must report the stall and resume delivery when frames return.
#[test]
fn the_capture_thread_survives_timeouts_reports_the_stall_and_resumes() {
    let interval = Duration::from_millis(40);
    let stall_after = Duration::from_millis(50);
    let mut source = synthetic(interval);
    for _ in 0..3 {
        source.fail_after(2, CaptureError::Timeout(Duration::from_millis(50)));
    }
    let config = PipelineConfig {
        stall_after,
        ..PipelineConfig::default()
    };
    let h = Pipeline::start(Box::new(source), config);
    let out = h.output();

    // The stall is visible as itself while the failures are in flight, and `frames_stalled_for`
    // agrees with the state rather than contradicting it.
    wait_for(
        "the pipeline to report Stalled",
        Duration::from_secs(10),
        || h.state() == PipelineState::Stalled,
    );
    let stalled_for = h
        .frames_stalled_for()
        .expect("two frames arrived before the failures");
    assert!(
        stalled_for >= stall_after,
        "state says Stalled but frames_stalled_for is only {stalled_for:?}"
    );
    assert_ne!(
        h.state(),
        PipelineState::Disconnected,
        "a timeout must never be reported as a disconnection "
    );

    wait_for(
        "the three scripted timeouts",
        Duration::from_secs(10),
        || h.stats().capture_errors >= 3,
    );

    // Delivery resumes: frames keep arriving after the failures.
    let after = h.stats().frames_captured;
    wait_for("frames after the stall", Duration::from_secs(10), || {
        h.stats().frames_captured >= after + 3
    });
    let f = wait_frame(&out, Duration::from_secs(10));
    assert_eq!((f.width, f.height), (1920, 1080));

    let stats = h.stats();
    assert_eq!(stats.capture_errors, 3, "extra capture errors: {stats:?}");
    assert!(matches!(
        stats.state,
        PipelineState::Running | PipelineState::Stalled
    ));
    assert!(stats.frames_decoded > 0);

    // Both threads are still alive: stop joins them rather than finding them gone.
    let t = Instant::now();
    h.stop();
    assert!(t.elapsed() < Duration::from_secs(2));
}

/// Disconnection must remain distinguishable from a stall, with bounded shutdown.
#[test]
fn a_disconnection_is_reported_and_stop_does_not_hang() {
    let mut source = synthetic(Duration::from_millis(2));
    source.fail_after(3, CaptureError::Disconnected);
    let h = Pipeline::start(Box::new(source), PipelineConfig::default());

    wait_for("disconnected state", Duration::from_secs(10), || {
        h.state() == PipelineState::Disconnected
    });
    let stats = h.stats();
    assert_eq!(stats.frames_captured, 3);
    assert_eq!(stats.state, PipelineState::Disconnected);
    assert!(stats
        .last_error
        .as_deref()
        .is_some_and(|e| e.contains("disconnected")));

    let t = Instant::now();
    h.stop();
    let elapsed = t.elapsed();
    assert!(elapsed < Duration::from_secs(1), "stop took {elapsed:?}");
}

/// (6) A healthy pipeline stops promptly.
#[test]
fn stop_joins_both_threads_within_a_second() {
    let h = Pipeline::start(
        Box::new(synthetic(Duration::from_millis(2))),
        PipelineConfig::default(),
    );
    wait_for("frames", Duration::from_secs(10), || {
        h.stats().frames_decoded >= 2
    });
    let t = Instant::now();
    h.stop();
    let elapsed = t.elapsed();
    assert!(elapsed < Duration::from_secs(1), "stop took {elapsed:?}");
}

/// Repeated immediate errors must neither kill the pipeline nor spin at CPU speed.
/// Sticky `POLLERR` and temporary `EIO` can produce this failure pattern.
#[test]
fn a_source_that_only_errors_is_retried_at_a_bounded_rate_and_stays_stalled_not_dead() {
    struct AlwaysBad(Arc<AtomicU64>);
    impl FrameSource for AlwaysBad {
        fn next_frame(&mut self) -> Result<CompressedFrame, CaptureError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Err(CaptureError::Io(std::io::Error::from_raw_os_error(
                libc::EIO,
            )))
        }
        fn describe(&self) -> String {
            "always-EIO".to_string()
        }
    }

    let calls = Arc::new(AtomicU64::new(0));
    let config = PipelineConfig {
        stall_after: Duration::from_millis(20),
        ..PipelineConfig::default()
    };
    let h = Pipeline::start(Box::new(AlwaysBad(Arc::clone(&calls))), config);
    wait_for("stalled", Duration::from_secs(10), || {
        h.state() == PipelineState::Stalled
    });

    // Measure the retry rate over a fixed window rather than over the whole test.
    let before = calls.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(200));
    let during = calls.load(Ordering::Relaxed) - before;
    let stats = h.stats();
    assert!(
        during < 200,
        "{during} next_frame calls in 200 ms: the error backoff is gone"
    );
    assert!(
        during >= 2,
        "the source was barely retried at all: {during}"
    );
    assert!(stats.capture_errors > 5, "EIO was not counted");
    assert_eq!(stats.state, PipelineState::Stalled);
    assert!(h.frames_stalled_for().is_none());
    let t = Instant::now();
    h.stop();
    assert!(t.elapsed() < Duration::from_secs(1));
}

/// An unparseable frame stream is the other instant-failure shape: `V4l2Source` returns
/// `BadFrame` after a *successful*, and therefore prompt, dequeue. Same bound.
#[test]
fn an_unparseable_frame_stream_is_also_retried_at_a_bounded_rate() {
    struct BadFrames(Arc<AtomicU64>);
    impl FrameSource for BadFrames {
        fn next_frame(&mut self) -> Result<CompressedFrame, CaptureError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Err(CaptureError::BadFrame("no SOF".into()))
        }
        fn describe(&self) -> String {
            "bad-frames".to_string()
        }
    }

    let calls = Arc::new(AtomicU64::new(0));
    let h = Pipeline::start(
        Box::new(BadFrames(Arc::clone(&calls))),
        PipelineConfig::default(),
    );
    std::thread::sleep(Duration::from_millis(200));
    let n = calls.load(Ordering::Relaxed);
    let stats = h.stats();
    h.stop();
    assert!(n < 200, "{n} BadFrame retries in 200 ms");
    assert_eq!(stats.capture_errors, n, "not every BadFrame was counted");
    assert_ne!(stats.state, PipelineState::Disconnected);
}

/// Reviewer blocker 1, end to end. A frame whose SOF has been corrupted into claiming
/// 4096x4096 used to decode successfully, allocate 64 MiB and reach the renderer as a valid
/// frame with `decode_errors == 0`. It must now be refused at the source, counted, and leave
/// the honest frames around it flowing.
#[test]
fn a_corrupt_sof_frame_never_reaches_the_renderer_and_is_counted() {
    let (good, other) = two_distinct_fixtures();
    let mut bad = good.clone();
    patch_sof(&mut bad, 4096, 4096);

    let source =
        SyntheticSource::new(vec![good, bad, other], Duration::from_millis(2)).expect("source");
    let h = Pipeline::start(Box::new(source), PipelineConfig::default());
    let out = h.output();

    wait_for(
        "the corrupt frames to be refused and good ones to keep flowing",
        Duration::from_secs(10),
        || {
            let s = h.stats();
            s.capture_errors >= 2 && s.frames_decoded >= 4
        },
    );

    // Everything the renderer is handed is a real 1080p frame, never the 4096x4096 lie.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut seen = 0;
    while Instant::now() < deadline && seen < 6 {
        if let Some(f) = out.wait_take(Duration::from_millis(20)) {
            assert_eq!(
                (f.width, f.height),
                (1920, 1080),
                "a {}x{} frame reached the renderer",
                f.width,
                f.height
            );
            assert!(f.rgba.len() <= (MAX_WIDTH as usize) * (MAX_HEIGHT as usize) * 4);
            seen += 1;
        }
    }
    assert!(seen > 0, "no frames were delivered at all");

    let stats = h.stats();
    h.stop();
    assert!(
        stats
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("ceiling")),
        "the refusal was not reported: {:?}",
        stats.last_error
    );
    assert_ne!(stats.state, PipelineState::Disconnected);
    assert!(stats.frames_decoded >= 4, "good frames stopped flowing");
    assert!(
        stats.capture_errors >= 2,
        "the corrupt frames were not counted: {stats:?}"
    );
    assert_eq!(
        stats.decode_errors, 0,
        "the ceiling is enforced at the source, so the decoder never saw the frame: {stats:?}"
    );
}

/// `stop` must be prompt from every state the pipeline can be in, not just the healthy one.
#[test]
fn stop_is_prompt_from_every_state() {
    /// A source that takes `0` to answer, then reports a timeout.
    struct SlowSource(Duration);
    impl FrameSource for SlowSource {
        fn next_frame(&mut self) -> Result<CompressedFrame, CaptureError> {
            std::thread::sleep(self.0);
            Err(CaptureError::Timeout(self.0))
        }
        fn describe(&self) -> String {
            "slow".to_string()
        }
    }

    // (a) Immediately after start, before any frame exists.
    let h = Pipeline::start(
        Box::new(synthetic(Duration::from_millis(200))),
        PipelineConfig::default(),
    );
    let t = Instant::now();
    h.stop();
    assert!(
        t.elapsed() < Duration::from_secs(1),
        "fresh: {:?}",
        t.elapsed()
    );

    // (b) Both threads parked: decode in a 30 s wait_take, capture in a long source call.
    let h = Pipeline::start(
        Box::new(SlowSource(Duration::from_millis(700))),
        PipelineConfig {
            decode_wait: Duration::from_secs(30),
            ..PipelineConfig::default()
        },
    );
    std::thread::sleep(Duration::from_millis(50));
    let t = Instant::now();
    h.stop();
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "parked: {:?}",
        t.elapsed()
    );

    // (c) After a disconnection, with the capture thread already gone.
    let mut s = synthetic(Duration::from_millis(1));
    s.fail_after(2, CaptureError::Disconnected);
    let h = Pipeline::start(Box::new(s), PipelineConfig::default());
    wait_for("disconnect", Duration::from_secs(10), || {
        h.state() == PipelineState::Disconnected
    });
    let t = Instant::now();
    h.stop();
    assert!(t.elapsed() < Duration::from_secs(1), "post-disconnect");

    // (d) Dropped rather than stopped, with a decode_wait nobody would ever wait out.
    let h = Pipeline::start(
        Box::new(SlowSource(Duration::from_millis(30))),
        PipelineConfig {
            decode_wait: Duration::from_secs(300),
            ..PipelineConfig::default()
        },
    );
    let t = Instant::now();
    drop(h);
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "drop with a huge decode_wait: {:?}",
        t.elapsed()
    );
}

/// Start and stop repeatedly, so a shutdown race shows up as a hang or a panicked join rather
/// than as an occasional mystery in production.
#[test]
fn start_and_stop_repeatedly_without_a_race() {
    for i in 0..100 {
        let h = Pipeline::start(
            Box::new(synthetic(Duration::ZERO)),
            PipelineConfig {
                decode_wait: Duration::from_millis(1),
                ..PipelineConfig::default()
            },
        );
        if i % 3 == 0 {
            let _ = h.output().take();
        }
        let t = Instant::now();
        if i % 2 == 0 {
            h.stop();
        } else {
            drop(h);
        }
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "round {i}: shutdown took {:?}",
            t.elapsed()
        );
    }
}

/// Closing the handoff discards pending output. The renderer retains its own
/// already-uploaded texture independently.
#[test]
fn stopping_the_pipeline_discards_the_slots_pending_frame() {
    let h = Pipeline::start(
        Box::new(synthetic(Duration::from_millis(2))),
        PipelineConfig::default(),
    );
    let out = h.output();
    wait_for("a pending decoded frame", Duration::from_secs(10), || {
        out.is_pending()
    });
    h.stop();
    assert!(out.is_closed(), "the slot was not closed by stop");
    assert!(
        !out.is_pending(),
        "the pending frame survived stop; if that is now intended, update this test"
    );
}

/// Strict JPEG errors discard one frame without terminating the decode worker.
#[test]
fn a_truncated_frame_among_good_ones_is_skipped_not_fatal() {
    let (good, other) = two_distinct_fixtures();
    let truncated = good[..good.len() * 6 / 10].to_vec();
    let source = SyntheticSource::new(
        vec![good.clone(), truncated, other],
        Duration::from_millis(2),
    )
    .expect("source");
    let h = Pipeline::start(Box::new(source), PipelineConfig::default());

    wait_for(
        "a decode error and later decodes",
        Duration::from_secs(10),
        || {
            let s = h.stats();
            s.decode_errors >= 2 && s.frames_decoded >= 4
        },
    );
    assert_ne!(h.state(), PipelineState::Disconnected);
    h.stop();
}
