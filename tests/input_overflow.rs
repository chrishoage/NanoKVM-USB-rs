//! §9.2 item 5 — **overflow failure** (§2.8): saturation invalidates pending input, releases, and
//! requires recapture rather than degrading. The saturation cause here is a **stalled writer**,
//! which is what the plan insists on: "a blocked port write, a device that stopped draining, or a
//! serial link wedged mid-frame all saturate a queue while the user types normally".
//!
//! Assertions are on the recorded frame sequence, except where a counter *is* the observable
//! (`stats().overflows`).

use std::time::Duration;

use nanokvm::input::testing::{fake_link, gate_frame, release_frames_rel, stall_writer};
use nanokvm::input::{spawn, Config, Event, ReleaseOutcome, ReleaseReason, SubmitError};
use nanokvm::proto::report::button;

const WAIT: Duration = Duration::from_secs(10);

fn config() -> Config {
    Config {
        max_barriers: 16,
        transact_timeout: Duration::from_millis(50),
    }
}

/// A stream of alternating button transitions — which cannot coalesce — past `max_barriers`.
/// `submit` fails, the session is cancelled, and the stale transitions are never written (§2.8).
#[test]
fn transitions_beyond_the_bound_fail_the_session() {
    let (link, ctl) = fake_link();
    let cfg = config();
    let (producer, writer) = spawn(link, cfg);
    stall_writer(&producer, &ctl);

    for i in 0..cfg.max_barriers {
        producer
            .submit(Event::Button {
                button: button::LEFT,
                down: i % 2 == 0,
            })
            .expect("within the bound");
    }
    assert_eq!(
        producer.submit(Event::Button {
            button: button::LEFT,
            down: false
        }),
        Err(SubmitError::Overflow),
        "coalescing cannot make room for a transition, so the session has failed"
    );

    // §2.8: do not silently resume. Input stays disengaged until deliberate recapture.
    assert!(!producer.is_engaged());
    assert_eq!(
        producer.submit(Event::Button {
            button: button::RIGHT,
            down: true
        }),
        Err(SubmitError::Disengaged)
    );

    ctl.release_stalls();
    assert!(producer.wait_until_engageable(WAIT));

    // The writer discarded the stale queue and wrote the release itself: no button frame from
    // before the overflow ever reached the link.
    let mut expected = vec![gate_frame()];
    expected.extend(release_frames_rel());
    assert_eq!(ctl.frames(), expected);

    let stats = producer.stats();
    assert_eq!(
        stats.overflows, 1,
        "one overflow event, visible rather than silent"
    );
    assert_eq!(stats.cancellations, 1);
    let release = stats.last_release.expect("a release was recorded");
    assert_eq!(release.reason, ReleaseReason::Overflow);
    assert_eq!(release.outcome, ReleaseOutcome::Submitted);

    // Recapture works and the next session writes normally.
    producer.engage().expect("deliberate recapture");
    producer
        .submit(Event::Button {
            button: button::LEFT,
            down: true,
        })
        .expect("new session");
    ctl.wait_for_frames(4);
    let _ = writer.shutdown();
}

/// The normal path: the queue is dominated by redundant absolute positions, coalescing reclaims
/// them, and ten thousand of them fit behind a stalled writer without overflowing (§2.8 step 1).
#[test]
fn ten_thousand_absolute_positions_coalesce_and_never_overflow() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    for i in 0..10_000u16 {
        producer
            .submit(Event::PointerAbs {
                x: i % 4096,
                y: (i / 2) % 4096,
            })
            .expect("motion must always fit; it coalesces");
    }

    let stats = producer.stats();
    assert_eq!(stats.overflows, 0);
    assert_eq!(
        stats.queue_depth, 1,
        "ten thousand positions, one coalesced run"
    );
    assert_eq!(stats.barrier_depth, 0);
    assert!(stats.abs_replaced >= 9_999);
    assert!(producer.is_engaged());

    ctl.release_stalls();
    ctl.wait_for_frames(2);

    // Exactly one report, at the newest position (§2.4: the drag becomes a straight line, which
    // is the accepted degradation, not a bug).
    let last = 9_999u16;
    assert_eq!(
        ctl.frames()[1],
        nanokvm::input::testing::abs_frame(0, last % 4096, (last / 2) % 4096, 0)
    );
    let _ = writer.shutdown();
}

/// Relative motion behind a stalled writer coalesces too, and the accumulation is preserved in
/// full when it finally flushes (§2.2, §2.3) — no overflow, nothing lost.
#[test]
fn a_flood_of_relative_motion_never_overflows_and_is_never_lost() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    for _ in 0..1_000 {
        producer
            .submit(Event::PointerRel { dx: 1, dy: -1 })
            .expect("motion must always fit");
    }
    assert_eq!(producer.stats().overflows, 0);
    assert_eq!(producer.stats().queue_depth, 1);

    ctl.release_stalls();
    // 1000 splits into ceil(1000/127) = 8 reports.
    ctl.wait_for_frames(9);

    let frames = ctl.frames();
    let dx: i32 = frames[1..9]
        .iter()
        .map(|f| i32::from(f.payload[2] as i8))
        .sum();
    let dy: i32 = frames[1..9]
        .iter()
        .map(|f| i32::from(f.payload[3] as i8))
        .sum();
    assert_eq!(
        (dx, dy),
        (1_000, -1_000),
        "saturate and split, never clamp and lose"
    );
    let _ = writer.shutdown();
}
