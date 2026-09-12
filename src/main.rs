//! `nanokvm` — the viewer (plan §12, Stages 1 and 2).
//!
//! Wiring only: parse arguments, choose the devices, start the subsystems and hand the
//! event loop over to [`nanokvm::viewer::run`].
//!
//! Stage 4a adds a fourth subsystem and a third device: the dongle's own sound card (§12 Stage
//! 4a). It is started last, nothing waits for it, and `--no-audio` starts none of it — §4.1 rev 5
//! makes audio a side channel, and the ordering here is what makes that true rather than
//! intended.
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
//!
//! # Subcommands open one node, and never the viewer's (§12 Stage 3)
//!
//! Everything above is the *viewer*, which is what `nanokvm` with no subcommand still runs, flag
//! for flag. A subcommand starts none of those threads and opens **only the node it needs**:
//! `devices` opens nothing unless `--probe` is given, `key`/`type`/`macro` open the serial node,
//! `shot` opens the video node. So [`select_devices`] is asked for one [`NodeKind`] rather than a
//! pair — see its doc for what that changes and what it deliberately does not.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser};

use nanokvm::audio::alsa::DefaultSinkOpener;
use nanokvm::audio::{AudioConfig, AudioHandle};
use nanokvm::capture::{Pipeline, PipelineConfig, V4l2Source};
use nanokvm::cli::{devices, keys, shot, Command};
use nanokvm::discovery::{
    self, Constraints, DiscoveringCardOpener, DiscoveringLinkSource, DiscoveringOpener, Evidence,
    NodeKind, NodeResolver,
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

/// Seconds between the viewer's statistics lines when `--stats-interval` is not given. Stated in
/// that flag's help text, which is where the user reads it.
const DEFAULT_STATS_INTERVAL_SECS: u64 = 5;

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
    ),
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Serial node of the CH9329 bridge, e.g. /dev/ttyACM1. Discovered from USB topology when
    /// omitted (§8). Given together with --video it is used exactly as typed and discovery is
    /// not consulted; given alone it picks among the pairs discovery found.
    #[arg(long, value_name = "PATH", global = true)]
    serial: Option<PathBuf>,

    /// Video node of the capture device, e.g. /dev/video4. Discovered from USB topology when
    /// omitted (§8). Given together with --serial it is used exactly as typed and discovery is
    /// not consulted; given alone it picks among the pairs discovery found.
    #[arg(long, value_name = "PATH", global = true)]
    video: Option<PathBuf>,

    /// Capture width in pixels.
    #[arg(long, default_value_t = 1920, global = true)]
    width: u32,

    /// Capture height in pixels.
    #[arg(long, default_value_t = 1080, global = true)]
    height: u32,

    /// Capture frame rate. Passed to S_PARM explicitly; without it the driver keeps its own
    /// default, which is 240 fps at 1080p (§6, A7).
    #[arg(long, default_value_t = 60, global = true)]
    fps: u32,

    /// Pointer mode. Absolute can reach a specific pixel; relative cannot, because the target
    /// applies its own pointer acceleration (§3.4). [default: abs]
    #[arg(long, value_enum)]
    pointer: Option<PointerArg>,

    /// Do not open the dongle's sound card. The viewer is then exactly what it was in Stage 3:
    /// no card is opened, no audio thread runs, and the title says only "audio off".
    #[arg(long)]
    no_audio: bool,

    /// Seconds between statistics lines; 0 disables them. [default: 5]
    #[arg(long, value_name = "SECS")]
    stats_interval: Option<u64>,

    /// Quit cleanly after this many seconds. Development flag for bounded hardware runs.
    #[arg(long, value_name = "SECS", hide = true)]
    exit_after: Option<u64>,

    /// Open this chrome popover at startup: Video, Keyboard, Mouse or Audio. Development flag —
    /// it exists so §12 Stage 4b's "event-loop handling latency with a popover open" can be
    /// measured on a desk where nothing may drive the pointer.
    #[arg(long, value_name = "NAME", hide = true)]
    chrome_popover: Option<String>,

    /// Engage capture as soon as the window is up. Development flag: §12 Stage 4c's exit criterion
    /// types onto the target, and nothing on this desk may drive the pointer or the keyboard for a
    /// hardware run. It feeds the reducer the same Enter edge a keypress would, which is consumed
    /// and never forwarded.
    #[arg(long, hide = true)]
    capture_on_start: bool,

    /// Paste the clipboard as soon as capture engages. Development flag, and it implies
    /// --capture-on-start: a paste needs a captured session, and nothing can click for it.
    #[arg(long, hide = true)]
    paste_on_capture: bool,

    /// Feed the release key this many milliseconds after a paste starts. Development flag: §12
    /// Stage 4c's exit criterion includes cancelling a running paste, and nobody can press the
    /// release key for an unattended run. It takes the key's own path, not a private cancel.
    #[arg(long, value_name = "MS", hide = true)]
    paste_cancel_after_ms: Option<u64>,

    /// Read USB topology from this directory instead of /sys. Development flag: it points
    /// discovery at a recorded tree (`fixtures/sysfs/`) so that a test of the command line does
    /// not enumerate the machine it runs on. It also stops discovery opening any video node —
    /// see `cli::sources`.
    #[arg(long, value_name = "PATH", global = true, hide = true)]
    sysfs_root: Option<PathBuf>,
}

/// The viewer-only flags, by the name the user typed, for the usage error a subcommand gets.
///
/// They are `Option` for exactly this: a clap default would make "the user asked for relative
/// pointer mode" indistinguishable from "the user said nothing", and `nanokvm --pointer relative
/// devices` would then be silently ignored rather than refused.
impl Args {
    fn viewer_only_flag(&self) -> Option<&'static str> {
        if self.pointer.is_some() {
            return Some("--pointer");
        }
        if self.no_audio {
            return Some("--no-audio");
        }
        if self.stats_interval.is_some() {
            return Some("--stats-interval");
        }
        if self.exit_after.is_some() {
            return Some("--exit-after");
        }
        if self.chrome_popover.is_some() {
            return Some("--chrome-popover");
        }
        if self.capture_on_start {
            return Some("--capture-on-start");
        }
        if self.paste_on_capture {
            return Some("--paste-on-capture");
        }
        if self.paste_cancel_after_ms.is_some() {
            return Some("--paste-cancel-after-ms");
        }
        None
    }
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

/// Which nodes a command is going to open.
///
/// A subcommand opens one node and must not require the other to exist (§12 Stage 3): `key
/// --serial /dev/ttyACM1` has to work on a desk whose video node is unplugged, and asking for a
/// pair would make discovery's verdict about the video node fatal to a command that never
/// touches it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Needed {
    Both,
    One(NodeKind),
}

/// What [`select_devices`] resolved. The node that was asked for is always present.
#[derive(Debug, Default)]
struct Selection {
    video: Option<PathBuf>,
    serial: Option<PathBuf>,
    /// The sysfs directory of the USB device the video node belongs to, when discovery
    /// enumerated one. It is what the audio card is paired against, and the only thing the card
    /// opener needs — see [`discovery::reopen::CardResolver`], which is why it is carried here
    /// rather than re-derived by a second run of discovery on every audio retry.
    video_usb: Option<PathBuf>,
}

impl Selection {
    /// One node and nothing else: the §8 override arm, where the node that was asked for was
    /// named on the command line and there is no pair to report.
    fn just(kind: NodeKind, path: PathBuf) -> Selection {
        match kind {
            NodeKind::Video => Selection {
                video: Some(path),
                serial: None,
                video_usb: None,
            },
            NodeKind::Serial => Selection {
                video: None,
                serial: Some(path),
                video_usb: None,
            },
        }
    }

    /// The node of `kind`. Present by construction: [`select_devices`] prints discovery's listing
    /// and exits rather than return without the node it was asked for.
    fn node(&self, kind: NodeKind) -> PathBuf {
        let path = match kind {
            NodeKind::Video => &self.video,
            NodeKind::Serial => &self.serial,
        };
        path.clone().unwrap_or_else(|| {
            unreachable!("select_devices was asked for the {kind} node and returned without one")
        })
    }
}

/// The device nodes to open, from `--video`/`--serial`, discovery, or both (§8).
///
/// §8's policy in one function: an explicit path always wins and is used even when no rule
/// connects it to the other node — that is what makes the tool usable when discovery is wrong —
/// while anything discovery cannot narrow to exactly one pair is a printed listing and a
/// non-zero exit, never a guess.
///
/// # Asking for one node (§12 Stage 3)
///
/// [`Needed::One`] changes exactly one thing: **a discovery failure is survivable when the needed
/// node was given explicitly.** Discovery still runs, and a pair it does find is still used and
/// still logged with its evidence, because the pairing is what says the node belongs to this
/// dongle and a command that skipped it would be quieter about a real ambiguity than the viewer
/// is. But `key --serial /dev/ttyACM1` on a desk with no video node must send the chord: §8 makes
/// the explicit path the thing that rescues a wrong discovery, and there is nothing left to
/// infer once the one node needed has been named. The warning it logs is the same "given
/// explicitly, so used as asked" the both-given arm has always logged.
///
/// It does **not** make discovery cheaper or narrower. `inventory` opens a `VIDIOC_QUERYCAP` on
/// the video nodes that could take part in a pair whatever node is wanted, because that is what
/// decides the pairing; the probe is read-only and the alternative is a special case in the one
/// module whose job is to be the single answer to "which nodes are the dongle's".
///
/// The error path deliberately does not go through `anyhow`, and so this returns no `Result`:
/// [`discovery::DiscoveryError`]'s `Display` is a multi-line table of every node found, and
/// wrapping it in "Error: " with a context chain would bury the part the user has to read. It is
/// printed as-is and the process exits 1.
fn select_devices(
    constraints: &Constraints,
    need: Needed,
    sysfs_root: Option<&PathBuf>,
) -> Selection {
    let (sysfs, probe) = nanokvm::cli::sources(sysfs_root.map(PathBuf::as_path));

    let pair = match discovery::discover(&sysfs, probe.as_ref(), constraints) {
        Ok(pair) => pair,
        Err(e) => {
            // §8's override, for a command that needs one node: the pairing could not be made,
            // but the node this command opens was named, so there is nothing left to infer.
            if let Needed::One(kind) = need {
                if let Some(path) = explicit(constraints, kind) {
                    log::warn!(
                        "using {} as the {kind} node — no USB topology evidence pairs it with \
                         anything ({e}). It was given explicitly, so it is used as asked; if that \
                         is a mistake, `nanokvm devices` shows what discovery can see.",
                        path.display()
                    );
                    return Selection::just(kind, path);
                }
            }
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    match need {
        Needed::Both => log_pairing_paragraph(&pair),
        Needed::One(kind) => {
            let (used, other) = match kind {
                NodeKind::Video => (&pair.video.dev, &pair.serial.dev),
                NodeKind::Serial => (&pair.serial.dev, &pair.video.dev),
            };
            let line = short_pairing_line(
                &used.display().to_string(),
                &other.display().to_string(),
                pair.evidence.as_ref(),
            );
            match &pair.evidence {
                Some(e) if e.is_proof() => log::info!("{line}"),
                _ => log::warn!("{line}"),
            }
        }
    }
    Selection {
        video_usb: pair.video.usb.as_ref().map(|u| u.sysfs.clone()),
        video: Some(pair.video.dev),
        serial: Some(pair.serial.dev),
    }
}

/// The viewer's account of the pairing: the whole of §8's reasoning, once, at startup.
///
/// It is long because of what it precedes — a window that is about to capture the user's keyboard
/// and write CH9329 frames into whichever port this chose. A subcommand gets
/// [`short_pairing_line`] instead.
fn log_pairing_paragraph(pair: &discovery::Pair) {
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
             --serial to overrule it; `nanokvm devices` shows what discovery can see."
        ),
        None => log::warn!(
            "using {video} + {serial} — no USB topology evidence connects these two nodes. Both \
             were given explicitly, so they are used as asked; if that is a mistake, \
             `nanokvm devices` shows what discovery can see."
        ),
    }
}

/// A subcommand's account of the same pairing: one line, naming the node it opens, the node that
/// vouches for it, and where the evidence can be read in full.
///
/// The paragraph above is right for the viewer and wrong here. `nanokvm key a` is a one-shot
/// command whose output is two lines, and six lines of §8 reasoning in front of them — on every
/// run, dry runs included — is noise that trains the user to stop reading warnings. The degraded
/// rule still warns, because it is still an inference; a proof logs at `info` and says which of
/// §8's two proofs it was. `nanokvm devices` prints the whole evidence on demand.
fn short_pairing_line(used: &str, other: &str, evidence: Option<&Evidence>) -> String {
    match evidence {
        Some(Evidence::InternalHub { .. }) => format!(
            "using {used}, paired with {other} by the degraded internal-hub rule \
             (nanokvm devices shows the evidence)"
        ),
        Some(e) => format!(
            "using {used}, paired with {other} by the {} rule (nanokvm devices shows the evidence)",
            e.kind()
        ),
        None => format!(
            "using {used}: no USB topology evidence connects it to {other}. Both were given \
             explicitly, so they are used as asked (nanokvm devices shows what discovery can see)"
        ),
    }
}

/// The single node a subcommand opens (§12 Stage 3). See [`select_devices`] for what asking for
/// one node changes.
fn one_node(args: &Args, constraints: &Constraints, kind: NodeKind) -> PathBuf {
    select_devices(constraints, Needed::One(kind), args.sysfs_root.as_ref()).node(kind)
}

/// The path `--video`/`--serial` gave for one node kind, if any.
fn explicit(constraints: &Constraints, kind: NodeKind) -> Option<PathBuf> {
    match kind {
        NodeKind::Video => constraints.video.clone(),
        NodeKind::Serial => constraints.serial.clone(),
    }
}

fn constraints(args: &Args) -> Constraints {
    Constraints {
        video: args.video.clone(),
        serial: args.serial.clone(),
    }
}

/// The path handed to a `--dry-run`, which resolves no node at all (§12 Stage 3).
///
/// A dry run opens nothing, so there is nothing for discovery to answer — and running it anyway
/// would make "opens no device" untrue: `inventory` opens a `VIDIOC_QUERYCAP` on every candidate
/// video node, which on this desk is the dongle's. So the dispatch below compiles first and this
/// stands in for the node, where it is never read. It is not a path: if one ever reached an
/// `open()`, the message would say where it came from.
const NO_NODE: &str = "<no device: --dry-run resolves none>";

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    let constraints = constraints(&args);

    // A viewer flag with a subcommand is a mistake worth naming rather than ignoring: `--pointer`
    // means nothing to `devices`. Checked here, rather than by clap's
    // `args_conflicts_with_subcommands`, because that setting also rejects `nanokvm --serial X key
    // a` — a global written before the subcommand instead of after it, which is how most people
    // type it and is not a mistake at all (H4).
    if let (Some(command), Some(flag)) = (&args.command, args.viewer_only_flag()) {
        Args::command()
            .error(
                clap::error::ErrorKind::ArgumentConflict,
                format!("{flag} has no meaning for '{}'", command_name(command)),
            )
            .exit();
    }

    // Each subcommand resolves the one node it opens and then does its own work; none of them
    // reaches `run_viewer`, and none of them opens the node it does not need (see the module
    // docs and `cli`).
    match &args.command {
        // Opens nothing at all unless `--probe`, and then only the serial node discovery would
        // select or `--serial` named (`cli::devices::probe_targets`), so discovery's own listing
        // is the whole answer and `select_devices` is not involved.
        Some(Command::Devices(a)) => devices::run(a, &constraints, args.sysfs_root.as_deref()),
        Some(Command::Shot(a)) => {
            let video = one_node(&args, &constraints, NodeKind::Video);
            shot::run(a, &video, args.width, args.height, args.fps)
        }
        // §2.8 item 3 in the dispatch: a dry run is dispatched *before* `select_devices`, so it
        // reads no `/sys` and opens no node — not even the read-only `QUERYCAP` discovery uses to
        // tell a capture node from its metadata sibling (H3).
        Some(Command::Key(a)) => keys::run_key(a, &serial_node(&args, &constraints, a.dry_run)),
        Some(Command::Type(a)) => keys::run_type(a, &serial_node(&args, &constraints, a.dry_run)),
        Some(Command::Macro(a)) => keys::run_macro(a, &serial_node(&args, &constraints, a.dry_run)),
        None => run_viewer(&args, &constraints),
    }
}

/// Which chrome popover `--chrome-popover` named, or `None` when the flag was not given.
///
/// A development flag (§12 Stage 4b): it exists so the measurement with a popover open can be
/// taken on a desk where nothing may drive the pointer. The names come from the popovers
/// themselves, so the flag cannot name one that does not exist.
fn chrome_popover(name: Option<&str>) -> Result<Option<nanokvm::viewer::chrome::Popover>> {
    let Some(name) = name else {
        return Ok(None);
    };
    viewer::chrome::ChromeUi::popover_by_name(name)
        .map(Some)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "--chrome-popover {name}: expected one of Video, Keyboard, Mouse or Audio"
            )
        })
}

/// The serial node a keyboard command will open, or [`NO_NODE`] for a dry run that will not.
fn serial_node(args: &Args, constraints: &Constraints, dry_run: bool) -> PathBuf {
    if dry_run {
        return PathBuf::from(NO_NODE);
    }
    one_node(args, constraints, NodeKind::Serial)
}

/// The subcommand's name as the user typed it, for the usage error above.
fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Devices(_) => "devices",
        Command::Shot(_) => "shot",
        Command::Key(_) => "key",
        Command::Type(_) => "type",
        Command::Macro(_) => "macro",
    }
}

/// Stage 1 and 2's viewer, unchanged: the three subsystems, the window, and the release-all on
/// the way out. Reached only when no subcommand was given.
fn run_viewer(args: &Args, constraints: &Constraints) -> Result<()> {
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
    let selection = select_devices(constraints, Needed::Both, args.sysfs_root.as_ref());
    let (video_path, serial_path) = (
        selection.node(NodeKind::Video),
        selection.node(NodeKind::Serial),
    );

    // ---- serial, first and independent of everything else (§6.1 S1-1) --------------------
    // The writer owns opening now (§2.7): it opens, resynchronises the chip's parser (§5.1),
    // re-queries device info and retries on a backoff, for the initial link and every
    // replacement alike. `wait_for_link` is what keeps the Stage 1 fail-fast startup on top of
    // that. The receiver is the *source's*, so it stays valid across every reopen and every
    // rename (see `discovery::reopen`).
    let (source, unsolicited) = DiscoveringLinkSource::new(
        NodeResolver::real(NodeKind::Serial, constraints.clone()),
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
    // One wording for the lock bits, shared with `key`/`type` and `devices --probe`: `caps=on`,
    // never `caps=true` (`DeviceInfo`'s `Display`).
    log::info!("{info}");
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
                    // The bits only: an unsolicited 0x81 push is not an answer to `GET_INFO`, so
                    // printing its version and target flag would claim more than was asked (§3.4).
                    log::info!("target lock state: {}", state.locks());
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
        NodeResolver::real(NodeKind::Video, constraints.clone()),
        Box::new(source),
        &video_path,
        args.width,
        args.height,
        args.fps,
    );
    let pipeline = Pipeline::start_with_opener(Box::new(opener), PipelineConfig::default());

    // ---- the chrome's settings and the mode list (§12 Stage 4b) -----------------------------
    // Loaded **before** the window, and a parse failure is returned rather than defaulted: §12
    // Stage 4b says a malformed file is an error naming the key, not a silent reset, and an error
    // raised after the window exists is one nobody reads. `None` — no `HOME`, no
    // `XDG_CONFIG_HOME` — means the defaults and nothing remembered, which is honest degradation
    // rather than a dotfile scattered in the working directory.
    let chrome_store = viewer::chrome::Store::discover();
    let chrome_config = match chrome_store.as_ref() {
        Some(store) => store.load().with_context(|| {
            format!(
                "reading the chrome settings from {}. Fix the key it names, or delete the file to \
                 start from the defaults.",
                store.path().display()
            )
        })?,
        None => {
            log::warn!(
                "neither XDG_CONFIG_HOME nor HOME is set, so the chrome's settings cannot be \
                 remembered between runs; using the defaults"
            );
            viewer::chrome::Config::default()
        }
    };

    // The Video popover's list, read from the device rather than written down (§12 Stage 4b). A
    // read-only `VIDIOC_ENUM_FRAMESIZES` on a second handle — the pipeline owns the streaming one
    // — and a failure is a warning with an empty list, because a menu that cannot be built is not
    // a reason to refuse to show a picture.
    let video_modes = match nanokvm::capture::v4l2::enumerate_modes(&video_path) {
        Ok(modes) => {
            log::info!(
                "the device enumerates {} MJPEG mode(s): {}",
                modes.len(),
                modes
                    .iter()
                    .map(|(w, h)| format!("{w}x{h}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            modes
        }
        Err(e) => {
            log::warn!(
                "could not enumerate the device's capture modes ({e}); the chrome's Video \
                 popover will say so rather than offer a guess"
            );
            Vec::new()
        }
    };

    // ---- audio, last and least (§4.1 rev 5, §12 Stage 4a) ----------------------------------
    // Started after video and input are already running, and nothing below waits for it: audio
    // is a side channel, so a card that is absent, busy or unbound must leave the client exactly
    // as usable as it was in Stage 3. `--no-audio` spawns nothing at all — not a thread, not an
    // open — which is what makes "exactly Stage 3" checkable rather than merely intended.
    //
    // The card is resolved the same way the other two nodes are (§8, C13): rediscovered on every
    // open, because card numbers renumber on replug exactly as /dev names do.
    let audio = match (args.no_audio, &selection.video_usb) {
        (true, _) => {
            log::info!("audio: disabled by --no-audio; no sound card will be opened");
            None
        }
        // A `--video` discovery never enumerated: there is no USB device for a card to belong to,
        // so there is nothing to pair and nothing a retry could change. Saying so once beats two
        // threads reporting the same thing for the rest of the session.
        (false, None) => {
            log::warn!(
                "audio: the video node was given explicitly and discovery never enumerated it, \
                 so there is no USB device to match a sound card against; running without audio. \
                 `nanokvm devices` shows what discovery can see"
            );
            None
        }
        (false, Some(usb)) => {
            let config = AudioConfig::default();
            log::info!(
                "audio: buffer {}x{} frames configured ({:?}), playing to ALSA {}",
                config.ring.periods,
                config.ring.period_frames,
                config.ring.depth(),
                nanokvm::audio::alsa::PLAYBACK_DEVICE,
            );
            Some(AudioHandle::spawn(
                Box::new(DiscoveringCardOpener::real(usb.clone(), config)),
                Box::new(DefaultSinkOpener::new(config)),
                config,
            ))
        }
    };

    // ---- the window ------------------------------------------------------------------------
    log::info!("press {RELEASE_KEY} (or Mod+Escape on niri) to release capture");
    // The defaults live here rather than in clap, because the flags have to stay `Option` to tell
    // "asked for" from "not mentioned" when a subcommand is present (see `viewer_only_flag`).
    let config = ViewerConfig {
        // The command line still wins at startup, and the chrome's remembered choice is the
        // default when the flag was not given — which is why `--pointer` is an `Option`.
        pointer: match args.pointer {
            Some(p) => p.into(),
            None => chrome_config.mouse_mode.into(),
        },
        stats_interval: match args.stats_interval.unwrap_or(DEFAULT_STATS_INTERVAL_SECS) {
            0 => None,
            s => Some(Duration::from_secs(s)),
        },
        exit_after: args.exit_after.map(Duration::from_secs),
        chrome_popover: chrome_popover(args.chrome_popover.as_deref())?,
        // `--paste-on-capture` implies `--capture-on-start`: a paste needs a captured session, and
        // the flag exists precisely for runs where nothing can capture one by hand.
        capture_on_start: args.capture_on_start || args.paste_on_capture,
        paste_on_capture: args.paste_on_capture,
        paste_cancel_after: args.paste_cancel_after_ms.map(Duration::from_millis),
        chrome: chrome_config,
        chrome_store,
        video_modes,
        fps: args.fps,
    };
    let result = viewer::run(&pipeline, producer, audio.as_ref(), config, &INTERRUPTED);

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
    // Explicit rather than left to the drop order, so the audio threads' last log lines land
    // before the process exits and a stuck one is reported rather than silently detached.
    drop(audio);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanokvm::discovery::UsbDevice;

    /// H5: a subcommand gets one line, not the viewer's paragraph — but it still says the pairing
    /// was inferred, and still says where the evidence is. A six-line warning on every `key a`
    /// trains the reader to skip warnings, which costs more than it buys.
    #[test]
    fn a_subcommand_gets_one_line_that_still_names_the_degraded_rule() {
        let hub = UsbDevice {
            sysfs: PathBuf::from("/sys/devices/usb3/3-2/3-2.2"),
            name: "3-2.2".to_string(),
            vid: 0x1a40,
            pid: 0x0101,
            product: Some("USB 2.0 Hub".to_string()),
            serial: None,
            speed: Some("480".to_string()),
            busnum: 3,
            devnum: 9,
        };
        let line = short_pairing_line(
            "/dev/ttyACM1",
            "/dev/video4",
            Some(&Evidence::InternalHub { hub }),
        );
        assert_eq!(
            line,
            "using /dev/ttyACM1, paired with /dev/video4 by the degraded internal-hub rule \
             (nanokvm devices shows the evidence)"
        );
        assert_eq!(line.lines().count(), 1, "one line: {line}");
    }

    /// A proof-grade pairing says which proof it was, in one line, and the caller logs it at
    /// `info` rather than `warn` (§8 items 1 and 2 are the kernel's own assertions).
    #[test]
    fn a_proof_grade_pairing_names_the_rule_in_one_line() {
        let line = short_pairing_line("/dev/video4", "/dev/ttyACM1", Some(&Evidence::SameDevice));
        assert_eq!(
            line,
            "using /dev/video4, paired with /dev/ttyACM1 by the same USB device rule \
             (nanokvm devices shows the evidence)"
        );
        assert!(Evidence::SameDevice.is_proof());
        let peer = Evidence::PortPeer {
            video_port: "4-2-port2".to_string(),
            peer_port: "3-2-port2".to_string(),
        };
        assert!(short_pairing_line("a", "b", Some(&peer)).lines().count() == 1);
    }

    /// Both nodes given explicitly: there is no rule to name, and §8 says they are used as asked.
    #[test]
    fn an_unevidenced_pairing_says_it_was_the_users_choice() {
        let line = short_pairing_line("/dev/ttyACM9", "/dev/video9", None);
        assert!(line.contains("no USB topology evidence"), "{line}");
        assert!(line.contains("given"), "{line}");
        assert_eq!(line.lines().count(), 1, "{line}");
    }

    /// H4: the viewer-only flags are `Option` so that "not mentioned" is distinguishable, and the
    /// error names the flag the user typed rather than the first one in the struct.
    #[test]
    fn each_viewer_only_flag_is_named_when_it_is_the_one_given() {
        let args = Args::parse_from(["nanokvm", "--pointer", "relative", "devices"]);
        assert_eq!(args.viewer_only_flag(), Some("--pointer"));
        // Not `global`, so after the subcommand clap refuses them itself, with exit 2 and the
        // flag's own name — which is the same answer by a shorter route.
        let args = Args::parse_from(["nanokvm", "--stats-interval", "1", "devices"]);
        assert_eq!(args.viewer_only_flag(), Some("--stats-interval"));
        let args = Args::parse_from(["nanokvm", "--exit-after", "1", "devices"]);
        assert_eq!(args.viewer_only_flag(), Some("--exit-after"));
    }

    /// The globals a subcommand *does* take are accepted on either side of it, and none of them
    /// is a viewer-only flag. `nanokvm --serial X key a` was a usage error before H4.
    #[test]
    fn the_shared_globals_are_accepted_before_or_after_the_subcommand() {
        for argv in [
            ["nanokvm", "--serial", "/dev/x", "devices"],
            ["nanokvm", "devices", "--serial", "/dev/x"],
        ] {
            let args = Args::parse_from(argv);
            assert_eq!(args.serial, Some(PathBuf::from("/dev/x")), "{argv:?}");
            assert_eq!(args.viewer_only_flag(), None, "{argv:?}");
        }
    }

    /// The viewer's own defaults are unchanged by the flags becoming `Option`.
    #[test]
    fn the_viewer_defaults_are_the_ones_stated_in_help() {
        let args = Args::parse_from(["nanokvm"]);
        assert_eq!(args.pointer.unwrap_or(PointerArg::Abs), PointerArg::Abs);
        assert_eq!(
            args.stats_interval.unwrap_or(DEFAULT_STATS_INTERVAL_SECS),
            5
        );
        assert_eq!(args.exit_after, None);
        assert_eq!(args.width, 1920);
        assert_eq!(args.height, 1080);
        assert_eq!(args.fps, 60);
    }
}
