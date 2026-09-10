//! The capture state machine driven against the **real** input path (plan §2.6, §2.8, §12 Stage 1).
//!
//! `viewer::state::reduce` is pure and `viewer_state.rs` asserts its transition table. This test
//! closes the other half: that performing the actions it emits, against a real writer thread over a
//! fake link, produces the behaviour §2.6 describes — nothing is written before capture, a
//! release-all reaches the link on every trigger, input stays refused until the user deliberately
//! re-engages and the writer has acknowledged, and a click that captured never leaves a button held
//! on the target.
//!
//! **The driver feeds real events, not pre-classified ones.** It hands the reducer the button
//! transitions winit would deliver and lets the reducer decide which of them capture. That is
//! deliberate: the defect this file grew a test for was a copy of the "which click captures" rule
//! living outside the reducer and drifting out of step with it, and a driver that re-implemented
//! the same rule would have hidden the drift rather than shown it.
//!
//! It needs no display and no hardware: the reducer has no window in it, and the link is
//! `input::testing::fake_link`.

use std::time::Duration;

use nanokvm::input::testing::{fake_link, FakeControl};
use nanokvm::input::{self, Producer, ReleaseReason, SubmitError};
use nanokvm::proto::cmd;
use nanokvm::proto::report::button;
use nanokvm::proto::HidKey;
use nanokvm::viewer::state::{reduce, Action, CaptureState, Session, Trigger};

/// A stand-in for `App::perform`: everything the event loop would do, minus the window.
///
/// Kept deliberately literal rather than shared with the viewer, so that a change to the viewer's
/// action handling shows up here as a difference rather than being silently mirrored.
struct Driver {
    producer: Producer,
    session: Session,
    inhibitor_armed: bool,
    pointer_grabbed: bool,
    exited: bool,
    forwarded: usize,
    engage_attempts: usize,
    releases: Vec<ReleaseReason>,
}

impl Driver {
    fn new(producer: Producer) -> Self {
        Driver {
            producer,
            session: Session::new(),
            inhibitor_armed: false,
            pointer_grabbed: false,
            exited: false,
            forwarded: 0,
            engage_attempts: 0,
            releases: Vec::new(),
        }
    }

    fn feed(&mut self, trigger: Trigger) {
        let (next, actions) = reduce(self.session, trigger);
        self.session = next;
        for action in actions {
            match action {
                Action::ArmInhibitor => self.inhibitor_armed = true,
                Action::DisarmInhibitor => self.inhibitor_armed = false,
                Action::GrabPointer => self.pointer_grabbed = true,
                Action::ReleasePointer => self.pointer_grabbed = false,
                Action::TryEngage => {
                    self.engage_attempts += 1;
                    let t = match self.producer.engage() {
                        Ok(()) => Trigger::EngageSucceeded,
                        Err(_) => Trigger::EngageDeferred,
                    };
                    self.feed(t);
                }
                Action::ReleaseAll(reason) => {
                    self.releases.push(reason);
                    self.producer.request_release_all(reason);
                }
                Action::Forward(event) => {
                    self.forwarded += 1;
                    if let Err(e) = self.producer.submit(event) {
                        self.feed(Trigger::SubmitFailed(e));
                    }
                }
                Action::Exit => self.exited = true,
            }
        }
    }

    fn state(&self) -> CaptureState {
        self.session.capture()
    }

    /// One left-button transition, exactly as `WindowEvent::MouseInput` delivers it.
    fn left(&mut self, down: bool) {
        self.feed(Trigger::Button {
            mask: button::LEFT,
            down,
        });
    }

    /// The viewer's engage retry: the event loop never blocks on the acknowledgement, it retries
    /// on the next tick (§2.6).
    fn capture(&mut self) {
        self.left(true);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while self.state() == CaptureState::Engaging && std::time::Instant::now() < deadline {
            self.feed(Trigger::Retry);
            std::thread::yield_now();
        }
        assert_eq!(
            self.state(),
            CaptureState::Captured,
            "engagement never completed"
        );
        // Let go of the click that captured, so the session is left in the state a user spends
        // almost all of their time in.
        self.left(false);
    }

    /// Every mouse button mask the writer has handed the transport, in order.
    fn mouse_masks(control: &FakeControl) -> Vec<u8> {
        control
            .frames()
            .into_iter()
            .filter(|f| f.cmd == cmd::SEND_MS_REL_DATA || f.cmd == cmd::SEND_MS_ABS_DATA)
            .filter_map(|f| f.payload.get(1).copied())
            .collect()
    }
}

fn setup() -> (Driver, FakeControl) {
    let (link, control) = fake_link();
    let (producer, writer) = input::spawn(link, input::Config::default());
    // The writer outlives the test body; leaking the handle keeps it running without a release-all
    // firing from `Drop` in the middle of an assertion.
    std::mem::forget(writer);
    (Driver::new(producer), control)
}

#[test]
fn nothing_is_written_before_the_user_captures() {
    let (mut driver, control) = setup();
    // Every input class, while released.
    driver.feed(Trigger::Key {
        key: HidKey::Usage(0x04),
        down: true,
    });
    driver.feed(Trigger::Button {
        mask: button::RIGHT,
        down: true,
    });
    driver.feed(Trigger::Motion(nanokvm::input::Event::PointerAbs {
        x: 100,
        y: 100,
    }));
    assert_eq!(driver.forwarded, 0, "input was forwarded while released");
    assert_eq!(
        control.frame_count(),
        0,
        "bytes reached the link while released"
    );
    assert_eq!(driver.state(), CaptureState::Released);
}

#[test]
fn the_capture_click_is_consumed_and_never_reaches_the_target() {
    let (mut driver, control) = setup();
    driver.capture();
    assert!(driver.inhibitor_armed);
    assert!(driver.pointer_grabbed);
    // §CLAUDE.md: a blind click on a live desktop can launch or destroy something. Neither the
    // click that captures nor its release may produce a frame.
    assert_eq!(control.frame_count(), 0);
    assert_eq!(driver.forwarded, 0);
}

#[test]
fn captured_input_reaches_the_link() {
    let (mut driver, control) = setup();
    driver.capture();
    driver.feed(Trigger::Key {
        key: HidKey::Usage(0x04),
        down: true,
    });
    control.wait_for_frames(1);
    assert_eq!(driver.forwarded, 1);
}

/// §2.6's trigger list, each one asserted to release and to disengage the producer.
#[test]
fn every_release_trigger_releases_and_disengages() {
    let triggers = [
        (Trigger::FocusLost, ReleaseReason::FocusLost),
        (Trigger::InhibitorInactive, ReleaseReason::UserRequested),
        (Trigger::ReleaseKey, ReleaseReason::UserRequested),
        (
            Trigger::SubmitFailed(SubmitError::Overflow),
            ReleaseReason::Overflow,
        ),
        (Trigger::CloseRequested, ReleaseReason::Shutdown),
    ];
    for (trigger, reason) in triggers {
        let (mut driver, control) = setup();
        driver.capture();
        driver.feed(trigger);

        assert_eq!(driver.state(), CaptureState::Released, "on {trigger:?}");
        assert_eq!(driver.releases, vec![reason], "on {trigger:?}");
        assert!(
            !driver.inhibitor_armed,
            "inhibitor left armed on {trigger:?}"
        );
        assert!(
            !driver.pointer_grabbed,
            "pointer left grabbed on {trigger:?}"
        );

        // The writer synthesizes the release itself; the local barrier is the ack (§2.6.1).
        assert!(
            driver
                .producer
                .wait_until_engageable(Duration::from_secs(5)),
            "the release-all was never acknowledged on {trigger:?}"
        );
        assert!(
            control.frame_count() > 0,
            "no release frames on {trigger:?}"
        );

        // §2.8: input stays disengaged until deliberate recapture. Nothing the viewer does
        // afterwards may forward anything.
        assert!(
            !driver.producer.is_engaged(),
            "still engaged after {trigger:?}"
        );
        driver.feed(Trigger::Key {
            key: HidKey::Usage(0x04),
            down: true,
        });
        assert_eq!(driver.forwarded, 0, "forwarded after {trigger:?}");
    }
}

#[test]
fn close_requested_also_asks_the_loop_to_exit() {
    let (mut driver, _control) = setup();
    driver.capture();
    driver.feed(Trigger::CloseRequested);
    assert!(driver.exited);
    assert_eq!(driver.releases, vec![ReleaseReason::Shutdown]);
}

#[test]
fn a_deliberate_recapture_re_engages() {
    let (mut driver, _control) = setup();
    driver.capture();
    driver.feed(Trigger::FocusLost);
    assert!(!driver.producer.is_engaged());
    // The user clicks again. `engage` fails with `NotYetAcked` until the writer catches up, which
    // is exactly why `Engaging` exists.
    driver.capture();
    assert!(driver.producer.is_engaged());
    driver.feed(Trigger::Motion(nanokvm::input::Event::PointerAbs {
        x: 1,
        y: 2,
    }));
    assert_eq!(driver.forwarded, 1);
}

// ---------------------------------------------------------------------------------------------
// B1, end to end: the bytes the writer hands the transport, not merely the actions the reducer
// emitted. The failure this guards against is a mouse frame whose button mask never returns to
// zero — LEFT held down on someone's console with nothing left to release it.
// ---------------------------------------------------------------------------------------------

#[test]
fn b1_a_click_released_while_engaging_leaves_no_button_held_on_the_link() {
    let (mut driver, control) = setup();

    // 1. Capture and release, exactly as pressing Pause does.
    driver.capture();
    driver.feed(Trigger::ReleaseKey);
    assert_eq!(driver.state(), CaptureState::Released);

    // 2. The user clicks again straight away. The release-all has not been acknowledged yet, so
    //    the state machine sits in `Engaging`.
    driver.left(true);
    assert_eq!(
        driver.state(),
        CaptureState::Engaging,
        "the re-capture must be deferred for this to be the scenario under test"
    );
    // 3. …and lets go of that click while it is still waiting.
    driver.left(false);

    // 4. The acknowledgement lands and the retry succeeds.
    assert!(driver
        .producer
        .wait_until_engageable(Duration::from_secs(5)));
    driver.feed(Trigger::Retry);
    assert_eq!(driver.state(), CaptureState::Captured);

    // 5. One perfectly ordinary click on the target.
    let before = control.frame_count();
    driver.left(true);
    driver.left(false);
    control.wait_for_frames(before + 2);

    let masks = Driver::mouse_masks(&control);
    assert_eq!(
        masks.last().copied(),
        Some(0u8),
        "the last mouse frame still carries a button mask: LEFT is held on the target. \
         Masks written, in order: {masks:?}"
    );
    assert!(
        masks.contains(&button::LEFT),
        "the press itself never reached the link: {masks:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// M1: re-capture must not wait for the 250 ms tick. An input event arriving while `Engaging` is
// itself the retry, so no tick is fed anywhere in this test.
// ---------------------------------------------------------------------------------------------

#[test]
fn m1_input_while_engaging_retries_the_engagement_without_any_tick() {
    let (mut driver, _control) = setup();
    driver.capture();
    driver.feed(Trigger::ReleaseKey);

    driver.left(true);
    assert_eq!(driver.state(), CaptureState::Engaging);
    let attempts = driver.engage_attempts;

    // The user starts typing. Not a tick in sight.
    assert!(driver
        .producer
        .wait_until_engageable(Duration::from_secs(5)));
    driver.feed(Trigger::Key {
        key: HidKey::Usage(0x04),
        down: true,
    });

    assert!(
        driver.engage_attempts > attempts,
        "the keystroke did not retry the engagement"
    );
    assert_eq!(
        driver.state(),
        CaptureState::Captured,
        "the acknowledgement had landed, so the very next event should have completed capture"
    );
}

// ---------------------------------------------------------------------------------------------
// N4: the producer can end the session by itself. The viewer's tick asks
// `state::self_release_trigger` and feeds whatever it says; here the release is asserted to reach
// the link.
// ---------------------------------------------------------------------------------------------

#[test]
fn n4_a_producer_that_disengages_itself_releases_capture() {
    let (mut driver, control) = setup();
    driver.capture();

    // The writer's own cancellation path: something else triggered a release-all, so the producer
    // is disengaged while the viewer still believes it is capturing.
    driver.producer.request_release_all(ReleaseReason::LinkDown);
    assert!(!driver.producer.is_engaged());
    assert_eq!(
        driver.state(),
        CaptureState::Captured,
        "the viewer has not noticed yet — that is the condition N4 covers"
    );

    let trigger = nanokvm::viewer::state::self_release_trigger(
        driver.producer.is_engaged(),
        driver.producer.stats().link_down,
    )
    .expect("a release is owed");
    driver.feed(trigger);

    assert_eq!(driver.state(), CaptureState::Released);
    assert!(!driver.inhibitor_armed);
    assert!(!driver.pointer_grabbed);
    assert!(driver
        .producer
        .wait_until_engageable(Duration::from_secs(5)));
    assert!(
        control.frame_count() > 0,
        "no release frames reached the link"
    );
}

// ---------------------------------------------------------------------------------------------
// N5: `App::tick` feeds `CloseRequested` on every tick while an interrupt is pending. One close,
// one epoch.
// ---------------------------------------------------------------------------------------------

#[test]
fn n5_a_repeated_close_bumps_the_epoch_once() {
    let (mut driver, _control) = setup();
    driver.capture();

    driver.feed(Trigger::CloseRequested);
    assert!(driver
        .producer
        .wait_until_engageable(Duration::from_secs(5)));
    let epoch = driver.producer.stats().requested_epoch;

    // Twenty more ticks' worth, as an interrupted loop would produce over five seconds.
    for _ in 0..20 {
        driver.feed(Trigger::CloseRequested);
    }

    assert_eq!(
        driver.releases,
        vec![ReleaseReason::Shutdown],
        "released more than once"
    );
    assert_eq!(
        driver.producer.stats().requested_epoch,
        epoch,
        "a repeated close bumped the epoch again"
    );
    assert!(driver.exited);
}
