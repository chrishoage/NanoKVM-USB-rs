//! The window title (plan §2.6.1, §2.8, §6.1 S1-2 and S2-1, §12 Stage 1).
//!
//! The title is the only place most of this client's conditions are ever reported: a user who
//! never reads a log still sees it. §2.8 says an overflow must be surfaced rather than counted,
//! §6.1 S2-1 says a stall and a disconnection must be distinguishable, and §2.6.1 says an
//! undelivered release-all must be said out loud. Those are behaviours, so the wording is
//! asserted here rather than eyeballed on a live run — and `viewer::title::compose` is pure
//! precisely so it can be.
//!
//! Each test states the fault it describes and asserts on the *distinction*, not just on the
//! presence of some text: several of these strings are only useful because they differ from
//! another string the same window shows in a different situation.

use std::time::Duration;

use nanokvm::capture::PipelineState;
use nanokvm::input::SubmitError;
use nanokvm::proto::report::button;
use nanokvm::viewer::state::{reduce, CaptureState, Session, Trigger};
use nanokvm::viewer::title::{compose, TitleFacts};

/// Everything fine: running, frames flowing, link up, nothing owed.
fn healthy<'a>(session: Session) -> TitleFacts<'a> {
    TitleFacts {
        present_mode: "Fifo",
        session,
        pipeline: PipelineState::Running,
        disconnected_for: None,
        frames_stalled_for: Some(Duration::from_millis(8)),
        link_down: false,
        link_down_for: None,
        reconnect_attempts: 0,
        release_unsent: false,
        format_mismatch_accepted: false,
        video_size: Some((1920, 1080)),
        negotiated_size: Some((1920, 1080)),
    }
}

fn captured() -> Session {
    let (s, _) = reduce(
        Session::new(),
        Trigger::Button {
            mask: button::LEFT,
            down: true,
        },
    );
    let (s, _) = reduce(s, Trigger::EngageSucceeded);
    assert_eq!(s.capture(), CaptureState::Captured);
    s
}

#[test]
fn a_healthy_session_says_only_what_it_is_doing() {
    let title = compose(&healthy(captured()));
    assert!(
        title.starts_with("NanoKVM-USB — Fifo — [captured"),
        "{title}"
    );
    assert!(
        !title.contains('—') || !title.contains("reconnecting"),
        "nothing is wrong, so nothing may be claimed: {title}"
    );
    assert!(!title.contains("stopped"), "{title}");
    assert!(!title.contains("UNSENT"), "{title}");
}

#[test]
fn the_release_binding_is_on_screen_while_captured() {
    // §12 Stage 1: the viewer's own way out must be shown, because the compositor's is
    // configuration this client does not control.
    let title = compose(&healthy(captured()));
    assert!(title.contains("Pause"), "{title}");
}

// ---------------------------------------------------------------------------------------------
// S2-1 — a stall and a disconnection are different faults and must read differently.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_stall_says_frames_stopped_and_never_that_the_device_is_gone() {
    let mut f = healthy(captured());
    f.frames_stalled_for = Some(Duration::from_secs(3));
    let title = compose(&f);
    assert!(title.contains("frames stopped 3s ago"), "{title}");
    assert!(
        !title.contains("gone") && !title.contains("disconnected"),
        "the device is still there; only the frames stopped: {title}"
    );
    // A5: the signal is not reportable on this hardware and must never be inferred.
    assert!(!title.to_lowercase().contains("signal"), "{title}");
}

#[test]
fn a_disconnection_says_the_device_is_gone_and_that_the_client_is_retrying() {
    let mut f = healthy(captured());
    f.pipeline = PipelineState::Reconnecting;
    f.disconnected_for = Some(Duration::from_secs(7));
    let title = compose(&f);
    assert!(
        title.contains("capture device gone, reconnecting (7s)"),
        "{title}"
    );
    assert!(
        !title.contains("frames stopped"),
        "a disconnection must not be worded as a stall: {title}"
    );
}

#[test]
fn a_stall_and_a_disconnection_never_produce_the_same_words() {
    let mut stalled = healthy(captured());
    stalled.frames_stalled_for = Some(Duration::from_secs(4));
    let mut gone = healthy(captured());
    gone.pipeline = PipelineState::Reconnecting;
    gone.disconnected_for = Some(Duration::from_secs(4));
    assert_ne!(
        compose(&stalled),
        compose(&gone),
        "§6.1 S2-1: the two conditions must be distinguishable on screen"
    );
}

#[test]
fn a_brief_gap_between_frames_is_not_worth_saying() {
    let mut f = healthy(captured());
    // Under the stall threshold: at 60 fps a frame is 16 ms, and a title that flickered
    // "frames stopped 0s ago" between frames would be noise, not information.
    f.frames_stalled_for = Some(Duration::from_millis(400));
    assert!(!compose(&f).contains("frames stopped"), "{}", compose(&f));
}

#[test]
fn before_the_first_frame_the_title_says_so_rather_than_nothing() {
    let mut f = healthy(Session::new());
    f.frames_stalled_for = None;
    assert!(
        compose(&f).contains("waiting for the first frame"),
        "{}",
        compose(&f)
    );
}

#[test]
fn a_stopped_pipeline_is_not_called_a_reconnecting_one() {
    let mut f = healthy(captured());
    f.pipeline = PipelineState::Stopped;
    let title = compose(&f);
    assert!(title.contains("capture stopped"), "{title}");
    assert!(!title.contains("reconnecting"), "{title}");
}

// ---------------------------------------------------------------------------------------------
// §2.7 — "serial DOWN" used to be permanent. It is not any more, and the title says what is
// happening about it.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_down_link_says_how_long_and_how_many_attempts() {
    let mut f = healthy(captured());
    f.link_down = true;
    f.link_down_for = Some(Duration::from_secs(12));
    f.reconnect_attempts = 9;
    let title = compose(&f);
    assert!(
        title.contains("serial DOWN, reconnecting (12s, 9 attempts)"),
        "{title}"
    );
}

#[test]
fn a_link_that_was_never_up_still_reports_its_attempts() {
    // Startup with no dongle: there is no outage to time, but there is something to say.
    let mut f = healthy(Session::new());
    f.link_down = true;
    f.link_down_for = None;
    f.reconnect_attempts = 3;
    let title = compose(&f);
    assert!(
        title.contains("serial DOWN, reconnecting (3 attempts)"),
        "{title}"
    );
}

#[test]
fn a_serial_failure_and_a_capture_failure_are_both_shown_at_once() {
    // They are independent subsystems (§6.1 S1-1) and can fail together; a title that showed
    // only the first would hide half of what is wrong.
    let mut f = healthy(captured());
    f.pipeline = PipelineState::Reconnecting;
    f.disconnected_for = Some(Duration::from_secs(2));
    f.link_down = true;
    f.link_down_for = Some(Duration::from_secs(2));
    let title = compose(&f);
    assert!(title.contains("capture device gone"), "{title}");
    assert!(title.contains("serial DOWN"), "{title}");
}

// ---------------------------------------------------------------------------------------------
// §2.8 — the session ended for a reason, and the reason is on screen until input is flowing
// again. Stage 1 showed the same "[click or Enter to capture]" for a queue overflow as for the
// user pressing Pause.
// ---------------------------------------------------------------------------------------------

#[test]
fn an_overflow_is_named_in_the_title_and_not_only_in_a_counter() {
    let (session, _) = reduce(captured(), Trigger::SubmitFailed(SubmitError::Overflow));
    let title = compose(&healthy(session));
    assert!(
        title.contains("input interrupted: queue overflowed — click to re-capture"),
        "{title}"
    );
}

#[test]
fn a_link_down_session_says_input_stopped_rather_than_pretending_it_is_idle() {
    let (session, _) = reduce(captured(), Trigger::SubmitFailed(SubmitError::LinkDown));
    let title = compose(&healthy(session));
    assert!(title.contains("input stopped: serial link down"), "{title}");
}

#[test]
fn a_failed_session_does_not_look_like_a_deliberate_release() {
    let (failed, _) = reduce(captured(), Trigger::SubmitFailed(SubmitError::Overflow));
    let (asked, _) = reduce(captured(), Trigger::ReleaseKey);
    assert_eq!(failed.capture(), CaptureState::Released);
    assert_eq!(asked.capture(), CaptureState::Released);
    assert_ne!(
        compose(&healthy(failed)),
        compose(&healthy(asked)),
        "§2.8: an overflow is a failed session, and must not read like the user asking to stop"
    );
}

// ---------------------------------------------------------------------------------------------
// §2.6.1 — an undelivered release-all means the target may still be holding keys, and nothing
// local can fix it. It outranks tidiness.
// ---------------------------------------------------------------------------------------------

#[test]
fn an_unsent_release_warns_that_the_target_may_still_hold_keys() {
    let mut f = healthy(Session::new());
    f.release_unsent = true;
    let title = compose(&f);
    assert!(
        title.contains("release UNSENT — target may still hold keys"),
        "{title}"
    );
}

#[test]
fn every_condition_can_be_shown_together() {
    // The worst case: nothing is working and the target may be holding keys. All of it must
    // still be legible, and none of it may be dropped to make room.
    let (session, _) = reduce(captured(), Trigger::SubmitFailed(SubmitError::Overflow));
    let title = compose(&TitleFacts {
        present_mode: "Fifo",
        session,
        pipeline: PipelineState::Reconnecting,
        disconnected_for: Some(Duration::from_secs(30)),
        frames_stalled_for: Some(Duration::from_secs(30)),
        link_down: true,
        link_down_for: Some(Duration::from_secs(30)),
        reconnect_attempts: 40,
        release_unsent: true,
        format_mismatch_accepted: true,
        video_size: Some((640, 480)),
        negotiated_size: Some((1920, 1080)),
    });
    for expected in [
        "capture device gone, reconnecting (30s)",
        "serial DOWN, reconnecting (30s, 40 attempts)",
        "input interrupted: queue overflowed",
        "video 640x480, negotiated 1920x1080 not established",
        "release UNSENT",
    ] {
        assert!(title.contains(expected), "missing {expected:?} in {title}");
    }
}

/// **An accepted format mismatch is surfaced, not counted (C5).** When the capture watchdog has
/// spent its restarts *and* its one escalation reopen and given up, the window shows a 640x480
/// image on a 1080p session for the rest of the run. C5's argument about the input side applies
/// unchanged here: a counter in a five-second log line is "a silent counter by another name", and
/// what the user sees is the picture.
///
/// The fragment is a function of the two *sizes*, not of the counter, so it is the sizes that
/// clear it: a frame back at the negotiated size takes it out of the title on its own, which is
/// what the watchdog's own re-arming makes happen.
#[test]
fn an_accepted_format_mismatch_is_in_the_title_and_a_negotiated_frame_clears_it() {
    let mut f = healthy(captured());
    f.format_mismatch_accepted = true;
    f.video_size = Some((640, 480));
    f.negotiated_size = Some((1920, 1080));
    let title = compose(&f);
    assert!(
        title.contains("— video 640x480, negotiated 1920x1080 not established"),
        "an accepted mismatch was left to the log: {title}"
    );

    // The frames came back. The counter is still nonzero — it is cumulative and always will be —
    // and the title must nevertheless stop saying the mode was not established.
    f.video_size = Some((1920, 1080));
    let recovered = compose(&f);
    assert!(
        !recovered.contains("not established"),
        "the fragment outlived the condition: {recovered}"
    );
    assert_eq!(recovered, compose(&healthy(captured())));

    // And the sizes alone are not enough: A6's benign transient puts a 640x480 frame on screen
    // after every reopen, and the watchdog sits it out. Saying "not established" then would
    // train the user to ignore the words.
    let mut transient = healthy(captured());
    transient.video_size = Some((640, 480));
    transient.negotiated_size = Some((1920, 1080));
    assert!(
        !compose(&transient).contains("not established"),
        "the benign A6 transient was reported as a failure: {}",
        compose(&transient)
    );
}
