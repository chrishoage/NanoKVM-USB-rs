//! Real serial transport and input writer recovery against pseudo-terminal devices.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nanokvm::input::testing::wait_for_reconnects;
use nanokvm::input::{spawn_with_source, Config, ReconnectConfig, ReleaseOutcome, ReleaseReason};
use nanokvm::link::{Link, LinkError, LinkSource};
use nanokvm::proto::cmd;
use nanokvm::proto::frame::encode;
use nanokvm::proto::report::{KeyboardReport, MOUSE_RELEASE_ALL};
use nanokvm::serial::fake::{Behaviour, FakeCh9329};
use nanokvm::serial::{OpenOptions, SerialLink, SerialLinkSource, RESYNC_PREAMBLE_LEN};
use serde::Deserialize;

const TIMEOUT: Duration = Duration::from_millis(500);
const WAIT: Duration = Duration::from_secs(10);

// -- fixtures, read as tests/serial_pty.rs reads them ---------------

#[derive(Debug, Deserialize)]
struct Fixtures {
    #[serde(default)]
    packet: Vec<Entry>,
    #[serde(default)]
    response: Vec<Entry>,
    #[serde(default)]
    disagreement: Vec<Disagreement>,
}

#[derive(Debug, Deserialize)]
struct Entry {
    name: String,
    data: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct Disagreement {
    name: String,
    #[serde(default)]
    trigger: String,
}

fn fixtures() -> Fixtures {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/packets/ch9329.toml");
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    toml::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

fn payload(name: &str) -> Vec<u8> {
    let f = fixtures();
    f.packet
        .into_iter()
        .chain(f.response)
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("no fixture named {name}"))
        .data
}

/// Leading two-hex-digit tokens only. Mirrors `scripts/check-fixtures.py::parse`.
fn hex_prefix(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for token in s.split_whitespace() {
        match u8::from_str_radix(token, 16) {
            Ok(b) if token.len() == 2 => out.push(b),
            _ => break,
        }
    }
    out
}

fn script() -> Behaviour {
    Behaviour {
        get_info_payload: payload("get_info_reply"),
        ..Behaviour::default()
    }
}

/// The port options every test here uses: the defaults, with the silence and quiet windows pulled
/// in so a test that provokes a timeout still finishes quickly. Neither is a correctness knob.
fn opts() -> OpenOptions {
    OpenOptions {
        silence_before_expire: Duration::from_millis(30),
        quiet_after_timeout: Duration::from_millis(20),
        ..OpenOptions::default()
    }
}

/// The 16 zero bytes `SerialLink::resync` writes.
fn preamble() -> Vec<u8> {
    vec![0u8; RESYNC_PREAMBLE_LEN]
}

/// The bytes a torn keyboard write leaves behind: a header claiming `LEN 8` with only two payload
/// bytes sent, which is what a `write_all` that failed after seven bytes puts on the wire. Read
/// from the fixture that recorded the measurement rather than retyped, because retyping the
/// anomaly is how a test stops testing the anomaly.
fn torn_frame() -> Vec<u8> {
    let name = "device_receive_parser_has_no_inter_byte_timeout";
    let bytes = hex_prefix(
        &fixtures()
            .disagreement
            .into_iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("no disagreement named {name}"))
            .trigger,
    );
    assert_eq!(
        bytes.len(),
        7,
        "expected seven torn bytes, got {bytes:02X?}"
    );
    bytes
}

/// Poll a condition with a bounded deadline, the way the pty fake's own `wait_for_received` does.
fn settle<F: Fn() -> bool>(what: &str, done: F) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

// -- 1. the preamble on the wire -------------------------------------------

/// `resync` writes sixteen zero bytes, in one go, and they are counted.
#[test]
fn the_preamble_is_sixteen_zero_bytes_in_one_write() {
    let fake = FakeCh9329::spawn(script()).expect("spawn the fake");
    let mut link = SerialLink::open_with(&fake.slave_path(), opts()).expect("open the pty");

    link.resync().expect("the preamble is written");
    settle("the preamble to arrive", || {
        fake.raw_rx().len() >= RESYNC_PREAMBLE_LEN
    });

    assert_eq!(fake.raw_rx(), preamble());
    let stats = link.stats();
    assert_eq!(stats.resyncs_sent, 1);
    assert_eq!(stats.bytes_tx, RESYNC_PREAMBLE_LEN as u64);
    assert_eq!(link.is_down(), None);
}

/// H-shape, without hardware: zeros in front of a healthy link change nothing. The device
/// still answers `GET_INFO`, and the frame that follows the preamble is intact on the wire.
#[test]
fn zeros_in_front_of_a_healthy_link_are_harmless() {
    let fake = FakeCh9329::spawn(script()).expect("spawn the fake");
    let mut link = SerialLink::open_with(&fake.slave_path(), opts()).expect("open the pty");

    let before = link
        .get_info(TIMEOUT)
        .expect("GET_INFO before the preamble");
    link.resync().expect("the preamble is written");
    let after = link.get_info(TIMEOUT).expect("GET_INFO after the preamble");

    assert_eq!(
        before, after,
        "the preamble changed nothing the device reports"
    );
    let mut expected = encode(cmd::GET_INFO, &[]).expect("GET_INFO frames");
    expected.extend(preamble());
    expected.extend(encode(cmd::GET_INFO, &[]).expect("GET_INFO frames"));
    assert_eq!(
        fake.raw_rx(),
        expected,
        "the stream is the first request, then sixteen zeros, then the second request — nothing \
         interleaved and nothing lost"
    );
}

/// A torn frame consumes the next request without padding; the preamble must
/// restore framing so the same request receives a reply.
#[test]
fn a_torn_frame_eats_the_next_command_and_the_preamble_is_what_frees_it() {
    let fake = FakeCh9329::spawn(script()).expect("spawn the fake");
    let mut link = SerialLink::open_with(&fake.slave_path(), opts()).expect("open the pty");

    // The negative control: tear a frame, then ask a question. The parser at the far end is still
    // waiting for the eight payload bytes it was promised and consumes the request as them.
    link.write_raw_for_test(&torn_frame())
        .expect("the torn bytes are written");
    let swallowed = link.transact(cmd::GET_INFO, &[], Duration::from_millis(200));
    assert!(
        matches!(swallowed, Err(LinkError::Timeout { .. })),
        "a torn frame must eat the next command, or this test is not reproducing : \
         {swallowed:?}"
    );

    // And the repair.
    link.resync().expect("the preamble is written");
    let info = link
        .get_info(TIMEOUT)
        .expect("GET_INFO answers once the stale frame has been completed");
    assert!((info.version - 1.8).abs() < 0.05);
    assert_eq!(link.is_down(), None);
}

// -- 2. the source, and the receiver that outlives its links ----------------

/// The unsolicited channel must survive replacement links so lock updates continue.
#[test]
fn the_unsolicited_receiver_survives_a_reopen() {
    let fake = FakeCh9329::spawn(script()).expect("spawn the fake");
    let (mut source, unsolicited) = SerialLinkSource::new(&fake.slave_path(), opts());
    let push = encode(cmd::GET_INFO | 0x80, &payload("get_info_reply")).expect("a 0x81 push");

    let first = source.open().expect("open the pty");
    fake.inject_unsolicited(&push);
    let got = unsolicited
        .recv_timeout(WAIT)
        .expect("the push reaches the source's receiver");
    assert!(nanokvm::serial::lock_state_from(&got).is_some());

    // The link goes; the receiver does not.
    drop(first);
    let second = source.open().expect("reopen the same pty");
    fake.inject_unsolicited(&push);
    let got = unsolicited
        .recv_timeout(WAIT)
        .expect("a push on the second link reaches the same receiver");
    assert!(
        nanokvm::serial::lock_state_from(&got).is_some(),
        "the evidence that the target processed a keystroke must not stop at a reconnect"
    );
    drop(second);
}

/// A source that cannot open reports it as `Down` — which is what the writer's retry loop expects
/// and what `LinkError::is_transport_failure` classifies.
#[test]
fn a_source_on_a_missing_path_refuses_rather_than_panicking() {
    let (mut source, _rx) = SerialLinkSource::new(Path::new("/dev/nonexistent-nanokvm"), opts());
    let Err(err) = source.open() else {
        panic!("there is no such device, so the open must fail");
    };
    assert!(matches!(err, LinkError::Down(_)), "{err:?}");
    assert!(err.is_transport_failure());
    assert!(source.describe().contains("nonexistent-nanokvm"));
}

// -- 3. the writer, end to end, over two ptys ------------------------------

/// A [`LinkSource`] over whichever pty the test currently points it at. The real
/// [`SerialLinkSource`] is bound to one path for the reasons in its documentation; a test that
/// wants the *device* to be replaced rather than merely reopened needs to move the path, so it
/// implements the trait itself over the same `SerialLink`.
struct SwappablePty {
    path: Arc<Mutex<Option<PathBuf>>>,
}

impl LinkSource for SwappablePty {
    fn open(&mut self) -> Result<Box<dyn Link>, LinkError> {
        let path = self
            .path
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .ok_or_else(|| LinkError::Down("no pty is plugged in".into()))?;
        Ok(Box::new(SerialLink::open_with(&path, opts())?))
    }
    fn describe(&self) -> String {
        "a swappable pty".to_string()
    }
}

/// Idle link loss must trigger the same commissioning sequence as a failed write:
/// preamble, keyboard release, mouse release, then device information.
#[test]
fn the_writer_reconnects_over_a_real_port_and_the_new_device_sees_the_whole_sequence() {
    let first = FakeCh9329::spawn(script()).expect("spawn the first fake");
    let path = Arc::new(Mutex::new(Some(first.slave_path())));
    let source = SwappablePty {
        path: Arc::clone(&path),
    };
    let (producer, writer) = spawn_with_source(
        Box::new(source),
        Config {
            transact_timeout: Duration::from_millis(200),
            reconnect: ReconnectConfig {
                initial_backoff: Duration::from_millis(2),
                max_backoff: Duration::from_millis(20),
                get_info_timeout: Duration::from_millis(200),
            },
            ..Config::default()
        },
    );
    producer
        .wait_for_link(WAIT)
        .expect("the first link comes up");

    // The device goes away, and a different one takes its place.
    let second = FakeCh9329::spawn(script()).expect("spawn the second fake");
    first.hang_up();
    *path.lock().unwrap_or_else(|p| p.into_inner()) = Some(second.slave_path());

    // Submit nothing: transport health alone must trigger recovery.
    wait_for_reconnects(&producer, 1);

    let expected: Vec<u8> = preamble()
        .into_iter()
        .chain(
            encode(
                cmd::SEND_KB_GENERAL_DATA,
                &KeyboardReport::RELEASE_ALL.payload(),
            )
            .expect("frames"),
        )
        .chain(encode(cmd::SEND_MS_REL_DATA, &MOUSE_RELEASE_ALL.payload()).expect("frames"))
        .chain(encode(cmd::GET_INFO, &[]).expect("frames"))
        .collect();
    settle("the sequence to land on the second device", || {
        second.raw_rx().len() >= expected.len()
    });
    assert_eq!(
        second.raw_rx(),
        expected,
        "the new device must see the preamble first, then the release-all pair, then the proof"
    );
    assert_eq!(
        second.raw_rx()[..RESYNC_PREAMBLE_LEN],
        [0u8; RESYNC_PREAMBLE_LEN],
        "and the preamble really is sixteen zeros on the wire"
    );

    let stats = producer.stats();
    assert!(!stats.link_down);
    assert_eq!(stats.reconnects, 1);
    assert_eq!(
        stats.last_release.map(|r| (r.reason, r.outcome)),
        Some((ReleaseReason::Reconnected, ReleaseOutcome::Submitted))
    );

    // And the reconnected writer drives the new device.
    producer.engage().expect("deliberate recapture");
    producer
        .submit(nanokvm::input::Event::Key {
            key: nanokvm::proto::report::HidKey::Usage(0x04),
            down: true,
        })
        .expect("the link is up again");
    settle("the keystroke to land on the second device", || {
        second
            .received()
            .iter()
            .any(|f| f.cmd == cmd::SEND_KB_GENERAL_DATA && f.data.get(2) == Some(&0x04))
    });

    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);
}

/// An empty input queue must not hide a hung-up link from status or producers.
#[test]
fn an_idle_writer_notices_a_link_that_hung_up_with_nothing_queued() {
    let fake = FakeCh9329::spawn(script()).expect("spawn the fake");
    let (source, _unsolicited) = SerialLinkSource::new(&fake.slave_path(), opts());
    let (producer, writer) = spawn_with_source(
        Box::new(source),
        Config {
            transact_timeout: Duration::from_millis(200),
            reconnect: ReconnectConfig {
                initial_backoff: Duration::from_millis(2),
                max_backoff: Duration::from_millis(20),
                get_info_timeout: Duration::from_millis(200),
            },
            ..Config::default()
        },
    );
    producer.wait_for_link(WAIT).expect("the link comes up");
    assert!(!producer.stats().link_down);

    // Remove the device while the input queue is empty.
    fake.hang_up();

    // Allow one second for the 100 ms health poll; five seconds is the test failsafe.
    let limit = Duration::from_secs(5);
    let deadline = Instant::now() + limit;
    while !producer.stats().link_down {
        assert!(
            Instant::now() < deadline,
            "the writer must report the link down within {limit:?} of the hang-up even with \
             nothing queued"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    // Stop the fixed-path writer before another parallel test can reuse this PTY
    // number. Otherwise reconnect could send commissioning reports to that test.
    assert_eq!(writer.shutdown(), ReleaseOutcome::Unsent);
}
