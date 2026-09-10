//! The §9.1 packet tests. `fixtures/packets/ch9329.toml` is the authority for byte-level
//! correctness; these tests read it rather than retyping its bytes, because retyping is exactly
//! where a transcription bug enters and silently blesses a wrong encoder.
//!
//! Also carries the fixed regression corpus from §9.2 item 6, whose three cases are known real
//! traffic rather than fuzzer material.

use nanokvm::link::Reply;
use nanokvm::proto::frame::{encode, Event, Frame, Parser};
use nanokvm::proto::report::{KeyboardReport, MouseAbsReport, MouseRelReport, ABS_MAX};
use nanokvm::proto::{cmd, DeviceInfo};
use serde::Deserialize;

// -- the fixture file -------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Fixtures {
    #[serde(default)]
    packet: Vec<Entry>,
    #[serde(default)]
    response: Vec<Entry>,
}

/// One `[[packet]]` or `[[response]]` table. Field names match the file exactly; the descriptive
/// fields (`cmd_name`, `payload_meaning`, `checksum_derivation`, …) are ignored.
#[derive(Debug, Deserialize)]
struct Entry {
    name: String,
    cmd: u8,
    data: Vec<u8>,
    len: u8,
    /// Hex bytes, possibly followed by prose. Parse with [`hex_prefix`].
    frame: String,
    checksum: u8,
    observed: bool,
}

impl Entry {
    fn frame_bytes(&self) -> Vec<u8> {
        hex_prefix(&self.frame)
    }
}

/// Leading two-hex-digit tokens only, stopping at the first token that is not one. Mirrors
/// `scripts/check-fixtures.py::parse`, so a `frame` value may carry a trailing comment.
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

fn fixtures() -> Fixtures {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/packets/ch9329.toml");
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    toml::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

/// Feed `bytes` to a fresh parser and demand exactly one frame and nothing else.
fn sole_frame(bytes: &[u8]) -> Frame {
    let mut p = Parser::new();
    p.push(bytes);
    let events: Vec<Event> = p.drain_events().collect();
    assert_eq!(events.len(), 1, "expected one event, got {events:?}");
    assert_eq!(p.pending_len(), 0, "bytes left over after a complete frame");
    match events.into_iter().next() {
        Some(Event::Frame(f)) => f,
        other => panic!("expected a frame, got {other:?}"),
    }
}

#[test]
fn the_hex_reader_stops_at_prose() {
    // No fixture carries a trailing comment today, but the format permits one and
    // `scripts/check-fixtures.py` reads it this way, so the two readers must agree.
    assert_eq!(
        hex_prefix("57 AB 00 01 00 03"),
        vec![0x57, 0xAB, 0x00, 0x01, 0x00, 0x03]
    );
    assert_eq!(
        hex_prefix("57 AB 00 -- GET_INFO, 00 payload"),
        vec![0x57, 0xAB, 0x00]
    );
    assert_eq!(hex_prefix("(nothing)"), Vec::<u8>::new());
    assert_eq!(hex_prefix(""), Vec::<u8>::new());
    // A one-digit token is not two hex digits, so it terminates the run rather than widening it.
    assert_eq!(hex_prefix("57 AB 0 01"), vec![0x57, 0xAB]);
}

#[test]
fn the_fixture_file_is_not_empty() {
    let f = fixtures();
    assert!(f.packet.len() >= 14, "only {} packets", f.packet.len());
    assert!(f.response.len() >= 8, "only {} responses", f.response.len());
}

#[test]
fn the_fixture_fields_agree_with_the_fixture_frame_bytes() {
    // Not a test of our code: a guard that a hand-edit cannot drift the declared fields away from
    // the bytes and take these tests with it. `scripts/check-fixtures.py` does the same.
    let f = fixtures();
    for e in f.packet.iter().chain(f.response.iter()) {
        let bytes = e.frame_bytes();
        assert_eq!(
            bytes.len(),
            6 + e.len as usize,
            "{}: LEN disagrees with frame",
            e.name
        );
        assert_eq!(bytes[3], e.cmd, "{}: cmd field", e.name);
        assert_eq!(bytes[4], e.len, "{}: len field", e.name);
        assert_eq!(
            bytes[5..bytes.len() - 1],
            e.data[..],
            "{}: data field",
            e.name
        );
        assert_eq!(
            bytes[bytes.len() - 1],
            e.checksum,
            "{}: checksum field",
            e.name
        );
    }
}

// -- encode -----------------------------------------------------------------

#[test]
fn every_packet_encodes_to_its_fixture_frame() {
    for e in fixtures().packet {
        let got = encode(e.cmd, &e.data).unwrap_or_else(|err| panic!("{}: {err}", e.name));
        assert_eq!(
            got,
            e.frame_bytes(),
            "{} (observed on the wire: {})",
            e.name,
            e.observed
        );
    }
}

// -- parse ------------------------------------------------------------------

#[test]
fn every_packet_parses_back_to_its_command_and_payload() {
    for e in fixtures().packet {
        let f = sole_frame(&e.frame_bytes());
        assert_eq!(f.cmd, e.cmd, "{}", e.name);
        assert_eq!(f.data, e.data, "{}", e.name);
    }
}

#[test]
fn every_response_parses_and_answers_its_request() {
    for e in fixtures().response {
        let f = sole_frame(&e.frame_bytes());
        assert_eq!(f.cmd, e.cmd, "{}", e.name);
        assert_eq!(f.data, e.data, "{}", e.name);

        let request = e.cmd & 0x3F;
        let reply: Reply = f.into();
        assert!(
            reply.answers(request),
            "{}: does not answer {request:#04x}",
            e.name
        );

        let is_error = e.cmd & 0xC0 == 0xC0;
        assert_eq!(reply.is_error(), is_error, "{}", e.name);
        if is_error {
            // §3.1: every error frame ends in a non-zero code, which is why upstream's short
            // checksum discards all of them. Ours must surface the code.
            let code = reply
                .error_code()
                .unwrap_or_else(|| panic!("{}: no error code", e.name));
            assert_eq!(Some(code), e.data.first().copied(), "{}", e.name);
            assert!(
                matches!(code, 0xE4 | 0xE5),
                "{}: unexpected error code {code:#04x}",
                e.name
            );
        } else {
            assert_eq!(reply.error_code(), None, "{}", e.name);
        }
    }
}

#[test]
fn the_fixture_responses_cover_both_documented_error_codes() {
    let codes: Vec<u8> = fixtures()
        .response
        .iter()
        .filter(|e| e.cmd & 0xC0 == 0xC0)
        .filter_map(|e| e.data.first().copied())
        .collect();
    assert!(codes.contains(&0xE4), "no checksum-error fixture");
    assert!(codes.contains(&0xE5), "no parameter-error fixture");
}

// -- report builders against the fixture payloads ---------------------------

/// §9.2 item 2 and the Appendix: the mouse payloads carry a leading mode byte and are exactly 5
/// and 7 bytes. The device ACKs the short form and does nothing, so nothing but a byte-count
/// assertion in our own encoder catches a regression.
#[test]
fn every_mouse_packet_is_reproduced_by_its_report_builder() {
    let mut rel = 0;
    let mut abs = 0;
    for e in fixtures().packet {
        match e.cmd {
            cmd::SEND_MS_REL_DATA => {
                assert_eq!(
                    e.data.len(),
                    MouseRelReport::LEN,
                    "{}: payload length",
                    e.name
                );
                assert_eq!(e.data[0], MouseRelReport::MODE, "{}: mode byte", e.name);
                let report = MouseRelReport {
                    buttons: e.data[1],
                    dx: e.data[2] as i8,
                    dy: e.data[3] as i8,
                    wheel: e.data[4] as i8,
                };
                assert_eq!(report.payload().len(), 5, "{}", e.name);
                assert_eq!(report.payload()[..], e.data[..], "{}", e.name);
                rel += 1;
            }
            cmd::SEND_MS_ABS_DATA => {
                assert_eq!(
                    e.data.len(),
                    MouseAbsReport::LEN,
                    "{}: payload length",
                    e.name
                );
                assert_eq!(e.data[0], MouseAbsReport::MODE, "{}: mode byte", e.name);
                let x = u16::from_le_bytes([e.data[2], e.data[3]]);
                let y = u16::from_le_bytes([e.data[4], e.data[5]]);
                let report = MouseAbsReport {
                    buttons: e.data[1],
                    x,
                    y,
                    wheel: e.data[6] as i8,
                };
                assert_eq!(report.payload().len(), 7, "{}", e.name);
                if x <= ABS_MAX && y <= ABS_MAX {
                    assert_eq!(report.payload()[..], e.data[..], "{}", e.name);
                } else {
                    // FINDING, not a bug to paper over: `mouse_abs_max_15bit` carries x = 32767 to
                    // document that the device masks and clamps it (§3.4). Our encoder clamps at
                    // construction, so it cannot reproduce those bytes -- deliberately, since an
                    // overshoot past 8191 jumps the pointer to (0,0) on a live console. Assert
                    // the clamp instead, and that it lands where the device would put the pointer.
                    let p = report.payload();
                    assert_eq!(
                        u16::from_le_bytes([p[2], p[3]]),
                        x.min(ABS_MAX),
                        "{}",
                        e.name
                    );
                    assert_eq!(
                        u16::from_le_bytes([p[4], p[5]]),
                        y.min(ABS_MAX),
                        "{}",
                        e.name
                    );
                    assert_eq!(
                        (x & 0x1FFF).min(ABS_MAX),
                        ABS_MAX,
                        "{}: fixture premise",
                        e.name
                    );
                }
                abs += 1;
            }
            _ => {}
        }
    }
    assert!(
        rel >= 5 && abs >= 5,
        "expected mouse fixtures, saw {rel} rel and {abs} abs"
    );
}

#[test]
fn every_keyboard_packet_is_reproduced_by_its_report_builder() {
    let mut seen = 0;
    for e in fixtures().packet {
        if e.cmd != cmd::SEND_KB_GENERAL_DATA {
            continue;
        }
        assert_eq!(
            e.data.len(),
            KeyboardReport::LEN,
            "{}: payload length",
            e.name
        );
        let mut keys = [0u8; 6];
        keys.copy_from_slice(&e.data[2..8]);
        let report = KeyboardReport {
            modifiers: e.data[0],
            keys,
        };
        assert_eq!(report.payload()[..], e.data[..], "{}", e.name);
        seen += 1;
    }
    assert!(seen >= 3, "expected keyboard fixtures, saw {seen}");
}

#[test]
fn the_release_all_report_is_the_release_all_fixture() {
    let f = fixtures();
    let kb = f
        .packet
        .iter()
        .find(|e| e.name == "kb_release_all")
        .expect("kb_release_all");
    assert_eq!(KeyboardReport::RELEASE_ALL.payload()[..], kb.data[..]);
    let ms = f
        .packet
        .iter()
        .find(|e| e.name == "mouse_rel_idle")
        .expect("mouse_rel_idle");
    assert_eq!(
        nanokvm::proto::report::MOUSE_RELEASE_ALL.payload()[..],
        ms.data[..]
    );
}

#[test]
fn the_get_info_reply_fixture_parses_into_device_info() {
    let f = fixtures();
    let reply = f
        .response
        .iter()
        .find(|e| e.name == "get_info_reply")
        .expect("get_info_reply");
    let info = DeviceInfo::parse(&reply.data).expect("8-byte payload");
    assert!((info.version - 1.8).abs() < 1e-6, "{}", info.version);
    assert!(info.target_connected);
    assert!(!info.caps_lock);

    let caps = f
        .response
        .iter()
        .find(|e| e.name == "get_info_reply_caps_on")
        .expect("get_info_reply_caps_on");
    let info = DeviceInfo::parse(&caps.data).expect("8-byte payload");
    assert!(info.caps_lock, "lockBits 0x02 is CapsLock");
    assert!(!info.num_lock && !info.scroll_lock);
}

// -- regression corpus, §9.2 item 6 -----------------------------------------

/// §3.2. Real traffic: the reply to an undefined command is five bytes with no checksum byte.
/// Standalone it is undecidable until the caller's read timeout fires, at which point it retires
/// as `Truncated`. A parser that indexes `frame[5 + len]` panics here instead.
#[test]
fn regression_s3_2_five_byte_reply_with_no_checksum_byte() {
    let bytes = [0x57, 0xAB, 0x00, 0xFE, 0x00];
    let mut p = Parser::new();
    p.push(&bytes);
    assert_eq!(p.next_event(), None);
    assert_eq!(p.pending_len(), 5);
    assert_eq!(
        p.expire_partial(),
        Some(Event::Truncated {
            raw: bytes.to_vec()
        })
    );
    assert_eq!(p.pending_len(), 0);
}

/// §3.2 again, in the shape that actually costs a reply: the malformed frame immediately followed
/// by a real one. The next frame's `0x57` is consumed as the missing checksum byte, so byte-wise
/// resync is the only thing that gets the following reply back.
#[test]
fn regression_s3_2_no_checksum_reply_does_not_eat_the_next_reply() {
    let reply = fixtures()
        .response
        .into_iter()
        .find(|e| e.name == "get_info_reply")
        .expect("get_info_reply");
    let mut bytes = vec![0x57, 0xAB, 0x00, 0xFE, 0x00];
    bytes.extend_from_slice(&reply.frame_bytes());

    let mut p = Parser::new();
    p.push(&bytes);
    let events: Vec<Event> = p.drain_events().collect();

    assert!(
        matches!(events.first(), Some(Event::BadChecksum { raw }) if raw == &bytes[..6]),
        "expected BadChecksum over the malformed frame plus the stolen 0x57, got {events:?}"
    );
    match events.last() {
        Some(Event::Frame(f)) => {
            assert_eq!(f.cmd, reply.cmd);
            assert_eq!(f.data, reply.data);
        }
        other => panic!("the following reply must survive, got {other:?}"),
    }
    assert_eq!(
        events.len(),
        2,
        "no garbage should be reported twice: {events:?}"
    );
}

/// §3.1. Upstream's short receive checksum rejects every error frame, so it cannot tell a reported
/// device error from silence. Ours parses them and hands the code up.
#[test]
fn regression_s3_1_error_frames_are_surfaced_not_dropped() {
    for e in fixtures()
        .response
        .into_iter()
        .filter(|e| e.cmd & 0xC0 == 0xC0)
    {
        let bytes = e.frame_bytes();
        let reply: Reply = sole_frame(&bytes).into();
        assert!(reply.is_error(), "{}", e.name);
        assert_eq!(reply.error_code(), e.data.first().copied(), "{}", e.name);
        assert!(reply.answers(e.cmd & 0x3F), "{}", e.name);

        // The bug itself: summing one byte short excludes the error code and fails to match.
        let upstream_sum: u32 = bytes[..bytes.len() - 2].iter().map(|b| u32::from(*b)).sum();
        assert_ne!(
            (upstream_sum & 0xFF) as u8,
            bytes[bytes.len() - 1],
            "{}: this fixture no longer demonstrates the §3.1 rejection",
            e.name
        );
    }
}

/// Appendix. The device pushes an unsolicited `0x81` lock-state frame about 15 ms after a lock-key
/// change, and it can arrive *after* an unrelated acknowledgement. The parser must produce it as
/// its own frame in stream order and leave the matching to the caller's command byte.
#[test]
fn regression_appendix_unsolicited_frame_between_other_frames() {
    let f = fixtures();
    let find = |name: &str| {
        f.response
            .iter()
            .chain(f.packet.iter())
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("fixture {name}"))
            .frame_bytes()
    };

    // A keyboard ack, then the unsolicited lock-state push, then the ack for the mouse report the
    // writer sent in between.
    let mut stream = find("kb_ack");
    stream.extend_from_slice(&find("get_info_reply_caps_on"));
    stream.extend_from_slice(&find("mouse_rel_ack"));

    let mut p = Parser::new();
    // Split at a boundary that falls inside the unsolicited frame.
    p.push(&stream[..10]);
    p.push(&stream[10..]);
    let frames: Vec<Frame> = p
        .drain_events()
        .map(|e| match e {
            Event::Frame(f) => f,
            other => panic!("unexpected {other:?}"),
        })
        .collect();

    let cmds: Vec<u8> = frames.iter().map(|f| f.cmd).collect();
    assert_eq!(
        cmds,
        vec![0x82, 0x81, 0x85],
        "frames must come out in stream order"
    );

    // Matching is by command byte, never by arrival order: the mouse reply is the third frame.
    let mouse = frames
        .into_iter()
        .map(Reply::from)
        .find(|r| r.answers(cmd::SEND_MS_REL_DATA))
        .expect("the SEND_MS_REL_DATA reply must be findable by command byte");
    assert!(!mouse.is_error());
}
