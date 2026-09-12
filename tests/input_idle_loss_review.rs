//! Idle transport-loss regressions: detect hang-up, acknowledge cancellation, and reconnect.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nanokvm::input::testing::{
    fake_link, fake_source, wait_for_cancellations, wait_for_reconnects, FakeControl,
    FakeSourceControl,
};
use nanokvm::input::{
    spawn, spawn_with_source, Config, Event, Producer, ReconnectConfig, ReleaseOutcome,
    ReleaseReason, SubmitError, WriterHandle,
};
use nanokvm::link::Link;
use nanokvm::proto::cmd;
use nanokvm::proto::report::HidKey;

const WAIT: Duration = Duration::from_secs(10);
const A: u8 = 0x04;

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

/// The edges a sampler thread records: when `link_down` first went true, when it next went false,
/// and the `reconnects` count at that second edge.
type Edges = (Option<Instant>, Option<Instant>, u64);

/// Wait for a sampler thread to have recorded one of its edges. `None` at the failsafe deadline.
fn await_edge<F: Fn(Edges) -> Option<Instant>>(
    edges: &Arc<std::sync::Mutex<Edges>>,
    pick: F,
) -> Option<Instant> {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if let Some(at) = pick(*edges.lock().unwrap_or_else(|p| p.into_inner())) {
            return Some(at);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    None
}

/// A source with `n` links queued and a writer whose first link is already commissioned.
fn rig(n: usize) -> (Producer, WriterHandle, FakeSourceControl, Vec<FakeControl>) {
    let (source, sctl) = fake_source();
    let links: Vec<FakeControl> = (0..n).map(|_| sctl.push_link()).collect();
    let (producer, writer) = spawn_with_source(Box::new(source), config());
    producer
        .wait_for_link(WAIT)
        .expect("the first link comes up");
    (producer, writer, sctl, links)
}

// ---------------------------------------------------------------------------
// 1. The fake cannot tell "the writer did not write" from "the writer wrote".
// ---------------------------------------------------------------------------

/// `FakeControl::hang_up` makes every later call unrecorded, so the two assertions the slice's
/// own idle test rests on — `frame_count` unchanged and `calls.len` unchanged — hold whether
/// or not the writer wrote to the dead link. A call that was refused is still a call the writer
/// made, and the claim "no write was needed to discover it" is only testable if the fake records
/// the attempt.
///
/// Failing here means the idle test's central assertion is insensitive to the behaviour it names.
#[test]
fn a_call_the_writer_makes_to_a_hung_up_fake_link_is_still_observable() {
    let (mut link, ctl) = fake_link();
    ctl.hang_up();

    let before = ctl.calls().len();
    let _ = link.transact(cmd::GET_INFO, &[], Duration::from_millis(1));
    let _ = link.resync();

    assert_ne!(
        ctl.calls().len(),
        before,
        "a hung-up FakeLink swallows the calls made to it without trace, so \
         `calls.len == calls_before` cannot distinguish a writer that stayed silent from one \
         that wrote to the dead link; the fake needs a refused-call counter"
    );
}

// ---------------------------------------------------------------------------
// 2. an outage the device never recovers from must not wedge input.
// ---------------------------------------------------------------------------

/// Idle link loss records an unsent release and advances the epoch even without
/// a replacement, preventing a permanently closed recapture gate.
#[test]
fn an_idle_hang_up_that_never_recovers_records_unsent_and_never_wedges_input() {
    let (producer, writer, _sctl, links) = rig(1);
    let before = producer.stats().cancellations;

    links[0].hang_up();
    let record = wait_for_cancellations(&producer, before + 1);

    assert_eq!(record.reason, ReleaseReason::LinkDown);
    assert_eq!(
        record.outcome,
        ReleaseOutcome::Unsent,
        "nothing could be sent on a hung-up link, and  says so out loud"
    );

    let stats = producer.stats();
    assert!(stats.link_down, "the outage is visible: {stats:?}");
    assert!(
        stats.down_since.is_some(),
        "a snapshot may never show `link_down` with no start: {stats:?}"
    );
    assert_eq!(stats.reconnects, 0, "nothing was commissioned: {stats:?}");
    assert!(
        stats.acked_epoch >= stats.requested_epoch,
        "C1: the ack must have advanced past the release the writer triggered: {stats:?}"
    );

    producer
        .engage()
        .expect("C1: a dead cable must not close the epoch gate for good");
    assert_eq!(
        producer.submit(Event::Key {
            key: HidKey::Usage(A),
            down: true,
        }),
        Err(SubmitError::LinkDown),
        "and a submission on a down link is refused rather than queued into nothing"
    );

    assert_eq!(writer.shutdown(), ReleaseOutcome::Unsent);
}

// ---------------------------------------------------------------------------
// 3. One outage costs one outage's worth of bookkeeping.
// ---------------------------------------------------------------------------

/// The poll must not double-count against the write path: one idle hang-up that is repaired is
/// two cancellation sequences (the `LinkDown` release and the `Reconnected` one) and
/// one reconnect — the same as a write-driven failure.
#[test]
fn an_idle_hang_up_costs_exactly_one_outage_of_bookkeeping() {
    let (producer, writer, _sctl, links) = rig(2);
    let before = producer.stats().cancellations;

    links[0].hang_up();
    wait_for_reconnects(&producer, 1);

    // Give any second trigger a chance to land before the count is read.
    std::thread::sleep(Duration::from_millis(300));
    let stats = producer.stats();
    assert_eq!(
        stats.cancellations,
        before + 2,
        "one LinkDown release and one Reconnected release, not more: {stats:?}"
    );
    assert_eq!(stats.reconnects, 1, "{stats:?}");
    assert!(!stats.link_down, "{stats:?}");
    assert!(stats.down_since.is_none(), "{stats:?}");

    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);
}

// ---------------------------------------------------------------------------
// 4. (`spawn`, no reconnect): reported once, and no spin.
// ---------------------------------------------------------------------------

/// A `spawn`ed  writer has no reconnect, so after the idle poll reports the loss it parks
/// again with `link_down` set. It must report it once and then stay quiet: a writer that
/// re-triggered on every 100 ms wake would spin the epoch for the life of the process.
#[test]
fn a_fixed_link_writer_reports_an_idle_hang_up_once_and_then_stays_quiet() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());

    ctl.hang_up();
    let record = wait_for_cancellations(&producer, 1);
    assert_eq!(record.reason, ReleaseReason::LinkDown);
    assert_eq!(record.outcome, ReleaseOutcome::Unsent);

    let after_first = producer.stats();
    std::thread::sleep(Duration::from_millis(500));
    let later = producer.stats();
    assert_eq!(
        later.cancellations, after_first.cancellations,
        "the poll re-reported a loss it had already reported: {later:?}"
    );
    assert_eq!(
        later.requested_epoch, after_first.requested_epoch,
        "the epoch must not advance once per health poll: {later:?}"
    );
    assert!(later.link_down);

    assert_eq!(writer.shutdown(), ReleaseOutcome::Unsent);
}

// ---------------------------------------------------------------------------
// 5. Shutdown stays bounded on the idle path.
// ---------------------------------------------------------------------------

/// The bounded-shutdown guarantee, reached through the idle loss path: the link hangs up with
/// nothing queued, the writer drops it and goes looking for a replacement, and the shutdown that
/// lands while that open is parked must still return promptly.
#[test]
fn a_shutdown_during_the_reopen_the_idle_poll_provoked_returns_promptly() {
    let (source, sctl) = fake_source();
    let first = sctl.push_link();
    sctl.push_link();
    let (producer, writer) = spawn_with_source(Box::new(source), config());
    producer.wait_for_link(WAIT).expect("the first link is up");

    sctl.hold_opens();
    first.hang_up();
    // The writer notices without being written to, and is now parked inside the reopen.
    sctl.wait_for_attempts(2);

    let done = Arc::new(AtomicU64::new(0));
    let flag = Arc::clone(&done);
    let joiner = std::thread::spawn(move || {
        let outcome = writer.shutdown();
        flag.store(1, Ordering::SeqCst);
        outcome
    });
    let deadline = Instant::now() + Duration::from_secs(1);
    while done.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    let returned = done.load(Ordering::SeqCst) == 1;
    sctl.release_opens();
    let outcome = joiner.join().expect("the shutdown thread must not panic");

    assert!(
        returned,
        "shutdown blocked for over a second on the reopen the idle poll provoked \
         (it eventually returned {outcome:?})"
    );
    assert_eq!(outcome, ReleaseOutcome::Unsent);
}

// ---------------------------------------------------------------------------
// 6. A submission racing the health poll is never lost silently.
// ---------------------------------------------------------------------------

/// Input discarded during concurrent link loss must have a corresponding release record.
#[test]
fn a_submission_racing_the_health_poll_is_always_covered_by_a_release_record() {
    let (producer, writer, _sctl, links) = rig(2);
    producer.engage().expect("the epoch gate is satisfied");

    let p = producer.clone();
    let submitter = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_millis(400);
        while Instant::now() < deadline {
            let _ = p.submit(Event::Key {
                key: HidKey::Usage(A),
                down: true,
            });
            let _ = p.submit(Event::Key {
                key: HidKey::Usage(A),
                down: false,
            });
        }
    });
    std::thread::sleep(Duration::from_millis(20));
    links[0].hang_up();
    wait_for_reconnects(&producer, 1);
    submitter.join().expect("the submitting thread");

    let stats = producer.stats();
    assert_eq!(stats.reconnects, 1, "{stats:?}");
    assert!(
        stats.queue_depth == 0,
        "the reconnect drains the queue rather than replaying it: {stats:?}"
    );
    assert_eq!(
        links[1].frames().len(),
        3,
        "the replacement link saw only the release-all pair and GET_INFO — never a replay of \
         input produced against the session that died "
    );

    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);
}

// ---------------------------------------------------------------------------
// 7. The poll must not invent an outage on a healthy link.
// ---------------------------------------------------------------------------

/// Ten wake-ups a second, for ever, on a link that is fine: nothing may move. A false positive
/// here would disengage a user who is simply not typing.
#[test]
fn an_idle_writer_on_a_healthy_link_never_triggers_anything() {
    let (producer, writer, _sctl, links) = rig(1);
    let before = producer.stats();
    let frames_before = links[0].frame_count();
    let calls_before = links[0].calls().len();

    // Twelve health polls' worth of doing nothing at all.
    std::thread::sleep(Duration::from_millis(1200));

    let after = producer.stats();
    assert_eq!(after.cancellations, before.cancellations, "{after:?}");
    assert_eq!(after.requested_epoch, before.requested_epoch, "{after:?}");
    assert_eq!(after.reconnects, 0, "{after:?}");
    assert!(!after.link_down, "{after:?}");
    assert_eq!(
        links[0].frame_count(),
        frames_before,
        "the health query must put no bytes on the link: {after:?}"
    );
    assert_eq!(
        links[0].calls().len(),
        calls_before,
        "and it is not a call of any other kind either: {after:?}"
    );

    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);
}

// ---------------------------------------------------------------------------
// 8. `Stats` stays one consistent snapshot across repeated idle outages.
// ---------------------------------------------------------------------------

/// `link_down` and `down_since` are written in one critical section, and a reader hammering
/// `stats` across several idle-discovered outages must never catch them apart — nor see
/// `reconnects` credited while the link is still down.
#[test]
fn stats_never_straddle_a_transition_the_idle_poll_provoked() {
    let (source, sctl) = fake_source();
    sctl.yield_links_when_empty();
    let (producer, writer) = spawn_with_source(Box::new(source), config());
    producer.wait_for_link(WAIT).expect("the first link is up");

    let stop = Arc::new(AtomicU64::new(0));
    let p = producer.clone();
    let flag = Arc::clone(&stop);
    let watcher = std::thread::spawn(move || {
        let mut torn = 0u64;
        let mut credited_while_down = 0u64;
        while flag.load(Ordering::SeqCst) == 0 {
            let s = p.stats();
            if s.link_down != s.down_since.is_some() {
                torn += 1;
            }
            if s.link_down && s.reconnects > 0 && s.device_info.is_none() {
                credited_while_down += 1;
            }
        }
        (torn, credited_while_down)
    });

    for round in 1..=4u64 {
        let links = sctl.auto_links();
        // The link currently in service is the last one the source made, or the scripted first.
        if let Some(current) = links.last() {
            current.hang_up();
        } else {
            // The very first link came from the auto pool too once the script is empty; if not,
            // provoke the first outage by exhausting the script.
            sctl.auto_links();
        }
        wait_for_reconnects(&producer, round);
    }

    stop.store(1, Ordering::SeqCst);
    let (torn, credited) = watcher.join().expect("the watching thread");
    assert_eq!(torn, 0, "a snapshot showed link_down and down_since apart");
    assert_eq!(
        credited, 0,
        "a reconnect was credited while the link was still down"
    );

    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);
}

/// Polling only after reset returns can miss an outage already repaired by the writer.
#[test]
fn the_h_a4_pattern_of_sampling_link_down_after_the_outage_is_a_race() {
    let (producer, writer, sctl, links) = rig(2);

    // Observe before provoking the outage so both loss and recovery remain measurable.
    let stop = Arc::new(AtomicU64::new(0));
    let edges: Arc<std::sync::Mutex<Edges>> = Arc::new(std::sync::Mutex::new((None, None, 0)));
    let p = producer.clone();
    let flag = Arc::clone(&stop);
    let recorded = Arc::clone(&edges);
    let sampler = std::thread::spawn(move || {
        while flag.load(Ordering::SeqCst) == 0 {
            let s = p.stats();
            let mut e = recorded.lock().unwrap_or_else(|p| p.into_inner());
            let (down_at, up_at, reconnects_at_up): &mut (Option<Instant>, Option<Instant>, u64) =
                &mut e;
            if down_at.is_none() {
                if s.link_down {
                    *down_at = Some(Instant::now());
                }
            } else if up_at.is_none() && !s.link_down {
                *up_at = Some(Instant::now());
                *reconnects_at_up = s.reconnects;
            }
            drop(e);
            std::thread::sleep(Duration::from_millis(1));
        }
    });

    // `hold_opens` stands in for the ~400 ms the dongle spends re-enumerating: without it an
    // in-memory reopen lands in microseconds and *this* test would be the racy one.
    sctl.hold_opens();
    let provoked = Instant::now();
    links[0].hang_up();
    let down_at = await_edge(&edges, |e| e.0).expect("the sampler must see the link go down");
    sctl.release_opens();
    // Stands in for "usb-replug.py returned, and the writer got there first".
    wait_for_reconnects(&producer, 1);
    let up_at = await_edge(&edges, |e| e.1).expect("the sampler must see the link come back");
    stop.store(1, Ordering::SeqCst);
    sampler.join().expect("the sampling thread");

    // The old pattern: a level poll that only starts now. `link_down` is an edge, and the edge
    // has been and gone.
    let mut seen = false;
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if producer.stats().link_down {
            seen = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let stats = producer.stats();
    assert!(
        !seen,
        "a level poll of `link_down` started after the repair landed must find nothing — if this \
         ever fires, `link_down` is outliving its reconnect and test 3's invariant is broken: \
         {stats:?}"
    );

    let reconnects_at_up = edges.lock().unwrap_or_else(|p| p.into_inner()).2;
    assert_eq!(
        reconnects_at_up, 1,
        "the link came back up with the reconnect credited: {stats:?}"
    );
    // Measure notice and recovery against the clock started before the outage.
    println!(
        "notice latency (bounded by a clock started before the outage): {:?}, down for {:?}",
        down_at.saturating_duration_since(provoked),
        up_at.saturating_duration_since(down_at)
    );

    assert_eq!(writer.shutdown(), ReleaseOutcome::Submitted);
}
