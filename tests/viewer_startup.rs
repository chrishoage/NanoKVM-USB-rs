//! Process-level checks that invalid device selection fails before opening a viewer.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use nanokvm::serial::fake::{Behaviour, FakeCh9329};

/// A path that cannot exist, used for both node kinds. not `/dev/video0`–`3` or
/// `/dev/ttyACM0`: those are the user's hardware (CLAUDE.md), and a test must never open them
/// even to watch it fail.
const ABSENT: &str = "/dev/nanokvm-does-not-exist";

/// Long enough for the 2 s startup link wait plus process spawn, short enough that a hang is a
/// failure rather than a wait.
const BOUND: Duration = Duration::from_secs(20);

fn run(args: &[&str]) -> (std::process::Output, Duration) {
    let started = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_nanokvm"))
        .args(args)
        // The window is never reached in these cases, but if a regression reached it, this keeps
        // it from finding a compositor and hanging around.
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .output()
        .expect("run the nanokvm binary");
    (output, started.elapsed())
}

#[test]
fn a_serial_node_that_is_not_there_is_a_reported_failure_rather_than_a_window() {
    let (out, elapsed) = run(&["--serial", ABSENT, "--video", ABSENT]);
    assert!(
        !out.status.success(),
        "a missing dongle must not be a successful start"
    );
    assert!(
        elapsed < BOUND,
        "startup must give up promptly, took {elapsed:?}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(ABSENT),
        "the message must name the node that was tried:\n{stderr}"
    );
    // The wording that tells a user what to do about it. Losing it to the
    // reconnect rewrite would be a silent regression in the only message they get.
    assert!(
        stderr.contains("dialout") || stderr.contains("uucp"),
        "the permissions hint must survive:\n{stderr}"
    );
}

#[test]
fn a_video_node_that_is_not_there_is_still_fatal_at_startup() {
    // Let serial startup succeed so the test isolates the capture-open failure.
    // Initial failure must exit; only an established viewer attempts recovery.
    let fake = FakeCh9329::spawn(Behaviour::default()).expect("spawn the fake CH9329");
    let port = fake.slave_path();
    let port = port.to_str().expect("pty path is utf-8");
    let (out, elapsed) = run(&["--serial", port, "--video", ABSENT]);

    assert!(
        !out.status.success(),
        "a wrong --video must fail loudly, not open a window onto nothing"
    );
    assert!(elapsed < BOUND, "took {elapsed:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(ABSENT) && stderr.contains("MJPEG"),
        "the message must name the node and the mode that was asked for:\n{stderr}"
    );
    // And it got past serial: the CH9329 handshake is logged before capture is opened, so its
    // absence here would mean this test proved nothing about the capture path.
    assert!(
        stderr.contains("CH9329 firmware"),
        "the serial link must have come up first:\n{stderr}"
    );
}

#[test]
fn the_absent_node_really_is_absent() {
    // If something ever creates this path, both tests above would start passing for the wrong
    // reason, and one of them opens whatever is there.
    assert!(
        !Path::new(ABSENT).exists(),
        "{ABSENT} exists; pick another name"
    );
}
