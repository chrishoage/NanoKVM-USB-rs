//! The CH9329 transport: a serial port, a reader thread that matches replies by command byte,
//! and a [`crate::link::Link`] implementation over the pair (plan §4, §4.1, Stage 1 in §12).
//!
//! Three facts about this device shape everything here, and all three are measured rather than
//! assumed (`docs/STAGE0_FINDINGS.md`):
//!
//! - **Replies are matched by command byte, never by arrival order** (A12, Appendix). The device
//!   pushes an unrequested `0x81` lock-state frame about 15 ms after any lock-key change, and it
//!   can land between a request and that request's reply. A reader that assumes the next frame
//!   answers the last request mis-attributes it.
//! - **A torn write corrupts the following command** (A15, §5.1). The chip's receive parser has
//!   no inter-byte timeout: it waits indefinitely for the rest of a short frame and then eats the
//!   next command as the remainder. So a frame is written whole or the link is abandoned.
//! - **Not every reply is a well-formed frame** (A13, §3.2). An undefined command is answered
//!   with five bytes and no checksum byte, forever. The reader retires that with
//!   [`crate::proto::frame::Parser::expire_partial`] after a period of silence, so the caller
//!   gets a [`crate::link::LinkError::Timeout`] instead of a hang.
//!
//! Layout: `port` opens the port and nothing else, `reader` owns the reader thread and the
//! matcher, [`SerialLink`] joins them, and `source` re-runs the open on demand for Stage 2's
//! reconnect (§2.7, §12). The split is what makes that reopen cost nothing: the matcher was never
//! entangled with opening in the first place.
//!
//! [`fake`] is a scripted CH9329 on a pty (§9.3) so the tests exercise this module unmodified.

#[doc(hidden)]
pub mod fake;
mod link;
mod port;
mod reader;
mod source;

pub use link::{SerialLink, RESYNC_PREAMBLE_LEN};
pub use port::OpenOptions;
pub use reader::SerialStats;
pub use source::SerialLinkSource;

/// Free-function form of [`SerialLink::lock_state_from`], for callers that hold only a `Reply`.
pub fn lock_state_from(reply: &crate::link::Reply) -> Option<crate::proto::DeviceInfo> {
    SerialLink::lock_state_from(reply)
}
