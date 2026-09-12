//! Device resolvers for serial, video, and audio recovery.
//!
//! Explicit paths are reopened unchanged. Discovered video and serial nodes are resolved
//! again because open descriptors can keep their old indices reserved across a replug.
//!
//! Audio resolves sound cards against the selected capture device's USB directory, without
//! probing video or serial nodes. Moving the dongle to another physical port changes that
//! directory and requires a viewer restart for audio; same-port replug can renumber cards.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

use super::probe::{NodeProbe, RealProbe};
use super::sysfs::{RealSysfs, Sysfs};
use super::{AudioPairing, Constraints, DiscoveryError, Evidence, NodeKind, SoundCard};
use crate::audio::{AudioConfig, AudioError, PcmSource, PcmSourceOpener};
use crate::capture::{CaptureError, FrameSource, SourceOpener, V4l2Opener};
use crate::link::{Link, LinkError, LinkSource, Reply};
use crate::serial::{OpenOptions, SerialLinkSource};

/// Fixed or rediscovered node selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The path the user typed. Reopened as typed, for ever; discovery is never consulted
    /// for it.
    Explicit(PathBuf),
    /// Discovered. [`discover`](super::discover) is re-run with the original constraints on every
    /// attempt, so a node that came back under a different name is found.
    Discover,
}

/// One node's path, and what justifies it.
#[derive(Clone, Debug)]
pub struct Resolved {
    /// The device node to open.
    pub path: PathBuf,
    /// Pairing evidence when available; explicit paths may have none.
    pub evidence: Option<Evidence>,
    /// Whether discovery was consulted at all. `false` is [`Mode::Explicit`].
    pub discovered: bool,
}

/// Resolve node paths using the original constraints. Retaining the constraints rather
/// than the first answer allows rediscovery after node renumbering.
pub struct NodeResolver {
    kind: NodeKind,
    constraints: Constraints,
    sysfs: Box<dyn Sysfs + Send>,
    probe: Box<dyn NodeProbe + Send>,
}

impl NodeResolver {
    /// Create a resolver with injectable sysfs and capability access.
    pub fn new(
        kind: NodeKind,
        constraints: Constraints,
        sysfs: Box<dyn Sysfs + Send>,
        probe: Box<dyn NodeProbe + Send>,
    ) -> Self {
        NodeResolver {
            kind,
            constraints,
            sysfs,
            probe,
        }
    }

    /// A resolver against the real `/sys` and a real `VIDIOC_QUERYCAP` probe.
    pub fn real(kind: NodeKind, constraints: Constraints) -> Self {
        Self::new(
            kind,
            constraints,
            Box::new(RealSysfs::default()),
            Box::new(RealProbe),
        )
    }

    /// Which node this resolver is for.
    pub fn kind(&self) -> NodeKind {
        self.kind
    }

    /// This node's mode, which is decided by the constraint the user gave for *this* node and by
    /// nothing else.
    pub fn mode(&self) -> Mode {
        let explicit = match self.kind {
            NodeKind::Video => &self.constraints.video,
            NodeKind::Serial => &self.constraints.serial,
        };
        match explicit {
            Some(path) => Mode::Explicit(path.clone()),
            None => Mode::Discover,
        }
    }

    /// Resolve the path for this open attempt. Explicit mode performs no discovery;
    /// discovered mode can fail temporarily during re-enumeration.
    pub fn resolve(&self) -> Result<Resolved, DiscoveryError> {
        if let Mode::Explicit(path) = self.mode() {
            return Ok(Resolved {
                path,
                evidence: None,
                discovered: false,
            });
        }
        let pair = super::discover(&*self.sysfs, &*self.probe, &self.constraints)?;
        let path = match self.kind {
            NodeKind::Video => pair.video.dev,
            NodeKind::Serial => pair.serial.dev,
        };
        Ok(Resolved {
            path,
            evidence: pair.evidence,
            discovered: true,
        })
    }
}

impl std::fmt::Debug for NodeResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeResolver")
            .field("kind", &self.kind)
            .field("mode", &self.mode())
            .finish_non_exhaustive()
    }
}

/// Log the selected path and evidence, warning if the node path changed.
fn log_resolution(kind: NodeKind, resolved: &Resolved, previous: Option<&Path>) {
    let path = resolved.path.display();
    let because = match (&resolved.evidence, resolved.discovered) {
        (Some(e), _) => format!("rediscovered — {e}"),
        (None, true) => {
            "rediscovered — no USB topology evidence (both nodes given explicitly)".to_string()
        }
        (None, false) => format!("{} was given explicitly", kind.flag()),
    };
    match previous {
        Some(p) if p != resolved.path => log::warn!(
            "{kind} node moved: {} is now {path} ({because})",
            p.display()
        ),
        _ => log::info!("{kind} node for this attempt: {path} ({because})"),
    }
}

/// Serial source that resolves its path before each open and delegates port setup
/// and unsolicited-message forwarding to [`SerialLinkSource`].
pub struct DiscoveringLinkSource {
    resolver: NodeResolver,
    opts: OpenOptions,
    /// The inner source and the path it opens. Replaced only when the path changes, so an
    /// ordinary reconnect at the same path reuses it and its forwarder bookkeeping.
    current: Option<(PathBuf, SerialLinkSource)>,
    unsolicited: Sender<Reply>,
    pumps: Vec<JoinHandle<()>>,
}

impl DiscoveringLinkSource {
    /// A source that resolves through `resolver`, and the unsolicited-frame receiver that
    /// survives every reopen and every rename.
    pub fn new(resolver: NodeResolver, opts: OpenOptions) -> (Self, Receiver<Reply>) {
        let (tx, rx) = mpsc::channel();
        (
            DiscoveringLinkSource {
                resolver,
                opts,
                current: None,
                unsolicited: tx,
                pumps: Vec::new(),
            },
            rx,
        )
    }

    /// Join the pumps whose inner sources have already gone. Never blocks: `is_finished` is
    /// checked first (the same rule `SerialLinkSource::reap` follows).
    fn reap(&mut self) {
        let mut live = Vec::with_capacity(self.pumps.len());
        for handle in self.pumps.drain(..) {
            if handle.is_finished() {
                let _ = handle.join();
            } else {
                live.push(handle);
            }
        }
        self.pumps = live;
    }

    /// Point the inner source at `path`, building a new one (and a new pump) only if the path
    /// changed.
    fn inner_for(&mut self, path: &Path) -> Result<&mut SerialLinkSource, LinkError> {
        if self.current.as_ref().is_none_or(|(p, _)| p != path) {
            self.rebuild(path)?;
        }
        match self.current.as_mut() {
            Some((_, source)) => Ok(source),
            // Unreachable: `rebuild` either filled the slot or returned an error. Reported rather
            // than asserted, because a panic here would be inside the writer's reconnect loop.
            None => Err(LinkError::Down(
                "internal: no serial source after resolving the node".to_string(),
            )),
        }
    }

    /// Build a fresh inner source for `path`, with a pump feeding the long-lived receiver.
    fn rebuild(&mut self, path: &Path) -> Result<(), LinkError> {
        self.reap();
        let (source, from_source) = SerialLinkSource::new(path, self.opts);
        let to_logger = self.unsolicited.clone();
        let handle = std::thread::Builder::new()
            .name("nanokvm-serial-unsol-pump".to_string())
            .spawn(move || {
                // Stop forwarding when the source, links, or receiving logger are dropped.
                for reply in from_source {
                    if to_logger.send(reply).is_err() {
                        log::debug!("unsolicited consumer went away; pump ending");
                        return;
                    }
                }
                log::debug!("serial source replaced; unsolicited pump ending");
            })
            .map_err(|e| LinkError::Down(format!("spawn unsolicited pump thread: {e}")))?;
        self.pumps.push(handle);
        // Dropping the previous source ends its own forwarders once its links have gone.
        self.current = Some((path.to_path_buf(), source));
        Ok(())
    }
}

impl LinkSource for DiscoveringLinkSource {
    fn open(&mut self) -> Result<Box<dyn Link>, LinkError> {
        let previous = self.current.as_ref().map(|(p, _)| p.clone());
        // A discovery failure here is `Down`, which is what the writer's backoff is for: while
        // the dongle is out of its socket there is no pair to find, and that is not an error
        // worth giving up on.
        let resolved = self
            .resolver
            .resolve()
            .map_err(|e| LinkError::Down(format!("serial node: {e}")))?;
        log_resolution(NodeKind::Serial, &resolved, previous.as_deref());
        self.inner_for(&resolved.path)?.open()
    }

    /// Describes the *intent*, because the writer takes this once at construction — before any
    /// node has been resolved — and reuses it in every later log line, including the ones about
    /// links opened long afterwards.
    fn describe(&self) -> String {
        match self.resolver.mode() {
            Mode::Explicit(p) => format!("{} at {} baud", p.display(), self.opts.baud),
            Mode::Discover => format!(
                "the serial node USB discovery selects, at {} baud (rediscovered on each attempt)",
                self.opts.baud
            ),
        }
    }
}

/// Video opener that resolves the path before reconnect.
///
/// The first source is already open so startup errors can be reported before creating
/// a window. Later opens recover without stopping input.
pub struct DiscoveringOpener {
    resolver: NodeResolver,
    width: u32,
    height: u32,
    fps: u32,
    /// The source `main` opened, handed over on the first call and never again.
    first: Option<Box<dyn FrameSource>>,
    /// The path most recently opened, for the "the node moved" log line.
    last: Option<PathBuf>,
}

impl DiscoveringOpener {
    /// An opener that yields `first` (already streaming from `path`) and then reopens by
    /// resolving through `resolver`.
    pub fn with_open_source(
        resolver: NodeResolver,
        first: Box<dyn FrameSource>,
        path: impl Into<PathBuf>,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Self {
        DiscoveringOpener {
            resolver,
            width,
            height,
            fps,
            first: Some(first),
            last: Some(path.into()),
        }
    }

    /// An opener with no source in hand: every call resolves and opens. Used by tests and by any
    /// caller that does not want the fail-fast startup.
    pub fn new(resolver: NodeResolver, width: u32, height: u32, fps: u32) -> Self {
        DiscoveringOpener {
            resolver,
            width,
            height,
            fps,
            first: None,
            last: None,
        }
    }
}

impl SourceOpener for DiscoveringOpener {
    fn open(&mut self) -> Result<Box<dyn FrameSource>, CaptureError> {
        if let Some(source) = self.first.take() {
            return Ok(source);
        }
        // Use a configuration error so ambiguous selection retains its actionable listing.
        let resolved = self
            .resolver
            .resolve()
            .map_err(|e| CaptureError::Config(format!("video node: {e}")))?;
        log_resolution(NodeKind::Video, &resolved, self.last.as_deref());
        self.last = Some(resolved.path.clone());
        V4l2Opener::new(&resolved.path, self.width, self.height, self.fps).open()
    }

    fn describe(&self) -> String {
        let (node, how) = match (self.resolver.mode(), &self.last) {
            (Mode::Explicit(p), _) => (p.display().to_string(), ""),
            (Mode::Discover, Some(p)) => (
                p.display().to_string(),
                " (node rediscovered on each reopen)",
            ),
            (Mode::Discover, None) => (
                "the node USB discovery selects".to_string(),
                " (rediscovered on each reopen)",
            ),
        };
        format!(
            "V4L2 {node} MJPG {}x{} @{} fps{how}",
            self.width, self.height, self.fps
        )
    }

    /// Renegotiate at the next open.
    ///
    /// It also discards the source `main` handed over, if that has not been taken yet: that
    /// source is already streaming at the old mode, so yielding it after a format change would
    /// silently ignore the request. Dropping it releases the node, and the next `open` resolves
    /// and opens it afresh at the new size.
    fn set_format(&mut self, width: u32, height: u32, fps: u32) -> bool {
        self.width = width;
        self.height = height;
        self.fps = fps;
        drop(self.first.take());
        true
    }
}

/// Resolve the audio card against the selected video device's USB directory.
///
/// Only sound-class sysfs is read; no video or serial probe runs during retries. Card
/// numbers are resolved again for same-port replug. Moving to another port changes
/// the USB directory and requires restarting the viewer for audio.
pub struct CardResolver {
    /// The capture node's USB device directory in sysfs (`/devices/…/3-2.2.2`), not a `/dev`
    /// name: the device is what a card pairs against, and the path survives the renumbering that
    /// both `/dev` names and card numbers go through.
    usb: PathBuf,
    sysfs: Box<dyn Sysfs + Send>,
}

impl CardResolver {
    pub fn new(usb: PathBuf, sysfs: Box<dyn Sysfs + Send>) -> Self {
        CardResolver { usb, sysfs }
    }

    /// A resolver against the real `/sys`. It opens nothing — no probe, no `/dev` node.
    pub fn real(usb: PathBuf) -> Self {
        Self::new(usb, Box::new(RealSysfs::default()))
    }

    /// Resolve the current ALSA name or return an audio-only error.
    ///
    /// USB attributes are refreshed because device numbers can change at the same port.
    /// Failures do not enter the video/serial discovery error path.
    pub fn resolve(&self) -> Result<ResolvedCard, AudioError> {
        let usb = super::read_usb_device(&*self.sysfs, &self.usb).ok_or_else(|| {
            AudioError::NoCard(format!(
                "the capture node's USB device {} is not present; the dongle is unplugged, or it \
                 came back on another port and this client would have to be restarted to follow \
                 it (video and input do follow it)",
                self.usb.display()
            ))
        })?;
        let cards = super::collect_sound_cards(&*self.sysfs);
        match super::audio_for(Some(&usb), &cards) {
            AudioPairing::Paired(card) => {
                let device = card.alsa_device().ok_or_else(|| {
                    AudioError::NoCard(format!(
                        "the paired card is named {:?}, which is not card<N>, so no ALSA device \
                         name can be derived from it",
                        card.name
                    ))
                })?;
                Ok(ResolvedCard { device, card })
            }
            other => Err(AudioError::NoCard(format!("{other} ({usb})"))),
        }
    }
}

impl std::fmt::Debug for CardResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CardResolver").finish_non_exhaustive()
    }
}

/// One resolution of the dongle's sound card.
#[derive(Clone, Debug)]
pub struct ResolvedCard {
    /// The ALSA name to open, e.g. `hw:8`. Derived from the card's sysfs name on every resolve.
    pub device: String,
    /// The card itself, for the log line.
    pub card: SoundCard,
}

/// How a resolved card is turned into an open [`PcmSource`].
///
/// Injected so the resolution policy above can be tested without ALSA and without touching the
/// user's sound hardware — which CLAUDE.md forbids in any case. Production passes
/// [`crate::audio::alsa::open_capture`].
pub type OpenCard =
    Box<dyn FnMut(&str, Option<Arc<AtomicBool>>) -> Result<Box<dyn PcmSource>, AudioError> + Send>;

/// A [`PcmSourceOpener`] that re-resolves the dongle's card before every open.
pub struct DiscoveringCardOpener {
    resolver: CardResolver,
    open: OpenCard,
    /// The ALSA name most recently opened, for the "the card moved" log line.
    last: Option<String>,
    /// Handed to every device opened, so a blocking device call can be interrupted by a shutdown
    /// ([`PcmSourceOpener::on_stop_flag`]).
    stop: Option<Arc<AtomicBool>>,
}

impl DiscoveringCardOpener {
    pub fn new(resolver: CardResolver, open: OpenCard) -> Self {
        DiscoveringCardOpener {
            resolver,
            open,
            last: None,
            stop: None,
        }
    }

    /// The production opener: resolve `usb`'s card through `/sys`, then open it with ALSA.
    pub fn real(usb: PathBuf, config: AudioConfig) -> Self {
        Self::new(
            CardResolver::real(usb),
            Box::new(move |device, stop| crate::audio::alsa::open_capture(device, &config, stop)),
        )
    }
}

impl PcmSourceOpener for DiscoveringCardOpener {
    fn open(&mut self) -> Result<Box<dyn PcmSource>, AudioError> {
        let resolved = self.resolver.resolve()?;
        match &self.last {
            Some(previous) if *previous != resolved.device => log::warn!(
                "audio card moved: {previous} is now {} ({})",
                resolved.device,
                resolved.card
            ),
            _ => log::info!(
                "audio card for this attempt: {} ({})",
                resolved.device,
                resolved.card
            ),
        }
        let source = (self.open)(&resolved.device, self.stop.clone())?;
        // Only after the open succeeded: a name that could not be opened is not the name we are
        // now on, and recording it would suppress the "moved" line on the attempt that worked.
        self.last = Some(resolved.device);
        Ok(source)
    }

    fn describe(&self) -> String {
        match &self.last {
            Some(device) => format!("ALSA {device} (the dongle's card, rediscovered on each open)"),
            None => "ALSA — the card USB discovery pairs with the capture node (rediscovered on \
                     each open)"
                .to_string(),
        }
    }

    fn on_stop_flag(&mut self, stop: Arc<AtomicBool>) {
        self.stop = Some(stop);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::discovery::testing::{fixture, MapProbe, OverrideSysfs};

    /// Device directories inside the `two-dongles` recording, spelled the way an override must
    /// spell them: the canonical `/devices/...` path, which is where `usb_device_of` lands.
    const BUS: &str = "/devices/pci0000:00/0000:00:08.1/0000:0b:00.3/usb3/3-2";

    /// Replaceable recorded sysfs tree for simulating hotplug between resolver calls.
    struct Hotplug {
        tree: Mutex<Box<dyn Sysfs + Send>>,
    }

    impl Hotplug {
        fn new(tree: impl Sysfs + Send + 'static) -> Self {
            Hotplug {
                tree: Mutex::new(Box::new(tree)),
            }
        }

        fn replace(&self, tree: impl Sysfs + Send + 'static) {
            *self.tree.lock().expect("not poisoned") = Box::new(tree);
        }
    }

    impl Sysfs for Hotplug {
        fn read_attr(&self, dir: &Path, name: &str) -> Option<String> {
            self.tree.lock().expect("not poisoned").read_attr(dir, name)
        }
        fn read_link(&self, path: &Path) -> Option<PathBuf> {
            self.tree.lock().expect("not poisoned").read_link(path)
        }
        fn list_dir(&self, dir: &Path) -> Vec<PathBuf> {
            self.tree.lock().expect("not poisoned").list_dir(dir)
        }
        fn exists(&self, path: &Path) -> bool {
            self.tree.lock().expect("not poisoned").exists(path)
        }
    }

    /// So two resolvers — one per node — can share one hotpluggable tree, which is what the
    /// binary's two resolvers do with `/sys`.
    impl<S: Sysfs> Sysfs for Arc<S> {
        fn read_attr(&self, dir: &Path, name: &str) -> Option<String> {
            (**self).read_attr(dir, name)
        }
        fn read_link(&self, path: &Path) -> Option<PathBuf> {
            (**self).read_link(path)
        }
        fn list_dir(&self, dir: &Path) -> Vec<PathBuf> {
            (**self).list_dir(dir)
        }
        fn exists(&self, path: &Path) -> bool {
            (**self).exists(path)
        }
    }

    /// A [`Sysfs`] that fails the test if it is read at all. "Discovery was not consulted" is only
    /// provable by making consultation impossible to miss.
    struct ForbiddenSysfs;

    impl Sysfs for ForbiddenSysfs {
        fn read_attr(&self, dir: &Path, name: &str) -> Option<String> {
            panic!(
                "discovery read {}/{name} for an explicit path",
                dir.display()
            );
        }
        fn read_link(&self, path: &Path) -> Option<PathBuf> {
            panic!("discovery resolved {} for an explicit path", path.display());
        }
        fn list_dir(&self, dir: &Path) -> Vec<PathBuf> {
            panic!("discovery listed {} for an explicit path", dir.display());
        }
        fn exists(&self, path: &Path) -> bool {
            panic!("discovery stat'ed {} for an explicit path", path.display());
        }
    }

    /// Change one hub identifier to leave only one pair selectable in the two-dongle fixture.
    fn only_dongle_under(hub: &str) -> impl Sysfs + Send + 'static {
        let other = if hub.ends_with("3-2.2") {
            format!("{BUS}/3-2.3")
        } else {
            format!("{BUS}/3-2.2")
        };
        OverrideSysfs::new(fixture("two-dongles")).set(other, "idProduct", "0102")
    }

    fn probe() -> MapProbe {
        MapProbe::new()
            .capture("/dev/video4", true)
            .capture("/dev/video5", false)
            .capture("/dev/video6", true)
            .capture("/dev/video7", false)
            .default_capture(false)
    }

    fn constraints(video: Option<&str>, serial: Option<&str>) -> Constraints {
        Constraints {
            video: video.map(PathBuf::from),
            serial: serial.map(PathBuf::from),
        }
    }

    #[test]
    fn an_explicit_path_is_reopened_without_consulting_discovery() {
        for kind in [NodeKind::Video, NodeKind::Serial] {
            let resolver = NodeResolver::new(
                kind,
                constraints(Some("/dev/video9"), Some("/dev/ttyACM7")),
                Box::new(ForbiddenSysfs),
                Box::new(MapProbe::new()),
            );
            assert!(matches!(resolver.mode(), Mode::Explicit(_)));
            // Twice: the second call is the reopen, which is where a "rediscover after the first
            // failure" bug would live.
            for attempt in 0..2 {
                let r = resolver.resolve().expect("an explicit path cannot fail");
                let expected = match kind {
                    NodeKind::Video => "/dev/video9",
                    NodeKind::Serial => "/dev/ttyACM7",
                };
                assert_eq!(r.path, PathBuf::from(expected), "attempt {attempt}");
                assert!(!r.discovered, "discovery must not have been consulted");
                assert!(r.evidence.is_none());
            }
        }
    }

    #[test]
    fn a_discovered_node_that_comes_back_renamed_is_found_again() {
        let sysfs = Arc::new(Hotplug::new(only_dongle_under(&format!("{BUS}/3-2.2"))));
        // One resolver per node, both asked twice across a change of tree — the replug case. A
        // resolver that cached its first answer, or reopened by the path it first returned,
        // fails here.
        let video = NodeResolver::new(
            NodeKind::Video,
            constraints(None, None),
            Box::new(Arc::clone(&sysfs)),
            Box::new(probe()),
        );
        let serial = NodeResolver::new(
            NodeKind::Serial,
            constraints(None, None),
            Box::new(Arc::clone(&sysfs)),
            Box::new(probe()),
        );

        assert_eq!(
            (
                video.resolve().expect("one pair").path,
                serial.resolve().expect("one pair").path
            ),
            (PathBuf::from("/dev/video4"), PathBuf::from("/dev/ttyACM1"))
        );

        // It came back on the other hub port, where the kernel had to give it different node
        // names because the old ones were still taken.
        sysfs.replace(only_dongle_under(&format!("{BUS}/3-2.3")));

        let after = video.resolve().expect("one pair after the replug");
        assert_eq!(after.path, PathBuf::from("/dev/video6"));
        assert!(after.discovered);
        assert!(
            after.evidence.is_some(),
            "a rediscovered node carries the evidence that chose it"
        );
        assert_eq!(
            serial.resolve().expect("one pair after the replug").path,
            PathBuf::from("/dev/ttyACM2")
        );
    }

    #[test]
    fn an_explicit_video_still_re_infers_the_serial_node_on_every_attempt() {
        // Both dongles present, so unaided discovery would refuse (`Ambiguous`). The explicit
        // video node is what narrows it, on this attempt and on every later one.
        let serial = NodeResolver::new(
            NodeKind::Serial,
            constraints(Some("/dev/video6"), None),
            Box::new(fixture("two-dongles")),
            Box::new(probe()),
        );
        assert_eq!(
            serial.resolve().expect("the pair containing video6").path,
            PathBuf::from("/dev/ttyACM2")
        );
    }

    #[test]
    fn an_ambiguous_inventory_on_reopen_is_an_error_rather_than_a_guess() {
        // The unmutated tree: two complete dongles, both pairable, nothing to choose between.
        let resolver = NodeResolver::new(
            NodeKind::Serial,
            constraints(None, None),
            Box::new(fixture("two-dongles")),
            Box::new(probe()),
        );
        let err = resolver
            .resolve()
            .expect_err("two dongles cannot be narrowed to one");
        assert!(
            matches!(err, DiscoveryError::Ambiguous(_)),
            "expected Ambiguous, got {err:?}"
        );
        // And it says what to type, because a reopen loop's error text is all the user gets.
        let text = err.to_string();
        assert!(text.contains("--serial"), "{text}");
    }

    #[test]
    fn a_dongle_that_is_not_back_yet_is_an_error_the_reopen_loop_can_retry() {
        // Both hubs relabelled: nothing pairs, which is what discovery sees between the unplug
        // and the replug.
        let sysfs = OverrideSysfs::new(fixture("two-dongles"))
            .set(format!("{BUS}/3-2.2"), "idProduct", "0102")
            .set(format!("{BUS}/3-2.3"), "idProduct", "0102");
        let resolver = NodeResolver::new(
            NodeKind::Video,
            constraints(None, None),
            Box::new(sysfs),
            Box::new(probe()),
        );
        let err = resolver.resolve().expect_err("nothing to pair");
        assert!(
            matches!(err, DiscoveryError::NoPair(_)),
            "expected NoPair, got {err:?}"
        );
    }

    #[test]
    fn an_opener_hands_over_the_source_main_opened_before_it_resolves_anything() {
        struct Dummy;
        impl FrameSource for Dummy {
            fn next_frame(&mut self) -> Result<crate::capture::CompressedFrame, CaptureError> {
                Err(CaptureError::Timeout(std::time::Duration::ZERO))
            }
            fn describe(&self) -> String {
                "the source main opened".to_string()
            }
        }
        // A `ForbiddenSysfs` resolver: if the first call resolved anything at all, this panics.
        let mut opener = DiscoveringOpener::with_open_source(
            NodeResolver::new(
                NodeKind::Video,
                constraints(None, None),
                Box::new(ForbiddenSysfs),
                Box::new(MapProbe::new()),
            ),
            Box::new(Dummy),
            "/dev/video4",
            1920,
            1080,
            60,
        );
        let first = opener.open().expect("the already-open source");
        assert_eq!(first.describe(), "the source main opened");
        assert!(
            opener.describe().contains("/dev/video4"),
            "describe names the node in hand: {}",
            opener.describe()
        );
    }
    // ---- the sound card ------------------------------------------------------

    /// The `usb2-desk` recording's capture device: `3-2.2.2`, the one `/dev/video4` resolves to
    /// and the one `card8` is an interface of. This is what the viewer hands the card opener.
    const DESK_CAPTURE_USB: &str =
        "/devices/pci0000:00/0000:00:08.1/0000:0b:00.3/usb3/3-2/3-2.2/3-2.2.2";

    /// A [`PcmSource`] that reads nothing. The audio reopen tests are about *which name* was
    /// opened; what comes out of it is `tests/audio_path.rs`'s business.
    struct NoSource(String);

    impl crate::audio::PcmSource for NoSource {
        fn read_period(&mut self, _out: &mut [i16]) -> Result<(), AudioError> {
            Err(AudioError::Io {
                device: self.0.clone(),
                why: "this fake never reads".to_string(),
            })
        }
        fn describe(&self) -> String {
            format!("fake source on {}", self.0)
        }
    }

    /// An opener that records every ALSA name it was asked to open, and opens none of them.
    fn recording_opener(
        resolver: CardResolver,
    ) -> (DiscoveringCardOpener, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let opener = DiscoveringCardOpener::new(
            resolver,
            Box::new(move |device: &str, _stop| {
                recorder
                    .lock()
                    .expect("not poisoned")
                    .push(device.to_string());
                Ok(Box::new(NoSource(device.to_string())) as Box<dyn crate::audio::PcmSource>)
            }),
        );
        (opener, seen)
    }

    /// A recorded tree with one sound-class entry renamed, which is the one mutation
    /// [`OverrideSysfs`] cannot express: a card's *number* lives in its directory name, not in an
    /// attribute. Everything else, including the `device` link that decides the pairing, is the
    /// real recording.
    struct Renumbered<S: Sysfs> {
        inner: S,
        from: &'static str,
        to: &'static str,
    }

    impl<S: Sysfs> Renumbered<S> {
        /// The path as the inner tree spells it.
        fn back(&self, path: &Path) -> PathBuf {
            let prefix = Path::new(super::super::SOUND_CLASS).join(self.to);
            match path.strip_prefix(&prefix) {
                Ok(rest) => Path::new(super::super::SOUND_CLASS)
                    .join(self.from)
                    .join(rest),
                Err(_) => path.to_path_buf(),
            }
        }
    }

    impl<S: Sysfs> Sysfs for Renumbered<S> {
        fn read_attr(&self, dir: &Path, name: &str) -> Option<String> {
            self.inner.read_attr(&self.back(dir), name)
        }
        fn read_link(&self, path: &Path) -> Option<PathBuf> {
            self.inner.read_link(&self.back(path))
        }
        fn list_dir(&self, dir: &Path) -> Vec<PathBuf> {
            self.inner
                .list_dir(&self.back(dir))
                .into_iter()
                .map(|e| {
                    if e.file_name().and_then(|n| n.to_str()) == Some(self.from) {
                        e.with_file_name(self.to)
                    } else {
                        e
                    }
                })
                .collect()
        }
        fn exists(&self, path: &Path) -> bool {
            self.inner.exists(&self.back(path))
        }
    }

    /// Card renumbering must change the ALSA name while retaining the selected USB identity.
    #[test]
    fn a_card_that_comes_back_renumbered_is_found_again() {
        let sysfs = Arc::new(Hotplug::new(fixture("usb2-desk")));
        let resolver = CardResolver::new(
            PathBuf::from(DESK_CAPTURE_USB),
            Box::new(Arc::clone(&sysfs)),
        );
        let (mut opener, opened) = recording_opener(resolver);

        opener.open().expect("the card as recorded");
        sysfs.replace(Renumbered {
            inner: fixture("usb2-desk"),
            from: "card8",
            to: "card9",
        });
        opener.open().expect("the card after the replug");

        assert_eq!(
            *opened.lock().expect("not poisoned"),
            vec!["hw:8".to_string(), "hw:9".to_string()],
            "the ALSA name is derived from the card's sysfs name on every open"
        );
    }

    /// Audio retries must not enumerate or open video and serial nodes.
    #[test]
    fn resolving_the_card_opens_no_video_node_and_never_enumerates_one() {
        /// Listing either node class is how discovery starts; nothing else can reach a probe.
        struct NoNodeClasses<S: Sysfs>(S);
        impl<S: Sysfs> Sysfs for NoNodeClasses<S> {
            fn read_attr(&self, dir: &Path, name: &str) -> Option<String> {
                self.0.read_attr(dir, name)
            }
            fn read_link(&self, path: &Path) -> Option<PathBuf> {
                self.0.read_link(path)
            }
            fn list_dir(&self, dir: &Path) -> Vec<PathBuf> {
                assert!(
                    dir != Path::new("/class/video4linux") && dir != Path::new("/class/tty"),
                    "the card resolver enumerated {} — that is discovery, and discovery probes",
                    dir.display()
                );
                self.0.list_dir(dir)
            }
            fn exists(&self, path: &Path) -> bool {
                self.0.exists(path)
            }
        }

        let resolver = CardResolver::new(
            PathBuf::from(DESK_CAPTURE_USB),
            Box::new(NoNodeClasses(fixture("usb2-desk"))),
        );
        let resolved = resolver.resolve().expect("the dongle's own card");
        assert_eq!(resolved.device, "hw:8");
    }

    /// A capture device that is not there is `no card` and nothing worse: the audio thread's
    /// retry loop is the only thing that sees it, and the message says which device was looked
    /// for so the user can tell "unplugged" from "on another port".
    #[test]
    fn a_dongle_that_is_not_back_yet_is_a_no_card_condition_the_audio_loop_can_retry() {
        // The device directory with no `idVendor` is what an unplugged dongle leaves behind: a
        // path that resolves to nothing.
        let sysfs = OverrideSysfs::new(fixture("usb2-desk")).remove(DESK_CAPTURE_USB, "idVendor");
        let resolver = CardResolver::new(PathBuf::from(DESK_CAPTURE_USB), Box::new(sysfs));
        let err = resolver.resolve().expect_err("nothing to pair against");
        match &err {
            AudioError::NoCard(why) => assert!(why.contains("3-2.2.2"), "{why}"),
            other => panic!("expected NoCard, got {other:?}"),
        }
    }

    /// An absent sound card leaves video/serial pairing usable.
    #[test]
    fn a_paired_dongle_with_no_card_is_no_card_and_not_an_error() {
        let usb = "/devices/pci0000:00/0000:00:08.1/0000:0b:00.3/usb4/4-2/4-2.2";
        let resolver = CardResolver::new(PathBuf::from(usb), Box::new(fixture("usb3-stage0")));
        let err = resolver.resolve().expect_err("that tree has no sound card");
        match &err {
            AudioError::NoCard(why) => assert!(
                why.contains("no sound card belongs to the capture node"),
                "{why}"
            ),
            other => panic!("expected NoCard, got {other:?}"),
        }
    }

    /// An explicit `--video` steers audio as it steers everything else: the card is the
    /// card of whichever capture *device* the constraints selected, with no flag of its own. Here
    /// that is the second dongle, whose own card is `card10`.
    #[test]
    fn the_card_is_the_one_on_the_capture_device_it_was_given() {
        let usb = format!("{BUS}/3-2.3/3-2.3.2");
        let resolver = CardResolver::new(PathBuf::from(usb), Box::new(fixture("two-dongles")));
        let resolved = resolver.resolve().expect("the second dongle's card");
        assert_eq!(resolved.device, "hw:10");
        assert_eq!(resolved.card.name, "card10");

        // A card on another internal-hub port does not belong to the capture device.
        assert_ne!(resolved.card.name, "card9");
    }
}
