//! Pairing the dongle's two device nodes by USB topology (plan §8).
//!
//! The dongle presents itself as **two unrelated-looking Linux devices**: a UVC video node and a
//! CDC-ACM serial node. Nothing in either node says which dongle it belongs to — §8 measured
//! that even the `iSerial` strings are unrelated (`20210621`, a firmware date, against
//! `5C37176280`, a bridge serial). So the pairing has to come from where the two devices *sit*,
//! and §8's policy is blunt about the consequence: **exactly one candidate pair is used, anything
//! else fails with the candidates listed. Never silently pick one.**
//!
//! ## The two physical shapes, and why both are supported
//!
//! ```text
//! SuperSpeed link (Stage 0, docs/stage0/topology.md)
//!   usb4 ── 4-2 (USB3 hub) ── 4-2.2   345f:2133  video     <- /dev/video4
//!                              └ port 4-2-port2  --peer--> 3-2-port2
//!   usb3 ── 3-2 (USB2 hub) ── 3-2.2   1a40:0101  internal hub
//!                              └────── 3-2.2.4  1a86:55d3  serial  <- /dev/ttyACM1
//!
//! USB 2.0-only link (Stage 1, this desk)
//!   usb3 ── 3-2 (USB2 hub) ── 3-2.2   1a40:0101  internal hub
//!                              ├────── 3-2.2.2  345f:2133  video   <- /dev/video4, /dev/video5
//!                              └────── 3-2.2.4  1a86:55d3  serial  <- /dev/ttyACM1
//! ```
//!
//! At SuperSpeed the video device enumerates on a bus the kernel presents as entirely separate
//! from the serial device's, and the only thing joining them is the kernel's port `peer` symlink
//! — which asserts *these two ports are one physical connector*. On a USB 2.0-only port the video
//! device cannot reach SuperSpeed, falls back behind the dongle's own internal hub, and there is
//! no `peer` link at all. §8 listed the second shape as **untested**; STAGE1_FINDINGS recorded it
//! happening for real. Both are implemented here and both are pinned by a fixture (§9.1: the
//! recording is the authority, not a retyped description of it).
//!
//! ## Evidence, strongest first (§8, as revised by `topology.md`)
//!
//! 1. [`Evidence::SameDevice`] — one USB device, two interfaces. Proof. Not available on this
//!    unit, and quite possibly on no unit; kept because §8 keeps it and it costs two comparisons.
//! 2. [`Evidence::PortPeer`] — the kernel's `peer` assertion. Proof, and no vendor id is
//!    required: the kernel is stating a fact about the connector, not a guess about the device.
//! 3. [`Evidence::InternalHub`] — containment under the dongle's own hub. **Not proof.** §8 is
//!    explicit that on a USB 2.0-only port "the check degrades to a common-ancestor test, which
//!    is sound *because the shared ancestor is inside the dongle*" — so its soundness rests
//!    entirely on the claim that the shared hub *is* the dongle's, and nothing asserts that. Its
//!    messages say "degraded" for that reason: a user about to let this program write CH9329
//!    frames into a serial port should be told which of the two kinds of answer they got.
//!
//! **Item 3 here is deliberately narrower than §8's wording, and that is a correction, not an
//! omission.** The id §8 would have to check, `1a40:0101`, is an unbranded generic hub chip with
//! no manufacturer string and no serial. Checking only the hub id makes the rule say "any two USB
//! devices behind any cheap hub are a NanoKVM". This desk is the counter-example that forces the
//! point: the user's webcam (`5-1.4.4.4.2`, `046d:086b`) and an unrelated CDC-ACM device
//! (`5-1.4.4.4.3`, `043e:9a8a`) are **direct children of one generic hub** (`5-1.4.4.4`,
//! `0bda:5411`) and would pair under the loose rule. So item 3 additionally requires the video
//! device to be [`VIDEO_ID`] and the serial device to be [`SERIAL_ID`]. Item 2 needs no such
//! guard because the kernel supplies the proof; item 3 has no proof to lean on, so it leans on
//! identity instead. The cost is that a hypothetical future dongle revision with different ids
//! stops auto-pairing on a USB 2.0 port — it still lists, and `--video`/`--serial` still work,
//! which §8 guarantees permanently.
//!
//! All three of those ids are commodity part numbers (Terminus hub, MacroSilicon capture, QinHeng
//! bridge), so the containment rule can in principle be satisfied by a capture stick and a serial
//! adapter sharing a cheap hub. It still pairs — this desk depends on it, and refusing would
//! leave the USB 2.0 shape with no automatic answer at all — but the caller logs it at `warn` and
//! names `--video`/`--serial`, because that is the one case where the automatic answer can be
//! wrong with nothing looking wrong.
//!
//! ## Explicit overrides are not discovery's to veto
//!
//! §8: "Always honour explicit `--serial` and `--video` overrides, which also make the tool
//! usable when discovery is wrong." [`discover`] therefore never consults the inventory to
//! *select* when both are given: the two paths are used as handed over, whether or not the class
//! walk enumerated them (a `ttyUSB` node bound by `ch343` rather than `cdc_acm`, a
//! `/dev/serial/by-id/…` path, a `/sys` that could not be read at all). Discovery still runs
//! there, but only to attach the evidence — or the absence of it — for the caller to log, and it
//! cannot fail that selection.
//!
//! ## What opens what
//!
//! Nothing in this module opens the serial node, and it opens a video node only when the answer
//! could change the outcome (see [`inventory`]). Reading `/sys` is free of side effects; opening
//! `/dev` is not, and a device-listing command that disturbs the user's hardware to produce a
//! nicer table is a bad trade.

pub mod probe;
pub mod reopen;
pub mod sysfs;
pub mod testing;

pub use probe::{NoProbe, NodeProbe, RealProbe};
pub use reopen::{DiscoveringLinkSource, DiscoveringOpener, Mode, NodeResolver};
pub use sysfs::{RealSysfs, Sysfs};

use std::fmt;
use std::path::{Path, PathBuf};

/// The class directory every UVC node registers under.
const VIDEO_CLASS: &str = "/class/video4linux";
/// The class directory CDC-ACM ports register under.
const TTY_CLASS: &str = "/class/tty";
/// Only `ttyACM*` is considered: the CH9329 bridge is a CDC-ACM device, and scanning every
/// `/sys/class/tty` entry would walk 60-odd virtual consoles for nothing.
const TTY_PREFIX: &str = "ttyACM";
/// Video nodes are `video*`; `v4l-subdev*` and `media*` live in other classes.
const VIDEO_PREFIX: &str = "video";

/// MACROSILICON "USB2 Video" / "USB3 Video" — the dongle's capture function (§8, `topology.md`).
pub const VIDEO_ID: (u16, u16) = (0x345f, 0x2133);
/// QinHeng CH9329 "USB Single Serial" — the dongle's HID bridge (§8).
pub const SERIAL_ID: (u16, u16) = (0x1a86, 0x55d3);
/// The unbranded 4-port hub inside the dongle. Generic, hence the extra id checks on evidence 3.
pub const INTERNAL_HUB_ID: (u16, u16) = (0x1a40, 0x0101);

/// One USB device, resolved up from a class node.
///
/// §8: "resolve up to the USB device, not to an arbitrary shared ancestor". `sysfs` is the
/// device directory itself (`/devices/.../3-2.2.2`), never one of its interface children
/// (`3-2.2.2:1.0`) and never the hub above it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsbDevice {
    /// sysfs-absolute path of the device directory.
    pub sysfs: PathBuf,
    /// The kernel's name for it, e.g. `3-2.2.2`.
    pub name: String,
    pub vid: u16,
    pub pid: u16,
    pub product: Option<String>,
    pub serial: Option<String>,
    /// Raw `speed` attribute in Mbit/s, e.g. `480`, `5000`.
    pub speed: Option<String>,
    pub busnum: u32,
    pub devnum: u32,
}

impl UsbDevice {
    /// `vid:pid` as the pair the evidence rules compare against.
    pub fn id(&self) -> (u16, u16) {
        (self.vid, self.pid)
    }

    /// The device directory this one is plugged into, or `None` for a root hub.
    fn parent(&self) -> Option<&Path> {
        self.sysfs.parent()
    }
}

impl fmt::Display for UsbDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {:04x}:{:04x}", self.name, self.vid, self.pid)?;
        if let Some(p) = &self.product {
            write!(f, " \"{p}\"")?;
        }
        if let Some(s) = &self.speed {
            write!(f, " {s}M")?;
        }
        Ok(())
    }
}

/// Why a probe could not answer for a node.
///
/// The errno is kept alongside the message because one of them changes what the *program* should
/// say: if every probe was refused with `EACCES`, "more than one candidate pair" is a lie about a
/// permissions problem (see [`DiscoveryError::ProbesDenied`]).
#[derive(Clone, Debug)]
pub struct ProbeFailure {
    pub message: String,
    /// `Some(libc::EACCES)`, `Some(libc::EBUSY)`, … `None` for an error with no errno.
    pub errno: Option<i32>,
}

/// A `/dev/video*` node and the USB device it belongs to.
#[derive(Clone, Debug)]
pub struct VideoNode {
    /// e.g. `/dev/video4`.
    pub dev: PathBuf,
    /// The USB device the class walk resolved this node to.
    ///
    /// `None` only for a node named by `--video` that discovery never enumerated — an override
    /// is obeyed without discovery's agreement (§8), and "I do not know what this is" is the
    /// honest thing to record rather than a fabricated device.
    pub usb: Option<UsbDevice>,
    /// `Some(true)`/`Some(false)` from [`NodeProbe`]; `None` when the probe was not run or
    /// failed. See [`inventory`] for when it is run.
    pub is_capture: Option<bool>,
    /// Set when the probe was run and returned an error. `EACCES` and `EBUSY` both land here,
    /// and both are things the user can act on, so §8's listing shows them.
    pub probe_error: Option<ProbeFailure>,
    /// Whether the probe was consulted at all. Distinguishes "metadata node" from "not asked".
    pub probed: bool,
}

impl VideoNode {
    /// Whether this node may be selected as the capture node.
    ///
    /// A node the probe called a capture node always may. A node with no probe answer may only
    /// if no sibling on the same USB device gave a definite `true` — that is what keeps
    /// `/dev/video5` out of the running while still surviving an `EACCES` on `/dev/video4`
    /// rather than declaring the dongle absent.
    fn eligible(&self, siblings: &[VideoNode]) -> bool {
        match self.is_capture {
            Some(v) => v,
            None => !siblings.iter().any(|s| {
                s.is_capture == Some(true)
                    && s.usb
                        .as_ref()
                        .zip(self.usb.as_ref())
                        .is_some_and(|(a, b)| a.sysfs == b.sysfs)
            }),
        }
    }

    fn capture_note(&self) -> &'static str {
        match (self.is_capture, self.probe_error.is_some(), self.probed) {
            (Some(true), _, _) => "capture",
            (Some(false), _, _) => "metadata (not a capture node)",
            (None, true, _) => "capture status unknown (probe failed)",
            (None, false, true) => "capture status unknown",
            (None, false, false) => "not probed (no topological pairing candidate)",
        }
    }
}

/// A `/dev/ttyACM*` node and the USB device it belongs to.
#[derive(Clone, Debug)]
pub struct SerialNode {
    /// e.g. `/dev/ttyACM1`.
    pub dev: PathBuf,
    /// `None` for a node named by `--serial` that discovery never enumerated; see
    /// [`VideoNode::usb`].
    pub usb: Option<UsbDevice>,
}

/// Why two nodes are believed to be one dongle (§8, strongest first).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Evidence {
    /// Identical `busnum:devnum`: two interfaces of one USB device.
    SameDevice,
    /// The kernel's `peer` symlink joins the two hub ports, so they are one physical connector.
    PortPeer {
        /// The hub-port directory the video device occupies, e.g. `4-2-port2`.
        video_port: String,
        /// Its `peer`, e.g. `3-2-port2`.
        peer_port: String,
    },
    /// Both are direct children of the dongle's own internal hub, and all three vendor ids match.
    ///
    /// §8's *degraded* case, not proof: see the module docs.
    InternalHub {
        /// The hub itself, for the listing.
        hub: UsbDevice,
    },
}

impl Evidence {
    /// The short name used in logs, in [`Inventory`] listings and by
    /// `scripts/device-health.py`, which prints the same three strings.
    pub fn kind(&self) -> &'static str {
        match self {
            Evidence::SameDevice => "same USB device",
            Evidence::PortPeer { .. } => "port peer",
            Evidence::InternalHub { .. } => "internal hub (degraded)",
        }
    }

    /// Whether this is one of §8's two *proofs* (items 1 and 2) rather than its degraded
    /// common-ancestor test (item 3). The caller logs the degraded case at `warn`.
    pub fn is_proof(&self) -> bool {
        !matches!(self, Evidence::InternalHub { .. })
    }
}

impl fmt::Display for Evidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Evidence::SameDevice => write!(f, "same USB device (busnum:devnum) — proof"),
            Evidence::PortPeer {
                video_port,
                peer_port,
            } => write!(
                f,
                "port peer: the serial device sits under {peer_port}, which the kernel declares \
                 the peer of the video device's port {video_port} — proof"
            ),
            // Deliberately never says "proof", and never the word on its own either: this is
            // §8's degraded test, contained by a hub whose id — like the other two — is a
            // commodity part number. `scripts/device-health.py` prints this string verbatim.
            Evidence::InternalHub { hub } => write!(
                f,
                "internal hub (degraded): both are direct children of the dongle's own hub {hub} \
                 — §8's common-ancestor test, contained by that hub and by all three vendor ids, \
                 which are commodity part numbers rather than a kernel assertion"
            ),
        }
    }
}

/// One video node and one serial node believed to be the same dongle.
///
/// `evidence` is `None` only when the user supplied **both** `--video` and `--serial`: §8 says
/// explicit selection is honoured permanently and makes the tool "usable when discovery is
/// wrong", so an override is obeyed even when no rule connects the two. The caller logs a warning
/// in that case; it is not an error.
#[derive(Clone, Debug)]
pub struct Pair {
    pub video: VideoNode,
    pub serial: SerialNode,
    pub evidence: Option<Evidence>,
}

impl fmt::Display for Pair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} + {} — {}",
            self.video.dev.display(),
            self.serial.dev.display(),
            match &self.evidence {
                Some(e) => e.to_string(),
                None => "no evidence (both nodes given explicitly)".to_string(),
            }
        )
    }
}

/// Everything discovery found, whether or not it paired.
///
/// This is the message a user reads when told to pass `--video`/`--serial` (§8), and it is also
/// what `nanokvm devices` prints (§12 Stage 3), because §8's error path needs it anyway.
#[derive(Clone, Debug, Default)]
pub struct Inventory {
    pub videos: Vec<VideoNode>,
    pub serials: Vec<SerialNode>,
    pub pairs: Vec<Pair>,
}

impl Inventory {
    fn video(&self, dev: &Path) -> Option<&VideoNode> {
        self.videos.iter().find(|v| v.dev == dev)
    }

    fn serial(&self, dev: &Path) -> Option<&SerialNode> {
        self.serials.iter().find(|s| s.dev == dev)
    }

    /// A copy carrying only the pairs `keep` accepts. Used to narrow an `Ambiguous` report to
    /// the pairs the user's one constraint left standing, so the message names the real choice.
    fn retaining_pairs(&self, keep: impl Fn(&Pair) -> bool) -> Inventory {
        Inventory {
            videos: self.videos.clone(),
            serials: self.serials.clone(),
            pairs: self.pairs.iter().filter(|p| keep(p)).cloned().collect(),
        }
    }

    /// The same listing, with the nodes `--video`/`--serial` name marked and the selection those
    /// flags produce spelled out.
    ///
    /// `nanokvm devices` uses this rather than plain [`Display`](fmt::Display): a listing that
    /// silently ignored the flags on its own command line would be answering a question the user
    /// did not ask (finding 7 of the Stage 2 review).
    pub fn listing<'a>(&'a self, constraints: &'a Constraints) -> Listing<'a> {
        Listing {
            inventory: self,
            constraints,
        }
    }

    /// Whether every video node that was probed was refused with `EACCES`, and at least one was.
    ///
    /// That is a permissions problem wearing the costume of an ambiguity: with no probe answers
    /// nothing can rule the metadata node out, so the sibling nodes all stay eligible and the
    /// pair count goes up. Saying "more than one candidate pair" there sends the user hunting
    /// for a second dongle.
    fn every_probe_denied(&self) -> bool {
        let probed: Vec<&VideoNode> = self.videos.iter().filter(|v| v.probed).collect();
        !probed.is_empty()
            && probed
                .iter()
                .all(|v| v.probe_error.as_ref().and_then(|e| e.errno) == Some(libc::EACCES))
    }
}

impl fmt::Display for Inventory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.listing(&NO_CONSTRAINTS).fmt(f)
    }
}

/// No `--video`/`--serial`, so nothing in a listing is marked. A constant rather than a
/// temporary because [`Listing`] borrows what it is given.
static NO_CONSTRAINTS: Constraints = Constraints {
    video: None,
    serial: None,
};

/// An [`Inventory`] rendered against the constraints in force. See [`Inventory::listing`].
pub struct Listing<'a> {
    inventory: &'a Inventory,
    constraints: &'a Constraints,
}

impl Listing<'_> {
    /// `->` marks a node the user named on the command line; two spaces keeps the columns lined
    /// up for the rest.
    fn mark(chosen: Option<&PathBuf>, dev: &Path) -> &'static str {
        match chosen {
            Some(p) if p == dev => "->",
            _ => "  ",
        }
    }
}

impl fmt::Display for Listing<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inv = self.inventory;
        let (cv, cs) = (
            self.constraints.video.as_ref(),
            self.constraints.serial.as_ref(),
        );

        writeln!(f, "video nodes:")?;
        if inv.videos.is_empty() {
            writeln!(f, "  (none)")?;
        }
        for v in &inv.videos {
            writeln!(
                f,
                "{} {} {} — {}",
                Self::mark(cv, &v.dev),
                v.dev.display(),
                describe(v.usb.as_ref()),
                v.capture_note()
            )?;
            if let Some(e) = &v.probe_error {
                writeln!(f, "      probe failed: {}", e.message)?;
            }
        }
        writeln!(f, "serial nodes:")?;
        if inv.serials.is_empty() {
            writeln!(f, "  (none)")?;
        }
        for s in &inv.serials {
            writeln!(
                f,
                "{} {} {}",
                Self::mark(cs, &s.dev),
                s.dev.display(),
                describe(s.usb.as_ref())
            )?;
        }
        writeln!(f, "pairs:")?;
        if inv.pairs.is_empty() {
            writeln!(f, "  (none)")?;
        }
        for p in &inv.pairs {
            let mark = match (cv, cs) {
                (None, None) => "  ",
                (v, s) => {
                    let hit = v.is_none_or(|p2| *p2 == p.video.dev)
                        && s.is_none_or(|p2| *p2 == p.serial.dev);
                    if hit {
                        "->"
                    } else {
                        "  "
                    }
                }
            };
            writeln!(f, "{mark} {p}")?;
        }

        // A path the flags name that the class walk never enumerated is not an error — §8 obeys
        // it anyway — but a listing that did not mention it would look like it had been ignored.
        for (kind, chosen) in [(NodeKind::Video, cv), (NodeKind::Serial, cs)] {
            let Some(path) = chosen else { continue };
            let known = match kind {
                NodeKind::Video => inv.video(path).is_some(),
                NodeKind::Serial => inv.serial(path).is_some(),
            };
            if !known {
                writeln!(
                    f,
                    "note: {} {} is not a node discovery enumerated; it will still be used as \
                     given (§8).",
                    kind.flag(),
                    path.display()
                )?;
            }
        }
        if let (Some(v), Some(s)) = (cv, cs) {
            writeln!(
                f,
                "selection: {} + {} — both given explicitly, so discovery is not consulted.",
                v.display(),
                s.display()
            )?;
        }
        Ok(())
    }
}

/// A node's USB device for the listing, or a stand-in for one discovery could not resolve.
fn describe(usb: Option<&UsbDevice>) -> String {
    match usb {
        Some(u) => u.to_string(),
        None => "(not enumerated by discovery)".to_string(),
    }
}

/// Explicit `--video`/`--serial` selection (§8: honoured permanently).
#[derive(Clone, Debug, Default)]
pub struct Constraints {
    pub video: Option<PathBuf>,
    pub serial: Option<PathBuf>,
}

/// Which half of a pair a message is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Video,
    Serial,
}

impl NodeKind {
    /// The command-line flag that supplies this node, for error messages that tell the user
    /// exactly what to type.
    pub fn flag(self) -> &'static str {
        match self {
            NodeKind::Video => "--video",
            NodeKind::Serial => "--serial",
        }
    }

    fn other(self) -> NodeKind {
        match self {
            NodeKind::Video => NodeKind::Serial,
            NodeKind::Serial => NodeKind::Video,
        }
    }
}

impl fmt::Display for NodeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            NodeKind::Video => "video",
            NodeKind::Serial => "serial",
        })
    }
}

/// Why discovery could not name exactly one pair.
///
/// Every variant's `Display` names every node and every `vid:pid` found, because the action the
/// user has to take is to pick two of them and pass them on the command line (§8).
#[derive(Clone, Debug)]
pub enum DiscoveryError {
    NoVideoNodes,
    NoSerialNodes,
    NoPair(Inventory),
    Ambiguous(Inventory),
    /// Every video node discovery probed refused to open with `EACCES`, and without a single
    /// probe answer the tie between a capture node and its metadata sibling cannot be broken.
    /// Reported instead of [`DiscoveryError::Ambiguous`], which would blame the hardware for a
    /// permissions problem.
    ProbesDenied(Inventory),
    /// A `--video`/`--serial` path that is not a USB node on this system at all.
    ConstraintNotFound {
        kind: NodeKind,
        path: PathBuf,
        available: Vec<PathBuf>,
    },
    /// A single `--video`/`--serial` that pairs with nothing, so the other half cannot be
    /// inferred. Supplying both is always allowed and never lands here.
    ConstraintUnpaired {
        kind: NodeKind,
        path: PathBuf,
        inventory: Inventory,
    },
}

impl std::error::Error for DiscoveryError {}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiscoveryError::NoVideoNodes => write!(
                f,
                "no USB video node found under /sys/class/video4linux. Is the dongle plugged in?"
            ),
            DiscoveryError::NoSerialNodes => write!(
                f,
                "no USB ttyACM node found under /sys/class/tty. Is the dongle plugged in, and is \
                 the cdc_acm module loaded?"
            ),
            DiscoveryError::NoPair(inv) => write!(
                f,
                "no video node could be paired with a serial node by USB topology.\n{inv}\
                 Pass both --video and --serial to select the nodes yourself."
            ),
            DiscoveryError::Ambiguous(inv) => write!(
                f,
                "more than one candidate pair; refusing to guess.\n{inv}\
                 Pass --video and/or --serial to say which one you mean."
            ),
            DiscoveryError::ProbesDenied(inv) => write!(
                f,
                "permission denied on every video node discovery tried to identify (EACCES), so \
                 the capture node cannot be told from its metadata sibling.\n{inv}\
                 Add yourself to the 'video' group and log in again, or pass --video and \
                 --serial to select the nodes yourself."
            ),
            DiscoveryError::ConstraintNotFound {
                kind,
                path,
                available,
            } => {
                write!(
                    f,
                    "{} {} is not a USB {kind} node on this system. Available: ",
                    kind.flag(),
                    path.display()
                )?;
                if available.is_empty() {
                    write!(f, "(none)")
                } else {
                    let list: Vec<String> =
                        available.iter().map(|p| p.display().to_string()).collect();
                    write!(f, "{}", list.join(", "))
                }
            }
            DiscoveryError::ConstraintUnpaired {
                kind,
                path,
                inventory,
            } => write!(
                f,
                "{} {} does not pair with any {} node by USB topology, so the {} node cannot be \
                 inferred.\n{inventory}Pass {} as well.",
                kind.flag(),
                path.display(),
                kind.other(),
                kind.other(),
                kind.other().flag()
            ),
        }
    }
}

/// Everything discovery can see, with the pairing rules already applied.
///
/// ## Probe policy
///
/// [`NodeProbe::is_capture_node`] opens a `/dev/video*` node, so it is called only for nodes
/// whose USB device takes part in at least one *topological* candidate pair. Topology is decided
/// first, entirely from `/sys`; the probe then only ever breaks a tie between sibling nodes on a
/// device that could genuinely be selected — which is exactly the `/dev/video4` versus
/// `/dev/video5` question it exists for. Nodes that no rule could ever choose (the user's webcam
/// on an unrelated hub) are listed as `not probed` and never opened. A node with no probe answer
/// stays eligible unless a sibling answered `true`, so an `EACCES` degrades to "try it and see"
/// rather than to "the dongle is not here".
pub fn inventory(sysfs: &dyn Sysfs, probe: &dyn NodeProbe) -> Inventory {
    let mut videos = collect_video_nodes(sysfs);
    let serials = collect_serial_nodes(sysfs);

    // Topology first, with no probe answers at all: a pair that no rule accepts can never be
    // selected, so its nodes never need opening.
    let mut candidates: Vec<(usize, usize, Evidence)> = Vec::new();
    for (vi, v) in videos.iter().enumerate() {
        for (si, s) in serials.iter().enumerate() {
            let (Some(vu), Some(su)) = (v.usb.as_ref(), s.usb.as_ref()) else {
                continue;
            };
            if let Some(e) = evidence_for(sysfs, vu, su) {
                candidates.push((vi, si, e));
            }
        }
    }

    let interesting: Vec<PathBuf> = candidates
        .iter()
        .filter_map(|(vi, _, _)| videos[*vi].usb.as_ref().map(|u| u.sysfs.clone()))
        .collect();
    for v in &mut videos {
        if !v
            .usb
            .as_ref()
            .is_some_and(|u| interesting.contains(&u.sysfs))
        {
            continue;
        }
        v.probed = true;
        match probe.is_capture_node(&v.dev) {
            Ok(is_capture) => v.is_capture = Some(is_capture),
            Err(e) => {
                v.probe_error = Some(ProbeFailure {
                    message: e.to_string(),
                    errno: e.raw_os_error(),
                })
            }
        }
    }

    let snapshot = videos.clone();
    let pairs = candidates
        .into_iter()
        .filter(|(vi, _, _)| videos[*vi].eligible(&snapshot))
        .map(|(vi, si, evidence)| Pair {
            video: videos[vi].clone(),
            serial: serials[si].clone(),
            evidence: Some(evidence),
        })
        .collect();

    Inventory {
        videos,
        serials,
        pairs,
    }
}

/// The one pair to use, or why there is not exactly one (§8).
///
/// **Both constraints given: this cannot fail.** The two paths are used exactly as handed over
/// and discovery is not consulted for the selection at all — not for whether the nodes exist, not
/// for whether the class walk saw them, not for whether anything joins them. §8 makes the
/// override the thing that rescues a wrong discovery, and an override a wrong discovery can veto
/// rescues nothing. The inventory is still built, but only to attach the evidence (or record its
/// absence) for the caller to log.
///
/// One given: the other half is inferred from the pairs involving it, and anything other than
/// exactly one pair is an error naming the candidates — inferring genuinely does need the named
/// node to be in the inventory, so [`DiscoveryError::ConstraintNotFound`] stays for that case.
/// Neither given: exactly one pair is used, zero is [`DiscoveryError::NoPair`], more is
/// [`DiscoveryError::Ambiguous`]. Nothing here ever picks one of several.
pub fn discover(
    sysfs: &dyn Sysfs,
    probe: &dyn NodeProbe,
    constraints: &Constraints,
) -> Result<Pair, DiscoveryError> {
    let inv = inventory(sysfs, probe);

    // Before any of discovery's own verdicts, including "no nodes at all": a user who named both
    // halves has already answered the question those verdicts exist to answer.
    if let (Some(video), Some(serial)) = (&constraints.video, &constraints.serial) {
        return Ok(explicit_pair(sysfs, &inv, video, serial));
    }

    if inv.videos.is_empty() {
        return Err(DiscoveryError::NoVideoNodes);
    }
    if inv.serials.is_empty() {
        return Err(DiscoveryError::NoSerialNodes);
    }

    match (&constraints.video, &constraints.serial) {
        (Some(p), None) => {
            let v = find_video(&inv, p)?;
            let inv = inv.retaining_pairs(|p| p.video.dev == v.dev);
            one_pair(inv, NodeKind::Video, v.dev.clone())
        }
        (None, Some(p)) => {
            let s = find_serial(&inv, p)?;
            let inv = inv.retaining_pairs(|p| p.serial.dev == s.dev);
            one_pair(inv, NodeKind::Serial, s.dev.clone())
        }
        _ => match inv.pairs.len() {
            1 => Ok(inv.pairs[0].clone()),
            0 => Err(DiscoveryError::NoPair(inv)),
            _ => Err(ambiguous(inv)),
        },
    }
}

/// The pair a user asked for by naming both halves (§8), with whatever evidence happens to
/// support it.
///
/// A node the class walk enumerated is reused as it stands, so the listing keeps its USB device
/// and probe answer; one it did not is carried as just the path the user typed. Evidence is
/// recomputed from the two USB devices rather than looked up in `inv.pairs`, because the pair
/// list holds only *selectable* combinations — an explicit `--video` naming the metadata node is
/// still worth telling the user is on the right dongle.
fn explicit_pair(sysfs: &dyn Sysfs, inv: &Inventory, video: &Path, serial: &Path) -> Pair {
    let video = inv.video(video).cloned().unwrap_or_else(|| VideoNode {
        dev: video.to_path_buf(),
        usb: None,
        is_capture: None,
        probe_error: None,
        probed: false,
    });
    let serial = inv.serial(serial).cloned().unwrap_or_else(|| SerialNode {
        dev: serial.to_path_buf(),
        usb: None,
    });
    let evidence = video
        .usb
        .as_ref()
        .zip(serial.usb.as_ref())
        .and_then(|(v, s)| evidence_for(sysfs, v, s));
    Pair {
        video,
        serial,
        evidence,
    }
}

/// Exactly one pair in `inv`, or the error naming what the one constraint left standing.
fn one_pair(inv: Inventory, kind: NodeKind, path: PathBuf) -> Result<Pair, DiscoveryError> {
    match inv.pairs.len() {
        1 => Ok(inv.pairs[0].clone()),
        0 => Err(DiscoveryError::ConstraintUnpaired {
            kind,
            path,
            inventory: inv,
        }),
        _ => Err(ambiguous(inv)),
    }
}

/// "More than one candidate pair" — unless the reason nothing could be ruled out is that every
/// probe was refused, in which case that is the thing to say.
fn ambiguous(inv: Inventory) -> DiscoveryError {
    if inv.every_probe_denied() {
        DiscoveryError::ProbesDenied(inv)
    } else {
        DiscoveryError::Ambiguous(inv)
    }
}

fn find_video(inv: &Inventory, path: &Path) -> Result<VideoNode, DiscoveryError> {
    inv.video(path)
        .cloned()
        .ok_or_else(|| DiscoveryError::ConstraintNotFound {
            kind: NodeKind::Video,
            path: path.to_path_buf(),
            available: inv.videos.iter().map(|v| v.dev.clone()).collect(),
        })
}

fn find_serial(inv: &Inventory, path: &Path) -> Result<SerialNode, DiscoveryError> {
    inv.serial(path)
        .cloned()
        .ok_or_else(|| DiscoveryError::ConstraintNotFound {
            kind: NodeKind::Serial,
            path: path.to_path_buf(),
            available: inv.serials.iter().map(|s| s.dev.clone()).collect(),
        })
}

// ---- node collection ---------------------------------------------------------------------

fn collect_video_nodes(sysfs: &dyn Sysfs) -> Vec<VideoNode> {
    class_nodes(sysfs, VIDEO_CLASS, VIDEO_PREFIX)
        .into_iter()
        .map(|(dev, usb)| VideoNode {
            dev,
            usb: Some(usb),
            is_capture: None,
            probe_error: None,
            probed: false,
        })
        .collect()
}

fn collect_serial_nodes(sysfs: &dyn Sysfs) -> Vec<SerialNode> {
    class_nodes(sysfs, TTY_CLASS, TTY_PREFIX)
        .into_iter()
        .map(|(dev, usb)| SerialNode {
            dev,
            usb: Some(usb),
        })
        .collect()
}

/// Every entry of a class directory whose name starts with `prefix` and which resolves up to a
/// USB device. Entries that do not (a PCI capture card, a virtual tty) are silently skipped:
/// they cannot be this dongle, and listing them would only pad the error message.
fn class_nodes(sysfs: &dyn Sysfs, class: &str, prefix: &str) -> Vec<(PathBuf, UsbDevice)> {
    sysfs
        .list_dir(Path::new(class))
        .into_iter()
        .filter_map(|entry| {
            let name = entry.file_name()?.to_str()?.to_string();
            if !name.starts_with(prefix) {
                return None;
            }
            let usb = usb_device_of(sysfs, &entry)?;
            Some((PathBuf::from("/dev").join(name), usb))
        })
        .collect()
}

/// §8: resolve a class node's `device` link **up to the USB device**, not to an arbitrary
/// ancestor. The link lands on a USB *interface* (`3-2.2.2:1.0`), which has no `idVendor`; the
/// first ancestor that does is the device. Stopping anywhere higher would make every device
/// behind a hub look like the same device.
fn usb_device_of(sysfs: &dyn Sysfs, class_dir: &Path) -> Option<UsbDevice> {
    let mut dir = sysfs.read_link(&class_dir.join("device"))?;
    loop {
        if sysfs.exists(&dir.join("idVendor")) {
            return read_usb_device(sysfs, &dir);
        }
        let parent = dir.parent()?;
        if parent == dir || parent.as_os_str().is_empty() {
            return None;
        }
        dir = parent.to_path_buf();
    }
}

fn read_usb_device(sysfs: &dyn Sysfs, dir: &Path) -> Option<UsbDevice> {
    let vid = hex16(sysfs.read_attr(dir, "idVendor")?)?;
    let pid = hex16(sysfs.read_attr(dir, "idProduct")?)?;
    Some(UsbDevice {
        sysfs: dir.to_path_buf(),
        name: dir.file_name()?.to_str()?.to_string(),
        vid,
        pid,
        product: sysfs.read_attr(dir, "product"),
        serial: sysfs.read_attr(dir, "serial"),
        speed: sysfs.read_attr(dir, "speed"),
        busnum: sysfs
            .read_attr(dir, "busnum")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        devnum: sysfs
            .read_attr(dir, "devnum")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
    })
}

fn hex16(s: String) -> Option<u16> {
    u16::from_str_radix(s.trim(), 16).ok()
}

// ---- the evidence rules ------------------------------------------------------------------

/// The strongest evidence joining these two USB devices, or `None` for "unrelated".
fn evidence_for(sysfs: &dyn Sysfs, video: &UsbDevice, serial: &UsbDevice) -> Option<Evidence> {
    if same_device(video, serial) {
        return Some(Evidence::SameDevice);
    }
    if let Some(e) = port_peer(sysfs, video, serial) {
        return Some(e);
    }
    internal_hub(sysfs, video, serial)
}

/// §8 evidence 1. `busnum:devnum` is the kernel's identity for a USB device, but both attributes
/// can be missing from a partial snapshot, and `0:0` is not a device — so the sysfs paths are
/// compared as well and either one being equal is enough.
fn same_device(video: &UsbDevice, serial: &UsbDevice) -> bool {
    video.sysfs == serial.sysfs
        || (video.busnum == serial.busnum && video.devnum == serial.devnum && video.devnum != 0)
}

/// §8 evidence 2, the kernel's own assertion.
///
/// Find the hub port the video device is plugged into, follow its `peer` symlink to the port on
/// the companion bus, resolve that port's `device`, and require the serial device to be it or to
/// sit underneath it. Two peered ports are the same physical connector, so nothing about the
/// devices themselves needs checking.
fn port_peer(sysfs: &dyn Sysfs, video: &UsbDevice, serial: &UsbDevice) -> Option<Evidence> {
    let video_port = port_dir_of(&video.sysfs)?;
    let peer = sysfs.read_link(&video_port.join("peer"))?;
    let peer_device = sysfs.read_link(&peer.join("device"))?;
    if serial.sysfs != peer_device && !serial.sysfs.starts_with(&peer_device) {
        return None;
    }
    Some(Evidence::PortPeer {
        video_port: file_name(&video_port)?,
        peer_port: file_name(&peer)?,
    })
}

/// §8 evidence 3, narrowed. See the module docs for why all three ids are required.
fn internal_hub(sysfs: &dyn Sysfs, video: &UsbDevice, serial: &UsbDevice) -> Option<Evidence> {
    if video.id() != VIDEO_ID || serial.id() != SERIAL_ID {
        return None;
    }
    let hub_dir = nearest_common_usb_ancestor(sysfs, &video.sysfs, &serial.sysfs)?;
    // Direct children only. A device two hops down is behind a further hub, which is a different
    // physical arrangement than the one §8 measured and is not covered by this rule.
    if video.parent() != Some(hub_dir.as_path()) || serial.parent() != Some(hub_dir.as_path()) {
        return None;
    }
    let hub = read_usb_device(sysfs, &hub_dir)?;
    if hub.id() != INTERNAL_HUB_ID {
        return None;
    }
    Some(Evidence::InternalHub { hub })
}

/// The deepest directory both paths share that is itself a USB device.
fn nearest_common_usb_ancestor(sysfs: &dyn Sysfs, a: &Path, b: &Path) -> Option<PathBuf> {
    let mut common = PathBuf::new();
    for (x, y) in a.components().zip(b.components()) {
        if x != y {
            break;
        }
        common.push(x);
    }
    loop {
        if sysfs.exists(&common.join("idVendor")) {
            return Some(common);
        }
        let parent = common.parent()?;
        if parent == common || parent.as_os_str().is_empty() {
            return None;
        }
        common = parent.to_path_buf();
    }
}

/// The sysfs hub-port device a USB device is plugged into.
///
/// The kernel puts port directories under the *hub's interface 0*, named after the hub, with the
/// root hub spelling itself `usb<bus>` in the device name and `usb<bus>-port<n>` in the port
/// name. `scripts/device-health.py` computes the same path; the two must stay in step.
fn port_dir_of(device: &Path) -> Option<PathBuf> {
    let name = device.file_name()?.to_str()?;
    let parent = device.parent()?;
    let hub = parent.file_name()?.to_str()?;
    let (bus, path) = name.split_once('-')?;
    let port = path.rsplit('.').next()?;
    if hub.starts_with("usb") {
        Some(
            parent
                .join(format!("{bus}-0:1.0"))
                .join(format!("usb{bus}-port{port}")),
        )
    } else {
        Some(
            parent
                .join(format!("{hub}:1.0"))
                .join(format!("{hub}-port{port}")),
        )
    }
}

fn file_name(p: &Path) -> Option<String> {
    Some(p.file_name()?.to_str()?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_dir_of_a_device_on_an_external_hub() {
        assert_eq!(
            port_dir_of(Path::new("/devices/pci/usb3/3-2/3-2.2/3-2.2.2")),
            Some(PathBuf::from(
                "/devices/pci/usb3/3-2/3-2.2/3-2.2:1.0/3-2.2-port2"
            ))
        );
    }

    #[test]
    fn port_dir_of_a_device_on_a_root_hub() {
        assert_eq!(
            port_dir_of(Path::new("/devices/pci/usb3/3-2")),
            Some(PathBuf::from("/devices/pci/usb3/3-0:1.0/usb3-port2"))
        );
    }

    /// `scripts/device-health.py` prints these three strings verbatim so its verdict can be
    /// compared with the client's word for word. Changing one here without changing it there
    /// would silently let the health check and the client disagree.
    #[test]
    fn every_evidence_message_opens_with_its_kind() {
        let hub = UsbDevice {
            sysfs: PathBuf::from("/devices/3-2.2"),
            name: "3-2.2".to_string(),
            vid: INTERNAL_HUB_ID.0,
            pid: INTERNAL_HUB_ID.1,
            product: None,
            serial: None,
            speed: None,
            busnum: 3,
            devnum: 31,
        };
        for evidence in [
            Evidence::SameDevice,
            Evidence::PortPeer {
                video_port: "4-2-port2".to_string(),
                peer_port: "3-2-port2".to_string(),
            },
            Evidence::InternalHub { hub },
        ] {
            let text = evidence.to_string();
            assert!(
                text.starts_with(evidence.kind()),
                "{text:?} must open with {:?}",
                evidence.kind()
            );
            if evidence.is_proof() {
                assert!(text.ends_with("proof"), "{text:?}");
            } else {
                // §8 calls item 3 a degraded common-ancestor test. The word "proof" must not
                // appear at all — not even as "not proof", which is what a user skims past.
                assert!(!text.contains("proof"), "{text:?}");
                assert!(text.contains("degraded"), "{text:?}");
            }
        }
    }

    /// The three strings `scripts/device-health.py` prints. Kept as literals here so a change to
    /// either implementation has to be made in both places or this fails.
    #[test]
    fn the_evidence_kinds_are_the_ones_device_health_prints() {
        let script = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/device-health.py"),
        )
        .expect("scripts/device-health.py must be readable");
        for kind in ["same USB device", "port peer", "internal hub (degraded)"] {
            assert!(
                script.contains(kind),
                "scripts/device-health.py no longer prints {kind:?}; the health check and the \
                 client must report the same verdict in the same words"
            );
        }
    }

    #[test]
    fn port_dir_of_a_non_usb_name_is_none() {
        assert_eq!(port_dir_of(Path::new("/devices/pci/platform")), None);
    }

    #[test]
    fn same_device_ignores_a_zero_devnum() {
        let make = |name: &str, devnum: u32| UsbDevice {
            sysfs: PathBuf::from("/devices").join(name),
            name: name.to_string(),
            vid: 1,
            pid: 2,
            product: None,
            serial: None,
            speed: None,
            busnum: 3,
            devnum,
        };
        assert!(!same_device(&make("a", 0), &make("b", 0)));
        assert!(same_device(&make("a", 7), &make("b", 7)));
    }
}
