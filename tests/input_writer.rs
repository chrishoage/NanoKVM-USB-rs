//! Writer robustness (§3.1, §5.1, §2.6.1) and the §2.8 instrumentation sanity checks.
//!
//! A device error frame is a rejected report on a healthy link (§3.1) and must not stop the
//! writer or lose the next report. A timeout means the frame was written and nothing answered —
//! the device may still have acted on it (§2.6.1) — so it is counted, flagged, and the writer
//! carries on.

use std::time::Duration;

use nanokvm::input::testing::{fake_link, gate_frame, kb_frame, stall_writer, Behaviour};
use nanokvm::input::{spawn, Config, Event, Producer, ReleaseReason};
use nanokvm::proto::report::{button, HidKey};

const WAIT: Duration = Duration::from_secs(10);
const A: u8 = 0x04;
const B: u8 = 0x05;

fn config() -> Config {
    Config {
        max_barriers: 64,
        transact_timeout: Duration::from_millis(50),
        // Slice A added `Config::reconnect`; these paths use `spawn`, which never reconnects.
        ..Config::default()
    }
}

/// A device error reply does not stop the writer and does not lose the next report (§3.1).
#[test]
fn a_device_error_does_not_stop_the_writer() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    // The next call after the stall is rejected by the device; the one after it is fine.
    ctl.script([Behaviour::DeviceError(0xE4)]);
    producer
        .submit(Event::Key {
            key: HidKey::Usage(A),
            down: true,
        })
        .expect("key");
    producer
        .submit(Event::Key {
            key: HidKey::Usage(B),
            down: true,
        })
        .expect("key");

    ctl.release_stalls();
    ctl.wait_for_frames(3);

    assert_eq!(
        ctl.frames(),
        vec![
            gate_frame(),
            kb_frame(0, [A, 0, 0, 0, 0, 0]),
            kb_frame(0, [A, B, 0, 0, 0, 0])
        ]
    );
    let stats = producer.stats();
    assert_eq!(stats.device_errors, 1);
    assert!(
        !stats.link_down,
        "a rejected frame does not mean the link is gone"
    );
    assert_eq!(stats.kb_reports, 2);
    let _ = writer.shutdown();
}

/// A timeout is counted, flags the path degraded, and the writer continues (§5.1, §2.6.1).
#[test]
fn a_timeout_is_counted_and_the_writer_continues() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    ctl.script([Behaviour::Timeout]);
    producer
        .submit(Event::Key {
            key: HidKey::Usage(A),
            down: true,
        })
        .expect("key");
    producer
        .submit(Event::Key {
            key: HidKey::Usage(B),
            down: true,
        })
        .expect("key");

    ctl.release_stalls();
    ctl.wait_for_frames(3);

    assert_eq!(ctl.frames()[2], kb_frame(0, [A, B, 0, 0, 0, 0]));
    let stats = producer.stats();
    assert_eq!(stats.timeouts, 1);
    assert!(stats.degraded);
    assert!(!stats.link_down);
    let _ = writer.shutdown();
}

/// §2.8: queue depth, time-in-queue and the coalescing counters are populated. Sanity only — the
/// point is that overload is observable rather than mysterious.
#[test]
fn the_instrumentation_is_populated() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    for i in 0..5 {
        for x in 0..10u16 {
            producer
                .submit(Event::PointerAbs { x: x * 7, y: i })
                .expect("motion");
            producer.submit(Event::Wheel { delta: 1 }).expect("wheel");
        }
        producer
            .submit(Event::Button {
                button: button::LEFT,
                down: i % 2 == 0,
            })
            .expect("button");
    }

    let queued = producer.stats();
    assert_eq!(queued.barrier_depth, 5);
    assert_eq!(
        queued.queue_depth, 10,
        "five motion runs interleaved with five barriers"
    );
    assert!(queued.max_queue_depth >= 10);
    assert!(queued.max_barrier_depth >= 5);
    assert!(queued.abs_replaced >= 45);
    assert!(queued.wheel_summed >= 45);

    ctl.release_stalls();
    // Five coalesced motion runs and five button transitions, one frame each, after the gate.
    ctl.wait_for_frames(11);
    producer.request_release_all(ReleaseReason::UserRequested);
    assert!(producer.wait_until_engageable(WAIT));

    let done = producer.stats();
    assert_eq!(done.queue_depth, 0);
    assert!(
        done.max_time_in_queue > Duration::ZERO,
        "time-in-queue is measured"
    );
    assert!(done.last_time_in_queue <= done.max_time_in_queue);
    assert!(done.kb_reports >= 1, "the release-all keyboard report");
    assert!(done.abs_reports >= 1);
    assert!(done.rel_reports >= 1);
    assert_eq!(done.acked_epoch, done.requested_epoch);
    let _ = writer.shutdown();
}

/// The stats snapshot is readable from another thread while the writer is working, and a cloned
/// producer sees the same path (`Producer: Clone + Send + Sync`).
#[test]
fn stats_are_readable_from_a_clone_on_another_thread() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    let clone = producer.clone();
    let reader = std::thread::spawn(move || clone.stats().requested_epoch);

    producer
        .submit(Event::Key {
            key: HidKey::Usage(A),
            down: true,
        })
        .expect("key");
    ctl.wait_for_frames(1);
    assert_eq!(reader.join().expect("reader thread"), 0);
    let _ = writer.shutdown();
}

/// The API contract the viewer is written against: `Producer: Clone + Send + Sync` (§2.9).
#[test]
fn producer_is_clone_send_and_sync() {
    fn assert_bounds<T: Clone + Send + Sync>() {}
    assert_bounds::<Producer>();
}
