//! Opening the port, and nothing else.
//!
//! Kept apart from the reader and the matcher on purpose: Stage 2's reconnect (§2.7, §12) has to
//! re-open a port and hand it to a fresh reader without disturbing anything else, and that is far
//! easier if opening was never entangled with matching in the first place.

use std::path::Path;
use std::time::Duration;

use crate::link::LinkError;
use crate::proto::BAUD;

/// Port settings, the timeouts the reader loop runs on, and the two deadlines the write half
/// answers to.
///
/// The line settings are the ones confirmed on hardware: 57600 8N1, no flow control (Appendix,
/// `docs/stage0/serial.md`). Other baud rates produce silence, so they are not a tuning knob;
/// baud reconfiguration is an explicit opt-in operation and never happens at startup (§5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenOptions {
    /// Line rate. Anything but [`BAUD`] produces silence on this device (§5.1).
    pub baud: u32,
    /// Read timeout on the port itself. Short on purpose: it is the reader loop's heartbeat, the
    /// interval at which it notices silence, a stop request, and a hang-up. It is *not* a reply
    /// deadline — that is `transact`'s `timeout` argument.
    ///
    /// It applies to the reader's handle alone. `serialport` keeps one `timeout` per handle and
    /// uses it for both directions, so leaving the write half on this value would make a 25 ms
    /// stall in `write` tear a frame — and a torn frame is consumed as the head of the *next*
    /// command (§5.1, A15). [`OpenOptions::write_timeout`] is what the write half gets instead.
    pub read_timeout: Duration,
    /// Write deadline on the link's own handle, applied after the read half is cloned off.
    ///
    /// One second, against writes that return in about **50 µs** on this CDC-ACM device
    /// regardless of report size (A11, §5.1): four orders of magnitude of slack, so reaching it
    /// means the port is wedged rather than busy, and tearing the frame is then unavoidable
    /// anyway. It must never be as short as [`OpenOptions::read_timeout`], because a `write_all`
    /// cut short by a deadline is exactly the A15 failure this transport exists to avoid.
    ///
    /// It bounds each `write` call, not the whole frame, and it does **not** bound the `flush`
    /// that follows: `serialport`'s flush is `tcdrain`, which returns when the kernel has handed
    /// the bytes on and is only interruptible by a signal. That is acceptable here for the same
    /// A11 reason — there is no UART shift register draining at 57600 baud behind this port, only
    /// a USB bulk endpoint that has already taken the data — and it is not acceptable to skip the
    /// flush, since the frame must be on the wire before the reply can be waited for.
    pub write_timeout: Duration,
    /// How long the stream must be silent, with an incomplete frame buffered, before the reader
    /// gives up on it via [`crate::proto::frame::Parser::expire_partial`].
    ///
    /// This is what retires the §3.2 five-byte reply with no checksum byte (A13). It must exceed
    /// the worst plausible inter-byte gap inside a real frame; at 57600 baud one byte is 174 µs
    /// and the whole 14-byte `GET_INFO` reply arrives at +3.98 ms (Appendix), so 100 ms is three
    /// orders of magnitude of slack and still an order of magnitude below a sane reply timeout.
    pub silence_before_expire: Duration,
    /// How long the link must be quiet after a [`crate::link::LinkError::Timeout`] before the next
    /// request may be put in flight — the resynchronisation window (§3.1, §5.1, A12).
    ///
    /// A reply that arrives after its `transact` gave up is byte-identical to the fresh reply the
    /// *next* `transact` is waiting for, and the input writer sends the same command byte back to
    /// back all day (§2.6, A11), so the two cannot be told apart on the wire. They can be told
    /// apart in time: the acknowledged round trip is 4.15 ms for a keyboard report and 17.0 ms for
    /// a mouse report (A11), so 50 ms of silence is about three mouse round trips — long enough
    /// that a reply which has not landed is not coming, and short enough that the one quiet window
    /// a timeout costs is invisible next to the timeout itself.
    ///
    /// The window is not a hang: `transact` waits at most twice this (see
    /// [`crate::serial::SerialLink::transact`]), so a device that never stops talking delays the
    /// writer rather than wedging it.
    pub quiet_after_timeout: Duration,
}

impl Default for OpenOptions {
    fn default() -> OpenOptions {
        OpenOptions {
            baud: BAUD,
            read_timeout: Duration::from_millis(25),
            write_timeout: Duration::from_secs(1),
            silence_before_expire: Duration::from_millis(100),
            quiet_after_timeout: Duration::from_millis(50),
        }
    }
}

/// Open `path` with [`OpenOptions`] applied, returning a handle carrying the *read* timeout.
///
/// The caller clones the read half off this handle and then applies
/// [`OpenOptions::write_timeout`] to the copy it keeps for writing — see
/// [`crate::serial::SerialLink::open_with`]. Doing it in that order is what gives the two halves
/// independent deadlines: `serialport` stores the timeout per handle and `try_clone` copies
/// whatever value is set at the moment of the clone.
///
/// A failure to open is [`LinkError::Down`] rather than [`LinkError::Io`]: there is no transport,
/// which is precisely what `Down` means to the input writer (§2.6.1), and the message carries the
/// path so the error is actionable without a second lookup.
pub(crate) fn open(
    path: &Path,
    opts: &OpenOptions,
) -> Result<Box<dyn serialport::SerialPort>, LinkError> {
    let name = path.to_string_lossy();
    serialport::new(name.as_ref(), opts.baud)
        .data_bits(serialport::DataBits::Eight)
        .parity(serialport::Parity::None)
        .stop_bits(serialport::StopBits::One)
        .flow_control(serialport::FlowControl::None)
        .timeout(opts.read_timeout)
        .open()
        .map_err(|e| LinkError::Down(format!("open {name} at {} baud: {e}", opts.baud)))
}
