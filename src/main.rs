//! `nanokvm` — the Stage 1 viewer (plan §12).
//!
//! Wiring only: parse arguments, open the two devices, start the three subsystems and hand the
//! event loop over to [`nanokvm::viewer::run`].
//!
//! The order below is deliberate. **Serial comes up first and is never taken down by the capture
//! path** (§6.1 S1-1): sending a chord to a target with no video is a primary use case, so a
//! capture failure after startup must not tear down input. A capture failure *at* startup is
//! fatal only because there is nothing to look at yet.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;

use nanokvm::capture::{Pipeline, PipelineConfig, V4l2Source};
use nanokvm::input::{self, ReleaseOutcome};
use nanokvm::serial::{self, SerialLink};
use nanokvm::viewer::{self, PointerMode, ViewerConfig, RELEASE_KEY};

/// Set by the `SIGINT` handler and polled by the event loop.
///
/// A signal handler may only do async-signal-safe things, and storing to an `AtomicBool` is one
/// of them. Everything the interrupt actually causes — the release-all, the clean exit — happens
/// on the event loop thread when it next notices (§2.6).
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_interrupt(_signal: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

#[derive(Parser, Debug)]
#[command(
    name = "nanokvm",
    about = "Native Linux client for the Sipeed NanoKVM-USB",
    after_help = concat!(
        "Capture and release:\n",
        "  Click in the window, or press Enter, to capture the keyboard and mouse.\n",
        "  Press Pause to release. On niri, Mod+Escape also releases: the compositor keeps that\n",
        "  binding for itself even while shortcuts are inhibited, so it always works.\n",
        "  Focus loss releases too, and every release sends a full key-and-button release to the\n",
        "  target.\n",
    )
)]
struct Args {
    /// Serial node of the CH9329 bridge, e.g. /dev/ttyACM1.
    #[arg(long, value_name = "PATH")]
    serial: PathBuf,

    /// Video node of the capture device, e.g. /dev/video4.
    #[arg(long, value_name = "PATH")]
    video: PathBuf,

    /// Capture width in pixels.
    #[arg(long, default_value_t = 1920)]
    width: u32,

    /// Capture height in pixels.
    #[arg(long, default_value_t = 1080)]
    height: u32,

    /// Capture frame rate. Passed to S_PARM explicitly; without it the driver keeps its own
    /// default, which is 240 fps at 1080p (§6, A7).
    #[arg(long, default_value_t = 60)]
    fps: u32,

    /// Pointer mode. Absolute can reach a specific pixel; relative cannot, because the target
    /// applies its own pointer acceleration (§3.4).
    #[arg(long, value_enum, default_value_t = PointerArg::Abs)]
    pointer: PointerArg,

    /// Seconds between statistics lines; 0 disables them.
    #[arg(long, value_name = "SECS", default_value_t = 5)]
    stats_interval: u64,

    /// Quit cleanly after this many seconds. Development flag for bounded hardware runs.
    #[arg(long, value_name = "SECS", hide = true)]
    exit_after: Option<u64>,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, clap::ValueEnum)]
enum PointerArg {
    Abs,
    Relative,
}

impl From<PointerArg> for PointerMode {
    fn from(a: PointerArg) -> Self {
        match a {
            PointerArg::Abs => PointerMode::Absolute,
            PointerArg::Relative => PointerMode::Relative,
        }
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    // SAFETY: `on_interrupt` is an `extern "C"` function whose entire body is one atomic store,
    // which is async-signal-safe. `libc::signal` is being handed a valid function pointer, and
    // the previous disposition is deliberately discarded — there is none worth restoring in a
    // program that installs this once at startup.
    unsafe {
        libc::signal(libc::SIGINT, on_interrupt as libc::sighandler_t);
    }

    // ---- serial, first and independent of everything else (§6.1 S1-1) --------------------
    let mut link = SerialLink::open(&args.serial).with_context(|| {
        format!(
            "opening the CH9329 serial link on {}. Check the path and that you can write to it \
             (it is usually owned by the 'uucp' or 'dialout' group).",
            args.serial.display()
        )
    })?;

    // The only handshake this device needs, and the one thing that proves the link answers
    // (Appendix). A failure here is fatal: without it we do not know there is a CH9329 there.
    let info = link
        .get_info(Duration::from_millis(500))
        .with_context(|| format!("GET_INFO on {} did not answer", args.serial.display()))?;
    log::info!(
        "CH9329 firmware {:.1}, target {}, locks: num={} caps={} scroll={}",
        info.version,
        if info.target_connected {
            "connected"
        } else {
            "NOT connected"
        },
        info.num_lock,
        info.caps_lock,
        info.scroll_lock
    );
    if !info.target_connected {
        log::warn!(
            "the device reports no target on its HID side; keystrokes will go nowhere until the \
             target enumerates it"
        );
    }

    // The device pushes an unsolicited 0x81 frame about 15 ms after a lock-key state change
    // (A12). A lock-bit change is proof the target processed a keystroke, which is the cheapest
    // end-to-end check there is — so it is logged rather than dropped.
    {
        let unsolicited = link.take_unsolicited();
        std::thread::Builder::new()
            .name("nanokvm-locks".to_string())
            .spawn(move || {
                for reply in unsolicited {
                    if let Some(state) = serial::lock_state_from(&reply) {
                        log::info!(
                            "target lock state: num={} caps={} scroll={}",
                            state.num_lock,
                            state.caps_lock,
                            state.scroll_lock
                        );
                    }
                }
                log::debug!("unsolicited-frame reader finished");
            })
            .context("spawning the lock-state reader thread")?;
    }

    let (producer, writer) = input::spawn(link, input::Config::default());

    // ---- capture ---------------------------------------------------------------------------
    let source =
        V4l2Source::open(&args.video, args.width, args.height, args.fps).with_context(|| {
            format!(
                "opening {} for MJPEG {}x{}@{}. Check the node is the dongle's video interface \
                 and that nothing else is streaming from it.",
                args.video.display(),
                args.width,
                args.height,
                args.fps
            )
        })?;
    let pipeline = Pipeline::start(Box::new(source), PipelineConfig::default());

    // ---- the window ------------------------------------------------------------------------
    log::info!("press {RELEASE_KEY} (or Mod+Escape on niri) to release capture");
    let config = ViewerConfig {
        pointer: args.pointer.into(),
        stats_interval: match args.stats_interval {
            0 => None,
            s => Some(Duration::from_secs(s)),
        },
        exit_after: args.exit_after.map(Duration::from_secs),
    };
    let result = viewer::run(&pipeline, producer, config, &INTERRUPTED);

    // ---- shutdown, release first -------------------------------------------------------------
    // §2.6: clean shutdown is a release-all trigger, and §2.6.1: an `Unsent` outcome means the
    // target may still be holding keys, which is worth saying rather than hiding.
    match writer.shutdown() {
        ReleaseOutcome::Submitted => log::info!("release-all submitted on shutdown"),
        ReleaseOutcome::Unsent => log::warn!(
            "the shutdown release-all was UNSENT: the target may still be holding keys. \
             Nothing local can fix that — check the target."
        ),
    }
    pipeline.stop();
    result
}
