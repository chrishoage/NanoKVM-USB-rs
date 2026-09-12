//! Visible connection, capture, release, and audio status in the window title.

use std::time::Duration;

use nanokvm::audio::{RingConfig, RingCounts};
use nanokvm::capture::PipelineState;
use nanokvm::input::SubmitError;
use nanokvm::proto::report::button;
use nanokvm::viewer::state::{reduce, CaptureState, Session, Trigger};
use nanokvm::viewer::title::{compose, AudioTitle, TitleFacts};

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
        audio: healthy_audio(),
    }
}

/// Healthy audio fixture for tests of unrelated title segments.
fn healthy_audio<'a>() -> AudioTitle<'a> {
    AudioTitle::On {
        muted: false,
        counts: RingCounts::default(),
        ring: RingConfig::default(),
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
    // Show the local release key because compositor shortcuts are user-configurable.
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
    // the signal is not reportable on this hardware and must never be inferred.
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
        "the two conditions must be distinguishable on screen"
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

// Serial reconnect progress.

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
    // They are independent subsystems and can fail together; a title that showed
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

// Retain the capture-exit reason until input resumes.

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
        "an overflow is a failed session, and must not read like the user asking to stop"
    );
}

// An unsent release must remain visible because target keys may still be held.

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
        audio: AudioTitle::Unavailable("capture hw:8 is gone: No such device"),
    });
    for expected in [
        "capture device gone, reconnecting (30s)",
        "serial DOWN, reconnecting (30s, 40 attempts)",
        "input interrupted: queue overflowed",
        "video 640x480, negotiated 1920x1080 not established",
        "audio unavailable: capture hw:8 is gone: No such device",
        "release UNSENT",
    ] {
        assert!(title.contains(expected), "missing {expected:?} in {title}");
    }
}

/// An accepted format mismatch is surfaced, not counted. When the capture watchdog has
/// spent its restarts *and* its one escalation reopen and given up, the window shows a 640x480
/// image on a 1080p session for the rest of the run. argument about the input side applies
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
        title.contains("video 640x480, negotiated 1920x1080 not established"),
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

    // And the sizes alone are not enough: benign transient puts a 640x480 frame on screen
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

// ---- audio --------------------------------------------------------------------

/// `--no-audio` must leave the viewer *exactly* as left it. The title is the only surface
/// a viewer had, so "" is checkable here: two words, no counters, no
/// buffer depth, no reason — and nothing about audio anywhere else in the string.
#[test]
fn no_audio_says_only_that_it_is_off() {
    let mut f = healthy(captured());
    f.audio = AudioTitle::Off;
    let title = compose(&f);
    assert!(title.contains(" audio off"), "{title}");
    for forbidden in [
        "buffer",
        "muted",
        "overrun",
        "underrun",
        "drift",
        "unavailable",
    ] {
        assert!(!title.contains(forbidden), "{forbidden:?} in {title}");
    }
}

/// Label configured capacity accurately; it is not measured end-to-end latency.
#[test]
fn running_audio_states_the_configured_buffer_depth() {
    let mut f = healthy(captured());
    f.audio = AudioTitle::On {
        muted: false,
        counts: RingCounts::default(),
        ring: RingConfig {
            periods: 8,
            period_frames: 480,
        },
    };
    let title = compose(&f);
    assert!(
        title.contains("audio on (buffer 8x480 frames configured)"),
        "{title}"
    );
    assert!(
        !title.contains("latency") && !title.contains("ms"),
        "the depth must not be dressed up as a latency: {title}"
    );
}

/// Muted is a different word from off, because they are different situations: one card is open
/// and running, the other was never opened.
#[test]
fn muted_audio_is_not_the_same_as_audio_off() {
    let mut f = healthy(captured());
    f.audio = AudioTitle::On {
        muted: true,
        counts: RingCounts::default(),
        ring: RingConfig::default(),
    };
    let muted = compose(&f);
    f.audio = AudioTitle::Off;
    let off = compose(&f);
    assert!(muted.contains("audio muted"), "{muted}");
    assert!(!muted.contains("audio off"), "{muted}");
    assert!(off.contains("audio off"), "{off}");
    assert_ne!(muted, off);
}

/// Show nonzero failure counters without cluttering a healthy session.
#[test]
fn the_audio_counters_appear_only_once_something_has_been_dropped() {
    let mut f = healthy(captured());
    let quiet = compose(&f);
    assert!(!quiet.contains("overruns"), "{quiet}");

    f.audio = AudioTitle::On {
        muted: false,
        counts: RingCounts {
            overruns: 3,
            underruns: 1,
            drift_drops: 2,
            drift_inserts: 0,
            ..RingCounts::default()
        },
        ring: RingConfig::default(),
    };
    let title = compose(&f);
    assert!(
        title.contains("overruns 3, underruns 1, drift 2/0"),
        "{title}"
    );
}

/// The reason is the actionable part: "busy" is fixed by closing something, "gone" by plugging
/// something in. The title carries the condition's own wording rather than a generic failure.
#[test]
fn unavailable_audio_says_why_rather_than_only_that_it_failed() {
    let mut f = healthy(captured());
    f.audio = AudioTitle::Unavailable("capture hw:8 is busy: Device or resource busy");
    let title = compose(&f);
    assert!(
        title.contains("audio unavailable: capture hw:8 is busy: Device or resource busy"),
        "{title}"
    );
}

/// An unfinished open must say “opening”, then report a supervision timeout.
/// It must never appear as working audio.
#[test]
fn an_opening_side_says_so_rather_than_claiming_the_audio_is_on() {
    let mut f = healthy(captured());
    f.audio = AudioTitle::Opening("playback");
    let opening = compose(&f);
    assert!(
        opening.contains("audio opening"),
        "the title must say it is opening: {opening}"
    );
    assert!(
        opening.contains("playback"),
        "and which side, because the two are fixed in different places: {opening}"
    );
    assert!(
        !opening.contains("audio on"),
        "\"opening\" must not read as \"on\": unfinished opens need a distinct status: {opening}"
    );
    assert!(
        !opening.contains("audio off"),
        "nor as the deliberate --no-audio: {opening}"
    );
}

/// An audio failure and a video failure are different failures and must be legible together. A
/// title that let the audio segment displace the capture one would be hiding the more important
/// of the two.
#[test]
fn an_audio_failure_never_displaces_a_video_or_serial_one() {
    let mut f = healthy(captured());
    f.pipeline = PipelineState::Reconnecting;
    f.disconnected_for = Some(Duration::from_secs(5));
    f.link_down = true;
    f.link_down_for = Some(Duration::from_secs(5));
    f.audio = AudioTitle::Unavailable("capture hw:8 is gone: No such device");
    let title = compose(&f);
    assert!(
        title.contains("capture device gone, reconnecting (5s)"),
        "{title}"
    );
    assert!(title.contains("serial DOWN, reconnecting"), "{title}");
    assert!(title.contains("audio unavailable"), "{title}");
}
