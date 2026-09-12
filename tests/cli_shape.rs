//! The command-line surface of Stage 3 (plan §12, §8, §10.2), run against the built binary.
//!
//! Two things here are contract rather than cosmetics, and both are invisible to a unit test:
//!
//! - **A subcommand opens only the node it needs and never starts the viewer.** Nothing in the
//!   library would notice if `key` began requiring a video node, or if `shot` brought up the
//!   input writer; the evidence has to come from running the binary and reading what it says.
//! - **`devices --probe` opens only a serial node discovery paired, or the one `--serial` named.**
//!   On this desk `/dev/ttyACM0` is an unrelated device on the user's hub (CLAUDE.md), so "which
//!   node would be opened" is tested as a pure function in `cli::devices`, and what this file
//!   pins is that the probe's traffic is exactly one `GET_INFO` and three `GET_USB_STRING` — no
//!   keyboard or pointer frame reaches a device whose identity is still being established.
//!
//! No hardware and no display. The only device any test here opens is the pty fake; every other
//! path is one that cannot exist, and discovery is pointed at a **recorded** desk with
//! `--sysfs-root` so that these runs enumerate a fixture rather than whatever is plugged into the
//! machine. `/dev/video0`–`3` and `/dev/ttyACM0` are the user's hardware and are never named, not
//! even as a path expected to fail.

#[path = "support/child_guard.rs"]
mod child_guard;

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use nanokvm::proto::cmd;
use nanokvm::serial::fake::{Behaviour, FakeCh9329, ERR_PARAM};

use child_guard::ChildGuard;

/// A path that cannot exist, as in `tests/viewer_startup.rs`, which also guards that it does not.
const ABSENT: &str = "/dev/nanokvm-does-not-exist";

/// The viewer's capture hint, logged just before the window is created. Its absence is how these
/// tests show a subcommand never got near the viewer.
const VIEWER_HINT: &str = "to release capture";

/// Longest any command here may take. Every one of them is a listing, a help text or a handful of
/// probe transactions against a pty; the bound is what keeps a hang in the binary from becoming a
/// hang in the suite.
const EXIT_BOUND: Duration = Duration::from_secs(15);

/// The recorded desk these runs enumerate instead of the developer's own — see the module docs
/// and `cli::sources`.
fn fixture_root() -> PathBuf {
    nanokvm::discovery::testing::fixture("usb2-desk")
        .root()
        .to_path_buf()
}

/// What one run of the binary produced.
struct Ran {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

/// Run the binary against the fixture desk, bounded, killing it if it overruns.
///
/// Every child goes through [`ChildGuard`], whose `Drop` kills and reaps: `Command::output()` waits
/// for as long as the child cares to run, and an assertion that unwinds past a live child leaves
/// it running.
fn run(args: &[&str]) -> Ran {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nanokvm"));
    cmd.args(args)
        .arg("--sysfs-root")
        .arg(fixture_root())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // If a regression ever reached the window, this keeps it from finding a compositor and
        // hanging around.
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY");
    let mut guard = ChildGuard::spawn("nanokvm", &mut cmd).expect("spawn the nanokvm binary");

    // Taken before the wait, because reaping the child takes the pipes with it; read after it,
    // because reading a live child's output to EOF is an unbounded wait. Everything these
    // commands print is a handful of lines, well inside a pipe buffer.
    let (mut out, mut err) = (guard.take_stdout(), guard.take_stderr());
    let Some(status) = guard.wait_bounded(EXIT_BOUND).expect("poll the child") else {
        panic!("nanokvm {args:?} did not exit within {EXIT_BOUND:?}");
    };
    let (mut stdout, mut stderr) = (String::new(), String::new());
    if let Some(o) = out.as_mut() {
        let _ = o.read_to_string(&mut stdout);
    }
    if let Some(e) = err.as_mut() {
        let _ = e.read_to_string(&mut stderr);
    }
    Ran {
        status,
        stdout,
        stderr,
    }
}

fn stdout(out: &Ran) -> String {
    out.stdout.clone()
}

fn stderr(out: &Ran) -> String {
    out.stderr.clone()
}

// ---- shape ---------------------------------------------------------------------------------

#[test]
fn the_help_lists_every_subcommand_and_still_describes_the_viewer() {
    let out = run(&["--help"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    for command in ["devices", "shot", "key", "type", "macro"] {
        assert!(text.contains(command), "no {command} in:\n{text}");
    }
    assert!(
        text.contains("Capture and release:"),
        "the viewer is still what `nanokvm` with no subcommand does:\n{text}"
    );
}

/// §10.2: the layout belongs to the *target* and the host cannot observe it, so the help must say
/// so and must state the default rather than leave the user to assume one.
#[test]
fn the_type_help_states_the_default_layout_and_whose_layout_it_is() {
    let out = run(&["type", "--help"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("[default: us]"), "{text}");
    assert!(text.contains("TARGET"), "{text}");
    // The grammar the compiler implements, shared verbatim with `src/script/`.
    assert!(text.contains("CHORD grammar"), "{text}");
}

#[test]
fn the_devices_help_works() {
    let out = run(&["devices", "--help"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("--probe"), "{}", stdout(&out));
}

/// A viewer flag in front of a subcommand is a mistake, not a no-op: `--pointer` means nothing to
/// `devices`, and silently ignoring it would leave the user believing it took effect. The error
/// names the flag, because "unexpected argument" would leave the user guessing which one.
#[test]
fn a_viewer_flag_with_a_subcommand_is_a_usage_error() {
    for (argv, flag) in [
        (vec!["--pointer", "relative", "devices"], "--pointer"),
        (vec!["devices", "--pointer", "relative"], "--pointer"),
        // §12 Stage 4a's flag is a viewer flag like the others: `devices` opens no card, so
        // "do not open the card" is meaningless to it and saying nothing would be worse.
        (vec!["--no-audio", "devices"], "--no-audio"),
        (vec!["devices", "--no-audio"], "--no-audio"),
    ] {
        let out = run(&argv);
        assert_eq!(out.status.code(), Some(2), "{argv:?}: {}", stderr(&out));
        assert!(stderr(&out).contains(flag), "{}", stderr(&out));
        assert!(stderr(&out).contains("devices"), "{}", stderr(&out));
    }
}

/// A global written **before** the subcommand is how most people type it, and it is not a mistake:
/// `nanokvm --serial X devices` and `nanokvm devices --serial X` are the same command. The first
/// used to be a usage error, because the setting that refused a viewer flag refused these too.
#[test]
fn a_shared_global_works_on_either_side_of_the_subcommand() {
    for argv in [
        vec!["--serial", ABSENT, "--video", ABSENT, "devices"],
        vec!["devices", "--serial", ABSENT, "--video", ABSENT],
    ] {
        let out = run(&argv);
        assert!(out.status.success(), "{argv:?}: {}", stderr(&out));
        assert!(
            stdout(&out).contains(&format!("selection: {ABSENT} + {ABSENT}")),
            "{argv:?}:\n{}",
            stdout(&out)
        );
    }
}

/// The listing is discovery's answer for the tree it was pointed at: with the recorded desk, the
/// dongle's two nodes, paired. No node is opened to produce it (`--sysfs-root` also stops the
/// `QUERYCAP`), so the capture node is reported as unprobed rather than claimed.
#[test]
fn the_listing_names_the_pair_of_the_recorded_desk() {
    let out = run(&["devices"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("/dev/video4 + /dev/ttyACM1"),
        "the recorded desk's pair must be in the listing:\n{text}"
    );
    assert!(
        text.contains("/dev/ttyACM0"),
        "the listing is everything found, including the user's unrelated node:\n{text}"
    );
    assert!(
        !text.contains("/dev/video9"),
        "the listing must come from the fixture, not from this machine:\n{text}"
    );
}

/// §12 Stage 4a: `nanokvm devices` prints the paired card. The recorded desk's dongle carries
/// `card8` on the same USB device as `/dev/video4`, and the listing says so with the evidence —
/// which is the strongest §8 has, and the one the video-and-serial pairing cannot have here.
#[test]
fn the_listing_names_the_paired_sound_card_and_the_evidence_for_it() {
    let out = run(&["devices"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("sound cards:"), "{text}");
    assert!(
        text.contains("card8 \"Video\" (hw:8)"),
        "the ALSA name the client would open must be in the listing:\n{text}"
    );
    assert!(
        text.contains("audio: card8"),
        "the pair must say which card is its own:\n{text}"
    );
    assert!(
        text.contains("same USB device as the capture node (busnum:devnum) — proof"),
        "the audio pairing states its evidence, which is §8's strongest:\n{text}"
    );
    // The webcam's card is in the same recording and belongs to someone else's hardware.
    assert!(
        text.contains("card7"),
        "the listing is everything found:\n{text}"
    );
    assert!(
        !text.contains("audio: card7"),
        "no pair may claim the webcam's microphone:\n{text}"
    );
}

/// And a desk with no sound card at all says "no audio" honestly rather than leaving the line out.
/// The SuperSpeed reconstruction is exactly that desk: `topology.md` recorded the dongle's two
/// device nodes and nothing about its audio interface, so the tree has no card to find (see
/// `fixtures/sysfs/MANIFEST.md`). The pairing must be unaffected, which is §4.1 rev 5's whole
/// point about audio being a side channel.
#[test]
fn a_desk_with_no_sound_card_says_no_audio_rather_than_leaving_the_line_out() {
    let root = nanokvm::discovery::testing::fixture("usb3-stage0")
        .root()
        .to_path_buf();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nanokvm"));
    cmd.args(["devices", "--sysfs-root"])
        .arg(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY");
    let mut guard = ChildGuard::spawn("nanokvm", &mut cmd).expect("spawn the nanokvm binary");
    let mut out = guard.take_stdout();
    let status = guard
        .wait_bounded(EXIT_BOUND)
        .expect("poll the child")
        .expect("devices must exit promptly");
    let mut text = String::new();
    if let Some(o) = out.as_mut() {
        let _ = o.read_to_string(&mut text);
    }

    assert!(status.success(), "{text}");
    assert!(text.contains("sound cards:\n  (none)"), "{text}");
    assert!(
        text.contains("audio: no audio: no sound card belongs to the capture node's USB device"),
        "the pair still pairs and says honestly that it has no audio:\n{text}"
    );
    assert!(
        text.contains("/dev/video4 + /dev/ttyACM1"),
        "an absent card changes nothing about the pairing:\n{text}"
    );
}

// ---- devices -------------------------------------------------------------------------------

/// The listing answers the command line it was given (review finding 7): both paths are named,
/// neither is a node discovery enumerated, and both are still what would be used (§8).
#[test]
fn devices_honours_the_flags_on_its_own_command_line_and_exits_zero() {
    let out = run(&["devices", "--serial", ABSENT, "--video", ABSENT]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert_eq!(
        text.matches("is not a node discovery enumerated").count(),
        2,
        "both flags must be called out:\n{text}"
    );
    assert!(
        text.contains(&format!("selection: {ABSENT} + {ABSENT}")),
        "the listing must say what the flags select:\n{text}"
    );
}

/// `--probe` prints what the link says about itself — and asks nothing else. An unidentified
/// serial port must never be sent an input report (§3.4: an ack is not evidence of effect, and
/// here it would be evidence written into someone else's device).
#[test]
fn a_probe_reports_the_firmware_and_the_usb_strings_and_sends_nothing_else() {
    let fake = FakeCh9329::spawn(Behaviour::default()).expect("spawn the fake CH9329");
    let port = fake.slave_path();
    let out = run(&[
        "devices",
        "--probe",
        "--serial",
        port.to_str().expect("pty path is utf-8"),
        "--video",
        ABSENT,
    ]);
    assert!(out.status.success(), "{}", stderr(&out));

    let text = stdout(&out);
    assert!(text.contains("CH9329 firmware 1.8"), "{text}");
    assert!(text.contains("target connected"), "{text}");
    assert!(
        text.contains("locks: num=off caps=off scroll=off"),
        "{text}"
    );
    assert!(text.contains(r#"manufacturer "Sipeed""#), "{text}");
    assert!(text.contains(r#"product "NanoKVM-USB""#), "{text}");
    assert!(text.contains(r#"serial "BA1612624UJPW2RUJ""#), "{text}");

    let frames = fake.received();
    assert_eq!(
        frames.iter().filter(|f| f.cmd == cmd::GET_INFO).count(),
        1,
        "one GET_INFO: {frames:?}"
    );
    assert_eq!(
        frames
            .iter()
            .filter(|f| f.cmd == cmd::GET_USB_STRING)
            .count(),
        3,
        "one GET_USB_STRING per string: {frames:?}"
    );
    // Written as "nothing but those two commands" rather than as a list of the input commands,
    // so that a command added later is caught too — and so this file names no pointer command.
    assert!(
        frames
            .iter()
            .all(|f| f.cmd == cmd::GET_INFO || f.cmd == cmd::GET_USB_STRING),
        "a probe must send no input report: {frames:?}"
    );
    fake.stop();
}

/// A device that answers but will not say what it is, is a failed probe: the listing still
/// printed, so the table is not lost, but the exit status says the question was not answered.
#[test]
fn a_probe_that_is_refused_names_the_command_and_exits_one() {
    let fake = FakeCh9329::spawn(Behaviour {
        reject: vec![(cmd::GET_USB_STRING, ERR_PARAM)],
        ..Behaviour::default()
    })
    .expect("spawn the fake CH9329");
    let port = fake.slave_path();
    let out = run(&[
        "devices",
        "--probe",
        "--serial",
        port.to_str().expect("pty path is utf-8"),
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("probe failed:"), "{text}");
    assert!(
        text.contains("GET_USB_STRING"),
        "the failing command must be named:\n{text}"
    );
    assert!(
        text.contains("video nodes:"),
        "the listing still printed:\n{text}"
    );
    fake.stop();
}

// ---- the subcommands open one node, and never the viewer ------------------------------------

/// `shot` needs the video node and nothing else, and it must not start the viewer on the way.
#[test]
fn shot_resolves_only_the_video_node_and_starts_no_viewer() {
    let out = run(&["shot", "--video", ABSENT]);
    assert!(!out.status.success(), "{}", stdout(&out));
    let text = stderr(&out);
    assert!(
        text.contains(ABSENT),
        "the message must name the path:\n{text}"
    );
    assert!(
        !text.contains(VIEWER_HINT),
        "a subcommand must not start the viewer:\n{text}"
    );
}

/// `key --serial …` must work without `--video`: §8's override names the one node this command
/// opens, and there is nothing left to infer. Reaching the *open* is the proof — a `key` that
/// still demanded a pair would have died in discovery instead, naming the video node it does not
/// need.
#[test]
fn key_needs_no_video_node_and_starts_no_viewer() {
    let out = run(&["key", "--serial", ABSENT, "a"]);
    assert!(!out.status.success(), "{}", stdout(&out));
    let text = stderr(&out);
    assert!(
        text.contains(ABSENT),
        "the message must name the path:\n{text}"
    );
    assert!(
        text.contains("opening the CH9329 serial link"),
        "device selection must have got as far as the command opening its one node:\n{text}"
    );
    assert!(
        !text.contains(VIEWER_HINT),
        "a subcommand must not start the viewer:\n{text}"
    );
}
