//! Pure compilation of chords, text, and macros into keyboard reports.
//!
//! Compilation collects input errors before device access. Text mapping uses a declared
//! target layout and rejects unsupported characters. The viewer and CLI share this mapper;
//! the CLI sends compiled reports directly, while the viewer submits key transitions.

pub mod chord;
pub mod compile;
pub mod dry_run;
pub mod fixtures;
pub mod keynames;
pub mod layout;

use std::time::Duration;

use crate::proto::report::KeyboardReport;

pub use chord::{parse_chord, parse_chord_with, Chord, ChordError};
pub use compile::{compile_key, compile_macro, compile_type, CapsLock, CompileError};
pub use dry_run::render;
pub use fixtures::{Fixture, FixtureError, Fixtures};
pub use layout::{Layout, LayoutError};

/// Default milliseconds between CLI reports and between viewer paste transitions.
/// The 40 ms pacing gives the target desktop time to process input; bridge
/// acknowledgements alone can arrive faster than the target accepts typing.
pub const REPORT_DELAY_MS: u64 = 40;

/// The chord and macro grammar, as the user reads it.
///
/// Lives here rather than in the CLI because the compiler is what enforces it: the two would
/// otherwise drift, and help text that describes a grammar the compiler does not accept is worse
/// than none.
pub const GRAMMAR: &str = "\
Key chords, for example ctrl+alt+t, shift+f10, super, ctrl+c:
  modifiers:  ctrl alt shift super  (right-hand: rctrl ralt rshift rsuper)
  keys:       a-z 0-9 f1-f24 enter esc tab space backspace delete insert home end
              pgup pgdn up down left right capslock numlock scrolllock printscreen
              pause menu kp0-kp9 kpenter kpplus kpminus kpmultiply kpdivide kpdecimal
              minus equal bracketleft bracketright backslash semicolon quote backquote
              comma period slash plus, or any single printable character on the layout
  A chord of only modifiers taps them (the target decides their action).

Macro files use one step per line, `#` comments, blank lines ignored:
  layout us            # optional, first non-comment line; must agree with --layout if given
  key ctrl+alt+t       # one chord
  type hello     # text after one space, to end of line, verbatim
  wait 500             # milliseconds
";

/// One report and the script fragment that produced it, so a failure can name the keystroke —
/// "it failed" and "it failed after typing `sudo reb`" are different facts to the person holding
/// the console.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    pub label: String,
    pub report: KeyboardReport,
}

/// One thing to do, in order. A script is these and nothing else: no branching, no state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Send(Outgoing),
    Wait(Duration),
}

/// A compiled script: every step already resolved to a report.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Script {
    pub steps: Vec<Step>,
}

impl Script {
    /// How many reports this script sends. Not `steps.len()`: a wait sends nothing.
    pub fn report_count(&self) -> usize {
        self.steps
            .iter()
            .filter(|s| matches!(s, Step::Send(_)))
            .count()
    }

    /// Press with `modifiers` held, then release everything. Two steps, always in that order: the
    /// release is not optional, because a script that aborts between them leaves the key held on
    /// the target.
    ///
    /// A `usage` of zero is the report's "no key", which is what a modifier-only chord presses.
    pub(crate) fn tap(&mut self, usage: u8, modifiers: u8, label: String) {
        self.steps.push(Step::Send(Outgoing {
            label,
            report: KeyboardReport {
                modifiers,
                keys: [usage, 0, 0, 0, 0, 0],
            },
        }));
        self.steps.push(Step::Send(Outgoing {
            label: "release".to_string(),
            report: KeyboardReport::RELEASE_ALL,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tap_is_a_press_then_a_release_and_counts_two_reports() {
        let mut script = Script::default();
        script.tap(0x04, 0, "key a".to_string());
        script.steps.push(Step::Wait(Duration::from_millis(5)));
        assert_eq!(script.steps.len(), 3);
        assert_eq!(script.report_count(), 2, "a wait sends nothing");
        let Step::Send(release) = &script.steps[1] else {
            panic!("the second step must be the release");
        };
        assert_eq!(release.report, KeyboardReport::RELEASE_ALL);
    }

    /// The help text and the compiler have to agree, so the words the grammar advertises are
    /// checked against the tables that implement it.
    #[test]
    fn the_grammar_text_names_only_words_the_compiler_accepts() {
        for name in ["enter", "kpdecimal", "printscreen", "plus", "f24", "menu"] {
            assert!(GRAMMAR.contains(name), "the grammar should list {name}");
            assert!(
                keynames::key_by_name(name).is_some(),
                "{name} is advertised but unknown"
            );
        }
        for name in [
            "ctrl", "alt", "shift", "super", "rctrl", "ralt", "rshift", "rsuper",
        ] {
            assert!(GRAMMAR.contains(name), "the grammar should list {name}");
            assert!(keynames::modifier_by_name(name).is_some(), "{name}");
        }
        for step in ["layout us", "key ctrl+alt+t", "type hello", "wait 500"] {
            assert!(GRAMMAR.contains(step), "the grammar should show {step}");
        }
        assert!(compile_macro(
            "layout us\nkey ctrl+alt+t\ntype hello\nwait 500\n",
            None,
            CapsLock::Off
        )
        .is_ok());
    }
}
