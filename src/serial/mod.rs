//! CH9329 serial transport and deterministic test devices.
//!
//! The reader matches replies by command because lock-state notifications can arrive
//! unsolicited. The transport completes each frame before allowing cancellation.

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
