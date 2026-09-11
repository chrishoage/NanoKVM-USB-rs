//! Device pairing against recorded sysfs trees (plan §8, §9.1).
//!
//! Every test here runs the real [`nanokvm::discovery`] code over a real directory tree under
//! `fixtures/sysfs/`. The trees are not committed: `fixtures/sysfs/usb2-desk.sysfs` is, being
//! the recording `scripts/snapshot-sysfs.py` took of this desk serialised to one file, and
//! `fixtures/sysfs/synthesize.py` expands it and reconstructs the other three trees from it.
//! `nanokvm::discovery::testing::fixture` runs that script the first time a tree it was asked
//! for is missing, so `cargo test` needs python3 on PATH. §9.1's rule holds: the recording is
//! the authority. Nothing
//! below retypes a topology into a hand-built fake, and the two mutation tests express "the same
//! desk, with one attribute changed" as an override on the recording rather than as a second,
//! hand-written tree that could differ from it in ways nobody notices.
//!
//! No hardware. The one test that needs the dongle is `tests/discovery_hardware.rs`.

use std::path::{Path, PathBuf};

use proptest::prelude::*;

use nanokvm::discovery::testing::{fixture, MapProbe, OverrideSysfs};
use nanokvm::discovery::{
    discover, inventory, Constraints, DiscoveryError, Evidence, Pair, Sysfs, UsbDevice,
};

/// Device directories inside the `usb2-desk` recording, for the mutation tests.
const DESK: &str = "/devices/pci0000:00/0000:00:08.1/0000:0b:00.3/usb3/3-2/3-2.2";
const DESK_HUB: &str = DESK;
const DESK_VIDEO: &str = "/devices/pci0000:00/0000:00:08.1/0000:0b:00.3/usb3/3-2/3-2.2/3-2.2.2";
const DESK_SERIAL: &str = "/devices/pci0000:00/0000:00:08.1/0000:0b:00.3/usb3/3-2/3-2.2/3-2.2.4";

/// The bus-5 dock in the same recording: a generic hub with a webcam and an unrelated CDC-ACM
/// device as its direct children. The negative control, and the shape §8's loose wording pairs.
const DOCK_HUB: &str =
    "/devices/pci0000:00/0000:00:08.1/0000:0b:00.4/usb5/5-1/5-1.4/5-1.4.4/5-1.4.4.4";
const DOCK_VIDEO: &str =
    "/devices/pci0000:00/0000:00:08.1/0000:0b:00.4/usb5/5-1/5-1.4/5-1.4.4/5-1.4.4.4/5-1.4.4.4.2";
const DOCK_SERIAL: &str =
    "/devices/pci0000:00/0000:00:08.1/0000:0b:00.4/usb5/5-1/5-1.4/5-1.4.4/5-1.4.4.4/5-1.4.4.4.3";

/// The probe answers measured on this desk with `v4l2-ctl --info` (see
/// `src/discovery/probe.rs`).
///
/// The user's webcam nodes default to `true`. They were never opened — `/dev/video0`–`3` are
/// off limits — so `true` is the *conservative* stand-in: it makes every one of them an
/// eligible capture node, so a pairing rule that were too loose would pair one of them and the
/// negative-control test below would catch it. Which nodes discovery actually opens is pinned
/// separately, by [`a_node_no_rule_could_choose_is_never_opened`].
fn desk_probe() -> MapProbe {
    MapProbe::new()
        .capture("/dev/video4", true)
        .capture("/dev/video5", false)
        .default_capture(true)
}

/// `two-dongles` adds a second identical unit, so the same two answers repeat.
fn two_dongle_probe() -> MapProbe {
    desk_probe()
        .capture("/dev/video6", true)
        .capture("/dev/video7", false)
}

fn only_pair(inv: &nanokvm::discovery::Inventory) -> &Pair {
    assert_eq!(inv.pairs.len(), 1, "expected exactly one pair in:\n{inv}");
    &inv.pairs[0]
}

/// The USB device behind a node the class walk enumerated. `None` is reserved for a node named
/// by `--video`/`--serial` that discovery never saw, which an inventory node never is — so this
/// asserts rather than matches.
fn usb(usb: &Option<UsbDevice>) -> &UsbDevice {
    usb.as_ref()
        .expect("a node discovery enumerated always resolves to a USB device")
}

// ---- usb2-desk: the USB 2.0 fallback shape (§8 evidence 3) --------------------------------

#[test]
fn the_desk_pairs_video4_with_ttyacm1_through_the_dongles_own_hub() {
    let sysfs = fixture("usb2-desk");
    let inv = inventory(&sysfs, &desk_probe());
    let pair = only_pair(&inv);

    assert_eq!(pair.video.dev, Path::new("/dev/video4"));
    assert_eq!(pair.serial.dev, Path::new("/dev/ttyACM1"));
    match &pair.evidence {
        Some(Evidence::InternalHub { hub }) => {
            assert_eq!(hub.name, "3-2.2");
            assert_eq!(hub.id(), (0x1a40, 0x0101));
        }
        other => panic!("expected InternalHub evidence on a USB 2.0 link, got {other:?}"),
    }
    assert_eq!(usb(&pair.video.usb).name, "3-2.2.2");
    assert_eq!(usb(&pair.serial.usb).name, "3-2.2.4");
}

#[test]
fn the_metadata_node_is_listed_but_never_paired() {
    let sysfs = fixture("usb2-desk");
    let inv = inventory(&sysfs, &desk_probe());

    let video5 = inv
        .videos
        .iter()
        .find(|v| v.dev == Path::new("/dev/video5"))
        .expect("/dev/video5 must appear in the listing");
    assert_eq!(video5.is_capture, Some(false));
    assert_eq!(
        usb(&video5.usb).name,
        "3-2.2.2",
        "video5 is the same USB device as video4; only QUERYCAP separates them"
    );
    assert!(
        !inv.pairs
            .iter()
            .any(|p| p.video.dev == Path::new("/dev/video5")),
        "the UVC metadata node must never be selected as the capture node:\n{inv}"
    );
}

#[test]
fn an_unrelated_webcam_and_serial_port_behind_one_generic_hub_do_not_pair() {
    let sysfs = fixture("usb2-desk");
    let inv = inventory(&sysfs, &desk_probe());

    let webcams: Vec<_> = inv
        .videos
        .iter()
        .filter(|v| usb(&v.usb).name == "5-1.4.4.4.2")
        .collect();
    assert_eq!(
        webcams.len(),
        4,
        "the webcam's four nodes must be listed:\n{inv}"
    );
    let acm0 = inv
        .serials
        .iter()
        .find(|s| s.dev == Path::new("/dev/ttyACM0"))
        .expect("/dev/ttyACM0 must be listed");
    assert_eq!(usb(&acm0.usb).name, "5-1.4.4.4.3");

    assert!(
        !inv.pairs
            .iter()
            .any(|p| p.serial.dev == acm0.dev || usb(&p.video.usb).name == "5-1.4.4.4.2"),
        "the webcam (046d:086b) and ttyACM0 (043e:9a8a) are direct children of one hub, exactly \
         the shape §8's looser wording would have paired. Evidence 3 rejects them on their own \
         ids — 046d:086b is not 345f:2133 — which it checks before it ever reads the hub. The \
         hub-id half of the rule has its own negative control, \
         a_generic_hub_with_the_dongles_two_ids_under_it_still_does_not_pair:\n{inv}"
    );
}

/// The containment half of the rule, pinned on its own.
///
/// The recording above is rejected on the video and serial ids, so it says nothing about whether
/// the hub is checked at all. This is the same recording with the dock's two devices relabelled
/// as the dongle's — leaving `5-1.4.4.4` `0bda:5411` as the **only** thing that can still reject
/// the pair. It is "a capture stick and a serial adapter in a cheap hub", which must not be
/// mistaken for a NanoKVM.
#[test]
fn a_generic_hub_with_the_dongles_two_ids_under_it_still_does_not_pair() {
    let sysfs = OverrideSysfs::new(fixture("usb2-desk"))
        // Isolate the case: the real dongle's hub is relabelled so it drops out of the answer.
        .set(DESK_HUB, "idProduct", "0102")
        .set(DOCK_VIDEO, "idVendor", "345f")
        .set(DOCK_VIDEO, "idProduct", "2133")
        .set(DOCK_SERIAL, "idVendor", "1a86")
        .set(DOCK_SERIAL, "idProduct", "55d3");

    let err = discover(&sysfs, &desk_probe(), &Constraints::default())
        .expect_err("the dock hub 0bda:5411 is not the dongle's 1a40:0101");
    assert!(
        matches!(err, DiscoveryError::NoPair(_)),
        "expected NoPair, got {err}"
    );
}

#[test]
fn a_node_no_rule_could_choose_is_never_opened() {
    let sysfs = fixture("usb2-desk");
    // No default: being asked about any other node panics with the node's name.
    let probe = MapProbe::new()
        .capture("/dev/video4", true)
        .capture("/dev/video5", false);
    let _ = inventory(&sysfs, &probe);

    assert_eq!(
        probe.asked(),
        vec![PathBuf::from("/dev/video4"), PathBuf::from("/dev/video5")],
        "the probe opens a device node; only nodes on a USB device with a topological pairing \
         candidate may be opened"
    );
}

#[test]
fn a_probe_failure_keeps_the_node_usable_and_reports_why() {
    let sysfs = fixture("usb2-desk");
    let probe = desk_probe().failing("/dev/video4", libc::EACCES);
    let inv = inventory(&sysfs, &probe);

    let pair = only_pair(&inv);
    assert_eq!(
        pair.video.dev,
        Path::new("/dev/video4"),
        "an EACCES on the only plausible node must not delete it from discovery:\n{inv}"
    );
    let video4 = &pair.video;
    assert_eq!(video4.is_capture, None);
    assert!(video4.probe_error.is_some());
    let listing = inv.to_string();
    assert!(
        listing.contains("probe failed"),
        "the listing must say why the node could not be probed:\n{listing}"
    );
}

#[test]
fn a_definite_capture_sibling_wins_over_an_unprobeable_one() {
    let sysfs = fixture("usb2-desk");
    // The opposite of the desk's real answers: video4 cannot be opened, video5 says it captures.
    let probe = MapProbe::new()
        .failing("/dev/video4", libc::EBUSY)
        .capture("/dev/video5", true);
    let inv = inventory(&sysfs, &probe);

    let pair = only_pair(&inv);
    assert_eq!(
        pair.video.dev,
        Path::new("/dev/video5"),
        "a sibling the probe confirmed must beat one it could not answer for:\n{inv}"
    );
}

// ---- usb3-stage0: the SuperSpeed shape (§8 evidence 2) -------------------------------------

#[test]
fn the_superspeed_shape_pairs_across_two_buses_by_the_kernels_port_peer() {
    let sysfs = fixture("usb3-stage0");
    let inv = inventory(&sysfs, &desk_probe());
    let pair = only_pair(&inv);

    assert_eq!(pair.video.dev, Path::new("/dev/video4"));
    assert_eq!(pair.serial.dev, Path::new("/dev/ttyACM1"));
    assert_eq!(
        pair.evidence,
        Some(Evidence::PortPeer {
            video_port: "4-2-port2".to_string(),
            peer_port: "3-2-port2".to_string(),
        }),
        "the video device is on bus 4 and the serial device on bus 3; only the peer link joins \
         them:\n{inv}"
    );
    assert_eq!(usb(&pair.video.usb).busnum, 4);
    assert_eq!(usb(&pair.serial.usb).busnum, 3);
}

// ---- two-dongles: ambiguity, and constraints resolving it (§8 policy) ----------------------

#[test]
fn two_dongles_refuse_to_be_guessed_between() {
    let sysfs = fixture("two-dongles");
    let err = discover(&sysfs, &two_dongle_probe(), &Constraints::default())
        .expect_err("two dongles must never silently resolve to one");

    let DiscoveryError::Ambiguous(inv) = &err else {
        panic!("expected Ambiguous, got {err}");
    };
    assert_eq!(inv.pairs.len(), 2, "{inv}");
    let text = err.to_string();
    for node in ["/dev/video4", "/dev/ttyACM1", "/dev/video6", "/dev/ttyACM2"] {
        assert!(text.contains(node), "{node} missing from:\n{text}");
    }
}

#[test]
fn a_serial_constraint_picks_the_dongle_it_belongs_to() {
    let sysfs = fixture("two-dongles");
    let constraints = Constraints {
        serial: Some(PathBuf::from("/dev/ttyACM1")),
        ..Constraints::default()
    };
    let pair = discover(&sysfs, &two_dongle_probe(), &constraints).expect("one pair remains");
    assert_eq!(pair.video.dev, Path::new("/dev/video4"));
    assert_eq!(pair.serial.dev, Path::new("/dev/ttyACM1"));
}

#[test]
fn a_video_constraint_picks_the_dongle_it_belongs_to() {
    let sysfs = fixture("two-dongles");
    let constraints = Constraints {
        video: Some(PathBuf::from("/dev/video6")),
        ..Constraints::default()
    };
    let pair = discover(&sysfs, &two_dongle_probe(), &constraints).expect("one pair remains");
    assert_eq!(pair.video.dev, Path::new("/dev/video6"));
    assert_eq!(
        pair.serial.dev,
        Path::new("/dev/ttyACM2"),
        "the second dongle's serial node, not the first's"
    );
}

// ---- the narrowed evidence-3 rule (module docs, correcting §8's wording) -------------------

#[test]
fn a_hub_that_is_not_the_dongles_own_defeats_containment() {
    let sysfs = OverrideSysfs::new(fixture("usb2-desk")).set(DESK_HUB, "idProduct", "0102");
    let err = discover(&sysfs, &desk_probe(), &Constraints::default())
        .expect_err("containment must not accept an unknown hub");
    assert!(
        matches!(err, DiscoveryError::NoPair(_)),
        "expected NoPair, got {err}"
    );
}

#[test]
fn a_serial_device_that_is_not_the_ch9329_defeats_containment() {
    let sysfs = OverrideSysfs::new(fixture("usb2-desk")).set(DESK_SERIAL, "idVendor", "0403");
    let err = discover(&sysfs, &desk_probe(), &Constraints::default())
        .expect_err("containment must not accept an unknown serial bridge");
    assert!(
        matches!(err, DiscoveryError::NoPair(_)),
        "expected NoPair, got {err}"
    );
}

/// Each of the three ids the containment rule checks, wrong on its own.
///
/// The proptest below states the property; this states the *coverage*, deterministically.
/// Deleting any one of the three id checks from `internal_hub` fails exactly one of these three
/// cases, which is the whole claim the narrowed rule rests on. Randomised generation cannot make
/// that claim on its own: a `u16` that has to miss one value while two others hit theirs is a
/// coincidence a property test will not supply.
#[test]
fn every_one_of_the_three_ids_is_required_for_containment() {
    for (what, dir, wrong) in [
        ("the internal hub", DESK_HUB, "0102"),
        ("the video device", DESK_VIDEO, "2134"),
        ("the serial bridge", DESK_SERIAL, "55d4"),
    ] {
        let sysfs = OverrideSysfs::new(fixture("usb2-desk")).set(dir, "idProduct", wrong);
        match discover(&sysfs, &desk_probe(), &Constraints::default()) {
            Err(DiscoveryError::NoPair(_)) => {}
            Err(e) => panic!("expected NoPair when {what} is not the dongle's, got:\n{e}"),
            Ok(pair) => panic!(
                "{what} was relabelled {wrong} and containment paired anyway:\n  {pair}\n\
                 all three ids are commodity part numbers; dropping any one of the checks makes \
                 the rule pair unrelated hardware"
            ),
        }
    }
}

/// `idProduct` values for the property below: the real one often, near misses and arbitrary
/// values the rest of the time.
///
/// Drawing all three from `any::<u16>()` — which is what this test used to do — makes the hub-id
/// check unreachable: a case only gets past `video.id() != VIDEO_ID || serial.id() != SERIAL_ID`
/// when two independent `u16` draws both land on their exact value, about 2⁻³². The property was
/// then vacuous for two of the three ids, and deleting the hub check left it green.
fn an_id(real: u16) -> impl Strategy<Value = u16> {
    prop_oneof![
        6 => Just(real),
        1 => Just(real ^ 1),
        1 => Just(0x0000),
        1 => Just(0xffff),
        1 => any::<u16>(),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// The whole justification for narrowing §8's evidence 3 is that the hub id alone is a
    /// generic part number. Nothing may reach `InternalHub` unless all three ids are the
    /// dongle's.
    #[test]
    fn internal_hub_evidence_never_appears_unless_all_three_ids_match(
        hub in an_id(0x0101), video in an_id(0x2133), serial in an_id(0x55d3),
    ) {
        let sysfs = OverrideSysfs::new(fixture("usb2-desk"))
            .set(DESK_HUB, "idProduct", &format!("{hub:04x}"))
            .set(DESK_VIDEO, "idProduct", &format!("{video:04x}"))
            .set(DESK_SERIAL, "idProduct", &format!("{serial:04x}"));
        let inv = inventory(&sysfs, &desk_probe());
        for pair in &inv.pairs {
            if matches!(pair.evidence, Some(Evidence::InternalHub { .. })) {
                prop_assert_eq!((hub, video, serial), (0x0101, 0x2133, 0x55d3));
            }
        }
    }
}

/// The property above is only worth its runtime if its cases reach all three checks.
///
/// `internal_hub` returns early on the video and serial ids, so a case exercises the hub-id check
/// only when those two are right and the hub is wrong. This samples the same strategy the same
/// number of times, deterministically, and asserts that each of the three checks is the one
/// deciding at least one case.
#[test]
fn the_containment_property_reaches_all_three_id_checks() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;

    let mut runner = TestRunner::deterministic();
    let strategy = (an_id(0x0101), an_id(0x2133), an_id(0x55d3));
    let (mut hub_check, mut video_check, mut serial_check) = (0, 0, 0);
    for _ in 0..64 {
        let (hub, video, serial) = strategy.new_tree(&mut runner).unwrap().current();
        match (hub == 0x0101, video == 0x2133, serial == 0x55d3) {
            (false, true, true) => hub_check += 1,
            (_, false, _) => video_check += 1,
            (_, _, false) => serial_check += 1,
            _ => {}
        }
    }
    assert!(
        hub_check > 0 && video_check > 0 && serial_check > 0,
        "of 64 generated cases, {hub_check} could only be rejected by the hub id, {video_check} \
         by the video id and {serial_check} by the serial id; a zero means the property test \
         cannot see that check at all"
    );
}

// ---- constraints (§8: honoured permanently, and never silently ignored) --------------------

#[test]
fn a_constraint_naming_a_node_that_is_not_there_says_what_is() {
    let sysfs = fixture("usb2-desk");
    let constraints = Constraints {
        video: Some(PathBuf::from("/dev/video9")),
        ..Constraints::default()
    };
    let err = discover(&sysfs, &desk_probe(), &constraints).expect_err("no such node");
    let DiscoveryError::ConstraintNotFound {
        path, available, ..
    } = &err
    else {
        panic!("expected ConstraintNotFound, got {err}");
    };
    assert_eq!(path, Path::new("/dev/video9"));
    assert!(available.contains(&PathBuf::from("/dev/video4")), "{err}");
    assert!(err.to_string().contains("/dev/video4"), "{err}");
}

#[test]
fn both_constraints_are_obeyed_even_with_no_evidence_at_all() {
    let sysfs = fixture("usb2-desk");
    let constraints = Constraints {
        video: Some(PathBuf::from("/dev/video4")),
        serial: Some(PathBuf::from("/dev/ttyACM0")),
    };
    let pair = discover(&sysfs, &desk_probe(), &constraints)
        .expect("§8: an explicit override is what makes the tool usable when discovery is wrong");
    assert_eq!(pair.video.dev, Path::new("/dev/video4"));
    assert_eq!(pair.serial.dev, Path::new("/dev/ttyACM0"));
    assert_eq!(
        pair.evidence, None,
        "no rule joins the dongle's video node to the monitor's serial port; the caller warns \
         rather than refusing"
    );
}

#[test]
fn a_lone_constraint_that_pairs_with_nothing_is_an_error_naming_the_candidates() {
    let sysfs = fixture("usb2-desk");
    let constraints = Constraints {
        serial: Some(PathBuf::from("/dev/ttyACM0")),
        ..Constraints::default()
    };
    let err = discover(&sysfs, &desk_probe(), &constraints)
        .expect_err("the video half cannot be inferred from an unpaired serial node");
    let DiscoveryError::ConstraintUnpaired { path, .. } = &err else {
        panic!("expected ConstraintUnpaired, got {err}");
    };
    assert_eq!(path, Path::new("/dev/ttyACM0"));
    let text = err.to_string();
    assert!(text.contains("--video"), "must say what to pass:\n{text}");
    assert!(
        text.contains("/dev/video4"),
        "must list the candidates:\n{text}"
    );
}

// ---- the message the user actually reads (§8: fail with the candidates listed) -------------

#[test]
fn a_no_pair_message_names_every_node_and_every_vid_pid() {
    let sysfs = OverrideSysfs::new(fixture("usb2-desk")).set(DESK_HUB, "idProduct", "0102");
    let err = discover(&sysfs, &desk_probe(), &Constraints::default()).expect_err("no pair");
    let text = err.to_string();

    for expected in [
        "/dev/video4",
        "/dev/video5",
        "/dev/video0",
        "/dev/ttyACM0",
        "/dev/ttyACM1",
        "345f:2133",
        "1a86:55d3",
        "046d:086b",
        "043e:9a8a",
        "--video",
        "--serial",
    ] {
        assert!(
            text.contains(expected),
            "{expected:?} missing from:\n{text}"
        );
    }
}

#[test]
fn an_ambiguous_message_names_both_pairs_and_their_evidence() {
    let sysfs = fixture("two-dongles");
    let err =
        discover(&sysfs, &two_dongle_probe(), &Constraints::default()).expect_err("ambiguous");
    let text = err.to_string();

    assert!(text.contains("3-2.2 1a40:0101"), "{text}");
    assert!(text.contains("3-2.3 1a40:0101"), "{text}");
    assert_eq!(
        text.matches("internal hub").count(),
        2,
        "both pairs must show their evidence:\n{text}"
    );
}

// ---- overrides discovery may not veto (§8, review findings 1 and 2) ------------------------

/// A [`Sysfs`] that hides one class directory: `/sys` unreadable in a container, a class the walk
/// does not know about, a driver that registers the node somewhere else. These are the cases §8's
/// permanent override exists for.
struct HideClass<S: Sysfs> {
    inner: S,
    class: &'static str,
}

impl<S: Sysfs> Sysfs for HideClass<S> {
    fn read_attr(&self, dir: &Path, name: &str) -> Option<String> {
        self.inner.read_attr(dir, name)
    }
    fn read_link(&self, path: &Path) -> Option<PathBuf> {
        self.inner.read_link(path)
    }
    fn list_dir(&self, dir: &Path) -> Vec<PathBuf> {
        if dir == Path::new(self.class) {
            return Vec::new();
        }
        self.inner.list_dir(dir)
    }
    fn exists(&self, path: &Path) -> bool {
        self.inner.exists(path)
    }
}

/// The CH9329 binds to `cdc_acm` on this desk, but the same silicon binds to `ch341`/`ch343` on
/// other kernels and appears as `/dev/ttyUSB0`. `TTY_PREFIX` is `ttyACM`, so such a node is never
/// in the inventory — and §8 says naming it explicitly, with an explicit `--video`, is obeyed
/// anyway. Discovery's opinion of a path the user typed is not a veto.
#[test]
fn a_serial_path_discovery_never_enumerated_is_still_obeyed() {
    let sysfs = fixture("usb2-desk");
    let constraints = Constraints {
        video: Some(PathBuf::from("/dev/video4")),
        serial: Some(PathBuf::from("/dev/ttyUSB0")),
    };
    let pair = discover(&sysfs, &desk_probe(), &constraints).unwrap_or_else(|e| {
        panic!(
            "§8: an explicit override is honoured permanently and is what makes the tool usable \
             when discovery is wrong. Both halves were given and discovery refused them:\n{e}"
        )
    });
    assert_eq!(pair.video.dev, Path::new("/dev/video4"));
    assert_eq!(pair.serial.dev, Path::new("/dev/ttyUSB0"));
    assert!(
        pair.serial.usb.is_none(),
        "discovery did not enumerate this node, and must say so rather than invent a device"
    );
    assert_eq!(
        pair.evidence, None,
        "no evidence can join a node discovery cannot resolve; the caller warns"
    );
}

/// The same hole through the other door. `/dev/serial/by-id/...` is the path a user reaches for
/// precisely because it survives replug, and it is not what the class walk produces.
#[test]
fn a_udev_stable_serial_path_is_still_obeyed() {
    let sysfs = fixture("usb2-desk");
    let by_id = "/dev/serial/by-id/usb-1a86_USB_Single_Serial_5C37176280-if00";
    let constraints = Constraints {
        video: Some(PathBuf::from("/dev/video4")),
        serial: Some(PathBuf::from(by_id)),
    };
    let pair = discover(&sysfs, &desk_probe(), &constraints)
        .unwrap_or_else(|e| panic!("both overrides given, both refused:\n{e}"));
    assert_eq!(pair.serial.dev, Path::new(by_id));
}

/// Discovery finding nothing at all is exactly when the override matters most. `NoVideoNodes` and
/// `NoSerialNodes` used to be returned before the constraints were read, so a user who supplied
/// both paths was told the dongle was not plugged in and the process exited 1.
#[test]
fn both_overrides_survive_a_class_walk_that_finds_nothing() {
    let sysfs = HideClass {
        inner: fixture("usb2-desk"),
        class: "/class/tty",
    };
    let constraints = Constraints {
        video: Some(PathBuf::from("/dev/video4")),
        serial: Some(PathBuf::from("/dev/ttyACM1")),
    };
    let pair = discover(&sysfs, &desk_probe(), &constraints).unwrap_or_else(|e| {
        panic!("both halves were named explicitly; discovery must not get a veto over them:\n{e}")
    });
    assert_eq!(pair.video.dev, Path::new("/dev/video4"));
    assert_eq!(pair.serial.dev, Path::new("/dev/ttyACM1"));
}

/// And with no readable `/sys` at all, which is what a container without `/sys` mounted looks
/// like: still not an error, because nothing about the answer depends on discovery.
#[test]
fn both_overrides_survive_an_unreadable_sysfs() {
    let sysfs = nanokvm::discovery::RealSysfs::new("/nonexistent-sysfs-root");
    let constraints = Constraints {
        video: Some(PathBuf::from("/dev/video4")),
        serial: Some(PathBuf::from("/dev/ttyACM1")),
    };
    let pair = discover(&sysfs, &desk_probe(), &constraints)
        .expect("discovery must not fail a selection it was not asked to make");
    assert_eq!(pair.video.dev, Path::new("/dev/video4"));
    assert_eq!(pair.serial.dev, Path::new("/dev/ttyACM1"));
}

/// An explicit pair that *is* the dongle still gets its evidence attached, so the caller can log
/// which rule agreed. This is the only reason discovery runs at all in the both-given case.
#[test]
fn both_overrides_still_carry_whatever_evidence_exists() {
    let sysfs = fixture("usb2-desk");
    let constraints = Constraints {
        video: Some(PathBuf::from("/dev/video4")),
        serial: Some(PathBuf::from("/dev/ttyACM1")),
    };
    let pair = discover(&sysfs, &desk_probe(), &constraints).expect("both given");
    assert!(matches!(pair.evidence, Some(Evidence::InternalHub { .. })));
}

// ---- how confidently the answer is reported (§8, review finding 3) -------------------------

/// `1a40:0101` is Terminus, `345f:2133` is a MacroSilicon capture part sold in dozens of
/// unbranded sticks, and `1a86:55d3` is a QinHeng serial part sold in dozens of unbranded
/// adapters. Nothing stops a user owning one of each and one cheap hub, and the rule accepts that
/// arrangement — this desk needs it to, because it is the only rule that works on a USB 2.0 port.
/// What must not happen is calling it proof: that is what a user reads before this program opens
/// the port and starts writing CH9329 frames into it.
///
/// The fixture is `usb2-desk` with the dock's three devices relabelled with those commodity ids
/// and the real dongle's hub relabelled so it drops out — nothing else changed.
#[test]
fn containment_evidence_is_never_reported_as_proof() {
    let sysfs = OverrideSysfs::new(fixture("usb2-desk"))
        .set(DESK_HUB, "idProduct", "0102")
        .set(DOCK_HUB, "idVendor", "1a40")
        .set(DOCK_HUB, "idProduct", "0101")
        .set(DOCK_VIDEO, "idVendor", "345f")
        .set(DOCK_VIDEO, "idProduct", "2133")
        .set(DOCK_SERIAL, "idVendor", "1a86")
        .set(DOCK_SERIAL, "idProduct", "55d3");

    // One capture node and three metadata/subdevice nodes: the ordinary shape for a UVC camera.
    let probe = MapProbe::new()
        .capture("/dev/video0", true)
        .default_capture(false);
    let pair = discover(&sysfs, &probe, &Constraints::default())
        .expect("the three-id rule accepts this arrangement, and the desk depends on that");
    assert_eq!(pair.video.dev, Path::new("/dev/video0"));
    assert_eq!(pair.serial.dev, Path::new("/dev/ttyACM0"));

    let evidence = pair.evidence.as_ref().expect("containment evidence");
    let message = evidence.to_string();
    assert!(
        !message.contains("proof"),
        "evidence 3 is a containment heuristic over three commodity vendor ids. §8 ranks items 1 \
         and 2 as proof and item 3 as a degraded common-ancestor test. The user is told:\n  \
         {message}"
    );
    assert!(message.contains("degraded"), "{message}");
    assert!(!evidence.is_proof(), "{message}");
    assert!(
        Evidence::SameDevice.is_proof()
            && Evidence::PortPeer {
                video_port: "4-2-port2".to_string(),
                peer_port: "3-2-port2".to_string(),
            }
            .is_proof(),
        "§8's items 1 and 2 are the kernel's own assertions and stay proof"
    );
}

// ---- the root-hub port shape (§8 evidence 2, no external hub) -------------------------------

/// The dongle plugged straight into a motherboard USB 3 port — the arrangement most users have.
/// The port directory is spelled `usb4/4-0:1.0/usb4-port2`, a different branch of `port_dir_of`
/// from the `<hub>/<hub>:1.0/<hub>-portN` form every other fixture uses. Until this tree existed
/// that branch was covered only by a string-level unit test.
#[test]
fn a_dongle_on_a_root_port_pairs_through_the_root_hubs_peer_link() {
    let sysfs = fixture("usb3-rootport");
    let inv = inventory(&sysfs, &desk_probe());
    let pair = only_pair(&inv);

    assert_eq!(pair.video.dev, Path::new("/dev/video4"));
    assert_eq!(pair.serial.dev, Path::new("/dev/ttyACM1"));
    assert_eq!(
        pair.evidence,
        Some(Evidence::PortPeer {
            video_port: "usb4-port2".to_string(),
            peer_port: "usb3-port2".to_string(),
        }),
        "the video device is a direct child of the SuperSpeed root hub, so its port directory is \
         the root-hub form:\n{inv}"
    );
    assert_eq!(usb(&pair.video.usb).name, "4-2");
    assert_eq!(usb(&pair.serial.usb).name, "3-2.4");
}

// ---- permission denied is not ambiguity -----------------------------------------------------

/// A user not in the `video` group gets `EACCES` from every probe, so nothing can tell the
/// capture node from its metadata sibling and both stay eligible — two pairs where there is one
/// dongle. "More than one candidate pair" sends that user hunting for hardware they do not have.
#[test]
fn permission_denied_on_every_probe_says_so_rather_than_ambiguous() {
    let sysfs = fixture("usb2-desk");
    let probe = MapProbe::new()
        .failing("/dev/video4", libc::EACCES)
        .failing("/dev/video5", libc::EACCES)
        .default_capture(true);
    let err = discover(&sysfs, &probe, &Constraints::default())
        .expect_err("nothing can be selected without a probe answer");

    assert!(
        matches!(err, DiscoveryError::ProbesDenied(_)),
        "expected ProbesDenied, got {err}"
    );
    let text = err.to_string();
    assert!(
        text.contains("Permission denied") || text.contains("permission denied"),
        "{text}"
    );
    assert!(
        text.contains("video"),
        "must name the group to join:\n{text}"
    );
}

/// The other side of it: one `EBUSY` among the failures is not a permissions story, and claiming
/// it is would send the user to `usermod` for a node another process is streaming.
#[test]
fn a_mixed_probe_failure_is_still_reported_as_ambiguous() {
    let sysfs = fixture("usb2-desk");
    let probe = MapProbe::new()
        .failing("/dev/video4", libc::EACCES)
        .failing("/dev/video5", libc::EBUSY)
        .default_capture(true);
    let err = discover(&sysfs, &probe, &Constraints::default()).expect_err("two eligible nodes");
    assert!(
        matches!(err, DiscoveryError::Ambiguous(_)),
        "expected Ambiguous, got {err}"
    );
}

// ---- `devices` honours the flags on its own command line (review finding 7) -----------------

/// `devices --video X --serial Y` (Stage 1's `--list-devices`) used to print the unfiltered
/// table and exit 0, so the flags looked ignored. The listing marks what they name and spells
/// out the selection.
#[test]
fn a_listing_marks_the_nodes_the_flags_name() {
    let sysfs = fixture("usb2-desk");
    let inv = inventory(&sysfs, &desk_probe());
    let constraints = Constraints {
        video: Some(PathBuf::from("/dev/video4")),
        serial: Some(PathBuf::from("/dev/ttyACM1")),
    };
    let text = inv.listing(&constraints).to_string();

    assert!(text.contains("-> /dev/video4"), "{text}");
    assert!(text.contains("-> /dev/ttyACM1"), "{text}");
    assert!(!text.contains("-> /dev/video5"), "{text}");
    assert!(!text.contains("-> /dev/ttyACM0"), "{text}");
    assert!(
        text.contains("selection: /dev/video4 + /dev/ttyACM1"),
        "the listing must say what the flags select:\n{text}"
    );
    assert_eq!(
        inv.to_string(),
        inv.listing(&Constraints::default()).to_string(),
        "with no flags the listing is the plain table Stage 1 printed"
    );
    assert!(
        !inv.to_string().contains("->"),
        "nothing is marked when nothing was asked for:\n{inv}"
    );
}

/// A flag naming a node discovery never enumerated is the case §8's override exists for, so the
/// listing must say it will still be used rather than leave the user thinking it was dropped.
#[test]
fn a_listing_calls_out_a_flag_it_could_not_find() {
    let sysfs = fixture("usb2-desk");
    let inv = inventory(&sysfs, &desk_probe());
    let constraints = Constraints {
        serial: Some(PathBuf::from("/dev/ttyUSB0")),
        ..Constraints::default()
    };
    let text = inv.listing(&constraints).to_string();
    assert!(text.contains("/dev/ttyUSB0"), "{text}");
    assert!(text.contains("still be used"), "{text}");
}
