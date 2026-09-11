//! Rediscovery on reconnect: the two adapters the binary reopens its devices through
//! (§8, §2.7, §6.1 S2-4).
//!
//! [`super`] is a pure function over sysfs. This module is the one place that is not: it sits on
//! top of that function and hands the answer to the two subsystems that need a *fresh* device
//! after a failure — [`crate::input::spawn_with_source`]'s [`LinkSource`] and
//! [`crate::capture::Pipeline::start_with_opener`]'s [`SourceOpener`]. It lives under
//! `discovery/` because the policy it implements is §8's and nothing else's; nothing in
//! discovery's core depends on it.
//!
//! # The policy, which is §8's in two sentences
//!
//! **A node that was discovered is rediscovered on every attempt.** The kernel does not promise
//! a device the same `/dev` name across a replug — it promises the lowest free one — so a dongle
//! that comes back while something else holds `video4` comes back as `video6`, and a reopener
//! that only knew the old name would retry a name that is now somebody else's device, or nobody's,
//! for ever. Re-running [`discover`](super::discover) with the *original* constraints is what
//! makes "unplug it, plug it back in" work, and it is also what keeps the §8 evidence attached to
//! every reopen rather than only to the first one.
//!
//! **A node that was named explicitly is reopened by that name and nothing else.** §8: an explicit
//! `--video`/`--serial` "is honoured permanently" and is what makes the tool usable when discovery
//! is wrong. A reopener that quietly rediscovered would take that away at exactly the moment it
//! matters, and could move a live keyboard link onto a device the user did not ask for. So
//! [`Mode::Explicit`] never calls discovery at all — not to check the path, not to attach
//! evidence, not to log.
//!
//! The two modes are per *node*, because the constraints are: `--video` alone fixes the video
//! node and leaves the serial node to be re-inferred from it on every attempt, which is precisely
//! what §8's pairing rules are for.
//!
//! # Why the unsolicited receiver is merged here
//!
//! [`crate::serial::SerialLinkSource`] hands out the long-lived unsolicited receiver once and
//! opens one fixed path (its module docs explain why that receiver must outlive any one link).
//! When the path itself can change, "once" and "one fixed path" pull apart: a rediscovered
//! `/dev/ttyACM2` needs a new `SerialLinkSource`, whose receiver the A12 lock-state logger is not
//! holding. [`DiscoveringLinkSource`] therefore owns the channel the logger holds and pumps each
//! inner source's receiver into it, so the logger's receiver survives a *rename* exactly as
//! `SerialLinkSource`'s survives a *reopen*. One pump thread per inner source — one per path
//! change, which on this desk is one for the life of the process.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use super::probe::{NodeProbe, RealProbe};
use super::sysfs::{RealSysfs, Sysfs};
use super::{Constraints, DiscoveryError, Evidence, NodeKind};
use crate::capture::{CaptureError, FrameSource, SourceOpener, V4l2Opener};
use crate::link::{Link, LinkError, LinkSource, Reply};
use crate::serial::{OpenOptions, SerialLinkSource};

/// How one node's path is found again, per §8.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The path the user typed. Reopened exactly as typed, for ever; discovery is never consulted
    /// for it (§8: an explicit override is honoured permanently).
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
    /// The §8 evidence joining this node to the other half of the dongle, when discovery ran.
    /// `None` for an explicit path (nothing was consulted) and for an explicit pair.
    pub evidence: Option<Evidence>,
    /// Whether discovery was consulted at all. `false` is [`Mode::Explicit`].
    pub discovered: bool,
}

/// Resolves one node's path, again and again, under one fixed policy (§8).
///
/// Holds the [`Constraints`] the process started with rather than the path it ended up using:
/// re-running discovery with the *answer* rather than the *question* would let a wrong first
/// answer pin itself permanently.
pub struct NodeResolver {
    kind: NodeKind,
    constraints: Constraints,
    sysfs: Box<dyn Sysfs + Send>,
    probe: Box<dyn NodeProbe + Send>,
}

impl NodeResolver {
    /// A resolver for `kind` reading `sysfs` and probing with `probe`.
    ///
    /// The two are injected rather than constructed here so the policy can be tested against a
    /// recorded tree (materialised under `fixtures/sysfs/` from `fixtures/sysfs/usb2-desk.sysfs`
    /// by `synthesize.py`), which is the only way "a renamed node is found again"
    /// is testable at all without a replug.
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

    /// The path to open now.
    ///
    /// In [`Mode::Explicit`] this touches nothing: no sysfs read, no probe, no device opened.
    /// In [`Mode::Discover`] it re-runs the whole of §8 — which may fail, and a failure is the
    /// ordinary answer while a device is still coming back, not a defect.
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

/// One line per reopen saying which node was chosen and on what evidence (§2.6.1, §6.1: report
/// outcomes where they can be seen).
///
/// A *changed* node is logged at `warn`, because it is the one case where the client is now
/// driving a different device than it was a moment ago, and the user should be able to see that
/// in the log without having asked for `debug`.
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

/// A [`LinkSource`] that re-resolves the serial node before every open (§2.7 step 3a, §8).
///
/// Wraps [`SerialLinkSource`] rather than reimplementing it: everything about opening a CH9329 —
/// the port options, the reader thread, the per-link unsolicited forwarder — stays in `serial`,
/// and this type adds only "which path, this time". See the module docs for the pump.
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
    /// survives every reopen **and** every rename.
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
                // Ends when the inner source and all its links are dropped, or when the logger
                // drops its receiver. Both are ordinary (A12 evidence is optional).
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
        // worth giving up on (§2.7).
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
                "the serial node §8 discovery names, at {} baud (rediscovered on each attempt)",
                self.opts.baud
            ),
        }
    }
}

/// A [`SourceOpener`] that re-resolves the video node before every reopen (§6.1 S2-4, §8).
///
/// **The first source is handed in already open.** Startup stays fail-fast: `main` opens the
/// `V4l2Source` itself so that a wrong `--video`, or a dongle that is not plugged in, is a
/// reported failure with a usable message rather than a window showing nothing while a reopen
/// loop spins. Everything *after* startup is the opposite policy — §6.1 says a capture failure
/// must not tear down input — and that is what this opener provides from its second call on.
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
        // `Config` rather than `Disconnected`: "discovery cannot name one pair" is a condition the
        // user resolves with `--video`/`--serial`, and its `Display` is the table that tells them
        // how. The pipeline retries every open failure on its backoff regardless of the variant,
        // so nothing else turns on this choice.
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
                "the node §8 discovery names".to_string(),
                " (rediscovered on each reopen)",
            ),
        };
        format!(
            "V4L2 {node} MJPG {}x{} @{} fps{how}",
            self.width, self.height, self.fps
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::discovery::testing::{fixture, MapProbe, OverrideSysfs};

    /// Device directories inside the `two-dongles` recording, spelled the way an override must
    /// spell them: the **canonical** `/devices/...` path, which is where `usb_device_of` lands.
    const BUS: &str = "/devices/pci0000:00/0000:00:08.1/0000:0b:00.3/usb3/3-2";

    /// A [`Sysfs`] whose tree can be replaced between calls, so **one** resolver can be asked
    /// twice across a hotplug. The mutation itself is an [`OverrideSysfs`] over a real recorded
    /// tree, per §9.1: the fixture is the authority and nothing here retypes one.
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

    /// The `two-dongles` tree with the dongle *other* than `hub` relabelled, so exactly one pair
    /// remains. §8's evidence 3 requires all three vendor ids, so one wrong `idProduct` on a hub
    /// is enough to take that dongle out of the answer — and it is a one-attribute change over
    /// the real recording rather than a hand-built tree.
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
}
