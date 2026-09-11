//! The capture state machine, as a pure reducer (plan §2.6, §2.8, §12 Stage 1).
//!
//! §2.6 lists the release-all triggers and §12 adds the compositor-driven one. That list is the
//! correctness surface of this module, so it is written as `reduce(session, trigger) -> (session,
//! actions)` with no window, no producer and no clock in scope: every trigger and every action can
//! be asserted in a unit test that needs no display.
//!
//! Three capture phases:
//!
//! ```text
//!            left-click in window / Enter                 engage() == Ok
//!   Released ────────────────────────────▶ Engaging ─────────────────────────▶ Captured
//!      ▲                                      │  ▲                                 │
//!      │                                      └──┘ engage() == NotYetAcked         │
//!      │        focus lost │ inhibitor inactive │ Pause │ submit failed │ close     │
//!      └───────────────────────────────────────────────────────────────────────────┘
//!                        always: request_release_all(reason), disarm, ungrab
//! ```
//!
//! **`Engaging` exists because acknowledgement is asynchronous.** §2.6: "a new session must not
//! emit input until it observes `acked_epoch >=` its own epoch", and [`crate::input::Producer::engage`]
//! enforces that by failing with `NotYetAcked`. Waiting for it on the event loop would block input
//! handling, so the state machine holds `Engaging` and retries — including on any input event that
//! arrives while it waits, so re-capture does not sit out a whole 250 ms tick.
//!
//! **No input is forwarded outside `Captured`.** That is not a convention observed by the caller;
//! it is a property of this reducer, asserted directly by its tests.
//!
//! # Why the consumed-edge bookkeeping lives here
//!
//! The click and the `Enter` that *enter* capture must not reach the target — a blind click on a
//! live console can launch or destroy something (`CLAUDE.md`) — and neither may their matching
//! release, or the target sees half a transition. That is a two-edge rule, and a two-edge rule
//! kept outside the state machine is a second, unsynchronised state machine: the event loop used
//! to hold a `consumed_left_press` flag that a button-up in `Released`/`Engaging` never cleared,
//! so the *next* genuine click had its press forwarded and its release eaten, leaving LEFT held on
//! the target. The flags are therefore part of [`Session`], and the reducer maintains this
//! invariant across every state:
//!
//! > **A press that was forwarded always has a release that is forwardable.**
//!
//! It follows from three rules, each visible in one place below: a consumed edge clears its flag
//! on the matching up *in any state*; every transition to `Released` clears both flags (the
//! release-all has zeroed the target's masks anyway, §2.6); and a press arriving while a consume
//! is still pending clears the flag before forwarding, because the up it was waiting for can no
//! longer arrive.

use crate::input::{Event, ReleaseOutcome, ReleaseReason, ReleaseRecord, SubmitError};
use crate::proto::report::button;
use crate::proto::HidKey;

/// Whether the viewer is forwarding input to the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaptureState {
    /// Nothing is forwarded, the compositor keeps its shortcuts, the pointer is free.
    #[default]
    Released,
    /// The user asked to capture; the inhibitor and the grab are up but the writer has not yet
    /// acknowledged the previous release-all, so nothing may be sent (§2.6).
    Engaging,
    /// Input is forwarded to the target.
    Captured,
}

impl CaptureState {
    /// Short text for the window title, including how to get out — §12 Stage 1 requires the
    /// viewer's own release binding to be shown on screen.
    pub fn title_fragment(&self, release_key: &str) -> String {
        match self {
            CaptureState::Released => "[click or Enter to capture]".to_string(),
            CaptureState::Engaging => "[engaging…]".to_string(),
            CaptureState::Captured => {
                format!("[captured — {release_key} or Mod+Esc releases]")
            }
        }
    }
}

/// Why input stopped, kept until the user deliberately re-captures (§2.8).
///
/// §2.8 requires an overflow to be "surfaced — not a silent counter", and the Stage 1 viewer did
/// not meet that: an overflow released the session, logged a warning, and put the title back to
/// `[click or Enter to capture]`, which is indistinguishable from the user having pressed `Pause`.
/// A user who never reads the log therefore saw input stop for no stated reason. The notice is
/// carried in the session so that the *reason the last session ended* is on screen until a new
/// one starts, and so that a pure reducer test can pin both raising it and clearing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// [`SubmitError::Overflow`]: coalescing could not make room, so the session was cancelled.
    Overflow,
    /// [`SubmitError::LinkDown`]: the serial transport went away under the session (§2.7 is
    /// reconnecting, but nothing may be forwarded until it comes back and the user re-captures).
    LinkDown,
}

impl Notice {
    /// What the title says. Both name the cause *and* the way out, because "input interrupted"
    /// with no instruction is only marginally better than silence.
    pub fn title_fragment(self) -> &'static str {
        match self {
            Notice::Overflow => "input interrupted: queue overflowed — click to re-capture",
            Notice::LinkDown => "input stopped: serial link down",
        }
    }
}

/// The reducer's whole state: the capture phase, the consumed-edge bookkeeping described in the
/// module docs, and the one-shot close latch.
///
/// The fields are private so the invariant in the module docs cannot be broken from outside; the
/// accessors below are everything a caller — or a test — needs to observe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Session {
    capture: CaptureState,
    /// A left button-down was consumed to enter capture, so its matching up must be consumed too.
    consumed_left: bool,
    /// The same for the `Enter` that entered capture (§12 Stage 1's keyboard capture binding).
    consumed_enter: bool,
    /// A close has already been requested. Closing releases once, not once per tick: `App::tick`
    /// feeds `CloseRequested` while `interrupted` is set, and a release-all per tick would bump
    /// `requested_epoch` every 250 ms until the loop actually stops (§2.6).
    closing: bool,
    /// Why the last session ended, when it ended for a reason the user needs telling (§2.8).
    /// Cleared by the next successful engagement — the deliberate recapture §2.8 asks for — and
    /// by nothing else, so it cannot be missed by looking away for a tick.
    notice: Option<Notice>,
}

impl Session {
    /// A fresh session: released, nothing consumed, not closing.
    pub fn new() -> Self {
        Self::default()
    }

    /// The capture phase.
    pub fn capture(&self) -> CaptureState {
        self.capture
    }

    /// Whether a close has already been requested and released for.
    pub fn is_closing(&self) -> bool {
        self.closing
    }

    /// Whether a left button-down has been consumed and its up is still owed. Observable so the
    /// invariant in the module docs can be asserted directly.
    pub fn left_consumed(&self) -> bool {
        self.consumed_left
    }

    /// The same for `Enter`.
    pub fn enter_consumed(&self) -> bool {
        self.consumed_enter
    }

    /// Why input stopped, if it stopped for a reason worth showing (§2.8).
    pub fn notice(&self) -> Option<Notice> {
        self.notice
    }
}

/// Everything that can move the state machine.
///
/// Motion, buttons and keys are triggers rather than a separate "forward this" path so that the
/// no-forwarding-outside-`Captured` rule — and the consumed-edge rule above it — are enforced in
/// one place. In particular there is **no** separate "this click captures" trigger: the caller
/// reports the button transition it saw and the reducer decides what it means, because deciding
/// that outside the reducer is what let the two views of a click drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// A mapped key transition (host repeat already discarded by [`super::input_map::map_key`]).
    Key { key: HidKey, down: bool },
    /// A mouse button transition, as its report bit.
    Button { mask: u8, down: bool },
    /// Pointer or wheel motion, already translated.
    Motion(Event),
    /// The viewer's own release binding (§12 Stage 1). Never forwarded.
    ReleaseKey,
    /// `WindowEvent::Focused(false)`. niri does not deactivate the inhibitor on focus loss, so
    /// this is client-driven (§2.6, §12 Stage 1).
    FocusLost,
    /// The compositor set the shortcut inhibitor `inactive` — on niri, the user's `Mod+Escape`.
    /// A compositor-driven "let me out" signal, and the user's way out that does not depend on
    /// this client (q6c).
    InhibitorInactive,
    /// [`crate::input::Producer::submit`] refused an event (§2.8: overflow is an explicit failure
    /// requiring deliberate recapture, not a silent degradation) — or the producer was found
    /// disengaged, or the link down, on a tick with no event having been refused at all.
    SubmitFailed(SubmitError),
    /// The window manager or the user asked to close.
    CloseRequested,
    /// [`crate::input::Producer::engage`] returned `Ok`.
    EngageSucceeded,
    /// [`crate::input::Producer::engage`] returned an error.
    EngageDeferred,
    /// A timer tick or any other event, offering the chance to retry engagement.
    Retry,
}

impl Trigger {
    /// Whether this is user input rather than a control signal.
    pub fn is_input(&self) -> bool {
        matches!(
            self,
            Trigger::Key { .. } | Trigger::Button { .. } | Trigger::Motion(_)
        )
    }
}

/// What the caller must do as a result. The reducer never performs any of these itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Create the `zwp_keyboard_shortcuts_inhibitor_v1` (§1.5).
    ArmInhibitor,
    /// Destroy it.
    DisarmInhibitor,
    /// Lock or confine the pointer and hide the cursor (relative mode only).
    GrabPointer,
    /// Release the grab and show the cursor.
    ReleasePointer,
    /// Call [`crate::input::Producer::engage`] and feed the answer back in.
    TryEngage,
    /// Call [`crate::input::Producer::request_release_all`] with this reason (§2.6).
    ReleaseAll(ReleaseReason),
    /// Submit this event to the target.
    Forward(Event),
    /// Leave the event loop.
    Exit,
}

/// The actions one trigger produced, in the order they must be performed.
///
/// A `Vec` rather than an inline array: it does not allocate when empty, which is the common case,
/// and the one small allocation on a forwarded event is far below the ~100 reports per second the
/// link can carry (§5.1).
pub type Actions = Vec<Action>;

/// The release sequence, in order: ask for the release-all first so the writer starts discarding
/// stale work immediately, then give the compositor and the pointer back.
fn leave_capture(reason: ReleaseReason) -> Actions {
    vec![
        Action::ReleaseAll(reason),
        Action::DisarmInhibitor,
        Action::ReleasePointer,
    ]
}

fn begin_capture() -> Actions {
    vec![Action::ArmInhibitor, Action::GrabPointer, Action::TryEngage]
}

fn forward(event: Event) -> Actions {
    vec![Action::Forward(event)]
}

/// Which §2.8 notice, if any, a refused submission leaves on screen.
///
/// Only the two failures the user can neither predict nor undo get one. `Disengaged` does not:
/// the producer disengages *after* some other cancellation, whose own trigger already carried
/// whatever notice was owed — and a link failure behind it is already in the title from
/// [`crate::input::Stats::link_down`]. `ShuttingDown` does not either: the window is going away.
fn notice_for(error: SubmitError) -> Option<Notice> {
    match error {
        SubmitError::Overflow => Some(Notice::Overflow),
        SubmitError::LinkDown => Some(Notice::LinkDown),
        SubmitError::Disengaged | SubmitError::ShuttingDown => None,
    }
}

/// Which §2.6 reason a refused submission corresponds to.
fn reason_for(error: SubmitError) -> ReleaseReason {
    match error {
        // The producer has already triggered this reason itself; asking again coalesces into the
        // same sequence (§2.6, "setting an already-set flag is a no-op").
        SubmitError::Overflow => ReleaseReason::Overflow,
        SubmitError::LinkDown => ReleaseReason::LinkDown,
        SubmitError::Disengaged => ReleaseReason::CaptureReleased,
        // The writer is already running the shutdown's own release-all and will exit after it;
        // asking again coalesces into that same sequence rather than starting a new one.
        SubmitError::ShuttingDown => ReleaseReason::Shutdown,
    }
}

/// Leave capture: the full §2.6 sequence, and both consumed edges dropped because the release-all
/// zeroes the target's key and button masks — there is no held press left for an up to match.
fn release(mut session: Session, reason: ReleaseReason) -> (Session, Actions) {
    session.capture = CaptureState::Released;
    session.consumed_left = false;
    session.consumed_enter = false;
    (session, leave_capture(reason))
}

/// One transition of a key or button whose *press* may be consumed to enter capture.
///
/// `consumed` selects the session flag for this particular edge pair and `event` is what the
/// target would see were it forwarded. Both `Enter` and the left mouse button follow exactly this
/// shape, so they share it rather than each growing their own copy — two copies drifting apart is
/// the defect this replaces.
fn capturing_edge(
    mut session: Session,
    down: bool,
    consumed: fn(&mut Session) -> &mut bool,
    event: Event,
) -> (Session, Actions) {
    if down {
        return match session.capture {
            CaptureState::Captured => {
                // A genuine press. Any pending consume is dropped first: the up it was waiting
                // for can no longer arrive, and leaving it set would eat *this* press's release
                // and leave the button held on the target.
                *consumed(&mut session) = false;
                (session, forward(event))
            }
            CaptureState::Released => {
                // The press that captures, consumed here and never forwarded.
                *consumed(&mut session) = true;
                session.capture = CaptureState::Engaging;
                (session, begin_capture())
            }
            CaptureState::Engaging => {
                *consumed(&mut session) = true;
                (session, vec![Action::TryEngage])
            }
        };
    }
    // The up. A consumed press eats its up in **any** state, including one reached after the press
    // was consumed — that is the whole point.
    if *consumed(&mut session) {
        *consumed(&mut session) = false;
        return (session, Actions::new());
    }
    match session.capture {
        CaptureState::Captured => (session, forward(event)),
        CaptureState::Engaging => (session, vec![Action::TryEngage]),
        CaptureState::Released => (session, Actions::new()),
    }
}

/// An ordinary input transition: forwarded only while captured, and a retry opportunity while
/// engaging.
fn plain_input(session: Session, event: Event) -> (Session, Actions) {
    match session.capture {
        CaptureState::Captured => (session, forward(event)),
        CaptureState::Engaging => (session, vec![Action::TryEngage]),
        CaptureState::Released => (session, Actions::new()),
    }
}

/// The §2.6.1 release-outcome notice, as a two-field state machine over
/// [`crate::input::Stats::last_release`].
///
/// An `Unsent` release means the target may still be holding keys and nothing local can fix it,
/// so it is shown until something contradicts it. What contradicts it is the **next** release
/// that was actually submitted — which, across a §2.7 reconnect, is the release-all the writer
/// sends on the replacement link (`Writer::commission`, reason `Reconnected`): the keys really
/// are released then, and the title must stop claiming otherwise.
///
/// It lives here, in the pure module, because "does the notice clear when the reconnect's release
/// lands?" is a question about this bookkeeping and not about winit. `App` keeps the logging.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReleaseNotice {
    /// The epoch of the last record acted on, so each release is reported once rather than every
    /// tick (§2.6.1).
    reported_epoch: Option<u64>,
    raised: bool,
}

impl ReleaseNotice {
    /// Fold in the latest [`crate::input::Stats::last_release`].
    ///
    /// Returns the record when it has not been seen before, so the caller logs it exactly once.
    /// Records are matched by epoch: epochs increase monotonically (§2.6) and every completed
    /// sequence has its own, so "same epoch" is "same release" and nothing else.
    pub fn observe(&mut self, last_release: Option<ReleaseRecord>) -> Option<ReleaseRecord> {
        let record = last_release?;
        if self.reported_epoch == Some(record.epoch) {
            return None;
        }
        self.reported_epoch = Some(record.epoch);
        self.raised = match record.outcome {
            ReleaseOutcome::Unsent => true,
            ReleaseOutcome::Submitted => false,
        };
        Some(record)
    }

    /// Whether the title should be carrying the "release UNSENT" warning.
    pub fn raised(&self) -> bool {
        self.raised
    }
}

/// Which release a `Captured` session owes when the producer has gone quiet without ever refusing
/// a submission, or `None` if it owes none.
///
/// The producer can end a session by itself: the writer hits `LinkDown` and triggers its own
/// cancellation (§2.6), or an overflow raised elsewhere disengages it. No event reaches the event
/// loop when that happens, so a session that watched only its own submissions would sit in
/// `Captured` — title claiming capture, inhibitor armed, pointer grabbed — over a producer that can
/// no longer send anything, and would never run the §2.6 release sequence for it. The tick asks
/// this instead, and feeds back whatever it returns.
///
/// Disengagement is reported ahead of a link failure because it is the more specific fact: the
/// producer disengages only after a cancellation has already been triggered for some reason, while
/// `link_down` says no more than that the transport is gone.
pub fn self_release_trigger(engaged: bool, link_down: bool) -> Option<Trigger> {
    if !engaged {
        return Some(Trigger::SubmitFailed(SubmitError::Disengaged));
    }
    if link_down {
        return Some(Trigger::SubmitFailed(SubmitError::LinkDown));
    }
    None
}

/// The whole state machine. Pure: same input, same output, no side effects.
pub fn reduce(session: Session, trigger: Trigger) -> (Session, Actions) {
    use CaptureState::*;
    use Trigger::*;

    let mut s = session;

    // Closing wins from every state, and still releases first: a window that vanishes while the
    // target holds a key leaves the key held (§2.6, clean shutdown). It releases exactly **once** —
    // the caller feeds this on every tick while an interrupt is pending, and a release per tick
    // would bump `requested_epoch` every 250 ms until the loop actually stops.
    if let CloseRequested = trigger {
        if s.closing {
            return (s, vec![Action::Exit]);
        }
        let (mut s, mut actions) = release(s, ReleaseReason::Shutdown);
        s.closing = true;
        actions.push(Action::Exit);
        return (s, actions);
    }
    // After a close, nothing re-captures and nothing is forwarded: the writer's shutdown
    // release-all is the last thing that may reach the target (§2.6, `SubmitError::ShuttingDown`).
    if s.closing {
        return (s, Actions::new());
    }

    match trigger {
        // ---- the two edges that can enter capture -------------------------------------------
        Button { mask, down } if mask == button::LEFT => capturing_edge(
            s,
            down,
            |s| &mut s.consumed_left,
            Event::Button {
                button: button::LEFT,
                down,
            },
        ),
        Key { key, down } if Some(key) == super::input_map::enter_key() => {
            capturing_edge(s, down, |s| &mut s.consumed_enter, Event::Key { key, down })
        }

        // ---- ordinary input ------------------------------------------------------------------
        Key { key, down } => plain_input(s, Event::Key { key, down }),
        Button { mask, down } => plain_input(s, Event::Button { button: mask, down }),
        Motion(e) => plain_input(s, e),

        // ---- every release trigger in §2.6 -----------------------------------------------------
        FocusLost if s.capture != Released => release(s, ReleaseReason::FocusLost),
        InhibitorInactive if s.capture != Released => release(s, ReleaseReason::UserRequested),
        ReleaseKey if s.capture != Released => release(s, ReleaseReason::UserRequested),
        SubmitFailed(e) if s.capture != Released => {
            let (mut s, actions) = release(s, reason_for(e));
            // §2.8: surface it. The notice outlives the release, because the release is over in
            // a tick and the user may be looking at the target rather than at the title.
            if let Some(notice) = notice_for(e) {
                s.notice = Some(notice);
            }
            (s, actions)
        }

        // ---- waiting on the writer's acknowledgement (§2.6) -------------------------------------
        EngageSucceeded if s.capture == Engaging => {
            // The consumed edges deliberately survive this transition: the click that captured is
            // very often still held when the acknowledgement lands, and its up must still be
            // eaten.
            s.capture = Captured;
            // Input is flowing again, so the reason the *previous* session ended has been read
            // and acted on. Clearing it here rather than on the click means a recapture that is
            // still waiting on the writer's acknowledgement keeps saying why it is waiting.
            s.notice = None;
            (s, Actions::new())
        }
        Retry if s.capture == Engaging => (s, vec![Action::TryEngage]),

        // ---- everything else is deliberately inert ----------------------------------------------
        // Released forwards nothing and releases nothing, and a stray engage answer for a session
        // that has already ended must not resurrect it.
        _ => (s, Actions::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enter() -> HidKey {
        super::super::input_map::enter_key().expect("Enter is mapped")
    }

    fn engaging() -> Session {
        let (s, _) = reduce(
            Session::new(),
            Trigger::Button {
                mask: button::LEFT,
                down: true,
            },
        );
        assert_eq!(s.capture(), CaptureState::Engaging);
        s
    }

    fn captured() -> Session {
        let (s, _) = reduce(engaging(), Trigger::EngageSucceeded);
        assert_eq!(s.capture(), CaptureState::Captured);
        s
    }

    /// A captured session with no consumed edge outstanding — where a user is after the click that
    /// captured has been let go of.
    fn captured_clean() -> Session {
        let (s, _) = reduce(
            captured(),
            Trigger::Button {
                mask: button::LEFT,
                down: false,
            },
        );
        assert!(!s.left_consumed());
        s
    }

    fn released_by(trigger: Trigger, expected: ReleaseReason) {
        for from in [engaging(), captured()] {
            let (state, actions) = reduce(from, trigger);
            assert_eq!(
                state.capture(),
                CaptureState::Released,
                "from {from:?} on {trigger:?}"
            );
            assert!(
                actions.contains(&Action::ReleaseAll(expected)),
                "from {from:?} on {trigger:?}: expected ReleaseAll({expected:?}), got {actions:?}"
            );
            assert!(actions.contains(&Action::DisarmInhibitor), "{actions:?}");
            assert!(actions.contains(&Action::ReleasePointer), "{actions:?}");
            assert!(
                !actions.iter().any(|a| matches!(a, Action::Forward(_))),
                "a release must not forward anything: {actions:?}"
            );
            assert!(!state.left_consumed(), "a release must clear the left edge");
            assert!(
                !state.enter_consumed(),
                "a release must clear the enter edge"
            );
        }
    }

    /// §2.6's trigger list, one assertion per trigger.
    #[test]
    fn focus_loss_releases() {
        released_by(Trigger::FocusLost, ReleaseReason::FocusLost);
    }

    #[test]
    fn inhibitor_inactive_releases() {
        // The user's Mod+Escape on niri. The compositor keeps that bind for itself even while
        // inhibiting, and the client is told by the inhibitor going inactive (q6c).
        released_by(Trigger::InhibitorInactive, ReleaseReason::UserRequested);
    }

    #[test]
    fn the_release_key_releases() {
        released_by(Trigger::ReleaseKey, ReleaseReason::UserRequested);
    }

    #[test]
    fn a_refused_submission_releases_with_its_own_reason() {
        released_by(
            Trigger::SubmitFailed(SubmitError::Overflow),
            ReleaseReason::Overflow,
        );
        released_by(
            Trigger::SubmitFailed(SubmitError::LinkDown),
            ReleaseReason::LinkDown,
        );
        released_by(
            Trigger::SubmitFailed(SubmitError::Disengaged),
            ReleaseReason::CaptureReleased,
        );
        released_by(
            Trigger::SubmitFailed(SubmitError::ShuttingDown),
            ReleaseReason::Shutdown,
        );
    }

    #[test]
    fn close_releases_and_exits_from_every_state() {
        for from in [Session::new(), engaging(), captured()] {
            let (state, actions) = reduce(from, Trigger::CloseRequested);
            assert_eq!(state.capture(), CaptureState::Released);
            assert!(state.is_closing());
            assert!(actions.contains(&Action::ReleaseAll(ReleaseReason::Shutdown)));
            assert_eq!(actions.last(), Some(&Action::Exit));
        }
    }

    #[test]
    fn a_click_captures_and_is_consumed() {
        let (state, actions) = reduce(
            Session::new(),
            Trigger::Button {
                mask: button::LEFT,
                down: true,
            },
        );
        assert_eq!(state.capture(), CaptureState::Engaging);
        assert_eq!(
            actions,
            vec![Action::ArmInhibitor, Action::GrabPointer, Action::TryEngage]
        );
        // The point: a blind click never reaches the target's desktop.
        assert!(!actions.iter().any(|a| matches!(a, Action::Forward(_))));
        assert!(state.left_consumed());
    }

    #[test]
    fn enter_captures_from_the_keyboard() {
        let (state, actions) = reduce(
            Session::new(),
            Trigger::Key {
                key: enter(),
                down: true,
            },
        );
        assert_eq!(state.capture(), CaptureState::Engaging);
        assert!(actions.contains(&Action::TryEngage));
        assert!(!actions.iter().any(|a| matches!(a, Action::Forward(_))));
        assert!(state.enter_consumed());
    }

    #[test]
    fn engagement_retries_until_the_writer_acknowledges() {
        // §2.6: input must not flow until acked_epoch reaches the session's epoch, and the event
        // loop must not block waiting for it.
        let (state, actions) = reduce(engaging(), Trigger::EngageDeferred);
        assert_eq!(state.capture(), CaptureState::Engaging);
        assert!(actions.is_empty());
        let (state, actions) = reduce(state, Trigger::Retry);
        assert_eq!(state.capture(), CaptureState::Engaging);
        assert_eq!(actions, vec![Action::TryEngage]);
        let (state, actions) = reduce(state, Trigger::EngageSucceeded);
        assert_eq!(state.capture(), CaptureState::Captured);
        assert!(actions.is_empty());
    }

    #[test]
    fn nothing_is_forwarded_outside_captured() {
        let inputs = [
            Trigger::Key {
                key: HidKey::Usage(0x04),
                down: true,
            },
            Trigger::Button {
                mask: button::RIGHT,
                down: true,
            },
            Trigger::Button {
                mask: button::LEFT,
                down: true,
            },
            Trigger::Button {
                mask: button::LEFT,
                down: false,
            },
            Trigger::Motion(Event::PointerAbs { x: 1, y: 2 }),
            Trigger::Motion(Event::PointerRel { dx: 3, dy: 4 }),
            Trigger::Motion(Event::Wheel { delta: 1 }),
            Trigger::Retry,
            Trigger::EngageSucceeded,
            Trigger::EngageDeferred,
        ];
        for from in [Session::new(), engaging()] {
            for t in inputs {
                let (_, actions) = reduce(from, t);
                assert!(
                    !actions.iter().any(|a| matches!(a, Action::Forward(_))),
                    "{from:?} + {t:?} forwarded something: {actions:?}"
                );
            }
        }
    }

    #[test]
    fn captured_forwards_every_input_class() {
        let cases: [(Trigger, Event); 5] = [
            (
                Trigger::Key {
                    key: HidKey::Usage(0x04),
                    down: true,
                },
                Event::Key {
                    key: HidKey::Usage(0x04),
                    down: true,
                },
            ),
            (
                Trigger::Button {
                    mask: button::LEFT,
                    down: true,
                },
                Event::Button {
                    button: button::LEFT,
                    down: true,
                },
            ),
            (
                Trigger::Motion(Event::PointerAbs { x: 10, y: 20 }),
                Event::PointerAbs { x: 10, y: 20 },
            ),
            (
                Trigger::Motion(Event::PointerRel { dx: -1, dy: 2 }),
                Event::PointerRel { dx: -1, dy: 2 },
            ),
            (
                Trigger::Motion(Event::Wheel { delta: -1 }),
                Event::Wheel { delta: -1 },
            ),
        ];
        for (trigger, event) in cases {
            let (state, actions) = reduce(captured_clean(), trigger);
            assert_eq!(state.capture(), CaptureState::Captured);
            assert_eq!(actions, vec![Action::Forward(event)]);
        }
    }

    #[test]
    fn the_release_key_does_nothing_when_already_released() {
        let (state, actions) = reduce(Session::new(), Trigger::ReleaseKey);
        assert_eq!(state.capture(), CaptureState::Released);
        assert!(actions.is_empty());
    }

    #[test]
    fn the_title_always_says_how_to_get_out() {
        assert!(CaptureState::Captured
            .title_fragment("Pause")
            .contains("Pause"));
        assert!(CaptureState::Captured
            .title_fragment("Pause")
            .contains("Mod+Esc"));
        assert!(CaptureState::Released
            .title_fragment("Pause")
            .contains("capture"));
    }

    #[test]
    fn input_triggers_are_distinguishable_from_control_signals() {
        assert!(Trigger::Key {
            key: HidKey::Usage(0x04),
            down: true
        }
        .is_input());
        assert!(Trigger::Button {
            mask: button::LEFT,
            down: false
        }
        .is_input());
        assert!(Trigger::Motion(Event::Wheel { delta: 1 }).is_input());
        for t in [
            Trigger::Retry,
            Trigger::FocusLost,
            Trigger::CloseRequested,
            Trigger::ReleaseKey,
            Trigger::InhibitorInactive,
            Trigger::EngageSucceeded,
            Trigger::EngageDeferred,
        ] {
            assert!(!t.is_input(), "{t:?}");
        }
    }
}
