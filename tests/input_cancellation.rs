//! §9.2 item 4 — **cancellation synchronization** (§2.6). The race is the point.
//!
//! **No test in this file asserts that the device received anything** (§9.2 item 4, last bullet;
//! §2.6.1). Acknowledgment is a *local barrier*: an advanced `acked_epoch` means the writer
//! discarded all stale work and either handed the release to the transport or recorded it as
//! unsent. It never means the chip processed it or that the target's HID state changed. Every
//! assertion below is therefore about the order and content of the frames the writer *submitted*
//! to the link, and about the locally observable gating — never about delivery.
//!
//! Determinism comes from parking the writer inside a transact and releasing it explicitly. No
//! test sleeps.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use nanokvm::input::testing::{
    abs_frame, fake_link, gate_frame, kb_frame, rel_frame, release_frames_abs, release_frames_rel,
    stall_writer, wait_for_cancellations, Behaviour,
};
use nanokvm::input::{
    spawn, Config, EngageError, Event, ReleaseOutcome, ReleaseReason, SubmitError,
};
use nanokvm::proto::cmd;
use nanokvm::proto::report::{button, HidKey};

const WAIT: Duration = Duration::from_secs(10);
const A: u8 = 0x04;
const B: u8 = 0x05;
const C: u8 = 0x06;

fn config() -> Config {
    Config {
        max_barriers: 8,
        transact_timeout: Duration::from_millis(50),
    }
}

/// (a) The §2.6 race, in full: a key-down that was dequeued and passed its epoch check, then
/// stalled inside the link, while cancellation is triggered during the stall. The release must
/// land **after** that write, and no pre-cancellation event may be written after the release.
#[test]
fn a_stale_key_down_is_never_written_after_the_release() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());

    // The key-down itself is the stalled write: dequeued, epoch-checked, now inside the link.
    ctl.script([Behaviour::Stall]);
    producer
        .submit(Event::Key {
            key: HidKey::Usage(A),
            down: true,
        })
        .expect("key");
    ctl.wait_for_stalled(1);

    // Queued behind it, all still un-written.
    producer
        .submit(Event::Key {
            key: HidKey::Usage(B),
            down: true,
        })
        .expect("key");
    producer
        .submit(Event::Key {
            key: HidKey::Usage(C),
            down: true,
        })
        .expect("key");

    // Cancellation happens while the writer is inside the transact.
    producer.request_release_all(ReleaseReason::FocusLost);
    ctl.release_stalls();
    assert!(producer.wait_until_engageable(WAIT));

    let frames = ctl.frames();
    let mut expected = vec![kb_frame(0, [A, 0, 0, 0, 0, 0])];
    expected.extend(release_frames_rel());
    assert_eq!(frames, expected);

    // Stated the other way round, which is the property that matters: after the release-all,
    // nothing from before the cancellation was written.
    let release_at = frames
        .iter()
        .position(|f| *f == kb_frame(0, [0; 6]))
        .expect("release");
    assert!(frames[release_at..]
        .iter()
        .all(|f| !f.payload.contains(&B) && !f.payload.contains(&C)));
    let _ = writer.shutdown();
}

/// (b) A completely full queue still produces a release: the release is synthesized by the writer
/// and never enqueued, so the flag always has room (§2.6, §2.8).
#[test]
fn a_full_queue_still_produces_a_release() {
    let (link, ctl) = fake_link();
    let cfg = config();
    let (producer, writer) = spawn(link, cfg);
    stall_writer(&producer, &ctl);

    // Alternating button transitions cannot coalesce, so this fills the barrier bound exactly.
    for i in 0..cfg.max_barriers {
        producer
            .submit(Event::Button {
                button: button::LEFT,
                down: i % 2 == 0,
            })
            .expect("the queue must accept exactly max_barriers transitions");
    }
    assert_eq!(producer.stats().barrier_depth, cfg.max_barriers);

    producer.request_release_all(ReleaseReason::UserRequested);
    ctl.release_stalls();
    assert!(producer.wait_until_engageable(WAIT));

    let mut expected = vec![gate_frame()];
    expected.extend(release_frames_rel());
    assert_eq!(
        ctl.frames(),
        expected,
        "the full queue was discarded, the release still went out"
    );
    assert_eq!(producer.stats().cancellations, 1);
    let _ = writer.shutdown();
}

/// (c) Repeated and concurrent triggers coalesce into **one** sequence and one release (§2.6).
#[test]
fn fifty_concurrent_triggers_produce_one_release() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    let producer = Arc::new(producer);
    let mut threads = Vec::new();
    for _ in 0..50 {
        let p = Arc::clone(&producer);
        threads.push(thread::spawn(move || {
            p.request_release_all(ReleaseReason::UserRequested)
        }));
    }
    for t in threads {
        t.join().expect("trigger thread");
    }

    // Every trigger landed while the writer was parked, so all fifty coalesce into one sequence.
    ctl.release_stalls();
    assert!(producer.wait_until_engageable(WAIT));

    let mut expected = vec![gate_frame()];
    expected.extend(release_frames_rel());
    assert_eq!(ctl.frames(), expected);

    let stats = producer.stats();
    assert_eq!(stats.cancellations, 1, "one sequence");
    assert_eq!(stats.requested_epoch, 50);
    assert_eq!(
        stats.acked_epoch, 50,
        "one epoch advance, to the latched value"
    );
    assert_eq!(
        stats.last_release.map(|r| r.outcome),
        Some(ReleaseOutcome::Submitted)
    );
    let _ = writer.shutdown();
}

/// (d) A new session cannot submit until `engage` succeeds, and `engage` fails with
/// `NotYetAcked` while the writer is stalled before the ack (§2.6 "gating the next capture
/// session").
#[test]
fn a_new_session_waits_for_the_ack_before_it_may_submit() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    producer
        .submit(Event::Key {
            key: HidKey::Usage(A),
            down: true,
        })
        .expect("key");
    producer.request_release_all(ReleaseReason::CaptureReleased);

    assert!(!producer.is_engaged());
    assert_eq!(
        producer.submit(Event::Key {
            key: HidKey::Usage(B),
            down: true
        }),
        Err(SubmitError::Disengaged)
    );
    assert!(
        matches!(producer.engage(), Err(EngageError::NotYetAcked { .. })),
        "the writer is still stalled before the ack"
    );

    ctl.release_stalls();
    assert!(producer.wait_until_engageable(WAIT));

    producer.engage().expect("engage once the ack has landed");
    assert!(producer.is_engaged());
    let before = ctl.frame_count();
    producer
        .submit(Event::Key {
            key: HidKey::Usage(B),
            down: true,
        })
        .expect("key");
    ctl.wait_for_frames(before + 1);
    assert_eq!(ctl.frames()[before], kb_frame(0, [B, 0, 0, 0, 0, 0]));
    let _ = writer.shutdown();
}

/// (e) Transport down: the ack still advances, the outcome is `Unsent`, `link_down` is surfaced,
/// and input is not wedged (§2.6.1). A submission afterwards is **reported**, not silently
/// accepted: `SubmitError::LinkDown`.
#[test]
fn a_dead_transport_advances_the_ack_and_reports_unsent() {
    let (link, ctl) = fake_link();
    ctl.set_behaviour(Behaviour::Fail);
    let (producer, writer) = spawn(link, config());

    producer
        .submit(Event::Key {
            key: HidKey::Usage(A),
            down: true,
        })
        .expect("submission is local");
    // The writer itself triggers this cancellation, so there is no epoch bump to wait on.
    let release = wait_for_cancellations(&producer, 1);
    assert!(
        producer.wait_until_engageable(WAIT),
        "a dead cable must not wedge input"
    );

    let stats = producer.stats();
    assert!(stats.link_down, "the failure is surfaced, not hidden");
    assert_eq!(release.reason, ReleaseReason::LinkDown);
    assert_eq!(
        release.outcome,
        ReleaseOutcome::Unsent,
        "the target may still be holding keys"
    );
    assert_eq!(stats.acked_epoch, stats.requested_epoch);

    // Only the frame that failed was ever handed to the link.
    assert_eq!(ctl.frames(), vec![kb_frame(0, [A, 0, 0, 0, 0, 0])]);

    producer.engage().expect("the epoch gate is satisfied");
    assert_eq!(
        producer.submit(Event::Key {
            key: HidKey::Usage(B),
            down: true
        }),
        Err(SubmitError::LinkDown),
        "refused and reported rather than queued into a link that will never drain"
    );
    assert_eq!(writer.shutdown(), ReleaseOutcome::Unsent);
}

/// §9.2 item 4: **no frame is ever split by cancellation**. The `Link` seam makes framing atomic
/// by construction — one `transact` is one whole frame — so the property to check is that every
/// frame the writer submitted, across a cancellation storm, is a complete well-formed report of
/// the length its command implies (5 and 7 for the mouse, 8 for the keyboard).
#[test]
fn every_frame_on_the_link_is_complete() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());

    for round in 0..20u16 {
        let _ = producer.submit(Event::PointerAbs {
            x: round * 3,
            y: round,
        });
        let _ = producer.submit(Event::Key {
            key: HidKey::Usage(A),
            down: true,
        });
        let _ = producer.submit(Event::PointerRel { dx: 400, dy: -400 });
        let _ = producer.submit(Event::Button {
            button: button::LEFT,
            down: true,
        });
        producer.request_release_all(ReleaseReason::UserRequested);
        assert!(producer.wait_until_engageable(WAIT));
        producer.engage().expect("engage");
    }
    let outcome = writer.shutdown();
    assert_eq!(outcome, ReleaseOutcome::Submitted);

    for frame in ctl.frames() {
        let expected = match frame.cmd {
            cmd::SEND_KB_GENERAL_DATA => 8,
            cmd::SEND_MS_ABS_DATA => 7,
            cmd::SEND_MS_REL_DATA => 5,
            other => panic!("unexpected command byte {other:#04x} on the link"),
        };
        assert_eq!(frame.payload.len(), expected, "truncated frame {frame:?}");
    }
}

/// In `Abs` mode the release also puts the absolute device's buttons up, at the last known
/// position — the chip presents the two mice as separate HID devices (§2.6).
#[test]
fn a_release_in_abs_mode_releases_the_absolute_device_too() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    producer
        .submit(Event::PointerAbs { x: 1234, y: 567 })
        .expect("motion");
    producer
        .submit(Event::Button {
            button: button::LEFT,
            down: true,
        })
        .expect("press");
    ctl.release_stalls();
    ctl.wait_for_frames(3);

    producer.request_release_all(ReleaseReason::UserRequested);
    assert!(producer.wait_until_engageable(WAIT));

    let mut expected = vec![
        gate_frame(),
        abs_frame(0, 1234, 567, 0),
        abs_frame(button::LEFT, 1234, 567, 0),
    ];
    expected.extend(release_frames_abs(1234, 567));
    assert_eq!(ctl.frames(), expected);
    let _ = writer.shutdown();
}

/// Clean shutdown is a §2.6 trigger: the handle waits for the release to finish and reports its
/// outcome.
#[test]
fn shutdown_releases_everything_and_reports_the_outcome() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());

    producer
        .submit(Event::Key {
            key: HidKey::Usage(A),
            down: true,
        })
        .expect("key");
    ctl.wait_for_frames(1);

    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);
    let frames = ctl.frames();
    assert_eq!(&frames[frames.len() - 2..], release_frames_rel().as_slice());
    assert_eq!(frames[frames.len() - 1], rel_frame(0, 0, 0, 0));
}

// ---------------------------------------------------------------------------------------------
// §2.6 "the writer checks the flag *between* frames" — and the frame is one `transact`, not one
// queue entry. The three tests below are the regression suite for the adversarial review's MAJOR
// finding: `flush_motion` wrote every split report of one coalesced run in a loop without
// re-reading the flag, so a release-all triggered mid-run waited for the whole run — hundreds of
// reports, seconds of a live console with a key or button still held.

/// One entry, one enormous accumulated delta. The release-all must land at the first frame
/// boundary after the trigger, not after the run.
#[test]
fn a_release_all_is_not_delayed_behind_a_split_motion_run() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());

    // Park the writer inside frame 0 (the gate), and arm a second stall for the run's first
    // split report.
    ctl.script([Behaviour::Stall, Behaviour::Stall]);
    producer
        .submit(Event::PointerRel { dx: 1, dy: 0 })
        .expect("gate");
    ctl.wait_for_stalled(1);

    // One entry, one accumulated delta. Uncapped this was 788 reports.
    producer
        .submit(Event::PointerRel { dx: 100_000, dy: 0 })
        .expect("motion always fits");

    ctl.release_stalls(); // the gate returns; the writer pops the motion run
    ctl.wait_for_frames(2);
    ctl.wait_for_stalled(1); // parked inside the FIRST split report of that run

    // Focus loss, mid-run.
    producer.request_release_all(ReleaseReason::FocusLost);
    ctl.release_stalls();
    assert!(producer.wait_until_engageable(WAIT));

    let frames = ctl.frames();
    let first_release = frames
        .iter()
        .position(|f| *f == kb_frame(0, [0; 6]))
        .expect("a release-all must be written");
    assert_eq!(
        first_release, 2,
        "the gate, the one report already in flight, then the release: {frames:?}"
    );
    let _ = writer.shutdown();
}

/// The realistic shape of the same defect: no single huge delta, just an ordinary 1 kHz pointer
/// moving at ordinary speed while the writer is behind by one stalled transact. Uncapped and
/// unchecked this delayed the release by 126 reports, about 1.4 s at the measured 91 relative
/// reports per second (§5.1).
#[test]
fn a_realistic_motion_flood_does_not_delay_the_release_all() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());

    ctl.script([Behaviour::Stall, Behaviour::Stall]);
    producer
        .submit(Event::PointerRel { dx: 1, dy: 0 })
        .expect("gate");
    ctl.wait_for_stalled(1);

    // 2000 events at 8 px each: two seconds of a 1 kHz mouse while the writer is stalled.
    for _ in 0..2000 {
        producer
            .submit(Event::PointerRel { dx: 8, dy: 4 })
            .expect("motion always fits");
    }
    ctl.release_stalls();
    ctl.wait_for_frames(2);
    ctl.wait_for_stalled(1);

    producer.request_release_all(ReleaseReason::FocusLost);
    ctl.release_stalls();
    assert!(producer.wait_until_engageable(WAIT));

    let frames = ctl.frames();
    let first_release = frames
        .iter()
        .position(|f| *f == kb_frame(0, [0; 6]))
        .expect("a release-all must be written");
    assert_eq!(first_release, 2, "release-all delayed: {frames:?}");
    let _ = writer.shutdown();
}

/// Ten thousand relative events behind a stalled writer — an accumulation far past anything the
/// cap allows — and the release-all still costs at most the **one** report already in flight.
#[test]
fn a_ten_thousand_report_run_yields_the_release_all_within_one_report() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());

    ctl.script([Behaviour::Stall, Behaviour::Stall]);
    producer
        .submit(Event::PointerRel { dx: 1, dy: 0 })
        .expect("gate");
    ctl.wait_for_stalled(1);

    // 10 000 events of a full report's worth each: 1.27 million accumulated pixels, which is
    // ten thousand reports before the cap and thirty-three after it.
    for _ in 0..10_000 {
        producer
            .submit(Event::PointerRel { dx: 127, dy: 127 })
            .expect("motion always fits");
    }
    ctl.release_stalls();
    ctl.wait_for_frames(2);
    ctl.wait_for_stalled(1);

    producer.request_release_all(ReleaseReason::FocusLost);
    ctl.release_stalls();
    assert!(producer.wait_until_engageable(WAIT));

    let frames = ctl.frames();
    let first_release = frames
        .iter()
        .position(|f| *f == kb_frame(0, [0; 6]))
        .expect("a release-all must be written");
    let after_trigger = first_release - 1;
    assert!(
        after_trigger <= 1,
        "{after_trigger} mouse reports were written between the trigger and the release-all"
    );
    let _ = writer.shutdown();
}

/// The MINOR finding: the pointer mode survived the release-all, so a new session whose first
/// event was a `Button` with no preceding motion emitted an **absolute** report at the *previous*
/// session's coordinates — a click at a stale position on a live console. With the mode reset,
/// the click goes out as a zero-motion relative report, which lands wherever the pointer is.
#[test]
fn a_button_before_any_motion_in_a_new_session_is_not_a_click_at_the_old_position() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());

    producer
        .submit(Event::PointerAbs { x: 1234, y: 567 })
        .expect("motion");
    producer
        .submit(Event::Button {
            button: button::LEFT,
            down: true,
        })
        .expect("press");
    ctl.wait_for_frames(2);

    producer.request_release_all(ReleaseReason::CaptureReleased);
    assert!(producer.wait_until_engageable(WAIT));
    // The release itself still addresses the absolute device: that is where the buttons are held.
    let frames = ctl.frames();
    assert_eq!(
        &frames[frames.len() - 3..],
        release_frames_abs(1234, 567).as_slice()
    );

    producer.engage().expect("deliberate recapture");
    let before = ctl.frame_count();
    producer
        .submit(Event::Button {
            button: button::LEFT,
            down: true,
        })
        .expect("press");
    ctl.wait_for_frames(before + 1);

    assert_eq!(
        ctl.frames()[before],
        rel_frame(button::LEFT, 0, 0, 0),
        "a click with no known position must not be aimed at the last session's coordinates"
    );
    let _ = writer.shutdown();
}
