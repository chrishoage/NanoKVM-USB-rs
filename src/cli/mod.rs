//! The subcommands (plan §12, Stage 3).
//!
//! Stage 1 and 2 shipped one flag-based viewer. Stage 3 keeps that invocation exactly as it was —
//! `nanokvm` with no subcommand is still the viewer, with the same flags — and hangs the
//! conveniences off it as subcommands: [`devices`], [`shot`], and the keyboard senders in
//! [`keys`].
//!
//! # What a subcommand is allowed to open
//!
//! **Only the node it needs, and never the other one.** `devices` opens nothing at all unless
//! `--probe` is given, and then only the serial node discovery would *select* or `--serial` named;
//! `key`/`type`/`macro` open the serial node; `shot` opens the video node. None of them starts
//! the viewer's threads. That is not tidiness: the two nodes are separate USB devices (§8), the
//! user's own hardware sits on the same machine, and an open fd on a node keeps its index and
//! renumbers it on the next replug (C13). The cost that remains is discovery's own: `inventory`
//! opens a `VIDIOC_QUERYCAP` on the video nodes that take part in a candidate pair, whichever
//! node the command actually wants, because that is what decides the pairing (§8) and it is
//! read-only — and a `--dry-run` does not pay even that, because `main.rs` dispatches it before
//! devices are resolved at all.
//!
//! The argument structs live here rather than in `main.rs` so that each command's help text sits
//! next to the code that implements it. What is shared between the commands lives here too: the
//! signal handling every one of them installs, the sysfs-and-probe pair every one of them
//! discovers through ([`sources`]), and [`out`], through which every user-facing line is printed.

pub mod devices;
pub mod keys;
pub mod out;
pub mod shot;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::Subcommand;

use crate::discovery::{NoProbe, NodeProbe, RealProbe, RealSysfs};

/// The signals that mean "stop, but stop safely". SIGINT is the operator at the keyboard; SIGTERM
/// is what a harness sends when its timeout fires, and these commands exist to be driven by
/// exactly such a harness; SIGHUP is the terminal going away underneath them. All three can arrive
/// while a key is held down or a `.tmp` is half written, and taking the default disposition for
/// any of them kills the process between a press and its release, or between a `create` and its
/// `rename`. CLAUDE.md's rule is "always send a release-all before exiting anything that sent
/// input", with no exemption for which signal the caller chose, so all three take the
/// flag-and-unwind path (§2.6).
///
/// SIGKILL cannot be caught and is the documented exception: it leaves the key held, which is why
/// the operator's escape is SIGTERM.
const CAUGHT_SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

/// Set by the signal handler, polled by whichever loop is running.
///
/// The handler may only do async-signal-safe things and this store is one. Everything the
/// interrupt actually causes — the abort, the release-all, the unlinked temporary — happens on the
/// main thread, which is what makes the `Drop` guards the single place either is emitted (§2.6).
///
/// One flag for every subcommand: a process runs one of them.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_interrupt(_signal: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// Route every signal in [`CAUGHT_SIGNALS`] to the flag the command polls.
///
/// Called once, before anything is opened and so before anything can be held down or half
/// written. Shared by [`keys`] and [`shot`] because the argument for catching all three is the
/// same in both: `shot` that dies between `create` and `rename` leaves a `.tmp` behind, and `key`
/// that dies between a press and its release leaves a key down.
pub fn install_signal_handlers() {
    for signal in CAUGHT_SIGNALS {
        // SAFETY: `on_interrupt` is an `extern "C"` function whose whole body is one atomic store,
        // which is async-signal-safe. The previous disposition is deliberately discarded; these
        // commands install the handler once and never restore it.
        unsafe {
            libc::signal(signal, on_interrupt as libc::sighandler_t);
        }
    }
}

/// Whether one of [`CAUGHT_SIGNALS`] has been seen.
pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// What discovery reads and what it may open, for every discovery call a command makes.
///
/// `None` is this machine: `/sys` and one read-only `VIDIOC_QUERYCAP` per candidate video node,
/// which is §8's answer to "which nodes are the dongle's".
///
/// `Some(root)` is the hidden `--sysfs-root`, which points discovery at a recorded tree
/// (`fixtures/sysfs/`) so that a test of the *command line* does not have to enumerate the
/// developer's desk. It also replaces the probe, for the reason [`discovery::NoProbe`] gives: a
/// recording names `/dev` nodes that belong to whatever is plugged into this machine now, and
/// opening those to answer a question about a fixture is exactly what §8's "opens nothing it does
/// not need" exists to prevent.
pub fn sources(sysfs_root: Option<&Path>) -> (RealSysfs, Box<dyn NodeProbe>) {
    match sysfs_root {
        Some(root) => (RealSysfs::new(root), Box::new(NoProbe)),
        None => (RealSysfs::default(), Box::new(RealProbe)),
    }
}

/// The subcommands. Absent means the viewer, which is Stage 1's behaviour unchanged.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// List the video and serial nodes discovery can see, with the pairing evidence. Nodes named
    /// by --video/--serial are marked with `->`.
    Devices(devices::DevicesArgs),
    /// Capture one frame to a JPEG file and exit.
    Shot(shot::ShotArgs),
    /// Send one or more key chords to the target, then release everything.
    Key(keys::KeyArgs),
    /// Type text on the target, one key at a time, against a declared layout.
    Type(keys::TypeArgs),
    /// Run a macro file of key, type and wait steps.
    #[command(name = "macro")]
    Macro(keys::MacroArgs),
}
