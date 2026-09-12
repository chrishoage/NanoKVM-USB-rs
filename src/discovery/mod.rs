//! Video, serial, and audio pairing from USB topology.
//!
//! Video and serial may be separate USB devices on different buses. Pairing prefers a
//! shared USB device, then the kernel's SuperSpeed/HighSpeed port peer link. The fallback
//! requires direct children of a 1a40:0101 hub with video 345f:2133 and serial 1a86:55d3.
//! That fallback is reported as degraded evidence because these identifiers are not unique.
//! Multiple candidate pairs require explicit selection.
//!
//! Candidate video nodes receive a read-only capability query to exclude metadata nodes.
//! Product strings and node numbers do not identify a dongle: both can change on reconnect.
//!
//! Audio pairs only with the capture node's own USB device. Missing or ambiguous audio
//! is reported independently and never invalidates a video/serial pair.

pub mod probe;
pub mod reopen;
pub mod sysfs;
pub mod testing;

pub use probe::{NoProbe, NodeProbe, RealProbe};
pub use reopen::{
    CardResolver, DiscoveringCardOpener, DiscoveringLinkSource, DiscoveringOpener, Mode,
    NodeResolver, ResolvedCard,
};
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
/// The class directory ALSA cards register under.
const SOUND_CLASS: &str = "/class/sound";
/// `/sys/class/sound` also holds `controlC8`, `pcmC8D0c`, `seq` and `timer`. Only `card<N>` is a
/// card, and the `<N>` is the card number — which is why it is parsed back out of the name rather
/// than remembered from anywhere (see [`SoundCard::number`]).
const CARD_PREFIX: &str = "card";

/// MACROSILICON "USB2 Video" / "USB3 Video" — the dongle's capture function.
pub const VIDEO_ID: (u16, u16) = (0x345f, 0x2133);
/// QinHeng CH9329 "USB Single Serial" — the dongle's HID bridge.
pub const SERIAL_ID: (u16, u16) = (0x1a86, 0x55d3);
/// The unbranded 4-port hub inside the dongle. Generic, hence the extra id checks on evidence 3.
pub const INTERNAL_HUB_ID: (u16, u16) = (0x1a40, 0x0101);

/// USB device resolved from a class node. `sysfs` names the device directory itself,
/// not an interface below it or a hub above it.
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
    /// USB identity, or `None` for an explicit path absent from the inventory.
    pub usb: Option<UsbDevice>,
    /// `Some(true)`/`Some(false)` from [`NodeProbe`]; `None` when the probe was not run or
    /// failed. See [`inventory`] for when it is run.
    pub is_capture: Option<bool>,
    /// Capability-query error retained for the device listing.
    pub probe_error: Option<ProbeFailure>,
    /// Whether the probe was consulted at all. Distinguishes "metadata node" from "not asked".
    pub probed: bool,
}

impl VideoNode {
    /// Whether this node is eligible for selection. An inconclusive probe stays eligible
    /// unless a sibling on the same USB device is confirmed as the capture node.
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

/// ALSA card and its USB identity, when it has one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SoundCard {
    /// sysfs-absolute path of the card directory, e.g. `/devices/.../3-2.2.2:1.2/sound/card8`.
    pub sysfs: PathBuf,
    /// The kernel's name for it, e.g. `card8`. This is where the card number comes from.
    pub name: String,
    /// The card's ALSA id (`/sys/class/sound/cardN/id`), e.g. `Video`. Used only for the
    /// `hw:CARD=` spelling and for the listing; nothing pairs on it.
    pub id: Option<String>,
    /// The USB device the class walk resolved this card to. `None` when it resolves to something
    /// that is not a USB device at all — a PCI codec, an HDMI audio function — which is the
    /// common case on any desk and is why this is an `Option` rather than a filter.
    pub usb: Option<UsbDevice>,
}

impl SoundCard {
    /// Parse the card number from the current name. Resolve the card again before opening
    /// after replug; a stored number can refer to another device.
    pub fn number(&self) -> Option<u32> {
        self.name.strip_prefix(CARD_PREFIX)?.parse().ok()
    }

    /// ALSA name for the first PCM, such as `hw:8`, or `None` for an invalid card name.
    pub fn alsa_device(&self) -> Option<String> {
        Some(format!("hw:{}", self.number()?))
    }

    /// The same card by its ALSA id, e.g. `hw:CARD=Video`. For logs: it is stable across a
    /// renumber where [`SoundCard::alsa_device`] is not, but it is not unique — two identical
    /// dongles produce two cards called `Video`, and ALSA then resolves this to the lower one. So
    /// it is printed and never opened.
    pub fn alsa_card_id(&self) -> Option<String> {
        Some(format!("hw:CARD={}", self.id.as_ref()?))
    }
}

impl fmt::Display for SoundCard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)?;
        if let Some(id) = &self.id {
            write!(f, " \"{id}\"")?;
        }
        if let Some(dev) = self.alsa_device() {
            write!(f, " ({dev})")?;
        }
        match &self.usb {
            Some(u) => write!(f, " on {u}"),
            // Not the `describe` wording the node listings use: a PCI codec is not a node
            // discovery failed to enumerate, it is a card that could never pair, and saying so is
            // the answer to "why is my card not listed against the dongle?".
            None => write!(f, " — not a USB device"),
        }
    }
}

/// Audio pairing result. Missing or ambiguous audio does not invalidate video or input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioPairing {
    /// Exactly one card is on the capture node's own USB device.
    Paired(SoundCard),
    /// No card is. An ordinary outcome: a dongle whose audio interface is unbound, a kernel
    /// without `snd-usb-audio`, or a capture device that simply has no audio function.
    NoCard,
    /// Multiple cards share the USB device; list them without selecting one.
    Ambiguous(Vec<SoundCard>),
    /// The video node was named explicitly and discovery never enumerated it, so there is no USB
    /// device to compare a card against. Not "no audio": "not asked".
    Unknown,
}

impl AudioPairing {
    /// The card to open, if there is one.
    pub fn card(&self) -> Option<&SoundCard> {
        match self {
            AudioPairing::Paired(c) => Some(c),
            _ => None,
        }
    }
}

impl fmt::Display for AudioPairing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AudioPairing::Paired(card) => write!(
                f,
                "{card} — same USB device as the capture node (busnum:devnum) — proof"
            ),
            AudioPairing::NoCard => write!(
                f,
                "no audio: no sound card belongs to the capture node's USB device"
            ),
            AudioPairing::Ambiguous(cards) => {
                write!(
                    f,
                    "no audio: {} sound cards belong to the capture node's USB device, \
                           refusing to guess between",
                    cards.len()
                )?;
                for c in cards {
                    write!(f, " {}", c.name)?;
                }
                Ok(())
            }
            AudioPairing::Unknown => write!(
                f,
                "no audio: the video node was given explicitly and discovery never enumerated \
                 it, so there is no USB device to match a sound card against"
            ),
        }
    }
}

/// Find the card on the video node's own USB device. Shared-hub containment is
/// insufficient because another sound card could occupy a different port on that hub.
fn audio_for(video: Option<&UsbDevice>, cards: &[SoundCard]) -> AudioPairing {
    let Some(video) = video else {
        return AudioPairing::Unknown;
    };
    let mut matched: Vec<SoundCard> = cards
        .iter()
        .filter(|c| c.usb.as_ref().is_some_and(|u| same_device(video, u)))
        .cloned()
        .collect();
    match matched.len() {
        0 => AudioPairing::NoCard,
        1 => AudioPairing::Paired(matched.remove(0)),
        _ => AudioPairing::Ambiguous(matched),
    }
}

/// Why two nodes are believed to be one dongle.
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
    /// Matching video and serial devices are direct children of the expected internal hub.
    /// This is degraded evidence: the three device identifiers are commodity part numbers.
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

    /// Whether pairing uses shared-device or kernel port-peer evidence.
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
            // Label the hub heuristic as degraded so it cannot be mistaken for unique identity.
            Evidence::InternalHub { hub } => write!(
                f,
                "internal hub (degraded): both are direct children of the dongle's own hub {hub} \
                 — an internal-hub containment test, contained by that hub and by all three vendor ids, \
                 which are commodity part numbers rather than a kernel assertion"
            ),
        }
    }
}

/// Selected video and serial nodes with optional pairing evidence.
///
/// Explicit paths are honored even when no topology rule connects them; in that case
/// `evidence` is `None` and the caller reports the missing evidence.
#[derive(Clone, Debug)]
pub struct Pair {
    pub video: VideoNode,
    pub serial: SerialNode,
    pub evidence: Option<Evidence>,
    /// The sound card on the capture node's own USB device. Never an error: audio
    /// is a side channel and its absence changes nothing about this pair.
    pub audio: AudioPairing,
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
        )?;
        write!(f, "\n      audio: {}", self.audio)
    }
}

/// Device inventory, including unpaired nodes and selection evidence.
#[derive(Clone, Debug, Default)]
pub struct Inventory {
    pub videos: Vec<VideoNode>,
    pub serials: Vec<SerialNode>,
    /// All enumerated sound cards, including non-USB cards, for explaining failed pairing.
    pub cards: Vec<SoundCard>,
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
            cards: self.cards.clone(),
            pairs: self.pairs.iter().filter(|p| keep(p)).cloned().collect(),
        }
    }

    /// List devices with explicit paths marked and their resulting selection shown.
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

/// Static empty constraints because [`Listing`] borrows its selection.
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
        writeln!(f, "sound cards:")?;
        if inv.cards.is_empty() {
            writeln!(f, "  (none)")?;
        }
        for c in &inv.cards {
            writeln!(f, "   {c}")?;
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

        // Include explicit paths absent from sysfs so the listing still reflects the request.
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
                     given.",
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

/// Explicit `--video`/`--serial` selection.
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
    /// what to type.
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

/// Why discovery could not name one pair.
///
/// Every variant's `Display` names every node and every `vid:pid` found, because the action the
/// user has to take is to pick two of them and pass them on the command line.
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

/// Build the inventory and apply pairing rules.
///
/// Topology is resolved before probing. Only video nodes in a candidate pair receive
/// a read-only capability query; unrelated nodes remain unprobed. An inconclusive
/// probe stays eligible unless a sibling is confirmed as the capture node.
pub fn inventory(sysfs: &dyn Sysfs, probe: &dyn NodeProbe) -> Inventory {
    let mut videos = collect_video_nodes(sysfs);
    let serials = collect_serial_nodes(sysfs);
    // Read-only, and no ALSA: discovery's rule is `/sys` plus one `QUERYCAP` and nothing else
    // . Opening the card is `audio`'s business, at the moment it wants samples.
    let cards = collect_sound_cards(sysfs);

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
            audio: audio_for(videos[vi].usb.as_ref(), &cards),
            video: videos[vi].clone(),
            serial: serials[si].clone(),
            evidence: Some(evidence),
        })
        .collect();

    Inventory {
        videos,
        serials,
        cards,
        pairs,
    }
}

/// Select a pair or return a diagnostic inventory.
///
/// Two explicit paths are accepted even without inventory evidence. One explicit path
/// constrains candidate pairs and requires a unique match. With neither path, zero
/// pairs returns [`DiscoveryError::NoPair`] and multiple pairs returns
/// [`DiscoveryError::Ambiguous`]. Inventory is still collected for diagnostics.
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

/// Construct an explicitly selected pair. Reuse available node metadata and compute
/// evidence directly, even if the pair would not be automatically selectable.
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
    // The card is matched against whatever the *video* node resolved to, which for a node
    // discovery never enumerated is nothing at all — `AudioPairing::Unknown`, not "no audio".
    let audio = audio_for(video.usb.as_ref(), &inv.cards);
    Pair {
        video,
        serial,
        evidence,
        audio,
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

/// Every `/sys/class/sound/card<N>`, USB or not.
///
/// Unlike the video and tty walks this keeps the entries that do not resolve to a USB device.
/// A desk has several — the HDMI codec, the motherboard's analogue output — and they are the
/// negative control the pairing rule is checked against, as well as the answer to a user asking
/// why their card did not pair. `controlC8`, `pcmC8D0c`, `seq` and `timer` share this directory
/// and are not cards; the `card` prefix plus the number parse in [`SoundCard::number`] is what
/// keeps them out.
fn collect_sound_cards(sysfs: &dyn Sysfs) -> Vec<SoundCard> {
    sysfs
        .list_dir(Path::new(SOUND_CLASS))
        .into_iter()
        .filter_map(|entry| {
            let name = entry.file_name()?.to_str()?.to_string();
            let card = SoundCard {
                sysfs: sysfs.read_link(&entry).unwrap_or(entry.clone()),
                name,
                id: sysfs.read_attr(&entry, "id"),
                usb: usb_device_of(sysfs, &entry),
            };
            // `card.number()` parses the name, so this is also the filter that drops `controlC8`
            // and friends: nothing without a `card<N>` name gets in, and nothing that gets in can
            // fail to produce an ALSA device string later.
            card.number().map(|_| card)
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

/// Stop at the USB device, identified by `idVendor`; walking farther would collapse
/// distinct devices under their shared hub.
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

/// Compare non-missing bus/device identity or canonical device paths. Missing numeric
/// attributes must not make unrelated snapshot nodes equal.
fn same_device(video: &UsbDevice, serial: &UsbDevice) -> bool {
    video.sysfs == serial.sysfs
        || (video.busnum == serial.busnum && video.devnum == serial.devnum && video.devnum != 0)
}

/// Follow the video port's kernel peer link to its companion bus and require serial
/// to be that device or descend from it. The peer link identifies one physical connector.
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

/// Require the known hub, video, and serial identifiers for the fallback heuristic.
fn internal_hub(sysfs: &dyn Sysfs, video: &UsbDevice, serial: &UsbDevice) -> Option<Evidence> {
    if video.id() != VIDEO_ID || serial.id() != SERIAL_ID {
        return None;
    }
    let hub_dir = nearest_common_usb_ancestor(sysfs, &video.sysfs, &serial.sysfs)?;
    // Nested hubs are a different topology and do not satisfy the direct-child rule.
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
                // The hub heuristic must be labeled degraded in diagnostics.
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
