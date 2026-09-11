//! Hardware discovery test. Needs the dongle (§9.3: feature-gated, `#[ignore]`d, never gating CI).
//!
//! ```text
//! cargo test --features hardware --test discovery_hardware -- --ignored --nocapture
//! ```
//!
//! This is the test the fixtures cannot be: it proves the recorded trees still describe the desk,
//! and that `RealSysfs` and `RealProbe` read the live kernel the same way `RealSysfs` reads a
//! snapshot. If it disagrees with `tests/discovery.rs`, the fixture is stale — re-record it
//! with `scripts/snapshot-sysfs.py --file fixtures/sysfs/usb2-desk.sysfs --force` (see
//! `fixtures/sysfs/MANIFEST.md`, which lists the `--exclude` globs that keep the reconstructed
//! bus-5 control out of the recording).
//!
//! It reads `/sys` and opens the *discovered* video node for one `VIDIOC_QUERYCAP`. It never
//! streams, never opens the serial node, and never opens a node discovery would not use — the
//! probe policy in `discovery::inventory` is what keeps `/dev/video0`–`3` (unrelated hardware,
//! CLAUDE.md) untouched, and the assertion below on which nodes exist is read out of `/sys`, not
//! by opening them.

#![cfg(feature = "hardware")]

use std::path::Path;

use nanokvm::discovery::{discover, inventory, Constraints, Evidence, RealProbe, RealSysfs};

/// The dongle's nodes on this desk (CLAUDE.md).
const VIDEO: &str = "/dev/video4";
const SERIAL: &str = "/dev/ttyACM1";

#[test]
#[ignore = "needs the NanoKVM-USB dongle"]
fn the_dongle_on_this_desk_discovers_itself() {
    let sysfs = RealSysfs::default();
    let probe = RealProbe;

    let inv = inventory(&sysfs, &probe);
    println!("{inv}");

    let pair = discover(&sysfs, &probe, &Constraints::default())
        .unwrap_or_else(|e| panic!("discovery failed on a desk with the dongle plugged in:\n{e}"));

    assert_eq!(pair.video.dev, Path::new(VIDEO));
    assert_eq!(pair.serial.dev, Path::new(SERIAL));
    let video_usb = pair
        .video
        .usb
        .as_ref()
        .expect("the video node resolves to a USB device");
    let serial_usb = pair
        .serial
        .usb
        .as_ref()
        .expect("the serial node resolves to a USB device");
    assert_eq!(video_usb.id(), nanokvm::discovery::VIDEO_ID);
    assert_eq!(serial_usb.id(), nanokvm::discovery::SERIAL_ID);

    // §12 "verify by consequence": the evidence is asserted, not just the outcome, because the
    // right answer reached by the wrong rule is the failure this whole module exists to avoid.
    // STAGE1_FINDINGS records this desk on a USB 2.0 link, which is §8's evidence 3.
    match &pair.evidence {
        Some(Evidence::InternalHub { hub }) => {
            println!("evidence: {}", pair.evidence.as_ref().unwrap());
            assert_eq!(hub.id(), nanokvm::discovery::INTERNAL_HUB_ID);
        }
        Some(Evidence::PortPeer { .. }) => panic!(
            "the dongle is on a SuperSpeed link again — that is good news, but this test and \
             fixtures/sysfs/usb2-desk both assume the USB 2.0 shape STAGE1_FINDINGS recorded. \
             Re-run scripts/snapshot-sysfs.py and update the expectation."
        ),
        other => panic!("expected InternalHub evidence, got {other:?}"),
    }

    // The metadata node must be present and rejected: if it were missing, the probe policy would
    // be untested here even though it passed.
    let video5 = inv
        .videos
        .iter()
        .find(|v| v.dev == Path::new("/dev/video5"))
        .expect("the dongle registers a UVC metadata node alongside the capture node");
    assert_eq!(
        video5.is_capture,
        Some(false),
        "QUERYCAP device_caps on /dev/video5 must report META_CAPTURE, not VIDEO_CAPTURE"
    );
    assert_eq!(
        video5.usb.as_ref().map(|u| &u.sysfs),
        Some(&video_usb.sysfs)
    );
}
