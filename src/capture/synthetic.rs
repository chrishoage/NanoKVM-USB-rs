//! The no-hardware [`FrameSource`] (§4, §9.3).
//!
//! §9.3: "a synthetic `FrameSource` covers decode and render and makes latency and
//! drop-behaviour assertions deterministic". This one replays real captured frames, so the
//! decode path under test is the same one the device exercises.
//!
//! It deliberately **mimics an mmap ring**: each frame is written into one internal buffer that
//! is reused and overwritten, and the caller is handed a copy. That is the only way to make
//! §9.2 item 8 — "no pending frame references a requeued mmap buffer" — testable without
//! hardware. A source that handed out a fresh `Vec` each time would pass that test vacuously.

use std::path::Path;
use std::time::{Duration, Instant};

use super::v4l2::now_monotonic;
use super::{jpeg, CaptureError, CompressedFrame, FrameSource, MAX_HEIGHT, MAX_WIDTH};

/// A scripted failure: after `after` frames have been delivered, the next call fails once.
struct ScriptedFailure {
    after: usize,
    error: CaptureError,
}

/// Replays a list of JPEGs as a [`FrameSource`] (§9.3).
pub struct SyntheticSource {
    /// The source material. Never handed out directly.
    frames: Vec<Vec<u8>>,
    /// The stand-in for an mmap buffer: overwritten on every call, never handed out.
    ring: Vec<u8>,
    interval: Duration,
    next_due: Option<Instant>,
    cursor: usize,
    delivered: usize,
    sequence: u32,
    failures: Vec<ScriptedFailure>,
    label: String,
}

impl SyntheticSource {
    /// A source replaying `frames` in order, looping, one every `interval`.
    ///
    /// `interval` of [`Duration::ZERO`] means "as fast as [`FrameSource::next_frame`] is
    /// called", which is what the drop-before-decode test needs (§5.2).
    ///
    /// # Errors
    ///
    /// [`CaptureError::Config`] if `frames` is empty.
    pub fn new(frames: Vec<Vec<u8>>, interval: Duration) -> Result<Self, CaptureError> {
        if frames.is_empty() {
            return Err(CaptureError::Config(
                "synthetic source has no frames".into(),
            ));
        }
        let capacity = frames.iter().map(Vec::len).max().unwrap_or(0);
        Ok(SyntheticSource {
            frames,
            ring: Vec::with_capacity(capacity),
            interval,
            next_due: None,
            cursor: 0,
            delivered: 0,
            sequence: 0,
            failures: Vec::new(),
            label: "synthetic (in-memory frames)".to_string(),
        })
    }

    /// A source replaying every `.jpg` in `dir`, sorted by name. Used with
    /// `fixtures/frames/absrange`, which holds real 1920x1080 frames off this device (§9.1).
    ///
    /// # Errors
    ///
    /// [`CaptureError::Config`] if the directory cannot be read or holds no `.jpg` files,
    /// [`CaptureError::Io`] if a file cannot be read.
    pub fn from_dir(dir: &Path, interval: Duration) -> Result<Self, CaptureError> {
        let mut paths: Vec<_> = std::fs::read_dir(dir)
            .map_err(|e| CaptureError::Config(format!("{}: {e}", dir.display())))?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jpg"))
            .collect();
        paths.sort();
        let mut frames = Vec::with_capacity(paths.len());
        for p in &paths {
            frames.push(std::fs::read(p).map_err(CaptureError::Io)?);
        }
        let mut s = Self::new(frames, interval)?;
        s.label = format!("synthetic ({}, {} frames)", dir.display(), paths.len());
        Ok(s)
    }

    /// Script a failure: after `n` frames have been delivered, the next call returns `error`
    /// once, and the call after that resumes normal delivery.
    ///
    /// A failed call does not count as a delivery, so calling this three times with the same `n`
    /// produces three consecutive failures — which is how §6.1 S1-2's "frames stop and later
    /// resume" is exercised without hardware.
    ///
    /// **A scripted failure is paced like a frame**: the call sleeps out its interval before
    /// failing, exactly as a real dequeue timeout consumes wall clock. Without that, three
    /// scripted 50 ms timeouts elapse in microseconds and the pipeline never actually stalls,
    /// so a test claiming to exercise a stall exercises nothing.
    ///
    /// Entries are consumed in the order they are scheduled.
    pub fn fail_after(&mut self, n: usize, error: CaptureError) {
        self.failures.push(ScriptedFailure { after: n, error });
    }

    /// Change the inter-frame interval.
    pub fn set_interval(&mut self, interval: Duration) {
        self.interval = interval;
        self.next_due = None;
    }

    /// Override the string returned by [`FrameSource::describe`].
    pub fn set_label(&mut self, label: impl Into<String>) {
        self.label = label.into();
    }

    /// How many frames have been handed out.
    pub fn delivered(&self) -> usize {
        self.delivered
    }

    /// The frames this source replays, for a test that needs to know what it should be seeing.
    pub fn source_frames(&self) -> &[Vec<u8>] {
        &self.frames
    }

    /// Sleep until this frame is due, if a non-zero interval was configured.
    fn pace(&mut self) {
        if self.interval.is_zero() {
            return;
        }
        let now = Instant::now();
        let due = self.next_due.unwrap_or(now);
        if due > now {
            std::thread::sleep(due - now);
        }
        // Schedule from the due time so the nominal rate does not drift, but never let a
        // backlog build up if the caller was slow.
        let next = due + self.interval;
        let now = Instant::now();
        self.next_due = Some(if next < now {
            now + self.interval
        } else {
            next
        });
    }
}

impl FrameSource for SyntheticSource {
    /// Hand out the next frame, or the next scripted failure.
    ///
    /// The frame is written into the internal ring buffer and a **copy** is returned, so the
    /// returned frame stays valid and unchanged no matter how many times the ring is
    /// subsequently overwritten (§5.2, §9.2 item 8).
    fn next_frame(&mut self) -> Result<CompressedFrame, CaptureError> {
        // Pace first, then fail: a scripted failure has to cost the wall clock a real dequeue
        // failure costs, or it cannot produce a stall. See `fail_after`.
        let scripted = self.failures.iter().position(|f| f.after == self.delivered);
        self.pace();
        if let Some(pos) = scripted {
            return Err(self.failures.remove(pos).error);
        }

        // Overwrite the ring, exactly as the driver overwrites a requeued mmap buffer.
        let src = &self.frames[self.cursor % self.frames.len()];
        self.ring.clear();
        self.ring.extend_from_slice(src);
        self.cursor = self.cursor.wrapping_add(1);

        // A6: dimensions from this frame's own header, the same rule the real source follows.
        let (width, height) = jpeg::dimensions(&self.ring)
            .map_err(|e| CaptureError::BadFrame(format!("synthetic frame {}: {e}", self.cursor)))?;
        // And the same sanity ceiling the real source applies, so a corrupt-SOF frame can be
        // replayed through the whole pipeline in a test and be refused where it would be
        // refused on hardware.
        if !super::dimensions_in_range(width, height) {
            return Err(CaptureError::BadFrame(format!(
                "synthetic frame {}: header claims {width}x{height}, over the \
                 {MAX_WIDTH}x{MAX_HEIGHT} ceiling",
                self.cursor
            )));
        }

        // The copy. This is what makes the returned frame own its bytes.
        let jpeg_bytes = self.ring.clone();

        let frame = CompressedFrame {
            jpeg: jpeg_bytes,
            width,
            height,
            // An **absolute** `CLOCK_MONOTONIC` stamp, because that is what `CompressedFrame`
            // documents and what the renderer subtracts from `now_monotonic()` to get a
            // capture-to-submit age (§5.5). A relative duration since some epoch of this
            // source's own would make every such age wrong by the process uptime. One interval
            // back from now stands in for the transfer time a real buffer's start-of-exposure
            // stamp is already behind by.
            captured_at: now_monotonic().saturating_sub(self.interval),
            sequence: self.sequence,
        };
        self.sequence = self.sequence.wrapping_add(1);
        self.delivered += 1;
        Ok(frame)
    }

    fn describe(&self) -> String {
        self.label.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::testsupport::{fixture_bytes, fixture_paths, patch_sof};

    fn two_distinct_fixtures() -> Vec<Vec<u8>> {
        let paths = fixture_paths("absrange");
        let a = fixture_bytes(&paths[0]);
        let b = fixture_bytes(
            paths
                .iter()
                .find(|p| fixture_bytes(p) != a)
                .expect("two distinct fixtures"),
        );
        vec![a, b]
    }

    #[test]
    fn frames_replay_in_order_and_loop() {
        let frames = two_distinct_fixtures();
        let mut s = SyntheticSource::new(frames.clone(), Duration::ZERO).expect("source");
        for i in 0..5u32 {
            let f = s.next_frame().expect("frame");
            assert_eq!(f.sequence, i);
            assert_eq!(f.jpeg, frames[i as usize % 2]);
            assert_eq!((f.width, f.height), (1920, 1080));
        }
        assert_eq!(s.delivered(), 5);
    }

    /// §9.2 item 8, directly: the failure this guards is silent corruption, not a crash.
    #[test]
    fn a_delivered_frame_survives_the_ring_being_overwritten() {
        let frames = two_distinct_fixtures();
        assert_ne!(
            frames[0], frames[1],
            "the test needs distinguishable frames"
        );
        let mut s = SyntheticSource::new(frames.clone(), Duration::ZERO).expect("source");

        let held = s.next_frame().expect("frame");
        let before = held.jpeg.clone();
        assert_eq!(before, frames[0]);

        // Overwrite the internal buffer many times with different content.
        for _ in 0..8 {
            s.next_frame().expect("frame");
        }

        assert_eq!(held.jpeg, before, "the held frame's bytes changed");
        assert_eq!(held.jpeg, frames[0], "the held frame is no longer frame 0");
        assert_eq!(held.sequence, 0);
    }

    #[test]
    fn a_scripted_failure_fires_once_and_delivery_resumes() {
        let mut s = SyntheticSource::new(two_distinct_fixtures(), Duration::ZERO).expect("source");
        s.fail_after(2, CaptureError::Timeout(Duration::from_millis(250)));

        assert!(s.next_frame().is_ok());
        assert!(s.next_frame().is_ok());
        assert!(matches!(s.next_frame(), Err(CaptureError::Timeout(_))));
        let resumed = s.next_frame().expect("delivery resumes");
        assert_eq!(resumed.sequence, 2, "a failure must not consume a sequence");
        assert_eq!(s.delivered(), 3);
    }

    #[test]
    fn repeated_scripts_at_the_same_point_fire_consecutively() {
        let mut s = SyntheticSource::new(two_distinct_fixtures(), Duration::ZERO).expect("source");
        for _ in 0..3 {
            s.fail_after(1, CaptureError::Timeout(Duration::from_millis(10)));
        }
        assert!(s.next_frame().is_ok());
        for i in 0..3 {
            assert!(
                matches!(s.next_frame(), Err(CaptureError::Timeout(_))),
                "failure {i} did not fire"
            );
        }
        assert!(s.next_frame().is_ok());
    }

    #[test]
    fn a_disconnect_can_be_scripted() {
        let mut s = SyntheticSource::new(two_distinct_fixtures(), Duration::ZERO).expect("source");
        s.fail_after(0, CaptureError::Disconnected);
        assert!(matches!(s.next_frame(), Err(CaptureError::Disconnected)));
    }

    #[test]
    fn an_empty_frame_list_is_refused() {
        assert!(matches!(
            SyntheticSource::new(Vec::new(), Duration::ZERO),
            Err(CaptureError::Config(_))
        ));
    }

    #[test]
    fn a_non_jpeg_frame_is_a_bad_frame() {
        let mut s =
            SyntheticSource::new(vec![b"not a jpeg".to_vec()], Duration::ZERO).expect("source");
        assert!(matches!(s.next_frame(), Err(CaptureError::BadFrame(_))));
    }

    #[test]
    fn pacing_holds_the_nominal_interval() {
        let mut s = SyntheticSource::new(two_distinct_fixtures(), Duration::from_millis(20))
            .expect("source");
        let t = Instant::now();
        for _ in 0..4 {
            s.next_frame().expect("frame");
        }
        // Four frames, the first due immediately: at least three intervals must have passed.
        assert!(
            t.elapsed() >= Duration::from_millis(55),
            "paced source ran too fast: {:?}",
            t.elapsed()
        );
    }

    #[test]
    fn captured_at_is_monotonic_non_decreasing() {
        let mut s = SyntheticSource::new(two_distinct_fixtures(), Duration::ZERO).expect("source");
        let mut last = Duration::ZERO;
        for _ in 0..10 {
            let f = s.next_frame().expect("frame");
            assert!(f.captured_at >= last);
            last = f.captured_at;
        }
    }

    /// `CompressedFrame::captured_at` is documented as an **absolute** `CLOCK_MONOTONIC` stamp,
    /// directly subtractable from [`now_monotonic`] (§5.5) — that subtraction is what
    /// `viewer::render` reports as capture-to-submit age. A relative duration since this
    /// source's own construction would make every such age wrong by the process uptime, so the
    /// clock is asserted rather than assumed.
    #[test]
    fn captured_at_is_on_the_documented_clock_not_a_relative_duration() {
        let mut s = SyntheticSource::new(two_distinct_fixtures(), Duration::from_millis(5))
            .expect("source");
        for _ in 0..3 {
            let f = s.next_frame().expect("frame");
            let age = now_monotonic().saturating_sub(f.captured_at);
            assert!(
                age < Duration::from_secs(1),
                "captured_at is not on CLOCK_MONOTONIC: age came out as {age:?}"
            );
            assert!(
                f.captured_at > Duration::ZERO,
                "captured_at is zero, so it is not an absolute stamp"
            );
        }
    }

    /// A scripted failure must cost wall clock, or "three 50 ms timeouts" is three microseconds
    /// and nothing ever stalls. This is the property the pipeline's stall test rests on.
    #[test]
    fn scripted_failures_are_paced_like_frames() {
        let interval = Duration::from_millis(20);
        let mut s = SyntheticSource::new(two_distinct_fixtures(), interval).expect("source");
        s.next_frame().expect("frame");
        for _ in 0..3 {
            s.fail_after(1, CaptureError::Timeout(Duration::from_millis(50)));
        }
        let t = Instant::now();
        for i in 0..3 {
            assert!(
                matches!(s.next_frame(), Err(CaptureError::Timeout(_))),
                "failure {i} did not fire"
            );
        }
        let gap = t.elapsed();
        assert!(
            gap >= interval * 2,
            "three scripted failures took {gap:?}; they are not paced"
        );
    }

    /// The sanity ceiling, at the source rather than at the decoder: a frame whose SOF has been
    /// corrupted into claiming a size the device cannot produce is an unusable frame (§6, A6),
    /// counted as such, and never handed on.
    #[test]
    fn a_header_over_the_ceiling_is_a_bad_frame() {
        let mut bytes = fixture_bytes(&fixture_paths("absrange")[0]);
        patch_sof(&mut bytes, 4096, 4096);
        let mut s = SyntheticSource::new(vec![bytes], Duration::ZERO).expect("source");
        let r = s.next_frame();
        assert!(
            matches!(&r, Err(CaptureError::BadFrame(m)) if m.contains("ceiling")),
            "{r:?}"
        );
        assert_eq!(s.delivered(), 0, "the oversized frame counted as delivered");
    }

    #[test]
    fn from_dir_loads_the_fixture_corpus() {
        let dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/frames/absrange");
        let mut s = SyntheticSource::from_dir(&dir, Duration::ZERO).expect("source");
        assert!(s.source_frames().len() >= 10);
        assert!(s.describe().contains("absrange"));
        let f = s.next_frame().expect("frame");
        assert_eq!((f.width, f.height), (1920, 1080));
    }
}
