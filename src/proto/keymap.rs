//! Physical key (winit [`KeyCode`]) to HID usage.
//!
//! A mechanical port of `reference/browser/src/libs/keyboard/keymap.ts`. Upstream keys its table
//! on W3C UI Events `code` values and winit's `KeyCode` variant names follow the same table
//! (Appendix), so the port is a rename with three name-level corrections, listed below.
//!
//! The table is *not* a layout. `KeyCode` is the physical key; what character it produces is the
//! target's business, and this client forwards keys rather than text (§10.2).
//!
//! # Where winit's names differ from upstream's keys
//!
//! - **`SuperLeft` / `SuperRight`.** winit renames W3C `MetaLeft` / `MetaRight`. Upstream lists
//!   `MetaLeft` and `WinLeft` as separate keys with the same usage `0xE3` (likewise `MetaRight` /
//!   `WinRight` and `0xE7`); winit has one variant each, so the alias simply disappears. Both are
//!   modifiers here, matching upstream's `ModifierMap`, which takes precedence over its
//!   `KeycodeMap` at every call site.
//! - **`WakeUp`.** Upstream's key is spelled `Wake`, which is not a W3C `code` and therefore never
//!   matches a browser event; the intended usage `0xF9` is mapped here under winit's (correct)
//!   `WakeUp`.
//! - **`AudioVolume*`.** Upstream lists both `AudioVolumeMute`/`Up`/`Down` and the legacy
//!   `VolumeMute`/`Up`/`Down` aliases at the same usages. winit has only the `AudioVolume*` names.
//!
//! # Intentionally unmapped `KeyCode` variants
//!
//! Every variant below returns `None`, and each has a reason. A key that maps to nothing is never
//! sent, so the target simply never sees it.
//!
//! | Variant | Why |
//! | --- | --- |
//! | `Fn`, `FnLock` | Hardware-local. winit documents `Fn` as a key that "does not generate a separate code"; neither has a HID keyboard/keypad usage, and neither is in upstream's table. Forwarding is meaningless — the target has its own `Fn`. |
//! | `NumpadStar`, `NumpadHash` | Phone/remote keypad keys, not numeric-keypad keys (winit's own docs say to use `NumpadMultiply` for the keypad `*`). No HID keyboard usage, absent from upstream. |
//! | `Meta`, `Hyper`, `Turbo` | Legacy bare modifiers with no left/right identity. The HID keyboard page has usages only for the eight sided modifiers (`0xE0`–`0xE7`), which the sided variants already cover. Absent from upstream. |
//! | `Abort`, `Resume`, `Suspend` | System-control keys, not keyboard-page usages. Absent from upstream. |
//! | `Open` | Sun-keyboard key with no entry in upstream's table. HID `0x74` is "Execute", a different key that winit does not expose; guessing an equivalence would be an invention, not a port. |
//! | `Hiragana`, `Katakana` | Dedicated Japanese word-processor keys. Upstream maps the *`Lang3`/`Lang4`* codes (`0x92`/`0x93`) that ordinary Japanese keyboards emit, and has no entry for these; those two variants are mapped. |
//! | `F25` … `F35` | The HID keyboard/keypad page stops at F24 (`0x73`). Upstream stops there too. |
//!
//! `KeyCode` is `#[non_exhaustive]`, so a variant added by a future winit also falls through to
//! `None`. The exhaustiveness test in this module carries a hand-maintained list of all 194
//! variants in winit 0.30.13 and asserts its own length, so a winit upgrade that adds keys trips a
//! test rather than silently dropping them.

use crate::proto::report::modifier;
use crate::proto::HidKey;
use winit::keyboard::KeyCode;

/// Map a physical key to the HID form the keyboard report carries: a modifier bit for the eight
/// sided modifiers, a keyboard/keypad usage for everything else, `None` for a key this client does
/// not forward (see the module documentation for the list and the reasons).
///
/// Ported from `browser/src/libs/keyboard/keymap.ts` (Appendix).
pub fn hid_key(code: KeyCode) -> Option<HidKey> {
    use KeyCode as K;

    // Modifiers first: upstream's `ModifierMap` shadows its `KeycodeMap` for these eight keys, and
    // the report carries them as bits of byte 0, never as usages in the key slots (Appendix).
    let usage: u8 = match code {
        K::ControlLeft => return Some(HidKey::Modifier(modifier::LEFT_CTRL)),
        K::ShiftLeft => return Some(HidKey::Modifier(modifier::LEFT_SHIFT)),
        K::AltLeft => return Some(HidKey::Modifier(modifier::LEFT_ALT)),
        K::SuperLeft => return Some(HidKey::Modifier(modifier::LEFT_META)),
        K::ControlRight => return Some(HidKey::Modifier(modifier::RIGHT_CTRL)),
        K::ShiftRight => return Some(HidKey::Modifier(modifier::RIGHT_SHIFT)),
        K::AltRight => return Some(HidKey::Modifier(modifier::RIGHT_ALT)),
        K::SuperRight => return Some(HidKey::Modifier(modifier::RIGHT_META)),

        // Letters.
        K::KeyA => 0x04,
        K::KeyB => 0x05,
        K::KeyC => 0x06,
        K::KeyD => 0x07,
        K::KeyE => 0x08,
        K::KeyF => 0x09,
        K::KeyG => 0x0a,
        K::KeyH => 0x0b,
        K::KeyI => 0x0c,
        K::KeyJ => 0x0d,
        K::KeyK => 0x0e,
        K::KeyL => 0x0f,
        K::KeyM => 0x10,
        K::KeyN => 0x11,
        K::KeyO => 0x12,
        K::KeyP => 0x13,
        K::KeyQ => 0x14,
        K::KeyR => 0x15,
        K::KeyS => 0x16,
        K::KeyT => 0x17,
        K::KeyU => 0x18,
        K::KeyV => 0x19,
        K::KeyW => 0x1a,
        K::KeyX => 0x1b,
        K::KeyY => 0x1c,
        K::KeyZ => 0x1d,

        // Digit row. Note `Digit0` is 0x27, after `Digit9`, not before `Digit1`.
        K::Digit1 => 0x1e,
        K::Digit2 => 0x1f,
        K::Digit3 => 0x20,
        K::Digit4 => 0x21,
        K::Digit5 => 0x22,
        K::Digit6 => 0x23,
        K::Digit7 => 0x24,
        K::Digit8 => 0x25,
        K::Digit9 => 0x26,
        K::Digit0 => 0x27,

        // Editing and punctuation.
        K::Enter => 0x28,
        K::Escape => 0x29,
        K::Backspace => 0x2a,
        K::Tab => 0x2b,
        K::Space => 0x2c,
        K::Minus => 0x2d,
        K::Equal => 0x2e,
        K::BracketLeft => 0x2f,
        K::BracketRight => 0x30,
        K::Backslash => 0x31,
        K::Semicolon => 0x33,
        K::Quote => 0x34,
        K::Backquote => 0x35,
        K::Comma => 0x36,
        K::Period => 0x37,
        K::Slash => 0x38,
        K::CapsLock => 0x39,

        // Function keys.
        K::F1 => 0x3a,
        K::F2 => 0x3b,
        K::F3 => 0x3c,
        K::F4 => 0x3d,
        K::F5 => 0x3e,
        K::F6 => 0x3f,
        K::F7 => 0x40,
        K::F8 => 0x41,
        K::F9 => 0x42,
        K::F10 => 0x43,
        K::F11 => 0x44,
        K::F12 => 0x45,

        // Control pad.
        K::PrintScreen => 0x46,
        K::ScrollLock => 0x47,
        K::Pause => 0x48,
        K::Insert => 0x49,
        K::Home => 0x4a,
        K::PageUp => 0x4b,
        K::Delete => 0x4c,
        K::End => 0x4d,
        K::PageDown => 0x4e,

        // Arrows.
        K::ArrowRight => 0x4f,
        K::ArrowLeft => 0x50,
        K::ArrowDown => 0x51,
        K::ArrowUp => 0x52,

        // Numeric keypad.
        K::NumLock => 0x53,
        K::NumpadDivide => 0x54,
        K::NumpadMultiply => 0x55,
        K::NumpadSubtract => 0x56,
        K::NumpadAdd => 0x57,
        K::NumpadEnter => 0x58,
        K::Numpad1 => 0x59,
        K::Numpad2 => 0x5a,
        K::Numpad3 => 0x5b,
        K::Numpad4 => 0x5c,
        K::Numpad5 => 0x5d,
        K::Numpad6 => 0x5e,
        K::Numpad7 => 0x5f,
        K::Numpad8 => 0x60,
        K::Numpad9 => 0x61,
        K::Numpad0 => 0x62,
        K::NumpadDecimal => 0x63,

        // Non-US and application keys.
        K::IntlBackslash => 0x64,
        K::ContextMenu => 0x65,
        K::Power => 0x66,
        K::NumpadEqual => 0x67,

        // Extended function keys.
        K::F13 => 0x68,
        K::F14 => 0x69,
        K::F15 => 0x6a,
        K::F16 => 0x6b,
        K::F17 => 0x6c,
        K::F18 => 0x6d,
        K::F19 => 0x6e,
        K::F20 => 0x6f,
        K::F21 => 0x70,
        K::F22 => 0x71,
        K::F23 => 0x72,
        K::F24 => 0x73,

        // Sun-keyboard editing keys. Upstream also lists `Execute` (0x74) and `Stop` (0x78);
        // winit exposes neither.
        K::Help => 0x75,
        K::Props => 0x76,
        K::Select => 0x77,
        K::Again => 0x79,
        K::Undo => 0x7a,
        K::Cut => 0x7b,
        K::Copy => 0x7c,
        K::Paste => 0x7d,
        K::Find => 0x7e,

        // Volume.
        K::AudioVolumeMute => 0x7f,
        K::AudioVolumeUp => 0x80,
        K::AudioVolumeDown => 0x81,

        // Keypad comma, and the Japanese/Korean input keys.
        K::NumpadComma => 0x85,
        K::IntlRo => 0x87,
        K::KanaMode => 0x88,
        K::IntlYen => 0x89,
        K::Convert => 0x8a,
        K::NonConvert => 0x8b,
        K::Lang1 => 0x90,
        K::Lang2 => 0x91,
        K::Lang3 => 0x92,
        K::Lang4 => 0x93,
        K::Lang5 => 0x94,

        // Keypad extras beyond the standard block.
        K::NumpadParenLeft => 0xb6,
        K::NumpadParenRight => 0xb7,
        K::NumpadBackspace => 0xbb,
        K::NumpadMemoryStore => 0xd0,
        K::NumpadMemoryRecall => 0xd1,
        K::NumpadMemoryClear => 0xd2,
        K::NumpadMemoryAdd => 0xd3,
        K::NumpadMemorySubtract => 0xd4,
        K::NumpadClear => 0xd8,
        K::NumpadClearEntry => 0xd9,

        // Media transport.
        K::MediaPlayPause => 0xe8,
        K::MediaStop => 0xe9,
        K::MediaTrackPrevious => 0xea,
        K::MediaTrackNext => 0xeb,
        K::Eject => 0xec,
        K::MediaSelect => 0xed,

        // Application launch. Upstream gives `LaunchApp2` and `BrowserSearch` the same 0xF0; kept
        // as found, because this is a port and the collision is upstream's (both are outside the
        // HID keyboard page anyway).
        K::LaunchMail => 0xee,
        K::LaunchApp1 => 0xef,
        K::LaunchApp2 => 0xf0,

        // Browser.
        K::BrowserSearch => 0xf0,
        K::BrowserHome => 0xf1,
        K::BrowserBack => 0xf2,
        K::BrowserForward => 0xf3,
        K::BrowserStop => 0xf4,
        K::BrowserRefresh => 0xf5,
        K::BrowserFavorites => 0xf6,

        // Power management.
        K::Sleep => 0xf8,
        K::WakeUp => 0xf9,

        // Intentionally unmapped. Listed rather than swept into the catch-all so that the table in
        // this module's documentation and the code cannot drift apart.
        K::Fn
        | K::FnLock
        | K::NumpadStar
        | K::NumpadHash
        | K::Meta
        | K::Hyper
        | K::Turbo
        | K::Abort
        | K::Resume
        | K::Suspend
        | K::Open
        | K::Hiragana
        | K::Katakana
        | K::F25
        | K::F26
        | K::F27
        | K::F28
        | K::F29
        | K::F30
        | K::F31
        | K::F32
        | K::F33
        | K::F34
        | K::F35 => return None,

        // `KeyCode` is `#[non_exhaustive]`: a variant added by a future winit lands here. The
        // exhaustiveness test is what makes that a visible failure rather than a silent one.
        _ => return None,
    };
    Some(HidKey::Usage(usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `KeyCode` variant in winit 0.30.13, hand-maintained.
    ///
    /// `KeyCode` is `#[non_exhaustive]`, so the compiler cannot check this list for us. Bump
    /// [`VARIANT_COUNT`] deliberately when upgrading winit, after adding the new variants here and
    /// deciding whether each maps or joins the documented unmapped set.
    const ALL: &[KeyCode] = {
        use KeyCode as K;
        &[
            K::Backquote,
            K::Backslash,
            K::BracketLeft,
            K::BracketRight,
            K::Comma,
            K::Digit0,
            K::Digit1,
            K::Digit2,
            K::Digit3,
            K::Digit4,
            K::Digit5,
            K::Digit6,
            K::Digit7,
            K::Digit8,
            K::Digit9,
            K::Equal,
            K::IntlBackslash,
            K::IntlRo,
            K::IntlYen,
            K::KeyA,
            K::KeyB,
            K::KeyC,
            K::KeyD,
            K::KeyE,
            K::KeyF,
            K::KeyG,
            K::KeyH,
            K::KeyI,
            K::KeyJ,
            K::KeyK,
            K::KeyL,
            K::KeyM,
            K::KeyN,
            K::KeyO,
            K::KeyP,
            K::KeyQ,
            K::KeyR,
            K::KeyS,
            K::KeyT,
            K::KeyU,
            K::KeyV,
            K::KeyW,
            K::KeyX,
            K::KeyY,
            K::KeyZ,
            K::Minus,
            K::Period,
            K::Quote,
            K::Semicolon,
            K::Slash,
            K::AltLeft,
            K::AltRight,
            K::Backspace,
            K::CapsLock,
            K::ContextMenu,
            K::ControlLeft,
            K::ControlRight,
            K::Enter,
            K::SuperLeft,
            K::SuperRight,
            K::ShiftLeft,
            K::ShiftRight,
            K::Space,
            K::Tab,
            K::Convert,
            K::KanaMode,
            K::Lang1,
            K::Lang2,
            K::Lang3,
            K::Lang4,
            K::Lang5,
            K::NonConvert,
            K::Delete,
            K::End,
            K::Help,
            K::Home,
            K::Insert,
            K::PageDown,
            K::PageUp,
            K::ArrowDown,
            K::ArrowLeft,
            K::ArrowRight,
            K::ArrowUp,
            K::NumLock,
            K::Numpad0,
            K::Numpad1,
            K::Numpad2,
            K::Numpad3,
            K::Numpad4,
            K::Numpad5,
            K::Numpad6,
            K::Numpad7,
            K::Numpad8,
            K::Numpad9,
            K::NumpadAdd,
            K::NumpadBackspace,
            K::NumpadClear,
            K::NumpadClearEntry,
            K::NumpadComma,
            K::NumpadDecimal,
            K::NumpadDivide,
            K::NumpadEnter,
            K::NumpadEqual,
            K::NumpadHash,
            K::NumpadMemoryAdd,
            K::NumpadMemoryClear,
            K::NumpadMemoryRecall,
            K::NumpadMemoryStore,
            K::NumpadMemorySubtract,
            K::NumpadMultiply,
            K::NumpadParenLeft,
            K::NumpadParenRight,
            K::NumpadStar,
            K::NumpadSubtract,
            K::Escape,
            K::Fn,
            K::FnLock,
            K::PrintScreen,
            K::ScrollLock,
            K::Pause,
            K::BrowserBack,
            K::BrowserFavorites,
            K::BrowserForward,
            K::BrowserHome,
            K::BrowserRefresh,
            K::BrowserSearch,
            K::BrowserStop,
            K::Eject,
            K::LaunchApp1,
            K::LaunchApp2,
            K::LaunchMail,
            K::MediaPlayPause,
            K::MediaSelect,
            K::MediaStop,
            K::MediaTrackNext,
            K::MediaTrackPrevious,
            K::Power,
            K::Sleep,
            K::AudioVolumeDown,
            K::AudioVolumeMute,
            K::AudioVolumeUp,
            K::WakeUp,
            K::Meta,
            K::Hyper,
            K::Turbo,
            K::Abort,
            K::Resume,
            K::Suspend,
            K::Again,
            K::Copy,
            K::Cut,
            K::Find,
            K::Open,
            K::Paste,
            K::Props,
            K::Select,
            K::Undo,
            K::Hiragana,
            K::Katakana,
            K::F1,
            K::F2,
            K::F3,
            K::F4,
            K::F5,
            K::F6,
            K::F7,
            K::F8,
            K::F9,
            K::F10,
            K::F11,
            K::F12,
            K::F13,
            K::F14,
            K::F15,
            K::F16,
            K::F17,
            K::F18,
            K::F19,
            K::F20,
            K::F21,
            K::F22,
            K::F23,
            K::F24,
            K::F25,
            K::F26,
            K::F27,
            K::F28,
            K::F29,
            K::F30,
            K::F31,
            K::F32,
            K::F33,
            K::F34,
            K::F35,
        ]
    };

    /// Variants of `KeyCode` in winit 0.30.13.
    const VARIANT_COUNT: usize = 194;

    /// The documented unmapped set, mirroring the table in this module's documentation.
    const UNMAPPED: &[KeyCode] = {
        use KeyCode as K;
        &[
            K::Fn,
            K::FnLock,
            K::NumpadStar,
            K::NumpadHash,
            K::Meta,
            K::Hyper,
            K::Turbo,
            K::Abort,
            K::Resume,
            K::Suspend,
            K::Open,
            K::Hiragana,
            K::Katakana,
            K::F25,
            K::F26,
            K::F27,
            K::F28,
            K::F29,
            K::F30,
            K::F31,
            K::F32,
            K::F33,
            K::F34,
            K::F35,
        ]
    };

    #[test]
    fn the_variant_list_is_complete_and_free_of_duplicates() {
        assert_eq!(
            ALL.len(),
            VARIANT_COUNT,
            "winit 0.30.13 has {VARIANT_COUNT} KeyCode variants; if a winit upgrade changed that, \
             add the new variants to ALL, map them or document them as unmapped, and bump \
             VARIANT_COUNT"
        );
        let mut sorted = ALL.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ALL.len(), "duplicate entry in ALL");
    }

    /// The point of the test: a key that is neither mapped nor deliberately dropped is a key that
    /// silently does nothing on the target.
    #[test]
    fn every_key_code_is_either_mapped_or_documented_as_unmapped() {
        for &code in ALL {
            let mapped = hid_key(code).is_some();
            let documented_unmapped = UNMAPPED.contains(&code);
            assert!(
                mapped != documented_unmapped,
                "{code:?}: mapped={mapped}, documented as unmapped={documented_unmapped} -- \
                 every key must be exactly one of the two"
            );
        }
    }

    #[test]
    fn the_unmapped_list_matches_the_documentation() {
        assert_eq!(UNMAPPED.len(), 24);
        for &code in UNMAPPED {
            assert!(
                ALL.contains(&code),
                "{code:?} is unmapped but missing from ALL"
            );
            assert_eq!(hid_key(code), None, "{code:?} is documented as unmapped");
        }
    }

    /// Spot check against `browser/src/libs/keyboard/keymap.ts`. `reference/` is gitignored and
    /// absent in CI, so the expected values are transcribed here rather than read at runtime.
    /// These are HID usages from the datasheet's own page, not device captures, so §9.1's "read
    /// the fixture" rule does not apply — `fixtures/packets/ch9329.toml` carries only frames.
    #[test]
    fn spot_check_against_upstream() {
        let cases: &[(KeyCode, HidKey)] = &[
            // Letters: first, last, and one in the middle.
            (KeyCode::KeyA, HidKey::Usage(0x04)),
            (KeyCode::KeyM, HidKey::Usage(0x10)),
            (KeyCode::KeyZ, HidKey::Usage(0x1d)),
            // Digits: the row wraps, 0 sits after 9.
            (KeyCode::Digit1, HidKey::Usage(0x1e)),
            (KeyCode::Digit9, HidKey::Usage(0x26)),
            (KeyCode::Digit0, HidKey::Usage(0x27)),
            // Punctuation and editing.
            (KeyCode::Enter, HidKey::Usage(0x28)),
            (KeyCode::Escape, HidKey::Usage(0x29)),
            (KeyCode::Space, HidKey::Usage(0x2c)),
            (KeyCode::Minus, HidKey::Usage(0x2d)),
            (KeyCode::BracketRight, HidKey::Usage(0x30)),
            (KeyCode::Backslash, HidKey::Usage(0x31)),
            (KeyCode::Slash, HidKey::Usage(0x38)),
            // The lock key the §9 hardware check watches for.
            (KeyCode::CapsLock, HidKey::Usage(0x39)),
            // Function keys, both blocks.
            (KeyCode::F1, HidKey::Usage(0x3a)),
            (KeyCode::F12, HidKey::Usage(0x45)),
            (KeyCode::F13, HidKey::Usage(0x68)),
            (KeyCode::F24, HidKey::Usage(0x73)),
            // Arrows: upstream's order is right, left, down, up.
            (KeyCode::ArrowRight, HidKey::Usage(0x4f)),
            (KeyCode::ArrowLeft, HidKey::Usage(0x50)),
            (KeyCode::ArrowDown, HidKey::Usage(0x51)),
            (KeyCode::ArrowUp, HidKey::Usage(0x52)),
            // Numpad, including the 0-after-9 wrap again.
            (KeyCode::NumLock, HidKey::Usage(0x53)),
            (KeyCode::NumpadDivide, HidKey::Usage(0x54)),
            (KeyCode::NumpadMultiply, HidKey::Usage(0x55)),
            (KeyCode::Numpad1, HidKey::Usage(0x59)),
            (KeyCode::Numpad0, HidKey::Usage(0x62)),
            (KeyCode::NumpadDecimal, HidKey::Usage(0x63)),
            (KeyCode::NumpadEqual, HidKey::Usage(0x67)),
            // Control pad.
            (KeyCode::Insert, HidKey::Usage(0x49)),
            (KeyCode::Delete, HidKey::Usage(0x4c)),
            (KeyCode::PageDown, HidKey::Usage(0x4e)),
            // Non-US and Japanese.
            (KeyCode::IntlBackslash, HidKey::Usage(0x64)),
            (KeyCode::IntlRo, HidKey::Usage(0x87)),
            (KeyCode::IntlYen, HidKey::Usage(0x89)),
            (KeyCode::Lang1, HidKey::Usage(0x90)),
            // All eight modifiers, as bits rather than usages.
            (KeyCode::ControlLeft, HidKey::Modifier(0x01)),
            (KeyCode::ShiftLeft, HidKey::Modifier(0x02)),
            (KeyCode::AltLeft, HidKey::Modifier(0x04)),
            (KeyCode::SuperLeft, HidKey::Modifier(0x08)),
            (KeyCode::ControlRight, HidKey::Modifier(0x10)),
            (KeyCode::ShiftRight, HidKey::Modifier(0x20)),
            (KeyCode::AltRight, HidKey::Modifier(0x40)),
            (KeyCode::SuperRight, HidKey::Modifier(0x80)),
        ];
        assert!(cases.len() >= 25);
        for &(code, want) in cases {
            assert_eq!(hid_key(code), Some(want), "{code:?}");
        }
    }

    /// The modifier bits are the report's byte 0, so each must be exactly one bit and all eight
    /// together must be 0xFF (Appendix).
    #[test]
    fn the_eight_modifiers_are_eight_distinct_bits() {
        let sided = [
            KeyCode::ControlLeft,
            KeyCode::ShiftLeft,
            KeyCode::AltLeft,
            KeyCode::SuperLeft,
            KeyCode::ControlRight,
            KeyCode::ShiftRight,
            KeyCode::AltRight,
            KeyCode::SuperRight,
        ];
        let mut union = 0u8;
        for code in sided {
            match hid_key(code) {
                Some(HidKey::Modifier(bit)) => {
                    assert_eq!(bit.count_ones(), 1, "{code:?} is not a single bit");
                    assert_eq!(union & bit, 0, "{code:?} collides with another modifier");
                    union |= bit;
                }
                other => panic!("{code:?} mapped to {other:?}, expected a modifier bit"),
            }
        }
        assert_eq!(union, 0xFF);
    }

    /// A usage of zero is "no key" in the report, so it must never be produced (Appendix).
    #[test]
    fn no_key_maps_to_usage_zero() {
        for &code in ALL {
            assert_ne!(hid_key(code), Some(HidKey::Usage(0)), "{code:?}");
        }
    }

    /// Upstream's table is not injective — `LaunchApp2` and `BrowserSearch` share 0xF0. This test
    /// pins that as the only collision, so a transcription slip that duplicates a usage fails.
    #[test]
    fn the_only_duplicate_usage_is_upstreams_own() {
        let mut usages: Vec<u8> = ALL
            .iter()
            .filter_map(|&c| match hid_key(c) {
                Some(HidKey::Usage(u)) => Some(u),
                _ => None,
            })
            .collect();
        let total = usages.len();
        usages.sort_unstable();
        usages.dedup();
        assert_eq!(
            total - usages.len(),
            1,
            "expected exactly one duplicated usage"
        );
        assert_eq!(
            hid_key(KeyCode::LaunchApp2),
            hid_key(KeyCode::BrowserSearch)
        );
    }
}
