//! Six-key HID projection and suppression until the physical key is released.

use std::time::Duration;

use nanokvm::input::testing::{fake_link, gate_frame, kb_frame, release_frames_rel, stall_writer};
use nanokvm::input::{spawn, Config, Event, ReleaseReason};
use nanokvm::proto::report::{modifier, HidKey};

const WAIT: Duration = Duration::from_secs(10);

fn config() -> Config {
    Config {
        max_barriers: 64,
        transact_timeout: Duration::from_millis(50),
        // Slice A added `Config::reconnect`; these paths use `spawn`, which never reconnects.
        ..Config::default()
    }
}

fn press(u: u8) -> Event {
    Event::Key {
        key: HidKey::Usage(u),
        down: true,
    }
}

fn release(u: u8) -> Event {
    Event::Key {
        key: HidKey::Usage(u),
        down: false,
    }
}

/// Seven keys pressed in order: the report carries the first six, and the seventh appears in no
/// report at all — pressing it changed nothing observable.
#[test]
fn a_seventh_key_never_appears_in_any_report() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    for usage in 4..=10u8 {
        producer.submit(press(usage)).expect("key");
    }

    ctl.release_stalls();
    ctl.wait_for_frames(7);

    let frames = ctl.frames();
    assert_eq!(
        frames,
        vec![
            gate_frame(),
            kb_frame(0, [4, 0, 0, 0, 0, 0]),
            kb_frame(0, [4, 5, 0, 0, 0, 0]),
            kb_frame(0, [4, 5, 6, 0, 0, 0]),
            kb_frame(0, [4, 5, 6, 7, 0, 0]),
            kb_frame(0, [4, 5, 6, 7, 8, 0]),
            kb_frame(0, [4, 5, 6, 7, 8, 9]),
        ]
    );
    // The seventh key (usage 10) is in no frame's payload, ever.
    assert!(frames.iter().all(|f| !f.payload.contains(&10u8)));
    let _ = writer.shutdown();
}

/// Releasing one of the six frees a slot and the report is rebuilt from the remaining `Reported`
/// keys: five, not six. The suppressed key is not promoted.
#[test]
fn releasing_one_of_six_yields_a_five_key_report() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    for usage in 4..=10u8 {
        producer.submit(press(usage)).expect("key");
    }
    producer.submit(release(4)).expect("key");

    ctl.release_stalls();
    ctl.wait_for_frames(8);

    let frames = ctl.frames();
    assert_eq!(
        frames.len(),
        8,
        "the suppressed key must not produce a frame of its own"
    );
    assert_eq!(frames[7], kb_frame(0, [5, 6, 7, 8, 9, 0]));
    assert!(frames.iter().all(|f| !f.payload.contains(&10u8)));
    let _ = writer.shutdown();
}

/// A suppressed key becomes eligible again only through a fresh press: release it, press it
/// again while five are `Reported`, and it enters a slot.
#[test]
fn a_suppressed_key_enters_a_slot_on_a_fresh_press() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    for usage in 4..=10u8 {
        producer.submit(press(usage)).expect("key");
    }
    producer.submit(release(4)).expect("key"); // frees a slot; five reported
    producer.submit(release(10)).expect("key"); // the suppressed key: no frame
    producer.submit(press(10)).expect("key"); // fresh press against five reported

    ctl.release_stalls();
    ctl.wait_for_frames(9);

    let frames = ctl.frames();
    assert_eq!(frames.len(), 9);
    assert_eq!(frames[7], kb_frame(0, [5, 6, 7, 8, 9, 0]));
    assert_eq!(frames[8], kb_frame(0, [5, 6, 7, 8, 9, 10]));
    let _ = writer.shutdown();
}

/// Pressing a key while six are `Reported` emits no report at all — nothing observable changed
/// . The frame either side of it proves the writer did not stall or reorder.
#[test]
fn pressing_a_key_while_six_are_reported_emits_nothing() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    for usage in 4..=9u8 {
        producer.submit(press(usage)).expect("key");
    }
    producer.submit(press(11)).expect("key"); // suppressed: no frame
    producer.submit(press(12)).expect("key"); // suppressed: no frame
    producer
        .submit(Event::Key {
            key: HidKey::Modifier(modifier::LEFT_CTRL),
            down: true,
        })
        .expect("modifier");

    ctl.release_stalls();
    ctl.wait_for_frames(8);

    let frames = ctl.frames();
    assert_eq!(frames.len(), 8);
    // Six key frames, then the modifier frame — and the modifier does not occupy a slot.
    assert_eq!(frames[6], kb_frame(0, [4, 5, 6, 7, 8, 9]));
    assert_eq!(frames[7], kb_frame(modifier::LEFT_CTRL, [4, 5, 6, 7, 8, 9]));
    let _ = writer.shutdown();
}

/// A duplicate key-down for a key already held emits nothing. The viewer discards host repeats
/// ; this is the second line of defence.
#[test]
fn a_duplicate_key_down_emits_nothing() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    producer.submit(press(4)).expect("key");
    for _ in 0..20 {
        producer.submit(press(4)).expect("host repeat");
    }
    producer.submit(press(5)).expect("key");

    ctl.release_stalls();
    ctl.wait_for_frames(3);

    assert_eq!(
        ctl.frames(),
        vec![
            gate_frame(),
            kb_frame(0, [4, 0, 0, 0, 0, 0]),
            kb_frame(0, [4, 5, 0, 0, 0, 0])
        ]
    );
    let _ = writer.shutdown();
}

/// A key-up for a key that is not held — what every key-up looks like after a release-all —
/// emits nothing.
#[test]
fn a_key_up_for_an_unheld_key_emits_nothing() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    producer.submit(release(4)).expect("key");
    producer.submit(press(5)).expect("key");

    ctl.release_stalls();
    ctl.wait_for_frames(2);

    assert_eq!(
        ctl.frames(),
        vec![gate_frame(), kb_frame(0, [5, 0, 0, 0, 0, 0])]
    );
    let _ = writer.shutdown();
}

/// Release clears suppression so the next press can use the first available slot.
#[test]
fn release_all_zeroes_the_report_and_empties_the_set() {
    let (link, ctl) = fake_link();
    let (producer, writer) = spawn(link, config());
    stall_writer(&producer, &ctl);

    for usage in 4..=10u8 {
        producer.submit(press(usage)).expect("key");
    }
    producer.request_release_all(ReleaseReason::FocusLost);
    ctl.release_stalls();
    assert!(
        producer.wait_until_engageable(WAIT),
        "the writer must acknowledge the release"
    );

    let after_release = ctl.frame_count();
    let frames = ctl.frames();
    assert_eq!(&frames[frames.len() - 2..], release_frames_rel().as_slice());

    // The set is empty: usage 10 was suppressed before the release, and now it is simply a key.
    producer.engage().expect("engage after the ack");
    producer.submit(press(10)).expect("key");
    ctl.wait_for_frames(after_release + 1);
    assert_eq!(
        ctl.frames()[after_release],
        kb_frame(0, [10, 0, 0, 0, 0, 0])
    );
    let _ = writer.shutdown();
}
