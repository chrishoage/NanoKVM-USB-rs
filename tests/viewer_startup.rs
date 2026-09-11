//! The binary's startup policy (plan §2.7, §6.1 S1-1 and S2-4, §8, §12 Stage 2).
//!
//! Stage 2 gives both subsystems a *source* of devices instead of one device, so a failure after
//! startup is recovered from rather than fatal. The decision this file pins is the other half of
//! that: **startup itself stays fail-fast**. A dongle that is not there, or a `--video` that
//! names the wrong node, must be a message the user reads on the terminal — not a window that
//! silently never types and never shows a frame while a reopen loop turns over behind it.
//!
//! It is worth a test because the failure mode is invisible: swapping `Pipeline::start` for
//! `Pipeline::start_with_opener`, or `input::spawn` for `spawn_with_source`, makes a missing
//! device *stop being an error* — the program still starts, still exits 0 at `--exit-after`, and
//! nothing in the unit tests notices. The evidence has to come from running the real binary.
//!
//! No hardware and no display: both cases fail before the window is created, one against a
//! non-existent path and one against the pty fake.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use nanokvm::serial::fake::{Behaviour, FakeCh9329};

/// A path that cannot exist, used for both node kinds. Deliberately not `/dev/video0`–`3` or
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
    // The Stage 1 wording that tells a user what to actually do about it. Losing it to the
    // reconnect rewrite would be a silent regression in the only message they get.
    assert!(
        stderr.contains("dialout") || stderr.contains("uucp"),
        "the permissions hint must survive:\n{stderr}"
    );
}

#[test]
fn a_video_node_that_is_not_there_is_still_fatal_at_startup() {
    // The serial half succeeds against the fake, so the only thing left to fail is the capture
    // open — which is the decision under test: §6.1 S1-1 says a capture failure *after* startup
    // must not tear down input, and this is why that is not extended backwards to startup.
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
        "the serial link must have come up first (§6.1 S1-1):\n{stderr}"
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
