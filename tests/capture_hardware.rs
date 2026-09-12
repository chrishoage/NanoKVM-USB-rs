//! Capture tests for the real NanoKVM-USB video device (345f:2133).
//!
//! The recorded path is `/dev/video4`; verify identity before running. Never open the
//! user's unrelated bus-5 devices. Run with the hardware feature, ignored tests enabled,
//! and one test thread. These tests stream video without sending target input.

#![cfg(feature = "hardware")]

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

/// The V4L2 node admits one streaming opener at a time, and the test harness runs tests in
/// parallel by default, so every test in this binary holds this lock while it owns the device.
static DEVICE: Mutex<()> = Mutex::new(());

use nanokvm::capture::v4l2::now_monotonic;
use nanokvm::capture::{jpeg, FrameSource, V4l2Source};

/// The dongle's video node on the recorded test setup (CLAUDE.md).
const NODE: &str = "/dev/video4";
/// The primary path: the target outputs 1080p, so 4K would cost 3.9x for an upscale.
const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const FPS: u32 = 60;
const FRAMES: usize = 120;

#[test]
#[ignore = "needs the NanoKVM-USB dongle on /dev/video4"]
fn capture_120_frames_at_1080p60() {
    let _device = DEVICE.lock().unwrap_or_else(|e| e.into_inner());
    let mut src = V4l2Source::open(Path::new(NODE), WIDTH, HEIGHT, FPS).expect("open /dev/video4");
    println!("source: {}", src.describe());
    println!("negotiated: {:?}", src.negotiated_format());

    let mut ages_ms: Vec<f64> = Vec::with_capacity(FRAMES);
    let mut last_captured = Duration::ZERO;
    let mut first_captured = Duration::ZERO;
    let mut sequences: Vec<u32> = Vec::with_capacity(FRAMES);

    for i in 0..FRAMES {
        let frame = src
            .next_frame()
            .unwrap_or_else(|e| panic!("frame {i}: {e}"));
        let now = now_monotonic();

        // the frame's own start-of-frame header is the authority. Re-parse it here rather
        // than trusting the source's own parse.
        let (w, h) = jpeg::dimensions(&frame.jpeg).unwrap_or_else(|e| panic!("frame {i}: {e}"));
        assert_eq!((w, h), (WIDTH, HEIGHT), "frame {i} header dimensions");
        assert_eq!((frame.width, frame.height), (w, h), "frame {i}");
        assert_eq!(frame.jpeg.len() as u64, u64::from(src.bytes_stats().last));

        // Driver timestamps use the monotonic clock; this measures dequeue age only.
        assert!(
            frame.captured_at >= last_captured,
            "frame {i}: captured_at went backwards, {:?} after {:?}",
            frame.captured_at,
            last_captured
        );
        if i == 0 {
            first_captured = frame.captured_at;
        }
        last_captured = frame.captured_at;

        let age = now.saturating_sub(frame.captured_at);
        ages_ms.push(age.as_secs_f64() * 1e3);
        sequences.push(frame.sequence);
    }

    let span = last_captured - first_captured;
    let fps = (FRAMES as f64 - 1.0) / span.as_secs_f64();

    let mut sorted = ages_ms.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let age_min = sorted[0];
    let age_p50 = sorted[sorted.len() / 2];
    let age_max = sorted[sorted.len() - 1];

    let bytes = src.bytes_stats();
    println!("-- {FRAMES} frames from {NODE} at {WIDTH}x{HEIGHT} --");
    println!(
        "  buffer flags: {:#010x} (expect TIMESTAMP_MONOTONIC|TSTAMP_SRC_SOE = 0x00012000 bits)",
        u32::from(src.last_flags())
    );
    println!("  rate from buffer timestamps: {fps:.2} fps over {span:.3?}");
    println!("  capture-to-dequeue age (NOT latency, ): min {age_min:.2} ms  p50 {age_p50:.2} ms  max {age_max:.2} ms");
    println!(
        "  bytesused: min {} max {} mean {:.0} last {} (n={})",
        bytes.min,
        bytes.max,
        bytes.mean(),
        bytes.last,
        bytes.count
    );
    println!(
        "  sequence: first {} last {} (span {})",
        sequences[0],
        sequences[FRAMES - 1],
        sequences[FRAMES - 1] - sequences[0]
    );

    assert!(
        (55.0..=61.0).contains(&fps),
        "measured {fps:.2} fps, expected 55..=61 after S_PARM (A7)"
    );
    assert!(
        age_max < 40.0,
        "capture-to-dequeue age reached {age_max:.2} ms, over the 40 ms bound"
    );
    assert_eq!(bytes.count, FRAMES as u64);
    assert!(bytes.min > 0);
}

/// A stall must be detectable rather than a hang. With a timeout shorter than the
/// frame period, `next_frame` must return `Timeout` and the source must keep working
/// afterwards — the v4l crate's own timeout facility corrupts the buffer ring here, which is
/// why this module polls the fd itself (see `capture::v4l2` module docs).
#[test]
#[ignore = "needs the NanoKVM-USB dongle on /dev/video4"]
fn a_short_timeout_returns_and_the_stream_recovers() {
    let _device = DEVICE.lock().unwrap_or_else(|e| e.into_inner());
    use nanokvm::capture::CaptureError;

    let mut src = V4l2Source::open(Path::new(NODE), WIDTH, HEIGHT, FPS).expect("open /dev/video4");
    // Drain one frame so the ring is in steady state.
    src.next_frame().expect("first frame");

    // 0 ms: poll returns immediately whether or not a buffer is ready, so at least one of these
    // must time out at 60 fps.
    src.set_timeout(Duration::from_millis(0));
    let mut timeouts = 0;
    for _ in 0..50 {
        match src.next_frame() {
            Err(CaptureError::Timeout(_)) => timeouts += 1,
            Err(e) => panic!("unexpected error: {e}"),
            Ok(_) => {}
        }
    }
    assert!(timeouts > 0, "a zero timeout never fired");
    println!("  {timeouts}/50 zero-length polls timed out, as expected");

    // The stream must still work afterwards. This is the assertion that matters: the v4l
    // crate's own `set_timeout` leaves the ring in a state where every later dequeue fails.
    src.set_timeout(Duration::from_millis(500));
    for i in 0..30 {
        let f = src
            .next_frame()
            .unwrap_or_else(|e| panic!("frame {i} after the timeouts: {e}"));
        assert_eq!((f.width, f.height), (WIDTH, HEIGHT));
    }
    println!("  30 frames delivered after {timeouts} timeouts; the ring survived");
}

/// A failed `DQBUF` can leave v4l’s arena index pointing at a driver-owned buffer.
/// Rebuilding the stream must recover from that state instead of repeating `EINVAL`.
#[test]
#[ignore = "needs the NanoKVM-USB dongle on /dev/video4"]
fn a_dropped_stream_is_rebuilt_and_frames_flow_again() {
    let _device = DEVICE.lock().unwrap_or_else(|e| e.into_inner());
    let mut src = V4l2Source::open(Path::new(NODE), WIDTH, HEIGHT, FPS).expect("open /dev/video4");
    assert_eq!(src.stream_rebuilds(), 0);
    assert!(
        src.buffers() >= 2,
        "REQBUFS granted {} buffers",
        src.buffers()
    );
    println!("  REQBUFS granted {} buffers", src.buffers());

    // Frames before.
    let before = src.next_frame().expect("a frame before the rebuild");
    assert_eq!((before.width, before.height), (WIDTH, HEIGHT));
    let seq_before = before.sequence;

    src.force_rebuild_for_test();

    // Frames after. The first call does the rebuild; nothing else changes.
    let mut delivered = 0;
    for i in 0..30 {
        let f = src
            .next_frame()
            .unwrap_or_else(|e| panic!("frame {i} after the rebuild: {e}"));
        assert_eq!((f.width, f.height), (WIDTH, HEIGHT), "frame {i}");
        assert!(!f.jpeg.is_empty());
        delivered += 1;
    }
    assert_eq!(delivered, 30);
    assert_eq!(
        src.stream_rebuilds(),
        1,
        "the rebuild was not counted, or happened more than once"
    );
    assert!(
        src.buffers() >= 2,
        "the rebuilt stream got {} buffers",
        src.buffers()
    );
    println!(
        "  30 frames delivered after the rebuild (sequence {seq_before} -> {}); \
         stream_rebuilds = {}",
        src.bytes_stats().count,
        src.stream_rebuilds()
    );
    println!("  bytesused after the rebuild: {}", src.bytes_stats());
}
