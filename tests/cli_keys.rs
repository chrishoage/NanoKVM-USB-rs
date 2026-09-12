//! Keyboard commands exercised as child processes against a pseudo-terminal bridge.
//!
//! Checks cover validation before device access, report order, partial delivery, and
//! release on errors and signals. Source checks exclude mouse commands from CLI scripts.

#[path = "support/child_guard.rs"]
mod child_guard;

use std::io::{Read, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use winit::keyboard::KeyCode;

use nanokvm::proto::frame::Frame;
use nanokvm::proto::keymap::hid_key;
use nanokvm::proto::report::modifier;
use nanokvm::proto::{cmd, HidKey};
use nanokvm::script::Fixtures;
use nanokvm::serial::fake::{Behaviour, FakeCh9329, DEFAULT_GET_INFO_PAYLOAD, ERR_CHECKSUM};

use child_guard::ChildGuard;

/// A path that cannot exist, as in `tests/viewer_startup.rs`, which also guards that it does not.
/// Given as `--video` so device selection is decided by `--serial` alone and never by what is
/// plugged into the recorded test setup.
const ABSENT: &str = "/dev/nanokvm-does-not-exist";

/// Longest a command under test may take to exit. Generous: the slowest run here is twenty
/// reports at 40 ms plus a 500 ms transaction timeout.
const EXIT_BOUND: Duration = Duration::from_secs(15);

/// Longest a test waits for the device to see something it expects.
const FRAME_BOUND: Duration = Duration::from_secs(5);

/// The bit `GET_INFO` sets when the target reports CapsLock on (`DeviceInfo::parse`: payload byte
/// 2, bit 1).
const CAPS_LOCK_BIT: u8 = 0b010;

// ---------------------------------------------------------------------------
// Running the binary against the fake.
// ---------------------------------------------------------------------------

/// The recorded desk these runs enumerate instead of the developer's own.
///
/// Without it the binary walks the real `/sys` and `QUERYCAP`s whatever video nodes are plugged
/// in — on the recorded test setup the dongle and the user's webcam (CLAUDE.md). `--sysfs-root` points discovery
/// at the fixture and stops it opening any node (`cli::sources`).
fn fixture_root() -> std::path::PathBuf {
    nanokvm::discovery::testing::fixture("usb2-desk")
        .root()
        .to_path_buf()
}

/// `nanokvm <args> --serial <the fake's pty> --video <a path that cannot exist>`.
///
/// The globals go after the subcommand here; either side works (H4), and `tests/cli_shape.rs`
/// pins that.
fn command(fake: &FakeCh9329, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nanokvm"));
    cmd.args(args)
        .arg("--serial")
        .arg(fake.slave_path())
        .arg("--video")
        .arg(ABSENT)
        .arg("--sysfs-root")
        .arg(fixture_root())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // If a regression ever reached the window, this keeps it from finding a compositor and
        // hanging around.
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY");
    cmd
}

fn spawn(fake: &FakeCh9329, args: &[&str]) -> ChildGuard {
    ChildGuard::spawn("nanokvm", &mut command(fake, args)).expect("spawn nanokvm")
}

/// Wait out the child within [`EXIT_BOUND`] and collect everything it said, stderr first.
///
/// The pipes are taken before the wait, because reaping the child takes them with it — and
/// they are read only after it, because reading a live child's output to EOF would be an
/// unbounded wait. Everything these commands print is a handful of lines, well inside a pipe
/// buffer, so nothing blocks in between.
fn finish(guard: &mut ChildGuard) -> (ExitStatus, String) {
    let (mut stdout, mut stderr) = (guard.take_stdout(), guard.take_stderr());
    let Some(status) = guard.wait_bounded(EXIT_BOUND).expect("poll the child") else {
        // The guard's `Drop` kills and reaps it as this unwinds.
        panic!("nanokvm did not exit within {EXIT_BOUND:?}");
    };
    let mut output = String::new();
    if let Some(e) = stderr.as_mut() {
        let _ = e.read_to_string(&mut output);
    }
    if let Some(o) = stdout.as_mut() {
        let _ = o.read_to_string(&mut output);
    }
    (status, output)
}

fn keyboard_frames(fake: &FakeCh9329) -> Vec<Frame> {
    fake.received()
        .into_iter()
        .filter(|f| f.cmd == cmd::SEND_KB_GENERAL_DATA)
        .collect()
}

fn is_release_all(f: &Frame) -> bool {
    f.data == vec![0u8; 8]
}

/// How many keyboard frames a script of `reports` reports puts on the wire: its own, plus the
/// release-all the `Released` guard sends on the way out.
///
/// The guard runs on *every* exit path including the successful one, so a clean run always
/// ends with one more release-all than the script itself contains. Spelled out here because a
/// test that quietly expected one fewer would be asserting the guard away.
fn with_the_guards_release(reports: usize) -> usize {
    reports + 1
}

/// Wait until `cond` holds or `limit` elapses. No sleep-as-synchronisation.
fn wait_until(limit: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    cond()
}

fn signal(guard: &ChildGuard, signal: libc::c_int) {
    let pid = guard.pid().expect("the child is still owned") as libc::pid_t;
    // SAFETY: `kill` with a pid the guard still owns, so it has not been reaped and the pid
    // cannot have been reused.
    unsafe {
        libc::kill(pid, signal);
    }
}

/// A `GET_INFO` payload like the fake's default, with the lock bits replaced.
fn locks(bits: u8) -> Vec<u8> {
    let mut payload = DEFAULT_GET_INFO_PAYLOAD;
    payload[2] = bits;
    payload.to_vec()
}

/// A macro file under cargo's own per-test temp directory, which is inside `target/` and so is
/// both writable and gitignored.
fn macro_file(name: &str, body: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("cli_keys");
    std::fs::create_dir_all(&dir).expect("create the temp directory");
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write the macro file");
    path
}

/// The usage `proto::keymap` forwards for a key. Never retyped: one authority for usages.
fn usage(key: KeyCode) -> u8 {
    match hid_key(key) {
        Some(HidKey::Usage(usage)) => usage,
        other => panic!("{key:?} has no usage in proto::keymap: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The safety machinery: the gate, the guard, the three signals.
// ---------------------------------------------------------------------------

/// SIGINT mid-script must leave no key held: the run aborts and the `Drop` guard's release-all is
/// the last thing on the wire.
///
/// Fails if the guard never runs, if the interrupt is only noticed after the whole script, or if
/// the last frame the device saw was a press.
#[test]
fn sigint_midscript_leaves_no_key_held() {
    let fake = FakeCh9329::spawn(Behaviour::default()).expect("spawn fake");
    // 20 reports at the default 40 ms pacing: ~800 ms, long enough to interrupt in the middle.
    let mut child = spawn(&fake, &["type", "aaaaaaaaaa"]);

    assert!(
        wait_until(FRAME_BOUND, || keyboard_frames(&fake).len() >= 3),
        "the script never started sending"
    );
    signal(&child, libc::SIGINT);
    let (status, output) = finish(&mut child);

    let frames = keyboard_frames(&fake);
    let presses = frames.iter().filter(|f| !is_release_all(f)).count();
    let releases = frames.iter().filter(|f| is_release_all(f)).count();

    assert!(
        frames.len() < 20,
        "SIGINT did not abort the script: all {} reports were delivered\n{output}",
        frames.len()
    );
    assert!(
        frames.last().is_some_and(is_release_all),
        "the last keyboard report the device saw was a press, not a release-all: {:?}\n{output}",
        frames.last()
    );
    assert!(
        releases >= presses,
        "{presses} presses but only {releases} releases reached the device\n{output}"
    );
    assert!(
        !status.success(),
        "an interrupted script exited 0, so a caller cannot tell it was cut short\n{output}"
    );
    assert!(
        output.contains(" of ") && output.contains("reports"),
        "the abort did not say how far the script got\n{output}"
    );
}

/// A key held when the process is terminated by anything other than SIGINT must not stay held.
///
/// A harness that shells out to `nanokvm type` with a timeout sends SIGTERM; closing the terminal
/// sends SIGHUP. Taking the default disposition for either kills the process between a press and
/// its release, with no `Drop` and no release-all on the wire — a key held down on a live console.
///
/// Fails while the last frame the device saw is a press.
#[test]
fn sigterm_midscript_leaves_no_key_held() {
    a_signal_midscript_leaves_no_key_held(libc::SIGTERM);
}

/// The same property for SIGHUP. A terminal closing under a live script is the everyday way this
/// happens, and it used to be fatal in the same way SIGTERM was.
#[test]
fn sighup_midscript_leaves_no_key_held() {
    a_signal_midscript_leaves_no_key_held(libc::SIGHUP);
}

fn a_signal_midscript_leaves_no_key_held(sig: libc::c_int) {
    let fake = FakeCh9329::spawn(Behaviour::default()).expect("spawn fake");
    // 300 ms between reports, so a signal delivered "at random" lands inside a press/release gap
    // rather than needing to hit a 40 ms window.
    let mut child = spawn(&fake, &["type", "aaaaaa", "--delay-ms", "300"]);

    assert!(
        wait_until(FRAME_BOUND, || {
            keyboard_frames(&fake)
                .last()
                .is_some_and(|f| !is_release_all(f))
        }),
        "the script never got as far as a press"
    );
    signal(&child, sig);
    let (_status, output) = finish(&mut child);

    let frames = keyboard_frames(&fake);
    assert!(
        frames.last().is_some_and(is_release_all),
        "signal {sig} killed the process with a key pressed and no release-all followed it. Last \
         report the device saw: {:?}\n{output}",
        frames.last().map(|f| f.data.clone())
    );
}

/// `target_connected == false` must send nothing at all — not even the guard's release-all, since
/// the guard is created after the gate.
///
/// Fails if any `SEND_KB_GENERAL_DATA` frame reaches the device when the gate refuses.
#[test]
fn the_get_info_gate_sends_no_keyboard_report() {
    let mut payload = DEFAULT_GET_INFO_PAYLOAD;
    payload[1] = 0x00; // target_connected = false
    let fake = FakeCh9329::spawn(Behaviour {
        get_info_payload: payload.to_vec(),
        ..Behaviour::default()
    })
    .expect("spawn fake");

    let mut child = spawn(&fake, &["type", "sudo reboot"]);
    let (status, output) = finish(&mut child);

    assert!(
        !status.success(),
        "the gate did not refuse a disconnected target\n{output}"
    );
    assert_eq!(
        keyboard_frames(&fake).len(),
        0,
        "keyboard reports went out to a target the device says is not connected\n{output}"
    );
    assert!(
        output.contains("no target on its HID side"),
        "the refusal must say what it saw\n{output}"
    );
}

/// Reject reports mid-script, including cleanup. The CLI must report partial
/// progress and the failed release attempt; a sent release is not a successful one.
#[test]
fn a_midscript_failure_aborts_and_still_releases() {
    let fake = FakeCh9329::spawn(Behaviour::default()).expect("spawn fake");
    let mut child = spawn(&fake, &["type", "abcdef"]);

    assert!(
        wait_until(FRAME_BOUND, || keyboard_frames(&fake).len() >= 2),
        "the script never started sending"
    );
    fake.reject_cmd(cmd::SEND_KB_GENERAL_DATA, ERR_CHECKSUM);

    let (status, output) = finish(&mut child);

    let frames = keyboard_frames(&fake);
    assert!(
        !status.success(),
        "a script whose report was rejected exited 0\n{output}"
    );
    assert!(
        frames.len() < 12,
        "the failure did not abort the script: {} of 12 reports still went out\n{output}",
        frames.len()
    );
    assert!(
        frames.last().is_some_and(is_release_all),
        "the last report the device saw was a press: {:?}\n{output}",
        frames.last()
    );
    assert!(
        output.contains(" of ") && output.contains("reports"),
        "the abort did not say how far the script got\n{output}"
    );
    assert!(
        output.contains("[release-all]"),
        "the release-all's outcome must be reported, not swallowed \n{output}"
    );
}

// ---------------------------------------------------------------------------
// Compile before opening.
// ---------------------------------------------------------------------------

/// `--dry-run` prints the table and opens nothing: not one byte reaches the port.
#[test]
fn a_dry_run_prints_the_table_and_sends_nothing() {
    let fake = FakeCh9329::spawn(Behaviour::default()).expect("spawn fake");
    let mut child = spawn(&fake, &["type", "hello", "--dry-run"]);
    let (status, output) = finish(&mut child);

    assert!(status.success(), "a dry run must exit 0\n{output}");
    assert!(
        output.contains("dry run: 10 reports, nothing sent, no device opened"),
        "{output}"
    );
    assert!(output.contains("type 'h'"), "{output}");

    // Bounded, and it must *stay* empty: a dry run that opened the port late would show up here.
    assert!(
        !wait_until(Duration::from_millis(200), || !fake.received().is_empty()),
        "a dry run reached the device: {:?}",
        fake.received()
    );
    assert!(fake.raw_rx().is_empty(), "{:?}", fake.raw_rx());
}

/// H3: `--dry-run` "opens no device" has to include discovery's own opening. `inventory` runs a
/// `VIDIOC_QUERYCAP` on every candidate video node, so a dry run that resolved devices first would
/// open the dongle's node to print a table of frames it is not going to send.
///
/// Pointed at a `--sysfs-root` that does not exist, discovery cannot succeed at all: the run
/// exiting 0 with its table is the evidence that it was never consulted.
#[test]
fn a_dry_run_resolves_no_device_at_all() {
    let missing = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("no-such-sysfs-root");
    assert!(!missing.exists(), "the point of this path is its absence");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nanokvm"));
    cmd.args(["key", "--dry-run", "a"])
        .arg("--sysfs-root")
        .arg(&missing)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY");
    let mut child = ChildGuard::spawn("nanokvm", &mut cmd).expect("spawn nanokvm");
    let (status, output) = finish(&mut child);

    assert!(
        status.success(),
        "a dry run consulted discovery, which had nothing to answer with\n{output}"
    );
    assert!(
        output.contains("dry run: 2 reports, nothing sent, no device opened"),
        "{output}"
    );
    assert!(output.contains("key a"), "{output}");
}

/// Unsupported text must fail before opening the serial port or sending a preamble.
#[test]
fn an_unreachable_character_is_refused_before_anything_is_opened() {
    let fake = FakeCh9329::spawn(Behaviour::default()).expect("spawn fake");
    let mut child = spawn(&fake, &["type", "héllo"]);
    let (status, output) = finish(&mut child);

    assert_eq!(status.code(), Some(1), "{output}");
    assert!(
        output.contains("'é'"),
        "the message must name the character:\n{output}"
    );
    assert!(
        output.contains("nothing was sent"),
        "the message must say the script was not attempted:\n{output}"
    );
    assert!(
        fake.raw_rx().is_empty(),
        "the port was opened for a script that does not compile: {:?}",
        fake.raw_rx()
    );
}

/// R3: `--layout` is `Option`, so a macro's own `layout` directive can still decide. It used to
/// carry a clap default of `us`, which made every directive but `layout us` report a conflict with
/// a flag the user never typed — and made the directive unusable.
#[test]
fn a_macro_layout_directive_decides_when_the_flag_is_absent() {
    let path = macro_file("layout-directive.macro", "layout us-qwerty\nkey a\n");
    let fake = FakeCh9329::spawn(Behaviour::default()).expect("spawn fake");
    let mut child = spawn(
        &fake,
        &["macro", path.to_str().expect("utf-8 path"), "--dry-run"],
    );
    let (status, output) = finish(&mut child);
    assert!(
        status.success(),
        "the file's own layout directive was refused\n{output}"
    );
    assert!(output.contains("dry run: 2 reports"), "{output}");
}

/// A file layout and an explicitly supplied CLI layout must agree.
#[test]
fn a_macro_layout_directive_is_still_validated_and_still_conflicts() {
    let path = macro_file("layout-de.macro", "layout de\nkey a\n");
    let file = path.to_str().expect("utf-8 path");

    let fake = FakeCh9329::spawn(Behaviour::default()).expect("spawn fake");
    let mut child = spawn(&fake, &["macro", file, "--dry-run"]);
    let (status, output) = finish(&mut child);
    assert_eq!(status.code(), Some(1), "{output}");
    assert!(output.contains("unknown layout"), "{output}");

    let fake = FakeCh9329::spawn(Behaviour::default()).expect("spawn fake");
    let mut child = spawn(&fake, &["macro", file, "--layout", "us", "--dry-run"]);
    let (status, output) = finish(&mut child);
    assert_eq!(status.code(), Some(1), "{output}");
    assert!(output.contains("conflicts with --layout us"), "{output}");
}

// ---------------------------------------------------------------------------
// CapsLock.
// ---------------------------------------------------------------------------

/// The default policy refuses, and refuses *before* the guard exists: a target whose CapsLock is
/// on would receive `HELLO` for `hello`, and silently typing it is the one thing this must not do.
#[test]
fn capslock_on_refuses_to_type_and_sends_no_keyboard_report() {
    let fake = FakeCh9329::spawn(Behaviour {
        get_info_payload: locks(CAPS_LOCK_BIT),
        ..Behaviour::default()
    })
    .expect("spawn fake");
    let mut child = spawn(&fake, &["type", "hello"]);
    let (status, output) = finish(&mut child);

    assert_eq!(status.code(), Some(1), "{output}");
    assert!(output.contains("CapsLock ON"), "{output}");
    assert!(output.contains("nanokvm key capslock"), "{output}");
    assert!(output.contains("--caps-lock ignore|compensate"), "{output}");
    // The port was opened, resynchronised and queried before this refusal, so what it promises is
    // about keyboard reports and not about the wire.
    assert!(output.contains("No keyboard report was sent."), "{output}");
    assert_eq!(
        keyboard_frames(&fake).len(),
        0,
        "the refusal sent reports anyway\n{output}"
    );
}

/// `compensate` inverts the shift bit on letters and says so; `ignore` sends what an
/// unlocked target would have been sent. The two differ in one bit, which is the whole decision.
#[test]
fn capslock_compensate_shifts_a_letter_and_ignore_does_not() {
    for (policy, want_modifiers) in [("compensate", modifier::LEFT_SHIFT), ("ignore", 0u8)] {
        let fake = FakeCh9329::spawn(Behaviour {
            get_info_payload: locks(CAPS_LOCK_BIT),
            ..Behaviour::default()
        })
        .expect("spawn fake");
        let mut child = spawn(&fake, &["type", "h", "--caps-lock", policy]);
        let (status, output) = finish(&mut child);

        assert!(status.success(), "--caps-lock {policy}\n{output}");
        let frames = keyboard_frames(&fake);
        assert_eq!(
            frames.len(),
            with_the_guards_release(2),
            "one press, one release, and the guard's\n{output}"
        );
        assert_eq!(
            frames[0].data[0], want_modifiers,
            "--caps-lock {policy} sent modifiers {:#04x}\n{output}",
            frames[0].data[0]
        );
        assert_eq!(frames[0].data[2], usage(KeyCode::KeyH), "{output}");
        assert!(is_release_all(&frames[1]), "{output}");
        assert_eq!(
            output.contains("compensating for CapsLock"),
            policy == "compensate",
            "--caps-lock {policy} must say whether it changed anything\n{output}"
        );
    }
}

/// A chord names a key, not a character, so the target's CapsLock decides nothing about it — but
/// the lock state is still printed, because it is what the device just said.
#[test]
fn a_chord_is_unaffected_by_the_capslock_policy() {
    let fake = FakeCh9329::spawn(Behaviour {
        get_info_payload: locks(CAPS_LOCK_BIT),
        ..Behaviour::default()
    })
    .expect("spawn fake");
    let mut child = spawn(&fake, &["key", "a"]);
    let (status, output) = finish(&mut child);

    assert!(
        status.success(),
        "a chord must not be refused for the target's CapsLock\n{output}"
    );
    // One wording for the lock bits, whichever command printed them (`DeviceInfo`'s `Display`):
    // `devices --probe` says `caps=on` and so does this.
    assert!(output.contains("caps=on"), "{output}");
    assert!(
        !output.contains("caps=true"),
        "two spellings of one bit:\n{output}"
    );
    let frames = keyboard_frames(&fake);
    assert_eq!(frames.len(), with_the_guards_release(2), "{output}");
    assert_eq!(frames[0].data[2], usage(KeyCode::KeyA), "{output}");
    assert!(is_release_all(&frames[1]), "{output}");
}

// ---------------------------------------------------------------------------
// What goes on the wire.
// ---------------------------------------------------------------------------

/// The command, as a macro file, through the real binary: `ctrl+alt+t`, `sudo reboot`,
/// Enter.
///
/// Two things are pinned. The frames, against the authority: every payload
/// `fixtures/packets/ch9329.toml` describes must arrive as the bytes the file gives, and the
/// bytes are read from it rather than retyped. The US-QWERTY table, as the modifier and usage
/// of every character in `sudo reboot` (HID usage tables ch. 10), so a later edit to the table
/// cannot quietly change what is typed at a root prompt.
#[test]
fn the_us_qwerty_path_types_the_reboot_command_in_order() {
    // (modifiers, usage) per character of "sudo reboot", US QWERTY.
    let text: &[(u8, u8)] = &[
        (0x00, 0x16), // s
        (0x00, 0x18), // u
        (0x00, 0x07), // d
        (0x00, 0x12), // o
        (0x00, 0x2c), // space
        (0x00, 0x15), // r
        (0x00, 0x08), // e
        (0x00, 0x05), // b
        (0x00, 0x12), // o
        (0x00, 0x12), // o
        (0x00, 0x17), // t
    ];
    let mut want: Vec<(u8, u8)> = vec![(
        modifier::LEFT_CTRL | modifier::LEFT_ALT,
        usage(KeyCode::KeyT),
    )];
    want.extend_from_slice(text);
    want.push((0x00, usage(KeyCode::Enter)));

    let path = macro_file(
        "reboot.macro",
        "# Open a terminal and reboot the target\nkey ctrl+alt+t\ntype sudo reboot\nkey enter\n",
    );
    let fake = FakeCh9329::spawn(Behaviour::default()).expect("spawn fake");
    let mut child = spawn(
        &fake,
        &[
            "macro",
            path.to_str().expect("utf-8 path"),
            "--delay-ms",
            "0",
        ],
    );
    let (status, output) = finish(&mut child);
    assert!(status.success(), "{output}");
    assert!(output.contains("delivered 26 reports"), "{output}");

    let frames = keyboard_frames(&fake);
    assert_eq!(
        frames.len(),
        with_the_guards_release(2 * want.len()),
        "one press, one release\n{output}"
    );
    assert!(
        frames.last().is_some_and(is_release_all),
        "even a clean run ends released\n{output}"
    );
    for (i, &(modifiers, usage)) in want.iter().enumerate() {
        let press = &frames[2 * i];
        assert_eq!(
            (press.data[0], press.data[2]),
            (modifiers, usage),
            "report {i} arrived as modifiers {:#04x} usage {:#04x}\n{output}",
            press.data[0],
            press.data[2]
        );
        assert!(
            is_release_all(&frames[2 * i + 1]),
            "report {i} was not released\n{output}"
        );
    }

    // Compare wire payloads with the independent packet fixtures.
    let fixtures = Fixtures::load().expect("fixtures/packets/ch9329.toml");
    let mut matched: Vec<String> = Vec::new();
    for frame in &frames {
        let Some(fixture) = fixtures.keyboard(&frame.data) else {
            continue;
        };
        assert_eq!(
            frame.encode().expect("re-encodes"),
            frame_bytes(&fixture.frame),
            "the frame the device received is not the one fixture {} pins",
            fixture.name
        );
        matched.push(fixture.name.clone());
    }
    matched.sort_unstable();
    matched.dedup();
    assert_eq!(
        matched,
        vec!["kb_release_all"],
        "the authority's keyboard packets for this script must all have been checked"
    );
}

/// Leading two-hex-digit tokens only, stopping at the first token that is not one. Mirrors
/// `tests/script.rs::frame_bytes`, so a `frame` value may carry a trailing comment.
fn frame_bytes(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for token in s.split_whitespace() {
        match u8::from_str_radix(token, 16) {
            Ok(b) if token.len() == 2 => out.push(b),
            _ => break,
        }
    }
    out
}

/// `type -` reads stdin to EOF, verbatim: the trailing newline is an Enter, because that is the
/// only way a piped script can press one.
#[test]
fn type_dash_reads_the_text_from_stdin() {
    let fake = FakeCh9329::spawn(Behaviour::default()).expect("spawn fake");
    let mut cmd = command(&fake, &["type", "-", "--delay-ms", "0"]);
    cmd.stdin(Stdio::piped());
    let mut child = ChildGuard::spawn("nanokvm", &mut cmd).expect("spawn nanokvm");
    child
        .take_stdin()
        .expect("stdin is piped")
        .write_all(b"hi\n")
        .expect("write the text");

    let (status, output) = finish(&mut child);
    assert!(status.success(), "{output}");

    let frames = keyboard_frames(&fake);
    assert_eq!(
        frames.len(),
        with_the_guards_release(6),
        "h, i and the newline\n{output}"
    );
    assert_eq!(frames[0].data[2], usage(KeyCode::KeyH), "{output}");
    assert_eq!(frames[2].data[2], usage(KeyCode::KeyI), "{output}");
    assert_eq!(
        frames[4].data[2],
        usage(KeyCode::Enter),
        "the trailing newline is a keystroke, not noise\n{output}"
    );
}

/// Compare elapsed time with and without `wait` because the fake records bytes,
/// not timestamps. Loose bounds tolerate scheduling noise while detecting a skipped wait.
#[test]
fn a_macro_wait_delays_the_reports_that_follow_it() {
    let with_wait = run_two_chord_macro("with-wait.macro", "key a\nwait 300\nkey b\n");
    let without = run_two_chord_macro("without-wait.macro", "key a\nkey b\n");

    assert!(
        with_wait >= Duration::from_millis(300),
        "the run finished in {with_wait:?}, which is less than the wait it was asked for"
    );
    assert!(
        with_wait >= without + Duration::from_millis(200),
        "the wait cost {with_wait:?} against {without:?} without it, so it was not honoured"
    );
}

/// Run a two-chord macro to completion and return how long the process took, having checked that
/// it did deliver its four reports.
fn run_two_chord_macro(name: &str, body: &str) -> Duration {
    let path = macro_file(name, body);
    let fake = FakeCh9329::spawn(Behaviour::default()).expect("spawn fake");
    let started = Instant::now();
    let mut child = spawn(
        &fake,
        &[
            "macro",
            path.to_str().expect("utf-8 path"),
            "--delay-ms",
            "0",
        ],
    );
    let (status, output) = finish(&mut child);
    let elapsed = started.elapsed();

    assert!(status.success(), "{output}");
    assert_eq!(
        keyboard_frames(&fake).len(),
        with_the_guards_release(4),
        "{output}"
    );
    elapsed
}

// ---------------------------------------------------------------------------
// The one hard rule.
// ---------------------------------------------------------------------------

/// No mouse, checked against the source rather than trusted: a click on a live desktop can launch
/// or destroy something (CLAUDE.md), and no command in `src/cli/` and nothing in the compiler has
/// any business building one.
///
/// Every file in both directories is read from disk and the count is asserted against the
/// directory listing, so a file added to either is searched too rather than quietly exempt. This
/// is the one such test: the narrower one in `tests/script.rs` covered `src/script/` alone and
/// left `shot.rs`, `devices.rs` and `out.rs` unchecked.
#[test]
fn no_file_under_src_cli_or_src_script_contains_a_mouse_path() {
    let mut sources: Vec<(String, String)> = Vec::new();
    let mut expected = 0usize;
    for dir in [
        concat!(env!("CARGO_MANIFEST_DIR"), "/src/cli"),
        concat!(env!("CARGO_MANIFEST_DIR"), "/src/script"),
    ] {
        for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{dir}: {e}")) {
            let path = entry.expect("dir entry").path();
            expected += 1;
            sources.push((
                path.display().to_string(),
                std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display())),
            ));
        }
    }
    assert_eq!(sources.len(), expected, "a file was not read");
    assert!(
        sources.len() >= 10,
        "only {} files were searched; both directories should be there",
        sources.len()
    );
    for name in ["keys.rs", "shot.rs", "devices.rs", "compile.rs", "chord.rs"] {
        assert!(
            sources.iter().any(|(path, _)| path.ends_with(name)),
            "{name} was not among the files searched"
        );
    }

    // Split so this test's own text does not satisfy the search it performs.
    let needles = [
        concat!("SEND_", "MS_ABS_DATA"),
        concat!("SEND_", "MS_REL_DATA"),
        concat!("Mouse", "AbsReport"),
        concat!("Mouse", "RelReport"),
        concat!("cmd::", "SEND_MS"),
    ];
    for (name, source) in &sources {
        for needle in needles {
            assert_eq!(
                source.matches(needle).count(),
                0,
                "{needle} must not appear in {name}: no mouse report is ever built here"
            );
        }
    }
}

/// H1: `nanokvm key capslock | head -1` used to end in `failed printing to stdout: Broken pipe`.
/// A reader that leaves is an ordinary thing, and the command must finish its own work — the
/// release-all above all — and exit cleanly.
///
/// The read end of the pipe is dropped before the child prints, which is what `head` does when it
/// has had its line. The fix must not be `SIG_DFL` for SIGPIPE either: that would kill the process
/// at the write, past the `Released` guard, so the release-all reaching the device is half of what
/// this test is for.
#[test]
fn a_closed_stdout_is_not_a_panic_and_still_releases() {
    let fake = FakeCh9329::spawn(Behaviour::default()).expect("spawn fake");
    let mut cmd = command(&fake, &["key", "a"]);
    let mut child = ChildGuard::spawn("nanokvm", &mut cmd).expect("spawn nanokvm");
    // The reader goes away before the first line is written.
    drop(child.take_stdout());

    let mut stderr = child.take_stderr();
    let Some(status) = child.wait_bounded(EXIT_BOUND).expect("poll the child") else {
        panic!("nanokvm did not exit within {EXIT_BOUND:?}");
    };
    let mut output = String::new();
    if let Some(e) = stderr.as_mut() {
        let _ = e.read_to_string(&mut output);
    }

    assert!(
        status.success(),
        "a closed stdout must not fail the run (exit {:?})\n{output}",
        status.code()
    );
    assert!(
        !output.contains("Broken pipe") && !output.contains("panicked"),
        "the closed pipe became a panic\n{output}"
    );
    let frames = keyboard_frames(&fake);
    assert_eq!(
        frames.len(),
        with_the_guards_release(2),
        "the chord and the guard's release-all still went out\n{output}"
    );
    assert!(
        frames.last().is_some_and(is_release_all),
        "the last report the device saw was a press\n{output}"
    );
}
