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
}
