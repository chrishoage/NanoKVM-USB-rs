//! Chord parsing into a key and modifier set.
//!
//! Single-character tokens use the target layout before the case-insensitive name table,
//! so `A` retains Shift and differs from `a`.

use winit::keyboard::KeyCode;

use crate::proto::report::modifier;
use crate::script::keynames;
use crate::script::layout::Layout;

/// A resolved chord: the modifier bits to hold, and the one key to press while they are held.
///
/// `key` is `None` for a modifier-only chord. A shift that came from the *layout* (`ctrl+:` on a
/// US board) or from a name (`plus`) is folded into `modifiers`, because that is where the report
/// carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    pub modifiers: u8,
    pub key: Option<KeyCode>,
}

/// Why a chord did not resolve. Every variant names the offending token *and* the whole chord: a
/// macro line names a step, and the user needs both to find it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChordError {
    #[error("an empty string is not a chord")]
    Empty,

    #[error("empty name in chord {chord:?}; a chord is names joined by '+', with none left out")]
    EmptyToken { chord: String },

    #[error("unknown key {token:?} in chord {chord:?}")]
    UnknownKey { token: String, chord: String },

    #[error("{token:?} is not a modifier in chord {chord:?}")]
    NotAModifier { token: String, chord: String },
}

/// Parse a chord using the default US QWERTY layout.
pub fn parse_chord(s: &str) -> Result<Chord, ChordError> {
    parse_chord_with(Layout::default(), s)
}

/// Resolve a chord, using `layout` for a last token that is a single character rather than a name.
pub fn parse_chord_with(layout: Layout, s: &str) -> Result<Chord, ChordError> {
    if s.is_empty() {
        return Err(ChordError::Empty);
    }
    let tokens: Vec<&str> = s.split('+').collect();
    if tokens.iter().any(|t| t.is_empty()) {
        return Err(ChordError::EmptyToken {
            chord: s.to_string(),
        });
    }

    let (last, held) = tokens.split_last().expect("split never yields nothing");
    let mut modifiers = 0u8;
    for token in held {
        modifiers |= keynames::modifier_by_name(token).ok_or_else(|| ChordError::NotAModifier {
            token: (*token).to_string(),
            chord: s.to_string(),
        })?;
    }

    // A modifier last is a modifier-only chord, so `ctrl` and `ctrl+shift` are taps rather than
    // errors. Checked before the key table, which does not know the modifier names at all.
    if let Some(bit) = keynames::modifier_by_name(last) {
        return Ok(Chord {
            modifiers: modifiers | bit,
            key: None,
        });
    }

    let (key, shift) = resolve_key(layout, last).ok_or_else(|| ChordError::UnknownKey {
        token: (*last).to_string(),
        chord: s.to_string(),
    })?;
    if shift {
        modifiers |= modifier::LEFT_SHIFT;
    }
    Ok(Chord {
        modifiers,
        key: Some(key),
    })
}

/// A single character goes through the layout first; anything longer is a name.
///
/// The order matters and used to be the other way round, which made `key A` type `a`: the
/// key-name table is case-insensitive, so `A` matched the name `a` and the shift the character
/// needs was lost. Only the layout knows that `A` is shift+`a` and `!` is shift+`1`, so for a
/// one-character token it is the authority, and the name table is consulted only if the layout
/// cannot reach the character at all. Two characters or more can only be a name (`plus`, `enter`,
/// `f10`), where the table is the authority and case does not matter.
fn resolve_key(layout: Layout, token: &str) -> Option<(KeyCode, bool)> {
    let mut chars = token.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if let Some(found) = layout.key_for_char(c) {
            return Some(found);
        }
    }
    keynames::key_by_name_with_shift(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn chord(s: &str) -> Chord {
        parse_chord(s).unwrap_or_else(|e| panic!("{s:?}: {e}"))
    }

    #[test]
    fn a_chord_is_its_modifier_bits_and_one_key() {
        assert_eq!(
            chord("ctrl+alt+t"),
            Chord {
                modifiers: modifier::LEFT_CTRL | modifier::LEFT_ALT,
                key: Some(KeyCode::KeyT),
            }
        );
        assert_eq!(
            chord("shift+f10"),
            Chord {
                modifiers: modifier::LEFT_SHIFT,
                key: Some(KeyCode::F10),
            }
        );
    }

    /// Modifier-only chords must still produce a press and release.
    #[test]
    fn a_chord_of_only_modifiers_is_legal() {
        assert_eq!(
            chord("super"),
            Chord {
                modifiers: modifier::LEFT_META,
                key: None,
            }
        );
        assert_eq!(
            chord("ctrl+shift"),
            Chord {
                modifiers: modifier::LEFT_CTRL | modifier::LEFT_SHIFT,
                key: None,
            }
        );
    }

    #[test]
    fn a_single_character_is_resolved_through_the_layout() {
        assert_eq!(chord("ctrl+c").key, Some(KeyCode::KeyC));
        // `=` is in the key table by its literal spelling, so no shift.
        assert_eq!(
            chord("shift+="),
            Chord {
                modifiers: modifier::LEFT_SHIFT,
                key: Some(KeyCode::Equal),
            }
        );
        // `:` is not a key name; the layout says shift+semicolon, and the shift joins the bits.
        assert_eq!(
            chord("ctrl+:"),
            Chord {
                modifiers: modifier::LEFT_CTRL | modifier::LEFT_SHIFT,
                key: Some(KeyCode::Semicolon),
            }
        );
    }

    /// `key A` must not type `a`. The name table is case-insensitive, so it answers `A` with the
    /// `a` key and no shift; only the layout knows the character needs one, and a single-character
    /// token is therefore the layout's before it is the table's.
    #[test]
    fn an_uppercase_single_character_keeps_its_shift() {
        assert_eq!(
            chord("A"),
            Chord {
                modifiers: modifier::LEFT_SHIFT,
                key: Some(KeyCode::KeyA),
            }
        );
        assert_eq!(
            chord("a"),
            Chord {
                modifiers: 0,
                key: Some(KeyCode::KeyA),
            }
        );
        assert_ne!(chord("A"), chord("a"), "the shift is the whole difference");
        // The same rule reached through a character whose shifted glyph has no name at all.
        assert_eq!(chord("!"), chord("shift+1"));
    }

    /// Two characters or more are names, and a name that denotes a shifted glyph still carries it:
    /// the change above must not have moved `plus` or the literal aliases.
    #[test]
    fn a_multi_character_token_is_still_a_name() {
        assert_eq!(
            chord("plus"),
            Chord {
                modifiers: modifier::LEFT_SHIFT,
                key: Some(KeyCode::Equal),
            }
        );
        assert_eq!(
            chord("="),
            Chord {
                modifiers: 0,
                key: Some(KeyCode::Equal),
            }
        );
        assert_eq!(chord("ENTER"), chord("enter"));
    }

    /// The grammar splits on `+`, so the literal key needs a word — and that word carries the
    /// shift, because `+` is shift+equal on a US board.
    #[test]
    fn a_literal_plus_is_spelt_plus() {
        assert_eq!(
            chord("plus"),
            Chord {
                modifiers: modifier::LEFT_SHIFT,
                key: Some(KeyCode::Equal),
            }
        );
        assert_eq!(
            chord("ctrl+plus").modifiers,
            modifier::LEFT_CTRL | modifier::LEFT_SHIFT
        );
    }

    #[test]
    fn an_unknown_key_names_the_token_and_the_chord() {
        let err = parse_chord("ctrl+xyz").unwrap_err();
        assert_eq!(
            err.to_string(),
            "unknown key \"xyz\" in chord \"ctrl+xyz\"",
            "the wording is part of the contract"
        );
    }

    #[test]
    fn an_unknown_modifier_names_the_token_and_the_chord() {
        let err = parse_chord("ctlr+c").unwrap_err();
        assert_eq!(
            err.to_string(),
            "\"ctlr\" is not a modifier in chord \"ctlr+c\"",
            "the wording is part of the contract"
        );
    }

    /// A key name in a modifier position is the same mistake as a misspelling and gets the same
    /// error: only a modifier may be held.
    #[test]
    fn a_key_name_may_not_be_held() {
        let err = parse_chord("a+b").unwrap_err();
        assert!(err.to_string().contains("is not a modifier"), "{err}");
    }

    #[test]
    fn empty_tokens_are_errors() {
        for bad in ["ctrl++c", "+a", "a+", "+"] {
            let err = parse_chord(bad).unwrap_err();
            assert!(
                matches!(err, ChordError::EmptyToken { .. }),
                "{bad:?} gave {err:?}"
            );
        }
        assert_eq!(parse_chord(""), Err(ChordError::Empty));
    }

    /// Case is ignored in every *name* — modifiers and multi-character keys alike. It is not
    /// ignored in a single character, because there the case is the character: `ctrl+alt+T`
    /// asks for the shifted key and gets the shift (see
    /// [`an_uppercase_single_character_keeps_its_shift`]).
    #[test]
    fn case_does_not_matter_in_a_name() {
        assert_eq!(chord("CTRL+ALT+enter"), chord("ctrl+alt+ENTER"));
        assert_eq!(
            chord("CTRL+ALT+T"),
            Chord {
                modifiers: modifier::LEFT_CTRL | modifier::LEFT_ALT | modifier::LEFT_SHIFT,
                key: Some(KeyCode::KeyT),
            }
        );
    }

    const MODIFIER_NAMES: [(&str, u8); 8] = [
        ("ctrl", modifier::LEFT_CTRL),
        ("alt", modifier::LEFT_ALT),
        ("shift", modifier::LEFT_SHIFT),
        ("super", modifier::LEFT_META),
        ("rctrl", modifier::RIGHT_CTRL),
        ("ralt", modifier::RIGHT_ALT),
        ("rshift", modifier::RIGHT_SHIFT),
        ("rsuper", modifier::RIGHT_META),
    ];

    proptest! {
        /// Built from the vocabulary, a chord always parses, and its bits are the OR of
        /// the modifiers named — no more (a stray shift) and no fewer (a dropped token).
        #[test]
        fn a_chord_built_from_the_vocabulary_parses_to_the_named_bits(
            held in prop::collection::vec(0usize..8, 0..4),
            key in prop::sample::select(&["a", "z", "9", "f24", "enter", "kp0", "slash"][..]),
        ) {
            let names: Vec<&str> = held.iter().map(|&i| MODIFIER_NAMES[i].0).collect();
            let text = names
                .iter()
                .copied()
                .chain(std::iter::once(key))
                .collect::<Vec<_>>()
                .join("+");
            let parsed = parse_chord(&text).map_err(|e| TestCaseError::fail(e.to_string()))?;
            let want = held.iter().fold(0u8, |bits, &i| bits | MODIFIER_NAMES[i].1);
            prop_assert_eq!(parsed.modifiers, want, "{}", text);
            prop_assert_eq!(parsed.key, keynames::key_by_name(key), "{}", text);
        }

        /// Garbage is an error, never a panic and never a silently different chord.
        #[test]
        fn random_text_never_panics(text in ".{0,24}") {
            let _ = parse_chord(&text);
        }
    }
}
