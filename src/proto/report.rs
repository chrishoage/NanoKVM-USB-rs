//! HID report builders. The payload shapes here are the ones that move the pointer on hardware
//! (A17); the bare HID report shapes from upstream's builder do not.

/// Modifier bits of the keyboard report's first byte (Appendix).
pub mod modifier {
    pub const LEFT_CTRL: u8 = 1 << 0;
    pub const LEFT_SHIFT: u8 = 1 << 1;
    pub const LEFT_ALT: u8 = 1 << 2;
    pub const LEFT_META: u8 = 1 << 3;
    pub const RIGHT_CTRL: u8 = 1 << 4;
    pub const RIGHT_SHIFT: u8 = 1 << 5;
    pub const RIGHT_ALT: u8 = 1 << 6;
    pub const RIGHT_META: u8 = 1 << 7;
}

/// Mouse button bits (Appendix).
pub mod button {
    pub const LEFT: u8 = 1 << 0;
    pub const RIGHT: u8 = 1 << 1;
    pub const MIDDLE: u8 = 1 << 2;
    pub const BACK: u8 = 1 << 3;
    pub const FORWARD: u8 = 1 << 4;
}

/// Absolute coordinates: 12 usable bits in a 13-bit field. Full scale 4095, divisor 4096, and the
/// device wraps to the origin at 8192, so the encoder clamps and never relies on the device (§3.4).
pub const ABS_MAX: u16 = 4095;
pub const ABS_DIVISOR: u32 = 4096;

/// Map a pixel on the target's screen to the absolute coordinate that lands on it.
///
/// The device law is `pixel = floor(v * extent / 4096)` (§3.4, measured). The exact right inverse
/// for every extent up to 4096 is `ceil(px * 4096 / extent)`: the smallest coordinate whose floor
/// reaches `px`. §3.4 proposes mapping "through the pixel centre", `((2*px + 1) * 2048) / extent`;
/// that is exact for every extent measured on this desk but lands one pixel short for 1536 of the
/// 3840 columns and 96 of the 2160 rows at 4K, so it is not used (Stage 1 finding; see
/// `tests/proto_report.rs`). Both formulas agree that the naive `px * 4096 / extent` sends 4093 for
/// pixel 1919 and leaves the last column unreachable. Pixels outside `extent` clamp to the last
/// pixel; extents above 4096 cannot address every pixel and are clamped to full scale.
pub fn abs_coord(px: u32, extent: u32) -> u16 {
    if extent == 0 {
        return 0;
    }
    let px = u64::from(px.min(extent - 1));
    let v = (px * u64::from(ABS_DIVISOR)).div_ceil(u64::from(extent));
    v.min(u64::from(ABS_MAX)) as u16
}

/// Full-state keyboard report: `[modifier, 0x00, key0..key5]`, 8 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyboardReport {
    pub modifiers: u8,
    pub keys: [u8; 6],
}

impl KeyboardReport {
    pub const LEN: usize = 8;
    pub const RELEASE_ALL: KeyboardReport = KeyboardReport {
        modifiers: 0,
        keys: [0; 6],
    };

    pub fn payload(&self) -> [u8; 8] {
        let k = self.keys;
        [self.modifiers, 0x00, k[0], k[1], k[2], k[3], k[4], k[5]]
    }
}

/// Relative mouse report: `[0x01, buttons, dx, dy, wheel]`, 5 bytes. The leading mode byte is
/// mandatory: without it the device ACKs and reads every field one byte to the left (A17).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MouseRelReport {
    pub buttons: u8,
    pub dx: i8,
    pub dy: i8,
    pub wheel: i8,
}

impl MouseRelReport {
    pub const LEN: usize = 5;
    pub const MODE: u8 = 0x01;
    /// Reports saturate at this magnitude; larger deltas are split across reports (§2.3).
    pub const DELTA_MAX: i32 = 127;

    pub fn payload(&self) -> [u8; 5] {
        [
            Self::MODE,
            self.buttons,
            self.dx as u8,
            self.dy as u8,
            self.wheel as u8,
        ]
    }
}

/// Absolute mouse report: `[0x02, buttons, xLo, xHi, yLo, yHi, wheel]`, 7 bytes, little-endian
/// coordinates in `0..=4095`. Construction clamps; the device would wrap past 8191 (§3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MouseAbsReport {
    pub buttons: u8,
    pub x: u16,
    pub y: u16,
    pub wheel: i8,
}

impl MouseAbsReport {
    pub const LEN: usize = 7;
    pub const MODE: u8 = 0x02;

    pub fn payload(&self) -> [u8; 7] {
        let x = self.x.min(ABS_MAX).to_le_bytes();
        let y = self.y.min(ABS_MAX).to_le_bytes();
        [
            Self::MODE,
            self.buttons,
            x[0],
            x[1],
            y[0],
            y[1],
            self.wheel as u8,
        ]
    }
}

/// The mouse half of a release-all: all buttons up, no motion (fixture `mouse_rel_idle`).
pub const MOUSE_RELEASE_ALL: MouseRelReport = MouseRelReport {
    buttons: 0,
    dx: 0,
    dy: 0,
    wheel: 0,
};

/// A physical key as the keyboard report carries it: either one modifier bit or one non-modifier
/// HID usage. Produced by `proto::keymap`, consumed by `input`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HidKey {
    /// Exactly one bit of [`modifier`].
    Modifier(u8),
    /// A non-zero HID usage in the keyboard/keypad page.
    Usage(u8),
}
