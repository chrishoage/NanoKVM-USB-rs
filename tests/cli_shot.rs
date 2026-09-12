//! Screenshot frame-selection policy and file/stdout output with injectable sources.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use nanokvm::capture::{jpeg, CaptureError, CompressedFrame, FrameSource, SyntheticSource};
use nanokvm::cli::shot::{pick_frame, Shot, ShotError, ShotPolicy, ShotTarget};

/// A path that cannot exist, as in `tests/viewer_startup.rs`, which also guards that it does not.
const ABSENT: &str = "/dev/nanokvm-does-not-exist";

/// The negotiated size every mismatch test compares against: the mode this client asks for.
const NEGOTIATED: (u32, u32) = (1920, 1080);

/// The size stuck stream delivered.
const STUCK: (u32, u32) = (640, 480);

// ---- fixtures and doubles --------------------------------------------------------------------

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/frames/absrange")
}

/// `n` fixture frames, all different from each other, so "the shot is frame 4" is a real
/// assertion rather than a tautology.
fn distinct_fixtures(n: usize) -> Vec<Vec<u8>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(fixture_dir())
        .expect("fixture dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jpg"))
        .collect();
    paths.sort();
    let mut out: Vec<Vec<u8>> = Vec::new();
    for p in &paths {
        let bytes = std::fs::read(p).expect("fixture");
        if !out.contains(&bytes) {
            out.push(bytes);
        }
        if out.len() == n {
            return out;
        }
    }
    panic!("only {} distinct fixture frames, wanted {n}", out.len());
}

fn one_fixture() -> Vec<u8> {
    distinct_fixtures(1).pop().expect("one fixture")
}

/// Rewrite a real frame's start-of-frame Y/X fields, as `tests/capture_pipeline.rs` does. The
/// result is a well-formed JPEG that lies about its size — which is what stuck
/// stream and stale frames look like from here.
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

/// A fixture frame whose header claims `(width, height)`.
fn frame_claiming((width, height): (u32, u32)) -> Vec<u8> {
    let mut bytes = one_fixture();
    patch_sof(&mut bytes, height as u16, width as u16);
    bytes
}

/// A source that negotiated a size and counts its restarts.
///
/// `SyntheticSource::negotiated_dimensions` is `None` — the truth for a source that negotiated
/// nothing — which leaves the size comparison inert, as it leaves the pipeline's
/// watchdog inert. This is the other half of comparison: what `S_FMT` committed to.
struct Negotiated<S: FrameSource> {
    inner: S,
    negotiated: Option<(u32, u32)>,
    restarts: u32,
}

impl<S: FrameSource> Negotiated<S> {
    fn new(inner: S, negotiated: (u32, u32)) -> Self {
        Negotiated {
            inner,
            negotiated: Some(negotiated),
            restarts: 0,
        }
    }
}

impl<S: FrameSource> FrameSource for Negotiated<S> {
    fn next_frame(&mut self) -> Result<CompressedFrame, CaptureError> {
        self.inner.next_frame()
    }

    fn describe(&self) -> String {
        format!("{} negotiated {:?}", self.inner.describe(), self.negotiated)
    }

    fn restart(&mut self) -> Result<(), CaptureError> {
        self.restarts += 1;
        self.inner.restart()
    }

    fn negotiated_dimensions(&self) -> Option<(u32, u32)> {
        self.negotiated
    }
}

/// A source that hands out what a test scripted, then times out for ever.
///
/// `SyntheticSource` refuses bytes it cannot parse before they reach the caller, which is right
/// for it and useless for testing what `pick_frame` does with a frame that arrives *and* is
/// unusable. This one delivers whatever bytes it is given, with the dimensions fields left as the
/// source found them — so a test can also show that `pick_frame` reads the header itself
/// rather than trusting those fields.
struct Scripted {
    queue: VecDeque<Result<Vec<u8>, CaptureError>>,
    calls: u32,
    sequence: u32,
}

impl Scripted {
    fn new(queue: Vec<Result<Vec<u8>, CaptureError>>) -> Self {
        Scripted {
            queue: queue.into(),
            calls: 0,
            sequence: 0,
        }
    }

    /// Nothing but timeouts, for the deadline test.
    fn always_timing_out() -> Self {
        Scripted::new(Vec::new())
    }
}

impl FrameSource for Scripted {
    fn next_frame(&mut self) -> Result<CompressedFrame, CaptureError> {
        self.calls += 1;
        let bytes = match self.queue.pop_front() {
            Some(Ok(bytes)) => bytes,
            Some(Err(e)) => return Err(e),
            None => return Err(CaptureError::Timeout(Duration::from_millis(250))),
        };
        let (width, height) = jpeg::dimensions(&bytes).unwrap_or((0, 0));
        let frame = CompressedFrame {
            jpeg: bytes,
            width,
            height,
            captured_at: Duration::ZERO,
            sequence: self.sequence,
        };
        self.sequence += 1;
        Ok(frame)
    }

    fn describe(&self) -> String {
        "scripted".to_string()
    }
}

/// A source that takes its time, so a test can put a frame's *arrival* on the far side of the
/// deadline. Nothing else here blocks: every other source answers instantly.
struct Delayed<S: FrameSource> {
    inner: S,
    delay: Duration,
}

impl<S: FrameSource> FrameSource for Delayed<S> {
    fn next_frame(&mut self) -> Result<CompressedFrame, CaptureError> {
        std::thread::sleep(self.delay);
        self.inner.next_frame()
    }

    fn describe(&self) -> String {
        format!("{} delayed by {:?}", self.inner.describe(), self.delay)
    }
}

/// A policy that looks at every frame, so a count in an assertion is the count of the thing the
/// test is about and not of `--skip`.
fn policy(skip: u32) -> ShotPolicy {
    ShotPolicy {
        skip,
        ..ShotPolicy::default()
    }
}

/// Every test bounds its own wait. Two seconds is far beyond any of these — the sources are all
/// instant — and short enough that a hang is a failure rather than a stuck suite.
fn soon() -> Instant {
    Instant::now() + Duration::from_secs(2)
}

/// A directory under `target/` this test binary owns, removed when the value drops.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Scratch {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "shot-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Scratch(dir)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---- the policy ------------------------------------------------------------------------------

/// the device emits up to eight frames at the *previous* resolution after an idle period, and
/// a stream that has just started is that case. The skipped frames must be gone, not merely
/// counted — so the shot's bytes are compared with the frame that should have been taken.
#[test]
fn the_skipped_frames_are_discarded_and_the_next_one_is_the_shot() {
    let frames = distinct_fixtures(10);
    let mut source = SyntheticSource::new(frames.clone(), Duration::ZERO).expect("source");
    let shot = pick_frame(&mut source, &policy(3), soon()).expect("a shot");
    assert_eq!(shot.jpeg, frames[3], "the shot is not the fourth frame");
    assert_eq!(shot.skipped, 3);
    assert_eq!(shot.mismatched, 0);
    assert!(!shot.restarted);
    assert_eq!((shot.width, shot.height), NEGOTIATED);
}

/// A truncated transition frame must be skipped rather than saved as a screenshot.
#[test]
fn a_truncated_frame_is_skipped_and_counted() {
    let frames = distinct_fixtures(2);
    let truncated = frames[0][..frames[0].len() - 2].to_vec();
    let mut source =
        SyntheticSource::new(vec![truncated, frames[1].clone()], Duration::ZERO).expect("source");

    let shot = pick_frame(&mut source, &policy(0), soon()).expect("a shot");
    assert_eq!(shot.jpeg, frames[1], "the truncated frame was kept");
    assert_eq!(shot.skipped, 1, "the truncated frame was not counted");
}

/// A frame that arrives and cannot be read is the frame's problem, not the run's.
#[test]
fn a_frame_whose_header_cannot_be_read_is_skipped_and_counted() {
    let good = one_fixture();
    // An EOI marker alone is insufficient: the frame also needs a valid SOF header.
    let headerless = vec![0xFF, 0xD8, 0xFF, 0xD9];
    let mut source = Scripted::new(vec![
        Ok(b"garbage".to_vec()),
        Ok(headerless),
        Ok(good.clone()),
    ]);

    let shot = pick_frame(&mut source, &policy(0), soon()).expect("a shot");
    assert_eq!(shot.jpeg, good);
    assert_eq!(shot.skipped, 2, "both unusable frames must be counted");
    assert_eq!((shot.width, shot.height), NEGOTIATED);
}

/// A short wrong-size burst should settle without restarting the stream.
#[test]
fn stale_size_frames_are_counted_and_the_first_frame_at_the_negotiated_size_is_the_shot() {
    let good = one_fixture();
    let mut frames = vec![frame_claiming(STUCK); 8];
    frames.push(good.clone());
    let synthetic = SyntheticSource::new(frames, Duration::ZERO).expect("source");
    let mut source = Negotiated::new(synthetic, NEGOTIATED);

    let shot = pick_frame(&mut source, &policy(0), soon()).expect("a shot");
    assert_eq!(shot.jpeg, good);
    assert_eq!(shot.mismatched, 8);
    assert!(!shot.restarted, "eight stale frames must not buy a restart");
    assert_eq!(source.restarts, 0);
    assert_eq!((shot.width, shot.height), NEGOTIATED);
}

/// the stream is stuck, not the device, and a fresh `STREAMON` re-commits the format. Once —
/// a restart costs a round trip and a handful of frames, and a shot that has to do it twice has
/// already answered the question.
#[test]
fn a_run_of_mismatched_frames_buys_exactly_one_restart_and_then_takes_the_next_good_frame() {
    let good = one_fixture();
    let mut frames = vec![frame_claiming(STUCK); 31];
    frames.push(good.clone());
    let synthetic = SyntheticSource::new(frames, Duration::ZERO).expect("source");
    let mut source = Negotiated::new(synthetic, NEGOTIATED);

    let shot = pick_frame(&mut source, &policy(0), soon()).expect("a shot");
    assert_eq!(shot.jpeg, good);
    assert_eq!(shot.mismatched, 31);
    assert!(shot.restarted, "the restart is not reported");
    assert_eq!(
        source.restarts, 1,
        "the stream was restarted {} times",
        source.restarts
    );
}

/// And when the restart does not fix it, the error says so rather than writing a screenshot at a
/// size nobody asked for. the frame's own header the authority, so `--any-size` is a
/// real answer and the message has to offer it.
#[test]
fn a_stream_that_never_returns_to_the_negotiated_size_fails_naming_both_sizes() {
    let synthetic =
        SyntheticSource::new(vec![frame_claiming(STUCK)], Duration::ZERO).expect("source");
    let mut source = Negotiated::new(synthetic, NEGOTIATED);

    let error = pick_frame(&mut source, &policy(0), soon()).expect_err("a wrong-size failure");
    let message = error.to_string();
    assert!(
        matches!(error, ShotError::WrongSize { mismatched: 90, .. }),
        "{error:?}"
    );
    assert!(message.contains("640x480"), "{message}");
    assert!(message.contains("1920x1080"), "{message}");
    assert!(message.contains("--any-size"), "{message}");
    assert_eq!(source.restarts, 1, "the restart must not be repeated");
}

/// `--any-size`: the header is the truth about the frame, so a user who wants that frame
/// is entitled to it — and the stream is then left alone entirely.
#[test]
fn any_size_takes_the_first_well_formed_frame_whatever_its_header_says() {
    let stale = frame_claiming(STUCK);
    let synthetic = SyntheticSource::new(vec![stale.clone()], Duration::ZERO).expect("source");
    let mut source = Negotiated::new(synthetic, NEGOTIATED);
    let policy = ShotPolicy {
        skip: 0,
        any_size: true,
        ..ShotPolicy::default()
    };

    let shot = pick_frame(&mut source, &policy, soon()).expect("a shot");
    assert_eq!(shot.jpeg, stale);
    assert_eq!((shot.width, shot.height), STUCK);
    assert_eq!(shot.mismatched, 0);
    assert!(!shot.restarted);
    assert_eq!(source.restarts, 0, "--any-size must not restart the stream");
}

/// A stalled source must remain retryable, but the overall deadline must bound the command.
#[test]
fn a_source_that_only_times_out_fails_at_the_deadline() {
    let mut source = Scripted::always_timing_out();
    let wait = Duration::from_millis(100);
    let started = Instant::now();

    let error = pick_frame(&mut source, &policy(0), started + wait).expect_err("a timeout");
    let elapsed = started.elapsed();

    assert!(matches!(error, ShotError::Timeout { .. }), "{error:?}");
    assert!(
        elapsed < wait + Duration::from_millis(200),
        "the deadline did not bound the wait: {elapsed:?}"
    );
    assert!(source.calls > 0, "the source was never asked");
}

/// R2: `--skip` is a budget of frames to look at and throw away (stale ones). A frame the
/// source refused never arrived, so it cannot have been one of them — sharing a counter made three
/// bad frames spend three of the eight skips, and the shot was then a frame the policy had been
/// told to discard.
#[test]
fn a_frame_the_source_refused_does_not_spend_the_skip_budget() {
    let frames = distinct_fixtures(10);
    let mut queue: Vec<Result<Vec<u8>, CaptureError>> = (0..3)
        .map(|i| Err(CaptureError::BadFrame(format!("short buffer {i}"))))
        .collect();
    queue.extend(frames.iter().cloned().map(Ok));
    let mut source = Scripted::new(queue);

    let shot = pick_frame(&mut source, &policy(8), soon()).expect("a shot");
    assert_eq!(
        shot.jpeg, frames[8],
        "the shot is not the ninth delivered frame, so the bad frames ate the budget"
    );
    assert_eq!(shot.skipped, 8, "exactly the --skip budget was skipped");
    assert_eq!(shot.discarded, 3, "the refused frames are reported too");
}

/// R8: the clock ends the *next* attempt, not the one in hand. The check used to come first and
/// never again, so a deadline already past — `--timeout 0`, or a caller that spent its budget
/// negotiating the format — returned a timeout without asking the source at all, and the one
/// good frame waiting for it was never looked at. Asking once and judging what comes back is what
/// the deadline was already worth: the real bound has always been the deadline plus the source's
/// dequeue timeout.
#[test]
fn a_frame_is_examined_even_when_the_deadline_has_already_passed() {
    let good = one_fixture();
    let late = Duration::from_millis(150);
    let mut source = Delayed {
        inner: Scripted::new(vec![Ok(good.clone())]),
        delay: late,
    };

    let started = Instant::now();
    let shot = pick_frame(&mut source, &policy(0), started).expect("the frame in hand is examined");
    assert_eq!(shot.jpeg, good);
    assert!(
        started.elapsed() >= late,
        "the frame arrived before the deadline after all, so this proves nothing"
    );
}

/// And it is *one* frame past the deadline: a source that keeps producing unusable frames
/// must not be asked for ever. The clock is consulted after every judgement.
#[test]
fn a_frame_that_is_no_good_ends_the_attempt_at_the_deadline() {
    let mut source = Scripted::new(vec![Ok(b"not a jpeg".to_vec()), Ok(one_fixture())]);
    let error = pick_frame(&mut source, &policy(0), Instant::now()).expect_err("a timeout");
    assert!(matches!(error, ShotError::Timeout { .. }), "{error:?}");
    assert_eq!(
        source.calls, 1,
        "the source was asked again after the deadline"
    );
}

/// A disconnection is fatal here: rediscovery is for a session that keeps running, and
/// a screenshot has nothing to keep running for. Asking a gone node again is the thing not to do.
#[test]
fn a_disconnected_source_fails_at_once_and_is_not_asked_again() {
    let mut source = Scripted::new(vec![Err(CaptureError::Disconnected)]);

    let error = pick_frame(&mut source, &policy(0), soon()).expect_err("a disconnection");
    assert!(matches!(error, ShotError::Disconnected { .. }), "{error:?}");
    assert_eq!(
        source.calls, 1,
        "the source was asked after it disconnected"
    );
}

// ---- what gets written -----------------------------------------------------------------------

fn shot_from(bytes: Vec<u8>) -> Shot {
    let (width, height) = jpeg::dimensions(&bytes).expect("fixture header");
    Shot {
        jpeg: bytes,
        width,
        height,
        skipped: 4,
        discarded: 0,
        mismatched: 0,
        restarted: false,
    }
}

/// A `.jpg` shot is the device's own bytes. Not a re-encode: the file has to be the frame whose
/// SOF header the reported size came from, or the size and the pixels are two different claims.
#[test]
fn a_jpeg_shot_is_the_device_bytes_byte_for_byte() {
    let scratch = Scratch::new("jpeg");
    let path = scratch.join("frame.jpg");
    let shot = shot_from(one_fixture());

    let target = ShotTarget::parse(Some(&path), std::time::SystemTime::now()).expect("target");
    let written = target.write(&shot).expect("write");

    assert_eq!(written, shot.jpeg.len());
    assert_eq!(std::fs::read(&path).expect("read back"), shot.jpeg);
    // The atomic write leaves nothing behind.
    let leftovers: Vec<_> = std::fs::read_dir(&scratch.0)
        .expect("scratch")
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .filter(|n| n.to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temporary left behind: {leftovers:?}");
}

/// A `.png` shot goes through the pipeline's own decoder, so it is the
/// same frame the viewer would have shown, at the size the frame's own header declares.
#[test]
fn a_png_shot_decodes_to_the_header_size_in_eight_bit_rgba() {
    let scratch = Scratch::new("png");
    let path = scratch.join("frame.png");
    let shot = shot_from(one_fixture());
    let (want_w, want_h) = (shot.width, shot.height);

    let target = ShotTarget::parse(Some(&path), std::time::SystemTime::now()).expect("target");
    let written = target.write(&shot).expect("write");
    assert!(written > 0);

    let file = std::io::BufReader::new(std::fs::File::open(&path).expect("open the png"));
    let decoder = png::Decoder::new(file);
    let mut reader = decoder.read_info().expect("png header");
    let mut buf = vec![0; reader.output_buffer_size().expect("png buffer size")];
    let info = reader.next_frame(&mut buf).expect("png pixels");

    assert_eq!((info.width, info.height), (want_w, want_h));
    assert_eq!(info.color_type, png::ColorType::Rgba);
    assert_eq!(info.bit_depth, png::BitDepth::Eight);
    assert_eq!(
        info.buffer_size(),
        want_w as usize * want_h as usize * 4,
        "the PNG is not the whole frame"
    );
}

/// An extension nobody handles is a usage error, named before anything is opened or written — a
/// `screen.bmp` that quietly contained a JPEG would be worse than a refusal.
#[test]
fn an_unknown_extension_is_refused_and_names_the_two_formats() {
    let scratch = Scratch::new("ext");
    let path = scratch.join("frame.bmp");

    let error = ShotTarget::parse(Some(&path), std::time::SystemTime::now())
        .expect_err("an unknown extension");
    let message = error.to_string();
    assert!(message.contains(".jpg"), "{message}");
    assert!(message.contains(".png"), "{message}");
    assert!(!path.exists(), "a file was created for a refused format");
}

#[test]
fn a_path_with_no_extension_is_refused_and_a_dash_is_stdout() {
    assert!(ShotTarget::parse(Some(Path::new("frame")), std::time::SystemTime::now()).is_err());
    assert_eq!(
        ShotTarget::parse(Some(Path::new("-")), std::time::SystemTime::now()).expect("stdout"),
        ShotTarget::Stdout
    );
    assert_eq!(ShotTarget::Stdout.label(), "stdout");
}

/// The default name has to be sortable and to say when the shot was taken; both are the reason it
/// is UTC and fixed-width.
#[test]
fn the_default_target_is_a_timestamped_jpeg_in_the_working_directory() {
    let now = std::time::UNIX_EPOCH + Duration::from_secs(1_789_134_307);
    let target = ShotTarget::parse(None, now).expect("default target");
    assert_eq!(
        target,
        ShotTarget::Jpeg(PathBuf::from("nanokvm-20260911-134507.jpg"))
    );
}

/// The counters are the evidence the policy did anything, so they are on the line the user reads
#[test]
fn the_summary_line_reports_every_counter() {
    let quiet = Shot {
        jpeg: Vec::new(),
        width: 1920,
        height: 1080,
        skipped: 8,
        discarded: 0,
        mismatched: 0,
        restarted: false,
    };
    assert_eq!(
        nanokvm::cli::shot::summary_line("frame.jpg", 173_402, &quiet),
        "wrote frame.jpg: 1920x1080, 173402 bytes, skipped 8 frames, 0 at another size"
    );

    let eventful = Shot {
        mismatched: 31,
        restarted: true,
        ..quiet
    };
    assert_eq!(
        nanokvm::cli::shot::summary_line("frame.jpg", 173_402, &eventful),
        "wrote frame.jpg: 1920x1080, 173402 bytes, skipped 8 frames, 31 at another size, \
         stream restarted once"
    );

    // Frames the source refused are their own count, and are reported only when there were any:
    // every hardware run so far has seen none.
    let refused = Shot {
        discarded: 3,
        ..eventful
    };
    assert_eq!(
        nanokvm::cli::shot::summary_line("frame.jpg", 173_402, &refused),
        "wrote frame.jpg: 1920x1080, 173402 bytes, skipped 8 frames, 31 at another size, \
         3 refused by the source, stream restarted once"
    );
}

// ---- the binary ------------------------------------------------------------------------------

/// The recorded desk every binary-level test enumerates instead of the developer's own.
///
/// Without it these runs walk the real `/sys` and `QUERYCAP` whatever video nodes are plugged in,
/// which on the recorded test setup is the dongle and the user's webcam (CLAUDE.md). `--sysfs-root` points
/// discovery at the fixture and stops it opening any node at all (`cli::sources`).
fn fixture_root() -> PathBuf {
    nanokvm::discovery::testing::fixture("usb2-desk")
        .root()
        .to_path_buf()
}

fn run_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_nanokvm"))
        .args(args)
        .arg("--sysfs-root")
        .arg(fixture_root())
        .current_dir(dir)
        // If a regression ever reached the window, this keeps it from finding a compositor and
        // hanging around.
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .output()
        .expect("run the nanokvm binary")
}

fn files_in(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .expect("scratch dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

/// A node that is not there is a message naming it, and no file: a `shot` that left an empty or
/// half-written `out.jpg` behind on failure would be worse than one that failed.
#[test]
fn a_video_node_that_is_not_there_names_the_path_and_writes_nothing() {
    let scratch = Scratch::new("absent");
    let out = run_in(&scratch.0, &["shot", "--video", ABSENT, "out.jpg"]);

    assert!(!out.status.success(), "opening {ABSENT} must not succeed");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(ABSENT) && stderr.contains("MJPEG"),
        "the message must name the node and the mode that was asked for:\n{stderr}"
    );
    assert!(
        files_in(&scratch.0).is_empty(),
        "files were left behind: {:?}",
        files_in(&scratch.0)
    );
}

/// The format is decided before the device is opened, so a typo costs nothing and opens nothing.
/// The proof is that the message is about the extension even though the `--video` path is absent
/// too.
#[test]
fn an_unknown_extension_is_refused_before_the_video_node_is_opened() {
    let scratch = Scratch::new("binext");
    let out = run_in(&scratch.0, &["shot", "--video", ABSENT, "out.bmp"]);

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(".jpg") && stderr.contains(".png"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("MJPEG"),
        "the node was opened anyway:\n{stderr}"
    );
    assert!(files_in(&scratch.0).is_empty());
}

/// `--timeout` is validated before the node is opened, so a value that cannot be waited
/// for is a usage error rather than a panic with the video node held. The proof is that the
/// message is about the timeout even though the `--video` path is absent too.
#[test]
fn an_unwaitable_timeout_is_refused_before_the_video_node_is_opened() {
    let scratch = Scratch::new("timeout");
    let out = run_in(
        &scratch.0,
        &[
            "shot",
            "--video",
            ABSENT,
            "--timeout",
            "18446744073709551615",
            "out.jpg",
        ],
    );

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--timeout"), "{stderr}");
    assert!(
        stderr.contains("3600"),
        "the limit must be named:\n{stderr}"
    );
    assert!(
        !stderr.contains("MJPEG"),
        "the node was opened before the timeout was checked:\n{stderr}"
    );
    assert!(files_in(&scratch.0).is_empty());
}

/// Help must expose the escape hatch for devices that cannot deliver the requested size.
#[test]
fn the_shot_help_documents_the_size_and_skip_policy() {
    let scratch = Scratch::new("help");
    let out = run_in(&scratch.0, &["shot", "--help"]);

    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("--any-size"), "{text}");
    assert!(text.contains("--skip"), "{text}");
    assert!(text.contains("--timeout"), "{text}");
}
