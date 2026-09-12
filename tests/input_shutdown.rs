//! Shutdown closes admission before final release so no later input can be sent.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use nanokvm::input::testing::{
    fake_link, kb_frame, rel_frame, release_frames_rel, stall_writer, Behaviour,
};
use nanokvm::input::{
    spawn, Config, EngageError, Event, ReleaseOutcome, ReleaseReason, SubmitError,
};
use nanokvm::proto::cmd;
use nanokvm::proto::report::HidKey;

const WAIT: Duration = Duration::from_secs(10);
const A: u8 = 0x04;
const B: u8 = 0x05;

fn config() -> Config {
    Config {
        max_barriers: 4096,
        transact_timeout: Duration::from_millis(50),
        // Slice A added `Config::reconnect`; these paths use `spawn`, which never reconnects.
        ..Config::default()
    }
}

/// Concurrent recapture must not submit after shutdown closes admission and sends
/// its final release.
#[test]
fn a_recapture_loop_cannot_write_input_after_the_shutdown_release() {
    let mut leaked = 0u32;
    let mut first = None;
    for i in 0..3000u32 {
        let (link, ctl) = fake_link();
        let (producer, writer) = spawn(link, config());
        let p = producer.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = Arc::clone(&stop);
        let t = thread::spawn(move || {
            let mut down = true;
            while !s2.load(Ordering::Relaxed) {
                if p.submit(Event::Key {
                    key: HidKey::Usage(A),
                    down,
                })
                .is_err()
                {
                    // Exercise ordinary recapture concurrently with shutdown.
                    let _ = p.engage();
                }
                down = !down;
            }
        });
        ctl.wait_for_frames(60);
        let outcome = writer.shutdown();
        stop.store(true, Ordering::Relaxed);
        t.join().expect("producer thread");

        let frames = ctl.frames();
        let n = frames.len();
        let tail: Vec<_> = frames[n - 2..].to_vec();
        let last = &frames[n - 1];
        let key_still_down = last.cmd == cmd::SEND_KB_GENERAL_DATA && last.payload != [0u8; 8];
        if tail != release_frames_rel() {
            leaked += 1;
            if key_still_down && first.is_none() {
                first = Some((i, outcome, frames[n - 4..].to_vec()));
            }
        }
    }
    if let Some((i, outcome, tail)) = first {
        panic!(
            "{leaked}/3000 shutdowns wrote no final release-all; the first one that left a KEY \
             HELD was iteration {i}, shutdown reported {outcome:?}, last four frames {tail:?}"
        );
    }
    assert_eq!(
        leaked, 0,
        "{leaked}/3000 shutdowns wrote no final release-all"
    );
}

/// The control for the test above: the same shape with a producer that never re-engages. It passed
/// before the fix and must keep passing after it — if this one ever fails, the cause is not the
/// concurrent `engage`.
#[test]
fn a_shutdown_with_no_concurrent_recapture_still_ends_in_a_release() {
    let mut leaked = 0u32;
    for _ in 0..3000u32 {
        let (link, ctl) = fake_link();
        let (producer, writer) = spawn(link, config());
        let p = producer.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = Arc::clone(&stop);
        let t = thread::spawn(move || {
            let mut down = true;
            while !s2.load(Ordering::Relaxed) {
                // Paced so the queue never overflows: no cancellation, no engage, ever.
                if p.stats().queue_depth < 8 {
                    if p.submit(Event::Key {
                        key: HidKey::Usage(A),
                        down,
                    })
                    .is_err()
                    {
                        break;
                    }
                    down = !down;
                }
            }
        });
        ctl.wait_for_frames(60);
        let _ = writer.shutdown();
        stop.store(true, Ordering::Relaxed);
        t.join().expect("producer thread");
        let frames = ctl.frames();
        let n = frames.len();
        if frames[n - 2..] != release_frames_rel()[..] {
            leaked += 1;
        }
    }
    assert_eq!(leaked, 0, "{leaked}/3000 without re-engage");
}

/// Distinct session usages reveal any old-epoch input delivered after release.
#[test]
fn no_frame_from_a_cancelled_session_survives_its_release() {
    for _ in 0..200 {
        let (link, ctl) = fake_link();
        let (producer, writer) = spawn(link, config());
        let p = producer.clone();
        let go = Arc::new(AtomicBool::new(true));
        let g2 = Arc::clone(&go);
        let t = thread::spawn(move || {
            let mut down = true;
            while g2.load(Ordering::Relaxed) {
                // Paced, so the only refusal this thread can see is the release-all disengaging
                // it — an overflow here would end the session before the assertion had anything
                // to look at.
                if p.stats().queue_depth < 8 {
                    if p.submit(Event::Key {
                        key: HidKey::Usage(A),
                        down,
                    })
                    .is_err()
                    {
                        break;
                    }
                    down = !down;
                }
            }
        });
        ctl.wait_for_frames(4);
        producer.request_release_all(ReleaseReason::FocusLost);
        assert!(producer.wait_until_engageable(WAIT));
        go.store(false, Ordering::Relaxed);
        t.join().expect("producer thread");
        assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);

        let frames = ctl.frames();
        // The first release-all pair: a zeroed keyboard report followed by the mouse release.
        let rel_all = rel_frame(0, 0, 0, 0);
        let first = (0..frames.len().saturating_sub(1))
            .find(|&i| frames[i] == kb_frame(0, [0; 6]) && frames[i + 1] == rel_all)
            .expect("a release pair");
        for (i, f) in frames.iter().enumerate().skip(first + 2) {
            assert!(
                !(f.cmd == cmd::SEND_KB_GENERAL_DATA && f.payload[2] == A),
                "session-0 key frame at {i} written after the release at {first}"
            );
        }
    }
}

/// The gate itself: from the moment `shutdown` begins — while the writer is still parked inside a
/// transact and has not written the release yet — `engage` and `submit` both fail with
/// `ShuttingDown`.
#[test]
fn engage_and_submit_fail_with_shutting_down_once_shutdown_has_begun() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    let p = producer.clone();
    let releaser = ctl.clone();
    let checker = thread::spawn(move || {
        // `engage` succeeds while the session is live (the epoch gate is satisfied), so the first
        // refusal is the moment the shutdown became observable. Bounded spin, never a sleep.
        let deadline = Instant::now() + WAIT;
        loop {
            match p.engage() {
                Err(EngageError::ShuttingDown) => break,
                _ => assert!(
                    Instant::now() < deadline,
                    "the shutdown never became observable to a producer"
                ),
            }
            std::hint::spin_loop();
        }
        assert_eq!(
            p.submit(Event::Key {
                key: HidKey::Usage(A),
                down: true
            }),
            Err(SubmitError::ShuttingDown),
            "nothing may be accepted once the last release-all is owed"
        );
        // Let the parked writer finish, so `shutdown` can return.
        releaser.release_stalls();
    });

    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);
    checker.join().expect("checker thread");
}

/// Admission stays closed after the writer exits.
#[test]
fn engage_and_submit_keep_failing_after_shutdown_has_finished() {
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

    assert!(matches!(producer.engage(), Err(EngageError::ShuttingDown)));
    assert_eq!(
        producer.submit(Event::Key {
            key: HidKey::Usage(B),
            down: true
        }),
        Err(SubmitError::ShuttingDown)
    );
    assert!(!producer.is_engaged());

    let frames = ctl.frames();
    assert_eq!(
        &frames[frames.len() - 2..],
        release_frames_rel().as_slice(),
        "the release-all is the last thing on the link"
    );
}

/// `shutdown` reports the outcome of its own release, not of whatever release happened to be
/// recorded before it. Two sequences run here; the returned record is the second one's.
#[test]
fn shutdown_reports_the_outcome_of_its_own_release() {
    let (link, _ctl) = fake_link();
    let (producer, writer) = spawn(link, config());

    producer
        .submit(Event::Key {
            key: HidKey::Usage(A),
            down: true,
        })
        .expect("key");
    producer.request_release_all(ReleaseReason::FocusLost);
    assert!(producer.wait_until_engageable(WAIT));
    producer.engage().expect("deliberate recapture");
    let earlier = producer.stats().last_release.expect("the first release");
    assert_eq!(earlier.epoch, 1);
    assert_eq!(earlier.reason, ReleaseReason::FocusLost);

    producer
        .submit(Event::Key {
            key: HidKey::Usage(B),
            down: true,
        })
        .expect("key");
    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);

    let record = producer.stats().last_release.expect("the shutdown release");
    assert_eq!(record.epoch, 2, "the epoch this shutdown requested");
    assert_eq!(record.reason, ReleaseReason::Shutdown);
    assert_eq!(record.outcome, ReleaseOutcome::Submitted);
}

/// …including when the link dies underneath that release: `Unsent` means the frames never reached
/// the transport and the target may still be holding keys.
#[test]
fn shutdown_reports_unsent_when_the_link_dies_under_its_own_release() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());

    producer
        .submit(Event::Key {
            key: HidKey::Usage(A),
            down: true,
        })
        .expect("key");
    ctl.wait_for_frames(1);

    // The link is healthy right up to the shutdown, so this is the shutdown's own release failing.
    ctl.set_behaviour(Behaviour::Fail);
    assert_eq!(writer.shutdown(), ReleaseOutcome::Unsent);

    let stats = producer.stats();
    let record = stats.last_release.expect("the shutdown release");
    assert_eq!(record.epoch, 1);
    assert_eq!(record.reason, ReleaseReason::Shutdown);
    assert_eq!(record.outcome, ReleaseOutcome::Unsent);
    assert!(stats.link_down, "the failure is surfaced, not hidden");
    assert_eq!(stats.acked_epoch, stats.requested_epoch);
}

/// A handle dropped instead of shut down closes the path the same way: the release-all is still
/// requested and no producer can re-engage behind it. The drop does not join, so the rendezvous is
/// the recorded frame sequence.
#[test]
fn dropping_the_handle_closes_the_path_too() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());

    producer
        .submit(Event::Key {
            key: HidKey::Usage(A),
            down: true,
        })
        .expect("key");
    ctl.wait_for_frames(1);
    drop(writer);

    ctl.wait_for_frames(3);
    assert!(matches!(producer.engage(), Err(EngageError::ShuttingDown)));
    assert_eq!(
        producer.submit(Event::Key {
            key: HidKey::Usage(B),
            down: true
        }),
        Err(SubmitError::ShuttingDown)
    );
    let frames = ctl.frames();
    assert_eq!(&frames[1..3], release_frames_rel().as_slice());
}
