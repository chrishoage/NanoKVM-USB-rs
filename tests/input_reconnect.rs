//! Reconnect commissioning and cancellation using controlled in-memory link sources.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nanokvm::input::testing::{
    commission_calls, fake_source, force_enqueue_key, kb_frame, release_frames_rel,
    wait_for_cancellations, wait_for_reconnects, Behaviour, FakeCall, FakeControl,
    FakeSourceControl, RecordedFrame,
};
use nanokvm::input::{
    spawn_with_source, Config, Event, Producer, ReconnectConfig, ReleaseOutcome, ReleaseReason,
    Stats, SubmitError, WriterHandle,
};
use nanokvm::proto::cmd;
use nanokvm::proto::report::HidKey;

const WAIT: Duration = Duration::from_secs(10);
const A: u8 = 0x04;
const B: u8 = 0x05;

/// Backoffs in milliseconds, so a test that must observe several of them finishes in a blink.
/// 1 ms is the floor the writer applies anyway.
fn config() -> Config {
    Config {
        max_barriers: 8,
        transact_timeout: Duration::from_millis(50),
        reconnect: ReconnectConfig {
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(8),
            get_info_timeout: Duration::from_millis(50),
        },
    }
}

/// Poll `done` until it holds or the deadline passes. For the two facts that have no rendezvous of
/// their own — an epoch bump, and a writer thread having exited. Fails the test at the deadline
/// rather than hanging.
fn settle<F: Fn() -> bool>(what: &str, done: F) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// A source with `n` links queued and nothing after them, and a started writer whose first link is
/// already commissioned. Returns the controls in the order the writer will open them.
fn rig(n: usize) -> (Producer, WriterHandle, FakeSourceControl, Vec<FakeControl>) {
    let (source, sctl) = fake_source();
    let links: Vec<FakeControl> = (0..n).map(|_| sctl.push_link()).collect();
    let (producer, writer) = spawn_with_source(Box::new(source), config());
    producer
        .wait_for_link(WAIT)
        .expect("the first link comes up");
    (producer, writer, sctl, links)
}

/// The `GET_INFO` frame, built the way the writer builds it.
fn get_info_frame() -> RecordedFrame {
    RecordedFrame {
        cmd: cmd::GET_INFO,
        payload: Vec::new(),
    }
}

/// Kill the link the writer is holding by making its next transact a transport failure, then make
/// the writer touch it. Returns once the failure has been noticed.
fn kill(producer: &Producer, link: &FakeControl, before: u64) {
    link.set_behaviour(Behaviour::Fail);
    producer.engage().expect("the epoch gate is satisfied");
    producer
        .submit(Event::Key {
            key: HidKey::Usage(A),
            down: true,
        })
        .expect("submission is local");
    // The writer triggers this cancellation itself, so there is no epoch bump to wait on.
    let _ = wait_for_cancellations(producer, before + 1);
}

// -- the initial link -------------------------------------------------------

/// Commissioning keeps the link down until device information is validated.
#[test]
fn the_first_link_is_commissioned_with_the_whole_sequence() {
    let (producer, writer, sctl, links) = rig(1);

    assert_eq!(links[0].calls(), commission_calls());

    let stats = producer.stats();
    assert!(!stats.link_down, "the GET_INFO answered, so the link is up");
    assert_eq!(sctl.attempts(), 1);
    assert_eq!(stats.reconnect_attempts, 1);
    assert_eq!(
        stats.reconnects, 0,
        "the first link replaced nothing, so it is not a reconnect"
    );
    assert!(stats.down_since.is_none());
    let info = stats.device_info.expect("GET_INFO answered");
    assert!((info.version - 1.8).abs() < 0.05);

    let release = stats.last_release.expect("the sequence is a cancellation");
    assert_eq!(release.reason, ReleaseReason::Reconnected);
    assert_eq!(release.outcome, ReleaseOutcome::Submitted);
    assert!(
        !producer.is_engaged(),
        "input resumes on deliberate recapture, not on a link coming up"
    );
    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);
}

/// Startup with no device at all is a reported condition rather than a panic, and the writer keeps
/// trying rather than giving up.
#[test]
fn a_startup_with_no_device_reports_it_and_keeps_trying() {
    let (source, sctl) = fake_source();
    let (producer, writer) = spawn_with_source(Box::new(source), config());

    assert_eq!(
        producer.wait_for_link(Duration::from_millis(50)),
        Err(SubmitError::LinkDown)
    );
    let stats = producer.stats();
    assert!(stats.link_down);
    assert!(stats.device_info.is_none());
    assert!(
        stats.down_since.is_some(),
        "the outage is timed from startup"
    );
    assert_eq!(
        producer.submit(Event::Key {
            key: HidKey::Usage(A),
            down: true
        }),
        Err(SubmitError::LinkDown),
        "refused and reported rather than queued into a link that does not exist"
    );

    sctl.wait_for_attempts(3);
    assert_eq!(writer.shutdown(), ReleaseOutcome::Unsent);
}

// -- a failure, and the link that replaces it -------------------------------

/// Acknowledge the failed release before attempting a replacement.
#[test]
fn a_transport_failure_is_released_unsent_before_anything_is_reopened() {
    // One link and then nothing: the writer will keep failing to reopen, which is what leaves the
    // `LinkDown` release as the most recent one for the assertions below.
    let (producer, writer, sctl, links) = rig(1);
    kill(&producer, &links[0], 1);

    let stats = producer.stats();
    let release = stats.last_release.expect("a release was recorded");
    assert_eq!(release.reason, ReleaseReason::LinkDown);
    assert_eq!(
        release.outcome,
        ReleaseOutcome::Unsent,
        "the target may still be holding keys"
    );
    assert!(stats.link_down);
    assert!(stats.down_since.is_some(), "the outage is being timed");
    assert_eq!(
        stats.acked_epoch, stats.requested_epoch,
        "a dead cable must not wedge input"
    );
    assert_eq!(stats.reconnects, 0);

    // Only the frame that failed was ever handed to the dead link, after its commissioning.
    let mut expected = commission_calls();
    expected.push(FakeCall::Frame(kb_frame(0, [A, 0, 0, 0, 0, 0])));
    assert_eq!(links[0].calls(), expected);

    sctl.wait_for_attempts(2);
    assert_eq!(writer.shutdown(), ReleaseOutcome::Unsent);
}

/// A new link receives only commissioning reports before recapture.
#[test]
fn the_replacement_link_sees_exactly_the_reconnect_sequence() {
    let (producer, writer, _sctl, links) = rig(2);
    kill(&producer, &links[0], 1);
    wait_for_reconnects(&producer, 1);

    assert_eq!(links[1].calls(), commission_calls());
    assert_eq!(
        links[1].calls().first(),
        Some(&FakeCall::Resync),
        "the preamble goes first, or the release-all is eaten as the remainder of the torn \
         frame it was meant to repair"
    );

    let stats = producer.stats();
    assert!(!stats.link_down);
    assert!(stats.down_since.is_none(), "the outage ended");
    assert_eq!(stats.reconnects, 1);
    assert_eq!(
        stats.reconnect_attempts, 2,
        "the initial open, then this one"
    );
    assert_eq!(stats.device_info.map(|i| i.target_connected), Some(true));
    let release = stats.last_release.expect("the sequence is a cancellation");
    assert_eq!(release.reason, ReleaseReason::Reconnected);
    assert_eq!(release.outcome, ReleaseOutcome::Submitted);
    assert!(
        !producer.is_engaged(),
        "the producer stays disengaged until the user re-grabs"
    );
    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);
}

/// Idle loss must be detected without writing to the dead link.
#[test]
fn an_idle_writer_discovers_a_hung_up_link_without_writing_to_it() {
    let (producer, writer, _sctl, links) = rig(2);
    let wrote_before = links[0].frame_count();
    let calls_before = links[0].calls().len();

    // Nothing is submitted, and nothing is triggered: the far end simply goes away.
    links[0].hang_up();
    wait_for_reconnects(&producer, 1);

    assert_eq!(
        links[0].frame_count(),
        wrote_before,
        "the writer must not have written to the dead link to discover it was dead"
    );
    assert_eq!(
        links[0].calls().len(),
        calls_before,
        "not a transact and not a preamble: the health query is the whole mechanism"
    );
    assert_eq!(
        links[0].refused(),
        0,
        "and the two assertions above mean it: a call the hung-up link refused is recorded as \
         `FakeCall::Refused`, so a writer that probed the corpse would move all three counts \
         rather than none of them"
    );
    assert_eq!(
        links[1].calls(),
        commission_calls(),
        "the replacement goes through the same sequence a failed write would have provoked"
    );

    let stats = producer.stats();
    assert!(!stats.link_down, "the outage ended with the new link");
    assert_eq!(stats.reconnects, 1);
    let release = stats.last_release.expect("the sequence is a cancellation");
    assert_eq!(release.reason, ReleaseReason::Reconnected);
    assert_eq!(release.outcome, ReleaseOutcome::Submitted);
    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);
}

/// Queued or partially consumed old-session input must never reach the replacement.
#[test]
fn nothing_from_before_the_failure_is_replayed_on_the_new_link() {
    let (producer, writer, _sctl, links) = rig(2);
    producer.engage().expect("the epoch gate is satisfied");

    // One key parked inside the dead link's transact, two more queued behind it.
    links[0].script([Behaviour::Stall]);
    for key in [A, B, 0x06] {
        producer
            .submit(Event::Key {
                key: HidKey::Usage(key),
                down: true,
            })
            .expect("submission is local");
    }
    links[0].wait_for_stalled(1);
    // The stalled call returns the *default*, so this is the transport dying under it.
    links[0].set_behaviour(Behaviour::Fail);
    links[0].release_stalls();

    wait_for_reconnects(&producer, 1);
    assert_eq!(
        links[1].calls(),
        commission_calls(),
        "a key queued before the cable went must never reach the target three seconds later"
    );
    assert_eq!(
        producer.stats().queue_discarded_on_reconnect,
        0,
        "the LinkDown cancellation already drained them; this drain is belt and braces"
    );
    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);
}

/// A producer that re-engaged while the link was down cannot get anything onto the new link
/// either: `submit` refuses while the transport is gone, and the reconnect raises its own epoch,
/// so there is no window in which a stale session's event is queued and then written.
#[test]
fn a_producer_that_re_engaged_while_down_reaches_the_new_link_with_nothing() {
    let (producer, writer, _sctl, links) = rig(2);
    kill(&producer, &links[0], 1);

    // `engage` checks only the ack, which the LinkDown release advanced — so this succeeds.
    producer
        .engage()
        .expect(" advanced the ack, so re-engagement is allowed");
    assert_eq!(
        producer.submit(Event::Key {
            key: HidKey::Usage(B),
            down: true
        }),
        Err(SubmitError::LinkDown),
        "…but nothing may be queued into a transport that is gone"
    );

    wait_for_reconnects(&producer, 1);
    assert_eq!(links[1].calls(), commission_calls());
    assert!(!producer.is_engaged(), "the reconnect disengaged it again");
    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);
}

/// And once the user does re-grab, input flows on the new link.
#[test]
fn input_reaches_the_new_link_after_the_user_re_engages() {
    let (producer, writer, _sctl, links) = rig(2);
    kill(&producer, &links[0], 1);
    wait_for_reconnects(&producer, 1);

    producer.engage().expect("deliberate recapture");
    producer
        .submit(Event::Key {
            key: HidKey::Usage(B),
            down: true,
        })
        .expect("the link is up again");
    // `frames` counts transacts, and the preamble is not one.
    let commissioned = commission_calls()
        .iter()
        .filter(|c| matches!(c, FakeCall::Frame(_)))
        .count();
    links[1].wait_for_frames(commissioned + 1);

    let mut expected = commission_calls();
    expected.push(FakeCall::Frame(kb_frame(0, [B, 0, 0, 0, 0, 0])));
    assert_eq!(links[1].calls(), expected);
    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);
}

// -- rejecting a link that opened but is not a CH9329 -----------------------

/// A link that cannot answer GET_INFO is rejected and retried.
#[test]
fn a_link_whose_get_info_times_out_is_not_accepted() {
    let (producer, writer, _sctl, links) = rig(3);
    // The second link answers the two release frames and then goes silent.
    links[1].script([Behaviour::Ack, Behaviour::Ack, Behaviour::Timeout]);
    kill(&producer, &links[0], 1);
    wait_for_reconnects(&producer, 1);

    assert_eq!(
        links[1].calls(),
        commission_calls(),
        "the whole sequence was attempted on it, up to and including the proof"
    );
    assert_eq!(links[2].calls(), commission_calls());
    let stats = producer.stats();
    assert_eq!(stats.reconnects, 1, "only the link that answered counts");
    assert_eq!(
        stats.reconnect_attempts, 3,
        "the initial link, the silent one, and the one that answered"
    );
    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);
}

/// A short information reply must also reject commissioning.
#[test]
fn a_link_whose_get_info_reply_is_too_short_is_not_accepted() {
    let (producer, writer, _sctl, links) = rig(3);
    links[1].set_get_info_payload(&[0x38]);
    kill(&producer, &links[0], 1);
    wait_for_reconnects(&producer, 1);

    assert_eq!(links[1].calls(), commission_calls());
    assert_eq!(producer.stats().reconnect_attempts, 3);
    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);
}

/// A link whose port dies before the preamble is rejected at step 3a, with nothing else attempted
/// on it — the release-all must not go out into a parser that is still mid-frame.
#[test]
fn a_link_that_cannot_take_the_preamble_is_rejected_before_anything_is_written() {
    let (producer, writer, _sctl, links) = rig(3);
    links[1].fail_resync(true);
    kill(&producer, &links[0], 1);
    wait_for_reconnects(&producer, 1);

    assert_eq!(
        links[1].calls(),
        vec![FakeCall::Resync],
        "the sequence stopped at the preamble"
    );
    assert_eq!(links[2].calls(), commission_calls());
    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);
}

// -- the backoff ------------------------------------------------------------

/// The retry interval doubles up to the cap, and the cap holds. Asserted on ordering and bounds
/// rather than on exact timing: the lower bounds are sound because the wait is a `Condvar`
/// deadline, which never returns early.
#[test]
fn the_backoff_doubles_and_then_holds_at_the_cap() {
    let (source, sctl) = fake_source();
    let cfg = config();
    let (producer, writer) = spawn_with_source(Box::new(source), cfg);

    sctl.wait_for_attempts(6);
    let times = sctl.attempt_times();
    let gaps: Vec<Duration> = times.windows(2).map(|w| w[1] - w[0]).collect();

    let initial = cfg.reconnect.initial_backoff;
    let max = cfg.reconnect.max_backoff;
    // 1 ms, 2 ms, 4 ms, 8 ms, 8 ms… — each gap is at least the backoff that preceded it.
    let mut expected = initial;
    for (i, gap) in gaps.iter().enumerate() {
        assert!(
            *gap >= expected,
            "gap {i} was {gap:?}, below the {expected:?} backoff that should precede it: {gaps:?}"
        );
        expected = (expected * 2).min(max);
    }
    // And the cap holds: no gap is anywhere near a doubling past it. The slack is generous
    // because a loaded machine can delay a wake-up, and this test is about the cap, not the clock.
    for (i, gap) in gaps.iter().enumerate() {
        assert!(
            *gap < max + Duration::from_secs(2),
            "gap {i} was {gap:?}, past the {max:?} cap by more than scheduling can explain"
        );
    }
    assert!(producer.stats().link_down);
    assert_eq!(writer.shutdown(), ReleaseOutcome::Unsent);
}

/// A shutdown during a backoff returns at once rather than after it, and reports the release it
/// could not send.
#[test]
fn a_shutdown_during_the_backoff_returns_immediately() {
    let (source, sctl) = fake_source();
    let (producer, writer) = spawn_with_source(
        Box::new(source),
        Config {
            reconnect: ReconnectConfig {
                // Long enough that a writer which slept it out would fail this test by a mile.
                initial_backoff: Duration::from_secs(30),
                max_backoff: Duration::from_secs(30),
                ..ReconnectConfig::default()
            },
            ..config()
        },
    );
    // The first attempt goes out immediately; the writer is now parked in the 30 s backoff.
    sctl.wait_for_attempts(1);
    settle("the writer to park in the backoff", || {
        producer.stats().reconnect_attempts >= 1
    });

    let started = Instant::now();
    let outcome = writer.shutdown();
    let elapsed = started.elapsed();

    assert_eq!(
        outcome,
        ReleaseOutcome::Unsent,
        "there was no transport to release on, and that is reported rather than hidden"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "shutdown waited out the backoff: {elapsed:?}"
    );
}

/// Cancellation during backoff must advance the epoch even if reconnect never succeeds.
#[test]
fn a_trigger_during_the_backoff_is_acknowledged_without_waiting_for_a_link() {
    let (source, sctl) = fake_source();
    let (producer, writer) = spawn_with_source(
        Box::new(source),
        Config {
            reconnect: ReconnectConfig {
                initial_backoff: Duration::from_secs(30),
                max_backoff: Duration::from_secs(30),
                ..ReconnectConfig::default()
            },
            ..config()
        },
    );
    sctl.wait_for_attempts(1);

    producer.request_release_all(ReleaseReason::FocusLost);
    assert!(
        producer.wait_until_engageable(WAIT),
        "the release must be acknowledged while the link is still down"
    );
    let release = producer
        .stats()
        .last_release
        .expect("a release was recorded");
    assert_eq!(release.reason, ReleaseReason::FocusLost);
    assert_eq!(release.outcome, ReleaseOutcome::Unsent);
    producer.engage().expect("the epoch gate is satisfied");
    assert_eq!(writer.shutdown(), ReleaseOutcome::Unsent);
}

// -- a shutdown landing inside the sequence ---------------------------------

/// A shutdown that arrives between steps 3a and 3e does not truncate the sequence: it completes,
/// and the shutdown then gets its own cancellation on the link that sequence just
/// commissioned — so its release is `Submitted`, not `Unsent`.
#[test]
fn a_shutdown_inside_the_reconnect_sequence_still_releases_on_the_new_link() {
    let (producer, writer, _sctl, links) = rig(2);
    // Park the writer inside step 3c, the keyboard release-all on the new link.
    links[1].script([Behaviour::Stall]);
    kill(&producer, &links[0], 1);
    links[1].wait_for_stalled(1);

    let before = producer.stats().requested_epoch;
    let outcome = std::thread::scope(|s| {
        let joiner = s.spawn(|| writer.shutdown());
        // The shutdown's trigger has landed once the epoch moves; only then is the stall released,
        // which is what puts the shutdown strictly inside the sequence.
        settle("the shutdown trigger", || {
            producer.stats().requested_epoch > before
        });
        links[1].release_stalls();
        joiner.join().expect("the shutdown thread must not panic")
    });

    assert_eq!(
        outcome,
        ReleaseOutcome::Submitted,
        "the shutdown release went out on the link the reconnect had just commissioned"
    );
    let calls = links[1].calls();
    let mut expected = commission_calls();
    expected.extend(
        nanokvm::input::testing::release_frames_rel()
            .into_iter()
            .map(FakeCall::Frame),
    );
    assert_eq!(calls, expected);
    assert_eq!(
        calls.last(),
        Some(&FakeCall::Frame(nanokvm::input::testing::rel_frame(
            0, 0, 0, 0
        ))),
        "the last thing on the wire is a release, not a GET_INFO"
    );
    assert_ne!(calls.last(), Some(&FakeCall::Frame(get_info_frame())));
}

// -- entry point is unchanged -------------------------------------

/// `spawn` never reconnects: after a failure the link stays down for good and no attempt is made.
#[test]
fn spawn_never_reconnects() {
    use nanokvm::input::spawn;
    use nanokvm::input::testing::fake_link;

    let (link, ctl) = fake_link();
    ctl.set_behaviour(Behaviour::Fail);
    let (producer, writer) = spawn(link, config());
    producer
        .submit(Event::Key {
            key: HidKey::Usage(A),
            down: true,
        })
        .expect("submission is local");
    let release = wait_for_cancellations(&producer, 1);

    assert_eq!(release.reason, ReleaseReason::LinkDown);
    let stats = producer.stats();
    assert!(stats.link_down, "and it stays down");
    assert_eq!(
        stats.reconnect_attempts, 0,
        "The fixed-link entry point does not look for another link"
    );
    assert_eq!(stats.reconnects, 0);
    assert!(stats.device_info.is_none(), "`spawn` never sends GET_INFO");
    assert_eq!(ctl.resyncs(), 0);
    assert_eq!(writer.shutdown(), ReleaseOutcome::Unsent);
}

// -- the snapshot tells a consistent story ----------------------------------

/// The invariant the viewer reads: a link that is up was commissioned, and being commissioned is
/// what the last release says. Checked across a failure and its repair rather than at one instant.
#[test]
fn a_link_that_is_up_was_commissioned_by_the_release_the_snapshot_reports() {
    fn consistent(stats: &Stats) {
        if stats.link_down {
            assert!(stats.down_since.is_some(), "an outage is always timed");
            return;
        }
        assert!(stats.down_since.is_none());
        assert!(
            stats.device_info.is_some(),
            "a link is only up once its GET_INFO answered"
        );
        let release = stats.last_release.expect("commissioning records a release");
        assert_eq!(release.reason, ReleaseReason::Reconnected);
        assert_eq!(release.outcome, ReleaseOutcome::Submitted);
        assert!(stats.reconnect_attempts >= stats.reconnects);
    }

    let (producer, writer, _sctl, links) = rig(2);
    consistent(&producer.stats());
    kill(&producer, &links[0], 1);
    consistent(&producer.stats());
    wait_for_reconnects(&producer, 1);
    consistent(&producer.stats());
    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);
}

/// Two failures in a row are two reconnects, and the counters do not drift.
#[test]
fn a_second_failure_reconnects_again() {
    let (producer, writer, _sctl, links) = rig(3);
    kill(&producer, &links[0], 1);
    wait_for_reconnects(&producer, 1);
    kill(&producer, &links[1], 3);
    wait_for_reconnects(&producer, 2);

    let stats = producer.stats();
    assert_eq!(stats.reconnects, 2);
    assert_eq!(stats.reconnect_attempts, 3);
    assert!(!stats.link_down);
    assert_eq!(links[2].calls(), commission_calls());
    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);
}

// -- the field path: opens that fail before one works ------------------------

/// Commissioning transactions excluding the raw preamble.
fn commission_frames() -> usize {
    commission_calls()
        .iter()
        .filter(|c| matches!(c, FakeCall::Frame(_)))
        .count()
}

/// The reconnect that happens in the field: the cable comes back a few attempts after
/// it went. Every refusal is counted, the link that finally opens is commissioned rather than
/// discarded because earlier attempts failed, and input flows on it once the user re-grabs.
#[test]
fn a_link_that_arrives_after_several_failed_opens_is_commissioned() {
    let (source, sctl) = fake_source();
    let first = sctl.push_link();
    sctl.push_failures(3);
    let eventual = sctl.push_link();
    let (producer, writer) = spawn_with_source(Box::new(source), config());
    producer
        .wait_for_link(WAIT)
        .expect("the first link comes up");

    kill(&producer, &first, 1);
    wait_for_reconnects(&producer, 1);

    assert_eq!(
        eventual.calls(),
        commission_calls(),
        "the link that finally opened saw the whole  sequence and nothing else"
    );
    let stats = producer.stats();
    assert_eq!(
        stats.reconnect_attempts, 5,
        "the initial open, three refusals, and the one that worked"
    );
    assert_eq!(stats.reconnects, 1);
    assert!(!stats.link_down, "the device came back and was picked up");
    assert!(stats.down_since.is_none(), "the outage ended");
    let release = stats.last_release.expect("commissioning records a release");
    assert_eq!(release.reason, ReleaseReason::Reconnected);
    assert_eq!(release.outcome, ReleaseOutcome::Submitted);

    // And it is a working link, not merely an accepted one.
    producer.engage().expect("deliberate recapture");
    producer
        .submit(Event::Key {
            key: HidKey::Usage(B),
            down: true,
        })
        .expect("the link is up again");
    eventual.wait_for_frames(commission_frames() + 1);
    let mut expected = commission_calls();
    expected.push(FakeCall::Frame(kb_frame(0, [B, 0, 0, 0, 0, 0])));
    assert_eq!(eventual.calls(), expected);
    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);
}

/// The same at startup: a device that is not enumerated yet is waited for, and the link that turns
/// up on the third attempt is the first link — commissioned in full, and not counted as a
/// *re*-connect however many attempts it took to get it.
#[test]
fn an_initial_link_that_is_not_there_yet_is_picked_up_when_it_appears() {
    let (source, sctl) = fake_source();
    sctl.push_failures(2);
    let eventual = sctl.push_link();
    let (producer, writer) = spawn_with_source(Box::new(source), config());

    let info = producer
        .wait_for_link(WAIT)
        .expect("the third attempt finds the device");
    assert!((info.version - 1.8).abs() < 0.05);
    assert_eq!(eventual.calls(), commission_calls());

    let stats = producer.stats();
    assert_eq!(stats.reconnect_attempts, 3, "two refusals and the open");
    assert_eq!(
        stats.reconnects, 0,
        "the first link ever replaced nothing, however many attempts it took"
    );
    assert!(!stats.link_down);
    assert_eq!(stats.device_info.map(|i| i.target_connected), Some(true));
    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);
}

// -- a shutdown while a port is opening --------------------------------------

/// Run `shutdown` on its own thread and report whether it came back within `bound`, without
/// failing there and then: the caller must be able to let the writer out of whatever it is parked
/// in before it asserts, or a regression hangs the suite instead of failing it.
fn shutdown_within(
    writer: WriterHandle,
    bound: Duration,
) -> (bool, std::thread::JoinHandle<ReleaseOutcome>) {
    let done = Arc::new(AtomicBool::new(false));
    let joiner = {
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let outcome = writer.shutdown();
            done.store(true, Ordering::SeqCst);
            outcome
        })
    };
    let deadline = Instant::now() + bound;
    while Instant::now() < deadline && !done.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(1));
    }
    (done.load(Ordering::SeqCst), joiner)
}

/// `WriterHandle::shutdown` joins the writer, and the writer cannot interrupt `LinkSource::open`.
/// Opening a real port is `open(2)` plus a `tcsetattr` against a device that has just
/// re-enumerated, whose kernel bound is seconds — so the open happens on a helper thread and the
/// writer waits on it with the shutdown flag in hand. This pins the bound.
#[test]
fn a_shutdown_while_the_writer_is_inside_an_open_returns_promptly() {
    let (source, sctl) = fake_source();
    sctl.push_link();
    sctl.hold_opens();
    let (producer, writer) = spawn_with_source(Box::new(source), config());
    // Parked inside the very first open, past every cancellation check the writer makes.
    sctl.wait_for_attempts(1);

    let (returned, joiner) = shutdown_within(writer, Duration::from_secs(1));
    // Whatever the answer, let the parked open out so a failure fails rather than hangs.
    sctl.release_opens();
    let outcome = joiner.join().expect("the shutdown thread must not panic");

    assert!(
        returned,
        "shutdown blocked for over a second because the writer was inside LinkSource::open \
         (it eventually returned {outcome:?}); nothing bounds that wait"
    );
    assert_eq!(
        outcome,
        ReleaseOutcome::Unsent,
        "there was no transport to release on, and that is reported rather than hidden"
    );
    assert!(producer.stats().link_down);
}

/// …and the link that open was about to produce is dropped without a single frame being written to
/// it. Nothing may be left that can put bytes on the port after `shutdown` has returned.
#[test]
fn a_link_that_arrives_after_the_shutdown_is_dropped_unused() {
    let (source, sctl) = fake_source();
    let late = sctl.push_link();
    sctl.hold_opens();
    let (_producer, writer) = spawn_with_source(Box::new(source), config());
    sctl.wait_for_attempts(1);

    let (returned, joiner) = shutdown_within(writer, Duration::from_secs(1));
    // Only now does the open return, with a perfectly good link.
    sctl.release_opens();
    let outcome = joiner.join().expect("the shutdown thread must not panic");
    // The source is dropped by whoever was holding it, which is the thread that did the open: once
    // that has happened the link is gone and "it was never written to" is final.
    sctl.wait_for_source_dropped();

    assert!(
        returned,
        "shutdown waited for the open (returned {outcome:?})"
    );
    assert_eq!(
        late.calls(),
        Vec::new(),
        "a link that arrived after the shutdown must never be written to"
    );
}

// -- a trigger that lands while the port is opening --------------------------

/// A cancellation raised while the writer is inside `open` has no sequence of its own left to run:
/// the reconnect's sequence covers its epoch and then clears the flag. Its reason is therefore
/// taken by that sequence, not left in the one-slot reason to mislabel the next, unrelated
/// release.
#[test]
fn a_trigger_that_lands_during_an_open_is_not_stranded_on_the_next_release() {
    let (producer, writer, sctl, links) = rig(2);

    sctl.hold_opens();
    kill(&producer, &links[0], 1);
    // The writer is parked inside the open for the replacement link.
    sctl.wait_for_attempts(2);
    producer.request_release_all(ReleaseReason::Overflow);
    sctl.release_opens();
    wait_for_reconnects(&producer, 1);

    let release = producer
        .stats()
        .last_release
        .expect("commissioning records a release");
    assert_eq!(
        release.reason,
        ReleaseReason::Reconnected,
        "the reconnect is what the release-all went out for"
    );
    assert_eq!(release.outcome, ReleaseOutcome::Submitted);

    // The next release is labelled with its own reason, not with the stranded one.
    producer.request_release_all(ReleaseReason::CaptureReleased);
    assert!(
        producer.wait_until_engageable(WAIT),
        "the release must complete"
    );
    let release = producer
        .stats()
        .last_release
        .expect("a release was recorded");
    assert_eq!(
        release.reason,
        ReleaseReason::CaptureReleased,
        "the reason of a release raised while the writer was reopening leaked into the next one"
    );
    assert_eq!(release.outcome, ReleaseOutcome::Submitted);

    let mut expected = commission_calls();
    expected.extend(release_frames_rel().into_iter().map(FakeCall::Frame));
    assert_eq!(links[1].calls(), expected);
    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);
}

/// Seed stale queued input directly to verify the reconnect drain independently
/// of admission checks that normally prevent it.
#[test]
fn the_reconnect_drain_discards_what_is_queued_while_it_runs() {
    let (producer, writer, _sctl, links) = rig(2);
    links[1].stall_resync(true);
    kill(&producer, &links[0], 1);
    // Parked at step 3a: `begin_reconnect` has raised the epoch, the drain has not run yet.
    links[1].wait_for_stalled(1);

    let epoch = producer.stats().requested_epoch;
    force_enqueue_key(&producer, HidKey::Usage(B), epoch);
    assert_eq!(producer.stats().queue_depth, 1, "the queue is not empty");

    links[1].release_stalls();
    wait_for_reconnects(&producer, 1);

    assert_eq!(
        links[1].calls(),
        commission_calls(),
        "an entry queued during the sequence must not be written on the link it commissions"
    );
    let stats = producer.stats();
    assert_eq!(
        stats.queue_discarded_on_reconnect, 1,
        "and it is counted, not silently dropped"
    );
    assert_eq!(stats.queue_depth, 0);
    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);
}

// -- the snapshot is a snapshot ----------------------------------------------

/// `Stats`'s link fields come out of one critical section, so a reader that samples while the
/// writer is flapping never sees a link that is down with no `down_since`, or one that is up and
/// still timing an outage. Four hundred transitions with a reader spinning on `stats` beside
/// them; the reader's complaints are the assertion.
#[test]
fn a_stats_snapshot_never_straddles_a_link_transition() {
    let (source, sctl) = fake_source();
    // No script: every link is made on demand, so the writer can flap as long as the test likes.
    sctl.yield_links_when_empty();
    let (producer, writer) = spawn_with_source(Box::new(source), config());
    producer
        .wait_for_link(WAIT)
        .expect("the first link comes up");

    let stop = Arc::new(AtomicBool::new(false));
    let bad: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let watcher = {
        let (producer, stop, bad) = (producer.clone(), Arc::clone(&stop), Arc::clone(&bad));
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let s = producer.stats();
                if s.link_down && s.down_since.is_none() {
                    bad.lock()
                        .expect("the watcher is the only writer")
                        .push("link_down with no down_since".to_string());
                }
                if !s.link_down && s.down_since.is_some() {
                    bad.lock()
                        .expect("the watcher is the only writer")
                        .push("link up but still carrying a down_since".to_string());
                }
            }
        })
    };

    let deadline = Instant::now() + WAIT;
    let mut rounds = 0u64;
    for round in 0..400u64 {
        assert!(
            Instant::now() < deadline,
            "only completed {rounds} flap rounds before the deadline"
        );
        assert!(
            producer.wait_until_engageable(WAIT),
            "round {round}: the previous reconnect never acked"
        );
        let current = sctl
            .auto_links()
            .last()
            .cloned()
            .expect("the writer opened a link");
        current.set_behaviour(Behaviour::Fail);
        producer.engage().expect("the epoch gate is satisfied");
        producer
            .submit(Event::Key {
                key: HidKey::Usage(A),
                down: true,
            })
            .expect("submission is local");
        wait_for_reconnects(&producer, round + 1);
        rounds = round + 1;
    }
    stop.store(true, Ordering::SeqCst);
    watcher.join().expect("the watcher must not panic");

    let seen = bad.lock().expect("the watcher has finished").clone();
    assert!(
        seen.is_empty(),
        "over {rounds} transitions, {} snapshot(s) were internally inconsistent: {seen:?}",
        seen.len()
    );
    assert_eq!(producer.stats().reconnects, rounds);
    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);
}
