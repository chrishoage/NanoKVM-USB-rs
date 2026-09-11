//! `serial` against the pty fake (§9.3). The `SerialLink` under test is the real one, unmodified;
//! only the far end of the port is synthetic.
//!
//! Every reply the fake sends is either loaded from `fixtures/packets/ch9329.toml` or asserted
//! against it, because that file is the authority for byte-level correctness (§9.1) and retyping
//! its bytes is how a transcription bug silently blesses a wrong transport.
//!
//! Nothing here sleeps as its only guard. Where a test must act between two events on the link it
//! uses the fake's own knobs — `wait_for_received`, `inject_before_reply`, `drop_next_reply` —
//! which are exact rather than merely likely.

use std::time::{Duration, Instant};

use nanokvm::link::{Link, LinkError};
use nanokvm::proto::cmd;
use nanokvm::proto::frame::encode;
use nanokvm::serial::fake::{Behaviour, FakeCh9329};
use nanokvm::serial::{OpenOptions, SerialLink};
use serde::Deserialize;

// -- the fixture file, read exactly as tests/proto_fixtures.rs reads it -----

#[derive(Debug, Deserialize)]
struct Fixtures {
    #[serde(default)]
    packet: Vec<Entry>,
    #[serde(default)]
    response: Vec<Entry>,
    #[serde(default)]
    disagreement: Vec<Disagreement>,
}

/// A `[[disagreement]]` entry: where the wire parted company with the documented frame format
/// (§9.1). Only the observed bytes are needed here — the note is the finding, not the test.
#[derive(Debug, Deserialize)]
struct Disagreement {
    name: String,
    #[serde(default)]
    observed_reply: String,
}

#[derive(Debug, Deserialize)]
struct Entry {
    name: String,
    data: Vec<u8>,
    frame: String,
}

fn fixtures() -> Fixtures {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/packets/ch9329.toml");
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    toml::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"))
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

fn entry(name: &str) -> Entry {
    let f = fixtures();
    f.packet
        .into_iter()
        .chain(f.response)
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("no fixture named {name}"))
}

/// The payload of a fixture, e.g. the `GET_INFO` reply the fake should send.
fn payload(name: &str) -> Vec<u8> {
    entry(name).data
}

/// The complete on-the-wire frame of a fixture.
fn frame(name: &str) -> Vec<u8> {
    hex_prefix(&entry(name).frame)
}

/// The bytes a `[[disagreement]]` says the device really sent, e.g. the §3.2 five-byte reply with
/// no checksum byte. Read from the file for the same reason as everything else here: retyping the
/// anomaly is how a test stops testing the anomaly.
fn observed_reply(name: &str) -> Vec<u8> {
    let d = fixtures()
        .disagreement
        .into_iter()
        .find(|d| d.name == name)
        .unwrap_or_else(|| panic!("no disagreement named {name}"));
    hex_prefix(&d.observed_reply)
}

// -- harness ----------------------------------------------------------------

const TIMEOUT: Duration = Duration::from_millis(500);

/// A fake whose `GET_INFO` answer comes from the fixture file, and a real link onto its pty.
fn rig(script: Behaviour) -> (FakeCh9329, SerialLink) {
    let fake = FakeCh9329::spawn(script).expect("spawn the fake CH9329");
    let link = SerialLink::open(&fake.slave_path()).expect("open the fake's pty");
    (fake, link)
}

/// [`rig`] with the port options spelled out, for the tests that turn a timing knob.
fn rig_with(script: Behaviour, opts: OpenOptions) -> (FakeCh9329, SerialLink) {
    let fake = FakeCh9329::spawn(script).expect("spawn the fake CH9329");
    let link = SerialLink::open_with(&fake.slave_path(), opts).expect("open the fake's pty");
    (fake, link)
}

fn default_script() -> Behaviour {
    Behaviour {
        get_info_payload: payload("get_info_reply"),
        ..Behaviour::default()
    }
}

/// The 8-byte all-zero keyboard report (fixture `kb_release_all`), used wherever a test just needs
/// a well-formed request that is safe in every context.
fn kb_release_all() -> Vec<u8> {
    payload("kb_release_all")
}

// -- 1. open + get_info (§12 Stage 1: no handshake beyond GET_INFO) ---------

#[test]
fn get_info_reports_version_1_8_target_connected_and_locks_off() {
    let (fake, mut link) = rig(default_script());

    let info = link.get_info(TIMEOUT).expect("GET_INFO");

    assert!(
        (info.version - 1.8).abs() < 1e-6,
        "version {}",
        info.version
    );
    assert!(info.target_connected);
    assert!(!info.num_lock && !info.caps_lock && !info.scroll_lock);
    assert_eq!(link.is_down(), None);

    let received = fake.received();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].cmd, cmd::GET_INFO);
    assert!(received[0].data.is_empty());
    assert_eq!(link.stats().frames_rx, 1);
    assert_eq!(link.stats().unsolicited, 0);
}

// -- 2. matched by command byte, not arrival order (§9.2 item 6, A12) ------

#[test]
fn a_reply_is_matched_by_command_byte_not_by_arrival_order() {
    let (fake, mut link) = rig(default_script());

    // The A12 ordering exactly: a lock-state push lands between the request and its ack.
    fake.inject_before_reply(&frame("get_info_reply_caps_on"));

    let reply = link
        .transact(cmd::SEND_KB_GENERAL_DATA, &kb_release_all(), TIMEOUT)
        .expect("the keyboard ack, not the lock-state push");

    assert_eq!(
        reply.cmd,
        cmd::SEND_KB_GENERAL_DATA | 0x80,
        "must be the 0x82 ack"
    );
    assert_eq!(reply.data, payload("kb_ack"));

    let pushed = link
        .unsolicited()
        .recv_timeout(TIMEOUT)
        .expect("the 0x81 frame is diverted, not dropped");
    let locks = SerialLink::lock_state_from(&pushed).expect("0x81 decodes as DeviceInfo");
    assert!(locks.caps_lock, "the push carries caps_lock = true");
    assert!(!locks.num_lock && !locks.scroll_lock);

    let stats = link.stats();
    assert_eq!(stats.frames_rx, 2);
    assert_eq!(stats.unsolicited, 1);
    assert_eq!(stats.late_replies, 0, "this one was never requested at all");
}

// -- 3. fragmentation, leading garbage, and sustained traffic (§9.2 item 6) -

#[test]
fn a_reply_split_byte_by_byte_is_reassembled() {
    let (fake, mut link) = rig(default_script());
    fake.fragment_replies(true);

    let info = link
        .get_info(Duration::from_secs(2))
        .expect("GET_INFO in pieces");
    assert!((info.version - 1.8).abs() < 1e-6);
    assert_eq!(
        link.stats().bad_checksum,
        0,
        "a split frame is not a corrupt one"
    );
    assert_eq!(link.stats().truncated, 0);
}

#[test]
fn garbage_before_a_reply_is_skipped_and_counted() {
    let (fake, mut link) = rig(default_script());
    fake.prefix_garbage(&[0xDE, 0xAD, 0xBE, 0xEF]);

    let info = link.get_info(TIMEOUT).expect("GET_INFO behind garbage");
    assert!(info.target_connected);
    assert_eq!(link.stats().garbage_bytes, 4);
    assert_eq!(link.stats().frames_rx, 1);
}

#[test]
fn two_hundred_sequential_transacts_all_succeed() {
    let (fake, mut link) = rig(default_script());
    let report = kb_release_all();

    for i in 0..200 {
        let reply = link
            .transact(cmd::SEND_KB_GENERAL_DATA, &report, TIMEOUT)
            .unwrap_or_else(|e| panic!("transact {i}: {e}"));
        assert_eq!(reply.cmd, cmd::SEND_KB_GENERAL_DATA | 0x80);
    }

    let stats = link.stats();
    assert_eq!(stats.transacts, 200);
    assert_eq!(stats.frames_rx, 200);
    assert_eq!(stats.timeouts, 0);
    assert_eq!(stats.unsolicited, 0);
    assert_eq!(stats.bad_checksum, 0);
    assert_eq!(stats.garbage_bytes, 0);
    assert_eq!(fake.received().len(), 200);
}

// -- 4. error frames are surfaced, never dropped (§3.1, A14) ---------------

#[test]
fn an_error_frame_surfaces_as_link_error_device_and_the_link_survives() {
    let (fake, mut link) = rig(default_script());
    // The device sends 0xE4 for a checksum error (fixture `err_checksum_on_kb`). The fake is told
    // to reject the command outright rather than reproducing a corrupt write, because `SerialLink`
    // has no way to emit a bad checksum on purpose and should not grow one: `proto::encode` is the
    // only frame builder, and giving the transport a "write it wrong" hook would make the property
    // untestable elsewhere. The fake's own bad-checksum path is covered separately, below.
    fake.reject_cmd(cmd::SEND_KB_GENERAL_DATA, 0xE4);

    let err = link
        .transact(cmd::SEND_KB_GENERAL_DATA, &kb_release_all(), TIMEOUT)
        .expect_err("an error frame is an error");
    match err {
        LinkError::Device { cmd, code } => {
            assert_eq!(cmd, cmd::SEND_KB_GENERAL_DATA);
            assert_eq!(code, 0xE4);
        }
        other => panic!("expected LinkError::Device, got {other:?}"),
    }
    assert_eq!(
        link.is_down(),
        None,
        "a device error is not a transport failure"
    );
    assert_eq!(link.stats().device_errors, 1);

    fake.accept_cmd(cmd::SEND_KB_GENERAL_DATA);
    let reply = link
        .transact(cmd::SEND_KB_GENERAL_DATA, &kb_release_all(), TIMEOUT)
        .expect("the link recovers immediately (§3.1)");
    assert_eq!(reply.cmd, cmd::SEND_KB_GENERAL_DATA | 0x80);
}

#[test]
fn the_fakes_bad_checksum_path_answers_with_the_fixture_error_frame() {
    // Covers the half of §3.1 the test above cannot reach through `SerialLink`: a genuinely
    // corrupt frame on the wire, answered by `cmd | 0xC0` with 0xE4.
    let fake = FakeCh9329::spawn(default_script()).expect("spawn");
    let mut port = serialport::new(fake.slave_path().to_string_lossy().as_ref(), 57600)
        .timeout(Duration::from_millis(500))
        .open()
        .expect("open the pty directly");

    let mut corrupt = frame("get_info");
    let last = corrupt.len() - 1;
    corrupt[last] = corrupt[last].wrapping_add(1);
    std::io::Write::write_all(&mut port, &corrupt).expect("write");

    let want = frame("err_checksum_on_get_info");
    let mut got = vec![0u8; want.len()];
    std::io::Read::read_exact(&mut port, &mut got).expect("read the error frame");
    assert_eq!(got, want);
}

// -- 5. the five-byte reply with no checksum byte (§3.2, A13) --------------

#[test]
fn the_five_byte_no_checksum_reply_times_out_and_the_link_still_works() {
    let (_fake, mut link) = rig(default_script());
    // 0x7E is the undefined command the disagreement fixture was triggered with.
    const UNDEFINED: u8 = 0x7E;
    let timeout = Duration::from_millis(300);

    let started = Instant::now();
    let err = link
        .transact(UNDEFINED, &[], timeout)
        .expect_err("five bytes never become a frame");
    let elapsed = started.elapsed();

    match err {
        LinkError::Timeout { cmd, .. } => assert_eq!(cmd, UNDEFINED),
        other => panic!("expected LinkError::Timeout, got {other:?}"),
    }
    assert!(
        elapsed < timeout + Duration::from_millis(400),
        "returned after {elapsed:?}, which is not promptly"
    );
    assert_eq!(
        link.stats().truncated,
        1,
        "the reader must retire the partial frame via expire_partial"
    );
    assert_eq!(link.is_down(), None);

    let info = link
        .get_info(TIMEOUT)
        .expect("the next command is unaffected");
    assert!(info.target_connected);
}

// -- 6. hang-up (§2.7's precondition; A15's "the link is not trusted") -----

#[test]
fn a_hang_up_takes_the_link_down_promptly_and_the_reader_thread_joins() {
    let (fake, mut link) = rig(default_script());
    fake.drop_next_reply();

    let started = Instant::now();
    let result = std::thread::scope(|s| {
        let waiter = s.spawn(|| link.transact(cmd::GET_INFO, &[], Duration::from_secs(5)));
        assert!(
            fake.wait_for_received(1, Duration::from_secs(2)),
            "the fake never saw the request"
        );
        fake.hang_up();
        waiter.join()
    })
    .expect("the transact thread must not panic");
    let elapsed = started.elapsed();

    assert!(
        matches!(
            result,
            Err(LinkError::Down(_)) | Err(LinkError::Timeout { .. })
        ),
        "expected Down or Timeout, got {result:?}"
    );
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    assert!(link.is_down().is_some(), "the link must know it is gone");

    let started = Instant::now();
    let after = link.transact(cmd::GET_INFO, &[], Duration::from_secs(5));
    assert!(
        matches!(after, Err(LinkError::Down(_))),
        "a transact on a down link must not block: {after:?}"
    );
    assert!(started.elapsed() < Duration::from_millis(100));

    let started = Instant::now();
    drop(link);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "dropping the link must join the reader thread: {:?}",
        started.elapsed()
    );
}

// -- 7. every request was one contiguous well-formed frame (§5.1, A15) ----

#[test]
fn every_request_reached_the_device_as_one_contiguous_well_formed_frame() {
    let (fake, mut link) = rig(default_script());

    let requests: Vec<(u8, Vec<u8>)> = vec![
        (cmd::GET_INFO, Vec::new()),
        (cmd::SEND_KB_GENERAL_DATA, payload("kb_capslock_press")),
        (cmd::SEND_KB_GENERAL_DATA, kb_release_all()),
        (cmd::SEND_MS_REL_DATA, payload("mouse_rel_idle")),
        (cmd::SEND_MS_ABS_DATA, payload("mouse_abs_centre_4096")),
    ];
    for (c, p) in &requests {
        link.transact(*c, p, TIMEOUT)
            .unwrap_or_else(|e| panic!("transact {c:#04x}: {e}"));
    }

    let mut want = Vec::new();
    for (c, p) in &requests {
        want.extend_from_slice(&encode(*c, p).expect("fixture payloads all fit"));
    }
    assert_eq!(
        fake.raw_rx(),
        want,
        "the byte stream must be exactly the frames, back to back, with nothing between them"
    );
    assert_eq!(link.stats().bytes_tx, want.len() as u64);

    // And the same thing said in frames rather than bytes.
    let got: Vec<(u8, Vec<u8>)> = fake
        .received()
        .into_iter()
        .map(|f| (f.cmd, f.data))
        .collect();
    assert_eq!(got, requests);
}

// -- 8. a late reply is not the next request's reply ----------------------

#[test]
fn a_late_reply_is_not_mistaken_for_the_next_requests_reply() {
    let (fake, mut link) = rig(default_script());

    // Latched when the request arrives, so it applies to this request and no other.
    fake.delay_reply(Duration::from_millis(400));
    let err = link
        .transact(
            cmd::SEND_KB_GENERAL_DATA,
            &kb_release_all(),
            Duration::from_millis(80),
        )
        .expect_err("the fake is holding this one back");
    assert!(matches!(err, LinkError::Timeout { cmd, .. } if cmd == cmd::SEND_KB_GENERAL_DATA));
    // Safe to reset now: the delay for the request above was latched before it became visible in
    // `received`, and that request is already in flight.
    assert!(fake.wait_for_received(1, TIMEOUT));
    fake.delay_reply(Duration::ZERO);

    // A different command, so that mis-attributing the late 0x82 would be visible as the wrong
    // reply rather than an accidentally correct one.
    let info = link
        .get_info(Duration::from_secs(2))
        .expect("request two gets its own reply");
    assert!(info.target_connected);

    let stats = link.stats();
    assert_eq!(stats.timeouts, 1);
    assert_eq!(
        stats.late_replies, 1,
        "the late 0x82 must be counted as late"
    );
    assert_eq!(stats.unsolicited, 1, "and diverted, not delivered");

    let late = link
        .unsolicited()
        .recv_timeout(TIMEOUT)
        .expect("the late reply is still available to the app");
    assert_eq!(late.cmd, cmd::SEND_KB_GENERAL_DATA | 0x80);
}

// -- odds and ends --------------------------------------------------------

#[test]
fn a_short_get_info_payload_is_an_io_error_and_leaves_the_link_up() {
    let (_fake, mut link) = rig(Behaviour {
        // Two bytes: not enough for [version, connected, locks] (§3.2 — device lengths are claims).
        get_info_payload: vec![0x38, 0x01],
        ..Behaviour::default()
    });

    let err = link
        .get_info(TIMEOUT)
        .expect_err("a short payload is not a DeviceInfo");
    assert!(matches!(err, LinkError::Io(_)), "got {err:?}");
    assert_eq!(
        link.is_down(),
        None,
        "a malformed payload is not a dead cable"
    );
}

#[test]
fn an_unsolicited_push_with_no_request_in_flight_is_still_delivered() {
    let (fake, link) = rig(default_script());
    fake.inject_unsolicited(&frame("get_info_reply_caps_on"));

    let pushed = link
        .unsolicited()
        .recv_timeout(TIMEOUT)
        .expect("the idle push");
    let locks = SerialLink::lock_state_from(&pushed).expect("decodes");
    assert!(locks.caps_lock);
    assert_eq!(link.stats().unsolicited, 1);
}

/// Poll `garbage_bytes` up to a deadline, the way the fake's own `wait_for_received` polls.
///
/// The count is folded by the **reader thread**, which is not synchronised with `transact`:
/// `get_info` returns as soon as its own reply is routed, and the bytes injected ahead of it may
/// still be in flight in the parser. Asserting the total immediately is therefore a race — it
/// failed about one run in sixteen — and the fix is the one the rest of this file already uses:
/// wait for the condition with a bound, never sleep and hope.
fn wait_for_garbage(link: &SerialLink, at_least: u64, timeout: Duration) -> u64 {
    let deadline = Instant::now() + timeout;
    loop {
        let seen = link.stats().garbage_bytes;
        if seen >= at_least || Instant::now() >= deadline {
            return seen;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn the_reader_thread_survives_pure_garbage() {
    let (fake, mut link) = rig(default_script());
    fake.inject_unsolicited(&[0x00; 64]);
    fake.inject_unsolicited(&[0xFF; 64]);

    let info = link
        .get_info(Duration::from_secs(2))
        .expect("still working");
    assert!(info.target_connected);
    let garbage = wait_for_garbage(&link, 128, TIMEOUT);
    assert!(
        garbage >= 128,
        "only {garbage} of the 128 garbage bytes were counted"
    );
    assert_eq!(link.is_down(), None);
}

#[test]
fn open_options_are_honoured_and_a_missing_port_is_reported_as_down() {
    let err = SerialLink::open_with(
        std::path::Path::new("/dev/nanokvm-does-not-exist"),
        OpenOptions::default(),
    )
    .expect_err("there is no such port");
    assert!(matches!(err, LinkError::Down(_)), "got {err:?}");
}

// -- 9. the resynchronisation window after a timeout (§3.1, §5.1, A11, A12) -----------------
//
// The four tests below are the ones the review turned up. The device's reply to a request that
// already gave up is byte-identical to the reply the next request of that command is waiting for,
// and the input writer sends one command byte over and over (§2.6, A11), so the transport cannot
// tell them apart by content. It tells them apart in time instead: after a timeout the next
// request waits for the link to fall quiet, and until it does, a frame answering the timed-out
// command is diverted rather than delivered.

/// The defect the review found: `route` preferred the in-flight slot, so the reply to a request
/// that had already timed out was handed to the *next* request of the same command — reporting a
/// success for a frame the device may never have parsed (§3.4, A17). Here the second request is
/// never answered at all, so anything delivered to it can only be the first request's.
#[test]
fn a_late_reply_is_never_handed_to_the_next_transact_of_the_same_command() {
    let (fake, mut link) = rig(default_script());

    // 400 ms is far outside the 50 ms quiet window, which is the point: the window cannot be what
    // saves this one, so the reader's timed-out slot has to.
    fake.delay_reply(Duration::from_millis(400));
    let first = link.transact(
        cmd::SEND_KB_GENERAL_DATA,
        &kb_release_all(),
        Duration::from_millis(80),
    );
    assert!(
        matches!(first, Err(LinkError::Timeout { cmd, .. }) if cmd == cmd::SEND_KB_GENERAL_DATA),
        "the fake is holding this one back: {first:?}"
    );
    // Safe to reset now: request one's reply was planned before it became visible in `received`.
    assert!(fake.wait_for_received(1, TIMEOUT));
    fake.delay_reply(Duration::ZERO);
    fake.drop_next_reply();

    let second = link.transact(
        cmd::SEND_KB_GENERAL_DATA,
        &kb_release_all(),
        Duration::from_millis(600),
    );
    assert!(
        matches!(second, Err(LinkError::Timeout { .. })),
        "the device never acknowledged frame two, so this can only be frame one's late reply: \
         {second:?}"
    );

    let stats = link.stats();
    assert_eq!(stats.timeouts, 2);
    assert_eq!(stats.late_replies, 1, "the 0x82 that did arrive was late");
    assert_eq!(stats.unsolicited, 1, "and diverted, not delivered");
    assert_eq!(stats.resyncs, 1, "one quiet window, entered by request two");
    assert_eq!(fake.received().len(), 2, "both requests reached the device");

    let late = link
        .unsolicited()
        .recv_timeout(TIMEOUT)
        .expect("the late reply is still available to the app");
    assert_eq!(late.cmd, cmd::SEND_KB_GENERAL_DATA | 0x80);
    assert_eq!(link.is_down(), None, "none of this is a transport failure");
}

/// The same ambiguity in its ordinary form, where the late reply does land inside the quiet
/// window. It must be counted and diverted, and the next request — the *same* command — must get
/// its own reply. The two replies are made distinguishable by their payload, since on the wire
/// they would otherwise be the same bytes.
#[test]
fn a_late_reply_inside_the_quiet_window_is_diverted_and_the_next_transact_gets_its_own_reply() {
    let quiet = Duration::from_millis(300);
    let opts = OpenOptions {
        quiet_after_timeout: quiet,
        ..OpenOptions::default()
    };
    let (fake, mut link) = rig_with(default_script(), opts);

    // Held back past the 100 ms deadline, but well inside the 300 ms window.
    fake.delay_reply(Duration::from_millis(200));
    let first = link.transact(cmd::GET_INFO, &[], Duration::from_millis(100));
    assert!(
        matches!(first, Err(LinkError::Timeout { cmd, .. }) if cmd == cmd::GET_INFO),
        "{first:?}"
    );
    assert!(fake.wait_for_received(1, TIMEOUT));
    fake.delay_reply(Duration::ZERO);
    fake.set_get_info_payload(&payload("get_info_reply_caps_on"));

    let reply = link
        .transact(cmd::GET_INFO, &[], Duration::from_secs(2))
        .expect("request two gets a reply");
    assert_eq!(
        reply.data,
        payload("get_info_reply_caps_on"),
        "request two must get the answer written for it, not request one's"
    );

    let late = link
        .unsolicited()
        .recv_timeout(TIMEOUT)
        .expect("the late reply is diverted, not dropped");
    assert_eq!(
        late.data,
        payload("get_info_reply"),
        "and the diverted frame is request one's"
    );

    let stats = link.stats();
    assert_eq!(stats.frames_rx, 2);
    assert_eq!(stats.timeouts, 1);
    assert_eq!(stats.late_replies, 1);
    assert_eq!(stats.unsolicited, 1);
    assert_eq!(stats.resyncs, 1);
    assert_eq!(fake.received().len(), 2);
}

/// The window must not become a hang. Here the device chatters without pause, and with acks for
/// the very command that timed out — the worst case, since every one of them is a candidate late
/// reply. `transact` waits at most twice `quiet_after_timeout` and then writes anyway (§2.6: this
/// is the thread that delivers release-alls).
#[test]
fn the_quiet_window_is_bounded_when_the_device_will_not_stop_talking() {
    let quiet = Duration::from_millis(100);
    let opts = OpenOptions {
        quiet_after_timeout: quiet,
        ..OpenOptions::default()
    };
    let (fake, mut link) = rig_with(default_script(), opts);

    fake.drop_next_reply();
    let first = link.transact(
        cmd::SEND_KB_GENERAL_DATA,
        &kb_release_all(),
        Duration::from_millis(100),
    );
    assert!(matches!(first, Err(LinkError::Timeout { .. })), "{first:?}");
    assert!(fake.wait_for_received(1, TIMEOUT));

    let ack = encode(cmd::SEND_KB_GENERAL_DATA | 0x80, &payload("kb_ack")).expect("the ack fits");
    let spamming = std::sync::atomic::AtomicBool::new(true);
    let elapsed = std::thread::scope(|s| {
        s.spawn(|| {
            while spamming.load(std::sync::atomic::Ordering::Relaxed) {
                fake.inject_unsolicited(&ack);
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        let started = Instant::now();
        let second = link.transact(
            cmd::SEND_KB_GENERAL_DATA,
            &kb_release_all(),
            Duration::from_secs(2),
        );
        let elapsed = started.elapsed();
        spamming.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(
            second.is_ok(),
            "the request went out and was answered: {second:?}"
        );
        elapsed
    });

    assert!(
        elapsed < 2 * quiet + Duration::from_millis(600),
        "the writer was held for {elapsed:?}, past the {:?} bound plus slack",
        2 * quiet
    );
    let stats = link.stats();
    assert!(
        stats.unsolicited > 1,
        "the device really was chattering throughout: {stats:?}"
    );
    assert_eq!(stats.resyncs, 1);
    assert_eq!(link.is_down(), None);
}

/// The second defect the review found: when the unsolicited receiver had been dropped, the failed
/// `send` stopped the reader thread without marking the link down, so every later `transact` burnt
/// its whole timeout against a link still reporting itself healthy. Dropping the receiver is a
/// supported thing to do (§4.1) and must cost nothing but the frames themselves.
#[test]
fn dropping_the_unsolicited_receiver_leaves_the_link_hearing_and_working() {
    let (fake, mut link) = rig(default_script());
    link.transact(cmd::SEND_KB_GENERAL_DATA, &kb_release_all(), TIMEOUT)
        .expect("the first report is acknowledged");
    let before = link.stats().frames_rx;

    // Somebody takes the receiver and drops it, then the device pushes a lock-state frame (A12).
    drop(link.take_unsolicited());
    fake.inject_unsolicited(&frame("get_info_reply_caps_on"));

    let deadline = Instant::now() + TIMEOUT;
    while link.stats().unsolicited_dropped == 0 {
        assert!(
            Instant::now() < deadline,
            "the reader never saw the push: {:?}",
            link.stats()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        link.stats().frames_rx,
        before + 1,
        "the reader kept reading and kept parsing"
    );
    assert_eq!(link.is_down(), None, "and the link is still up");

    let reply = link
        .transact(cmd::SEND_KB_GENERAL_DATA, &kb_release_all(), TIMEOUT)
        .expect("a transact after the drop still gets its reply");
    assert_eq!(reply.cmd, cmd::SEND_KB_GENERAL_DATA | 0x80);

    let stats = link.stats();
    assert_eq!(stats.frames_rx, before + 2);
    assert_eq!(stats.unsolicited, 1);
    assert_eq!(stats.unsolicited_dropped, 1, "counted, not silently lost");
    assert_eq!(stats.timeouts, 0);
}

// -- 10. the write half has its own deadline (§5.1, A11, A15) -------------------------------

/// `serialport` keeps one timeout per handle and uses it for both directions, so the reader's
/// 25 ms heartbeat used to be the deadline for `write_all` of a frame as well — and a `write_all`
/// cut short tears the frame, which the chip then eats as the head of the next command (A15).
/// The halves now carry different timeouts, and neither leaks into the other.
#[test]
fn the_write_half_has_its_own_timeout_and_the_reader_keeps_its_heartbeat() {
    let defaults = OpenOptions::default();
    assert_eq!(defaults.read_timeout, Duration::from_millis(25));
    assert_eq!(defaults.write_timeout, Duration::from_secs(1));
    assert_eq!(defaults.quiet_after_timeout, Duration::from_millis(50));
    assert!(
        defaults.write_timeout > defaults.read_timeout,
        "a write deadline as short as the reader's heartbeat is the A15 failure"
    );

    // Two hundred times the heartbeat. If the reader shared it, the loop below would wake every
    // five seconds and the partial frame could not be retired on schedule.
    let opts = OpenOptions {
        write_timeout: Duration::from_secs(5),
        ..OpenOptions::default()
    };
    let (fake, mut link) = rig_with(default_script(), opts);

    let info = link.get_info(TIMEOUT).expect("writing still works");
    assert!(info.target_connected);

    // The §3.2 five-byte reply with no checksum byte (A13), pushed with nothing waiting for it.
    // Only the reader's own heartbeat can retire it, and only after `silence_before_expire`.
    let started = Instant::now();
    fake.inject_unsolicited(&observed_reply(
        "unknown_command_reply_has_no_checksum_byte",
    ));
    let deadline = started + Duration::from_secs(2);
    while link.stats().truncated == 0 {
        assert!(
            Instant::now() < deadline,
            "the partial frame was never retired: {:?}",
            link.stats()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let elapsed = started.elapsed();

    assert!(
        elapsed >= opts.silence_before_expire,
        "retired after {elapsed:?}, before the silence window had even passed"
    );
    assert!(
        elapsed < opts.silence_before_expire + Duration::from_millis(400),
        "retired after {elapsed:?}: the write half's {:?} timeout leaked into the reader",
        opts.write_timeout
    );
    assert_eq!(link.is_down(), None);
    link.get_info(TIMEOUT).expect("and the link still works");
}
