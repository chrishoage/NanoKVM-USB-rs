//! Capture recovery without hardware: §6.1's Stage 2 rows, S2-1, S2-2 and S2-4 (plan §6.1).
//!
//! §6.1's table gives two reachable conditions and a different response to each — a stall gets
//! "attempt restart", a disconnection gets "attempt rediscovery" — and §9.3 says the synthetic
//! source exists so those responses can be asserted deterministically. Everything here drives
//! the real [`Pipeline`] through a scripted source and a scripted opener; the only thing faked
//! is the device.
//!
//! Nothing here sleeps as a synchronisation primitive. Where a test waits, it polls a condition
//! with a deadline: the deadline bounds a hang, and the assertion is on the condition. Where a
//! test asserts an *absence* (no restart happened), it first waits for a positive condition that
//! proves the loop ran — several hundred capture errors, say — so the absence is observed rather
//! than merely not yet observed.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use nanokvm::capture::{
    jpeg, CaptureError, CompressedFrame, DecodedFrame, Decoder, FrameSource, Pipeline,
    PipelineConfig, PipelineState, SourceOpener,
};

/// Shared with `tests/capture_recovery_hardware.rs`, which spawns `scripts/usb-replug.py` through
/// it. Included by path because `tests/` subdirectories are not test targets of their own.
#[path = "support/child_guard.rs"]
mod child_guard;

// ---------------------------------------------------------------------------------------------
// Fixtures and helpers
// ---------------------------------------------------------------------------------------------

fn fixture(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/frames")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// A real 1920x1080 frame off this device (§9.1: captures are fixtures).
fn frame_1080p() -> Vec<u8> {
    fixture("mjpeg-1920x1080-01.jpg")
}

/// A real 1280x720 frame off the same device, for the mode-change tests.
fn frame_720p() -> Vec<u8> {
    fixture("mjpeg-1280x720-01.jpg")
}

/// A real 640x480 frame off the same device: the mode the dongle powers up in, and the one a
/// reopen whose UVC commit was lost streams forever (the format-watchdog tests).
fn frame_640x480() -> Vec<u8> {
    fixture("mjpeg-640x480-01.jpg")
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Poll `cond` until it holds. Panics on the deadline, so a hang is a test failure rather than a
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

/// A generous bound for anything that should happen "soon". Long enough that a loaded machine
/// does not fail the suite, short enough that a genuine hang is caught.
const PATIENCE: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------------------------
// The scripted source and opener
// ---------------------------------------------------------------------------------------------

/// One step of a source's script.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Act {
    /// Deliver `frames[i]`.
    Frame(usize),
    /// [`CaptureError::Timeout`]: the stall shape, with no error to explain it.
    Timeout,
    /// [`CaptureError::Io`] with `EIO`, which V4L2 documents for temporary problems.
    Io,
    /// [`CaptureError::Disconnected`]: `ENODEV`, a `POLLHUP`, a node that went away.
    Disconnect,
    /// A panic on the capture thread, as the `v4l` destructors produce (v4l2 docs item 4).
    Panic,
}

/// What `restart()` does on a scripted source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OnRestart {
    /// Succeed and change nothing: the stall outlives the restart.
    Nothing,
    /// Succeed and resume delivering `frames[0]` forever — a restart that worked.
    Heal,
    /// Succeed and resume delivering `frames[i]` forever. The stuck-stream shape the format
    /// watchdog exists for: the device streams the wrong mode until a fresh `STREAMON`
    /// re-commits the format, and the right mode after it.
    SwitchTo(usize),
    /// Report the node is gone. The pipeline must take the disconnect path, not count a failed
    /// restart.
    Disconnected,
    /// Panic on the calling thread, exactly as `V4l2Source::restart` does when the old stream's
    /// `STREAMOFF` fails with anything but `ENODEV` (v4l2 module docs item 4). The pipeline must
    /// treat it as a failed restart, not as a lost device.
    Panic,
}

/// What the scripted device did, and when. Shared with the test through an `Arc`.
#[derive(Default)]
struct Recorder {
    /// One entry per [`SourceOpener::open`] call, including the initial one.
    opens: Vec<Instant>,
    /// One entry per [`FrameSource::restart`] call.
    restarts: Vec<Instant>,
    /// Sources dropped, and sources whose `Drop` panicked on the way out.
    drops: u64,
    drop_panics: u64,
    /// `next_frame` calls, so "the loop is still running" is observable.
    calls: u64,
    /// Set while an [`OpenAct::Slow`] open is blocked inside the opener, so a test can wait for
    /// the pipeline to really be *inside* `open()` rather than about to be.
    in_slow_open: bool,
}

/// A [`FrameSource`] that follows a script. The last entry of `acts` is consumed like the rest;
/// when the script runs out, `tail` repeats forever.
///
/// Every act costs `pace` of wall clock, including the failures. A scripted timeout that returns
/// instantly is three microseconds of "stall" and nothing ever reaches
/// [`PipelineConfig::restart_after`] — the same trap `SyntheticSource::fail_after` documents.
struct ScriptedSource {
    acts: VecDeque<Act>,
    tail: Act,
    frames: Arc<Vec<Vec<u8>>>,
    /// The next frame's sequence number. Set it apart from the previous source's range and a
    /// frame that crossed a reopen becomes identifiable rather than merely suspicious.
    sequence: u32,
    pace: Duration,
    on_restart: OnRestart,
    /// What this source claims `S_FMT` committed to. `None` — the default, and what every other
    /// test here uses — leaves the pipeline's format watchdog inert.
    negotiated: Option<(u32, u32)>,
    panic_on_drop: bool,
    label: String,
    rec: Arc<Mutex<Recorder>>,
}

impl ScriptedSource {
    fn new(
        rec: &Arc<Mutex<Recorder>>,
        frames: Arc<Vec<Vec<u8>>>,
        acts: Vec<Act>,
        tail: Act,
    ) -> Self {
        ScriptedSource {
            acts: acts.into(),
            tail,
            frames,
            sequence: 0,
            pace: Duration::from_millis(5),
            on_restart: OnRestart::Nothing,
            negotiated: None,
            panic_on_drop: false,
            label: "scripted".to_string(),
            rec: Arc::clone(rec),
        }
    }
}

impl FrameSource for ScriptedSource {
    fn next_frame(&mut self) -> Result<CompressedFrame, CaptureError> {
        lock(&self.rec).calls += 1;
        std::thread::sleep(self.pace);
        let act = self.acts.pop_front().unwrap_or(self.tail);
        match act {
            Act::Timeout => Err(CaptureError::Timeout(self.pace)),
            Act::Io => Err(CaptureError::Io(std::io::Error::from_raw_os_error(
                libc::EIO,
            ))),
            Act::Disconnect => Err(CaptureError::Disconnected),
            Act::Panic => panic!("scripted panic, as a v4l destructor would"),
            Act::Frame(i) => {
                let jpeg_bytes = self.frames[i % self.frames.len()].clone();
                // A6, the same rule both real sources follow: dimensions from this frame's own
                // start-of-frame header.
                let (width, height) = jpeg::dimensions(&jpeg_bytes)
                    .map_err(|e| CaptureError::BadFrame(e.to_string()))?;
                let seq = self.sequence;
                self.sequence = self.sequence.wrapping_add(1);
                Ok(CompressedFrame {
                    jpeg: jpeg_bytes,
                    width,
                    height,
                    captured_at: nanokvm::capture::v4l2::now_monotonic(),
                    sequence: seq,
                })
            }
        }
    }

    fn restart(&mut self) -> Result<(), CaptureError> {
        lock(&self.rec).restarts.push(Instant::now());
        match self.on_restart {
            OnRestart::Nothing => Ok(()),
            OnRestart::Heal => {
                self.acts.clear();
                self.tail = Act::Frame(0);
                Ok(())
            }
            OnRestart::SwitchTo(i) => {
                self.acts.clear();
                self.tail = Act::Frame(i);
                Ok(())
            }
            OnRestart::Disconnected => Err(CaptureError::Disconnected),
            OnRestart::Panic => panic!("scripted restart panic (STREAMOFF failed, as v4l would)"),
        }
    }

    fn negotiated_dimensions(&self) -> Option<(u32, u32)> {
        self.negotiated
    }

    fn describe(&self) -> String {
        self.label.clone()
    }
}

impl Drop for ScriptedSource {
    /// The `v4l` destructors panic on any teardown ioctl that fails with something other than
    /// `ENODEV` (`capture::v4l2` module docs item 4), and they run on the capture thread. The
    /// pipeline has to absorb that and still reopen.
    fn drop(&mut self) {
        let mut rec = lock(&self.rec);
        rec.drops += 1;
        if self.panic_on_drop {
            rec.drop_panics += 1;
            drop(rec);
            panic!("scripted destructor panic (STREAMOFF failed, as the v4l crate would)");
        }
    }
}

/// One step of an opener's script.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenAct {
    /// The node is not back yet. `ENOENT`/`ENODEV`/`EBUSY` all look like this to the pipeline.
    Fail,
    /// Hand over the next source.
    Give,
    /// A device that answers *slowly* rather than one that is absent: block for this long inside
    /// `open`, then fail. `V4l2Source::open` is `S_FMT`, `S_PARM`, `REQBUFS`, four `mmap`s and
    /// `QBUF`s and a `STREAMON`, each a USB control transfer to a node that has just
    /// re-enumerated, where the kernel's control timeout runs to seconds. Nothing can interrupt
    /// that, so the pipeline must not be waiting on it when `stop()` arrives.
    Slow(Duration),
}

/// A [`SourceOpener`] that follows a script: `Err x N then Ok`, as many times as asked.
struct ScriptedOpener {
    acts: VecDeque<OpenAct>,
    tail: OpenAct,
    /// Builds the source for the *n*th successful open.
    make: Box<dyn FnMut(usize) -> ScriptedSource + Send>,
    given: usize,
    rec: Arc<Mutex<Recorder>>,
}

impl SourceOpener for ScriptedOpener {
    fn open(&mut self) -> Result<Box<dyn FrameSource>, CaptureError> {
        lock(&self.rec).opens.push(Instant::now());
        match self.acts.pop_front().unwrap_or(self.tail) {
            OpenAct::Fail => Err(CaptureError::Disconnected),
            OpenAct::Slow(d) => {
                lock(&self.rec).in_slow_open = true;
                std::thread::sleep(d);
                lock(&self.rec).in_slow_open = false;
                Err(CaptureError::Disconnected)
            }
            OpenAct::Give => {
                let source = (self.make)(self.given);
                self.given += 1;
                Ok(Box::new(source))
            }
        }
    }

    fn describe(&self) -> String {
        "scripted opener".to_string()
    }
}

/// A config with the recovery timers wound right down, so a test that exercises three backoff
/// steps takes a fifth of a second rather than eleven.
fn fast(reopen_backoff: Duration, max: Duration) -> PipelineConfig {
    PipelineConfig {
        stall_after: Duration::from_millis(50),
        reopen_backoff,
        reopen_max_backoff: max,
        restart_after: None,
        ..PipelineConfig::default()
    }
}

// ---------------------------------------------------------------------------------------------
// The disconnect path (§6.1 S2-4)
// ---------------------------------------------------------------------------------------------

/// The whole disconnect story in one test: the loss is reported, the reopen is retried on a
/// doubling backoff that respects its cap, the reopen succeeds, frames flow again — and the
/// renderer's last image survives the entire gap untouched (§6.1 S1-2, carried into S2-4).
#[test]
fn a_disconnect_reopens_on_a_doubling_backoff_and_frames_resume() {
    let frames = Arc::new(vec![frame_1080p()]);
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let opener: Box<dyn SourceOpener> = {
        let rec2 = Arc::clone(&rec);
        let frames = Arc::clone(&frames);
        Box::new(ScriptedOpener {
            acts: vec![
                OpenAct::Give,
                OpenAct::Fail,
                OpenAct::Fail,
                OpenAct::Fail,
                OpenAct::Give,
            ]
            .into(),
            tail: OpenAct::Give,
            make: Box::new(move |n| {
                if n == 0 {
                    ScriptedSource::new(
                        &rec2,
                        Arc::clone(&frames),
                        vec![Act::Frame(0), Act::Frame(0), Act::Frame(0)],
                        Act::Disconnect,
                    )
                } else {
                    ScriptedSource::new(&rec2, Arc::clone(&frames), vec![], Act::Frame(0))
                }
            }),
            given: 0,
            rec: Arc::clone(&rec),
        })
    };

    let backoff = Duration::from_millis(20);
    let cap = Duration::from_millis(80);
    let h = Pipeline::start_with_opener(opener, fast(backoff, cap));
    let out = h.output();

    // Frames first, decoded and pending in the output slot.
    wait_for("the first decoded frame", PATIENCE, || {
        out.is_pending() && h.stats().frames_decoded >= 1
    });
    let decoded_before = h.stats().frames_decoded;

    // The device goes away.
    wait_for("the disconnection", PATIENCE, || h.stats().disconnects >= 1);
    let st = h.stats();
    assert!(
        matches!(
            st.state,
            PipelineState::Disconnected | PipelineState::Reconnecting
        ),
        "a lost device reported as {:?}",
        st.state
    );
    assert!(
        st.disconnected_since.is_some(),
        "disconnected_since was not set"
    );
    assert!(
        st.last_error
            .as_deref()
            .is_some_and(|e| e.contains("disconnected")),
        "last_error was {:?}",
        st.last_error
    );

    // §6.1 S1-2, across the gap: the renderer's last image is still there. Nothing closed the
    // output slot and nothing drained it.
    assert!(
        out.is_pending(),
        "the last decoded frame was discarded during the gap"
    );

    // The device comes back.
    wait_for("the reopen", PATIENCE, || h.stats().reopens >= 1);
    wait_for("frames after the reopen", PATIENCE, || {
        h.stats().frames_decoded >= decoded_before + 3
    });
    wait_for("Running again", PATIENCE, || {
        h.state() == PipelineState::Running
    });

    let st = h.stats();
    assert_eq!(st.disconnects, 1, "{st:?}");
    assert_eq!(st.reopens, 1, "{st:?}");
    assert_eq!(
        st.reopen_attempts, 4,
        "three failures and one success expected; the initial open must not be counted: {st:?}"
    );
    assert_eq!(
        st.disconnected_since, None,
        "disconnected_since outlived the reopen"
    );
    assert_eq!(st.last_resolution, Some((1920, 1080)));

    // The backoff really doubled, and really stopped at the cap. opens[0] is the initial open;
    // opens[1..=3] are the three failures and opens[4] the success, so the gap *before* attempt
    // k is opens[k] - opens[k-1] for k >= 2.
    let opens = lock(&rec).opens.clone();
    assert_eq!(opens.len(), 5, "unexpected number of open() calls");
    let g1 = opens[2] - opens[1];
    let g2 = opens[3] - opens[2];
    let g3 = opens[4] - opens[3];
    assert!(
        g1 >= backoff * 2,
        "the second wait was {g1:?}, so the backoff did not double from {backoff:?}"
    );
    assert!(
        g2 >= cap,
        "the third wait was {g2:?}, expected the {cap:?} cap"
    );
    assert!(
        g3 >= cap && g3 < cap * 8,
        "the fourth wait was {g3:?}; the backoff should be pinned at the {cap:?} cap"
    );
    assert!(g2 >= g1, "the backoff did not grow: {g1:?} then {g2:?}");

    // The image that survived the gap is a whole, correctly sized frame, not a husk.
    let held = out.take().expect("a frame is still pending");
    assert_eq!((held.width, held.height), (1920, 1080));
    assert_eq!(held.rgba.len(), 1920 * 1080 * 4);

    h.stop();
}

/// `stop()` during a reconnect backoff must not wait the backoff out.
///
/// The observable consequences are both asserted: `stop()` returns promptly, and the reconnect
/// loop makes no further `open()` call afterwards — which is what "the thread really exited"
/// means from outside. (The `Stopped` state itself is not observable: `stop()` consumes the
/// handle, by Stage 1's design.)
#[test]
fn stop_during_a_reconnect_backoff_returns_promptly() {
    let frames = Arc::new(vec![frame_1080p()]);
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let opener: Box<dyn SourceOpener> = {
        let rec2 = Arc::clone(&rec);
        let frames = Arc::clone(&frames);
        Box::new(ScriptedOpener {
            acts: vec![OpenAct::Give].into(),
            // Never comes back: the pipeline sits in the backoff until it is stopped.
            tail: OpenAct::Fail,
            make: Box::new(move |_| {
                ScriptedSource::new(
                    &rec2,
                    Arc::clone(&frames),
                    vec![Act::Frame(0)],
                    Act::Disconnect,
                )
            }),
            given: 0,
            rec: Arc::clone(&rec),
        })
    };

    // A backoff far longer than the promptness bound below, so sleeping it out would fail.
    let h =
        Pipeline::start_with_opener(opener, fast(Duration::from_secs(3), Duration::from_secs(3)));
    wait_for("the reconnect loop", PATIENCE, || {
        h.state() == PipelineState::Reconnecting
    });
    let attempts_at_stop = lock(&rec).opens.len();

    let t = Instant::now();
    h.stop();
    let elapsed = t.elapsed();
    assert!(
        elapsed < Duration::from_millis(500),
        "stop() during a 3 s reconnect backoff took {elapsed:?}"
    );

    // And it stayed stopped: no further open() attempts once stop() returned.
    let settle = Instant::now() + Duration::from_millis(200);
    while Instant::now() < settle {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        lock(&rec).opens.len(),
        attempts_at_stop,
        "the reconnect loop kept trying after stop() returned"
    );
}

/// The `v4l` destructors panic on a failed teardown ioctl and they run on the capture thread
/// (v4l2 module docs item 4). A panicking `Drop` on the disconnect path must not kill the thread
/// and must not cost the reopen.
#[test]
fn a_panicking_destructor_does_not_kill_the_thread_or_skip_the_reopen() {
    let frames = Arc::new(vec![frame_1080p()]);
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let opener: Box<dyn SourceOpener> = {
        let rec2 = Arc::clone(&rec);
        let frames = Arc::clone(&frames);
        Box::new(ScriptedOpener {
            acts: vec![OpenAct::Give, OpenAct::Give].into(),
            tail: OpenAct::Give,
            make: Box::new(move |n| {
                let mut s = if n == 0 {
                    ScriptedSource::new(
                        &rec2,
                        Arc::clone(&frames),
                        vec![Act::Frame(0), Act::Frame(0)],
                        Act::Disconnect,
                    )
                } else {
                    ScriptedSource::new(&rec2, Arc::clone(&frames), vec![], Act::Frame(0))
                };
                s.panic_on_drop = n == 0;
                s
            }),
            given: 0,
            rec: Arc::clone(&rec),
        })
    };

    let h = Pipeline::start_with_opener(
        opener,
        fast(Duration::from_millis(10), Duration::from_millis(40)),
    );
    wait_for("the reopen", PATIENCE, || h.stats().reopens >= 1);
    assert_eq!(
        lock(&rec).drop_panics,
        1,
        "the panicking destructor never ran"
    );
    let after = h.stats().frames_captured;
    wait_for("frames after the panicking teardown", PATIENCE, || {
        h.stats().frames_captured >= after + 5
    });
    let st = h.stats();
    assert_eq!(st.disconnects, 1, "{st:?}");
    assert_eq!(st.reopens, 1, "{st:?}");
    h.stop();
}

/// A pipeline whose device is not there at all when it starts must retry rather than die: this
/// is "start the client, then plug the dongle in", and it is the same loop S2-4 uses.
#[test]
fn a_device_absent_at_startup_is_retried_rather_than_fatal() {
    let frames = Arc::new(vec![frame_1080p()]);
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let opener: Box<dyn SourceOpener> = {
        let rec2 = Arc::clone(&rec);
        let frames = Arc::clone(&frames);
        Box::new(ScriptedOpener {
            acts: vec![OpenAct::Fail, OpenAct::Fail].into(),
            tail: OpenAct::Give,
            make: Box::new(move |_| {
                ScriptedSource::new(&rec2, Arc::clone(&frames), vec![], Act::Frame(0))
            }),
            given: 0,
            rec: Arc::clone(&rec),
        })
    };

    let h = Pipeline::start_with_opener(
        opener,
        fast(Duration::from_millis(10), Duration::from_millis(40)),
    );
    wait_for("frames once the device appears", PATIENCE, || {
        h.stats().frames_decoded >= 3
    });
    let st = h.stats();
    // Nothing was ever connected, so nothing was disconnected: the failed *initial* open is not
    // a disconnection, and the retries after it are reopen attempts.
    assert_eq!(st.disconnects, 0, "{st:?}");
    assert_eq!(st.reopens, 1, "{st:?}");
    assert_eq!(st.reopen_attempts, 2, "{st:?}");
    h.stop();
}

/// Stage 1's entry point is unchanged: one source, no rediscovery, and the disconnection is
/// terminal. A caller that handed over a device it opened itself has given the pipeline nothing
/// to reopen, and inventing an attempt would be a lie about what was tried.
#[test]
fn pipeline_start_never_reopens_after_a_disconnection() {
    let frames = Arc::new(vec![frame_1080p()]);
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let source = ScriptedSource::new(
        &rec,
        frames,
        vec![Act::Frame(0), Act::Frame(0)],
        Act::Disconnect,
    );
    let h = Pipeline::start(
        Box::new(source),
        fast(Duration::from_millis(10), Duration::from_millis(40)),
    );

    wait_for("the disconnection", PATIENCE, || {
        h.state() == PipelineState::Disconnected
    });
    // And it stays there: the capture thread has exited, so no further next_frame calls happen.
    let calls = lock(&rec).calls;
    let settle = Instant::now() + Duration::from_millis(150);
    while Instant::now() < settle {
        assert_eq!(
            h.state(),
            PipelineState::Disconnected,
            "a non-recovering pipeline left Disconnected"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        lock(&rec).calls,
        calls,
        "the capture thread kept polling a disconnected source"
    );
    let st = h.stats();
    assert_eq!(st.disconnects, 1, "{st:?}");
    assert_eq!(st.reopens, 0, "{st:?}");
    assert_eq!(
        st.reopen_attempts, 0,
        "Pipeline::start must attempt no reopen at all: {st:?}"
    );
    h.stop();
}

/// The snapshot must never contradict itself: `Running` and "disconnected since" cannot both be
/// true. Sampled continuously across a real disconnect/reopen cycle rather than once.
#[test]
fn stats_never_report_running_while_disconnected() {
    let frames = Arc::new(vec![frame_1080p()]);
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let opener: Box<dyn SourceOpener> = {
        let rec2 = Arc::clone(&rec);
        let frames = Arc::clone(&frames);
        Box::new(ScriptedOpener {
            acts: vec![OpenAct::Give, OpenAct::Fail, OpenAct::Give].into(),
            tail: OpenAct::Give,
            make: Box::new(move |n| {
                if n == 0 {
                    ScriptedSource::new(
                        &rec2,
                        Arc::clone(&frames),
                        vec![Act::Frame(0), Act::Frame(0)],
                        Act::Disconnect,
                    )
                } else {
                    ScriptedSource::new(&rec2, Arc::clone(&frames), vec![], Act::Frame(0))
                }
            }),
            given: 0,
            rec: Arc::clone(&rec),
        })
    };

    let h = Pipeline::start_with_opener(
        opener,
        fast(Duration::from_millis(20), Duration::from_millis(40)),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_gap = false;
    let mut saw_running_after = false;
    while Instant::now() < deadline {
        let st = h.stats();
        match st.state {
            PipelineState::Running => assert_eq!(
                st.disconnected_since, None,
                "Running with a disconnected_since set: {st:?}"
            ),
            PipelineState::Disconnected | PipelineState::Reconnecting => {
                assert!(
                    st.disconnected_since.is_some(),
                    "disconnected without a disconnected_since: {st:?}"
                );
                saw_gap = true;
            }
            _ => {}
        }
        if saw_gap && st.reopens == 1 && st.state == PipelineState::Running {
            saw_running_after = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(saw_gap, "the disconnection was never observed");
    assert!(saw_running_after, "the pipeline never came back to Running");
    h.stop();
}

// ---------------------------------------------------------------------------------------------
// The stall path (§6.1 S2-1: "attempt restart")
// ---------------------------------------------------------------------------------------------

fn stall_config(restart_after: Option<Duration>) -> PipelineConfig {
    PipelineConfig {
        stall_after: Duration::from_millis(30),
        restart_after,
        reopen_backoff: Duration::from_millis(10),
        reopen_max_backoff: Duration::from_millis(40),
        ..PipelineConfig::default()
    }
}

/// A stall with no error to explain it gets a restart, then another one every `restart_after`
/// for as long as it lasts (§6.1: the stall row's response is "attempt restart").
#[test]
fn a_stall_restarts_the_stream_and_keeps_restarting_while_it_lasts() {
    let frames = Arc::new(vec![frame_1080p()]);
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let source = ScriptedSource::new(
        &rec,
        frames,
        vec![Act::Frame(0), Act::Frame(0)],
        Act::Timeout,
    );
    let restart_after = Duration::from_millis(60);
    let h = Pipeline::start(Box::new(source), stall_config(Some(restart_after)));

    wait_for("two stream restarts", PATIENCE, || {
        h.stats().stream_restarts >= 2
    });
    let restarts = lock(&rec).restarts.clone();
    assert!(restarts.len() >= 2);
    let gap = restarts[1] - restarts[0];
    assert!(
        gap >= restart_after,
        "the second restart came {gap:?} after the first, under the {restart_after:?} interval"
    );
    assert!(
        gap < restart_after * 10,
        "the second restart came {gap:?} later; the timer is not repeating"
    );
    let st = h.stats();
    assert_eq!(st.state, PipelineState::Stalled, "{st:?}");
    assert_eq!(
        st.disconnects, 0,
        "a stall must never be reported as a disconnection (§6.1): {st:?}"
    );
    h.stop();
}

/// A restart that works stops the restarting, and a frame arriving resets the timer — otherwise
/// a healthy 60 fps stream would be torn down every `restart_after` forever.
#[test]
fn a_frame_arriving_resets_the_restart_timer() {
    let frames = Arc::new(vec![frame_1080p()]);
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let mut source = ScriptedSource::new(&rec, frames, vec![Act::Frame(0)], Act::Timeout);
    source.on_restart = OnRestart::Heal;
    let restart_after = Duration::from_millis(60);
    let h = Pipeline::start(Box::new(source), stall_config(Some(restart_after)));

    wait_for("the healing restart", PATIENCE, || {
        h.stats().stream_restarts >= 1
    });
    let after_heal = h.stats().frames_captured;
    // Four restart intervals' worth of healthy frames. If the timer measured from anything but
    // the last frame, several more restarts would have fired by now.
    wait_for("frames after the restart", PATIENCE, || {
        h.stats().frames_captured >= after_heal + 30
    });
    let elapsed_intervals = Instant::now();
    while elapsed_intervals.elapsed() < restart_after * 3 {
        std::thread::sleep(Duration::from_millis(5));
    }
    let st = h.stats();
    assert_eq!(
        st.stream_restarts, 1,
        "the restart timer kept firing while frames were arriving: {st:?}"
    );
    assert_eq!(st.state, PipelineState::Running, "{st:?}");
    h.stop();
}

/// `restart_after: None` disables the restart entirely. Asserted against a source that is
/// stalled hard and has been polled hundreds of times, so the absence is observed rather than
/// merely not yet observed.
#[test]
fn restart_after_none_never_restarts() {
    let frames = Arc::new(vec![frame_1080p()]);
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let source = ScriptedSource::new(&rec, frames, vec![Act::Frame(0)], Act::Timeout);
    let h = Pipeline::start(Box::new(source), stall_config(None));

    // 20 timeouts at a 5 ms pace plus the 20 ms error backoff is a quarter of a second of stall:
    // four times over any restart interval this test could have used.
    wait_for("a long stall", PATIENCE, || h.stats().capture_errors >= 20);
    let st = h.stats();
    assert_eq!(st.stream_restarts, 0, "{st:?}");
    assert!(
        lock(&rec).restarts.is_empty(),
        "restart() was called anyway"
    );
    assert_eq!(st.state, PipelineState::Stalled, "{st:?}");
    h.stop();
}

/// A restart that comes back `Disconnected` has discovered what the stall could not distinguish:
/// the node is gone. That is the disconnect path — reopen — not a counted restart failure.
#[test]
fn a_restart_that_reports_disconnected_takes_the_disconnect_path() {
    let frames = Arc::new(vec![frame_1080p()]);
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let opener: Box<dyn SourceOpener> = {
        let rec2 = Arc::clone(&rec);
        let frames = Arc::clone(&frames);
        Box::new(ScriptedOpener {
            acts: vec![OpenAct::Give, OpenAct::Give].into(),
            tail: OpenAct::Give,
            make: Box::new(move |n| {
                let mut s = if n == 0 {
                    ScriptedSource::new(
                        &rec2,
                        Arc::clone(&frames),
                        vec![Act::Frame(0)],
                        Act::Timeout,
                    )
                } else {
                    ScriptedSource::new(&rec2, Arc::clone(&frames), vec![], Act::Frame(0))
                };
                if n == 0 {
                    s.on_restart = OnRestart::Disconnected;
                }
                s
            }),
            given: 0,
            rec: Arc::clone(&rec),
        })
    };

    let h = Pipeline::start_with_opener(opener, stall_config(Some(Duration::from_millis(50))));
    wait_for("the reopen the failed restart caused", PATIENCE, || {
        h.stats().reopens >= 1
    });
    let after = h.stats().frames_captured;
    wait_for("frames after the reopen", PATIENCE, || {
        h.stats().frames_captured >= after + 5
    });
    let st = h.stats();
    assert_eq!(st.disconnects, 1, "{st:?}");
    assert_eq!(st.reopens, 1, "{st:?}");
    assert!(st.stream_restarts >= 1, "{st:?}");
    h.stop();
}

// ---------------------------------------------------------------------------------------------
// Resolution change (§6.1 S2-3 groundwork, §6/A6)
// ---------------------------------------------------------------------------------------------

/// A target that changes mode mid-stream: 1920x1080, then 1280x720, then back. Every decoded
/// frame must be correctly sized for its own header (A6), the changes must be counted, and the
/// decoder's reused scratch buffer must not leave the smaller frame carrying anything of the
/// larger one — asserted by comparing against a fresh decode of the same fixture.
#[test]
fn a_resolution_switch_produces_correctly_sized_frames_at_every_step() {
    let frames = Arc::new(vec![frame_1080p(), frame_720p()]);
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let mut acts: Vec<Act> = Vec::new();
    for _ in 0..6 {
        acts.push(Act::Frame(0));
    }
    for _ in 0..6 {
        acts.push(Act::Frame(1));
    }
    for _ in 0..6 {
        acts.push(Act::Frame(0));
    }
    let mut source = ScriptedSource::new(&rec, Arc::clone(&frames), acts, Act::Frame(0));
    // Slower than a decode (2.4 ms at 1080p, A19) so the consumer below sees every frame rather
    // than the pipeline dropping them: this test is about each step, not about drop behaviour.
    source.pace = Duration::from_millis(15);
    let h = Pipeline::start(Box::new(source), stall_config(None));
    let out = h.output();

    let reference_720p = decode_reference(&frames[1]);
    let mut seen: Vec<(u32, u32)> = Vec::new();
    let mut checked_720p = false;
    let deadline = Instant::now() + PATIENCE;
    while seen.len() < 18 && Instant::now() < deadline {
        let Some(f) = out.wait_take(Duration::from_millis(50)) else {
            continue;
        };
        // The frame is self-consistent with its own header, whichever way the size just moved.
        assert_eq!(
            f.rgba.len(),
            DecodedFrame::expected_len(f.width, f.height),
            "a {}x{} frame carries {} RGBA bytes",
            f.width,
            f.height,
            f.rgba.len()
        );
        if (f.width, f.height) == (1280, 720) && !checked_720p {
            assert_eq!(
                f.rgba, reference_720p.rgba,
                "the 720p frame that followed a 1080p one does not match a fresh decode; \
                 the reused scratch buffer leaked"
            );
            checked_720p = true;
        }
        seen.push((f.width, f.height));
    }

    assert!(
        seen.contains(&(1920, 1080)) && seen.contains(&(1280, 720)),
        "both resolutions must have been observed, saw {seen:?}"
    );
    assert!(checked_720p, "no 720p frame reached the output slot");
    let st = h.stats();
    assert!(
        st.resolution_changes >= 2,
        "1080p -> 720p -> 1080p is two changes, counted {}: {st:?}",
        st.resolution_changes
    );
    assert_eq!(
        st.last_resolution,
        Some((1920, 1080)),
        "the stream ends back at 1080p: {st:?}"
    );
    h.stop();
}

/// A single frame decoded on its own, as the yardstick for "the pipeline decoded this correctly".
fn decode_reference(jpeg_bytes: &[u8]) -> DecodedFrame {
    let (width, height) = jpeg::dimensions(jpeg_bytes).expect("fixture header");
    let frame = CompressedFrame {
        jpeg: jpeg_bytes.to_vec(),
        width,
        height,
        captured_at: Duration::ZERO,
        sequence: 0,
    };
    let mut out = DecodedFrame::empty();
    Decoder::new()
        .decode(&frame, &mut out)
        .expect("fixture decode");
    out
}

/// The first frame establishes the resolution; it is not a change. Without this the counter
/// would report a mode change on every stream that ever started.
#[test]
fn the_first_frame_is_not_a_resolution_change() {
    let frames = Arc::new(vec![frame_1080p()]);
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let source = ScriptedSource::new(&rec, frames, vec![], Act::Frame(0));
    let h = Pipeline::start(Box::new(source), stall_config(None));
    wait_for("several frames", PATIENCE, || {
        h.stats().frames_captured >= 5
    });
    let st = h.stats();
    assert_eq!(st.resolution_changes, 0, "{st:?}");
    assert_eq!(st.last_resolution, Some((1920, 1080)));
    // And the unused `Slot`/`Io` arms of the script are wired: an EIO run is still survivable.
    h.stop();
}

/// The scripted `Io` act exists to keep the survivable-error path honest alongside the new
/// recovery ones: an `EIO` run must stay a counted capture error, never a disconnection and
/// never a reopen.
#[test]
fn an_io_error_run_is_neither_a_disconnect_nor_a_reopen() {
    let frames = Arc::new(vec![frame_1080p()]);
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let source = ScriptedSource::new(&rec, frames, vec![Act::Frame(0)], Act::Io);
    let h = Pipeline::start(Box::new(source), stall_config(None));
    wait_for("a run of I/O errors", PATIENCE, || {
        h.stats().capture_errors >= 10
    });
    let st = h.stats();
    assert_eq!(st.disconnects, 0, "{st:?}");
    assert_eq!(st.reopens, 0, "{st:?}");
    assert_eq!(st.reopen_attempts, 0, "{st:?}");
    h.stop();
}

/// A panic on the capture thread is a lost device, and a recovering pipeline reopens after one
/// rather than sitting dead — the shape `capture::v4l2`'s panicking destructors produce.
#[test]
fn a_panic_on_the_capture_thread_is_recovered_from_when_an_opener_is_available() {
    let frames = Arc::new(vec![frame_1080p()]);
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let opener: Box<dyn SourceOpener> = {
        let rec2 = Arc::clone(&rec);
        let frames = Arc::clone(&frames);
        Box::new(ScriptedOpener {
            acts: vec![OpenAct::Give, OpenAct::Give].into(),
            tail: OpenAct::Give,
            make: Box::new(move |n| {
                if n == 0 {
                    ScriptedSource::new(&rec2, Arc::clone(&frames), vec![Act::Frame(0)], Act::Panic)
                } else {
                    ScriptedSource::new(&rec2, Arc::clone(&frames), vec![], Act::Frame(0))
                }
            }),
            given: 0,
            rec: Arc::clone(&rec),
        })
    };
    // A backoff long enough to read the panic message off `stats()` before the reopen clears it.
    let h = Pipeline::start_with_opener(
        opener,
        fast(Duration::from_millis(300), Duration::from_millis(300)),
    );
    wait_for("the panic to be recorded", PATIENCE, || {
        h.stats()
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("panicked"))
    });
    wait_for("the reopen after the panic", PATIENCE, || {
        h.stats().reopens >= 1
    });
    let after = h.stats().frames_captured;
    wait_for("frames after the panic", PATIENCE, || {
        h.stats().frames_captured >= after + 5
    });
    let st = h.stats();
    assert_eq!(st.disconnects, 1, "{st:?}");
    // And the message does not outlive the recovery — see `last_error_is_cleared_by_a_reopen`.
    assert_eq!(st.last_error, None, "{st:?}");
    h.stop();
}

/// The per-interval `bytesused` window is the probe's instrument, and it must be a *window*:
/// taken and reset, never the cumulative fold.
#[test]
fn the_bytes_window_resets_when_taken() {
    let frames = Arc::new(vec![frame_1080p()]);
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let source = ScriptedSource::new(&rec, frames, vec![], Act::Frame(0));
    let h = Pipeline::start(Box::new(source), stall_config(None));
    wait_for("frames", PATIENCE, || h.stats().frames_captured >= 5);
    let first = h.take_bytes_window();
    assert!(first.count >= 5, "{first}");
    let immediately = h.take_bytes_window();
    assert_eq!(
        immediately.count, 0,
        "the window was not reset by taking it: {immediately}"
    );
    // It refills from the capture thread, and the cumulative fold was never touched.
    wait_for("the window to refill", PATIENCE, || {
        h.stats().frames_captured >= 10
    });
    let refilled = h.take_bytes_window();
    assert!(refilled.count > 0, "the window never refilled: {refilled}");
    let cumulative = h.stats().bytes;
    assert!(
        cumulative.count >= first.count,
        "the cumulative fold was reset too: {cumulative}"
    );
    h.stop();
}

// ---------------------------------------------------------------------------------------------
// The review's findings, pinned (REVIEW.md #1-#4)
// ---------------------------------------------------------------------------------------------

/// **Review finding #1.** A `restart()` that *panics* is a failed restart, never a lost device.
///
/// `V4l2Source::restart` drops the old stream, and the `v4l` destructors `panic!` on any teardown
/// ioctl that fails with something other than `ENODEV` (v4l2 module docs item 4) — which is
/// exactly the state a genuinely wedged stream is in, and the only state the stall timer exists
/// to attack. Before the fix that panic reached the capture loop's `catch_unwind` and became
/// `Step::Lost`: on `Pipeline::start` — the entry point `src/main.rs` uses, now with
/// `restart_after` on by default — every no-signal period would have ended with `disconnects = 1`
/// and the capture thread exited, from a stall Stage 1 survived indefinitely (§6.1 S1-1/S1-2).
#[test]
fn a_panicking_restart_is_a_failed_restart_not_a_lost_device() {
    let frames = Arc::new(vec![frame_1080p()]);
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let mut source = ScriptedSource::new(&rec, frames, vec![Act::Frame(0)], Act::Timeout);
    source.on_restart = OnRestart::Panic;
    let restart_after = Duration::from_millis(60);
    let h = Pipeline::start(Box::new(source), stall_config(Some(restart_after)));

    wait_for("the first restart attempt", PATIENCE, || {
        h.stats().stream_restarts >= 1
    });
    let calls_at_restart = lock(&rec).calls;

    // The loop has to prove it is still alive, and still restarting: two more intervals.
    wait_for("a second restart attempt", PATIENCE, || {
        h.stats().stream_restarts >= 2
    });

    let st = h.stats();
    let calls_now = lock(&rec).calls;
    h.stop();
    assert_eq!(
        st.disconnects, 0,
        "a restart that panicked was reported as a lost device; a stall must not be turned \
         into a disconnection (§6.1, A5): {st:?}"
    );
    assert_eq!(
        st.state,
        PipelineState::Stalled,
        "the pipeline left Stalled after a failed restart: {st:?}"
    );
    assert!(
        calls_now > calls_at_restart,
        "the capture thread stopped polling the source after a restart panicked \
         ({calls_at_restart} calls, still {calls_now} later)"
    );
    assert!(
        st.capture_errors >= 1,
        "the failed restart was not counted as a capture error: {st:?}"
    );
    assert!(
        st.last_error
            .as_deref()
            .is_some_and(|e| e.contains("panicked")),
        "the panic message was lost: {:?}",
        st.last_error
    );
}

/// **Review finding #2.** `stop()` must return promptly even while `opener.open()` is in flight.
///
/// The reopen *backoff* was already interruptible; the `open()` after it was not, and `stop()`
/// joins the capture thread. A real `V4l2Source::open` against a device that has just
/// re-enumerated is a chain of USB control transfers with second-scale kernel timeouts, so a user
/// closing the viewer at the wrong moment waited the whole open out — measured at 1.999 s against
/// a 2 s open. The open now runs on a helper thread the capture thread abandons; see
/// `PipelineHandle::stop` for the bound and the residual.
#[test]
fn stop_during_a_reopen_returns_promptly() {
    let frames = Arc::new(vec![frame_1080p()]);
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let slow = Duration::from_secs(2);
    let opener: Box<dyn SourceOpener> = {
        let rec2 = Arc::clone(&rec);
        let frames = Arc::clone(&frames);
        Box::new(ScriptedOpener {
            acts: vec![OpenAct::Give].into(),
            // A device that answers slowly, not one that is absent.
            tail: OpenAct::Slow(slow),
            make: Box::new(move |_| {
                ScriptedSource::new(
                    &rec2,
                    Arc::clone(&frames),
                    vec![Act::Frame(0)],
                    Act::Disconnect,
                )
            }),
            given: 0,
            rec: Arc::clone(&rec),
        })
    };

    let h = Pipeline::start_with_opener(
        opener,
        fast(Duration::from_millis(5), Duration::from_millis(20)),
    );
    wait_for("the capture thread to be inside open()", PATIENCE, || {
        lock(&rec).in_slow_open
    });

    let t = Instant::now();
    h.stop();
    let elapsed = t.elapsed();
    assert!(
        elapsed < Duration::from_millis(500),
        "stop() called while the pipeline was inside opener.open() took {elapsed:?}; \
         shutdown is only prompt between opens, not during one"
    );
    assert!(
        elapsed < slow,
        "stop() waited the {slow:?} open out ({elapsed:?})"
    );
}

/// **Review finding #3.** A pipeline that recovered must not go on reporting the failure it
/// recovered from.
///
/// `disconnected_since` was cleared inside the same lock acquisition that sets `Running`, and the
/// error string was left beside it saying the opposite, for the rest of the process's life. The
/// one consumer this slice ships — `examples/capture-probe.rs` — prints a line per change in
/// `last_error` and has an "error cleared" arm that could never fire, so a q12 timeline showed a
/// device that recovered an hour ago as still erroring.
#[test]
fn last_error_is_cleared_by_a_successful_reopen() {
    let frames = Arc::new(vec![frame_1080p()]);
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let opener: Box<dyn SourceOpener> = {
        let rec2 = Arc::clone(&rec);
        let frames = Arc::clone(&frames);
        Box::new(ScriptedOpener {
            // One failed reopen first, so the message being cleared is a failed-open message on
            // top of the disconnection message: both have to go.
            acts: vec![OpenAct::Give, OpenAct::Fail, OpenAct::Give].into(),
            tail: OpenAct::Give,
            make: Box::new(move |n| {
                if n == 0 {
                    ScriptedSource::new(
                        &rec2,
                        Arc::clone(&frames),
                        vec![Act::Frame(0), Act::Frame(0)],
                        Act::Disconnect,
                    )
                } else {
                    ScriptedSource::new(&rec2, Arc::clone(&frames), vec![], Act::Frame(0))
                }
            }),
            given: 0,
            rec: Arc::clone(&rec),
        })
    };

    let h = Pipeline::start_with_opener(
        opener,
        fast(Duration::from_millis(10), Duration::from_millis(40)),
    );
    wait_for("the disconnection to be reported", PATIENCE, || {
        h.stats().last_error.is_some()
    });
    wait_for("the reopen", PATIENCE, || h.stats().reopens >= 1);
    let after = h.stats().frames_captured;
    wait_for("frames after the reopen", PATIENCE, || {
        h.stats().frames_captured >= after + 30
    });
    wait_for("Running again", PATIENCE, || {
        h.state() == PipelineState::Running
    });

    let st = h.stats();
    h.stop();
    assert_eq!(
        st.last_error, None,
        "the pipeline is Running with frames flowing but still reports the disconnection it \
         recovered from: {st:?}"
    );
    assert_eq!(st.reopens, 1, "{st:?}");
    assert_eq!(st.disconnected_since, None, "{st:?}");
}

/// From the review's "checked and clean" column, kept because the negative result is worth
/// keeping: a frame captured *before* a disconnection cannot be delivered after the reopen
/// carrying a pre-unplug timestamp.
///
/// The compressed slot holds at most one frame and the decode thread is always parked in
/// `wait_take`, so anything the capture thread put there is consumed long before the backoff
/// ends. The second device's sequence numbers are given a disjoint range, so a frame that crossed
/// the gap would be unmistakable. (The *decoded* frame that survives in the output slot is §6.1
/// S1-2 working as intended, and is asserted elsewhere.)
#[test]
fn no_pre_disconnect_frame_is_delivered_after_the_reopen() {
    let frames = Arc::new(vec![frame_1080p()]);
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let opener: Box<dyn SourceOpener> = {
        let rec2 = Arc::clone(&rec);
        let frames = Arc::clone(&frames);
        Box::new(ScriptedOpener {
            acts: vec![OpenAct::Give, OpenAct::Give].into(),
            tail: OpenAct::Give,
            make: Box::new(move |n| {
                let mut s = if n == 0 {
                    ScriptedSource::new(
                        &rec2,
                        Arc::clone(&frames),
                        vec![Act::Frame(0), Act::Frame(0), Act::Frame(0)],
                        Act::Disconnect,
                    )
                } else {
                    ScriptedSource::new(&rec2, Arc::clone(&frames), vec![], Act::Frame(0))
                };
                if n > 0 {
                    s.sequence = 1_000;
                }
                s
            }),
            given: 0,
            rec: Arc::clone(&rec),
        })
    };

    // A gap long enough that a stranded pre-unplug frame would be obvious.
    let h = Pipeline::start_with_opener(
        opener,
        fast(Duration::from_millis(300), Duration::from_millis(300)),
    );
    wait_for("frames before the disconnection", PATIENCE, || {
        h.stats().last_sequence.is_some()
    });
    wait_for("the reopen", PATIENCE, || h.stats().reopens >= 1);
    // The first frame the pipeline reports after the reopen must come from the new device.
    wait_for("a frame from the new device", PATIENCE, || {
        h.stats().last_sequence.is_some_and(|s| s >= 1_000)
    });
    let st = h.stats();
    h.stop();
    assert!(
        st.last_sequence.is_some_and(|s| s >= 1_000),
        "a frame from before the disconnection was delivered after the reopen: {st:?}"
    );
}

/// **Review finding #4.** A child process spawned by a test must not survive the test.
///
/// H-B2 spawns `scripts/usb-replug.py` and reaches its `wait()` only past eight `assert!`s;
/// `std::process::Child` does not kill on drop, so any of those failures left a script that
/// resets USB devices running unsupervised and unreaped — against `00-common.md`'s "kill every
/// process you start". The fix is `support::child_guard::ChildGuard`, and this is the test that
/// the guard actually does it: `sleep` stands in for the script, and the scope that spawned it
/// unwinds the way H-B2's most likely failure does.
#[test]
fn a_spawned_child_is_killed_when_the_scope_that_spawned_it_unwinds() {
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    let published = Arc::new(AtomicU32::new(0));
    let pid = {
        let publish = Arc::clone(&published);
        let handle = std::thread::spawn(move || {
            let guard = child_guard::ChildGuard::spawn(
                "stand-in for scripts/usb-replug.py",
                Command::new("sleep").arg("30"),
            )
            .expect("spawning the stand-in for scripts/usb-replug.py");
            publish.store(guard.pid().expect("a pid"), Ordering::SeqCst);
            // Fail an assertion the way H-B2 does before it ever reaches `wait()`.
            assert_eq!(
                0, 1,
                "stands in for `the node never went away with --method reset`"
            );
        });
        assert!(handle.join().is_err(), "the stand-in assertion must fail");
        published.load(Ordering::SeqCst)
    };
    assert_ne!(pid, 0, "the stand-in child never published a pid");

    // SAFETY: `kill(pid, 0)` performs the permission and existence check only; it delivers no
    // signal. The pid was created by this test moments ago and reaped by the guard, so it names
    // either that child or nothing.
    let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
    if alive {
        // Do not leave it running for the next test to trip over.
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }
    assert!(
        !alive,
        "the subprocess spawned before the failing assertion (pid {pid}) was still running \
         afterwards: H-B2 would leave scripts/usb-replug.py resetting USB devices unsupervised"
    );
}

// ---------------------------------------------------------------------------------------------
// The format watchdog (§6/A6, C4): the device is in the wrong mode, not the client
// ---------------------------------------------------------------------------------------------
//
// Measured on 2026-09-11: a live viewer run through a USB reset reopened the node correctly —
// rediscovered it, `S_FMT` MJPG 1920x1080, `S_PARM` 60 fps — and then streamed 640x480 for the
// remaining two minutes, the V4L2 ERROR flag on the first buffer the only tell. A fresh open by
// `v4l2-ctl` immediately afterwards got 1920x1080 from frame one, so the *stream* was stuck and
// not the device, and a fresh `STREAMON` is what re-commits the format. The benign shape A6
// documents — up to eight frames at the previous resolution after an idle gap — is the same
// symptom for ~135 ms at 60 fps, so the watchdog must sit out the one and answer the other.
//
// Only a source that reports a negotiated mode is watched at all; every other test in this file
// leaves `ScriptedSource::negotiated` at `None` and the watchdog inert.

/// The watchdog timers wound right down, so a test that spends a three-restart budget on a
/// doubling backoff takes a second rather than seven. `restart_after: None` keeps the *stall*
/// restart out of these tests entirely: what is being counted is the watchdog's restarts.
fn watchdog_config(grace: Duration, limit: u32) -> PipelineConfig {
    PipelineConfig {
        stall_after: Duration::from_millis(500),
        restart_after: None,
        reopen_backoff: Duration::from_millis(10),
        reopen_max_backoff: Duration::from_millis(40),
        format_mismatch_grace: Some(grace),
        format_mismatch_restart_limit: limit,
        ..PipelineConfig::default()
    }
}

/// A source that negotiated 1920x1080 and delivers `acts`, then `tail`, forever.
fn watched_source(
    rec: &Arc<Mutex<Recorder>>,
    frames: &Arc<Vec<Vec<u8>>>,
    acts: Vec<Act>,
    tail: Act,
    on_restart: OnRestart,
) -> ScriptedSource {
    let mut s = ScriptedSource::new(rec, Arc::clone(frames), acts, tail);
    s.negotiated = Some((1920, 1080));
    s.on_restart = on_restart;
    s
}

/// The frames these tests use: index 0 is the negotiated 1920x1080, index 1 the 640x480 the
/// dongle powers up in.
fn watchdog_frames() -> Arc<Vec<Vec<u8>>> {
    Arc::new(vec![frame_1080p(), frame_640x480()])
}

/// **The benign transient must not be answered.** A6: after an idle period the device emits up to
/// eight frames at the previous resolution — three here — and then the right ones. That is not a
/// stuck stream and restarting it would cost frames for nothing.
#[test]
fn a_short_wrong_size_transient_never_triggers_the_format_watchdog() {
    let frames = watchdog_frames();
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let grace = Duration::from_millis(200);
    let source = watched_source(
        &rec,
        &frames,
        vec![Act::Frame(1), Act::Frame(1), Act::Frame(1)],
        Act::Frame(0),
        OnRestart::Nothing,
    );
    let h = Pipeline::start(Box::new(source), watchdog_config(grace, 3));

    // The three 640x480 frames take ~15 ms at the scripted 5 ms pace, well inside the grace
    // period — and then many times the grace period of correct frames, so the absence below is
    // observed rather than merely not yet observed.
    wait_for("frames past the transient", PATIENCE, || {
        h.stats().frames_captured >= 3 + (grace.as_millis() as u64 / 5) * 4
    });
    let st = h.stats();
    h.stop();
    assert_eq!(
        st.format_mismatch_restarts, 0,
        "the watchdog fired on the benign A6 transient: {st:?}"
    );
    assert_eq!(st.format_mismatch_accepted, 0, "{st:?}");
    assert!(
        lock(&rec).restarts.is_empty(),
        "restart() was called anyway"
    );
    assert_eq!(st.last_resolution, Some((1920, 1080)));
    assert_eq!(
        st.resolution_changes, 1,
        "640x480 -> 1920x1080 is one change"
    );
}

/// **The measured bug.** Frames keep arriving at the device's power-on 640x480 while `S_FMT`
/// negotiated 1920x1080; the watchdog restarts the stream once, the restart re-commits the
/// format, and that is the end of it.
#[test]
fn frames_stuck_in_the_wrong_mode_are_restarted_once_and_the_stats_say_so() {
    let frames = watchdog_frames();
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let grace = Duration::from_millis(60);
    let source = watched_source(
        &rec,
        &frames,
        vec![],
        Act::Frame(1),
        // The stuck stream: 640x480 until a `STREAMON` re-commits, 1920x1080 after it.
        OnRestart::SwitchTo(0),
    );
    let h = Pipeline::start(Box::new(source), watchdog_config(grace, 3));

    wait_for("the watchdog restart", PATIENCE, || {
        h.stats().format_mismatch_restarts >= 1
    });
    wait_for("frames at the negotiated size", PATIENCE, || {
        h.stats().last_resolution == Some((1920, 1080))
    });
    // Four grace periods of correct frames: a watchdog that kept firing would show here.
    let settled = h.stats().frames_captured;
    wait_for("frames after the restart", PATIENCE, || {
        h.stats().frames_captured >= settled + (grace.as_millis() as u64 / 5) * 4
    });

    let st = h.stats();
    h.stop();
    assert_eq!(
        st.format_mismatch_restarts, 1,
        "exactly one watchdog restart was needed: {st:?}"
    );
    assert_eq!(
        st.format_mismatch_accepted, 0,
        "the restart worked, so nothing should have been accepted: {st:?}"
    );
    assert_eq!(
        st.stream_restarts, 0,
        "a watchdog restart was counted as a stall restart: {st:?}"
    );
    assert_eq!(st.disconnects, 0, "{st:?}");
    assert_eq!(
        lock(&rec).restarts.len(),
        1,
        "restart() was called more than once"
    );
    assert_eq!(st.last_resolution, Some((1920, 1080)));
}

/// **The bound.** A unit that does not rescale internally would legitimately deliver a different
/// size forever (A6, C4 §6 item 3), so the watchdog gets a budget: N restarts on a doubling
/// backoff, then the SOF dimensions are accepted as the truth, said once, and the stream is left
/// alone with the frames still flowing.
#[test]
fn a_stream_that_never_reaches_the_negotiated_mode_is_restarted_n_times_then_accepted() {
    let frames = watchdog_frames();
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let grace = Duration::from_millis(40);
    let limit = 3;
    let source = watched_source(
        &rec,
        &frames,
        vec![],
        Act::Frame(1),
        // Nothing this client does changes what this device sends.
        OnRestart::Nothing,
    );
    let h = Pipeline::start(Box::new(source), watchdog_config(grace, limit));

    wait_for("the acceptance", PATIENCE, || {
        h.stats().format_mismatch_accepted >= 1
    });
    let at_acceptance = h.stats();
    assert_eq!(
        at_acceptance.format_mismatch_restarts, limit as u64,
        "the budget was not exactly {limit} restarts: {at_acceptance:?}"
    );

    // The backoff really doubled. `restarts[k]` is the k-th watchdog restart, and the wait
    // before it is `grace * 2^(k-1)`.
    let restarts = lock(&rec).restarts.clone();
    assert_eq!(restarts.len(), limit as usize, "restart() call count");
    let g1 = restarts[1] - restarts[0];
    let g2 = restarts[2] - restarts[1];
    assert!(
        g1 >= grace * 2,
        "the second watchdog restart came {g1:?} after the first, under the doubled {:?}",
        grace * 2
    );
    assert!(
        g2 >= grace * 4,
        "the third came {g2:?} after the second, under the doubled {:?}",
        grace * 4
    );
    assert!(g2 >= g1, "the backoff did not grow: {g1:?} then {g2:?}");

    // After the acceptance the frames keep coming and nothing else is restarted, for many times
    // the longest wait the watchdog could still be running.
    let after = h.stats().frames_captured;
    wait_for("frames after the acceptance", PATIENCE, || {
        h.stats().frames_captured >= after + 100
    });
    let st = h.stats();
    h.stop();
    assert_eq!(
        st.format_mismatch_restarts, limit as u64,
        "the watchdog kept restarting after it had accepted the size: {st:?}"
    );
    assert_eq!(
        st.format_mismatch_accepted, 1,
        "the acceptance — and the warning that goes with it — happened more than once: {st:?}"
    );
    assert_eq!(lock(&rec).restarts.len(), limit as usize);
    assert_eq!(
        st.last_resolution,
        Some((640, 480)),
        "A6: what the device sends is what is reported"
    );
    assert_eq!(st.disconnects, 0, "{st:?}");
    assert_eq!(st.reopens, 0, "{st:?}");
}

/// A watchdog restart that **panics** is a failed restart, not a lost device — the same rule the
/// stall restart follows (C4), because `V4l2Source::restart` drops the old stream and those
/// destructors panic on any teardown ioctl that fails with something other than `ENODEV`.
#[test]
fn a_panicking_watchdog_restart_is_a_failed_restart_not_a_lost_device() {
    let frames = watchdog_frames();
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let grace = Duration::from_millis(40);
    let limit = 2;
    let source = watched_source(&rec, &frames, vec![], Act::Frame(1), OnRestart::Panic);
    let h = Pipeline::start(Box::new(source), watchdog_config(grace, limit));

    wait_for("the budget spent on panicking restarts", PATIENCE, || {
        h.stats().format_mismatch_accepted >= 1
    });
    let after = h.stats().frames_captured;
    wait_for("frames after the panics", PATIENCE, || {
        h.stats().frames_captured >= after + 20
    });
    let st = h.stats();
    let t = Instant::now();
    h.stop();
    assert!(t.elapsed() < Duration::from_secs(2), "stop() hung");

    assert_eq!(st.format_mismatch_restarts, limit as u64, "{st:?}");
    assert_eq!(
        st.disconnects, 0,
        "a panicking restart was taken for a lost device: {st:?}"
    );
    assert_eq!(st.reopens, 0, "{st:?}");
    assert!(
        st.capture_errors >= limit as u64,
        "the failed restarts were not counted as capture errors: {st:?}"
    );
    assert!(
        st.last_error
            .as_deref()
            .is_some_and(|e| e.contains("restart panicked")),
        "last_error was {:?}",
        st.last_error
    );
}

/// **The live case, end to end.** The device is lost, the pipeline reopens it, the reopened
/// stream comes up in the wrong mode — and the watchdog, re-armed by the reopen, restarts it.
/// This is the run recorded on 2026-09-11, with the fix in place.
#[test]
fn the_format_watchdog_re_arms_after_a_reopen() {
    let frames = watchdog_frames();
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let grace = Duration::from_millis(60);
    let opener: Box<dyn SourceOpener> = {
        let rec2 = Arc::clone(&rec);
        let frames = Arc::clone(&frames);
        Box::new(ScriptedOpener {
            acts: vec![OpenAct::Give, OpenAct::Give].into(),
            tail: OpenAct::Give,
            make: Box::new(move |n| {
                if n == 0 {
                    // A healthy 1920x1080 stream that then goes away: the watchdog must not fire
                    // before the replug either.
                    watched_source(
                        &rec2,
                        &frames,
                        vec![Act::Frame(0), Act::Frame(0), Act::Frame(0)],
                        Act::Disconnect,
                        OnRestart::Nothing,
                    )
                } else {
                    // The device came back in its power-on mode with the commit lost.
                    watched_source(
                        &rec2,
                        &frames,
                        vec![],
                        Act::Frame(1),
                        OnRestart::SwitchTo(0),
                    )
                }
            }),
            given: 0,
            rec: Arc::clone(&rec),
        })
    };

    let h = Pipeline::start_with_opener(opener, watchdog_config(grace, 3));
    wait_for("the reopen", PATIENCE, || h.stats().reopens >= 1);
    wait_for("the watchdog restart after the reopen", PATIENCE, || {
        h.stats().format_mismatch_restarts >= 1
    });
    wait_for("1920x1080 frames again", PATIENCE, || {
        h.stats().last_resolution == Some((1920, 1080))
    });
    let settled = h.stats().frames_captured;
    wait_for("frames after the recovery", PATIENCE, || {
        h.stats().frames_captured >= settled + (grace.as_millis() as u64 / 5) * 4
    });

    let st = h.stats();
    h.stop();
    assert_eq!(st.disconnects, 1, "{st:?}");
    assert_eq!(st.reopens, 1, "{st:?}");
    assert_eq!(
        st.format_mismatch_restarts, 1,
        "the reopened stream took more than one restart to re-commit: {st:?}"
    );
    assert_eq!(st.format_mismatch_accepted, 0, "{st:?}");
    assert_eq!(st.last_resolution, Some((1920, 1080)));
}

// ---------------------------------------------------------------------------------------------
// Adversarial review of the format watchdog (slice G), and the fixes it bought. Every test below
// began as an attack on a claim the slice made in prose; a failing one means the prose and the
// code disagree. Four of the five failed and the code changed to answer them: two gates instead
// of one (frames *and* time), acceptance decided before the doubled wait rather than after it,
// and a single reopen escalation behind the restarts. The one that was rewritten rather than
// merely turned green says so in its own doc comment, with the hardware evidence that made the
// review's premise wrong.
// ---------------------------------------------------------------------------------------------

/// **A gap must not buy a restart.** The grace clock is wall clock from the first mismatched
/// frame, so a gap in which no frame arrives at all does count towards it — and on its own that
/// would let two mismatched frames either side of a gap shorter than `restart_after` (so the
/// stall path does not own it, and does not clear the clock) spend a restart on the evidence of
/// two frames.
///
/// What makes that impossible is the second gate: `PipelineConfig::format_mismatch_frames`
/// consecutive mismatched frames, which a stream delivering nothing cannot produce. The clock
/// does not have to be stopped for the property to hold, and the doc on `format_mismatch_grace`
/// now says that rather than claiming the clock stops.
#[test]
fn g_a_stall_does_not_stop_the_format_watchdog_clock() {
    let frames = watchdog_frames();
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let grace = Duration::from_millis(200);
    // The gap between the two mismatched frames is ~12 scripted timeouts (5 ms of pace plus the
    // pipeline's 20 ms error backoff each, escalating to 100 ms after ten) — comfortably over
    // the 200 ms grace and comfortably under the 2 s `restart_after`, so this is a hiccup the
    // stall path deliberately does not answer.
    let mut acts = vec![Act::Frame(1)];
    acts.extend(std::iter::repeat_n(Act::Timeout, 12));
    acts.push(Act::Frame(1));
    let source = watched_source(&rec, &frames, acts, Act::Frame(0), OnRestart::Nothing);
    let config = PipelineConfig {
        restart_after: Some(Duration::from_secs(2)),
        ..watchdog_config(grace, 3)
    };
    let h = Pipeline::start(Box::new(source), config);

    // Wait until the script is well past the second mismatched frame and into the negotiated
    // ones, so the absence asserted below is observed rather than merely not yet observed.
    wait_for("frames past the scripted gap", PATIENCE, || {
        h.stats().last_resolution == Some((1920, 1080))
    });
    let settled = h.stats().frames_captured;
    wait_for("frames after the gap", PATIENCE, || {
        h.stats().frames_captured >= settled + 40
    });
    let st = h.stats();
    let restarts = lock(&rec).restarts.len();
    h.stop();
    assert_eq!(
        st.stream_restarts, 0,
        "the gap was supposed to be shorter than restart_after: {st:?}"
    );
    assert_eq!(
        st.format_mismatch_restarts, 0,
        "the watchdog spent a restart on two mismatched frames either side of a stall, so the \
         grace clock ran while no frame was arriving — the opposite of what \
         format_mismatch_grace documents: {st:?} (restart() calls: {restarts})"
    );
}

/// **The transient is bounded in frames, so the gate that answers it has to be too.** A6: "up to
/// eight frames at the previous resolution after an idle period". A time-only grace converts that
/// into ~135 ms at 60 fps and into 800 ms at 10 fps, where a flat 500 ms grace would buy a restart
/// the stream did not need, at the worst possible moment — it has just come back. The frame gate
/// (`PipelineConfig::format_mismatch_frames`, 12 > A6's 8) is what makes the transient
/// un-triggerable at any rate; this test runs it at the shipped 500 ms grace and a tenth of the
/// frame rate the grace was justified at.
#[test]
fn g_the_eight_frame_transient_is_restarted_when_the_stream_is_slow() {
    let frames = watchdog_frames();
    let rec = Arc::new(Mutex::new(Recorder::default()));
    // The shipped default, not a wound-down one: this is about the real 500 ms.
    let grace = Duration::from_millis(500);
    let mut source = watched_source(
        &rec,
        &frames,
        std::iter::repeat_n(Act::Frame(1), 8).collect(),
        Act::Frame(0),
        OnRestart::Nothing,
    );
    source.pace = Duration::from_millis(100); // 10 fps
    let h = Pipeline::start(Box::new(source), watchdog_config(grace, 3));

    wait_for("frames past the eight-frame transient", PATIENCE, || {
        h.stats().last_resolution == Some((1920, 1080))
    });
    let settled = h.stats().frames_captured;
    wait_for("frames after the transient", PATIENCE, || {
        h.stats().frames_captured >= settled + 10
    });
    let st = h.stats();
    h.stop();
    assert_eq!(
        st.format_mismatch_restarts, 0,
        "the watchdog restarted the stream for A6's benign eight-frame transient because the \
         stream was slow: eight frames at 10 fps is 800 ms and the grace is a flat 500 ms: {st:?}"
    );
}

/// **The escalation, and its bound.** *Rewritten by the fixer from the reviewer's
/// `g_an_unfixable_mismatch_never_escalates_to_a_reopen`, and this note is why.*
///
/// The review argued that `restart()` — `REQBUFS`/`QBUF`/`STREAMON` on the same fd, with no
/// `S_FMT` and no `S_PARM` — is strictly weaker than the reopen that had already failed, and that
/// a reopen "was never observed to recover the mode", so the watchdog should escalate straight to
/// one. The first half is true about the *ioctls*; the second half is contradicted by the
/// hardware. The second live run of 2026-09-11
/// (`viewer-live-replug2.log`, 17:45:55–17:45:56) is a full reopen coming up at 640x480, the
/// watchdog restarting the stream after 516.7 ms, and the **very next frame at 1920x1080**. One
/// restart, fixed, on the exact failure this watchdog was written for. The likely mechanism is
/// timing rather than strength — the reopen lands about half a second after re-enumeration while
/// the device is still settling, and a `STREAMON` half a second later re-commits — which is also
/// why repeating the reopen *later* is the right escalation rather than the first move.
///
/// So `restart()` stays the first remedy and the reviewer's escalation is adopted behind it, with
/// the bound the review itself asked for. The contract this test pins is the bound: a stream that
/// nothing fixes gets its restart budget, then **exactly one** reopen with a fresh budget, then
/// acceptance — and no further reopen, ever, for that run of mismatched frames. A device that
/// never reaches the negotiated mode must end in acceptance, not in a reopen loop.
#[test]
fn g_an_unfixable_mismatch_escalates_to_exactly_one_reopen_then_accepts() {
    let frames = watchdog_frames();
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let grace = Duration::from_millis(40);
    let limit = 3;
    let opener: Box<dyn SourceOpener> = {
        let rec2 = Arc::clone(&rec);
        let frames = Arc::clone(&frames);
        Box::new(ScriptedOpener {
            acts: vec![OpenAct::Give].into(),
            tail: OpenAct::Give,
            // Every open gives the same thing: a unit that simply does not deliver the negotiated
            // mode. Neither a `STREAMON` nor a fresh open changes it, which is the hardware A6
            // warns about — one that does not rescale internally.
            make: Box::new(move |_| {
                watched_source(&rec2, &frames, vec![], Act::Frame(1), OnRestart::Nothing)
            }),
            given: 0,
            rec: Arc::clone(&rec),
        })
    };
    let h = Pipeline::start_with_opener(opener, watchdog_config(grace, limit));

    wait_for("the escalation reopen", PATIENCE, || {
        h.stats().format_mismatch_reopens >= 1
    });
    wait_for("the acceptance after it", PATIENCE, || {
        h.stats().format_mismatch_accepted >= 1
    });
    // Many times the longest wait the watchdog could still be in, so "it never reopened again" is
    // observed rather than merely not yet observed.
    let after = h.stats().frames_captured;
    wait_for("frames after the acceptance", PATIENCE, || {
        h.stats().frames_captured >= after + 200
    });
    let st = h.stats();
    let opens = lock(&rec).opens.len();
    h.stop();

    assert_eq!(
        st.format_mismatch_reopens, 1,
        "the watchdog escalated more than once on one run of mismatched frames: {st:?}"
    );
    assert_eq!(
        st.reopens, 1,
        "the escalation is a reopen and is counted as one, exactly once: {st:?}"
    );
    assert_eq!(
        opens, 2,
        "the opener was called {opens} time(s): the first open plus the one escalation, and no \
         reopen loop"
    );
    assert_eq!(
        st.format_mismatch_accepted, 1,
        "the run ended in something other than one acceptance: {st:?}"
    );
    assert_eq!(
        st.format_mismatch_restarts,
        u64::from(limit) * 2,
        "the escalation is supposed to buy exactly one more full restart budget: {st:?}"
    );
    assert_eq!(
        st.disconnects, 0,
        "the escalation was counted as a lost device; nothing was lost: {st:?}"
    );
    assert_eq!(
        st.last_resolution,
        Some((640, 480)),
        "A6: what the device sends is what is reported"
    );
}

/// **The escalation earns its place.** The same shape as the test above, except that the device
/// *can* be fixed by a fresh open — the `v4l2-ctl` outcome of 2026-09-11, where a new open of the
/// node got 1920x1080 from frame one. The restarts do nothing, the one escalation reopen recovers
/// the mode, and nothing is ever accepted.
#[test]
fn g_the_escalation_reopen_recovers_a_stream_no_restart_could_fix() {
    let frames = watchdog_frames();
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let grace = Duration::from_millis(40);
    let limit = 3;
    let opener: Box<dyn SourceOpener> = {
        let rec2 = Arc::clone(&rec);
        let frames = Arc::clone(&frames);
        Box::new(ScriptedOpener {
            acts: vec![OpenAct::Give].into(),
            tail: OpenAct::Give,
            make: Box::new(move |n| {
                if n == 0 {
                    // A stream stuck in the power-on mode that no `STREAMON` on the same fd
                    // re-commits.
                    watched_source(&rec2, &frames, vec![], Act::Frame(1), OnRestart::Nothing)
                } else {
                    // A fresh open gets the negotiated mode from frame one.
                    watched_source(&rec2, &frames, vec![], Act::Frame(0), OnRestart::Nothing)
                }
            }),
            given: 0,
            rec: Arc::clone(&rec),
        })
    };
    let h = Pipeline::start_with_opener(opener, watchdog_config(grace, limit));

    wait_for("the escalation reopen", PATIENCE, || {
        h.stats().format_mismatch_reopens >= 1
    });
    wait_for("frames at the negotiated size", PATIENCE, || {
        h.stats().last_resolution == Some((1920, 1080))
    });
    let settled = h.stats().frames_captured;
    wait_for("frames after the recovery", PATIENCE, || {
        h.stats().frames_captured >= settled + 200
    });
    let st = h.stats();
    h.stop();

    assert_eq!(st.format_mismatch_reopens, 1, "{st:?}");
    assert_eq!(st.reopens, 1, "{st:?}");
    assert_eq!(
        st.format_mismatch_accepted, 0,
        "the reopen recovered the mode, so nothing should have been accepted: {st:?}"
    );
    assert_eq!(
        st.format_mismatch_restarts,
        u64::from(limit),
        "one budget before the escalation, and none needed after it: {st:?}"
    );
    assert_eq!(
        st.disconnects, 0,
        "the escalation was counted as a lost device: {st:?}"
    );
    assert_eq!(st.last_resolution, Some((1920, 1080)));
}

/// **Acceptance must not wait out one more doubled grace.** The doubling exists to space
/// restarts out; once the budget is spent there is no restart left to space, so a further
/// `grace * 2^limit` — 4 s at the shipped defaults — would be pure silence, and it would arrive
/// as `format_mismatch_accepted` long after the watchdog had in fact given up. H-B2's failure
/// message would then print "accepted 0 time(s)" in exactly the case where the watchdog had given
/// up, and read as "still trying". So the budget is checked *before* the doubled gate and the
/// decision comes one plain grace after the last restart.
///
/// This pipeline is a non-recovering one (`Pipeline::start`), which has no second source to open,
/// so a spent budget is the whole ladder and the decision here is the acceptance itself.
#[test]
fn g_acceptance_waits_one_more_doubled_grace_after_the_last_restart() {
    let frames = watchdog_frames();
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let grace = Duration::from_millis(50);
    let limit = 3;
    let source = watched_source(&rec, &frames, vec![], Act::Frame(1), OnRestart::Nothing);
    let h = Pipeline::start(Box::new(source), watchdog_config(grace, limit));

    wait_for("the whole budget", PATIENCE, || {
        h.stats().format_mismatch_restarts >= u64::from(limit)
    });
    let last_restart = *lock(&rec)
        .restarts
        .last()
        .expect("the budget was spent, so restart() was called");
    wait_for("the acceptance", PATIENCE, || {
        h.stats().format_mismatch_accepted >= 1
    });
    let lag = last_restart.elapsed();
    h.stop();
    assert!(
        lag < grace * 2,
        "the watchdog took {lag:?} after its last restart to decide it had given up, a further \
         grace * 2^{limit} = {:?} of silence with no restart left to space out",
        grace * (1 << limit)
    );
}

/// **The mirror of the clock finding: a stall restart must not wipe the watchdog's evidence.**
///
/// A gap *longer* than `restart_after` fires the stall restart, and that restart used to clear
/// `mismatch_since` — without refilling the watchdog's budget, so a stream that is both
/// intermittent and stuck in the wrong mode had its grace reset over and over and the watchdog
/// never fired at all. The clock and the frame count now belong to the watchdog alone: the stall
/// path resets its own timer (`restart_base`) and touches nothing else, so the mismatched frames
/// either side of a stall restart still add up.
#[test]
fn g_a_stall_restart_does_not_wipe_the_format_watchdogs_evidence() {
    let frames = watchdog_frames();
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let grace = Duration::from_millis(20);
    // One 640x480 frame, then a gap long enough for the stall timer, over and over: six scripted
    // timeouts at 5 ms of pace plus the pipeline's 20 ms error backoff is ~150 ms, comfortably
    // over the 100 ms `restart_after` below.
    let mut acts = Vec::new();
    for _ in 0..40 {
        acts.push(Act::Frame(1));
        acts.extend(std::iter::repeat_n(Act::Timeout, 6));
    }
    let source = watched_source(&rec, &frames, acts, Act::Frame(1), OnRestart::Nothing);
    let config = PipelineConfig {
        restart_after: Some(Duration::from_millis(100)),
        ..watchdog_config(grace, 3)
    };
    let h = Pipeline::start(Box::new(source), config);

    wait_for("a watchdog restart despite the stalls", PATIENCE, || {
        h.stats().format_mismatch_restarts >= 1
    });
    let st = h.stats();
    h.stop();
    assert!(
        st.stream_restarts >= 1,
        "the stall timer never fired, so this test did not exercise the interaction it is about: \
         {st:?}"
    );
    assert!(
        st.format_mismatch_restarts >= 1,
        "the stall restarts kept clearing the watchdog's clock, so it never fired on a stream \
         that was stuck in the wrong mode the whole time: {st:?}"
    );
}

/// **Verification, not an attack.** The slice claims a frame at the negotiated size undoes an
/// acceptance and re-arms the watchdog with a full budget, and that a later mismatch starts the
/// grace again from the top. It does.
#[test]
fn g_acceptance_is_undone_by_a_negotiated_frame_and_the_grace_starts_again() {
    let frames = watchdog_frames();
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let grace = Duration::from_millis(40);
    let limit = 2;
    // Mismatched until the budget is spent and the mismatch accepted; then one healthy stretch,
    // which must re-arm; then mismatched again, which must be answered from a full budget.
    let mut acts: Vec<Act> = std::iter::repeat_n(Act::Frame(1), 200).collect();
    acts.extend(std::iter::repeat_n(Act::Frame(0), 10));
    let source = watched_source(&rec, &frames, acts, Act::Frame(1), OnRestart::Nothing);
    let h = Pipeline::start(Box::new(source), watchdog_config(grace, limit));

    wait_for("the first acceptance", PATIENCE, || {
        h.stats().format_mismatch_accepted >= 1
    });
    wait_for("the re-arming frames", PATIENCE, || {
        h.stats().last_resolution == Some((1920, 1080))
    });
    // The budget was refilled, so the second run of mismatched frames gets its own N restarts
    // and its own acceptance.
    wait_for("the second acceptance", PATIENCE, || {
        h.stats().format_mismatch_accepted >= 2
    });
    let st = h.stats();
    h.stop();
    assert_eq!(
        st.format_mismatch_restarts,
        u64::from(limit) * 2,
        "the re-armed watchdog did not get a full budget: {st:?}"
    );
    assert_eq!(st.disconnects, 0, "{st:?}");
    assert_eq!(st.reopens, 0, "{st:?}");
}
