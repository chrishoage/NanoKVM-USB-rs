//! The key-name vocabulary: the words a user may type in a chord, and the physical key each one
//! names.
//!
//! This table is *not* a layout and carries no usages. It maps a name to a winit [`KeyCode`], and
//! [`crate::proto::keymap::hid_key`] then decides what the report carries — the same authority the
//! viewer forwards through, so no usage is retyped here (§10.2).
//!
//! **Modifiers are a separate table on purpose.** A modifier is a bit of the report's first byte,
//! never a usage in a key slot (Appendix), so [`key_by_name`] does not know the modifier names at
//! all: a script cannot put `ctrl` into a usage slot even by accident, because the only function
//! that resolves it returns a bit.

use winit::keyboard::KeyCode;

use crate::proto::keymap::hid_key;
use crate::proto::HidKey;

/// `(name, key, shifted)` for every canonical key name.
///
/// `shifted` is true where the *name* denotes the shifted glyph of the key rather than the key
/// itself. Only `plus` does today: the chord grammar splits on `+`, so a literal `+` cannot be a
/// chord token and needs a word, and that word means shift+equal.
#[rustfmt::skip]
const KEYS: &[(&str, KeyCode, bool)] = {
    use KeyCode as K;
    &[
        // Letters.
        ("a", K::KeyA, false), ("b", K::KeyB, false), ("c", K::KeyC, false), ("d", K::KeyD, false),
        ("e", K::KeyE, false), ("f", K::KeyF, false), ("g", K::KeyG, false), ("h", K::KeyH, false),
        ("i", K::KeyI, false), ("j", K::KeyJ, false), ("k", K::KeyK, false), ("l", K::KeyL, false),
        ("m", K::KeyM, false), ("n", K::KeyN, false), ("o", K::KeyO, false), ("p", K::KeyP, false),
        ("q", K::KeyQ, false), ("r", K::KeyR, false), ("s", K::KeyS, false), ("t", K::KeyT, false),
        ("u", K::KeyU, false), ("v", K::KeyV, false), ("w", K::KeyW, false), ("x", K::KeyX, false),
        ("y", K::KeyY, false), ("z", K::KeyZ, false),

        // Digit row. The `0` key sits after `9` in the HID page but is named by its digit here.
        ("0", K::Digit0, false), ("1", K::Digit1, false), ("2", K::Digit2, false),
        ("3", K::Digit3, false), ("4", K::Digit4, false), ("5", K::Digit5, false),
        ("6", K::Digit6, false), ("7", K::Digit7, false), ("8", K::Digit8, false),
        ("9", K::Digit9, false),

        // Function keys. The HID keyboard page stops at F24 and so does `proto::keymap`.
        ("f1",  K::F1,  false), ("f2",  K::F2,  false), ("f3",  K::F3,  false),
        ("f4",  K::F4,  false), ("f5",  K::F5,  false), ("f6",  K::F6,  false),
        ("f7",  K::F7,  false), ("f8",  K::F8,  false), ("f9",  K::F9,  false),
        ("f10", K::F10, false), ("f11", K::F11, false), ("f12", K::F12, false),
        ("f13", K::F13, false), ("f14", K::F14, false), ("f15", K::F15, false),
        ("f16", K::F16, false), ("f17", K::F17, false), ("f18", K::F18, false),
        ("f19", K::F19, false), ("f20", K::F20, false), ("f21", K::F21, false),
        ("f22", K::F22, false), ("f23", K::F23, false), ("f24", K::F24, false),

        // Editing, navigation and the lock keys.
        ("enter",     K::Enter,      false), ("esc",        K::Escape,      false),
        ("tab",       K::Tab,        false), ("space",      K::Space,       false),
        ("backspace", K::Backspace,  false), ("delete",     K::Delete,      false),
        ("insert",    K::Insert,     false), ("home",       K::Home,        false),
        ("end",       K::End,        false), ("pgup",       K::PageUp,      false),
        ("pgdn",      K::PageDown,   false), ("up",         K::ArrowUp,     false),
        ("down",      K::ArrowDown,  false), ("left",       K::ArrowLeft,   false),
        ("right",     K::ArrowRight, false), ("capslock",   K::CapsLock,    false),
        ("numlock",   K::NumLock,    false), ("scrolllock", K::ScrollLock,  false),
        ("printscreen", K::PrintScreen, false), ("pause",   K::Pause,       false),
        ("menu",      K::ContextMenu, false),

        // Punctuation, by name. The literals are aliases, below.
        ("minus",        K::Minus,        false), ("equal",        K::Equal,        false),
        ("bracketleft",  K::BracketLeft,  false), ("bracketright", K::BracketRight, false),
        ("backslash",    K::Backslash,    false), ("semicolon",    K::Semicolon,    false),
        ("quote",        K::Quote,        false), ("backquote",    K::Backquote,    false),
        ("comma",        K::Comma,        false), ("period",       K::Period,       false),
        ("slash",        K::Slash,        false),
        // The one name that carries a shift: `+` cannot be a chord token (see `KEYS`).
        ("plus",         K::Equal,        true),

        // Numeric keypad. `kpplus` is the keypad key, unrelated to `plus` above.
        ("kp0", K::Numpad0, false), ("kp1", K::Numpad1, false), ("kp2", K::Numpad2, false),
        ("kp3", K::Numpad3, false), ("kp4", K::Numpad4, false), ("kp5", K::Numpad5, false),
        ("kp6", K::Numpad6, false), ("kp7", K::Numpad7, false), ("kp8", K::Numpad8, false),
        ("kp9", K::Numpad9, false),
        ("kpenter",    K::NumpadEnter,    false), ("kpplus",     K::NumpadAdd,      false),
        ("kpminus",    K::NumpadSubtract, false), ("kpmultiply", K::NumpadMultiply, false),
        ("kpdivide",   K::NumpadDivide,   false), ("kpdecimal",  K::NumpadDecimal,  false),
    ]
};

/// Alternative spellings, and every punctuation key by its literal glyph.
///
/// An alias never denotes a shifted glyph: the literals here are the *unshifted* character each
/// key produces on a US board, so `-` and `minus` are the same keystroke.
#[rustfmt::skip]
const ALIASES: &[(&str, KeyCode)] = {
    use KeyCode as K;
    &[
        ("return", K::Enter),      ("escape",   K::Escape),
        ("del",    K::Delete),     ("ins",      K::Insert),
        ("pageup", K::PageUp),     ("pagedown", K::PageDown),
        ("prtsc",  K::PrintScreen), ("sysrq",   K::PrintScreen),
        ("contextmenu", K::ContextMenu), ("grave", K::Backquote),

        ("-",  K::Minus),        ("=",  K::Equal),
        ("[",  K::BracketLeft),  ("]",  K::BracketRight),
        ("\\", K::Backslash),    (";",  K::Semicolon),
        ("'",  K::Quote),        ("`",  K::Backquote),
        (",",  K::Comma),        (".",  K::Period),
        ("/",  K::Slash),
    ]
};

/// `(name, key)` for the eight modifiers. The bit is not here — it comes from
/// [`crate::proto::keymap::hid_key`], which is the one authority for it.
///
/// Left-hand names are unsided (`ctrl`, not `lctrl`) because the target sees a bit and only a
/// bit; the right-hand names exist because some targets bind them separately (`altgr`).
#[rustfmt::skip]
const MODIFIERS: &[(&str, KeyCode)] = {
    use KeyCode as K;
    &[
        ("ctrl",  K::ControlLeft), ("control", K::ControlLeft),
        ("alt",   K::AltLeft),
        ("shift", K::ShiftLeft),
        ("super", K::SuperLeft),   ("win", K::SuperLeft), ("meta", K::SuperLeft),
        ("cmd",   K::SuperLeft),

        ("rctrl",  K::ControlRight),
        ("ralt",   K::AltRight),   ("altgr", K::AltRight),
        ("rshift", K::ShiftRight),
        ("rsuper", K::SuperRight),
    ]
};

/// The physical key a name denotes, ignoring whether the name means that key's *shifted* glyph.
///
/// Case-insensitive. Returns `None` for every modifier name, so a modifier can never reach a
/// usage slot. Callers that build a report want [`key_by_name_with_shift`]: `plus` resolves here
/// to the `=` key, which alone types `=`.
pub fn key_by_name(name: &str) -> Option<KeyCode> {
    key_by_name_with_shift(name).map(|(key, _)| key)
}

/// The physical key a name denotes and whether the name means the shifted glyph of it.
///
/// The shift is part of the *name*, not of the key: only `plus` sets it today, because the chord
/// grammar splits on `+` and so cannot carry a literal one.
pub fn key_by_name_with_shift(name: &str) -> Option<(KeyCode, bool)> {
    if let Some(&(_, key, shifted)) = KEYS.iter().find(|(n, _, _)| n.eq_ignore_ascii_case(name)) {
        return Some((key, shifted));
    }
    ALIASES
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|&(_, key)| (key, false))
}

/// The modifier bit `name` holds, taken from [`crate::proto::keymap::hid_key`] rather than
/// restated here. Case-insensitive; `None` for anything that is not one of the eight modifiers.
pub fn modifier_by_name(name: &str) -> Option<u8> {
    let key = MODIFIERS
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|&(_, key)| key)?;
    match hid_key(key) {
        Some(HidKey::Modifier(bit)) => Some(bit),
        _ => None,
    }
}

/// Every canonical key name, in table order, for help text and for tests. Aliases and the literal
/// punctuation spellings are deliberately absent: this is the list to *show* a user.
pub fn canonical_names() -> impl Iterator<Item = (&'static str, KeyCode)> {
    KEYS.iter().map(|&(name, key, _)| (name, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the table: a name that `proto::keymap` does not forward as a usage is a name
    /// that would compile to nothing the target can receive.
    #[test]
    fn every_key_name_resolves_to_a_forwarded_usage() {
        for (name, key) in KEYS
            .iter()
            .map(|&(n, k, _)| (n, k))
            .chain(ALIASES.iter().copied())
        {
            match hid_key(key) {
                Some(HidKey::Usage(usage)) => assert_ne!(usage, 0, "{name}: usage 0 is 'no key'"),
                other => panic!("{name} ({key:?}) is {other:?}, expected a usage"),
            }
        }
    }

    #[test]
    fn every_modifier_name_resolves_to_a_modifier_bit() {
        for &(name, key) in MODIFIERS {
            match hid_key(key) {
                Some(HidKey::Modifier(bit)) => {
                    assert_eq!(bit.count_ones(), 1, "{name} is not a single bit");
                    assert_eq!(modifier_by_name(name), Some(bit));
                }
                other => panic!("{name} ({key:?}) is {other:?}, expected a modifier bit"),
            }
        }
    }

    /// If a modifier were also a key name it could land in a usage slot, which the report has no
    /// encoding for (Appendix).
    #[test]
    fn no_name_is_both_a_key_and_a_modifier() {
        for &(name, _) in MODIFIERS {
            assert_eq!(key_by_name(name), None, "{name} is in both tables");
        }
        for (name, _) in canonical_names() {
            assert_eq!(modifier_by_name(name), None, "{name} is in both tables");
        }
    }

    #[test]
    fn the_tables_contain_no_duplicate_names() {
        let mut names: Vec<&str> = KEYS
            .iter()
            .map(|&(n, _, _)| n)
            .chain(ALIASES.iter().map(|&(n, _)| n))
            .chain(MODIFIERS.iter().map(|&(n, _)| n))
            .collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(total, names.len(), "a name appears twice");
    }

    #[test]
    fn aliases_resolve_to_the_same_key_as_their_canonical_name() {
        let pairs = [
            ("return", "enter"),
            ("escape", "esc"),
            ("del", "delete"),
            ("ins", "insert"),
            ("pageup", "pgup"),
            ("pagedown", "pgdn"),
            ("prtsc", "printscreen"),
            ("sysrq", "printscreen"),
            ("contextmenu", "menu"),
            ("grave", "backquote"),
            ("-", "minus"),
            ("=", "equal"),
            ("[", "bracketleft"),
            ("]", "bracketright"),
            ("\\", "backslash"),
            (";", "semicolon"),
            ("'", "quote"),
            ("`", "backquote"),
            (",", "comma"),
            (".", "period"),
            ("/", "slash"),
        ];
        assert_eq!(pairs.len(), ALIASES.len(), "an alias is untested");
        for (alias, canonical) in pairs {
            assert_eq!(
                key_by_name(alias),
                key_by_name(canonical),
                "{alias} should be {canonical}"
            );
            assert!(key_by_name(alias).is_some());
        }
        for (alias, canonical) in [("win", "super"), ("altgr", "ralt"), ("control", "ctrl")] {
            assert_eq!(modifier_by_name(alias), modifier_by_name(canonical));
        }
    }

    #[test]
    fn lookup_is_case_insensitive() {
        for name in ["ENTER", "Enter", "eNtEr"] {
            assert_eq!(key_by_name(name), key_by_name("enter"));
        }
        for name in ["CTRL", "Ctrl"] {
            assert_eq!(modifier_by_name(name), modifier_by_name("ctrl"));
        }
    }

    /// `plus` is the one name that means a key *and* a shift, because `+` cannot be a chord token.
    #[test]
    fn plus_is_the_shifted_equal_key() {
        assert_eq!(
            key_by_name_with_shift("plus"),
            Some((KeyCode::Equal, true)),
            "plus must be shift+equal"
        );
        assert_eq!(
            key_by_name_with_shift("equal"),
            Some((KeyCode::Equal, false))
        );
        let shifted: Vec<&str> = KEYS
            .iter()
            .filter(|&&(_, _, s)| s)
            .map(|&(n, _, _)| n)
            .collect();
        assert_eq!(shifted, vec!["plus"], "a new shifted name needs a test");
    }

    #[test]
    fn an_unknown_name_is_none() {
        for name in ["xyz", "", "f25", "kp10", "ctlr"] {
            assert_eq!(key_by_name(name), None, "{name:?}");
            assert_eq!(modifier_by_name(name), None, "{name:?}");
        }
    }

    /// The vocabulary the help text promises: a-z, 0-9, f1-f24 and the named keys.
    #[test]
    fn the_canonical_vocabulary_is_complete() {
        let names: Vec<&str> = canonical_names().map(|(n, _)| n).collect();
        assert_eq!(names.len(), KEYS.len());
        for letter in 'a'..='z' {
            assert!(key_by_name(&letter.to_string()).is_some(), "{letter}");
        }
        for digit in 0..=9 {
            assert!(key_by_name(&digit.to_string()).is_some(), "{digit}");
        }
        for n in 1..=24 {
            assert!(key_by_name(&format!("f{n}")).is_some(), "f{n}");
        }
        for n in 0..=9 {
            assert!(key_by_name(&format!("kp{n}")).is_some(), "kp{n}");
        }
    }
}
