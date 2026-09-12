//! Report ordering across coalesced motion and transition barriers.

use std::time::Duration;

use nanokvm::input::testing::{
    abs_frame, fake_link, gate_frame, kb_frame, rel_frame, stall_writer, RecordedFrame,
};
use nanokvm::input::{spawn, Config, Event};
use nanokvm::proto::report::{button, HidKey};

fn config() -> Config {
    Config {
        max_barriers: 64,
        transact_timeout: Duration::from_millis(50),
        // Slice A added `Config::reconnect`; these paths use `spawn`, which never reconnects.
        ..Config::default()
    }
}

/// Move, press, drag, and release must retain their order.
#[test]
fn move_press_drag_release_is_intact_by_construction() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    // Positions before the press coalesce to A; positions during the drag coalesce to B.
    for x in [10, 20, 30, 100] {
        producer
            .submit(Event::PointerAbs { x, y: 40 })
            .expect("motion");
    }
    producer
        .submit(Event::Button {
            button: button::LEFT,
            down: true,
        })
        .expect("press");
    for x in [110, 150, 200] {
        producer
            .submit(Event::PointerAbs { x, y: 40 })
            .expect("drag");
    }
    producer
        .submit(Event::Button {
            button: button::LEFT,
            down: false,
        })
        .expect("release");

    ctl.release_stalls();
    ctl.wait_for_frames(5);

    assert_eq!(
        ctl.frames(),
        vec![
            gate_frame(),
            abs_frame(0, 100, 40, 0),
            abs_frame(button::LEFT, 100, 40, 0),
            abs_frame(button::LEFT, 200, 40, 0),
            abs_frame(0, 200, 40, 0),
        ]
    );
    let _ = writer.shutdown();
}

/// Relative motion accumulates and splits without losing in-range deltas.
#[test]
fn relative_deltas_are_summed_and_split_at_127() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    producer
        .submit(Event::PointerRel { dx: 100, dy: 0 })
        .expect("motion");
    producer
        .submit(Event::PointerRel { dx: 200, dy: 0 })
        .expect("motion");

    ctl.release_stalls();
    ctl.wait_for_frames(4);

    let frames = ctl.frames();
    assert_eq!(
        frames,
        vec![
            gate_frame(),
            rel_frame(0, 127, 0, 0),
            rel_frame(0, 127, 0, 0),
            rel_frame(0, 46, 0, 0)
        ]
    );
    // Nothing lost and nothing reversed: the parts sum to the whole, in one direction.
    let sum: i32 = frames[1..]
        .iter()
        .map(|f| i32::from(f.payload[2] as i8))
        .sum();
    assert_eq!(sum, 300);
    assert!(frames[1..].iter().all(|f| (f.payload[2] as i8) > 0));
    let _ = writer.shutdown();
}

/// The same split in the negative direction, and on both axes at once.
#[test]
fn split_preserves_sign_on_both_axes() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    producer
        .submit(Event::PointerRel { dx: -300, dy: 130 })
        .expect("motion");

    ctl.release_stalls();
    ctl.wait_for_frames(4);

    assert_eq!(
        ctl.frames(),
        vec![
            gate_frame(),
            rel_frame(0, -127, 127, 0),
            rel_frame(0, -127, 3, 0),
            rel_frame(0, -46, 0, 0),
        ]
    );
    let _ = writer.shutdown();
}

/// Wheel deltas accumulate and split like relative motion.
#[test]
fn wheel_is_summed_and_split() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    producer.submit(Event::Wheel { delta: 100 }).expect("wheel");
    producer.submit(Event::Wheel { delta: 100 }).expect("wheel");

    ctl.release_stalls();
    ctl.wait_for_frames(3);

    assert_eq!(
        ctl.frames(),
        vec![
            gate_frame(),
            rel_frame(0, 0, 0, 127),
            rel_frame(0, 0, 0, 73)
        ]
    );
    let _ = writer.shutdown();
}

/// A wheel-only run in `Abs` mode goes out as an absolute report at the last known position.
#[test]
fn wheel_only_in_abs_mode_repeats_the_last_position() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    producer
        .submit(Event::PointerAbs { x: 700, y: 800 })
        .expect("motion");
    producer
        .submit(Event::Button {
            button: button::RIGHT,
            down: true,
        })
        .expect("press");
    producer.submit(Event::Wheel { delta: -3 }).expect("wheel");

    ctl.release_stalls();
    ctl.wait_for_frames(4);

    assert_eq!(
        ctl.frames(),
        vec![
            gate_frame(),
            abs_frame(0, 700, 800, 0),
            abs_frame(button::RIGHT, 700, 800, 0),
            abs_frame(button::RIGHT, 700, 800, -3),
        ]
    );
    let _ = writer.shutdown();
}

/// Mixed input queued behind a stalled writer must retain every transition barrier.
#[test]
fn transitions_are_never_reordered_under_a_stalled_writer() {
    const A: u8 = 0x04;
    const B: u8 = 0x05;

    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    producer
        .submit(Event::Key {
            key: HidKey::Usage(A),
            down: true,
        })
        .expect("key");
    for _ in 0..3 {
        producer
            .submit(Event::PointerRel { dx: 10, dy: 0 })
            .expect("motion");
    }
    producer
        .submit(Event::Button {
            button: button::LEFT,
            down: true,
        })
        .expect("press");
    producer
        .submit(Event::Key {
            key: HidKey::Usage(B),
            down: true,
        })
        .expect("key");
    producer
        .submit(Event::Key {
            key: HidKey::Usage(A),
            down: false,
        })
        .expect("key");
    producer
        .submit(Event::Button {
            button: button::LEFT,
            down: false,
        })
        .expect("release");
    producer.submit(Event::Wheel { delta: 3 }).expect("wheel");

    ctl.release_stalls();
    ctl.wait_for_frames(8);

    assert_eq!(
        ctl.frames(),
        vec![
            gate_frame(),
            kb_frame(0, [A, 0, 0, 0, 0, 0]),
            // The motion ahead of the press flushes first, with the button mask as it was.
            rel_frame(0, 30, 0, 0),
            rel_frame(button::LEFT, 0, 0, 0),
            kb_frame(0, [A, B, 0, 0, 0, 0]),
            kb_frame(0, [B, 0, 0, 0, 0, 0]),
            rel_frame(0, 0, 0, 0),
            rel_frame(0, 0, 0, 3),
        ]
    );
    let _ = writer.shutdown();
}

/// Coalescing a press with its release would erase the keystroke.
#[test]
fn a_fast_press_release_pair_is_never_merged_away() {
    const A: u8 = 0x04;

    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    producer
        .submit(Event::Key {
            key: HidKey::Usage(A),
            down: true,
        })
        .expect("key");
    producer
        .submit(Event::Key {
            key: HidKey::Usage(A),
            down: false,
        })
        .expect("key");

    ctl.release_stalls();
    ctl.wait_for_frames(3);

    assert_eq!(
        ctl.frames(),
        vec![
            gate_frame(),
            kb_frame(0, [A, 0, 0, 0, 0, 0]),
            kb_frame(0, [0; 6])
        ]
    );
    let _ = writer.shutdown();
}

/// A modifier press must remain before the click it modifies.
#[test]
fn shift_click_keeps_its_order() {
    use nanokvm::proto::report::modifier;

    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    producer
        .submit(Event::Key {
            key: HidKey::Modifier(modifier::LEFT_SHIFT),
            down: true,
        })
        .expect("modifier");
    producer
        .submit(Event::Button {
            button: button::LEFT,
            down: true,
        })
        .expect("press");
    producer
        .submit(Event::Button {
            button: button::LEFT,
            down: false,
        })
        .expect("release");
    producer
        .submit(Event::Key {
            key: HidKey::Modifier(modifier::LEFT_SHIFT),
            down: false,
        })
        .expect("modifier");

    ctl.release_stalls();
    ctl.wait_for_frames(5);

    assert_eq!(
        ctl.frames(),
        vec![
            gate_frame(),
            kb_frame(modifier::LEFT_SHIFT, [0; 6]),
            rel_frame(button::LEFT, 0, 0, 0),
            rel_frame(0, 0, 0, 0),
            kb_frame(0, [0; 6]),
        ]
    );
    let _ = writer.shutdown();
}

/// Absolute positions are clamped to `ABS_MAX` on the way in: an overshoot past 8191
/// would silently jump the pointer to the top-left corner of a live console.
#[test]
fn absolute_coordinates_are_clamped_before_they_reach_the_device() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    producer
        .submit(Event::PointerAbs { x: 60000, y: 4096 })
        .expect("motion");

    ctl.release_stalls();
    ctl.wait_for_frames(2);

    let frames: Vec<RecordedFrame> = ctl.frames();
    assert_eq!(frames[1], abs_frame(0, 4095, 4095, 0));
    let _ = writer.shutdown();
}

/// A zero-delta submission carries nothing observable and produces no frame at all.
#[test]
fn a_zero_delta_produces_no_frame() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    producer
        .submit(Event::PointerRel { dx: 0, dy: 0 })
        .expect("motion");
    producer.submit(Event::Wheel { delta: 0 }).expect("wheel");
    producer
        .submit(Event::Key {
            key: HidKey::Usage(0x04),
            down: true,
        })
        .expect("key");

    ctl.release_stalls();
    ctl.wait_for_frames(2);

    assert_eq!(
        ctl.frames(),
        vec![gate_frame(), kb_frame(0, [0x04, 0, 0, 0, 0, 0])]
    );
    let _ = writer.shutdown();
}

/// Bound accumulated runs separately from report splitting so cancellation cannot
/// wait behind unlimited motion.
#[test]
fn an_oversized_run_is_capped_and_the_truncation_is_counted() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    producer
        .submit(Event::PointerRel {
            dx: 100_000,
            dy: -50,
        })
        .expect("motion");

    ctl.release_stalls();
    // 4095 splits into 32 full reports plus a remainder of 31: 33 reports after the gate.
    ctl.wait_for_frames(34);

    let frames = ctl.frames();
    assert_eq!(frames.len(), 34, "the run is bounded, not unbounded");
    let dx: i32 = frames[1..]
        .iter()
        .map(|f| i32::from(f.payload[2] as i8))
        .sum();
    let dy: i32 = frames[1..]
        .iter()
        .map(|f| i32::from(f.payload[3] as i8))
        .sum();
    assert_eq!(
        (dx, dy),
        (4095, -50),
        "the capped axis stops at the cap; an axis inside it is untouched"
    );

    let stats = producer.stats();
    assert_eq!(
        stats.rel_truncated, 1,
        "one axis was clamped, counted once for the run"
    );
    assert_eq!(stats.rel_reports, 34);
    let _ = writer.shutdown();
}
