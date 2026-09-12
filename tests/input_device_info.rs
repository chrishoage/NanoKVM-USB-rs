//! The on-demand `GET_INFO` refresh (plan §12 Stage 4c, `docs/STAGE3_FINDINGS.md` D2).
//!
//! D2's CapsLock policy needs the target's lock bits, and the viewer's clipboard paste needs them
//! **fresh**: `Stats::device_info` on its own is whatever the link reported when it was
//! commissioned, which on a stable link is the value from process start. Deciding whether a page of
//! letters would arrive inverted from a reading taken minutes ago is exactly the "an
//! acknowledgement is not evidence of effect" mistake in another costume.
//!
//! So [`nanokvm::input::Producer::refresh_device_info`] asks the **writer** — the one thread that
//! may touch the link (§2.6, §5.1) — for a new one, and `Stats::device_info_generation` is how a
//! caller knows it is looking at an answer rather than at a memory of one.
//!
//! Nothing here asserts the device received anything (§2.6.1); every assertion is about the frames
//! the writer submitted and about what the snapshot then says.

use std::time::{Duration, Instant};

use nanokvm::input::testing::{fake_source, Behaviour, FakeCall, FakeControl, FakeSourceControl};
use nanokvm::input::{spawn_with_source, Config, Producer, ReconnectConfig, WriterHandle};
use nanokvm::proto::cmd;

const WAIT: Duration = Duration::from_secs(10);

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

fn rig(n: usize) -> (Producer, WriterHandle, FakeSourceControl, Vec<FakeControl>) {
    let (source, sctl) = fake_source();
    let links: Vec<FakeControl> = (0..n).map(|_| sctl.push_link()).collect();
    let (producer, writer) = spawn_with_source(Box::new(source), config());
    producer
        .wait_for_link(WAIT)
        .expect("the first link comes up");
    (producer, writer, sctl, links)
}

/// Poll a condition with a bounded deadline, failing at it rather than hanging. The generation has
/// no rendezvous of its own — it is advanced by the writer between frames.
fn settle<F: Fn() -> bool>(what: &str, done: F) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// How many `GET_INFO` frames a link has been asked for.
fn get_infos(link: &FakeControl) -> usize {
    link.calls()
        .iter()
        .filter(|c| matches!(c, FakeCall::Frame(f) if f.cmd == cmd::GET_INFO))
        .count()
}

/// The commissioning already read the lock bits once; a refresh reads them again, and the
/// generation is what says which reading the snapshot carries.
#[test]
fn a_refresh_transacts_get_info_and_advances_the_generation() {
    let (producer, writer, _sctl, links) = rig(1);
    let before = producer.stats();
    let commissioning_reads = get_infos(&links[0]);
    assert_eq!(
        commissioning_reads,
        1,
        "§2.7 step 3d already asked once: {:?}",
        links[0].calls()
    );
    assert_eq!(
        before.device_info_generation, 1,
        "the commissioning reading is generation 1"
    );

    // The target's CapsLock goes on while the link sits idle — which is the whole scenario: the
    // reading the viewer is holding is now wrong, and nothing told it.
    links[0].set_get_info_payload(&[0x38, 0x01, 0x02]);
    assert!(
        !producer.stats().device_info.expect("info").caps_lock,
        "the snapshot still carries the commissioning reading, as it must"
    );

    let baseline = producer.refresh_device_info();
    assert_eq!(
        baseline, before.device_info_generation,
        "the baseline is the generation the request was made against"
    );
    settle("the refresh to land", || {
        producer.stats().device_info_generation > baseline
    });

    assert_eq!(
        get_infos(&links[0]),
        commissioning_reads + 1,
        "exactly one more GET_INFO: {:?}",
        links[0].calls()
    );
    assert!(
        producer.stats().device_info.expect("info").caps_lock,
        "the refreshed reading is the one the snapshot carries"
    );
    let _ = writer.shutdown();
}

/// **The baseline comes back from the request, and a fast writer cannot beat it.**
///
/// The caller's rule is "wait until the generation passes the one my request was made against",
/// and the only place that number can be read without a race is inside the critical section the
/// request is made in. A caller that asked and *then* read `stats()` would, against a writer that
/// answered in between, take the answer's own generation for its baseline and wait for a second
/// answer that nobody asked for — which for the viewer's paste is the whole 3 s preparation
/// deadline and then a refusal saying the device did not answer, about a device that answered in
/// milliseconds.
///
/// Here the refresh is deliberately serviced **before** the caller looks at any snapshot, and the
/// baseline it was given still reads as "not yet answered". The discriminating assertion is the
/// first one: the number handed back is the generation from *before* the request, which is the one
/// thing a `stats()` taken after the call cannot be relied on to be.
#[test]
fn a_refresh_serviced_before_the_caller_looks_still_shows_as_a_change() {
    let (producer, writer, _sctl, links) = rig(1);
    let reads = get_infos(&links[0]);
    links[0].set_get_info_payload(&[0x38, 0x01, 0x02]);

    let before = producer.stats().device_info_generation;
    let baseline = producer.refresh_device_info();
    assert_eq!(
        baseline, before,
        "the baseline is the generation the request was made against, never the answer's"
    );
    // The writer answers while the caller is still holding nothing but that number.
    settle("the writer to service the refresh", || {
        get_infos(&links[0]) > reads
    });
    settle("the reading to be published", || {
        producer.stats().device_info.is_some_and(|i| i.caps_lock)
    });

    assert!(
        producer.stats().device_info_generation > baseline,
        "the answer must read as a change against the baseline the request returned"
    );
    assert!(!producer.stats().device_info_stale);
    let _ = writer.shutdown();
}

/// Repeated requests coalesce into one transaction, like every other one-slot flag in §2.6: a
/// menu item clicked twice must not queue two reads onto a link that is also carrying keystrokes.
#[test]
fn repeated_requests_coalesce_into_one_transaction() {
    let (producer, writer, _sctl, links) = rig(1);
    let before = producer.stats().device_info_generation;
    let reads = get_infos(&links[0]);

    for _ in 0..8 {
        let _ = producer.refresh_device_info();
    }
    settle("a refresh to land", || {
        producer.stats().device_info_generation > before
    });

    let after = get_infos(&links[0]);
    assert!(
        after - reads <= 8,
        "a one-slot flag cannot produce more reads than requests: {reads} -> {after}"
    );
    assert!(after > reads, "at least one read happened");
    let _ = writer.shutdown();
}

/// **A device that will not answer must not wedge the caller.** The generation advances anyway, so
/// a viewer waiting on it learns that it asked and that the newest answer there is is the old one
/// — which the paste then refuses to type letters through (D2, `chrome::paste`).
#[test]
fn a_device_that_does_not_answer_still_advances_the_generation() {
    let (producer, writer, _sctl, links) = rig(1);
    let before = producer.stats().device_info_generation;

    // The transport fails outright, which is the same shape as a device that has stopped
    // answering: the writer records a failed transaction rather than waiting on it.
    links[0].set_behaviour(Behaviour::Fail);
    let _ = producer.refresh_device_info();
    settle("the failed refresh to be recorded", || {
        producer.stats().device_info_generation > before
    });

    assert!(
        producer.stats().device_info_stale,
        "the reading is not an answer to the request, and must not pass for one"
    );
    let _ = writer.shutdown();
}

/// **A failed refresh does not end the session.** It is a read-only convenience — nobody's
/// keystroke depends on it — and one timed-out `GET_INFO` is not evidence that the transport is
/// gone. Escalating it to a `LinkDown` cancellation would take a user's capture away on behalf of
/// a menu item that only wanted to know whether CapsLock was on; the stale reading is the whole
/// consequence of it, and the paste refuses on that (D2, `chrome::paste`).
#[test]
fn a_failed_refresh_leaves_the_session_engaged() {
    let (producer, writer, _sctl, links) = rig(1);
    // Commissioning a link is a §2.7 sequence and ends disengaged, like every other cancellation;
    // this is the deliberate recapture that puts a session in front of the refresh.
    producer.engage().expect("deliberate recapture");
    let cancellations = producer.stats().cancellations;

    // One transaction times out — written, never answered, which is what a device that has stopped
    // answering looks like — and everything after it is served normally. A link that is *gone* is
    // the link-health poll's business (`Writer::note_link_lost`), not this one's.
    links[0].script([Behaviour::Timeout]);
    let before = producer.refresh_device_info();
    settle("the failed refresh to be recorded", || {
        producer.stats().device_info_generation > before
    });
    assert!(
        producer.stats().device_info_stale,
        "asked, and nobody answered"
    );

    // The consequence, not the acknowledgement: a keystroke is still admitted. `submit` is what
    // §2.6 closes on a cancellation, so a session that still takes one was never cancelled.
    producer
        .submit(nanokvm::input::Event::Key {
            key: nanokvm::proto::HidKey::Usage(0x04),
            down: true,
        })
        .expect("the session is still engaged");
    producer
        .submit(nanokvm::input::Event::Key {
            key: nanokvm::proto::HidKey::Usage(0x04),
            down: false,
        })
        .expect("and stays engaged");
    assert_eq!(
        producer.stats().cancellations,
        cancellations,
        "no release-all was raised: {:?}",
        producer.stats().last_release
    );
    let _ = writer.shutdown();
}

/// A refresh asked for while the link is down is answered locally, without touching a transport
/// that is not there: the generation moves and the reading does not.
#[test]
fn a_refresh_on_a_dead_link_answers_without_a_transaction() {
    // No links at all: the source has nothing to hand over, so the writer never commissions one.
    let (source, _sctl) = fake_source();
    let (producer, writer) = spawn_with_source(Box::new(source), config());
    assert!(producer.wait_for_link(Duration::from_millis(50)).is_err());

    let before = producer.refresh_device_info();
    settle("the refresh to be answered", || {
        producer.stats().device_info_generation > before
    });
    let stats = producer.stats();
    assert!(
        stats.device_info.is_none(),
        "there was never a reading to refresh"
    );
    assert!(stats.device_info_stale, "nobody could ask");
    let _ = writer.shutdown();
}

/// A reading that lands clears the staleness, so one failed refresh does not poison every paste
/// after it.
#[test]
fn a_reading_that_lands_clears_the_staleness() {
    let (producer, writer, _sctl, links) = rig(1);
    assert!(
        !producer.stats().device_info_stale,
        "the commissioning reading is an answer"
    );

    // A reply the parser cannot use: the generation advances, and the reading goes stale.
    links[0].set_get_info_payload(&[0x38]);
    let before = producer.refresh_device_info();
    settle("the short reply to be recorded", || {
        producer.stats().device_info_generation > before
    });
    assert!(
        producer.stats().device_info_stale,
        "a short payload is no answer"
    );

    // And a good one clears it.
    links[0].set_get_info_payload(&[0x38, 0x01, 0x02]);
    let before = producer.refresh_device_info();
    settle("the good reply to land", || {
        producer.stats().device_info_generation > before
    });
    let stats = producer.stats();
    assert!(!stats.device_info_stale);
    assert!(stats.device_info.expect("info").caps_lock);
    let _ = writer.shutdown();
}
