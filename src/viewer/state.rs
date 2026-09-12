//! Pure input-capture state machine.
//!
//! Capture waits for the previous release to be acknowledged locally. The click or Enter
//! that engages capture is consumed, including its release edge. Focus loss, explicit
//! release, overflow, and link loss cancel the session and preserve the reason for display.

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
    /// acknowledged the previous release-all, so nothing may be sent.
    Engaging,
    /// Input is forwarded to the target.
    Captured,
}

impl CaptureState {
    /// Window-title capture state and release hint.
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

/// Reason input stopped, retained until successful recapture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// [`SubmitError::Overflow`]: coalescing could not make room, so the session was cancelled.
    Overflow,
    /// The serial link failed; forwarding waits for recovery and recapture.
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
    /// The same for the `Enter` that entered capture.
    consumed_enter: bool,
    /// A close has already been requested. Closing releases once, not once per tick: `App::tick`
    /// feeds `CloseRequested` while `interrupted` is set, and a release-all per tick would bump
    /// `requested_epoch` every 250 ms until the loop stops.
    closing: bool,
    /// Session-failure notice, cleared only by successful engagement.
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

    /// Why input stopped, if it stopped for a reason worth showing.
    pub fn notice(&self) -> Option<Notice> {
        self.notice
    }
}

/// Everything that can move the state machine.
///
/// Motion, buttons and keys are triggers rather than a separate "forward this" path so that the
/// no-forwarding-outside-`Captured` rule — and the consumed-edge rule above it — are enforced in
/// one place. In particular there is no separate "this click captures" trigger: the caller
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
    /// The viewer's own release binding. Never forwarded.
    ReleaseKey,
    /// `WindowEvent::Focused(false)`. niri does not deactivate the inhibitor on focus loss, so
    /// this is client-driven.
    FocusLost,
    /// The compositor set the shortcut inhibitor `inactive` — on niri, the user's `Mod+Escape`.
    /// A compositor-driven "let me out" signal, and the user's way out that does not depend on
    /// this client.
    InhibitorInactive,
    /// Input refusal or producer-driven loss of the captured session.
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
    /// Create the `zwp_keyboard_shortcuts_inhibitor_v1`.
    ArmInhibitor,
    /// Destroy it.
    DisarmInhibitor,
    /// Lock or confine the pointer and hide the cursor (relative mode only).
    GrabPointer,
    /// Release the grab and show the cursor.
    ReleasePointer,
    /// Call [`crate::input::Producer::engage`] and feed the answer back in.
    TryEngage,
    /// Call [`crate::input::Producer::request_release_all`] with this reason.
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
/// link can carry.
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

/// Visible notice for overflow or link loss. Ordinary disengagement already has
/// a release cause; shutdown closes the window and needs no persistent notice.
fn notice_for(error: SubmitError) -> Option<Notice> {
    match error {
        SubmitError::Overflow => Some(Notice::Overflow),
        SubmitError::LinkDown => Some(Notice::LinkDown),
        SubmitError::Disengaged | SubmitError::ShuttingDown => None,
    }
}

/// Cancellation reason corresponding to a refused submission.
fn reason_for(error: SubmitError) -> ReleaseReason {
    match error {
        // The producer has already triggered this reason itself; asking again coalesces into the
        // same sequence.
        SubmitError::Overflow => ReleaseReason::Overflow,
        SubmitError::LinkDown => ReleaseReason::LinkDown,
        SubmitError::Disengaged => ReleaseReason::CaptureReleased,
        // The writer is already running the shutdown's own release-all and will exit after it;
        // asking again coalesces into that same sequence rather than starting a new one.
        SubmitError::ShuttingDown => ReleaseReason::Shutdown,
    }
}

/// Leave capture and discard consumed-edge bookkeeping. Release-all clears the
/// target state those release edges would otherwise match.
fn release(mut session: Session, reason: ReleaseReason) -> (Session, Actions) {
    session.capture = CaptureState::Released;
    session.consumed_left = false;
    session.consumed_enter = false;
    (session, leave_capture(reason))
}

/// One transition of a key or button whose *press* may be consumed to enter capture.
///
/// `consumed` selects the session flag for this particular edge pair and `event` is what the
/// target would see were it forwarded. Both `Enter` and the left mouse button follow this
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
    // The up. A consumed press eats its up in any state, including one reached after the press
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

/// Track the last release outcome and whether it was already reported.
///
/// An unsent warning persists until a later submitted release, including commissioning
/// on a replacement link. Local state alone cannot clear target-held keys.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReleaseNotice {
    /// The epoch of the last record acted on, so each release is reported once rather than every
    /// tick.
    reported_epoch: Option<u64>,
    raised: bool,
}

impl ReleaseNotice {
    /// Fold in the latest [`crate::input::Stats::last_release`].
    ///
    /// Returns the record when it has not been seen before, so the caller logs it once.
    /// Records are matched by epoch: epochs increase monotonically and every completed
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

/// Detect an input producer that ended a session without a host submission.
/// The event loop must release its inhibitor and pointer grab even when idle.
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
    // target holds a key leaves the key held. It releases once —
    // the caller feeds this on every tick while an interrupt is pending, and a release per tick
    // would bump `requested_epoch` every 250 ms until the loop stops.
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
    // release-all is the last thing that may reach the target.
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

        FocusLost if s.capture != Released => release(s, ReleaseReason::FocusLost),
        InhibitorInactive if s.capture != Released => release(s, ReleaseReason::UserRequested),
        ReleaseKey if s.capture != Released => release(s, ReleaseReason::UserRequested),
        SubmitFailed(e) if s.capture != Released => {
            let (mut s, actions) = release(s, reason_for(e));
            // Keep the failure notice past the release tick so it remains observable.
            if let Some(notice) = notice_for(e) {
                s.notice = Some(notice);
            }
            (s, actions)
        }

        // ---- waiting on the writer's acknowledgement -------------------------------------
        EngageSucceeded if s.capture == Engaging => {
            // The consumed edges survive this transition: the click that captured is
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

        // ---- everything else is inert ----------------------------------------------
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

    /// Every release trigger must clear capture state.
    #[test]
    fn focus_loss_releases() {
        released_by(Trigger::FocusLost, ReleaseReason::FocusLost);
    }

    #[test]
    fn inhibitor_inactive_releases() {
        // The user's Mod+Escape on niri. The compositor keeps that bind for itself even while
        // inhibiting, and the client is told by the inhibitor going inactive.
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
        // Poll engagement without blocking until the requested epoch is acknowledged.
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
