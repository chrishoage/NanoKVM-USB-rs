//! Command arguments, signal handling, and discovery adapters.
//!
//! Keyboard commands use serial; screenshots use video. Discovery may issue a read-only
//! `VIDIOC_QUERYCAP` on candidate video nodes to distinguish capture from metadata.
//! `devices` opens no serial node unless probing is requested. Keyboard dry runs perform
//! no discovery or device access.
//!
//! SIGINT, SIGTERM, and SIGHUP request orderly cleanup. Signal handlers only set a flag;
//! release reports and temporary-file cleanup run on the command's main thread.

pub mod devices;
pub mod keys;
pub mod out;
pub mod shot;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::Subcommand;

use crate::discovery::{NoProbe, NodeProbe, RealProbe, RealSysfs};

/// Signals that request cleanup instead of immediate termination.
const CAUGHT_SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

/// Signal-safe stop flag. Cleanup runs on the command thread.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_interrupt(_signal: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// Install cleanup handlers before opening devices or temporary files.
pub fn install_signal_handlers() {
    for signal in CAUGHT_SIGNALS {
        // SAFETY: `on_interrupt` is an `extern "C"` function whose whole body is one atomic store,
        // which is async-signal-safe. The previous disposition is discarded; these
        // commands install the handler once and never restore it.
        unsafe {
            libc::signal(signal, on_interrupt as *const () as libc::sighandler_t);
        }
    }
}

/// Whether one of `CAUGHT_SIGNALS` has been seen.
pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Choose live sysfs with a read-only probe, or recorded sysfs with a no-op probe.
/// Recorded node paths may belong to unrelated hardware on the test host.
pub fn sources(sysfs_root: Option<&Path>) -> (RealSysfs, Box<dyn NodeProbe>) {
    match sysfs_root {
        Some(root) => (RealSysfs::new(root), Box::new(NoProbe)),
        None => (RealSysfs::default(), Box::new(RealProbe)),
    }
}

/// Available commands. Omitting a subcommand starts the viewer.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// List video, serial, and audio devices with pairing information.
    Devices(devices::DevicesArgs),
    /// Save one frame as JPEG or PNG and exit.
    Shot(shot::ShotArgs),
    /// Send one or more key chords to the target, then release everything.
    Key(keys::KeyArgs),
    /// Type text using the declared target keyboard layout.
    Type(keys::TypeArgs),
    /// Run a macro file of key, type and wait steps.
    #[command(name = "macro")]
    Macro(keys::MacroArgs),
}
