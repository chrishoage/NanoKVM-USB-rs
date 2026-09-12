//! Where an input event goes: the target, the chrome, or nowhere (plan §12 Stage 4b).
//!
//! This is a pure rule, shaped like [`crate::viewer::state`], for the same reason: the sentence
//! "the chrome never steals a key the target was going to get" is a correctness claim, and a
//! correctness claim belongs in a function with no window, no egui context and no clock in scope
//! so that every clause of it can be asserted without a display.
//!
//! # Why this does not read `egui_winit::EventResponse::consumed`
//!
//! Because `consumed` is wrong for a KVM, measurably and in three separate ways:
//!
//! 1. **Tab is consumed unconditionally.** `egui_winit::State::on_window_event` reports
//!    `consumed: true` for the Tab key whether or not egui has focus, or even a widget
//!    (egui-winit 0.33.3 `src/lib.rs:415-418`: *"When pressing the Tab key, egui focuses the first
//!    focusable element, hence Tab always consumes."*). Routing on it would swallow Tab — on a
//!    KVM, to a shell.
//! 2. **It lags by a frame.** `consumed` for keys is `egui_ctx.wants_keyboard_input()`, which
//!    reflects the UI built *last* frame. The routing decision for the key in hand cannot be made
//!    from a predicate about the previous one.
//! 3. **Pointer motion is not covered at all.** Press and release use `wants_pointer_input()` but
//!    `CursorMoved` uses `is_using_pointer()` (lib.rs:326), so merely moving the cursor over the
//!    pill reports `consumed: false` and the motion would still be forwarded to the target.
//!
//! So the gate is at the source and it is ours: [`route_key`] and [`route_pointer`] decide, and
//! only a [`Sink::Chrome`] verdict hands the raw `WindowEvent` to `egui_winit`. `input_map` is
//! unchanged — it still discards host repeat and still intercepts the release key — and this runs
//! after it.
//!
//! # The two things that always get out
//!
//! [`KeyAction::Release`] — and [`KeyAction::Paste`], the other viewer-local binding on the same
//! physical key — routes to [`Sink::Target`] from **every** state: modal open, popover
//! open, pointer locked in relative capture, paste running. §12 Stage 1 requires the way out to
//! be reachable and §12 Stage 4c requires the never-forwarded surface to stay exactly one key.
//! A chrome that could trap the release binding would trap the user's keyboard.
//!
//! And so does **the up edge of a press the target already owns** — see [`Outstanding`]. Without
//! that clause the chrome breaks [`crate::viewer::state`]'s invariant from outside the reducer:
//! hold Ctrl, click the pill so a popover opens, let go of Ctrl, and the key-up has gone to egui
//! while Ctrl stays down on the target with no event left that could lift it.

use crate::proto::HidKey;
use crate::viewer::app::PointerMode;
use crate::viewer::input_map::KeyAction;
use crate::viewer::state::CaptureState;

/// Where one event is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sink {
    /// Forward to the target, through the capture state machine as before. For
    /// [`KeyAction::Release`] this is the release path rather than a forwarded keystroke — the
    /// reducer, not this module, knows the difference.
    Target,
    /// Hand the raw `WindowEvent` to `egui_winit` and nothing else.
    Chrome,
    /// Deliver nowhere. A host key arriving mid-paste corrupts the pasted text and a click moves
    /// the focus out from under it (§12 Stage 4c), and a key `input_map` already swallowed has no
    /// destination by definition.
    Dropped,
}

/// The class of pointer event being routed. The three winit gives us, named as themselves so the
/// rule reads as the plan's sentence rather than as a winit enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerKind {
    /// `CursorMoved`, or `DeviceEvent::MouseMotion` in relative mode.
    Motion,
    /// `MouseInput`, with the button's mask (one bit of [`crate::proto::button`]) and its edge.
    ///
    /// The identity and the edge are carried because the rule has to recognise the up of a press
    /// the target owns (see [`Outstanding`]); a bare "a button happened" cannot. `mask` is 0 for a
    /// button this client does not map, which is never outstanding and never forwarded.
    Button { mask: u8, down: bool },
    /// `MouseWheel`.
    Wheel,
}

/// The presses the **target** owns: what the viewer has forwarded and not yet sent an up for.
///
/// [`crate::viewer::state`]'s invariant is *"a press that was forwarded always has a release that
/// is forwardable"*, and the chrome can break it from outside the reducer — a flag flipping
/// between a press and its release is enough. So the up edge of anything in this set routes to
/// [`Sink::Target`] from every state, exactly like the release key.
///
/// Bit sets rather than a `Vec`, so [`RouteInputs`] stays `Copy` and the rule allocates nothing on
/// the event loop. A key-up for something not in the set is routed by the ordinary clauses: the
/// target never had it, so nothing is owed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Outstanding {
    /// Modifier bits currently down on the target ([`crate::proto::modifier`]).
    modifiers: u8,
    /// One bit per HID usage, `0..=255`.
    usages: [u64; 4],
    /// Mouse buttons currently down on the target ([`crate::proto::button`]).
    buttons: u8,
}

impl Outstanding {
    /// Record a key transition that was forwarded to the target.
    pub fn set_key(&mut self, key: HidKey, down: bool) {
        match key {
            HidKey::Modifier(bit) => {
                if down {
                    self.modifiers |= bit;
                } else {
                    self.modifiers &= !bit;
                }
            }
            HidKey::Usage(usage) => {
                let (word, bit) = (usize::from(usage) / 64, 1u64 << (usage % 64));
                if down {
                    self.usages[word] |= bit;
                } else {
                    self.usages[word] &= !bit;
                }
            }
        }
    }

    /// Whether the target is holding this key.
    pub fn holds_key(&self, key: HidKey) -> bool {
        match key {
            HidKey::Modifier(bit) => self.modifiers & bit != 0,
            HidKey::Usage(usage) => {
                self.usages[usize::from(usage) / 64] & (1u64 << (usage % 64)) != 0
            }
        }
    }

    /// Record a button transition that was forwarded to the target.
    pub fn set_button(&mut self, mask: u8, down: bool) {
        if down {
            self.buttons |= mask;
        } else {
            self.buttons &= !mask;
        }
    }

    /// Whether the target is holding any button in `mask`. A zero mask — an unmapped button — is
    /// never held.
    pub fn holds_button(&self, mask: u8) -> bool {
        mask != 0 && self.buttons & mask != 0
    }

    /// Every key the target is holding: usages in ascending order, then the modifiers.
    ///
    /// That order is the one a release must go out in for the target never to see a usage without
    /// the modifier it was pressed under — the same rule [`super::shortcut::transitions`] applies
    /// within a step, read backwards, which is why the modifiers come *last*. It is what
    /// [`super::paste::PasteJob`] builds its opening release prelude from (§12 Stage 4c).
    pub fn held_keys(&self) -> Vec<HidKey> {
        let mut out = Vec::new();
        for usage in 0u16..=255 {
            let usage = usage as u8;
            if self.holds_key(HidKey::Usage(usage)) {
                out.push(HidKey::Usage(usage));
            }
        }
        for bit in 0..8 {
            let bit = 1u8 << bit;
            if self.modifiers & bit != 0 {
                out.push(HidKey::Modifier(bit));
            }
        }
        out
    }

    /// Every button the target is holding, in [`crate::proto::button`]'s bit order.
    ///
    /// The other half of [`super::paste::PasteJob`]'s release prelude: a paste runs for minutes,
    /// and a button the viewer forwarded a press for and never released is a drag on a live
    /// desktop for every one of them (§12 Stage 4c).
    pub fn held_buttons(&self) -> Vec<u8> {
        (0..8)
            .map(|bit| 1u8 << bit)
            .filter(|&mask| self.buttons & mask != 0)
            .collect()
    }

    /// Forget everything: what a release-all does to the target's state (§2.6).
    pub fn clear(&mut self) {
        *self = Outstanding::default();
    }

    /// Whether the target is holding nothing at all.
    pub fn is_empty(&self) -> bool {
        *self == Outstanding::default()
    }
}

/// Everything the rule reads. Plain data, all of it owned by the event loop thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RouteInputs {
    /// The capture phase, from [`crate::viewer::state::Session`].
    pub capture: CaptureState,
    /// Absolute or relative, from [`crate::viewer::ViewerConfig`] as the chrome may have changed
    /// it.
    pub pointer: PointerMode,
    /// **Our** popover flag, not egui's. A popover the chrome opened is a fact the event loop
    /// knows synchronously; `Context::wants_keyboard_input()` is last frame's opinion of it.
    pub popover_open: bool,
    /// A modal dialog is up. Reserved: 4b ships no modal (§12 Stage 4b puts the settings modal
    /// out of scope), but the clause is in the plan and is cheaper to carry than to retrofit.
    pub modal_open: bool,
    /// The pointer is inside the pill or an open popover, by [`super::hit::HitAreas`] — our own
    /// rectangle in physical pixels, not `Context::is_pointer_over_area()`.
    pub pointer_over_chrome: bool,
    /// A clipboard paste is being typed into the target (§12 Stage 4c).
    pub paste_running: bool,
    /// The pointer grab is actually in force — `App::grab` is `Some`.
    ///
    /// Not implied by relative mode: [`crate::viewer::app`]'s `grab_pointer` tries `Locked` and
    /// then `Confined` and logs a warning when a compositor refuses both, and on such a
    /// compositor the pointer is still free to walk onto the pill. Assuming the lock succeeded
    /// would make the pill dead *and* forward the click that hit it to the target as a real
    /// button press — a click on a live desktop, which `CLAUDE.md` forbids outright.
    pub pointer_locked: bool,
    /// What the target is holding because this viewer told it to. See [`Outstanding`].
    pub outstanding: Outstanding,
    /// The subset of [`RouteInputs::outstanding`] that a **running paste** put down, rather than
    /// the user (§12 Stage 4c).
    ///
    /// Clause 2 exists for keys the *host* is holding: a press forwarded from this keyboard must
    /// have its release forwarded, whatever the flags did in between. A paste's own presses are
    /// not that. Without this distinction, a user who was holding Shift when the paste started and
    /// lets go of it while the paste happens to be holding Shift for a capital would have that
    /// key-up forwarded into the middle of the text — ending the capital half way through, which
    /// is precisely the corruption clause 3 exists to prevent.
    ///
    /// Nothing is stranded by excluding them. A paste begins by releasing everything the target
    /// held (`super::paste`), so after its prelude the only presses outstanding are its own; and
    /// every host key that arrives *during* a paste is dropped, so none can join them.
    pub paste_held: Outstanding,
}

impl RouteInputs {
    /// Whether the pointer is locked to the window and the chrome is therefore unreachable.
    ///
    /// Under a locked pointer winit delivers no `CursorMoved` at all (q6b, and
    /// `app.rs`'s `CursorMoved` arm says so), so there is no way to move onto the pill and no way
    /// to click it. Saying that here, once, is better than every clause re-deriving it — and it
    /// requires the grab to be **in force**, not merely asked for.
    ///
    /// Public because it is half of the routing rule rather than an implementation detail: the
    /// proptests in `tests/viewer_chrome.rs` state their expectations in terms of it, and a test
    /// that re-derived "is the chrome reachable" from the three fields could drift from the
    /// function it is checking.
    pub fn chrome_unreachable(&self) -> bool {
        self.capture == CaptureState::Captured
            && self.pointer == PointerMode::Relative
            && self.pointer_locked
    }
}

/// Where a key event goes.
///
/// The clauses, in the order they are applied and with the one place this deviates from the
/// research design noted:
///
/// 1. [`KeyAction::Release`] → [`Sink::Target`], from every state. See the module docs.
/// 2. The **up edge of a key the target is holding because the host pressed it** →
///    [`Sink::Target`], from every state. The press was forwarded, so the release must be
///    forwardable, whatever happened to the flags in between ([`Outstanding`]). A key a *running
///    paste* is holding is excluded — see [`RouteInputs::paste_held`].
/// 3. A paste is running → [`Sink::Dropped`]. **The research design put this clause after the
///    relative-capture clause; it is before it here.** The two overlap in practice — a paste can
///    only run while captured, and capture can be in relative mode — and with the design's order a
///    host keystroke during a relative-mode paste would be forwarded to the target and corrupt the
///    text, which is exactly what §12 Stage 4c forbids. Nothing else changes: the release key is
///    already out at clause 1 and still cancels the paste.
/// 4. The pointer is locked in relative capture → [`Sink::Target`]: the chrome cannot be reached,
///    so it cannot be owed anything.
/// 5. A popover or a modal is open → [`Sink::Chrome`]. This is the reference's
///    `isKeyboardEnable = false`, and it is the only state in which the chrome takes keys at all.
/// 6. Otherwise → [`Sink::Target`]. **Including Tab**, which is the whole reason this function
///    exists.
///
/// [`KeyAction::Swallowed`] and [`KeyAction::Unmapped`] are [`Sink::Dropped`] in every state:
/// `input_map` has already decided they go nowhere, and this function is total so that a caller
/// cannot forget one.
pub fn route_key(i: &RouteInputs, action: KeyAction) -> Sink {
    match action {
        // 1. The way out, from everywhere.
        KeyAction::Release => Sink::Target,
        // 1b. The paste chord, which is viewer-local exactly like the release key and is never
        // forwarded. [`resolve_paste_chord`] has already turned it into `Release` if a paste is
        // running, so reaching here means none is; the arm is unconditional anyway, because a
        // trigger the chrome could swallow is a trigger the user cannot reach.
        KeyAction::Paste => Sink::Target,
        KeyAction::Swallowed | KeyAction::Unmapped => Sink::Dropped,
        KeyAction::Forward { key, down } => {
            // 2. The target is holding this key because we forwarded *the host's* press. Its up
            // is owed to the target from every state, or the key stays down there forever. A key
            // the running paste is holding is not the host's — see `paste_held`.
            if !down && i.outstanding.holds_key(key) && !i.paste_held.holds_key(key) {
                return Sink::Target;
            }
            if i.paste_running {
                return Sink::Dropped; // 3
            }
            if i.chrome_unreachable() {
                return Sink::Target; // 4
            }
            if i.popover_open || i.modal_open {
                return Sink::Chrome; // 5
            }
            Sink::Target // 6
        }
    }
}

/// What the paste chord means right now: start one, or cancel the one that is running.
///
/// §12 Stage 4c reserves **one** never-forwarded surface — the release key — and hangs the paste
/// trigger off it as `Shift+Pause`. That leaves one question: what does `Shift+Pause` mean while a
/// paste is already running? **Release wins.** A second trigger must not restart a paste that is
/// half-typed, and the user pressing the chord again mid-paste is far more likely to want out of
/// it than to want it again; §2.6's release-all is also the only thing that makes "cancel" mean
/// anything, since it is what actually lifts the keys.
///
/// It is a function of its own, applied before [`route_key`], because it is a *meaning* and not a
/// destination: turned into [`KeyAction::Release`] here, everything downstream — the routing, the
/// reducer's release sequence, the §2.6 cancellation — is the path that already exists and is
/// already tested, rather than a second one that would have to be kept equivalent.
pub fn resolve_paste_chord(action: KeyAction, paste_running: bool) -> KeyAction {
    match action {
        KeyAction::Paste if paste_running => KeyAction::Release,
        other => other,
    }
}

/// Where a pointer event goes.
///
/// 1. The **up edge of a button the target is holding** → [`Sink::Target`], from every state. A
///    press forwarded over the video whose drag then crossed the pill would otherwise have its
///    release eaten by egui and leave the button down on a live desktop ([`Outstanding`]).
/// 2. The pointer is over the pill or a popover, or a popover or modal is open → [`Sink::Chrome`]
///    — **including while a paste runs**. A popover open captures the pointer even off the
///    rectangle so that a click outside it dismisses it rather than landing on the target: a click
///    on a live desktop can launch or destroy something (`CLAUDE.md`), and "the click that closed
///    the menu" must never be one. Dismissal itself is the chrome's job, not this function's:
///    `ChromeUi::build` closes the popover on a click outside its rectangle. The clause does not
///    apply while the pointer is locked in relative capture, where the chrome is unreachable
///    ([`RouteInputs::chrome_unreachable`]).
/// 3. A paste is running and this is a button **press** → [`Sink::Dropped`], the same clause
///    [`route_key`] applies to host keys and for the same reason: a click landing in the middle of
///    a paste puts the target's focus somewhere else, and the rest of the text is then typed into
///    whatever the click opened. Only the press is dropped — clause 1 still owes the up of any
///    button the target is already holding, and a press that was dropped never becomes
///    outstanding, so this can never strand one.
/// 4. Otherwise → [`Sink::Target`], which includes the relative-capture lock: the chrome is
///    unreachable there, so `pointer_over_chrome` is *forced* false by `chrome_unreachable`
///    rather than trusted — the last hit test before the lock took effect may have said otherwise.
///
/// # Why clause 2 is above clause 3, which is a change from how 4c first wrote it
///
/// 4c dropped every button press while a paste ran, from every state. That is right for the
/// target and wrong for the chrome: it also disabled the pill and every popover for the minutes a
/// long paste takes, so the Keyboard popover could show a running paste's progress and its cancel
/// hint and refuse to be clicked. The reason for the drop is "a click must not reach the *target*
/// mid-paste", and **a click on the chrome never reaches the target at all** — [`Sink::Chrome`]
/// hands it to egui and stops there, in every state, which is the invariant
/// `a_chrome_click_never_reaches_the_target` pins.
///
/// Nothing is stranded by the reordering: a press routed to the chrome never becomes
/// [`Outstanding`] either (only `Action::Forward` sets that), so clause 1 still owes exactly the
/// buttons the target really holds.
///
/// A running paste does not otherwise change pointer routing: motion and the wheel still reach the
/// target, because a paste types and does not point, and taking the pointer away from the user for
/// the minutes it runs would be a surprise with no cause.
pub fn route_pointer(i: &RouteInputs, kind: PointerKind) -> Sink {
    if let PointerKind::Button { mask, down: false } = kind {
        if i.outstanding.holds_button(mask) {
            return Sink::Target; // 1
        }
    }
    if !i.chrome_unreachable() && (i.pointer_over_chrome || i.popover_open || i.modal_open) {
        return Sink::Chrome; // 2
    }
    if i.paste_running && matches!(kind, PointerKind::Button { down: true, .. }) {
        return Sink::Dropped; // 3
    }
    Sink::Target // 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{button, modifier, HidKey};

    /// Tab's usage, the key the whole routing rule exists for.
    const TAB: HidKey = HidKey::Usage(0x2B);

    /// One of each pointer class, with a plain LEFT press for the button.
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

    fn key() -> KeyAction {
        KeyAction::Forward {
            key: TAB,
            down: true,
        }
    }

    /// Every combination of the seven booleans-and-enums, for the exhaustive assertions below.
    fn all_inputs() -> Vec<RouteInputs> {
        let mut out = Vec::new();
        for capture in [
            CaptureState::Released,
            CaptureState::Engaging,
            CaptureState::Captured,
        ] {
            for pointer in [PointerMode::Absolute, PointerMode::Relative] {
                for popover_open in [false, true] {
                    for modal_open in [false, true] {
                        for pointer_over_chrome in [false, true] {
                            for paste_running in [false, true] {
                                for pointer_locked in [false, true] {
                                    out.push(RouteInputs {
                                        capture,
                                        pointer,
                                        popover_open,
                                        modal_open,
                                        pointer_over_chrome,
                                        paste_running,
                                        pointer_locked,
                                        outstanding: Outstanding::default(),
                                        paste_held: Outstanding::default(),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// Clause 6, and risk 2 of the design: `egui_winit` consumes Tab unconditionally, so a router
    /// that read `consumed` would eat it. With nothing open it reaches the target.
    #[test]
    fn tab_reaches_the_target_with_nothing_open() {
        let tab = KeyAction::Forward {
            key: HidKey::Usage(0x2B),
            down: true,
        };
        let i = RouteInputs::default();
        assert_eq!(route_key(&i, tab), Sink::Target);
        // And while captured, which is the state a user is actually in when they press it.
        let i = RouteInputs {
            capture: CaptureState::Captured,
            ..RouteInputs::default()
        };
        assert_eq!(route_key(&i, tab), Sink::Target);
    }

    /// Clause 1, exhaustively: the release key gets out of **every** state, including modal-open
    /// and relative capture.
    #[test]
    fn the_release_key_gets_out_from_every_state() {
        for i in all_inputs() {
            assert_eq!(
                route_key(&i, KeyAction::Release),
                Sink::Target,
                "the release key was trapped by {i:?}"
            );
        }
    }

    /// Clause 5: a popover open sends keys to the chrome — and only then.
    #[test]
    fn an_open_popover_takes_the_keyboard() {
        let i = RouteInputs {
            popover_open: true,
            ..RouteInputs::default()
        };
        assert_eq!(route_key(&i, key()), Sink::Chrome);
        let i = RouteInputs {
            modal_open: true,
            ..RouteInputs::default()
        };
        assert_eq!(route_key(&i, key()), Sink::Chrome);
        // Merely hovering the pill does not: the chrome has taken no keyboard focus.
        let i = RouteInputs {
            pointer_over_chrome: true,
            ..RouteInputs::default()
        };
        assert_eq!(route_key(&i, key()), Sink::Target);
    }

    /// Clause 4: under a relative-mode pointer lock the chrome is unreachable, so every key goes
    /// to the target however the other flags are set — and the release key still gets out, which
    /// the test above covers.
    #[test]
    fn relative_capture_makes_the_chrome_unreachable() {
        for popover_open in [false, true] {
            for pointer_over_chrome in [false, true] {
                let i = RouteInputs {
                    capture: CaptureState::Captured,
                    pointer: PointerMode::Relative,
                    pointer_locked: true,
                    popover_open,
                    pointer_over_chrome,
                    ..RouteInputs::default()
                };
                assert_eq!(route_key(&i, key()), Sink::Target, "{i:?}");
                for kind in kinds() {
                    assert_eq!(route_pointer(&i, kind), Sink::Target, "{i:?} {kind:?}");
                }
            }
        }
    }

    /// **Review item 6.** The lock is a *request*: `App::grab_pointer` tries `Locked`, then
    /// `Confined`, and warns when a compositor refuses both — leaving `grab == None` and the
    /// pointer free to walk onto the pill. Assuming it succeeded made the pill dead and turned the
    /// click that hit it into a real button press on a live desktop.
    #[test]
    fn a_refused_grab_leaves_the_chrome_reachable() {
        let refused = RouteInputs {
            capture: CaptureState::Captured,
            pointer: PointerMode::Relative,
            pointer_locked: false,
            pointer_over_chrome: true,
            ..RouteInputs::default()
        };
        assert_eq!(
            route_pointer(
                &refused,
                PointerKind::Button {
                    mask: button::LEFT,
                    down: true
                }
            ),
            Sink::Chrome,
            "a click on the pill must reach the pill, not the target"
        );
        assert_eq!(route_pointer(&refused, PointerKind::Motion), Sink::Chrome);
        // With a popover open the keyboard goes the same way: the chrome is on screen and usable.
        let refused = RouteInputs {
            popover_open: true,
            ..refused
        };
        assert_eq!(route_key(&refused, key()), Sink::Chrome);
        // And the same state with the grab actually in force is the unreachable one.
        let locked = RouteInputs {
            pointer_locked: true,
            ..refused
        };
        assert_eq!(route_key(&locked, key()), Sink::Target);
    }

    /// **Review item 1, keys.** A press forwarded to the target keeps its release: the up edge of
    /// an outstanding key routes to the target however the flags moved in between — a popover
    /// opened under it, the pointer wandered onto the pill, a paste started.
    #[test]
    fn the_up_of_a_key_the_target_holds_reaches_the_target_from_every_state() {
        for held in [TAB, HidKey::Modifier(modifier::LEFT_CTRL)] {
            let mut outstanding = Outstanding::default();
            outstanding.set_key(held, true);
            let up = KeyAction::Forward {
                key: held,
                down: false,
            };
            for i in all_inputs() {
                // Nothing is owed while the target holds nothing: that is the ordinary rule.
                let with = RouteInputs { outstanding, ..i };
                assert_eq!(
                    route_key(&with, up),
                    Sink::Target,
                    "the up of a held {held:?} was trapped by {with:?}"
                );
            }
        }
    }

    /// The exact sequence both reviewers described: Ctrl down over the video (forwarded), the pill
    /// clicked so `popover_open` flips, then Ctrl up.
    #[test]
    fn holding_ctrl_then_opening_a_popover_still_releases_ctrl() {
        let ctrl = HidKey::Modifier(modifier::LEFT_CTRL);
        let mut i = RouteInputs {
            capture: CaptureState::Captured,
            ..RouteInputs::default()
        };
        let down = KeyAction::Forward {
            key: ctrl,
            down: true,
        };
        assert_eq!(route_key(&i, down), Sink::Target);
        i.outstanding.set_key(ctrl, true);

        // The click on the pill opens a popover: the chrome now owns the keyboard.
        i.popover_open = true;
        i.pointer_over_chrome = true;
        assert_eq!(route_key(&i, key()), Sink::Chrome, "an ordinary key still");

        let up = KeyAction::Forward {
            key: ctrl,
            down: false,
        };
        assert_eq!(
            route_key(&i, up),
            Sink::Target,
            "Ctrl would stay held on the target"
        );
    }

    /// **Review item 1, buttons.** LEFT pressed over the video, dragged across the pill so
    /// `pointer_over_chrome` flips, released. The up is the target's.
    #[test]
    fn the_up_of_a_button_the_target_holds_reaches_the_target_from_every_state() {
        let mut outstanding = Outstanding::default();
        outstanding.set_button(button::LEFT, true);
        let up = PointerKind::Button {
            mask: button::LEFT,
            down: false,
        };
        for i in all_inputs() {
            let with = RouteInputs { outstanding, ..i };
            assert_eq!(
                route_pointer(&with, up),
                Sink::Target,
                "the up of a held LEFT was trapped by {with:?}"
            );
            // A *different* button's up is owed nothing, so it routes by the ordinary clauses.
            let other = PointerKind::Button {
                mask: button::RIGHT,
                down: false,
            };
            assert_eq!(
                route_pointer(&with, other),
                route_pointer(&i, other),
                "an unheld button must not be special-cased"
            );
        }
    }

    /// **A click never lands in the middle of a paste.** A host key is dropped because it corrupts
    /// the text; a button press is dropped because it moves the target's focus out from under the
    /// paste, and the rest of the text is then typed into whatever the click opened. Motion and
    /// the wheel are untouched: a paste types, it does not point.
    #[test]
    fn a_button_press_during_a_paste_is_dropped_and_its_up_is_still_owed() {
        let press = PointerKind::Button {
            mask: button::LEFT,
            down: true,
        };
        let up = PointerKind::Button {
            mask: button::LEFT,
            down: false,
        };
        for i in all_inputs() {
            let pasting = RouteInputs {
                paste_running: true,
                ..i
            };
            // Off the chrome it is dropped; on it — the pill, or any popover open — it is the
            // chrome's, which never reaches the target either (the lead's ruling on 4c item 7).
            let on_chrome = !pasting.chrome_unreachable()
                && (pasting.pointer_over_chrome || pasting.popover_open || pasting.modal_open);
            let expected = if on_chrome {
                Sink::Chrome
            } else {
                Sink::Dropped
            };
            assert_eq!(
                route_pointer(&pasting, press),
                expected,
                "a click reached the target under {pasting:?}"
            );
            assert_ne!(
                route_pointer(&pasting, press),
                Sink::Target,
                "no button press may reach the target during a paste: {pasting:?}"
            );
            // Motion and the wheel are unchanged by the paste.
            for kind in [PointerKind::Motion, PointerKind::Wheel] {
                assert_eq!(
                    route_pointer(&pasting, kind),
                    route_pointer(&i, kind),
                    "{kind:?} changed under a paste in {i:?}"
                );
            }
            // A button pressed *before* the paste began is still owed its release.
            let mut outstanding = i.outstanding;
            outstanding.set_button(button::LEFT, true);
            let holding = RouteInputs {
                outstanding,
                ..pasting
            };
            assert_eq!(route_pointer(&holding, up), Sink::Target, "{holding:?}");
        }
    }

    /// **The pill stays clickable while a paste runs** (the lead's ruling on 4c item 7).
    ///
    /// 4c dropped every press from every state, which also disabled the Keyboard popover that
    /// shows the running paste's progress and its cancel hint. The reason for the drop is the
    /// *target*, and a chrome click never reaches it: `Sink::Chrome` ends at egui.
    #[test]
    fn the_pill_stays_clickable_while_a_paste_runs() {
        let press = PointerKind::Button {
            mask: button::LEFT,
            down: true,
        };
        let over_pill = RouteInputs {
            capture: CaptureState::Captured,
            pointer: PointerMode::Absolute,
            pointer_over_chrome: true,
            paste_running: true,
            ..RouteInputs::default()
        };
        assert_eq!(
            route_pointer(&over_pill, press),
            Sink::Chrome,
            "over the pill"
        );

        let popover = RouteInputs {
            pointer_over_chrome: false,
            popover_open: true,
            ..over_pill
        };
        assert_eq!(
            route_pointer(&popover, press),
            Sink::Chrome,
            "a popover open takes the pointer even off the rectangle, paste or no paste"
        );

        // And off the chrome it is still dropped, which is the half that protects the target.
        let over_video = RouteInputs {
            pointer_over_chrome: false,
            popover_open: false,
            ..over_pill
        };
        assert_eq!(route_pointer(&over_video, press), Sink::Dropped);

        // Under a relative-mode lock the chrome is unreachable, so the drop is the only answer.
        let locked = RouteInputs {
            pointer: PointerMode::Relative,
            pointer_locked: true,
            pointer_over_chrome: true,
            ..over_pill
        };
        assert_eq!(route_pointer(&locked, press), Sink::Dropped);
    }

    /// The prelude's other half: what a release-all owes is every key **and** every button, so
    /// `held_buttons` reports them in a fixed order and nothing else.
    #[test]
    fn held_buttons_are_every_button_the_target_holds() {
        let mut o = Outstanding::default();
        assert!(o.held_buttons().is_empty());
        o.set_button(button::MIDDLE, true);
        o.set_button(button::LEFT, true);
        assert_eq!(o.held_buttons(), vec![button::LEFT, button::MIDDLE]);
        o.set_button(button::LEFT, false);
        assert_eq!(o.held_buttons(), vec![button::MIDDLE]);
    }

    /// The set is a set: pressing and releasing leaves nothing behind, and an unmapped button
    /// (mask 0) is never held.
    #[test]
    fn the_outstanding_set_tracks_presses_and_releases() {
        let mut o = Outstanding::default();
        assert!(o.is_empty());
        o.set_key(TAB, true);
        o.set_key(HidKey::Modifier(modifier::LEFT_CTRL), true);
        o.set_button(button::LEFT, true);
        assert!(o.holds_key(TAB));
        assert!(o.holds_key(HidKey::Modifier(modifier::LEFT_CTRL)));
        assert!(!o.holds_key(HidKey::Usage(0x04)));
        assert!(o.holds_button(button::LEFT));
        assert!(!o.holds_button(button::RIGHT));
        assert!(!o.holds_button(0), "an unmapped button is never held");
        assert!(!o.is_empty());

        o.set_key(TAB, false);
        o.set_key(HidKey::Modifier(modifier::LEFT_CTRL), false);
        o.set_button(button::LEFT, false);
        assert!(o.is_empty(), "{o:?}");

        // A release-all is what clears the lot (§2.6).
        o.set_key(HidKey::Usage(0xFF), true);
        assert!(o.holds_key(HidKey::Usage(0xFF)), "the top of the bit set");
        o.clear();
        assert!(o.is_empty());
    }

    /// Clause 3: the paste's own text is the only thing typed while it runs.
    #[test]
    fn a_running_paste_drops_host_keys_but_not_the_release_key() {
        let i = RouteInputs {
            capture: CaptureState::Captured,
            paste_running: true,
            ..RouteInputs::default()
        };
        assert_eq!(route_key(&i, key()), Sink::Dropped);
        assert_eq!(route_key(&i, KeyAction::Release), Sink::Target);
        // And in relative mode too, which is where the research design's clause order would have
        // forwarded the keystroke into the middle of the pasted text.
        let i = RouteInputs {
            pointer: PointerMode::Relative,
            ..i
        };
        assert_eq!(route_key(&i, key()), Sink::Dropped);
        assert_eq!(route_key(&i, KeyAction::Release), Sink::Target);
    }

    /// The paste chord is viewer-local: it reaches the event loop from every state, exactly like
    /// the release key, and is never handed to the chrome or dropped. A trigger a menu could
    /// swallow is a trigger the user cannot reach.
    #[test]
    fn the_paste_chord_reaches_the_event_loop_from_every_state() {
        for popover_open in [false, true] {
            for modal_open in [false, true] {
                for pointer_locked in [false, true] {
                    for pointer in [PointerMode::Absolute, PointerMode::Relative] {
                        let i = RouteInputs {
                            capture: CaptureState::Captured,
                            pointer,
                            popover_open,
                            modal_open,
                            pointer_locked,
                            ..RouteInputs::default()
                        };
                        assert_eq!(route_key(&i, KeyAction::Paste), Sink::Target, "{i:?}");
                    }
                }
            }
        }
    }

    /// **Release wins.** The chord during a running paste is the release — it cancels rather than
    /// restarting — and with no paste running it is the paste.
    #[test]
    fn the_chord_starts_a_paste_and_cancels_a_running_one() {
        assert_eq!(
            resolve_paste_chord(KeyAction::Paste, false),
            KeyAction::Paste
        );
        assert_eq!(
            resolve_paste_chord(KeyAction::Paste, true),
            KeyAction::Release,
            "a second chord mid-paste must cancel, never restart"
        );
        // …and that resolved action routes to the release path, so the cancel is §2.6's.
        let running = RouteInputs {
            capture: CaptureState::Captured,
            paste_running: true,
            ..RouteInputs::default()
        };
        assert_eq!(
            route_key(&running, resolve_paste_chord(KeyAction::Paste, true)),
            Sink::Target
        );
    }

    /// Nothing else is touched by the resolution: it is about one action, in one state.
    #[test]
    fn resolving_the_chord_changes_nothing_else() {
        for action in [
            KeyAction::Release,
            KeyAction::Swallowed,
            KeyAction::Unmapped,
            key(),
        ] {
            for running in [false, true] {
                assert_eq!(resolve_paste_chord(action, running), action, "{action:?}");
            }
        }
    }

    /// The prelude a paste opens with comes from here: every key the target holds, usages before
    /// modifiers, so a usage is never released after the modifier it was pressed under.
    #[test]
    fn the_held_keys_list_releases_usages_before_modifiers() {
        let mut o = Outstanding::default();
        o.set_key(HidKey::Modifier(modifier::LEFT_SHIFT), true);
        o.set_key(HidKey::Usage(0x04), true);
        o.set_key(HidKey::Usage(0x2B), true);
        assert_eq!(
            o.held_keys(),
            vec![
                HidKey::Usage(0x04),
                HidKey::Usage(0x2B),
                HidKey::Modifier(modifier::LEFT_SHIFT),
            ]
        );
        // A released key leaves the list, and an empty set has no prelude at all.
        o.set_key(HidKey::Usage(0x04), false);
        assert_eq!(o.held_keys().len(), 2);
        o.clear();
        assert!(o.held_keys().is_empty());
    }

    /// Clause 2 is about the **host's** presses. A key the running paste is holding is not one,
    /// and a host key-up for it is dropped — forwarded, it would end a capital half way through
    /// and put the rest of the character in the wrong case.
    #[test]
    fn a_host_key_up_for_a_key_the_paste_is_holding_is_dropped() {
        let shift = HidKey::Modifier(modifier::LEFT_SHIFT);
        let mut held = Outstanding::default();
        held.set_key(shift, true);

        // The user was holding Shift, the paste is now holding Shift, and the user lets go.
        let i = RouteInputs {
            capture: CaptureState::Captured,
            paste_running: true,
            outstanding: held,
            paste_held: held,
            ..RouteInputs::default()
        };
        let up = KeyAction::Forward {
            key: shift,
            down: false,
        };
        assert_eq!(route_key(&i, up), Sink::Dropped);

        // The same up, for a key only the *host* is holding — during the paste's prelude, before
        // it has been released — is still owed to the target.
        let i = RouteInputs {
            paste_held: Outstanding::default(),
            ..i
        };
        assert_eq!(route_key(&i, up), Sink::Target);
    }

    /// Pointer clause 3: motion over the pill is the chrome's, motion off it is the target's.
    /// This is the case `EventResponse.consumed` gets wrong — `CursorMoved` consults
    /// `is_using_pointer()`, which is false for a hover.
    #[test]
    fn pointer_motion_over_the_pill_goes_to_the_chrome_and_off_it_to_the_target() {
        let over = RouteInputs {
            capture: CaptureState::Captured,
            pointer_over_chrome: true,
            ..RouteInputs::default()
        };
        let off = RouteInputs {
            pointer_over_chrome: false,
            ..over
        };
        for kind in kinds() {
            assert_eq!(route_pointer(&over, kind), Sink::Chrome, "{kind:?}");
            assert_eq!(route_pointer(&off, kind), Sink::Target, "{kind:?}");
        }
    }

    /// Pointer clause 3, the other half: while a popover is open the click that would dismiss it
    /// is the chrome's even when it lands outside the rectangle. A dismissing click must never
    /// reach a live desktop.
    ///
    /// Routing it here is half the story; **the popover actually closing** is `ChromeUi::build`'s
    /// job and is asserted where it happens, in `tests/viewer_chrome_ui.rs`'s
    /// `clicking_outside_an_open_popover_closes_it`.
    #[test]
    fn a_click_outside_an_open_popover_goes_to_the_chrome_rather_than_the_target() {
        let i = RouteInputs {
            capture: CaptureState::Captured,
            popover_open: true,
            pointer_over_chrome: false,
            ..RouteInputs::default()
        };
        assert_eq!(
            route_pointer(
                &i,
                PointerKind::Button {
                    mask: button::LEFT,
                    down: true
                }
            ),
            Sink::Chrome
        );
    }

    /// What `input_map` already discarded stays discarded, in every state.
    #[test]
    fn swallowed_and_unmapped_keys_go_nowhere() {
        for i in all_inputs() {
            for a in [KeyAction::Swallowed, KeyAction::Unmapped] {
                assert_eq!(route_key(&i, a), Sink::Dropped, "{i:?} {a:?}");
            }
        }
    }
}
