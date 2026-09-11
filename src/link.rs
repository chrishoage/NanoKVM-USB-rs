//! The seam between the input writer (§2.6) and a transport (`serial`, or a fake in tests).
//!
//! A `Link` moves whole frames. It is the writer's atomic unit: a call to `transact` either hands
//! one complete, well-formed frame to the transport or hands nothing. It never writes part of a
//! frame, because the CH9329 has no inter-byte timeout and a torn frame corrupts the following
//! command (§5.1, A15).

use std::time::Duration;

/// A reply frame from the device with its header, length and checksum already stripped and
/// validated. `cmd` is the reply's own command byte: `request | 0x80` on success, `request | 0xC0`
/// on error (Appendix).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub cmd: u8,
    pub data: Vec<u8>,
}

impl Reply {
    /// True when this reply answers `request` (success or error form).
    pub fn answers(&self, request: u8) -> bool {
        self.cmd == (request | 0x80) || self.cmd == (request | 0xC0)
    }
    pub fn is_error(&self) -> bool {
        self.cmd & 0xC0 == 0xC0
    }
    /// The one-byte error code carried by an error frame (`0xE4` checksum, `0xE5` parameter, …).
    pub fn error_code(&self) -> Option<u8> {
        if self.is_error() {
            self.data.first().copied()
        } else {
            None
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    /// The frame was written but no matching reply arrived. The device may still have acted on it.
    #[error("no reply to command {cmd:#04x} within {timeout:?}")]
    Timeout { cmd: u8, timeout: Duration },
    /// The device answered with an error frame (`cmd | 0xC0`). The link itself is healthy.
    #[error("device rejected command {cmd:#04x} with error code {code:#04x}")]
    Device { cmd: u8, code: u8 },
    /// The transport is gone (port closed, `ENODEV`, reader thread dead). Nothing was written.
    #[error("link down: {0}")]
    Down(String),
    #[error("link I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl LinkError {
    /// Whether the transport should be considered unusable after this error.
    pub fn is_transport_failure(&self) -> bool {
        matches!(self, LinkError::Down(_) | LinkError::Io(_))
    }
}

/// A frame-level transport to the CH9329.
pub trait Link: Send {
    /// Write exactly one complete frame for `cmd` with `payload`, then wait up to `timeout` for the
    /// reply whose command byte answers `cmd` (see [`Reply::answers`]). Frames that answer some
    /// other command — notably the unsolicited `0x81` lock-state push (A12) — must be diverted,
    /// never returned here and never treated as this call's reply.
    ///
    /// Contract:
    /// - the write is all-or-nothing at the frame boundary; a partial write is reported as
    ///   [`LinkError::Io`] and the link must be treated as down afterwards;
    /// - a device error frame is returned as `Err(LinkError::Device { .. })`, never dropped (§3.1);
    /// - an acknowledgement proves the frame was parsed, not that it had any effect (§3.4).
    fn transact(&mut self, cmd: u8, payload: &[u8], timeout: Duration) -> Result<Reply, LinkError>;

    /// Write bytes that complete any frame the chip's parser is waiting on, without forming a
    /// valid command (§5.1; §11 "Still open: Resynchronisation after a torn write").
    ///
    /// The chip's receive parser has **no inter-byte timeout**: after a torn write it waits
    /// indefinitely — measured to ten seconds — and then eats the *next* command as the remainder
    /// of the truncated one. Reopening the port does not clear that, because the parser state
    /// lives in the chip and the chip was not power-cycled; a write timeout on a live port and a
    /// `reset`-style replug that leaves the dongle powered both land there. So the first thing
    /// written on a link that may have inherited a mid-frame parser is a preamble that finishes
    /// the stale frame and forms nothing of its own. See [`crate::serial::SerialLink::resync`] for
    /// the argument about its size and its safety.
    ///
    /// The default does nothing, which is right for a fake and for any transport that cannot tear
    /// a frame in the first place.
    fn resync(&mut self) -> Result<(), LinkError> {
        Ok(())
    }

    /// Whether the transport has already failed — asked **without writing anything** (§2.6, §2.7).
    ///
    /// This exists because loss is not always write-driven. A transport that has a receiving half
    /// of its own learns about a hang-up from that half within a poll: `serialport` turns the
    /// `POLLHUP` on an unplugged CDC-ACM node into `BrokenPipe` on the very next read, and
    /// [`crate::serial::SerialLink`]'s reader thread marks the link down there. The input writer,
    /// meanwhile, may have nothing queued for minutes — a user who is not typing is the ordinary
    /// state of this program — so *nothing it does* would discover the failure. Measured on the
    /// dongle: the reader knew in milliseconds, the idle writer still had not noticed after 30 s,
    /// and because it kept the fd open the kernel could not free the tty index and the device came
    /// back under a different name.
    ///
    /// So the writer asks, on a short interval, instead of writing. A query rather than a keepalive
    /// is the whole point: the CH9329 acknowledges anything (§3.4, A17), so traffic would prove
    /// nothing about the link that a poll of its own read side does not already know, and would put
    /// bytes on a live console's keyboard to find out.
    ///
    /// The default is `false` — "as far as I know, fine" — which is right for a transport with no
    /// independent way to notice, and for a fake. Answering it must not write, must not block and
    /// must not wait on the device.
    fn is_down(&self) -> bool {
        false
    }
}

/// Produces a fresh transport after the previous one failed (§2.7). The writer owns one of these
/// for its whole life and asks it for a new [`Link`] whenever the current one is down.
///
/// Deciding *when* to reopen is not a transport concern and does not live here: §2.7's sequence —
/// discard the queue, release-all, re-query device info — needs the input writer's queue and its
/// epoch bookkeeping, so the writer drives it and a source does nothing but hand over a link.
pub trait LinkSource: Send {
    /// Open a new transport, or say why not. An `Err` is expected and ordinary: the device is
    /// simply not back yet.
    fn open(&mut self) -> Result<Box<dyn Link>, LinkError>;
    /// One line for logs: the path or the discovery rule this source reopens by.
    fn describe(&self) -> String;
}
