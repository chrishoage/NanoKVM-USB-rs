//! Capture diagnostic timeline using the production pipeline.
//!
//! Run `cargo run --example capture-probe -- --help` for options. Verify the selected
//! node belongs to capture device 345f:2133; `/dev/video4` is only a recorded example.
//! Never open the user's unrelated bus-5 devices or change the target's HDMI setup.
//!
//! Reports JPEG dimensions, negotiated format, errors, and recovery over time.
//! Polling occurs at 40 Hz; per-frame counters detect changes between samples.
//! Frame pixels do not establish HDMI signal state. Historical observations are
//! in `docs/hardware.md`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use clap::Parser;

use nanokvm::capture::v4l2::query_format;
use nanokvm::capture::{BytesUsedStats, Pipeline, PipelineConfig, V4l2Opener};

/// Set by the `SIGINT` handler and polled by the sampling loop, as `main.rs` does: a
/// signal handler may only do async-signal-safe things, and an atomic store is one of them.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_interrupt(_signal: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// How often `stats()` is sampled. 40 Hz: above the contract's 20 Hz floor, and above the 16.7 ms
/// frame period at 60 fps, so no state transition can hide between two samples.
const SAMPLE: Duration = Duration::from_millis(25);

/// A frame gap longer than this is worth reporting on its own line and in the summary. One second
/// is far beyond any hiccup at 60 fps and far short of a target reboot.
const GAP: Duration = Duration::from_secs(1);

#[derive(Parser, Debug)]
#[command(
    name = "capture-probe",
    about = "Stream from the capture node and log every state, dimension and format change"
)]
struct Args {
    /// Video node of the capture device. /dev/video4 is the dongle on the recorded test setup; /dev/video0-3
    /// belong to unrelated hardware and must never be opened (CLAUDE.md).
    #[arg(long, value_name = "PATH", default_value = "/dev/video4")]
    video: PathBuf,

    /// Stop after this many seconds. Omitted, it runs until SIGINT.
    #[arg(long, value_name = "SECS")]
    seconds: Option<u64>,

    /// Capture width in pixels.
    #[arg(long, default_value_t = 1920)]
    width: u32,

    /// Capture height in pixels.
    #[arg(long, default_value_t = 1080)]
    height: u32,

    /// Capture frame rate, passed to S_PARM explicitly.
    #[arg(long, default_value_t = 60)]
    fps: u32,
}

/// One observed frame gap.
struct Gap {
    /// Milliseconds since the probe started, at the last frame before the gap.
    at_ms: f64,
    duration: Duration,
}

/// How long frames were arriving at one SOF resolution, in first-seen order.
///
/// Time with no frames is not in here: not the outage while the pipeline reconnects, and not
/// a stall. A run that spanned a 40 s replug would otherwise report the resolution as having been
/// "seen" for 40 s in which nothing was seen at all, and the recovery timeline is the one place that
/// distinction is the entire point.
struct ModeTime {
    dims: (u32, u32),
    /// Summed over the sample intervals in which at least one frame arrived, so it is quantised
    /// to the 25 ms sample. Frames come at 16.7 ms at 60 fps, so in steady streaming every sample
    /// counts and this tracks the wall clock.
    streaming: Duration,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    // SAFETY: `on_interrupt` is an `extern "C"` function whose entire body is one atomic store,
    // which is async-signal-safe. `libc::signal` is handed a valid function pointer and the
    // previous disposition is discarded, as `main.rs` does.
    unsafe {
        libc::signal(libc::SIGINT, on_interrupt as libc::sighandler_t);
    }

    let started = Instant::now();
    let stamp = |t: Instant| (t - started).as_secs_f64() * 1e3;
    let now_ms = || started.elapsed().as_secs_f64() * 1e3;

    let config = PipelineConfig::default();
    println!(
        "# capture-probe on {} at {}x{}@{} fps",
        args.video.display(),
        args.width,
        args.height,
        args.fps
    );
    println!(
        "# recovery: restart_after {:?}, reopen backoff {:?} doubling to {:?}, stall_after {:?}",
        config.restart_after, config.reopen_backoff, config.reopen_max_backoff, config.stall_after
    );
    println!("# every line is milliseconds since start; SOF dimensions are A6's authority, G_FMT is the driver's opinion");

    let opener = Box::new(V4l2Opener::new(
        args.video.clone(),
        args.width,
        args.height,
        args.fps,
    ));
    let pipeline = Pipeline::start_with_opener(opener, config);
    let output = pipeline.output();

    // Everything the timeline is derived from. All of it comes from `stats()` and the clock;
    // none of it from the image.
    let mut state = pipeline.state();
    let mut last_error: Option<String> = None;
    let mut dims: Option<(u32, u32)> = None;
    // The pipeline's own count of resolution changes, mirrored so the timeline can tell a change
    // it *saw* from one it only heard about (flap — see the dimension block below).
    let mut changes: u64 = 0;
    let mut transitions_printed: u64 = 0;
    let mut mode_time: Vec<ModeTime> = Vec::new();
    let mut gaps: Vec<Gap> = Vec::new();
    let mut last_frame_at: Option<Instant> = None;
    let mut in_gap = false;
    let mut disconnected_at: Option<Instant> = None;
    let mut last_sample = Instant::now();

    let mut counters = Counters::default();
    let mut second = Instant::now();
    let mut frames_at_second = 0u64;

    println!("{:>10.1}  start  state={state:?}", now_ms());

    let deadline = args.seconds.map(|s| started + Duration::from_secs(s));
    loop {
        if INTERRUPTED.load(Ordering::SeqCst) {
            println!("{:>10.1}  SIGINT", now_ms());
            break;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }

        // Take whatever the renderer would have taken. The probe is the consumer here, so
        // without this every decoded frame would be counted as dropped post-decode and the
        // numbers would describe a pipeline nobody runs.
        while output.take().is_some() {}

        let st = pipeline.stats();

        if st.state != state {
            println!("{:>10.1}  state {state:?} -> {:?}", now_ms(), st.state);
            state = st.state;
        }
        if st.last_error != last_error {
            match &st.last_error {
                Some(e) => println!("{:>10.1}  error: {e}", now_ms()),
                None => println!("{:>10.1}  error cleared", now_ms()),
            }
            last_error = st.last_error.clone();
        }

        // How much of the wall clock this sample covers, and whether any frame arrived in it.
        // Both are needed before the dimension block below, which may move `dims`.
        let now = Instant::now();
        let slice = now.saturating_duration_since(last_sample);
        last_sample = now;
        if st.last_frame_at != last_frame_at {
            if let Some(d) = dims {
                match mode_time.iter_mut().find(|m| m.dims == d) {
                    Some(m) => m.streaming += slice,
                    None => mode_time.push(ModeTime {
                        dims: d,
                        streaming: slice,
                    }),
                }
            }
        }

        // SOF dimensions: this is the only authority on a frame's size, so a target-side
        // mode change appears here first, whatever G_FMT goes on saying.
        //
        // The *count* comes from `PipelineStats::resolution_changes`, which the capture thread
        // increments per frame, not from comparing `last_resolution` between samples. the
        // device emits up to eight frames at the previous resolution after an idle period, so a
        // target mode change arrives as a flap — and an excursion shorter than one 25 ms sample
        // (1.5 frames at 60 fps) is invisible to a sampler. Counting the pipeline's number means
        // the flap is still *reported*, even when the intermediate mode was never sampled; the
        // NOTE line below is that report.
        if st.resolution_changes != changes || st.last_resolution != dims {
            let missed = st.resolution_changes.saturating_sub(changes);
            if let Some(new) = st.last_resolution {
                let seq = st
                    .last_sequence
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "?".to_string());
                match dims {
                    Some(old) => {
                        println!(
                            "{:>10.1}  SOF dims {}x{} -> {}x{}  sequence {seq}  bytesused {}  \
                             (pipeline change #{})",
                            now_ms(),
                            old.0,
                            old.1,
                            new.0,
                            new.1,
                            st.bytes.last,
                            st.resolution_changes
                        );
                        transitions_printed += 1;
                    }
                    None => println!(
                        "{:>10.1}  SOF dims {}x{}  sequence {seq}  bytesused {}",
                        now_ms(),
                        new.0,
                        new.1,
                        st.bytes.last
                    ),
                }
            }
            if missed > 1 {
                println!(
                    "{:>10.1}  NOTE  {missed} resolution changes inside one {:?} sample; the \
                     modes between them were never sampled (A6's post-idle flap). The count is \
                     the pipeline's, per frame; the line above shows only where it ended up.",
                    now_ms(),
                    SAMPLE
                );
            }
            dims = st.last_resolution;
            changes = st.resolution_changes;
        }

        counters.report(&st, now_ms(), &mut disconnected_at);

        // Frame gaps, measured on the host clock between successive captures.
        if st.last_frame_at != last_frame_at {
            if let (Some(prev), Some(new)) = (last_frame_at, st.last_frame_at) {
                let gap = new.saturating_duration_since(prev);
                if gap >= GAP {
                    println!("{:>10.1}  frames resumed after a {gap:.3?} gap", stamp(new));
                    gaps.push(Gap {
                        at_ms: stamp(prev),
                        duration: gap,
                    });
                }
            }
            last_frame_at = st.last_frame_at;
            in_gap = false;
        } else if let Some(prev) = last_frame_at {
            if !in_gap && prev.elapsed() >= GAP {
                println!("{:>10.1}  no frame for {:.3?}", now_ms(), prev.elapsed());
                in_gap = true;
            }
        }

        if second.elapsed() >= Duration::from_secs(1) {
            let elapsed = second.elapsed();
            second = Instant::now();
            let window: BytesUsedStats = pipeline.take_bytes_window();
            let frames = st.frames_captured - frames_at_second;
            frames_at_second = st.frames_captured;
            let fmt = match query_format(&args.video) {
                Ok((f, (num, den))) => format!(
                    "G_FMT {} {}x{} sizeimage={}  G_PARM {num}/{den} ({:.3} fps)",
                    f.fourcc,
                    f.width,
                    f.height,
                    f.size,
                    if num == 0 {
                        0.0
                    } else {
                        f64::from(den) / f64::from(num)
                    }
                ),
                // While the pipeline is reconnecting there is no node to ask. That is not a
                // probe failure, it is the reading.
                Err(e) => format!("no device ({e})"),
            };
            let sizes = if window.count == 0 {
                "bytesused none".to_string()
            } else {
                format!(
                    "bytesused min={} mean={:.0} max={}",
                    window.min,
                    window.mean(),
                    window.max
                )
            };
            println!(
                "{:>10.1}  1s  state={:?}  frames={frames} ({:.1} fps)  {sizes}  {fmt}",
                now_ms(),
                st.state,
                frames as f64 / elapsed.as_secs_f64()
            );
        }

        std::thread::sleep(SAMPLE);
    }

    // Close the run's open book before the summary: the gap that had not ended. (The per-mode
    // times need no closing — they are accumulated per sample, not per run.)
    if let Some(prev) = last_frame_at {
        let open = prev.elapsed();
        if open >= GAP {
            gaps.push(Gap {
                at_ms: stamp(prev),
                duration: open,
            });
        }
    }

    let st = pipeline.stats();
    println!();
    println!("== summary after {:.1} ms ==", now_ms());
    // The post-decode drops are the probe's own sampling rate, not a pipeline fault: it drains
    // the output slot once per 25 ms sample while the decoder produces 60 frames a second, so a
    // third of them are displaced by design. A renderer takes every one.
    println!(
        "  frames: captured {} decoded {}  dropped pre-decode {} post-decode {} (the probe \n\
         samples at 40 Hz; post-decode drops are its own doing)  decode errors {}",
        st.frames_captured,
        st.frames_decoded,
        st.dropped_pre_decode,
        st.dropped_post_decode,
        st.decode_errors
    );
    println!(
        "  capture errors {}  stream restarts {}  disconnects {}  reopens {} in {} attempts",
        st.capture_errors, st.stream_restarts, st.disconnects, st.reopens, st.reopen_attempts
    );
    // The format watchdog, which is the half of the story the counters above cannot tell: a
    // reopen that "worked" can still leave the stream in the device's power-on mode, and the
    // only way that shows in a log is here (see `PipelineConfig::format_mismatch_grace`).
    println!(
        "  format-mismatch restarts {} (SOF disagreed with the negotiated mode for longer than \
         the grace period, across {} escalation reopen(s)){}",
        st.format_mismatch_restarts,
        st.format_mismatch_reopens,
        if st.format_mismatch_accepted > 0 {
            format!(
                "\n    ACCEPTED {} time(s): the negotiated format could not be established and \
                 the SOF dimensions were taken as the truth",
                st.format_mismatch_accepted
            )
        } else {
            String::new()
        }
    );
    println!("  bytesused over the whole run: {}", st.bytes);
    // The cross-check, printed rather than computed and thrown away: the pipeline counts a change
    // per frame and cannot miss one; this timeline samples at 40 Hz and can. When they disagree,
    // the difference is the number of changes that came and went inside one sample — which is
    // a mode change that leaves the negotiated format unchanged.
    println!(
        "  resolution changes: {} counted by the pipeline (per frame), {} printed above",
        st.resolution_changes, transitions_printed
    );
    if st.resolution_changes != transitions_printed {
        println!(
            "    they DISAGREE by {}: that many changes were shorter than one {:?} sample and \
             are counted but not in the timeline. The pipeline's count is the authority.",
            st.resolution_changes.abs_diff(transitions_printed),
            SAMPLE
        );
    } else {
        println!("    they agree, so the timeline above accounts for every change");
    }
    if mode_time.is_empty() {
        println!("  resolutions seen: none — no frame ever arrived");
    } else {
        println!("  time with frames actually arriving, per resolution");
        println!("  (outages and stalls excluded, so these need not add up to the run):");
        for m in &mode_time {
            println!("    {}x{} for {:.3?}", m.dims.0, m.dims.1, m.streaming);
        }
    }
    if gaps.is_empty() {
        println!("  gaps over {GAP:?}: none");
    } else {
        println!("  gaps over {GAP:?}: {}", gaps.len());
        for g in &gaps {
            println!("    at {:>10.1} ms  {:.3?}", g.at_ms, g.duration);
        }
    }
    if let Some(e) = &st.last_error {
        println!("  last error: {e}");
    }

    pipeline.stop();
}

/// The counter deltas the timeline reports, and the timings derived from them.
#[derive(Default)]
struct Counters {
    disconnects: u64,
    reopens: u64,
    reopen_attempts: u64,
    stream_restarts: u64,
    format_mismatch_restarts: u64,
    format_mismatch_reopens: u64,
    format_mismatch_accepted: u64,
}

impl Counters {
    /// Print a line for every counter that moved since the last sample.
    ///
    /// `disconnected_at` carries the disconnection's start across the reopen, because
    /// `PipelineStats::disconnected_since` is cleared the instant the reopen succeeds — which is
    /// the moment the duration becomes interesting.
    fn report(
        &mut self,
        st: &nanokvm::capture::PipelineStats,
        now_ms: f64,
        disconnected_at: &mut Option<Instant>,
    ) {
        if st.disconnects > self.disconnects {
            println!(
                "{now_ms:>10.1}  DISCONNECT #{} ({})",
                st.disconnects,
                st.last_error.as_deref().unwrap_or("no error recorded")
            );
            *disconnected_at = st.disconnected_since;
            self.disconnects = st.disconnects;
        }
        if st.reopen_attempts > self.reopen_attempts {
            println!(
                "{now_ms:>10.1}  reopen attempt #{} ({} failed so far)",
                st.reopen_attempts,
                st.reopen_attempts - st.reopens
            );
            self.reopen_attempts = st.reopen_attempts;
        }
        if st.reopens > self.reopens {
            match disconnected_at.take() {
                Some(t) => println!(
                    "{now_ms:>10.1}  REOPENED #{} — device was away {:.3?}",
                    st.reopens,
                    t.elapsed()
                ),
                None => println!("{now_ms:>10.1}  REOPENED #{}", st.reopens),
            }
            self.reopens = st.reopens;
        }
        if st.stream_restarts > self.stream_restarts {
            println!(
                "{now_ms:>10.1}  stream restart #{} (stalled for {:?})",
                st.stream_restarts,
                st.last_frame_at.map(|t| t.elapsed())
            );
            self.stream_restarts = st.stream_restarts;
        }
        if st.format_mismatch_restarts > self.format_mismatch_restarts {
            println!(
                "{now_ms:>10.1}  FORMAT-MISMATCH restart #{} (frames were {}, not the \
                 negotiated mode)",
                st.format_mismatch_restarts,
                match st.last_resolution {
                    Some((w, h)) => format!("{w}x{h}"),
                    None => "of unknown size".to_string(),
                }
            );
            self.format_mismatch_restarts = st.format_mismatch_restarts;
        }
        if st.format_mismatch_reopens > self.format_mismatch_reopens {
            println!(
                "{now_ms:>10.1}  FORMAT-MISMATCH reopen #{}: the restarts did not commit the \
                 negotiated mode, so the device is being opened again (at most one per run of \
                 mismatched frames)",
                st.format_mismatch_reopens
            );
            self.format_mismatch_reopens = st.format_mismatch_reopens;
        }
        if st.format_mismatch_accepted > self.format_mismatch_accepted {
            println!(
                "{now_ms:>10.1}  format mismatch ACCEPTED #{}: the negotiated mode could not be \
                 established; the SOF dimensions are the truth from here",
                st.format_mismatch_accepted
            );
            self.format_mismatch_accepted = st.format_mismatch_accepted;
        }
        // Resolution changes are not reported here: the main loop needs the *previous*
        // dimensions to print a transition, and reporting them in two places would double the
        // lines. It uses the same counter, `PipelineStats::resolution_changes`.
    }
}
