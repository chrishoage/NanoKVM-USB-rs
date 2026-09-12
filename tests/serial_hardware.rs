//! Serial input verification against bridge 1a86:55d3 and the connected target.
//!
//! Verify that `/dev/ttyACM1` still names the intended bridge before running. Tests
//! check returned CapsLock changes and attempt release on exit. Run one binary at
//! a time with `--test-threads=1`; leave the user's bus-5 hardware untouched.

#![cfg(feature = "hardware")]

use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::time::{Duration, Instant};

use nanokvm::link::Link;
use nanokvm::proto::cmd;
use nanokvm::proto::frame::DeviceInfo;
use nanokvm::proto::report::{KeyboardReport, MOUSE_RELEASE_ALL};
use nanokvm::serial::SerialLink;
use serde::Deserialize;

/// The dongle's serial node on the recorded test setup (CLAUDE.md). `/dev/ttyACM0` is unrelated hardware and is
/// never opened.
const PORT: &str = "/dev/ttyACM1";
const TIMEOUT: Duration = Duration::from_millis(500);
/// Allow 500 ms for a lock-state push; recorded responses arrived around 15 ms.
const FLIP_WINDOW: Duration = Duration::from_millis(500);

// -- fixtures ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Fixtures {
    #[serde(default)]
    packet: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
struct Entry {
    name: String,
    data: Vec<u8>,
}

/// A request payload from `fixtures/packets/ch9329.toml`, which is the authority.
fn payload(name: &str) -> Vec<u8> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/packets/ch9329.toml");
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let fixtures: Fixtures = toml::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"));
    fixtures
        .packet
        .into_iter()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("no fixture named {name}"))
        .data
}

// -- the release-all guard --------------------------------------------------

/// Owns the link and sends release-all when it goes out of scope, however it goes out of scope
struct Released(SerialLink);

impl Deref for Released {
    type Target = SerialLink;
    fn deref(&self) -> &SerialLink {
        &self.0
    }
}
impl DerefMut for Released {
    fn deref_mut(&mut self) -> &mut SerialLink {
        &mut self.0
    }
}

impl Drop for Released {
    fn drop(&mut self) {
        let kb = KeyboardReport::RELEASE_ALL.payload();
        let ms = MOUSE_RELEASE_ALL.payload();
        for (name, command, data) in [
            ("keyboard", cmd::SEND_KB_GENERAL_DATA, &kb[..]),
            ("mouse", cmd::SEND_MS_REL_DATA, &ms[..]),
        ] {
            match self.0.transact(command, data, TIMEOUT) {
                Ok(_) => eprintln!("[release-all] {name} report sent"),
                // Cleanup failure must remain visible because the target may still hold keys.
                Err(e) => eprintln!("[release-all] {name} report UNSENT: {e}"),
            }
        }
    }
}

// -- helpers ----------------------------------------------------------------

/// Press and release CapsLock. Two frames, each acknowledged; the ack is timed but asserted on
/// only as "the chip parsed it".
fn tap_caps_lock(link: &mut SerialLink) -> (Duration, Duration) {
    let press = payload("kb_capslock_press");
    let release = payload("kb_release_all");

    let t = Instant::now();
    link.transact(cmd::SEND_KB_GENERAL_DATA, &press, TIMEOUT)
        .expect("CapsLock press acknowledged");
    let press_rtt = t.elapsed();

    let t = Instant::now();
    link.transact(cmd::SEND_KB_GENERAL_DATA, &release, TIMEOUT)
        .expect("key release acknowledged");
    let release_rtt = t.elapsed();

    (press_rtt, release_rtt)
}

struct Flip {
    /// When the unsolicited `0x81` push reporting the new state arrived.
    via_push: Option<Duration>,
    /// When a `GET_INFO` first reported the new state.
    via_query: Option<Duration>,
    info: DeviceInfo,
}

/// Watch for `want` on both channels — the device's own push and a re-query — until `window`.
fn await_caps(link: &mut Released, want: bool, window: Duration) -> Flip {
    let started = Instant::now();
    let mut via_push = None;
    let mut latest = None;

    loop {
        while let Ok(reply) = link.unsolicited().try_recv() {
            if let Some(info) = SerialLink::lock_state_from(&reply) {
                if info.caps_lock == want && via_push.is_none() {
                    via_push = Some(started.elapsed());
                }
                latest = Some(info);
            }
        }
        if let Ok(info) = link.get_info(TIMEOUT) {
            latest = Some(info);
            if info.caps_lock == want {
                return Flip {
                    via_push,
                    via_query: Some(started.elapsed()),
                    info,
                };
            }
        }
        if started.elapsed() >= window {
            return Flip {
                via_push,
                via_query: None,
                info: latest.expect("at least one GET_INFO must have answered"),
            };
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// -- the test ---------------------------------------------------------------

#[test]
#[ignore = "needs the NanoKVM-USB dongle and its target"]
fn caps_lock_round_trip_proves_the_target_processed_the_keystroke() {
    let mut link = Released(SerialLink::open(Path::new(PORT)).expect("open /dev/ttyACM1"));

    let t = Instant::now();
    let start = link.get_info(TIMEOUT).expect("GET_INFO");
    let info_rtt = t.elapsed();
    println!("GET_INFO in {info_rtt:?}: {start:?}");
    assert!(
        (start.version - 1.8).abs() < 0.05,
        "expected version 1.8, got {}",
        start.version
    );
    assert!(
        start.target_connected,
        "the target has not enumerated the HID"
    );

    // -- flip it ----------------------------------------------------------
    let want = !start.caps_lock;
    let (press_rtt, release_rtt) = tap_caps_lock(&mut link);
    println!("CapsLock tap: press ack {press_rtt:?}, release ack {release_rtt:?}");
    let flipped = await_caps(&mut link, want, FLIP_WINDOW);
    println!(
        "caps {} -> {}: push at {:?}, query at {:?}",
        start.caps_lock, flipped.info.caps_lock, flipped.via_push, flipped.via_query
    );
    assert_eq!(
        flipped.info.caps_lock, want,
        "the lock bit did not change: the target did not process the keystroke "
    );

    // -- and back, so the target is left as it was found -------------------
    let (press_rtt2, release_rtt2) = tap_caps_lock(&mut link);
    println!("CapsLock tap: press ack {press_rtt2:?}, release ack {release_rtt2:?}");
    let restored = await_caps(&mut link, start.caps_lock, FLIP_WINDOW);
    println!(
        "caps {} -> {}: push at {:?}, query at {:?}",
        want, restored.info.caps_lock, restored.via_push, restored.via_query
    );
    assert_eq!(
        restored.info.caps_lock, start.caps_lock,
        "the target must be left exactly as it was found"
    );

    println!("stats: {:?}", link.stats());
    assert_eq!(link.is_down(), None);
}
