//! `nanokvm` — the viewer (plan §12, Stages 1 and 2).
//!
//! Wiring only: parse arguments, choose the two devices, start the three subsystems and hand the
//! event loop over to [`nanokvm::viewer::run`].
//!
//! The order below is deliberate. **Serial comes up first and is never taken down by the capture
//! path** (§6.1 S1-1): sending a chord to a target with no video is a primary use case, so a
//! capture failure after startup must not tear down input. A capture failure *at* startup is
//! fatal only because there is nothing to look at yet.
//!
//! # Startup fails fast; everything after startup recovers
//!
//! Stage 2 gives both subsystems a *source* of devices rather than one device
//! ([`input::spawn_with_source`], [`Pipeline::start_with_opener`]), so a device that goes away is
//! reopened rather than lost (§2.7, §6.1 S2-4). That is deliberately **not** extended to startup:
//!
//! - the serial link is still waited for, once, with [`Producer::wait_for_link`], and a dongle
//!   that is not there is an error the user reads rather than a window that silently never types;
//! - the video node is still opened here, by this thread, before the pipeline starts — a wrong
//!   `--video` must fail loudly, and at startup there is nothing to look at anyway, which is the
//!   argument §6.1 S1-1 makes for the opposite policy *after* startup.
//!
//! The already-open source is then handed to the opener, which yields it first and reopens by
//! path afterwards. So the fail-fast startup and the recovering steady state share one code path
//! and one negotiated mode.
//!
//! # Rediscovery on reopen (§8)
//!
//! Both reopeners go through [`discovery::NodeResolver`]: a node that was *discovered* is
//! rediscovered on every attempt, because the kernel gives a replugged device the lowest free
//! name and not necessarily the old one; a node that was given explicitly is reopened by that
//! name and discovery is never consulted for it. See [`nanokvm::discovery::reopen`].

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;

use nanokvm::capture::{Pipeline, PipelineConfig, V4l2Source};
use nanokvm::discovery::{
    self, Constraints, DiscoveringLinkSource, DiscoveringOpener, NodeKind, NodeResolver, RealProbe,
    RealSysfs,
};
use nanokvm::input::{self, ReleaseOutcome};
use nanokvm::serial::{self, OpenOptions};
use nanokvm::viewer::{self, PointerMode, ViewerConfig, RELEASE_KEY};

/// How long startup waits for the writer to commission a link before giving up (§2.7).
///
/// Long enough to cover one open plus the §5.1 resync preamble and a `GET_INFO` — measured at
/// well under 10 ms on this desk — with three orders of magnitude of slack, and short enough
/// that a missing dongle is reported while the user is still watching the terminal.
const STARTUP_LINK_WAIT: Duration = Duration::from_secs(2);

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
    /// Serial node of the CH9329 bridge, e.g. /dev/ttyACM1. Discovered from USB topology when
    /// omitted (§8). Given together with --video it is used exactly as typed and discovery is
    /// not consulted; given alone it picks among the pairs discovery found.
    #[arg(long, value_name = "PATH")]
    serial: Option<PathBuf>,

    /// Video node of the capture device, e.g. /dev/video4. Discovered from USB topology when
    /// omitted (§8). Given together with --serial it is used exactly as typed and discovery is
    /// not consulted; given alone it picks among the pairs discovery found.
    #[arg(long, value_name = "PATH")]
    video: Option<PathBuf>,

    /// List the video and serial nodes discovery can see, with the pairing evidence, and exit.
    /// Nodes named by --video/--serial are marked with `->`.
    #[arg(long)]
    list_devices: bool,

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

/// The two device nodes to open, from `--video`/`--serial`, discovery, or both (§8).
///
/// §8's policy in one function: an explicit path always wins and is used even when no rule
/// connects it to the other node — that is what makes the tool usable when discovery is wrong —
/// while anything discovery cannot narrow to exactly one pair is a printed listing and a
/// non-zero exit, never a guess.
///
/// The error path deliberately does not go through `anyhow`, and so this returns no `Result`:
/// [`discovery::DiscoveryError`]'s `Display` is a multi-line table of every node found, and
/// wrapping it in "Error: " with a context chain would bury the part the user has to read. It is
/// printed as-is and the process exits 1.
fn select_devices(args: &Args) -> (PathBuf, PathBuf) {
    let sysfs = RealSysfs::default();
    let probe = RealProbe;

    let pair = match discovery::discover(&sysfs, &probe, &constraints(args)) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    let (video, serial) = (pair.video.dev.display(), pair.serial.dev.display());
    match &pair.evidence {
        // §8's items 1 and 2 are the kernel's own assertions; nothing to add.
        Some(evidence) if evidence.is_proof() => {
            log::info!("using {video} + {serial} — {evidence}")
        }
        // Item 3 is a containment test over three commodity vendor ids. It is the only rule this
        // desk has on a USB 2.0 port, so it is used — but a user who is about to have CH9329
        // frames written into that port should be told the pairing is inferred, and how to
        // overrule it.
        Some(evidence) => log::warn!(
            "using {video} + {serial} — {evidence}. This is §8's degraded rule: the hub, the \
             capture chip and the serial bridge are all commodity parts, so an unrelated capture \
             stick and serial adapter sharing a cheap hub would look the same. Pass --video and \
             --serial to overrule it; --list-devices shows what discovery can see."
        ),
        None => log::warn!(
            "using {video} + {serial} — no USB topology evidence connects these two nodes. Both \
             were given explicitly, so they are used as asked; if that is a mistake, \
             --list-devices shows what discovery can see."
        ),
    }
    (pair.video.dev, pair.serial.dev)
}

fn constraints(args: &Args) -> Constraints {
    Constraints {
        video: args.video.clone(),
        serial: args.serial.clone(),
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    if args.list_devices {
        // Stage 3's `devices` listing, brought forward: §8's error path needs the same table, so
        // it may as well be reachable on purpose (§12 Stage 3).
        //
        // `--video`/`--serial` are honoured here rather than ignored: the nodes they name are
        // marked in the table, and a path discovery did not enumerate is called out as one that
        // would still be used. A listing that dropped the flags on its own command line would be
        // answering a different question than the one asked.
        //
        // TODO(stage-3): the link-level Sipeed serial (`GET_USB_STRING`, §8's last paragraph) is
        // the only unit-unique identifier the dongle has, and it belongs in this table. Reading
        // it means opening the CH9329 and talking to it, which a listing command must not do —
        // the same reason this listing does not open a video node it would never select.
        let inventory = discovery::inventory(&RealSysfs::default(), &RealProbe);
        print!("{}", inventory.listing(&constraints(&args)));
        return Ok(());
    }

    // SAFETY: `on_interrupt` is an `extern "C"` function whose entire body is one atomic store,
    // which is async-signal-safe. `libc::signal` is being handed a valid function pointer, and
    // the previous disposition is deliberately discarded — there is none worth restoring in a
    // program that installs this once at startup.
    unsafe {
        libc::signal(libc::SIGINT, on_interrupt as libc::sighandler_t);
    }

    // ---- which two nodes (§8) --------------------------------------------------------------
    // Reads /sys only, and opens a video node for one QUERYCAP just where that can change the
    // answer. Nothing here touches the serial node.
    let (video_path, serial_path) = select_devices(&args);

    // ---- serial, first and independent of everything else (§6.1 S1-1) --------------------
    // The writer owns opening now (§2.7): it opens, resynchronises the chip's parser (§5.1),
    // re-queries device info and retries on a backoff, for the initial link and every
    // replacement alike. `wait_for_link` is what keeps the Stage 1 fail-fast startup on top of
    // that. The receiver is the *source's*, so it stays valid across every reopen and every
    // rename (see `discovery::reopen`).
    let (source, unsolicited) = DiscoveringLinkSource::new(
        NodeResolver::real(NodeKind::Serial, constraints(&args)),
        OpenOptions::default(),
    );
    let (producer, writer) = input::spawn_with_source(Box::new(source), input::Config::default());

    // The one handshake this device needs, and the one thing that proves the link answers
    // (Appendix). A failure here is fatal: without it we do not know there is a CH9329 there.
    // The writer has already logged what each attempt reported — a permissions error on the node
    // reads differently from a device that is simply absent — so this says what was waited for
    // and points at those lines rather than guessing which of them was the cause.
    let info = producer.wait_for_link(STARTUP_LINK_WAIT).with_context(|| {
        format!(
            "no CH9329 answered on {} within {STARTUP_LINK_WAIT:?}. Check the path and that you \
             can write to it (it is usually owned by the 'uucp' or 'dialout' group); the log \
             lines above say what each open attempt reported.",
            serial_path.display()
        )
    })?;
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

    // ---- capture ---------------------------------------------------------------------------
    // Opened here, on this thread, so that a wrong `--video` or an absent dongle is a message
    // rather than an empty window (see the module docs). From the reopen on, the opener owns it.
    let source =
        V4l2Source::open(&video_path, args.width, args.height, args.fps).with_context(|| {
            format!(
                "opening {} for MJPEG {}x{}@{}. Check the node is the dongle's video interface \
                 and that nothing else is streaming from it.",
                video_path.display(),
                args.width,
                args.height,
                args.fps
            )
        })?;
    let opener = DiscoveringOpener::with_open_source(
        NodeResolver::real(NodeKind::Video, constraints(&args)),
        Box::new(source),
        &video_path,
        args.width,
        args.height,
        args.fps,
    );
    let pipeline = Pipeline::start_with_opener(Box::new(opener), PipelineConfig::default());

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
