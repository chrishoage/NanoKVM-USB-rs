//! Property tests for the pure `proto` module (§9.3: "`proto` is I/O-free, so it takes property
//! tests and a fuzzer directly").
//!
//! The parser reads bytes off hardware that is already known to emit frames its own documented
//! format forbids (§3.2), so the load-bearing property is the negative one: **no input panics**.

use nanokvm::proto::frame::{encode, Event, Frame, Parser, MAX_FRAME, MAX_PAYLOAD};
use nanokvm::proto::report::{MouseAbsReport, ABS_MAX};
use proptest::prelude::*;

/// Split `bytes` at the given boundaries and push each piece separately, so a property holds for
/// every read boundary rather than only for whole-frame reads (§9.2 item 6).
fn push_in_chunks(parser: &mut Parser, bytes: &[u8], chunk_sizes: &[usize]) {
    let mut rest = bytes;
    let mut sizes = chunk_sizes.iter().copied().cycle();
    while !rest.is_empty() {
        let n = sizes.next().unwrap_or(1).clamp(1, rest.len());
        let (head, tail) = rest.split_at(n);
        parser.push(head);
        rest = tail;
    }
}

fn frames_of(events: Vec<Event>) -> Vec<Frame> {
    events
        .into_iter()
        .filter_map(|e| match e {
            Event::Frame(f) => Some(f),
            _ => None,
        })
        .collect()
}

proptest! {
    /// (a) `parse(encode(cmd, payload)) == (cmd, payload)` for every command byte and every legal
    /// payload length.
    #[test]
    fn encode_then_parse_is_the_identity(
        cmd in any::<u8>(),
        payload in proptest::collection::vec(any::<u8>(), 0..=MAX_PAYLOAD),
    ) {
        let bytes = encode(cmd, &payload).expect("payload length is within range");
        prop_assert_eq!(bytes.len(), 6 + payload.len());

        let mut parser = Parser::new();
        parser.push(&bytes);
        let events: Vec<Event> = parser.drain_events().collect();
        prop_assert_eq!(events.len(), 1, "{:?}", events);
        prop_assert_eq!(parser.pending_len(), 0);
        match events.into_iter().next() {
            Some(Event::Frame(f)) => {
                prop_assert_eq!(f.cmd, cmd);
                prop_assert_eq!(f.data, payload);
            }
            other => prop_assert!(false, "expected a frame, got {:?}", other),
        }
    }

    /// (b) N frames concatenated and pushed in arbitrary chunk sizes come out as exactly those N
    /// frames, in order. Payloads may themselves contain `57 AB`; a frame is delimited by `LEN`,
    /// not by hunting for the next header.
    #[test]
    fn a_run_of_frames_survives_arbitrary_read_boundaries(
        specs in proptest::collection::vec(
            (any::<u8>(), proptest::collection::vec(any::<u8>(), 0..40)),
            1..12,
        ),
        chunk_sizes in proptest::collection::vec(1usize..64, 1..8),
    ) {
        let mut stream = Vec::new();
        for (cmd, payload) in &specs {
            stream.extend_from_slice(&encode(*cmd, payload).expect("in range"));
        }

        let mut parser = Parser::new();
        push_in_chunks(&mut parser, &stream, &chunk_sizes);
        let events: Vec<Event> = parser.drain_events().collect();

        prop_assert_eq!(parser.pending_len(), 0, "stream ended on a frame boundary");
        prop_assert_eq!(events.len(), specs.len(), "{:?}", events);
        for (event, (cmd, payload)) in events.into_iter().zip(specs) {
            match event {
                Event::Frame(f) => {
                    prop_assert_eq!(f.cmd, cmd);
                    prop_assert_eq!(f.data, payload);
                }
                other => prop_assert!(false, "expected a frame, got {:?}", other),
            }
        }
    }

    /// (c) Arbitrary bytes at arbitrary read boundaries never panic, and every frame the parser
    /// claims to have found really is a byte string that appeared in the input.
    #[test]
    fn arbitrary_bytes_never_panic_and_never_invent_a_frame(
        bytes in proptest::collection::vec(any::<u8>(), 0..600),
        chunk_sizes in proptest::collection::vec(1usize..40, 1..8),
    ) {
        let mut parser = Parser::new();
        push_in_chunks(&mut parser, &bytes, &chunk_sizes);
        let events: Vec<Event> = parser.drain_events().collect();

        for frame in frames_of(events) {
            let reencoded = frame.encode().expect("a parsed frame always re-encodes");
            prop_assert!(
                bytes.windows(reencoded.len()).any(|w| w == reencoded),
                "parser produced {:?}, which is not in the input",
                reencoded
            );
        }

        // Whatever the input, at most one incomplete frame is ever held back.
        prop_assert!(parser.pending_len() <= MAX_FRAME, "{}", parser.pending_len());
        // Expiring must not panic either, and must leave the buffer empty.
        let _ = parser.expire_partial();
        prop_assert_eq!(parser.pending_len(), 0);
    }

    /// (c) again, biased towards traffic that looks like frames: headers, plausible lengths and
    /// truncated tails, which is what a resynchronising parser actually meets on this device.
    #[test]
    fn frame_shaped_garbage_never_panics(
        pieces in proptest::collection::vec(
            prop_oneof![
                Just(vec![0x57u8, 0xAB]),
                Just(vec![0x57u8]),
                // The §3.2 five-byte reply with no checksum byte.
                Just(vec![0x57u8, 0xAB, 0x00, 0xFE, 0x00]),
                proptest::collection::vec(any::<u8>(), 0..6),
                (any::<u8>(), proptest::collection::vec(any::<u8>(), 0..20))
                    .prop_map(|(c, p)| encode(c, &p).expect("in range")),
            ],
            0..24,
        ),
        chunk_sizes in proptest::collection::vec(1usize..9, 1..6),
    ) {
        let stream: Vec<u8> = pieces.concat();
        let mut parser = Parser::new();
        push_in_chunks(&mut parser, &stream, &chunk_sizes);
        let events: Vec<Event> = parser.drain_events().collect();
        for frame in frames_of(events) {
            let reencoded = frame.encode().expect("a parsed frame always re-encodes");
            prop_assert!(stream.windows(reencoded.len()).any(|w| w == reencoded));
        }
        let _ = parser.expire_partial();
        prop_assert_eq!(parser.pending_len(), 0);
    }

    /// Every byte the parser is given is covered by at least one event: it is part of a frame,
    /// part of a rejected candidate, part of an expired partial, or counted as garbage. Nothing
    /// vanishes quietly, which is the failure §3.1 describes upstream having.
    ///
    /// Coverage rather than a partition, because byte-wise resync overlaps deliberately: the bytes
    /// after a rejected candidate's first header byte appear in its `raw` *and* are handed back to
    /// the scan, where they may become a frame.
    #[test]
    fn every_byte_is_covered_by_some_event(
        bytes in proptest::collection::vec(
            prop_oneof![Just(0x57u8), Just(0xABu8), Just(0x00u8), any::<u8>()],
            0..400,
        ),
    ) {
        let mut parser = Parser::new();
        parser.push(&bytes);
        let mut events: Vec<Event> = parser.drain_events().collect();
        events.extend(parser.expire_partial());

        let mut covered = 0usize;
        for event in events {
            covered += match event {
                Event::Frame(f) => 6 + f.data.len(),
                Event::BadChecksum { raw } | Event::Truncated { raw } => raw.len(),
                Event::Garbage { bytes } => bytes,
            };
        }
        prop_assert!(covered >= bytes.len(), "covered {} of {} bytes", covered, bytes.len());
    }

    /// (d) Whatever the caller puts in the struct, the payload on the wire carries coordinates
    /// within the 12 usable bits. Overshooting past 8191 wraps the pointer to the top-left corner
    /// of a live console, so the encoder clamps and never relies on the device (§3.4).
    #[test]
    fn an_absolute_report_never_emits_a_coordinate_above_full_scale(
        buttons in any::<u8>(),
        x in any::<u16>(),
        y in any::<u16>(),
        wheel in any::<i8>(),
    ) {
        let payload = MouseAbsReport { buttons, x, y, wheel }.payload();
        prop_assert_eq!(payload.len(), 7);
        prop_assert_eq!(payload[0], MouseAbsReport::MODE);
        prop_assert_eq!(payload[1], buttons);
        let sent_x = u16::from_le_bytes([payload[2], payload[3]]);
        let sent_y = u16::from_le_bytes([payload[4], payload[5]]);
        prop_assert!(sent_x <= ABS_MAX, "x = {}", sent_x);
        prop_assert!(sent_y <= ABS_MAX, "y = {}", sent_y);
        prop_assert_eq!(sent_x, x.min(ABS_MAX));
        prop_assert_eq!(sent_y, y.min(ABS_MAX));
    }

    /// The checksum covers the whole frame, so flipping any single byte is detected (§3.1).
    #[test]
    fn corrupting_any_byte_fails_the_checksum(
        cmd in any::<u8>(),
        payload in proptest::collection::vec(any::<u8>(), 0..32),
        index in 0usize..38,
        delta in 1u8..=255,
    ) {
        let mut bytes = encode(cmd, &payload).expect("in range");
        let index = index % bytes.len();
        bytes[index] = bytes[index].wrapping_add(delta);

        let mut parser = Parser::new();
        parser.push(&bytes);
        let events: Vec<Event> = parser.drain_events().collect();
        // A corrupted header byte turns the frame into garbage rather than a bad checksum, and a
        // corrupted LEN leaves an incomplete frame pending; neither may yield an accepted frame
        // with the original contents.
        for frame in frames_of(events) {
            prop_assert!(
                frame.cmd != cmd || frame.data != payload,
                "corrupting byte {} was not detected",
                index
            );
        }
    }
}
