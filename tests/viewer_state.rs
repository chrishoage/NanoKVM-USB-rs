//! The viewer's pure surfaces: the capture reducer, the letterbox geometry and the cursor mapping
//! (plan §2.6, §2.8, §3.4, §12 Stage 1).
//!
//! Everything here is a function of its arguments. There is no window, no GPU, no producer and no
//! clock, so none of it needs a display and none of it is timing-dependent. The properties that
//! matter — "a forwarded press always has a forwardable release", "leaving capture always runs the
//! full release sequence", "a viewport is always inside its attachment" — are asserted over swept
//! inputs rather than over one happy path, because each of them was broken by an input nobody had
//! thought to try.
//!
//! The end-to-end half, where the actions are performed against a real writer thread, is
//! `viewer_capture_session.rs`.

use nanokvm::input::{Event, ReleaseOutcome, ReleaseReason, ReleaseRecord, SubmitError};
use nanokvm::proto::report::{button, ABS_MAX};
use nanokvm::proto::HidKey;
use nanokvm::viewer::input_map::{self, KeyAction};
use nanokvm::viewer::render::{letterbox, Rect};
use nanokvm::viewer::state::{
    reduce, self_release_trigger, Action, CaptureState, Notice, ReleaseNotice, Session, Trigger,
};

fn enter() -> HidKey {
    input_map::enter_key().expect("Enter is mapped")
}

fn left(down: bool) -> Trigger {
    Trigger::Button {
        mask: button::LEFT,
        down,
    }
}

fn every_trigger() -> Vec<Trigger> {
    vec![
        Trigger::Key {
            key: HidKey::Usage(0x04),
            down: true,
        },
        Trigger::Key {
            key: HidKey::Usage(0x04),
            down: false,
        },
        Trigger::Key {
            key: enter(),
            down: true,
        },
        Trigger::Key {
            key: enter(),
            down: false,
        },
        left(true),
        left(false),
        Trigger::Button {
            mask: button::RIGHT,
            down: true,
        },
        Trigger::Button {
            mask: button::RIGHT,
            down: false,
        },
        Trigger::Motion(Event::PointerAbs { x: 1, y: 1 }),
        Trigger::Motion(Event::PointerRel { dx: 1, dy: 1 }),
        Trigger::Motion(Event::Wheel { delta: 1 }),
        Trigger::ReleaseKey,
        Trigger::FocusLost,
        Trigger::InhibitorInactive,
        Trigger::SubmitFailed(SubmitError::Overflow),
        Trigger::SubmitFailed(SubmitError::LinkDown),
        Trigger::SubmitFailed(SubmitError::Disengaged),
        Trigger::SubmitFailed(SubmitError::ShuttingDown),
        Trigger::CloseRequested,
        Trigger::EngageSucceeded,
        Trigger::EngageDeferred,
        Trigger::Retry,
    ]
}

/// A session in each phase, reached the way a user reaches it.
fn each_phase() -> Vec<Session> {
    let engaging = reduce(Session::new(), left(true)).0;
    let captured = reduce(engaging, Trigger::EngageSucceeded).0;
    // …and with the capture click let go of, which is where a user spends almost all their time.
    let settled = reduce(captured, left(false)).0;
    vec![Session::new(), engaging, captured, settled]
}

/// What the target's button mask would be after these actions, applied the way the writer applies
/// them (`input::held`).
fn apply(mask: &mut u8, actions: &[Action]) {
    for a in actions {
        match a {
            Action::Forward(Event::Button { button, down }) => {
                if *down {
                    *mask |= button;
                } else {
                    *mask &= !button;
                }
            }
            // §2.6: the writer synthesizes a full release and clears its own held masks.
            Action::ReleaseAll(_) => *mask = 0,
            _ => {}
        }
    }
}

/// Drives a session through triggers, tracking what the target would be holding.
struct Target {
    session: Session,
    buttons: u8,
    keys_down: Vec<HidKey>,
}

impl Target {
    fn new() -> Self {
        Target {
            session: Session::new(),
            buttons: 0,
            keys_down: Vec::new(),
        }
    }

    fn feed(&mut self, trigger: Trigger) -> Vec<Action> {
        let (next, actions) = reduce(self.session, trigger);
        self.session = next;
        apply(&mut self.buttons, &actions);
        for a in &actions {
            if let Action::Forward(Event::Key { key, down }) = a {
                if *down {
                    self.keys_down.push(*key);
                } else {
                    self.keys_down.retain(|k| k != key);
                }
            }
            if matches!(a, Action::ReleaseAll(_)) {
                self.keys_down.clear();
            }
        }
        actions
    }

    /// The engage answer a writer that has already acknowledged would give.
    fn engage_succeeds(&mut self) {
        self.feed(Trigger::Retry);
        self.feed(Trigger::EngageSucceeded);
        assert_eq!(self.session.capture(), CaptureState::Captured);
    }
}

// ---------------------------------------------------------------------------------------------
// H1 — the letterbox rectangle is the viewport handed to wgpu, and a viewport outside its
// attachment is a validation error, i.e. a panic on the render thread.
// ---------------------------------------------------------------------------------------------

#[test]
fn h1_letterbox_rect_is_always_inside_the_window() {
    let mut bad: Vec<(u32, u32, u32, u32, Rect)> = Vec::new();
    let windows = [
        (1, 1),
        (7, 3),
        (49, 7),
        (640, 480),
        (1280, 720),
        (1904, 2087),
        (3200, 1800),
        (3840, 2160),
        (16383, 1),
    ];
    let frames = [
        (1, 1),
        (3, 7),
        (49, 7),
        (640, 480),
        (1280, 1024),
        (1920, 1080),
        (3840, 2160),
    ];
    for w in windows {
        for f in frames {
            let Some(r) = letterbox(w, f) else { continue };
            let inside = r.x >= 0.0
                && r.y >= 0.0
                && r.x + r.width <= w.0 as f32
                && r.y + r.height <= w.1 as f32;
            if !inside {
                bad.push((w.0, w.1, f.0, f.1, r));
            }
        }
    }
    assert!(bad.is_empty(), "viewport outside the attachment: {bad:?}");
}

/// The same property swept hard over odd sizes: this is where the floating-point rounding shows
/// up, as an origin of about -1e-12 that `set_viewport` rejects.
#[test]
fn h1_letterbox_sweep_never_escapes_the_window() {
    let mut bad = Vec::new();
    for ww in 1..400u32 {
        for wh in [1u32, 3, 17, 199, 1080] {
            for f in [(1920u32, 1080u32), (1280, 1024), (7, 3), (65535, 1)] {
                let Some(r) = letterbox((ww, wh), f) else {
                    continue;
                };
                if r.x < 0.0 || r.y < 0.0 || r.x + r.width > ww as f32 || r.y + r.height > wh as f32
                {
                    bad.push(((ww, wh), f, r));
                }
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} escapes, e.g. {:?}",
        bad.len(),
        bad.first()
    );
}

// ---------------------------------------------------------------------------------------------
// H2 — an absolute coordinate outside 0..=4095 would jump the pointer somewhere arbitrary on a
// live console (§3.4).
// ---------------------------------------------------------------------------------------------

#[test]
fn h2_map_cursor_is_always_in_range() {
    let rects = [
        Rect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        },
        Rect {
            x: -10.0,
            y: -10.0,
            width: 1920.0,
            height: 1080.0,
        },
        Rect {
            x: 100.5,
            y: 50.25,
            width: 963.75,
            height: 541.5,
        },
        Rect {
            x: 0.0,
            y: 0.0,
            width: f32::MAX,
            height: f32::MAX,
        },
    ];
    let frames = [
        (1u32, 1u32),
        (1920, 1080),
        (3840, 2160),
        (65535, 65535),
        (8192, 8192),
    ];
    let positions = [
        (0.0, 0.0),
        (-1e9, -1e9),
        (1e9, 1e9),
        (f64::INFINITY, f64::INFINITY),
        (f64::NEG_INFINITY, 0.0),
        (1919.9999, 1079.9999),
    ];
    for r in rects {
        for f in frames {
            for p in positions {
                if let Some((x, y)) = input_map::map_cursor(p, r, f) {
                    assert!(x <= ABS_MAX && y <= ABS_MAX, "{p:?} {r:?} {f:?} -> {x},{y}");
                }
            }
        }
    }
}

/// A NaN position, which winit never produces. Documenting the guard rather than requiring a
/// particular answer: whatever comes out must still be a coordinate the device can accept.
#[test]
fn h2_map_cursor_with_a_nan_position_stays_in_range() {
    let r = Rect {
        x: 0.0,
        y: 0.0,
        width: 1920.0,
        height: 1080.0,
    };
    if let Some((x, y)) = input_map::map_cursor((f64::NAN, f64::NAN), r, (1920, 1080)) {
        assert!(x <= ABS_MAX && y <= ABS_MAX);
    }
}

// ---------------------------------------------------------------------------------------------
// M2 — the cursor is mapped through the window size the event loop currently holds, so a resize
// is reflected by the very next cursor position rather than after the render thread catches up.
// ---------------------------------------------------------------------------------------------

#[test]
fn m2_a_resize_changes_the_next_cursor_mapping() {
    let frame = (1920u32, 1080u32);
    let before = (1280u32, 720u32);
    let after = (2560u32, 1440u32);
    // The same physical pixel of the window, before and after the window doubles in size.
    let pos = (640.0, 360.0);

    let old = input_map::map_cursor_in_window(pos, before, frame).expect("mapped");
    let new = input_map::map_cursor_in_window(pos, after, frame).expect("mapped");

    // The centre of the old window is a quarter of the way into the new one, so the coordinate
    // must move. A mapping that kept using the pre-resize rectangle would return `old` and put the
    // pointer in the middle of the target's screen instead.
    assert_ne!(
        old, new,
        "the mapping ignored the new window size: {old:?} both times"
    );
    // And it agrees exactly with the rectangle for the size it was given.
    let rect = letterbox(after, frame).expect("rect");
    assert_eq!(
        new,
        input_map::map_cursor(pos, rect, frame).expect("mapped")
    );
}

#[test]
fn m2_a_degenerate_window_or_frame_maps_to_nothing() {
    // §3.4: there is no defensible coordinate here, and inventing one moves a pointer on a live
    // console.
    assert_eq!(
        input_map::map_cursor_in_window((0.0, 0.0), (0, 720), (1920, 1080)),
        None
    );
    assert_eq!(
        input_map::map_cursor_in_window((0.0, 0.0), (1280, 720), (0, 1080)),
        None
    );
}

// ---------------------------------------------------------------------------------------------
// H3 — every path out of capture runs the whole §2.6 sequence, and nothing is forwarded outside
// `Captured`.
// ---------------------------------------------------------------------------------------------

#[test]
fn h3_leaving_capture_always_runs_the_full_release_sequence() {
    for from in each_phase() {
        if from.capture() == CaptureState::Released {
            continue;
        }
        for t in every_trigger() {
            let (next, actions) = reduce(from, t);
            if next.capture() == CaptureState::Released {
                assert!(
                    actions.iter().any(|a| matches!(a, Action::ReleaseAll(_))),
                    "{from:?} + {t:?} -> Released with no ReleaseAll: {actions:?}"
                );
                assert!(
                    actions.contains(&Action::DisarmInhibitor),
                    "{from:?} + {t:?} -> Released with no DisarmInhibitor"
                );
                assert!(
                    actions.contains(&Action::ReleasePointer),
                    "{from:?} + {t:?} -> Released with no ReleasePointer"
                );
            }
        }
    }
}

#[test]
fn h3_no_trigger_forwards_outside_captured() {
    for from in each_phase() {
        if from.capture() == CaptureState::Captured {
            continue;
        }
        for t in every_trigger() {
            let (_, actions) = reduce(from, t);
            assert!(
                !actions.iter().any(|a| matches!(a, Action::Forward(_))),
                "{from:?} + {t:?} forwarded: {actions:?}"
            );
        }
    }
}

/// Entering capture arms the inhibitor and takes the grab before anything can be forwarded, from
/// either binding.
#[test]
fn h3_entering_capture_arms_before_engaging() {
    for t in [
        left(true),
        Trigger::Key {
            key: enter(),
            down: true,
        },
    ] {
        let (next, actions) = reduce(Session::new(), t);
        assert_eq!(next.capture(), CaptureState::Engaging, "{t:?}");
        assert_eq!(
            actions,
            vec![Action::ArmInhibitor, Action::GrabPointer, Action::TryEngage],
            "{t:?}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// N6 — the `Enter` that captures is consumed on BOTH edges. Its key-up used to be forwarded,
// harmless only by accident: `HeldState::apply_key` happens to absorb an up for a key that is not
// held, so a target that behaved differently would have seen half a keystroke.
// ---------------------------------------------------------------------------------------------

#[test]
fn n6_the_enter_that_captures_does_not_leak_its_key_up() {
    let mut t = Target::new();
    t.feed(Trigger::Key {
        key: enter(),
        down: true,
    });
    t.engage_succeeds();
    let actions = t.feed(Trigger::Key {
        key: enter(),
        down: false,
    });
    assert!(
        !actions.iter().any(|a| matches!(a, Action::Forward(_))),
        "the capture key's own key-up reached the target: {actions:?}"
    );
    assert!(t.keys_down.is_empty());
}

/// …and an `Enter` pressed *while captured* is an ordinary keystroke, forwarded on both edges.
#[test]
fn n6_an_enter_pressed_while_captured_is_forwarded_both_ways() {
    let mut t = Target::new();
    t.feed(left(true));
    t.engage_succeeds();
    t.feed(left(false));

    t.feed(Trigger::Key {
        key: enter(),
        down: true,
    });
    assert_eq!(
        t.keys_down,
        vec![enter()],
        "the press should reach the target"
    );
    t.feed(Trigger::Key {
        key: enter(),
        down: false,
    });
    assert!(t.keys_down.is_empty(), "and so should the release");
}

// ---------------------------------------------------------------------------------------------
// B1 — a consumed press must not swallow a later, genuine release. The defect: a button-UP that
// arrived while `Released`/`Engaging` left the consumed flag set, so the next real click had its
// press forwarded and its release eaten, leaving LEFT held on the target.
// ---------------------------------------------------------------------------------------------

#[test]
fn b1_a_button_up_while_engaging_does_not_swallow_the_next_real_click() {
    let mut t = Target::new();

    // The user clicks to capture. The writer has not acknowledged the previous release-all yet, so
    // this sits in `Engaging` (§2.6).
    t.feed(left(true));
    assert_eq!(t.session.capture(), CaptureState::Engaging);

    // The user lets go before the acknowledgement lands. That up belongs to the consumed press.
    t.feed(left(false));
    assert!(
        !t.session.left_consumed(),
        "the consumed press was never cleared, so the next real release will be eaten"
    );

    t.engage_succeeds();

    // One perfectly ordinary click on the target.
    t.feed(left(true));
    assert_eq!(t.buttons, button::LEFT, "the press must be forwarded");
    t.feed(left(false));
    assert_eq!(t.buttons, 0, "LEFT is still held on the target");
}

#[test]
fn b1_repeated_engaging_clicks_leave_no_stale_consumed_press() {
    let mut t = Target::new();
    for _ in 0..5 {
        t.feed(left(true));
        t.feed(left(false));
    }
    assert!(!t.session.left_consumed());
    t.engage_succeeds();
    t.feed(left(true));
    t.feed(left(false));
    assert_eq!(t.buttons, 0, "LEFT left held on the target");
}

/// The invariant itself, swept: after any sequence of left-button edges interleaved with the
/// triggers that move the state machine, the target is never left holding a button that no
/// subsequent release can clear.
#[test]
fn b1_a_forwarded_press_always_has_a_forwardable_release() {
    let interleave = [
        Trigger::Retry,
        Trigger::EngageSucceeded,
        Trigger::EngageDeferred,
        Trigger::FocusLost,
        Trigger::ReleaseKey,
        Trigger::InhibitorInactive,
    ];
    for a in interleave {
        for b in interleave {
            for downs in [
                [true, false, true, false],
                [true, true, false, false],
                [false, true, false, true],
                [true, false, false, true],
            ] {
                let mut t = Target::new();
                t.feed(downs[0].into_trigger());
                t.feed(a);
                t.feed(downs[1].into_trigger());
                t.feed(b);
                t.feed(downs[2].into_trigger());
                // Whatever state that left, a full press/release pair from here must not stick.
                t.feed(left(true));
                t.feed(left(false));
                assert_eq!(
                    t.buttons, 0,
                    "LEFT held after {a:?}/{b:?} over {downs:?}: session {:?}",
                    t.session
                );
            }
        }
    }
}

/// A tiny helper so the sweep above reads as a sequence of edges.
trait IntoLeft {
    fn into_trigger(self) -> Trigger;
}

impl IntoLeft for bool {
    fn into_trigger(self) -> Trigger {
        left(self)
    }
}

// ---------------------------------------------------------------------------------------------
// M1 — re-capture must not sit out a 250 ms tick. Any input arriving while `Engaging` is itself a
// retry opportunity, so the engagement completes on the next event rather than on the next tick.
// ---------------------------------------------------------------------------------------------

#[test]
fn m1_input_while_engaging_asks_for_an_engage_retry() {
    let engaging = reduce(Session::new(), left(true)).0;
    assert_eq!(engaging.capture(), CaptureState::Engaging);
    let inputs = [
        Trigger::Key {
            key: HidKey::Usage(0x04),
            down: true,
        },
        Trigger::Key {
            key: HidKey::Usage(0x04),
            down: false,
        },
        Trigger::Key {
            key: enter(),
            down: false,
        },
        Trigger::Button {
            mask: button::RIGHT,
            down: true,
        },
        Trigger::Motion(Event::PointerAbs { x: 1, y: 2 }),
        Trigger::Motion(Event::PointerRel { dx: 1, dy: 0 }),
        Trigger::Motion(Event::Wheel { delta: 1 }),
    ];
    for t in inputs {
        let (next, actions) = reduce(engaging, t);
        assert_eq!(next.capture(), CaptureState::Engaging, "{t:?}");
        assert!(
            actions.contains(&Action::TryEngage),
            "{t:?} while Engaging did not retry the engagement: {actions:?}"
        );
        assert!(
            !actions.iter().any(|a| matches!(a, Action::Forward(_))),
            "{t:?} forwarded before the acknowledgement: {actions:?}"
        );
    }
}

/// A left button-up that is *not* the consumed one retries like any other input. (Engaged from the
/// keyboard here, so the left button carries no consumed press.)
#[test]
fn m1_an_unconsumed_button_up_while_engaging_also_retries() {
    let engaging = reduce(
        Session::new(),
        Trigger::Key {
            key: enter(),
            down: true,
        },
    )
    .0;
    let (next, actions) = reduce(engaging, left(false));
    assert_eq!(next.capture(), CaptureState::Engaging);
    assert_eq!(actions, vec![Action::TryEngage]);
}

/// The `Enter` that captured is the one input that must **not** retry on its way past: its key-up
/// is consumed, and consuming it is the whole action.
#[test]
fn m1_the_consumed_enter_up_is_consumed_rather_than_retried() {
    let engaging = reduce(
        Session::new(),
        Trigger::Key {
            key: enter(),
            down: true,
        },
    )
    .0;
    let (next, actions) = reduce(
        engaging,
        Trigger::Key {
            key: enter(),
            down: false,
        },
    );
    assert!(actions.is_empty(), "{actions:?}");
    assert!(!next.enter_consumed());
}

// ---------------------------------------------------------------------------------------------
// N4 — the producer can disengage itself with no event reaching the event loop, so a captured
// session has to notice on the tick and release.
// ---------------------------------------------------------------------------------------------

#[test]
fn n4_a_self_disengaged_producer_owes_a_release() {
    // Engaged and healthy: nothing owed.
    assert_eq!(self_release_trigger(true, false), None);
    // The writer hit `LinkDown` and cancelled its own session.
    assert_eq!(
        self_release_trigger(false, false),
        Some(Trigger::SubmitFailed(SubmitError::Disengaged))
    );
    // Still engaged, but the transport is gone.
    assert_eq!(
        self_release_trigger(true, true),
        Some(Trigger::SubmitFailed(SubmitError::LinkDown))
    );
    // Both: the disengagement is the more specific fact and is reported first.
    assert_eq!(
        self_release_trigger(false, true),
        Some(Trigger::SubmitFailed(SubmitError::Disengaged))
    );
}

#[test]
fn n4_the_owed_release_runs_the_full_sequence() {
    for (engaged, link_down, reason) in [
        (false, false, ReleaseReason::CaptureReleased),
        (true, true, ReleaseReason::LinkDown),
    ] {
        let mut t = Target::new();
        t.feed(left(true));
        t.engage_succeeds();
        let trigger = self_release_trigger(engaged, link_down).expect("a release is owed");
        let actions = t.feed(trigger);
        assert_eq!(t.session.capture(), CaptureState::Released);
        assert!(actions.contains(&Action::ReleaseAll(reason)), "{actions:?}");
        assert!(actions.contains(&Action::DisarmInhibitor));
        assert!(actions.contains(&Action::ReleasePointer));
    }
}

// ---------------------------------------------------------------------------------------------
// N5 — `App::tick` feeds `CloseRequested` on every tick while an interrupt is pending. Only the
// first one may release: a release per tick bumps `requested_epoch` every 250 ms.
// ---------------------------------------------------------------------------------------------

#[test]
fn n5_a_repeated_close_releases_once() {
    for start in each_phase() {
        let (mut session, actions) = reduce(start, Trigger::CloseRequested);
        assert!(
            actions.contains(&Action::ReleaseAll(ReleaseReason::Shutdown)),
            "the first close must release: {actions:?}"
        );
        assert_eq!(actions.last(), Some(&Action::Exit));
        assert!(session.is_closing());

        for tick in 0..10 {
            let (next, actions) = reduce(session, Trigger::CloseRequested);
            session = next;
            assert_eq!(
                actions,
                vec![Action::Exit],
                "close #{} released again from {start:?}",
                tick + 2
            );
        }
    }
}

/// And nothing re-captures behind a close. The writer's shutdown release-all is the last thing
/// that may reach the target (§2.6), so a click arriving while the loop winds down must not arm
/// anything.
#[test]
fn n5_nothing_re_captures_after_a_close() {
    let (closed, _) = reduce(Session::new(), Trigger::CloseRequested);
    for t in every_trigger() {
        if matches!(t, Trigger::CloseRequested) {
            continue;
        }
        let (next, actions) = reduce(closed, t);
        assert_eq!(next.capture(), CaptureState::Released, "{t:?}");
        assert!(next.is_closing(), "{t:?}");
        assert!(
            actions.is_empty(),
            "{t:?} did something after a close: {actions:?}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// H6 — the client-side escape. §12 Stage 1 requires one that does not depend on the compositor's
// configuration, and there must be exactly one.
// ---------------------------------------------------------------------------------------------

#[test]
fn h6_only_keycode_pause_releases() {
    use winit::event::ElementState;
    use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
    assert_eq!(
        input_map::map_key(
            PhysicalKey::Code(KeyCode::Pause),
            ElementState::Pressed,
            false,
            ModifiersState::empty(),
        ),
        KeyAction::Release
    );
    // Everything else that is mapped is forwarded, including the keys a user might guess at —
    // **in every modifier state**, which is what keeps §12 Stage 4c's paste chord from having
    // quietly reserved a second key.
    for c in [KeyCode::ScrollLock, KeyCode::Escape, KeyCode::F12] {
        for modifiers in [
            ModifiersState::empty(),
            ModifiersState::SHIFT,
            ModifiersState::CONTROL | ModifiersState::ALT,
        ] {
            assert!(
                matches!(
                    input_map::map_key(
                        PhysicalKey::Code(c),
                        ElementState::Pressed,
                        false,
                        modifiers
                    ),
                    KeyAction::Forward { .. }
                ),
                "{c:?} under {modifiers:?} is not forwarded, so it is a second escape nobody \
                 documented"
            );
        }
    }
    // The one chord there is, on the one key already reserved (§12 Stage 4c).
    assert_eq!(
        input_map::map_key(
            PhysicalKey::Code(KeyCode::Pause),
            ElementState::Pressed,
            false,
            ModifiersState::SHIFT,
        ),
        KeyAction::Paste
    );
}

// ---------------------------------------------------------------------------------------------
// S2-notices — §2.8 says an overflow must be surfaced, "not a silent counter". Stage 1 released
// the session, logged a warning and put the title back to `[click or Enter to capture]`, which is
// what the title says when the *user* pressed Pause: the two failures a user cannot predict were
// indistinguishable from the one they asked for. The reason now lives in the session, so it is on
// screen until input is actually flowing again.
// ---------------------------------------------------------------------------------------------

#[test]
fn s2_a_refused_submission_leaves_a_notice_saying_why_input_stopped() {
    for (error, notice, fragment) in [
        (
            SubmitError::Overflow,
            Notice::Overflow,
            "input interrupted: queue overflowed — click to re-capture",
        ),
        (
            SubmitError::LinkDown,
            Notice::LinkDown,
            "input stopped: serial link down",
        ),
    ] {
        let mut t = Target::new();
        t.feed(left(true));
        t.engage_succeeds();
        assert_eq!(
            t.session.notice(),
            None,
            "a healthy session explains nothing"
        );

        t.feed(Trigger::SubmitFailed(error));
        assert_eq!(t.session.capture(), CaptureState::Released);
        assert_eq!(
            t.session.notice(),
            Some(notice),
            "{error:?} must leave a notice"
        );
        // The wording is part of the contract: it names the cause *and* the way out.
        assert_eq!(
            t.session.notice().expect("just asserted").title_fragment(),
            fragment
        );
    }
}

#[test]
fn s2_a_deliberate_release_explains_nothing_because_the_user_asked_for_it() {
    for trigger in [
        Trigger::ReleaseKey,
        Trigger::FocusLost,
        Trigger::InhibitorInactive,
        Trigger::SubmitFailed(SubmitError::Disengaged),
        Trigger::SubmitFailed(SubmitError::ShuttingDown),
    ] {
        let mut t = Target::new();
        t.feed(left(true));
        t.engage_succeeds();
        t.feed(trigger);
        assert_eq!(
            t.session.notice(),
            None,
            "{trigger:?} is not a failure the user needs telling about"
        );
    }
}

#[test]
fn s2_the_notice_survives_until_input_is_actually_flowing_again() {
    let mut t = Target::new();
    t.feed(left(true));
    t.engage_succeeds();
    t.feed(Trigger::SubmitFailed(SubmitError::Overflow));
    assert_eq!(t.session.notice(), Some(Notice::Overflow));

    // Every tick between the failure and the recapture keeps saying why.
    for _ in 0..10 {
        t.feed(Trigger::Retry);
        assert_eq!(t.session.notice(), Some(Notice::Overflow));
    }

    // The click that re-captures is not enough: the writer has not acknowledged yet, so nothing
    // is flowing and the explanation still stands.
    t.feed(left(true));
    assert_eq!(t.session.capture(), CaptureState::Engaging);
    assert_eq!(
        t.session.notice(),
        Some(Notice::Overflow),
        "engaging is not engaged"
    );

    t.feed(Trigger::EngageSucceeded);
    assert_eq!(t.session.capture(), CaptureState::Captured);
    assert_eq!(
        t.session.notice(),
        None,
        "a successful engagement is what clears it"
    );
}

#[test]
fn s2_a_second_failure_replaces_the_notice_rather_than_stacking_on_it() {
    let mut t = Target::new();
    t.feed(left(true));
    t.engage_succeeds();
    t.feed(Trigger::SubmitFailed(SubmitError::Overflow));
    t.feed(left(true));
    t.feed(Trigger::EngageSucceeded);
    t.feed(Trigger::SubmitFailed(SubmitError::LinkDown));
    assert_eq!(
        t.session.notice(),
        Some(Notice::LinkDown),
        "the title must describe the failure that just happened"
    );
}

// ---------------------------------------------------------------------------------------------
// S2-release-notice — §2.6.1: an `Unsent` release means the target may still be holding keys.
// Across a §2.7 reconnect the writer sends a release-all on the replacement link, and *that* is
// what makes the warning obsolete. Nothing else may clear it.
// ---------------------------------------------------------------------------------------------

fn record(epoch: u64, reason: ReleaseReason, outcome: ReleaseOutcome) -> ReleaseRecord {
    ReleaseRecord {
        epoch,
        reason,
        outcome,
    }
}

#[test]
fn s2_an_unsent_release_is_reported_once_and_stays_on_screen() {
    let mut notice = ReleaseNotice::default();
    assert_eq!(notice.observe(None), None, "nothing has been released yet");
    assert!(!notice.raised());

    let unsent = record(4, ReleaseReason::LinkDown, ReleaseOutcome::Unsent);
    assert_eq!(notice.observe(Some(unsent)), Some(unsent), "logged once");
    assert!(notice.raised());

    // The tick runs four times a second; the warning must not be logged four times a second.
    for _ in 0..10 {
        assert_eq!(notice.observe(Some(unsent)), None, "already reported");
        assert!(notice.raised(), "and still true");
    }
}

#[test]
fn s2_the_reconnects_release_clears_an_unsent_notice() {
    let mut notice = ReleaseNotice::default();
    notice.observe(Some(record(
        4,
        ReleaseReason::LinkDown,
        ReleaseOutcome::Unsent,
    )));
    assert!(notice.raised());

    // §2.7 step 3: the writer commissions the replacement link and releases everything on it.
    // The keys really are up now, so the warning must go — this is the whole reason the notice
    // is driven from `last_release` rather than latched when the link went down.
    let reconnected = record(5, ReleaseReason::Reconnected, ReleaseOutcome::Submitted);
    assert_eq!(notice.observe(Some(reconnected)), Some(reconnected));
    assert!(
        !notice.raised(),
        "a release that reached the replacement link retires the UNSENT warning"
    );
}

#[test]
fn s2_a_later_unsent_release_raises_the_notice_again() {
    let mut notice = ReleaseNotice::default();
    notice.observe(Some(record(
        1,
        ReleaseReason::FocusLost,
        ReleaseOutcome::Submitted,
    )));
    assert!(!notice.raised());
    notice.observe(Some(record(
        2,
        ReleaseReason::Shutdown,
        ReleaseOutcome::Unsent,
    )));
    assert!(notice.raised(), "each release is judged on its own outcome");
}
