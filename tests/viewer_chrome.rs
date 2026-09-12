//! The chrome's pure rules, as properties (plan §12 Stage 4b).
//!
//! The clause-by-clause assertions live beside the code in `src/viewer/chrome/`. What is here is
//! the property that has to hold for **every** state there is, not for the handful a test can
//! name: the viewer's release binding is never trapped by the chrome.
//!
//! Nothing here opens a device, a window or a config file.

use proptest::prelude::*;

use std::time::{Duration, Instant};

use nanokvm::proto::{button, modifier, HidKey};
use nanokvm::script::Layout;
use nanokvm::viewer::chrome::paste::{self, PasteJob};
use nanokvm::viewer::chrome::{
    resolve_paste_chord, route_key, route_pointer, Outstanding, PasteOutcome, PointerKind,
    RouteInputs, Sink,
};
use nanokvm::viewer::input_map::KeyAction;
use nanokvm::viewer::state::CaptureState;
use nanokvm::viewer::PointerMode;

/// Any routing state at all, including any set of presses the target is holding.
fn any_inputs() -> impl Strategy<Value = RouteInputs> {
    (
        prop_oneof![
            Just(CaptureState::Released),
            Just(CaptureState::Engaging),
            Just(CaptureState::Captured),
        ],
        prop_oneof![Just(PointerMode::Absolute), Just(PointerMode::Relative)],
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any_outstanding(),
        any_outstanding(),
    )
        .prop_map(
            |(
                capture,
                pointer,
                popover_open,
                modal_open,
                pointer_over_chrome,
                paste_running,
                pointer_locked,
                outstanding,
                paste_held,
            )| {
                RouteInputs {
                    capture,
                    pointer,
                    popover_open,
                    modal_open,
                    pointer_over_chrome,
                    paste_running,
                    pointer_locked,
                    outstanding,
                    paste_held,
                }
            },
        )
}

/// Any set of keys and buttons the target might be holding.
fn any_outstanding() -> impl Strategy<Value = Outstanding> {
    (
        prop::collection::vec(any_key(), 0..8),
        prop::collection::vec(any_button(), 0..4),
    )
        .prop_map(|(keys, buttons)| {
            let mut o = Outstanding::default();
            for key in keys {
                o.set_key(key, true);
            }
            for mask in buttons {
                o.set_button(mask, true);
            }
            o
        })
}

/// Any key this client can send: a modifier bit, or a usage.
fn any_key() -> impl Strategy<Value = HidKey> {
    prop_oneof![
        prop_oneof![
            Just(modifier::LEFT_CTRL),
            Just(modifier::LEFT_SHIFT),
            Just(modifier::LEFT_ALT),
            Just(modifier::LEFT_META),
            Just(modifier::RIGHT_CTRL),
        ]
        .prop_map(HidKey::Modifier),
        any::<u8>().prop_map(|u| HidKey::Usage(u.max(1))),
    ]
}

/// Any button `input_map` maps.
fn any_button() -> impl Strategy<Value = u8> {
    prop_oneof![
        Just(button::LEFT),
        Just(button::RIGHT),
        Just(button::MIDDLE),
        Just(button::BACK),
        Just(button::FORWARD),
    ]
}

/// One of each pointer class, with a plain LEFT press standing for the button.
fn kinds() -> [PointerKind; 3] {
    [
        PointerKind::Motion,
        PointerKind::Button {
            mask: button::LEFT,
            down: true,
        },
        PointerKind::Wheel,
    ]
}

/// Any key action `input_map` can produce.
fn any_action() -> impl Strategy<Value = KeyAction> {
    prop_oneof![
        Just(KeyAction::Release),
        Just(KeyAction::Paste),
        Just(KeyAction::Swallowed),
        Just(KeyAction::Unmapped),
        (any::<u8>(), any::<bool>()).prop_map(|(usage, down)| KeyAction::Forward {
            key: nanokvm::proto::HidKey::Usage(usage.max(1)),
            down,
        }),
    ]
}

proptest! {
    /// **The property 4b exists to keep true.** `KeyAction::Release` — the `Pause` binding — routes
    /// to the target's release path from every state there is, and nothing else ever routes there
    /// by being `Release`. A chrome that could swallow it would trap the user's keyboard behind a
    /// menu, and §12 Stage 1 requires the way out to be reachable.
    #[test]
    fn the_release_key_is_never_routed_anywhere_but_the_release_path(i in any_inputs()) {
        prop_assert_eq!(route_key(&i, KeyAction::Release), Sink::Target);
    }

    /// The complement: nothing that is *not* the release key is ever mistaken for it. Every other
    /// action lands in one of the three sinks by its own clause, and the function is total — there
    /// is no state in which a key event simply falls through unrouted.
    #[test]
    fn every_key_action_is_routed_somewhere(i in any_inputs(), action in any_action()) {
        let sink = route_key(&i, action);
        prop_assert!(matches!(sink, Sink::Target | Sink::Chrome | Sink::Dropped));
        // Swallowed and unmapped keys have already been decided against by `input_map`.
        if matches!(action, KeyAction::Swallowed | KeyAction::Unmapped) {
            prop_assert_eq!(sink, Sink::Dropped);
        }
    }

    /// **§12 Stage 4c's half of the same property.** The paste chord is the other viewer-local
    /// binding on the one reserved key, and it reaches the event loop from every state — a chord
    /// a popover could swallow is a chord the user cannot reach.
    #[test]
    fn the_paste_chord_is_never_routed_anywhere_but_the_event_loop(i in any_inputs()) {
        prop_assert_eq!(route_key(&i, KeyAction::Paste), Sink::Target);
    }

    /// And while a paste runs the chord **is** the release, from every state: it cancels, and it
    /// can never restart a paste that is half-typed.
    #[test]
    fn the_chord_during_a_paste_is_always_the_release(i in any_inputs()) {
        let running = RouteInputs { paste_running: true, ..i };
        let resolved = resolve_paste_chord(KeyAction::Paste, running.paste_running);
        prop_assert_eq!(resolved, KeyAction::Release);
        prop_assert_eq!(route_key(&running, resolved), Sink::Target);
    }

    /// While a paste runs, an ordinary host key reaches neither the target nor the chrome —
    /// unless it is the up of a press the **host** made and the target still holds, which is owed
    /// from every state. A keystroke forwarded into the middle of a paste corrupts the text
    /// (§12 Stage 4c).
    #[test]
    fn a_running_paste_drops_every_host_key_it_does_not_owe(
        i in any_inputs(),
        key in any_key(),
        down in any::<bool>(),
    ) {
        let mut outstanding = i.outstanding;
        outstanding.set_key(key, false);
        let running = RouteInputs { paste_running: true, outstanding, ..i };
        let sink = route_key(&running, KeyAction::Forward { key, down });
        prop_assert_eq!(sink, Sink::Dropped, "{:?}", running);
    }

    /// The other half: while a paste runs, the up of a key the **paste itself** is holding is
    /// dropped even though the target is holding it. The paste releases its own keys; forwarding
    /// the user's Shift-up into the middle of a capital is the corruption this prevents.
    #[test]
    fn a_running_paste_keeps_its_own_keys_from_the_host(i in any_inputs(), key in any_key()) {
        let mut held = i.outstanding;
        held.set_key(key, true);
        let mut paste_held = i.paste_held;
        paste_held.set_key(key, true);
        let running = RouteInputs {
            paste_running: true,
            outstanding: held,
            paste_held,
            ..i
        };
        let up = KeyAction::Forward { key, down: false };
        prop_assert_eq!(route_key(&running, up), Sink::Dropped, "{:?}", running);
    }

    /// Under a relative-mode pointer lock the chrome is unreachable, so nothing — key or pointer —
    /// may be routed to it, whatever the stale hit test or a popover left open from before the
    /// lock says. Only the release key gets out, which the first property covers.
    #[test]
    fn relative_capture_never_routes_anything_to_the_chrome(i in any_inputs(), action in any_action()) {
        let locked = RouteInputs {
            capture: CaptureState::Captured,
            pointer: PointerMode::Relative,
            ..i
        };
        let locked = RouteInputs { pointer_locked: true, ..locked };
        prop_assert_ne!(route_key(&locked, action), Sink::Chrome);
        for kind in kinds() {
            // A button press during a paste is dropped from every state, the lock included — see
            // `a_running_paste_drops_every_button_press`. Everything else the chrome cannot have
            // goes to the target.
            if locked.paste_running && matches!(kind, PointerKind::Button { down: true, .. }) {
                prop_assert_eq!(route_pointer(&locked, kind), Sink::Dropped);
            } else {
                prop_assert_eq!(route_pointer(&locked, kind), Sink::Target);
            }
        }
    }

    /// A pointer event is never dropped, **except a button press during a paste that is not on
    /// the chrome**. The chrome may take it and the target may take it, but a button press that
    /// reached neither would be a press with no release — the defect `viewer::state`'s
    /// consumed-edge bookkeeping exists to prevent. A press dropped mid-paste is not that defect:
    /// it never becomes outstanding, so there is no release owed for it (§12 Stage 4c).
    #[test]
    fn a_pointer_event_always_reaches_one_side(i in any_inputs()) {
        let on_chrome = !i.chrome_unreachable()
            && (i.pointer_over_chrome || i.popover_open || i.modal_open);
        for kind in kinds() {
            if i.paste_running
                && !on_chrome
                && matches!(kind, PointerKind::Button { down: true, .. })
            {
                continue;
            }
            prop_assert_ne!(route_pointer(&i, kind), Sink::Dropped);
        }
    }

    /// **A click must not land on the target in the middle of a paste.** A keystroke corrupts the
    /// text; a click moves the target's focus out from under it, and the rest of the paste is
    /// then typed into whatever the click opened — on a live desktop. So no button press reaches
    /// the target from any state, however the pill, the popover and the lock are arranged.
    #[test]
    fn a_running_paste_keeps_every_button_press_from_the_target(
        i in any_inputs(),
        mask in any_button(),
    ) {
        let running = RouteInputs { paste_running: true, ..i };
        let sink = route_pointer(&running, PointerKind::Button { mask, down: true });
        // The invariant is about the **target**, not about `Dropped`: on the chrome the press is
        // the chrome's, which never reaches the target either (the lead's ruling on 4c item 7),
        // and the pill must stay clickable while a paste that can take minutes runs.
        prop_assert_ne!(sink, Sink::Target, "{:?}", running);
        let on_chrome = !running.chrome_unreachable()
            && (running.pointer_over_chrome || running.popover_open || running.modal_open);
        prop_assert_eq!(
            sink,
            if on_chrome { Sink::Chrome } else { Sink::Dropped },
            "{:?}",
            running
        );
    }

    /// And the other half: a button the target is **already** holding still gets its up, paste or
    /// no paste. The press was forwarded before the paste began, so the release is owed from every
    /// state — the same clause that survives a popover.
    #[test]
    fn a_running_paste_still_owes_the_up_of_an_outstanding_button(
        i in any_inputs(),
        mask in any_button(),
    ) {
        let mut outstanding = i.outstanding;
        outstanding.set_button(mask, true);
        let running = RouteInputs { paste_running: true, outstanding, ..i };
        prop_assert_eq!(
            route_pointer(&running, PointerKind::Button { mask, down: false }),
            Sink::Target
        );
    }

    /// **The property review item 1 adds, and the other half of what 4b exists to keep true.**
    /// `viewer::state`'s invariant is that a press which was forwarded always has a release that is
    /// forwardable; the chrome can break it from outside the reducer, because a flag can flip
    /// between the two edges. So for **any** state at all, the up edge of a key the target is
    /// holding goes to the target — never to the chrome, never nowhere.
    #[test]
    fn the_up_of_an_outstanding_key_always_reaches_the_target(
        i in any_inputs(),
        key in any_key(),
    ) {
        let mut outstanding = i.outstanding;
        outstanding.set_key(key, true);
        // The host's press, not the paste's: a key a running paste is holding is its own to
        // release, and forwarding a host key-up for it would end a character half way through
        // (§12 Stage 4c, `RouteInputs::paste_held`).
        let mut paste_held = i.paste_held;
        paste_held.set_key(key, false);
        let i = RouteInputs { outstanding, paste_held, ..i };
        let up = KeyAction::Forward { key, down: false };
        prop_assert_eq!(route_key(&i, up), Sink::Target);
    }

    /// The same for a button: a press over the video whose drag crossed the pill still gets its
    /// release, or LEFT stays down on a live desktop.
    #[test]
    fn the_up_of_an_outstanding_button_always_reaches_the_target(
        i in any_inputs(),
        mask in any_button(),
    ) {
        let mut outstanding = i.outstanding;
        outstanding.set_button(mask, true);
        let i = RouteInputs { outstanding, ..i };
        prop_assert_eq!(
            route_pointer(&i, PointerKind::Button { mask, down: false }),
            Sink::Target
        );
    }
}

// ---------------------------------------------------------------------------------------------
// §12 Stage 4c — the paste job, as properties. The clause-by-clause assertions are beside the code
// in `src/viewer/chrome/paste.rs`; what is here holds for every text and every cancel point.
// ---------------------------------------------------------------------------------------------

/// Any text made of characters US QWERTY can reach, so every compile succeeds and the properties
/// are about the *job* rather than about the mapper.
fn any_typeable() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![prop::char::range(' ', '~'), Just('\n'), Just('\t'),],
        1..40,
    )
    .prop_map(|v| v.into_iter().collect())
}

/// Any job: any typeable text, with anything at all already held by the target.
fn any_job() -> impl Strategy<Value = PasteJob> {
    (any_typeable(), any_outstanding()).prop_map(|(text, outstanding)| {
        let body = paste::compile(&text, Layout::Us, Some(false)).expect("typeable");
        PasteJob::new(outstanding, body, paste::PACE)
    })
}

proptest! {
    /// Progress only ever goes forward, never past the total, and the two durations agree with it
    /// at every step. `total` is the step count and nothing else.
    #[test]
    fn progress_is_monotone_and_the_durations_follow_it(mut job in any_job()) {
        let total = job.total();
        prop_assert_eq!(job.expected_duration(), paste::PACE * total as u32);
        let mut now = Instant::now();
        let mut last = 0usize;
        while job.next_step(now).is_some() {
            job.delivered(now);
            prop_assert!(job.sent() > last, "progress went backwards or stalled");
            prop_assert!(job.sent() <= total);
            prop_assert_eq!(job.remaining(), total - job.sent());
            prop_assert_eq!(job.remaining_duration(), paste::PACE * job.remaining() as u32);
            prop_assert_eq!(job.progress().total, total);
            last = job.sent();
            now += paste::PACE;
        }
        prop_assert_eq!(job.sent(), total);
        prop_assert_eq!(job.outcome(), Some(&PasteOutcome::Done { total }));
        prop_assert_eq!(job.remaining_duration(), Duration::ZERO);
    }

    /// **A paste always ends with nothing held.** Run any job to completion and the target holds
    /// nothing — the same assertion the two built-in shortcuts make, for the same reason: a paste
    /// that left Shift down would leave the target unusable and nothing local could fix it.
    #[test]
    fn a_completed_job_leaves_nothing_held(mut job in any_job()) {
        let mut now = Instant::now();
        while job.next_step(now).is_some() {
            job.delivered(now);
            now += paste::PACE;
        }
        prop_assert!(job.held().is_empty(), "{:?} left held", job.held());
    }

    /// **Cancel from any state leaves nothing held** — once the release-all the cancellation
    /// reports as owed has run. The outcome names the progress it had, and the job offers no
    /// further steps.
    #[test]
    fn cancelling_at_any_point_reports_its_progress_and_owes_exactly_what_it_holds(
        mut job in any_job(),
        cut in 0usize..200,
    ) {
        let total = job.total();
        let cut = cut.min(total);
        let mut now = Instant::now();
        for _ in 0..cut {
            job.delivered(now);
            now += paste::PACE;
        }
        let held_before = job.held();
        let cancellation = job.cancel();
        prop_assert_eq!(cancellation.still_held, held_before);
        if cut == total {
            prop_assert_eq!(cancellation.outcome, PasteOutcome::Done { total });
        } else {
            prop_assert_eq!(cancellation.outcome, PasteOutcome::Cancelled { sent: cut, total });
        }
        prop_assert!(!job.is_running());
        prop_assert!(job.next_step(now).is_none());
        // §2.6's release-all, which is what the viewer runs alongside the cancel.
        let mut owed = cancellation.still_held;
        owed.clear();
        prop_assert!(owed.is_empty());
    }

    /// No step is ever offered before its pace has elapsed, and the first one is offered at once.
    #[test]
    fn the_pace_is_respected_between_every_pair_of_steps(mut job in any_job()) {
        let t0 = Instant::now();
        prop_assert!(job.next_step(t0).is_some());
        let mut now = t0;
        while job.next_step(now).is_some() {
            job.delivered(now);
            if job.is_running() {
                prop_assert!(job.next_step(now).is_none(), "a step went out early");
                prop_assert_eq!(job.due_at(), Some(now + paste::PACE));
            }
            now += paste::PACE;
        }
    }
}
