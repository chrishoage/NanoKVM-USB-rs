//! Framing: encode, streaming parse with resynchronisation. Pure; no I/O, no clock (§4).
//!
//! The wire format is (Appendix):
//!
//! ```text
//! HEAD1(0x57) HEAD2(0xAB) ADDR CMD LEN DATA[LEN] SUM
//! SUM = (HEAD1 + HEAD2 + ADDR + CMD + LEN + sum(DATA)) & 0xFF
//! ```
//!
//! Encoding is the easy half. Decoding is the module's reason to exist, because this device does
//! not always send well-formed frames:
//!
//! - it answers an undefined command with a **five-byte frame carrying no checksum byte at all**
//!   (§3.2, fixture `[[disagreement]] unknown_command_reply_has_no_checksum_byte`). A parser that
//!   indexes `frame[5 + len]` panics on real traffic;
//! - it emits **error frames** `CMD | 0xC0` whose payload is not zero-terminated, which upstream's
//!   off-by-one receive checksum discards entirely (§3.1). Ours surfaces them;
//! - it pushes **unsolicited `0x81` frames** between a request and its reply (Appendix), so the
//!   parser must simply produce frames in order and let the caller match by command byte.
//!
//! Because of the first item, the parser resynchronises **byte-wise**: after a checksum mismatch it
//! resumes scanning at the byte *after* the failed frame's first header byte, never after the whole
//! failed frame. See [`Parser`] for why that is the difference between recovering the next reply
//! and eating it.

use std::fmt;

use crate::link::Reply;

/// Frame header, both bytes (Appendix).
pub const HEAD: [u8; 2] = [0x57, 0xAB];

/// The only device address this client uses; the fixtures are all `ADDR = 0x00` (Appendix).
pub const ADDR: u8 = 0x00;

/// Largest payload a frame can carry, because `LEN` is one byte (Appendix).
pub const MAX_PAYLOAD: usize = 255;

/// Bytes of overhead around a payload: `HEAD1 HEAD2 ADDR CMD LEN … SUM`.
pub const OVERHEAD: usize = 6;

/// Largest complete frame: [`OVERHEAD`] plus [`MAX_PAYLOAD`].
pub const MAX_FRAME: usize = OVERHEAD + MAX_PAYLOAD;

/// Cap on the parser's reassembly buffer. A complete frame is at most [`MAX_FRAME`] bytes, so this
/// is slack rather than a working limit; it exists so that a stream which never contains a header
/// cannot grow the buffer without bound (§3.2).
pub const MAX_BUFFER: usize = 4096;

/// Encoding refused. The frame is never truncated to fit — a short write on this device consumes
/// the *following* command as payload, because its receive parser has no inter-byte timeout
/// (fixture `[[disagreement]] device_receive_parser_has_no_inter_byte_timeout`, §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    /// The payload does not fit in the one-byte `LEN` field.
    #[error("payload is {len} bytes, maximum is {MAX_PAYLOAD}")]
    PayloadTooLong { len: usize },
}

/// A reply payload was too short to hold the fields it is defined to carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// Fewer payload bytes than the reply's layout requires. Device-supplied lengths are never
    /// trusted; this is returned instead of indexing past the end (§3.2).
    #[error("payload is {got} bytes, need at least {need}")]
    ShortPayload { need: usize, got: usize },
}

/// The datasheet **transmit** checksum, which is also the correct receive checksum (Appendix).
///
/// `SUM = (HEAD1 + HEAD2 + ADDR + CMD + LEN + sum(DATA)) & 0xFF`. Upstream's `decode()` sums one
/// byte fewer, so it accepts a corrupt final payload byte and rejects every error frame (§3.1);
/// this covers the whole frame.
///
/// A `payload` longer than [`MAX_PAYLOAD`] cannot be framed at all — [`encode`] rejects it — so
/// the `LEN` term here is the low byte of the length.
pub fn checksum(addr: u8, cmd: u8, payload: &[u8]) -> u8 {
    let len = payload.len() as u8;
    let mut sum = HEAD[0] as u32 + HEAD[1] as u32 + addr as u32 + cmd as u32 + len as u32;
    for b in payload {
        sum += *b as u32;
    }
    (sum & 0xFF) as u8
}

/// The same checksum computed over an already-assembled frame's bytes, i.e. everything up to but
/// not including the `SUM` byte. Used on the receive side, where `LEN` is the device's claim.
fn checksum_over(bytes: &[u8]) -> u8 {
    let mut sum: u32 = 0;
    for b in bytes {
        sum += *b as u32;
    }
    (sum & 0xFF) as u8
}

/// Encode one complete frame for `cmd` with `payload`, addressed to [`ADDR`] (Appendix).
///
/// Errors rather than truncates: see [`EncodeError`].
pub fn encode(cmd: u8, payload: &[u8]) -> Result<Vec<u8>, EncodeError> {
    let mut buf = Vec::with_capacity(OVERHEAD + payload.len());
    encode_into(&mut buf, cmd, payload)?;
    Ok(buf)
}

/// [`encode`] into a caller-owned buffer, appending to whatever is already there.
///
/// On error `buf` is left exactly as it was found, so a rejected payload cannot leave a partial
/// frame queued for the transport (§5.1).
pub fn encode_into(buf: &mut Vec<u8>, cmd: u8, payload: &[u8]) -> Result<(), EncodeError> {
    if payload.len() > MAX_PAYLOAD {
        return Err(EncodeError::PayloadTooLong { len: payload.len() });
    }
    buf.reserve(OVERHEAD + payload.len());
    buf.extend_from_slice(&HEAD);
    buf.push(ADDR);
    buf.push(cmd);
    buf.push(payload.len() as u8);
    buf.extend_from_slice(payload);
    buf.push(checksum(ADDR, cmd, payload));
    Ok(())
}

/// One well-formed frame: header stripped, `LEN` consumed, checksum verified (Appendix).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub addr: u8,
    pub cmd: u8,
    pub data: Vec<u8>,
}

impl Frame {
    /// Re-encode this frame to the bytes it was parsed from.
    ///
    /// Only fails if `data` exceeds [`MAX_PAYLOAD`], which a parsed frame never does.
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        if self.data.len() > MAX_PAYLOAD {
            return Err(EncodeError::PayloadTooLong {
                len: self.data.len(),
            });
        }
        let mut buf = Vec::with_capacity(OVERHEAD + self.data.len());
        buf.extend_from_slice(&HEAD);
        buf.push(self.addr);
        buf.push(self.cmd);
        buf.push(self.data.len() as u8);
        buf.extend_from_slice(&self.data);
        buf.push(checksum(self.addr, self.cmd, &self.data));
        Ok(buf)
    }

    /// View this frame as a [`Reply`] for the `serial` reader to match by command byte (Appendix).
    /// `addr` is dropped: a reply's identity is its command byte, never its arrival order.
    pub fn into_reply(self) -> Reply {
        Reply {
            cmd: self.cmd,
            data: self.data,
        }
    }
}

impl From<Frame> for Reply {
    fn from(f: Frame) -> Reply {
        f.into_reply()
    }
}

/// What the [`Parser`] found in the byte stream. Everything the parser discards is reported;
/// nothing is dropped silently, because silent drops are how upstream lost every error frame
/// (§3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A complete frame whose checksum verified.
    Frame(Frame),
    /// `LEN` payload bytes plus a checksum byte arrived and the checksum did not match. `raw` is
    /// the whole rejected candidate, header byte through claimed checksum byte. Scanning resumes
    /// one byte into `raw`, so `raw` may include bytes belonging to the next frame — see
    /// [`Parser`].
    BadChecksum { raw: Vec<u8> },
    /// [`Parser::expire_partial`] was called with an incomplete frame in the buffer: the caller's
    /// read timeout fired and the missing bytes are not coming. This is the §3.2 five-byte reply.
    Truncated { raw: Vec<u8> },
    /// Bytes discarded while hunting for a header, coalesced. Bytes already reported inside an
    /// [`Event::BadChecksum`] are not counted again.
    Garbage { bytes: usize },
}

/// Streaming frame parser: bytes in, [`Event`]s out. Pure — it owns no clock, no I/O and no
/// timeout, so the caller drives expiry with [`Parser::expire_partial`] (§3.2, §4).
///
/// # Resynchronisation
///
/// The device answers an undefined command with `57 AB 00 FE 00` — five bytes, `LEN 0`, **no
/// checksum byte**, confirmed over a ten-second wait (§3.2). Feed that reply followed immediately
/// by a real one and a naive parser reads the next frame's `0x57` as the missing checksum byte,
/// rejects the candidate, and resumes *after* it — losing the next reply entirely.
///
/// So on a checksum mismatch this parser emits [`Event::BadChecksum`] and resumes scanning at the
/// byte **after the failed candidate's first header byte**, not after the whole candidate. The
/// stolen `0x57` is handed back to the scan and the following reply parses normally. The bytes in
/// between are not re-reported as [`Event::Garbage`]; they already appeared in `raw`.
///
/// # Ordering
///
/// Events come out in stream order. The parser never matches requests to replies — the device
/// pushes unsolicited `0x81` frames between a request and its answer, so matching is the caller's
/// job and is done by command byte (Appendix).
///
/// # Bounds
///
/// The reassembly buffer holds at most one incomplete frame ([`MAX_FRAME`] bytes) plus, briefly, a
/// chunk of the current `push`. If it ever exceeds [`MAX_BUFFER`] the contents are discarded as
/// [`Event::Garbage`]. Growth is bounded for any input, and no input panics.
#[derive(Debug, Default)]
pub struct Parser {
    buf: Vec<u8>,
    events: std::collections::VecDeque<Event>,
    /// Garbage bytes seen since the last event, awaiting coalescing.
    pending_garbage: usize,
    /// Bytes still to be re-scanned that were already reported inside an [`Event::BadChecksum`]
    /// `raw`. Discarding them must not count them a second time.
    suppressed: usize,
}

impl Parser {
    /// A parser with an empty buffer and no queued events.
    pub fn new() -> Parser {
        Parser::default()
    }

    /// Feed bytes read from the transport. Any complete frames become queued [`Event`]s.
    ///
    /// The input is consumed in [`MAX_BUFFER`]-sized chunks, so one enormous read cannot inflate
    /// the reassembly buffer.
    pub fn push(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(MAX_BUFFER) {
            self.buf.extend_from_slice(chunk);
            self.scan();
            self.enforce_bound();
        }
    }

    /// Take the next queued event, oldest first.
    pub fn next_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// Take every queued event, oldest first. Equivalent to looping on [`Parser::next_event`].
    pub fn drain_events(&mut self) -> std::collections::vec_deque::Drain<'_, Event> {
        self.events.drain(..)
    }

    /// The caller's read timeout fired while a frame was still incomplete: give up on it.
    ///
    /// Returns [`Event::Truncated`] carrying the abandoned bytes and empties the buffer, or `None`
    /// if nothing was pending. This is the only way the §3.2 five-byte no-checksum reply is
    /// retired when nothing follows it: the parser has no clock, and would otherwise wait forever
    /// for a sixth byte the fixture proves never arrives.
    ///
    /// The returned event is *not* queued. Events already queued by [`Parser::push`] are older and
    /// remain available from [`Parser::next_event`].
    pub fn expire_partial(&mut self) -> Option<Event> {
        if self.buf.is_empty() {
            return None;
        }
        let raw = std::mem::take(&mut self.buf);
        self.suppressed = 0;
        Some(Event::Truncated { raw })
    }

    /// Bytes held in the reassembly buffer: an incomplete frame, or a lone `0x57` that may yet
    /// become a header. Zero means the stream is at a frame boundary.
    pub fn pending_len(&self) -> usize {
        self.buf.len()
    }

    /// Drop everything: buffered bytes, queued events and pending garbage. For reconnect (§2.7),
    /// where nothing from the old connection may be carried across.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.events.clear();
        self.pending_garbage = 0;
        self.suppressed = 0;
    }

    // -- internals ---------------------------------------------------------

    /// Pull as many frames out of the buffer as it currently holds.
    fn scan(&mut self) {
        while self.align() {
            // `align` returning true guarantees a header at index 0, so `LEN` at index 4 is the
            // only byte still in question.
            if self.buf.len() < 5 {
                break;
            }
            let len = self.buf[4] as usize;
            let total = OVERHEAD + len;
            if self.buf.len() < total {
                break;
            }
            // `total >= OVERHEAD` and `self.buf.len() >= total`: every index below is in bounds.
            if checksum_over(&self.buf[..total - 1]) == self.buf[total - 1] {
                let frame = Frame {
                    addr: self.buf[2],
                    cmd: self.buf[3],
                    data: self.buf[5..5 + len].to_vec(),
                };
                self.flush_garbage();
                self.events.push_back(Event::Frame(frame));
                self.consume(total);
            } else {
                let raw = self.buf[..total].to_vec();
                self.flush_garbage();
                self.events.push_back(Event::BadChecksum { raw });
                // Byte-wise resync (§3.2): hand every byte after the first header byte back to the
                // scan, and do not charge them to `Garbage` a second time.
                self.consume(1);
                self.suppressed = total - 1;
            }
        }
        self.flush_garbage();
    }

    /// Discard leading bytes until the buffer starts with [`HEAD`]. Returns whether it does; a
    /// `false` return means more bytes are needed and the scan must stop.
    fn align(&mut self) -> bool {
        if self.buf.len() >= 2 && self.buf[0] == HEAD[0] && self.buf[1] == HEAD[1] {
            return true;
        }
        match self.buf.windows(2).position(|w| w == HEAD) {
            Some(i) => {
                self.discard(i);
                true
            }
            None => {
                // A header can only start where a 0x57 already sits, so everything except a
                // trailing 0x57 is provably garbage and can go now.
                let keep = usize::from(self.buf.last() == Some(&HEAD[0]));
                self.discard(self.buf.len() - keep);
                false
            }
        }
    }

    /// Drop `n` leading bytes as garbage, honouring the `suppressed` accounting.
    fn discard(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        let already_reported = n.min(self.suppressed);
        self.suppressed -= already_reported;
        self.pending_garbage += n - already_reported;
        self.buf.drain(..n);
    }

    /// Drop `n` leading bytes that an emitted event already accounted for.
    fn consume(&mut self, n: usize) {
        self.buf.drain(..n);
        self.suppressed = self.suppressed.saturating_sub(n);
    }

    fn flush_garbage(&mut self) {
        if self.pending_garbage > 0 {
            self.events.push_back(Event::Garbage {
                bytes: self.pending_garbage,
            });
            self.pending_garbage = 0;
        }
    }

    /// Backstop for the buffer bound. Unreachable in practice — `scan` leaves at most one
    /// incomplete frame behind — but it makes "never grows without bound" true by construction.
    fn enforce_bound(&mut self) {
        if self.buf.len() > MAX_BUFFER {
            let n = self.buf.len();
            self.buf.clear();
            let already_reported = n.min(self.suppressed);
            self.suppressed = 0;
            self.pending_garbage += n - already_reported;
            self.flush_garbage();
        }
    }
}

/// The `GET_INFO` (`0x01`) reply payload, `[versionChar, connectedFlag, lockBits, …]` (Appendix).
///
/// The lock bits originate in the *target's* HID output report, so a change in them is proof the
/// target processed a keystroke — the one end-to-end check that needs no video (Appendix).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeviceInfo {
    /// `1.0 + (data[0] - 0x30) / 10`. This unit reports `0x38` → 1.8 (Appendix).
    pub version: f32,
    /// The target has enumerated the emulated HID.
    pub target_connected: bool,
    pub num_lock: bool,
    pub caps_lock: bool,
    pub scroll_lock: bool,
}

impl DeviceInfo {
    /// Payload bytes required before any field can be read.
    pub const MIN_PAYLOAD: usize = 3;

    /// Parse a `GET_INFO` reply payload (Appendix).
    ///
    /// A payload shorter than [`DeviceInfo::MIN_PAYLOAD`] is [`ParseError::ShortPayload`], never a
    /// panic: `LEN` is the device's claim, and this device is already known to send frames its own
    /// documented format forbids (§3.2). Trailing bytes are ignored — the observed reply carries
    /// five zero bytes after the lock bits and none of them is documented.
    ///
    /// A version character below `'0'` saturates to 1.0 rather than wrapping.
    pub fn parse(payload: &[u8]) -> Result<DeviceInfo, ParseError> {
        let fields = (payload.first(), payload.get(1), payload.get(2));
        let (&version_char, &connected, &locks) = match fields {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => {
                return Err(ParseError::ShortPayload {
                    need: DeviceInfo::MIN_PAYLOAD,
                    got: payload.len(),
                })
            }
        };
        Ok(DeviceInfo {
            version: 1.0 + f32::from(version_char.saturating_sub(b'0')) / 10.0,
            target_connected: connected != 0,
            num_lock: locks & 0b001 != 0,
            caps_lock: locks & 0b010 != 0,
            scroll_lock: locks & 0b100 != 0,
        })
    }
}

/// The reply as one line, in one wording for every command that prints it.
///
/// `key`, `type` and the viewer's startup line used to print `caps=true` while `devices --probe`
/// printed `caps=on` for the same bit, which reads as two different facts about the target. The
/// lock bits are *states of the target's keyboard* (Appendix) and `on`/`off` is what a user checks
/// them against, so that is the spelling, and it lives here — next to the type — rather than in
/// each caller.
impl fmt::Display for DeviceInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CH9329 firmware {:.1}, target {}, locks: num={} caps={} scroll={}",
            self.version,
            if self.target_connected {
                "connected"
            } else {
                "NOT connected"
            },
            on_off(self.num_lock),
            on_off(self.caps_lock),
            on_off(self.scroll_lock),
        )
    }
}

impl DeviceInfo {
    /// Just the three lock bits, `num=… caps=… scroll=…`.
    ///
    /// For the one caller that has a `0x81` frame but not a *query* answer: the device pushes an
    /// unsolicited lock-state frame after a lock-key change (A12), and reporting its version and
    /// target flag as if they had just been asked for would be claiming more than was observed
    /// (§3.4). The spelling is the same one [`DeviceInfo`]'s `Display` uses.
    pub fn locks(&self) -> String {
        format!(
            "num={} caps={} scroll={}",
            on_off(self.num_lock),
            on_off(self.caps_lock),
            on_off(self.scroll_lock)
        )
    }
}

fn on_off(bit: bool) -> &'static str {
    if bit {
        "on"
    } else {
        "off"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The observed `GET_INFO` reply (Appendix, fixture `get_info_reply`). Retyped here only
    /// because these are unit tests of the parser's mechanics; the byte-level authority tests live
    /// in `tests/proto_fixtures.rs` and read the fixture file (§9.1).
    const GET_INFO_REPLY: [u8; 14] = [
        0x57, 0xAB, 0x00, 0x81, 0x08, 0x38, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC4,
    ];
    /// The §3.2 malformed reply: five bytes, no checksum.
    const NO_CHECKSUM_REPLY: [u8; 5] = [0x57, 0xAB, 0x00, 0xFE, 0x00];

    fn events(bytes: &[u8]) -> Vec<Event> {
        let mut p = Parser::new();
        p.push(bytes);
        p.drain_events().collect()
    }

    #[test]
    fn checksum_matches_the_datasheet_constant() {
        // 0x57 + 0xAB + 0x00 = 0x102, so GET_INFO with no payload sums to 0x103 & 0xFF.
        assert_eq!(checksum(ADDR, 0x01, &[]), 0x03);
    }

    #[test]
    fn encode_round_trips_through_the_parser() {
        let bytes = encode(0x02, &[0x00; 8]).expect("8-byte payload fits");
        let want = Event::Frame(Frame {
            addr: 0x00,
            cmd: 0x02,
            data: vec![0; 8],
        });
        assert_eq!(events(&bytes), vec![want]);
    }

    #[test]
    fn encode_rejects_an_oversized_payload_instead_of_truncating() {
        let too_long = vec![0u8; MAX_PAYLOAD + 1];
        assert_eq!(
            encode(0x02, &too_long),
            Err(EncodeError::PayloadTooLong { len: 256 })
        );
        let mut buf = vec![0xAA];
        assert!(encode_into(&mut buf, 0x02, &too_long).is_err());
        assert_eq!(
            buf,
            vec![0xAA],
            "a rejected payload must leave the buffer untouched"
        );
    }

    #[test]
    fn a_frame_split_across_reads_is_reassembled() {
        let mut p = Parser::new();
        for byte in GET_INFO_REPLY {
            p.push(&[byte]);
        }
        let got: Vec<Event> = p.drain_events().collect();
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(matches!(&got[0], Event::Frame(f) if f.cmd == 0x81));
    }

    #[test]
    fn leading_garbage_is_reported_and_coalesced() {
        let mut bytes = vec![0xDE, 0xAD, 0xBE, 0xEF];
        bytes.extend_from_slice(&GET_INFO_REPLY);
        let got = events(&bytes);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0], Event::Garbage { bytes: 4 });
        assert!(matches!(&got[1], Event::Frame(f) if f.cmd == 0x81));
    }

    #[test]
    fn a_bad_checksum_is_reported_not_dropped() {
        let mut bytes = GET_INFO_REPLY;
        bytes[13] ^= 0xFF;
        let got = events(&bytes);
        assert_eq!(
            got[0],
            Event::BadChecksum {
                raw: bytes.to_vec()
            }
        );
    }

    /// §3.2. The device's reply to an undefined command has no checksum byte. Followed immediately
    /// by a real reply, the missing byte is supplied by the next frame's `0x57`, the candidate
    /// fails its checksum, and byte-wise resync is what gets the real reply back. Resuming after
    /// the whole failed candidate would consume `57` and leave `AB 00 81 08 …`, losing the reply.
    #[test]
    fn no_checksum_reply_followed_by_a_real_reply_recovers_the_real_reply() {
        let mut bytes = NO_CHECKSUM_REPLY.to_vec();
        bytes.extend_from_slice(&GET_INFO_REPLY);
        let got = events(&bytes);
        assert_eq!(
            got,
            vec![
                // The rejected candidate ate the next frame's first byte.
                Event::BadChecksum {
                    raw: vec![0x57, 0xAB, 0x00, 0xFE, 0x00, 0x57]
                },
                Event::Frame(Frame {
                    addr: 0x00,
                    cmd: 0x81,
                    data: vec![0x38, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
                }),
            ],
            "byte-wise resync must hand the stolen 0x57 back to the scan"
        );
    }

    #[test]
    fn no_checksum_reply_alone_expires_as_truncated_and_never_panics() {
        let mut p = Parser::new();
        p.push(&NO_CHECKSUM_REPLY);
        assert_eq!(
            p.next_event(),
            None,
            "five bytes is not yet a decidable frame"
        );
        assert_eq!(p.pending_len(), 5);
        assert_eq!(
            p.expire_partial(),
            Some(Event::Truncated {
                raw: NO_CHECKSUM_REPLY.to_vec()
            })
        );
        assert_eq!(p.pending_len(), 0);
        assert_eq!(p.expire_partial(), None);
    }

    #[test]
    fn expire_partial_on_an_empty_buffer_is_none() {
        assert_eq!(Parser::new().expire_partial(), None);
    }

    #[test]
    fn a_lone_header_byte_is_held_not_discarded() {
        let mut p = Parser::new();
        p.push(&[0x11, 0x57]);
        assert_eq!(
            p.drain_events().collect::<Vec<_>>(),
            vec![Event::Garbage { bytes: 1 }]
        );
        assert_eq!(p.pending_len(), 1);
        p.push(&GET_INFO_REPLY[1..]);
        // The held 0x57 completes the header.
        assert!(matches!(p.next_event(), Some(Event::Frame(f)) if f.cmd == 0x81));
    }

    #[test]
    fn a_header_inside_a_payload_does_not_split_the_frame() {
        let bytes = encode(0x02, &[0x57, 0xAB, 0x00, 0x81, 0x08, 0x00, 0x00, 0x00]).expect("fits");
        let got = events(&bytes);
        assert_eq!(got.len(), 1, "{got:?}");
    }

    #[test]
    fn a_buffer_of_pure_garbage_stays_bounded() {
        let mut p = Parser::new();
        for _ in 0..64 {
            p.push(&[0x00; 1024]);
        }
        assert!(
            p.pending_len() <= MAX_BUFFER,
            "buffer grew to {}",
            p.pending_len()
        );
        let total: usize = p
            .drain_events()
            .map(|e| match e {
                Event::Garbage { bytes } => bytes,
                other => panic!("unexpected {other:?}"),
            })
            .sum();
        assert_eq!(total, 64 * 1024);
    }

    #[test]
    fn a_header_that_never_completes_stays_bounded() {
        let mut p = Parser::new();
        // LEN claims 255 payload bytes that never arrive.
        p.push(&[0x57, 0xAB, 0x00, 0x02, 0xFF]);
        for _ in 0..1000 {
            p.push(&[0x00]);
        }
        assert!(
            p.pending_len() <= MAX_FRAME,
            "buffer grew to {}",
            p.pending_len()
        );
    }

    #[test]
    fn device_info_parses_the_observed_reply() {
        let info = DeviceInfo::parse(&GET_INFO_REPLY[5..13]).expect("8-byte payload");
        assert!((info.version - 1.8).abs() < 1e-6, "{}", info.version);
        assert!(info.target_connected);
        assert!(!info.num_lock && !info.caps_lock && !info.scroll_lock);
    }

    /// One wording for every command that prints this (H2): `key`, `type`, `devices --probe` and
    /// the viewer's startup line all print these bits, and they used to disagree — `caps=true`
    /// here, `caps=on` there, for one bit of one reply.
    #[test]
    fn the_info_line_reports_lock_bits_as_states() {
        let info = DeviceInfo {
            version: 1.8,
            target_connected: true,
            num_lock: false,
            caps_lock: true,
            scroll_lock: false,
        };
        assert_eq!(
            info.to_string(),
            "CH9329 firmware 1.8, target connected, locks: num=off caps=on scroll=off"
        );
        let inverted = DeviceInfo {
            target_connected: false,
            num_lock: true,
            caps_lock: false,
            scroll_lock: true,
            ..info
        };
        assert_eq!(
            inverted.to_string(),
            "CH9329 firmware 1.8, target NOT connected, locks: num=on caps=off scroll=on"
        );
        // The bits alone, for the unsolicited push, in the same spelling.
        assert_eq!(info.locks(), "num=off caps=on scroll=off");
        assert_eq!(inverted.locks(), "num=on caps=off scroll=on");
        assert!(inverted.to_string().ends_with(&inverted.locks()));
    }

    #[test]
    fn device_info_reads_every_lock_bit() {
        for (bits, want) in [
            (0x00, [false, false, false]),
            (0x01, [true, false, false]),
            (0x02, [false, true, false]),
            (0x04, [false, false, true]),
            (0x07, [true, true, true]),
        ] {
            let info = DeviceInfo::parse(&[b'8', 0x01, bits]).expect("3 bytes is enough");
            assert_eq!(
                [info.num_lock, info.caps_lock, info.scroll_lock],
                want,
                "bits {bits:#04x}"
            );
        }
    }

    #[test]
    fn device_info_rejects_a_short_payload_instead_of_panicking() {
        for n in 0..DeviceInfo::MIN_PAYLOAD {
            let payload = vec![b'8'; n];
            assert_eq!(
                DeviceInfo::parse(&payload),
                Err(ParseError::ShortPayload { need: 3, got: n })
            );
        }
    }

    #[test]
    fn device_info_saturates_a_nonsense_version_char() {
        let info = DeviceInfo::parse(&[0x00, 0x00, 0x00]).expect("3 bytes");
        assert!((info.version - 1.0).abs() < 1e-6, "{}", info.version);
    }

    #[test]
    fn frame_becomes_a_reply_keyed_on_its_command_byte() {
        let reply: Reply = Frame {
            addr: 0x00,
            cmd: 0xC1,
            data: vec![0xE4],
        }
        .into();
        assert!(reply.answers(0x01));
        assert!(reply.is_error());
        assert_eq!(reply.error_code(), Some(0xE4));
    }

    #[test]
    fn reset_forgets_everything() {
        let mut p = Parser::new();
        p.push(&[0xFF, 0x57, 0xAB, 0x00]);
        p.reset();
        assert_eq!(p.pending_len(), 0);
        assert_eq!(p.next_event(), None);
    }
}
