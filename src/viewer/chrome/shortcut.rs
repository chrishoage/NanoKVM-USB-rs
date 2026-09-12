//! The chrome's two built-in shortcuts, and the send path they take (plan §12 Stage 4b, §2.9,
//! `docs/STAGE3_FINDINGS.md` D1).
//!
//! §12 Stage 4b: *"Keyboard (paste, the two built-in shortcuts through `script::compile`)"* — the
//! reference's `Win+Tab` and `Ctrl+Alt+Del`. The chords are compiled by
//! [`crate::script::compile_key`], so the HID usages come from the one keymap the viewer already
//! forwards through and nothing here retypes a usage.
//!
//! # The send path, and why it is not D1's
//!
//! D1 says a script is **not** "the same queue with a different admission policy": `key`, `type`
//! and `macro` transact one report at a time over their own [`crate::link::Link`], under the
//! `Released` guard, because §2.7's reconnect policy is wrong for a script (a script must stop and
//! say how far it got) and because the queue would coalesce nothing for it.
//!
//! **Neither reason applies to a chrome shortcut, and the alternative is not available anyway.**
//!
//! - *The port is taken.* The viewer's writer thread owns the serial link for the life of the
//!   window. There is no second `Link` for the chrome to transact over; opening the node twice
//!   would either fail or interleave two writers' frames on a chip whose parser has no inter-byte
//!   timeout (§5.1) — the "a truncated serial write corrupts the next command" hazard, with two
//!   writers guaranteeing it.
//! - *D1's first reason inverts.* A script stops and reports its progress because nobody is
//!   watching. Here the user is at the window: §2.7's answer — discard the queue, release
//!   everything, resume disengaged, say so in the title — is exactly what should happen to a
//!   `Ctrl+Alt+Del` whose link died halfway, and it is what happens to every other key the user
//!   presses.
//! - *D1's second reason is vacuous here.* Coalescing exists for pointer motion (§5.1, C8). These
//!   are key events, which §2.2 makes **barriers**: never dropped, never merged, never reordered.
//!   Six events cannot lose one.
//!
//! So a built-in shortcut is submitted through the viewer's own [`crate::input::Producer`] as
//! ordinary key transitions — byte for byte what the host would produce if the user held Ctrl and
//! Alt and tapped Delete themselves, which is the point of the menu item.
//!
//! # Nothing is left held
//!
//! [`crate::script::Script::tap`] is *always* a press followed by `RELEASE_ALL`; a script that
//! could stop between them is what §2.6 forbids. [`events`] turns that pair into transitions by
//! **differencing consecutive reports**, so the trailing `RELEASE_ALL` becomes an explicit key-up
//! for every modifier and every usage the press put down. The writer's held-state tracker (§2.5)
//! therefore sees a matched pair for everything, and
//! [`tests::every_builtin_ends_with_nothing_held`] asserts it rather than trusting it.

use crate::input::Event;
use crate::proto::report::KeyboardReport;
use crate::proto::HidKey;
use crate::script::{compile_key, CompileError, Layout, Script, Step};

/// The shortcuts the chrome offers. The reference's two, and no more: §12 Stage 4b keeps saved
/// user shortcuts and the on-screen keyboard out of scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    /// `Win+Tab` — the target's window switcher.
    WinTab,
    /// `Ctrl+Alt+Del`.
    CtrlAltDel,
}

impl Builtin {
    /// Both of them, in the order the menu lists them.
    pub const ALL: [Builtin; 2] = [Builtin::WinTab, Builtin::CtrlAltDel];

    /// The chord as [`crate::script::compile_key`]'s grammar spells it. This is the authority:
    /// the label below is derived from it, so the two cannot drift.
    pub fn chord(self) -> &'static str {
        match self {
            Builtin::WinTab => "super+tab",
            Builtin::CtrlAltDel => "ctrl+alt+delete",
        }
    }

    /// What the menu item says.
    pub fn label(self) -> &'static str {
        match self {
            Builtin::WinTab => "Win+Tab",
            Builtin::CtrlAltDel => "Ctrl+Alt+Del",
        }
    }
}

/// Compile one shortcut into the key transitions to submit, in order.
///
/// US QWERTY: a chord is *key forwarding*, not text injection, so it is layout-independent —
/// [`crate::script::compile_key`] resolves named keys through the keymap and never through a
/// layout table (§10.2, and `script`'s own module docs). The layout argument is still required by
/// the signature, and `Us` is the declared default (D2).
///
/// # Errors
///
/// [`CompileError`] if a chord does not compile. That cannot happen for [`Builtin::ALL`] — the
/// test below compiles both — but the error is propagated rather than unwrapped, because a
/// panicking menu item in a viewer holding someone's console is not a trade worth making.
pub fn events(builtin: Builtin) -> Result<Vec<Event>, CompileError> {
    let script = compile_key(&[builtin.chord().to_string()], Layout::Us)?;
    Ok(transitions(&script))
}

/// Turn a compiled script's reports into the key transitions that produce them.
///
/// The rule is one difference per report, applied in this order within each step:
///
/// 1. usages that went away, then modifiers that went away;
/// 2. modifiers that arrived, then usages that arrived.
///
/// Releases before presses, and modifiers outside usages, so that the target never observes a key
/// pressed without the modifier it was meant to be pressed under — which is the whole content of a
/// chord.
///
/// Shared with [`super::paste`], which differences a [`crate::script::compile_type`] script the
/// same way: one mapper, one rule about what a report transition is (§10.2 rev 5).
///
/// [`Step::Wait`] is ignored: [`compile_key`] emits none, and the pacing a `macro` file's `wait`
/// expresses belongs to the blocking sender D1 describes, not to a non-blocking submission from an
/// event loop that must not sleep (§2.9).
pub(super) fn transitions(script: &Script) -> Vec<Event> {
    let mut out = Vec::new();
    let mut held = KeyboardReport::RELEASE_ALL;
    for step in &script.steps {
        let Step::Send(outgoing) = step else {
            continue;
        };
        let next = outgoing.report;
        // 1a. usages released
        for usage in held.keys.iter().copied().filter(|u| *u != 0) {
            if !next.keys.contains(&usage) {
                out.push(Event::Key {
                    key: HidKey::Usage(usage),
                    down: false,
                });
            }
        }
        // 1b. modifiers released
        for bit in modifier_bits(held.modifiers & !next.modifiers) {
            out.push(Event::Key {
                key: HidKey::Modifier(bit),
                down: false,
            });
        }
        // 2a. modifiers pressed
        for bit in modifier_bits(next.modifiers & !held.modifiers) {
            out.push(Event::Key {
                key: HidKey::Modifier(bit),
                down: true,
            });
        }
        // 2b. usages pressed
        for usage in next.keys.iter().copied().filter(|u| *u != 0) {
            if !held.keys.contains(&usage) {
                out.push(Event::Key {
                    key: HidKey::Usage(usage),
                    down: true,
                });
            }
        }
        held = next;
    }
    out
}

/// The set bits of a modifier mask, lowest first.
fn modifier_bits(mask: u8) -> impl Iterator<Item = u8> {
    (0..8).map(|i| 1u8 << i).filter(move |b| mask & b != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::report::modifier;
    use std::collections::BTreeSet;

    /// The held set after replaying a transition list, as `(is_modifier, value)` pairs.
    fn replay(events: &[Event]) -> BTreeSet<(bool, u8)> {
        let mut held = BTreeSet::new();
        for e in events {
            let Event::Key { key, down } = e else {
                panic!("a shortcut must emit key events only, got {e:?}");
            };
            let id = match key {
                HidKey::Modifier(b) => (true, *b),
                HidKey::Usage(u) => (false, *u),
            };
            if *down {
                assert!(held.insert(id), "{id:?} was pressed while already held");
            } else {
                assert!(held.remove(&id), "{id:?} was released while not held");
            }
        }
        held
    }

    /// The exit criterion for the send path: **nothing is left held**, and every release matches a
    /// press. A shortcut that left Ctrl down would leave the target unusable and nothing local
    /// could fix it (§2.6.1).
    #[test]
    fn every_builtin_ends_with_nothing_held() {
        for b in Builtin::ALL {
            let events =
                events(b).unwrap_or_else(|e| panic!("{} does not compile: {e}", b.chord()));
            assert!(!events.is_empty(), "{} produced nothing", b.chord());
            let left = replay(&events);
            assert!(left.is_empty(), "{} left {left:?} held", b.chord());
        }
    }

    /// `Ctrl+Alt+Del`, transition by transition: both modifiers down, Delete down, then everything
    /// up. The usages come from the keymap through `compile_key`; the test names the modifier bits
    /// because those are the chord.
    #[test]
    fn ctrl_alt_del_presses_the_modifiers_before_the_key() {
        let events = events(Builtin::CtrlAltDel).expect("compiles");
        let downs: Vec<&Event> = events
            .iter()
            .filter(|e| matches!(e, Event::Key { down: true, .. }))
            .collect();
        assert_eq!(downs.len(), 3, "ctrl, alt, delete: {events:?}");
        assert_eq!(
            downs[0],
            &Event::Key {
                key: HidKey::Modifier(modifier::LEFT_CTRL),
                down: true
            }
        );
        assert_eq!(
            downs[1],
            &Event::Key {
                key: HidKey::Modifier(modifier::LEFT_ALT),
                down: true
            }
        );
        let Event::Key {
            key: HidKey::Usage(_),
            down: true,
        } = downs[2]
        else {
            panic!("the third press must be the Delete usage: {:?}", downs[2]);
        };
    }

    /// `Win+Tab` presses the left meta modifier — the one the reference calls Win — and Tab.
    #[test]
    fn win_tab_presses_meta_and_tab() {
        let events = events(Builtin::WinTab).expect("compiles");
        assert!(
            events.contains(&Event::Key {
                key: HidKey::Modifier(modifier::LEFT_META),
                down: true
            }),
            "{events:?}"
        );
        // Tab's usage comes from the keymap the viewer itself forwards through, resolved from the
        // same name the chord uses — never from a literal retyped here.
        let tab = crate::proto::keymap::hid_key(
            crate::script::keynames::key_by_name("tab").expect("tab is a known key name"),
        )
        .expect("tab has a HID usage");
        assert!(
            events.contains(&Event::Key {
                key: tab,
                down: true
            }),
            "{events:?}"
        );
    }

    /// The labels are for humans and the chords are for the compiler; both must name the same two
    /// shortcuts, and every chord must compile.
    #[test]
    fn the_labels_and_the_chords_agree() {
        assert_eq!(Builtin::WinTab.label(), "Win+Tab");
        assert_eq!(Builtin::CtrlAltDel.label(), "Ctrl+Alt+Del");
        for b in Builtin::ALL {
            assert!(
                compile_key(&[b.chord().to_string()], Layout::Us).is_ok(),
                "{}",
                b.chord()
            );
        }
    }

    /// A report carrying more than one new usage yields one transition each, and the ordering rule
    /// holds: releases first, then modifiers, then usages.
    #[test]
    fn a_hand_built_script_differences_correctly() {
        let mut script = Script::default();
        script.steps.push(Step::Send(crate::script::Outgoing {
            label: "press".to_string(),
            report: KeyboardReport {
                modifiers: modifier::LEFT_SHIFT,
                keys: [0x04, 0x05, 0, 0, 0, 0],
            },
        }));
        script
            .steps
            .push(Step::Wait(std::time::Duration::from_millis(5)));
        script.steps.push(Step::Send(crate::script::Outgoing {
            label: "release".to_string(),
            report: KeyboardReport::RELEASE_ALL,
        }));
        let events = transitions(&script);
        assert_eq!(
            events,
            vec![
                Event::Key {
                    key: HidKey::Modifier(modifier::LEFT_SHIFT),
                    down: true
                },
                Event::Key {
                    key: HidKey::Usage(0x04),
                    down: true
                },
                Event::Key {
                    key: HidKey::Usage(0x05),
                    down: true
                },
                Event::Key {
                    key: HidKey::Usage(0x04),
                    down: false
                },
                Event::Key {
                    key: HidKey::Usage(0x05),
                    down: false
                },
                Event::Key {
                    key: HidKey::Modifier(modifier::LEFT_SHIFT),
                    down: false
                },
            ]
        );
        assert!(replay(&events).is_empty());
    }
}
