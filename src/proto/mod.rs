//! CH9329 framing, HID reports, key mapping, and device replies.
//!
//! Encoding and parsing are independent of serial I/O. Protocol examples are stored in
//! `fixtures/packets/ch9329.toml`; see `docs/protocol.md` for framing and hardware quirks.

pub mod frame;
pub mod keymap;
pub mod report;
pub mod usb_string;

pub use frame::*;
pub use report::*;
pub use usb_string::*;

/// Command bytes.
pub mod cmd {
    pub const GET_INFO: u8 = 0x01;
    pub const SEND_KB_GENERAL_DATA: u8 = 0x02;
    pub const SEND_KB_MEDIA_DATA: u8 = 0x03;
    pub const SEND_MS_ABS_DATA: u8 = 0x04;
    pub const SEND_MS_REL_DATA: u8 = 0x05;
    pub const SEND_MY_HID_DATA: u8 = 0x06;
    pub const READ_MY_HID_DATA: u8 = 0x87;
    pub const GET_PARA_CFG: u8 = 0x08;
    pub const SET_PARA_CFG: u8 = 0x09;
    pub const GET_USB_STRING: u8 = 0x0a;
    pub const SET_USB_STRING: u8 = 0x0b;
    pub const SET_DEFAULT_CFG: u8 = 0x0c;
    pub const RESET: u8 = 0x0f;
}

/// Serial defaults confirmed on hardware.
pub const BAUD: u32 = 57600;
