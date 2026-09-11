//! `nanokvm devices` — what discovery can see, and optionally what the link says (§8, §12 Stage 3).
//!
//! The listing itself is Stage 1's `--list-devices` output unchanged: [`Inventory::listing`]
//! against the constraints on this command line, so the nodes `--video`/`--serial` name are
//! marked and a path discovery never enumerated is called out rather than silently dropped (§8's
//! override).
//!
//! `--probe` adds the one identifier the dongle has that sysfs cannot see: `GET_USB_STRING`'s
//! unit-unique serial (A16, §8's last paragraph). That means **opening a serial node and writing
//! CH9329 frames into it**, which is why the listing does not do it by default and why
//! [`probe_targets`] exists as a pure function with its own test: on this desk `/dev/ttyACM0` is
//! an unrelated device on someone else's hub, and a probe that walked every `ttyACM*` would be
//! writing frames into it.
//!
//! **Only the node discovery would actually select is opened**, never every node it happened to
//! pair. `discover` refuses to choose between two candidate pairs (§8: never a guess), so a probe
//! that walked `inventory.pairs` would open serial nodes the client itself would refuse to use —
//! on a desk with two dongles, the other unit's. So `--probe` asks [`discovery::discover`] and
//! probes its answer: exactly one node, or none and the reason why, or the one `--serial` named.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Result};

use crate::cli::out;
use crate::discovery::{self, Constraints, DiscoveryError, Pair};
use crate::link::Link;
use crate::proto::{DeviceInfo, UsbStrings};
use crate::serial::SerialLink;

/// Per-transaction bound for the probe.
///
/// `GET_INFO` answers in 3.98 ms on this desk (A11), so half a second is two orders of magnitude
/// of slack — enough that a slow reply is reported as the string it carries rather than as a
/// timeout, and short enough that a node that is not a CH9329 costs two seconds in total and not
/// a hang.
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(clap::Args, Debug)]
pub struct DevicesArgs {
    /// Open the serial node discovery would select (or the one --serial names) and ask it what it
    /// is: firmware, target-connected flag, lock bits and USB strings. Writes CH9329 frames, so it
    /// is never done to a node the client itself would refuse to use: where discovery cannot name
    /// one pair, nothing is probed and the exit status says so.
    #[arg(long)]
    pub probe: bool,
}

/// Print the listing, then one block per probed node. Exits non-zero only if a probe failed, or
/// if `--probe` was asked for a selection discovery would not make.
///
/// `sysfs_root` is the hidden `--sysfs-root`: `None` is this machine, `Some` a recorded tree (see
/// [`crate::cli::sources`]).
pub fn run(args: &DevicesArgs, constraints: &Constraints, sysfs_root: Option<&Path>) -> Result<()> {
    let (sysfs, probe_nodes) = crate::cli::sources(sysfs_root);
    let inventory = discovery::inventory(&sysfs, probe_nodes.as_ref());
    out::block(&inventory.listing(constraints).to_string());
    if !args.probe {
        return Ok(());
    }

    // The second walk is deliberate and is only paid for by `--probe`: the listing is every node
    // found, the selection is the one pair §8 would use, and they are different questions. It
    // re-reads `/sys` and re-runs the read-only `QUERYCAP` the listing already did — cheap beside
    // opening a serial node and writing CH9329 frames into it, which is what this is deciding.
    let selected = discovery::discover(&sysfs, probe_nodes.as_ref(), constraints);
    let targets = probe_targets(&selected, constraints);
    if targets.is_empty() {
        // Only an unresolved discovery can leave nothing to probe, and its own `Display` is the
        // table of everything found plus what to pass — which is the whole of what the user has
        // to act on (§8).
        let why = selected
            .as_ref()
            .expect_err("a selected pair always names a serial node to probe");
        bail!(
            "refusing to probe: {why}\nThe listing above is what discovery can see; --serial \
             names the node to ask."
        );
    }

    let mut failed = 0usize;
    for path in &targets {
        out::line(&format!("probe {}:", path.display()));
        match probe(path) {
            Ok(report) => {
                out::line(&format!("  {}", report.info));
                out::line(&format!("  usb strings: {}", report.strings));
            }
            Err(e) => {
                failed += 1;
                out::line(&format!("  probe failed: {e}"));
            }
        }
    }
    if failed > 0 {
        bail!(
            "{failed} of {} probe(s) failed; see the probe blocks above",
            targets.len()
        );
    }
    Ok(())
}

/// The serial nodes `--probe` may open, given what discovery selected.
///
/// **`--serial` wins outright, and nothing else is opened**: §8 makes an explicit path the thing
/// that rescues a wrong discovery, so a probe must be able to reach a node the pairing rules
/// never saw — including on a desk where discovery cannot choose at all.
///
/// Otherwise it is the serial half of the pair discovery *selected*, and nothing else. Not every
/// pair it found: two candidate pairs are [`DiscoveryError::Ambiguous`], which the client refuses
/// to guess between, and probing both would write CH9329 frames into a second dongle — or, with
/// a looser rule, into the unrelated CDC-ACM device this desk carries on the user's hub
/// (CLAUDE.md). An error therefore yields nothing to probe, and the caller prints its text.
pub fn probe_targets(
    selected: &Result<Pair, DiscoveryError>,
    constraints: &Constraints,
) -> Vec<PathBuf> {
    if let Some(path) = &constraints.serial {
        return vec![path.clone()];
    }
    match selected {
        Ok(pair) => vec![pair.serial.dev.clone()],
        Err(_) => Vec::new(),
    }
}

/// What one node answered.
struct ProbeReport {
    info: DeviceInfo,
    strings: UsbStrings,
}

/// Open one serial node and ask it the two questions that identify it.
///
/// The §5.1 preamble goes first, as it does on every link this client opens: the chip's receive
/// parser has no inter-byte timeout, so whatever torn frame a previous process left outstanding
/// would otherwise eat this command (A15). It is safe here because it forms no command of its own
/// — see [`SerialLink::resync`].
fn probe(path: &Path) -> Result<ProbeReport, anyhow::Error> {
    let mut link = SerialLink::open(path)?;
    link.resync()?;
    let info = link
        .get_info(PROBE_TIMEOUT)
        .map_err(|e| anyhow::anyhow!("GET_INFO: {e}"))?;
    let strings = link
        .get_usb_strings(PROBE_TIMEOUT)
        .map_err(|e| anyhow::anyhow!("GET_USB_STRING: {e}"))?;
    Ok(ProbeReport { info, strings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::testing::{fixture, MapProbe};

    /// The desk as recorded: `/dev/video4` is the capture node, `/dev/video5` its metadata sibling.
    fn desk_probe() -> MapProbe {
        MapProbe::new()
            .capture("/dev/video4", true)
            .capture("/dev/video5", false)
            .default_capture(true)
    }

    /// `two-dongles` adds a second identical unit, as in `tests/discovery.rs`.
    fn two_dongle_probe() -> MapProbe {
        desk_probe()
            .capture("/dev/video6", true)
            .capture("/dev/video7", false)
    }

    fn selection(tree: &str, probe: &MapProbe, constraints: &Constraints) -> Vec<PathBuf> {
        let sysfs = fixture(tree);
        let selected = discovery::discover(&sysfs, probe, constraints);
        probe_targets(&selected, constraints)
    }

    /// The rule this command exists to keep: `/dev/ttyACM0` on this desk is an unrelated CDC-ACM
    /// device on the user's hub (CLAUDE.md). It is in the listing, it is not what discovery
    /// selected, and a probe that opened it would write CH9329 frames into someone else's
    /// hardware.
    #[test]
    fn only_the_serial_node_discovery_selected_is_ever_probed() {
        let sysfs = fixture("usb2-desk");
        let inventory = discovery::inventory(&sysfs, &desk_probe());
        assert!(
            inventory
                .serials
                .iter()
                .any(|s| s.dev == Path::new("/dev/ttyACM0")),
            "the fixture must still carry the unrelated node this test is about"
        );
        assert_eq!(
            selection("usb2-desk", &desk_probe(), &Constraints::default()),
            vec![PathBuf::from("/dev/ttyACM1")]
        );
    }

    /// §8: a desk with two candidate pairs is one discovery refuses to choose between, so there is
    /// nothing it *would* select — and probing both would write CH9329 frames into the other
    /// unit. Nothing is opened and the caller prints discovery's own error.
    #[test]
    fn an_ambiguous_desk_yields_nothing_to_probe() {
        assert!(selection("two-dongles", &two_dongle_probe(), &Constraints::default()).is_empty());
    }

    /// §8: an explicit path is honoured permanently, including on a desk discovery cannot resolve
    /// and for a node it never enumerated — and it is then the *only* node opened.
    #[test]
    fn an_explicit_serial_is_the_only_node_probed_even_where_discovery_refuses_to_choose() {
        let constraints = Constraints {
            serial: Some(PathBuf::from("/dev/ttyACM2")),
            ..Constraints::default()
        };
        assert_eq!(
            selection("two-dongles", &two_dongle_probe(), &constraints),
            vec![PathBuf::from("/dev/ttyACM2")]
        );

        let unknown = Constraints {
            serial: Some(PathBuf::from("/dev/ttyUSB7")),
            ..Constraints::default()
        };
        assert_eq!(
            selection("usb2-desk", &desk_probe(), &unknown),
            vec![PathBuf::from("/dev/ttyUSB7")]
        );
    }
}
