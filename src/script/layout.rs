//! The declared **target** keyboard layout: which physical key, with or without shift, produces a
//! given character on the far side of the HID cable.
//!
//! §10.2 is the whole reason this type exists. Forwarding a key is layout-independent; turning
//! text into keystrokes is not, because the character a key produces is decided by the *target's*
//! layout, which this host cannot observe. So the layout is declared, never guessed, it is named
//! in help output and in every error, and a character it cannot reach is an error rather than an
//! approximation.
//!
//! Only US QWERTY is implemented. Adding one is adding a variant, its table and its names; nothing
//! else in the crate knows what a layout is.

use std::fmt;
use std::str::FromStr;

use winit::keyboard::KeyCode;

/// A declared target keyboard layout (§10.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// US QWERTY, the default stated in help output rather than assumed silently (§10.2).
    #[default]
    Us,
}

impl Layout {
    /// Every layout this client can declare, for help text and for the error that lists them.
    pub const ALL: &'static [Layout] = &[Layout::Us];

    /// The physical key that produces `c` on this layout, and whether shift is held for it.
    ///
    /// `'\n'` and `'\t'` are the two control characters with a key of their own, so a macro can
    /// carry a newline in its text; every other control character and everything outside ASCII is
    /// `None`, and the caller must refuse rather than approximate (§10.2).
    pub fn key_for_char(self, c: char) -> Option<(KeyCode, bool)> {
        match self {
            Layout::Us => us_key_for_char(c),
        }
    }
}

impl fmt::Display for Layout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Layout::Us => "us",
        })
    }
}

/// A `--layout` value, or a macro's `layout` directive, that names no layout this client has.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown layout {name:?}; supported layouts: {}", supported())]
pub struct LayoutError {
    pub name: String,
}

fn supported() -> String {
    Layout::ALL
        .iter()
        .map(|l| l.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

impl FromStr for Layout {
    type Err = LayoutError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("us") || s.eq_ignore_ascii_case("us-qwerty") {
            return Ok(Layout::Us);
        }
        Err(LayoutError {
            name: s.to_string(),
        })
    }
}

/// US QWERTY, indexed by `c - 'a'`. The *usages* are not here — they come from `proto::keymap`.
#[rustfmt::skip]
const LETTERS: [KeyCode; 26] = {
    use KeyCode as K;
    [
        K::KeyA, K::KeyB, K::KeyC, K::KeyD, K::KeyE, K::KeyF, K::KeyG, K::KeyH, K::KeyI, K::KeyJ,
        K::KeyK, K::KeyL, K::KeyM, K::KeyN, K::KeyO, K::KeyP, K::KeyQ, K::KeyR, K::KeyS, K::KeyT,
        K::KeyU, K::KeyV, K::KeyW, K::KeyX, K::KeyY, K::KeyZ,
    ]
};

/// Indexed by the digit, so `DIGITS[0]` is the `0` key even though it sits after `9` on the row.
#[rustfmt::skip]
const DIGITS: [KeyCode; 10] = {
    use KeyCode as K;
    [
        K::Digit0, K::Digit1, K::Digit2, K::Digit3, K::Digit4, K::Digit5, K::Digit6, K::Digit7,
        K::Digit8, K::Digit9,
    ]
};

/// What each digit key produces with shift held, indexed by the digit (US QWERTY).
const SHIFTED_DIGITS: [char; 10] = [')', '!', '@', '#', '$', '%', '^', '&', '*', '('];

/// `(unshifted, shifted, key)` for the punctuation keys of a US QWERTY board.
#[rustfmt::skip]
const PUNCTUATION: &[(char, char, KeyCode)] = {
    use KeyCode as K;
    &[
        ('-',  '_', K::Minus),        ('=',  '+', K::Equal),
        ('[',  '{', K::BracketLeft),  (']',  '}', K::BracketRight),
        ('\\', '|', K::Backslash),    (';',  ':', K::Semicolon),
        ('\'', '"', K::Quote),        ('`',  '~', K::Backquote),
        (',',  '<', K::Comma),        ('.',  '>', K::Period),
        ('/',  '?', K::Slash),
    ]
};

fn us_key_for_char(c: char) -> Option<(KeyCode, bool)> {
    if c.is_ascii_lowercase() {
        return Some((LETTERS[(c as u8 - b'a') as usize], false));
    }
    if c.is_ascii_uppercase() {
        return Some((LETTERS[(c as u8 - b'A') as usize], true));
    }
    if c.is_ascii_digit() {
        return Some((DIGITS[(c as u8 - b'0') as usize], false));
    }
    if let Some(d) = SHIFTED_DIGITS.iter().position(|&s| s == c) {
        return Some((DIGITS[d], true));
    }
    match c {
        ' ' => return Some((KeyCode::Space, false)),
        // The two control characters with a key of their own.
        '\n' => return Some((KeyCode::Enter, false)),
        '\t' => return Some((KeyCode::Tab, false)),
        _ => {}
    }
    PUNCTUATION
        .iter()
        .find_map(|&(plain, shifted, key)| match c {
            _ if c == plain => Some((key, false)),
            _ if c == shifted => Some((key, true)),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn the_layout_names_are_case_insensitive_and_the_error_lists_what_there_is() {
        for name in ["us", "US", "us-qwerty", "US-QWERTY"] {
            assert_eq!(name.parse::<Layout>(), Ok(Layout::Us), "{name}");
        }
        let err = "de".parse::<Layout>().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("\"de\""), "{msg}");
        assert!(msg.contains("supported layouts: us"), "{msg}");
        assert_eq!(Layout::default(), Layout::Us);
        assert_eq!(Layout::Us.to_string(), "us");
        assert_eq!(Layout::ALL, [Layout::Us].as_slice());
    }

    /// The §10.2 promise for `type`: every character a user can type on an ASCII keyboard is
    /// reachable, so an unreachable character really is exotic.
    #[test]
    fn every_printable_ascii_character_is_reachable() {
        for byte in 0x20u8..=0x7e {
            let c = byte as char;
            assert!(
                Layout::Us.key_for_char(c).is_some(),
                "{c:?} has no key on us"
            );
        }
        assert_eq!(Layout::Us.key_for_char('\n'), Some((KeyCode::Enter, false)));
        assert_eq!(Layout::Us.key_for_char('\t'), Some((KeyCode::Tab, false)));
    }

    /// Injective over printable ASCII: if two characters shared a `(key, shift)` pair, one of them
    /// would silently type the other.
    #[test]
    fn no_two_printable_characters_share_a_keystroke() {
        let mut seen: Vec<(char, (KeyCode, bool))> = Vec::new();
        for byte in 0x20u8..=0x7e {
            let c = byte as char;
            let stroke = Layout::Us.key_for_char(c).expect("printable");
            if let Some((other, _)) = seen.iter().find(|(_, s)| *s == stroke) {
                panic!("{c:?} and {other:?} are both {stroke:?}");
            }
            seen.push((c, stroke));
        }
        assert_eq!(seen.len(), 95);
    }

    #[test]
    fn control_characters_and_non_ascii_are_unreachable() {
        for c in ['\u{0}', '\u{7}', '\r', '\u{1b}', '\u{7f}', 'é', '→', '🙂'] {
            assert_eq!(Layout::Us.key_for_char(c), None, "{c:?}");
        }
    }

    proptest! {
        /// The two properties above, over the whole `char` space rather than a chosen sample.
        #[test]
        fn reachability_follows_the_declared_rule(c in proptest::char::any()) {
            let reachable = Layout::Us.key_for_char(c).is_some();
            let expected = matches!(c, ' '..='~' | '\n' | '\t');
            prop_assert_eq!(reachable, expected, "{:?}", c);
        }

        /// Shift is held for exactly the characters a US board needs it for.
        #[test]
        fn the_shift_bit_matches_the_board(c in 0x20u8..=0x7e) {
            let c = c as char;
            let (_, shift) = Layout::Us.key_for_char(c).unwrap();
            let needs_shift = c.is_ascii_uppercase()
                || SHIFTED_DIGITS.contains(&c)
                || PUNCTUATION.iter().any(|&(_, shifted, _)| shifted == c);
            prop_assert_eq!(shift, needs_shift, "{:?}", c);
        }
    }
}
