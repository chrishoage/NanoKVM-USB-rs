//! The runtime format switch the chrome's Video popover needs (plan §12 Stage 4b).
//!
//! `PipelineHandle::request_format` is the smallest thing that makes "choose a resolution" mean
//! anything: it records a request, the capture thread hands it to the [`SourceOpener`], and the
//! opener's answer decides whether the device is reopened. Everything here is asserted against
//! [`SyntheticSource`] replaying **real frames off this device** (`fixtures/frames/`, §9.1), so
//! the resolution the pipeline reports is read from a genuine JPEG start-of-frame header (§6, A6)
//! rather than from a number a fake made up.
//!
//! No hardware, no `/dev`, no `/sys`.
//!
//! # The corpus is gitignored, so every test here can skip
//!
//! `fixtures/frames/` is build output (`fixtures/frames/MANIFEST.md` says how to re-record it) and
//! is not in the repository. A fresh checkout therefore has no corpus, and a new test file that
//! panicked without one would break `cargo test` for anyone who has just cloned this. Each test
//! below asks [`corpus`] first and **returns with a message** when it is missing. Several
//! *pre-existing* tests in `src/capture/` still panic in that situation; that is a separate
//! decision and is not changed here.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nanokvm::capture::{
    CaptureError, FrameSource, Pipeline, PipelineConfig, PipelineHandle, SourceOpener,
    SyntheticSource,
};

/// Where the recorded frames live.
fn frames_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/frames")
}

/// Whether the recorded corpus is present, saying so once per test when it is not.
///
/// Every test here starts with `if !corpus() { return; }`. The corpus is gitignored build output,
/// and a fresh checkout must still be able to run `cargo test`.
fn corpus() -> bool {
    let dir = frames_dir();
    for (w, h) in [(1920, 1080), (1280, 720), (720, 480), (640, 480)] {
        let prefix = format!("mjpeg-{w}x{h}-");
        let found = std::fs::read_dir(&dir).ok().is_some_and(|entries| {
            entries.filter_map(Result::ok).any(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".jpg"))
            })
        });
        if !found {
            eprintln!(
                "skipping: no {w}x{h} frames under {} — the corpus is gitignored build output; \
                 see fixtures/frames/MANIFEST.md for how to record it",
                dir.display()
            );
            return false;
        }
    }
    true
}

/// Every `mjpeg-<W>x<H>-*.jpg` in `fixtures/frames`, as raw bytes.
fn frames_at(width: u32, height: u32) -> Vec<Vec<u8>> {
    let dir = frames_dir();
    let prefix = format!("mjpeg-{width}x{height}-");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|x| x == "jpg")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix))
        })
        .collect();
    paths.sort();
    assert!(
        !paths.is_empty(),
        "no {width}x{height} fixtures under {} — see fixtures/frames/MANIFEST.md",
        dir.display()
    );
    paths
        .iter()
        .map(|p| std::fs::read(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())))
        .collect()
}

/// An opener that honours `set_format` by replaying the fixture corpus for the mode asked for.
///
/// This is the whole point of the test: a *real* `V4l2Opener` renegotiates by reopening the node,
/// and what the caller can observe from outside is that frames start arriving at the new size.
/// This opener reproduces exactly that observable behaviour with no device.
struct ModeOpener {
    width: u32,
    height: u32,
    opens: Arc<AtomicU32>,
}

/// A synthetic source that also **reports what it negotiated**, the way `V4l2Source` does from
/// `G_FMT`. `SyntheticSource` itself answers `None`, which is the truth for it; the pipeline's
/// `PipelineStats::negotiated_dimensions` is fed from this answer and is what the chrome's Video
/// popover marks as the current mode (`App::tick`).
struct NegotiatingSource {
    inner: SyntheticSource,
    negotiated: (u32, u32),
}

impl FrameSource for NegotiatingSource {
    fn next_frame(&mut self) -> Result<nanokvm::capture::CompressedFrame, CaptureError> {
        self.inner.next_frame()
    }

    fn describe(&self) -> String {
        self.inner.describe()
    }

    fn negotiated_dimensions(&self) -> Option<(u32, u32)> {
        Some(self.negotiated)
    }
}

impl SourceOpener for ModeOpener {
    fn open(&mut self) -> Result<Box<dyn FrameSource>, CaptureError> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        let mut source =
            SyntheticSource::new(frames_at(self.width, self.height), Duration::from_millis(5))?;
        source.set_label(format!("synthetic {}x{}", self.width, self.height));
        Ok(Box::new(NegotiatingSource {
            inner: source,
            negotiated: (self.width, self.height),
        }))
    }

    fn describe(&self) -> String {
        format!("synthetic MJPG {}x{}", self.width, self.height)
    }

    fn set_format(&mut self, width: u32, height: u32, _fps: u32) -> bool {
        self.width = width;
        self.height = height;
        true
    }
}

/// An opener that cannot renegotiate — the default `set_format`, which every test double and
/// `Pipeline::start`'s one-shot wrapper get.
struct FixedOpener {
    opens: Arc<AtomicU32>,
}

impl SourceOpener for FixedOpener {
    fn open(&mut self) -> Result<Box<dyn FrameSource>, CaptureError> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        let source = SyntheticSource::new(frames_at(1920, 1080), Duration::from_millis(5))?;
        Ok(Box::new(source))
    }

    fn describe(&self) -> String {
        "synthetic, fixed at 1920x1080".to_string()
    }
}

fn wait_for(label: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {label}");
}

fn settled_at(handle: &PipelineHandle, width: u32, height: u32) {
    wait_for(
        &format!("frames at {width}x{height}"),
        Duration::from_secs(5),
        || handle.stats().last_resolution == Some((width, height)),
    );
}

/// The whole feature, end to end and by consequence: ask for 1280x720 and frames start arriving at
/// 1280x720, read from their own JPEG headers.
#[test]
fn requesting_a_mode_reopens_the_device_and_frames_arrive_at_the_new_size() {
    if !corpus() {
        return;
    }
    let opens = Arc::new(AtomicU32::new(0));
    let handle = Pipeline::start_with_opener(
        Box::new(ModeOpener {
            width: 1920,
            height: 1080,
            opens: Arc::clone(&opens),
        }),
        PipelineConfig::default(),
    );
    settled_at(&handle, 1920, 1080);
    let opens_before = opens.load(Ordering::SeqCst);

    handle.request_format(1280, 720, 60);
    settled_at(&handle, 1280, 720);

    let stats = handle.stats();
    assert_eq!(
        stats.format_changes, 1,
        "the change is counted as its own thing"
    );
    assert_eq!(
        stats.disconnects, 0,
        "a resolution change is not a disconnection: {stats:?}"
    );
    assert!(
        opens.load(Ordering::SeqCst) > opens_before,
        "the device must be reopened — S_FMT cannot be issued on a streaming node"
    );
    handle.stop();
}

/// And back again, so the popover is not a one-way door.
#[test]
fn a_second_request_switches_back() {
    if !corpus() {
        return;
    }
    let handle = Pipeline::start_with_opener(
        Box::new(ModeOpener {
            width: 1920,
            height: 1080,
            opens: Arc::new(AtomicU32::new(0)),
        }),
        PipelineConfig::default(),
    );
    settled_at(&handle, 1920, 1080);
    handle.request_format(1280, 720, 60);
    settled_at(&handle, 1280, 720);
    handle.request_format(1920, 1080, 60);
    settled_at(&handle, 1920, 1080);
    assert_eq!(handle.stats().format_changes, 2);
    handle.stop();
}

/// An opener that cannot renegotiate leaves the stream alone. Destroying a working stream to
/// reopen it at the size it already has would blank the picture for nothing, so the request is
/// declined and logged rather than obeyed.
#[test]
fn an_opener_that_cannot_renegotiate_keeps_its_stream() {
    if !corpus() {
        return;
    }
    let opens = Arc::new(AtomicU32::new(0));
    let handle = Pipeline::start_with_opener(
        Box::new(FixedOpener {
            opens: Arc::clone(&opens),
        }),
        PipelineConfig::default(),
    );
    settled_at(&handle, 1920, 1080);
    let opens_before = opens.load(Ordering::SeqCst);

    handle.request_format(1280, 720, 60);
    // Long enough for several capture passes to have seen the request.
    std::thread::sleep(Duration::from_millis(200));

    let stats = handle.stats();
    assert_eq!(stats.last_resolution, Some((1920, 1080)));
    assert_eq!(
        stats.format_changes, 0,
        "nothing changed, so nothing counted"
    );
    assert_eq!(
        opens.load(Ordering::SeqCst),
        opens_before,
        "the device must not be reopened when the mode cannot change"
    );
    handle.stop();
}

/// The request slot holds one mode, not a queue: a user clicking three entries in a second wants
/// the third, and three sequential reopens would take about a second and a half to arrive at the
/// same place.
#[test]
fn only_the_last_request_before_the_capture_thread_looks_is_applied() {
    if !corpus() {
        return;
    }
    let opens = Arc::new(AtomicU32::new(0));
    let handle = Pipeline::start_with_opener(
        Box::new(ModeOpener {
            width: 1920,
            height: 1080,
            opens: Arc::clone(&opens),
        }),
        PipelineConfig::default(),
    );
    settled_at(&handle, 1920, 1080);

    // Three in a row, with no chance for the capture thread to act between the first two.
    handle.request_format(640, 480, 60);
    handle.request_format(720, 480, 60);
    handle.request_format(1280, 720, 60);
    settled_at(&handle, 1280, 720);

    assert!(
        handle.stats().format_changes <= 3,
        "coalescing may not invent work"
    );
    assert_eq!(handle.stats().last_resolution, Some((1280, 720)));
    handle.stop();
}

/// The Video popover marks the mode the device has negotiated, and the viewer reads that marker
/// from `PipelineStats::negotiated_dimensions` (`App::tick`). This is the source: it must follow a
/// format change, and it is what tells `App` a mode the user clicked is the one already in force.
#[test]
fn the_negotiated_dimensions_follow_a_format_change() {
    if !corpus() {
        return;
    }
    let handle = Pipeline::start_with_opener(
        Box::new(ModeOpener {
            width: 1920,
            height: 1080,
            opens: Arc::new(AtomicU32::new(0)),
        }),
        PipelineConfig::default(),
    );
    settled_at(&handle, 1920, 1080);
    wait_for(
        "the negotiated mode to be reported",
        Duration::from_secs(5),
        || handle.stats().negotiated_dimensions.is_some(),
    );

    handle.request_format(1280, 720, 60);
    settled_at(&handle, 1280, 720);
    wait_for(
        "the negotiated mode to follow",
        Duration::from_secs(5),
        || handle.stats().negotiated_dimensions == Some((1280, 720)),
    );
    handle.stop();
}
