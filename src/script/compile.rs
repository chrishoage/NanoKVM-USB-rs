//! The compiler: user text in, a whole [`Script`] or an error out.
//!
//! **Compile everything before opening anything** (§2.8 item 3). Every function here is pure and
//! returns either a script that is fully resolvable to reports or an error that names every
//! problem it found; the caller opens the port only after an `Ok`. That is what makes "fully
//! typeable or not attempted" structural rather than a promise — a half-typed `sudo reboot` on a
//! live console is exactly the partially delivered sequence §2.8 forbids.
//!
//! The unreachable-character policy is §10.2's: fail loudly, name the characters, never
//! approximate — and name *all* of them, so a long line is fixed in one pass rather than one
//! character per run.

use std::time::Duration;

use winit::keyboard::KeyCode;

use crate::proto::keymap::hid_key;
use crate::proto::report::modifier;
use crate::proto::HidKey;
use crate::script::chord::{parse_chord_with, Chord, ChordError};
use crate::script::layout::{Layout, LayoutError};
use crate::script::{Script, Step};

/// Longest a single `wait` step may ask for.
///
/// An hour, which is far past any pacing a macro has a use for — the measured need is 300–500 ms
/// between steps that open a window. The bound is here rather than in the sender because §2.8
/// item 3 puts every refusal *before* the port is opened, and because it is what keeps every
/// deadline the send loop computes representable: `Instant::now() + Duration::from_millis(u64::MAX)`
/// overflows and panics, mid-script, with a key possibly held.
pub const MAX_WAIT: Duration = Duration::from_millis(3_600_000);

/// What the caller decided to do about the target's CapsLock, reduced to the three ways a
/// character can be encoded.
///
/// `GET_INFO` reports the target's lock bits, so the *decision* — warn, refuse, compensate — is
/// the caller's and needs a device. What reaches the compiler is only the encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CapsLock {
    /// CapsLock is known to be off: shift means shift. The ordinary case.
    #[default]
    Off,
    /// CapsLock may be on and the caller chose to type anyway, accepting whatever the target
    /// makes of it. Encoded exactly as [`CapsLock::Off`]; the difference is what the caller was
    /// told, not what goes on the wire.
    Ignore,
    /// CapsLock is on and is to be worked around: the shift bit is inverted for ASCII letters,
    /// which are the only characters the target's CapsLock affects.
    Compensate,
}

impl CapsLock {
    /// The shift bit to send for `c`, given what the layout asked for.
    fn shift_for(self, c: char, shift: bool) -> bool {
        match self {
            CapsLock::Off | CapsLock::Ignore => shift,
            CapsLock::Compensate if c.is_ascii_alphabetic() => !shift,
            CapsLock::Compensate => shift,
        }
    }
}

/// Why a script did not compile. Nothing here is `anyhow`: these are the words the user reads,
/// and they are part of the contract the tests hold (§2.8 item 3).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompileError {
    #[error(transparent)]
    Chord(#[from] ChordError),

    #[error(transparent)]
    Layout(#[from] LayoutError),

    /// A key the tables name but `proto::keymap` does not forward. Unreachable today — the
    /// keyname table is tested against `hid_key` — and kept so that a future table entry fails
    /// loudly instead of encoding usage 0, which the report reads as "no key".
    #[error("proto::keymap does not forward {key:?}, so it cannot be sent")]
    Unforwarded { key: KeyCode },

    #[error("nothing to type")]
    NothingToType,

    #[error("{}", unreachable_message(*layout, chars))]
    Unreachable {
        layout: Layout,
        /// Each unreachable character once, with the 1-based index of its first occurrence, in
        /// the order they first occur.
        chars: Vec<(char, usize)>,
    },

    #[error("the macro contains no steps")]
    EmptyMacro,

    #[error("unknown step {word:?}; expected key, type, wait or layout")]
    UnknownStep { word: String },

    #[error("{step} needs {argument}")]
    MissingArgument {
        step: &'static str,
        argument: &'static str,
    },

    #[error("wait {value:?} is not milliseconds")]
    BadWait { value: String },

    /// A `wait` longer than [`MAX_WAIT`]. Refused at *compile* time, with everything else: a
    /// deadline that far out overflows `Instant` and panics inside the send loop, which is a
    /// crash with a key possibly held rather than an error before anything opened (§2.8 item 3).
    /// An hour is well past any pacing a macro can want, so the bound costs nothing real.
    #[error(
        "wait {ms} ms is longer than the {} ms (1 hour) maximum for one step",
        MAX_WAIT.as_millis()
    )]
    WaitTooLong { ms: u64 },

    /// Every chord of a `key` invocation that did not resolve, so a line is fixed in one pass
    /// rather than one chord per run — the same rule as [`CompileError::Unreachable`].
    #[error("{}", invalid_chords_message(chords))]
    Chords { chords: Vec<ChordError> },

    /// Every macro line that did not compile, in order. See [`compile_macro`].
    #[error("{}", macro_lines_message(lines))]
    MacroLines { lines: Vec<(usize, CompileError)> },

    #[error("layout {found:?} conflicts with --layout {want}")]
    LayoutConflict { found: String, want: Layout },

    #[error("a layout directive must come before the first step")]
    LayoutTooLate,

    /// Everything a macro can go wrong with, carrying the line it went wrong on.
    #[error("macro line {line}: {source}")]
    Macro {
        line: usize,
        source: Box<CompileError>,
    },
}

/// The §10.2 message: every unreachable character, where it first occurs, and the reason the
/// whole thing was refused rather than approximated.
fn unreachable_message(layout: Layout, chars: &[(char, usize)]) -> String {
    let n = chars.len();
    let subject = if n == 1 {
        "character is"
    } else {
        "characters are"
    };
    let list = chars
        .iter()
        .map(|(c, at)| format!("{c:?} (at {at})"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{n} {subject} not reachable on layout {layout}: {list}\ntext injection needs a key for \
         every character (§10.2); nothing was sent."
    )
}

/// Every invalid chord, listed once each in the order given. Mirrors
/// [`unreachable_message`]: the count first, so "two of my chords are wrong" is the first thing
/// read, then each one with the reason it failed.
fn invalid_chords_message(chords: &[ChordError]) -> String {
    let n = chords.len();
    let subject = if n == 1 { "chord is" } else { "chords are" };
    let list = chords
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("; ");
    format!("{n} {subject} invalid: {list}\nnothing was sent.")
}

/// One line per failed macro line, in file order, each carrying its line number — the same words
/// a single failure gets, so a file with three mistakes reads as three of the errors the user
/// already knows rather than as a new format.
fn macro_lines_message(lines: &[(usize, CompileError)]) -> String {
    lines
        .iter()
        .map(|(line, e)| format!("macro line {line}: {e}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The report usage for a physical key. A modifier has no usage — it is a bit of byte 0
/// (Appendix) — and cannot reach here, because `keynames::key_by_name` does not know the modifier
/// names.
fn usage_of(key: KeyCode) -> Result<u8, CompileError> {
    match hid_key(key) {
        Some(HidKey::Usage(usage)) => Ok(usage),
        _ => Err(CompileError::Unforwarded { key }),
    }
}

/// One chord: press with the modifiers held, then release everything.
fn push_chord(script: &mut Script, chord: Chord, label: String) -> Result<(), CompileError> {
    let usage = match chord.key {
        Some(key) => usage_of(key)?,
        // A modifier-only chord presses the bits alone (C7).
        None => 0,
    };
    script.tap(usage, chord.modifiers, label);
    Ok(())
}

/// Compile one or more chords, each a press and a release (§2.8 item 3: all of them or none).
///
/// **Every** bad chord is reported, not the first: `key ctlr+c ctrl+xyz` names both misspellings
/// in one run, exactly as an unreachable character does. Stopping at the first would make fixing
/// a five-chord line a five-run job, with the port opened and a script half typed in between.
pub fn compile_key(chords: &[String], layout: Layout) -> Result<Script, CompileError> {
    let mut script = Script::default();
    let mut invalid: Vec<ChordError> = Vec::new();
    for text in chords {
        match parse_chord_with(layout, text) {
            Ok(chord) => push_chord(&mut script, chord, format!("key {text}"))?,
            Err(e) => invalid.push(e),
        }
    }
    if !invalid.is_empty() {
        return Err(CompileError::Chords { chords: invalid });
    }
    if script.steps.is_empty() {
        return Err(CompileError::MissingArgument {
            step: "key",
            argument: "a chord",
        });
    }
    Ok(script)
}

/// Compile text to one press and one release per character, against the *declared* layout.
///
/// This is text injection, not key forwarding (§10.2): a target on another layout would receive
/// different characters, which is why the layout is declared and named in every error. A
/// character the layout cannot produce is not approximated — every one is collected and reported
/// together, so a line is fixed once rather than a character per run.
pub fn compile_type(text: &str, layout: Layout, caps: CapsLock) -> Result<Script, CompileError> {
    if text.is_empty() {
        return Err(CompileError::NothingToType);
    }

    let mut unreachable: Vec<(char, usize)> = Vec::new();
    for (i, c) in text.chars().enumerate() {
        if layout.key_for_char(c).is_none() && !unreachable.iter().any(|&(seen, _)| seen == c) {
            unreachable.push((c, i + 1));
        }
    }
    if !unreachable.is_empty() {
        return Err(CompileError::Unreachable {
            layout,
            chars: unreachable,
        });
    }

    let mut script = Script::default();
    for c in text.chars() {
        let (key, shift) = layout.key_for_char(c).expect("checked above");
        let modifiers = if caps.shift_for(c, shift) {
            modifier::LEFT_SHIFT
        } else {
            0
        };
        script.tap(usage_of(key)?, modifiers, format!("type {c:?}"));
    }
    Ok(script)
}

/// Compile a macro file: one step per line, `#` comments, blank lines ignored.
///
/// ```text
/// layout us            # optional, before the first step; must agree with --layout if given
/// key ctrl+alt+t
/// type sudo reboot
/// wait 500
/// ```
///
/// `layout` is `Some` when the caller passed `--layout`, in which case a directive that disagrees
/// is an error rather than a silent override — the two say different things about the target and
/// only one of them can be true (§10.2). With `None`, the directive decides, and its absence
/// means [`Layout::default`].
///
/// **A `#` comments out a whole line, never the tail of one.** `type` takes its text verbatim to
/// the end of the line, so a tail comment would be typed on one line and stripped on another;
/// `type echo #1` types `#1`.
///
/// **Every** bad line is reported, not the first. A macro is written in one pass and should be
/// fixable in one: stopping at line 3 of a file whose lines 3, 7 and 11 are wrong means three
/// runs, and nothing is sent on any of them, so there is no reason to hurry the failure.
pub fn compile_macro(
    source: &str,
    layout: Option<Layout>,
    caps: CapsLock,
) -> Result<Script, CompileError> {
    let mut script = Script::default();
    let mut active = layout.unwrap_or_default();
    let mut declared = false;
    let mut failures: Vec<(usize, CompileError)> = Vec::new();

    for (i, raw) in source.lines().enumerate() {
        let line_no = i + 1;
        let line = raw.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // A line that failed contributed no step, so the ones after it are compiled against the
        // same state and their errors are the ones the user will see next time anyway.
        if let Err(e) = compile_line(&mut script, line, &mut active, &mut declared, layout, caps) {
            failures.push((line_no, e));
        }
    }

    match failures.len() {
        0 => {}
        // One failure keeps the wording a single mistake has always had.
        1 => {
            let (line, source) = failures.remove(0);
            return Err(CompileError::Macro {
                line,
                source: Box::new(source),
            });
        }
        _ => return Err(CompileError::MacroLines { lines: failures }),
    }

    if script.steps.is_empty() {
        return Err(CompileError::EmptyMacro);
    }
    Ok(script)
}

/// One macro line, already known to be neither blank nor a comment. Errors here carry no line
/// number; [`compile_macro`] adds it, so every path gets one.
fn compile_line(
    script: &mut Script,
    line: &str,
    active: &mut Layout,
    declared: &mut bool,
    caller: Option<Layout>,
    caps: CapsLock,
) -> Result<(), CompileError> {
    let (word, rest) = match line.split_once(' ') {
        Some((word, rest)) => (word, rest),
        None => (line, ""),
    };

    if word.eq_ignore_ascii_case("layout") {
        let name = rest.trim();
        if name.is_empty() {
            return Err(CompileError::MissingArgument {
                step: "layout",
                argument: "a layout name",
            });
        }
        // Before the first step, because everything compiled so far was compiled against the
        // layout this directive claims to set.
        if !script.steps.is_empty() || *declared {
            return Err(CompileError::LayoutTooLate);
        }
        *declared = true;
        match caller {
            // The conflict is checked on the *name*, before it is parsed, so an unknown layout
            // that also disagrees with `--layout` is reported as the disagreement it is.
            Some(want) if name.parse::<Layout>() != Ok(want) => {
                return Err(CompileError::LayoutConflict {
                    found: name.to_string(),
                    want,
                })
            }
            Some(_) => {}
            None => *active = name.parse::<Layout>()?,
        }
        return Ok(());
    }

    // Any step at all fixes the layout, so a later directive is out of place.
    *declared = true;

    if word.eq_ignore_ascii_case("key") {
        let text = rest.trim();
        if text.is_empty() {
            return Err(CompileError::MissingArgument {
                step: "key",
                argument: "a chord",
            });
        }
        let chord = parse_chord_with(*active, text)?;
        push_chord(script, chord, format!("key {text}"))?;
        return Ok(());
    }

    if word.eq_ignore_ascii_case("type") {
        // Verbatim: exactly one space separates `type` from its text, and everything after it —
        // inner runs of spaces, trailing spaces — is text.
        let typed = compile_type(rest, *active, caps)?;
        script.steps.extend(typed.steps);
        return Ok(());
    }

    if word.eq_ignore_ascii_case("wait") {
        let value = rest.trim();
        let ms: u64 = value.parse().map_err(|_| CompileError::BadWait {
            value: value.to_string(),
        })?;
        let wait = Duration::from_millis(ms);
        if wait > MAX_WAIT {
            return Err(CompileError::WaitTooLong { ms });
        }
        script.steps.push(Step::Wait(wait));
        return Ok(());
    }

    Err(CompileError::UnknownStep {
        word: word.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::report::KeyboardReport;

    fn chords(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    fn reports(script: &Script) -> Vec<KeyboardReport> {
        script
            .steps
            .iter()
            .filter_map(|s| match s {
                Step::Send(out) => Some(out.report),
                Step::Wait(_) => None,
            })
            .collect()
    }

    fn press(modifiers: u8, key: KeyCode) -> KeyboardReport {
        let Some(HidKey::Usage(usage)) = hid_key(key) else {
            panic!("{key:?} has no usage in proto::keymap");
        };
        KeyboardReport {
            modifiers,
            keys: [usage, 0, 0, 0, 0, 0],
        }
    }

    #[test]
    fn a_chord_is_one_press_with_the_modifier_bits_and_one_release() {
        let script = compile_key(&chords(&["ctrl+alt+t"]), Layout::Us).expect("compiles");
        assert_eq!(
            reports(&script),
            vec![
                press(modifier::LEFT_CTRL | modifier::LEFT_ALT, KeyCode::KeyT),
                KeyboardReport::RELEASE_ALL,
            ]
        );
        assert_eq!(script.report_count(), 2);
    }

    /// C7: `super` alone is a tap of the bit with no usage in any slot — and it is followed by a
    /// release, like every other press.
    #[test]
    fn a_modifier_only_chord_presses_the_bits_alone() {
        let script = compile_key(&chords(&["super"]), Layout::Us).expect("compiles");
        assert_eq!(
            reports(&script),
            vec![
                KeyboardReport {
                    modifiers: modifier::LEFT_META,
                    keys: [0; 6],
                },
                KeyboardReport::RELEASE_ALL,
            ]
        );
    }

    #[test]
    fn several_chords_compile_in_order() {
        let script = compile_key(&chords(&["ctrl+c", "enter"]), Layout::Us).expect("compiles");
        assert_eq!(
            reports(&script),
            vec![
                press(modifier::LEFT_CTRL, KeyCode::KeyC),
                KeyboardReport::RELEASE_ALL,
                press(0, KeyCode::Enter),
                KeyboardReport::RELEASE_ALL,
            ]
        );
    }

    /// §2.8 item 3: one bad chord and the whole invocation produces nothing to half-deliver.
    #[test]
    fn an_unknown_key_name_is_rejected_and_produces_nothing() {
        let err = compile_key(&chords(&["enter", "ctrl+xyz"]), Layout::Us).unwrap_err();
        assert_eq!(
            err.to_string(),
            "1 chord is invalid: unknown key \"xyz\" in chord \"ctrl+xyz\"\nnothing was sent."
        );
    }

    /// Every bad chord, not the first: fixing a line should be one edit and one run, not one run
    /// per mistake — the rule the unreachable-character error already follows.
    #[test]
    fn every_invalid_chord_is_reported_in_one_error() {
        let err = compile_key(&chords(&["ctlr+c", "enter", "ctrl+xyz"]), Layout::Us).unwrap_err();
        let msg = err.to_string();
        assert!(msg.starts_with("2 chords are invalid: "), "{msg}");
        assert!(
            msg.contains("\"ctlr\" is not a modifier in chord \"ctlr+c\""),
            "{msg}"
        );
        assert!(
            msg.contains("unknown key \"xyz\" in chord \"ctrl+xyz\""),
            "{msg}"
        );
        assert!(msg.contains("nothing was sent."), "{msg}");
    }

    #[test]
    fn text_is_one_press_and_release_per_character_with_shift_where_the_board_needs_it() {
        let script = compile_type("Hello, World!\n", Layout::Us, CapsLock::Off).expect("compiles");
        assert_eq!(script.report_count(), 2 * 14);
        let got = reports(&script);
        let shifted: Vec<usize> = got
            .iter()
            .enumerate()
            .filter(|(_, r)| r.modifiers == modifier::LEFT_SHIFT)
            .map(|(i, _)| i / 2)
            .collect();
        // 'H' at 0, 'W' at 7, '!' at 12.
        assert_eq!(shifted, vec![0, 7, 12]);
        // Every odd report is a release: a press is never left standing.
        for (i, report) in got.iter().enumerate() {
            if i % 2 == 1 {
                assert_eq!(*report, KeyboardReport::RELEASE_ALL, "report {i}");
            } else {
                assert_ne!(*report, KeyboardReport::RELEASE_ALL, "report {i}");
            }
        }
        assert_eq!(got[0], press(modifier::LEFT_SHIFT, KeyCode::KeyH));
        assert_eq!(got[2], press(0, KeyCode::KeyE));
        assert_eq!(got[26], press(0, KeyCode::Enter));
    }

    /// Compensating for a CapsLock that is on inverts the shift for letters and only letters: the
    /// target's CapsLock does not touch `!` or `,`.
    #[test]
    fn compensating_for_capslock_inverts_the_shift_on_letters_only() {
        let script =
            compile_type("Hello, World!\n", Layout::Us, CapsLock::Compensate).expect("compiles");
        let got = reports(&script);
        assert_eq!(got[0], press(0, KeyCode::KeyH), "H becomes unshifted");
        assert_eq!(
            got[2],
            press(modifier::LEFT_SHIFT, KeyCode::KeyE),
            "e becomes shifted"
        );
        // ',' is index 5, '!' index 12.
        assert_eq!(got[10], press(0, KeyCode::Comma), "the comma is untouched");
        assert_eq!(
            got[24],
            press(modifier::LEFT_SHIFT, KeyCode::Digit1),
            "the bang is untouched"
        );
    }

    #[test]
    fn ignoring_capslock_encodes_exactly_as_off_does() {
        let off = compile_type("Ab!", Layout::Us, CapsLock::Off).expect("compiles");
        let ignore = compile_type("Ab!", Layout::Us, CapsLock::Ignore).expect("compiles");
        assert_eq!(reports(&off), reports(&ignore));
    }

    /// §10.2: every unreachable character, once each, with where it first occurs — and no script.
    #[test]
    fn unreachable_characters_are_all_listed_once_with_their_positions() {
        let err = compile_type("abcdé123→ab\u{7}é", Layout::Us, CapsLock::Off).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.starts_with("3 characters are not reachable on layout us: "),
            "{msg}"
        );
        assert!(msg.contains("'é' (at 5)"), "{msg}");
        assert!(msg.contains("'→' (at 9)"), "{msg}");
        assert!(msg.contains("'\\u{7}' (at 12)"), "{msg}");
        assert!(
            msg.contains(
                "text injection needs a key for every character (§10.2); nothing was sent."
            ),
            "{msg}"
        );
        assert_eq!(msg.matches('é').count(), 1, "deduplicated: {msg}");
    }

    #[test]
    fn one_unreachable_character_reads_as_one() {
        let err = compile_type("naïve", Layout::Us, CapsLock::Off).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("1 character is not reachable on layout us: 'ï' (at 3)"),
            "{err}"
        );
        assert!(compile_type("naive", Layout::Us, CapsLock::Off).is_ok());
    }

    #[test]
    fn empty_text_is_refused() {
        assert_eq!(
            compile_type("", Layout::Us, CapsLock::Off)
                .unwrap_err()
                .to_string(),
            "nothing to type"
        );
    }

    // -- macros -------------------------------------------------------------

    #[test]
    fn a_macro_ignores_comments_and_blank_lines() {
        let source = "# a comment\n\n   \nkey enter\n# another\n";
        let script = compile_macro(source, None, CapsLock::Off).expect("compiles");
        assert_eq!(script.report_count(), 2);
    }

    #[test]
    fn a_macro_compiles_its_four_steps_in_order() {
        let source = "layout us\nkey ctrl+alt+t\ntype ab\nwait 500\n";
        let script = compile_macro(source, None, CapsLock::Off).expect("compiles");
        assert_eq!(script.steps.len(), 2 + 4 + 1);
        assert_eq!(
            script.steps.last(),
            Some(&Step::Wait(Duration::from_millis(500)))
        );
        assert_eq!(script.report_count(), 6);
    }

    /// `type` takes the rest of the line verbatim after exactly one separating space, so leading,
    /// inner and trailing spaces are all text. A tail `#` is text too.
    #[test]
    fn type_text_is_verbatim_after_one_space() {
        let script =
            compile_macro("type  a  b \ntype echo #1\n", None, CapsLock::Off).expect("compiles");
        let labels: Vec<String> = script
            .steps
            .iter()
            .filter_map(|s| match s {
                Step::Send(out) if out.label.starts_with("type") => Some(out.label.clone()),
                _ => None,
            })
            .collect();
        let typed: String = labels
            .iter()
            .map(|l| l.trim_start_matches("type ").trim_matches('\'').to_string())
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(typed, " a  b echo #1");
    }

    #[test]
    fn a_layout_directive_that_agrees_with_the_caller_is_accepted() {
        for directive in ["layout us", "layout us-qwerty", "layout US"] {
            let source = format!("{directive}\nkey a\n");
            assert!(
                compile_macro(&source, Some(Layout::Us), CapsLock::Off).is_ok(),
                "{directive}"
            );
        }
    }

    #[test]
    fn a_layout_directive_that_conflicts_with_the_caller_is_an_error() {
        let err = compile_macro("layout de\nkey a\n", Some(Layout::Us), CapsLock::Off).unwrap_err();
        assert_eq!(
            err.to_string(),
            "macro line 1: layout \"de\" conflicts with --layout us"
        );
    }

    #[test]
    fn a_layout_directive_the_caller_did_not_contradict_must_still_name_a_layout() {
        let err = compile_macro("layout de\nkey a\n", None, CapsLock::Off).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("macro line 1: unknown layout \"de\""),
            "{err}"
        );
    }

    #[test]
    fn a_layout_directive_after_the_first_step_is_an_error() {
        let err = compile_macro("key a\nlayout us\n", None, CapsLock::Off).unwrap_err();
        assert_eq!(
            err.to_string(),
            "macro line 2: a layout directive must come before the first step"
        );
    }

    #[test]
    fn macro_errors_carry_the_line_number() {
        let source = "# one\n\nkey enter\ntype ok\nkey a\nwait 10\ntpye x\n";
        let err = compile_macro(source, None, CapsLock::Off).unwrap_err();
        assert_eq!(
            err.to_string(),
            "macro line 7: unknown step \"tpye\"; expected key, type, wait or layout"
        );

        let err =
            compile_macro("key a\ntype b\nwait 5\nwait abc\n", None, CapsLock::Off).unwrap_err();
        assert_eq!(
            err.to_string(),
            "macro line 4: wait \"abc\" is not milliseconds"
        );

        let err = compile_macro("key a\nkey ctrl+xyz\n", None, CapsLock::Off).unwrap_err();
        assert_eq!(
            err.to_string(),
            "macro line 2: unknown key \"xyz\" in chord \"ctrl+xyz\""
        );

        let err = compile_macro("type é\n", None, CapsLock::Off).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("macro line 1: 1 character is not reachable"),
            "{err}"
        );
    }

    /// A file with three mistakes is fixed in one pass: every failing line is reported, in order,
    /// each with its own line number and its own words.
    #[test]
    fn every_failing_macro_line_is_reported_in_one_error() {
        let source = "key ctrl+xyz\nkey enter\ntpye x\ntype é\n";
        let err = compile_macro(source, None, CapsLock::Off).unwrap_err();
        let message = err.to_string();
        let lines: Vec<&str> = message.lines().collect();
        assert_eq!(
            lines[0],
            "macro line 1: unknown key \"xyz\" in chord \"ctrl+xyz\""
        );
        assert_eq!(
            lines[1],
            "macro line 3: unknown step \"tpye\"; expected key, type, wait or layout"
        );
        assert!(
            lines[2].starts_with("macro line 4: 1 character is not reachable"),
            "{:?}",
            lines[2]
        );
    }

    /// C3: a `wait` whose deadline cannot be represented used to panic inside the send loop, with
    /// a key possibly held. It is refused at compile time instead, and the limit is named.
    #[test]
    fn a_wait_longer_than_an_hour_is_refused_at_compile_time() {
        for value in ["18446744073709551615", "3600001"] {
            let err = compile_macro(&format!("wait {value}\n"), None, CapsLock::Off).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("3600000 ms"), "{value}: {msg}");
            assert!(msg.contains("1 hour"), "{value}: {msg}");
        }
        assert!(compile_macro("wait 3600000\n", None, CapsLock::Off).is_ok());
        // A value that is not a number at all is still the other error.
        assert_eq!(
            compile_macro("wait abc\n", None, CapsLock::Off)
                .unwrap_err()
                .to_string(),
            "macro line 1: wait \"abc\" is not milliseconds"
        );
    }

    #[test]
    fn a_macro_with_no_steps_is_an_error() {
        for source in ["", "# only a comment\n", "\n\n   \n", "layout us\n"] {
            assert_eq!(
                compile_macro(source, None, CapsLock::Off)
                    .unwrap_err()
                    .to_string(),
                "the macro contains no steps",
                "{source:?}"
            );
        }
    }

    #[test]
    fn a_step_without_its_argument_is_an_error() {
        for (source, want) in [
            ("key\n", "macro line 1: key needs a chord"),
            ("key   \n", "macro line 1: key needs a chord"),
            ("layout\n", "macro line 1: layout needs a layout name"),
        ] {
            assert_eq!(
                compile_macro(source, None, CapsLock::Off)
                    .unwrap_err()
                    .to_string(),
                want,
                "{source:?}"
            );
        }
        // `type` with nothing after it is the same mistake, in `compile_type`'s words.
        assert_eq!(
            compile_macro("type\n", None, CapsLock::Off)
                .unwrap_err()
                .to_string(),
            "macro line 1: nothing to type"
        );
    }

    /// The macro's declared layout is the one its `type` lines compile against.
    #[test]
    fn the_directive_supplies_the_layout_when_the_caller_did_not() {
        let script =
            compile_macro("layout us-qwerty\ntype a\n", None, CapsLock::Off).expect("compiles");
        assert_eq!(reports(&script)[0], press(0, KeyCode::KeyA));
    }
}
