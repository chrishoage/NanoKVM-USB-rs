//! Tests for the `type-keys` instrument (slice D2), in two layers.
//!
//! # Why they are here and not in the example
//!
//! An example's own `#[cfg(test)]` module is built and run only by `cargo test --examples`. Plain
//! `cargo test` — the gate `CLAUDE.md` documents — never runs it, so tests living inside
//! `examples/type-keys.rs` would quietly stop being run by the command that is supposed to run
//! them (review finding R3). The example's whole testable core therefore lives in
//! `examples/type_keys/script.rs`, which is not a Cargo target of its own; the example includes it
//! with `#[path]` to get a `main`, and this file includes the same source the same way. That makes
//! every test below an ordinary integration test, which `cargo test` with no flags runs.
//!
//! # The two layers
//!
//! - **In-process, against the included module**: the script compiler. Pure resolution, no I/O, so
//!   it is checked directly (`compile`, the layout table, the fixture cross-check).
//! - **Out-of-process, against the built binary**: the safety machinery — `run`, `pause`, the
//!   `GET_INFO` gate, the `Released` guard and the signal path. These only mean anything as
//!   *process* behaviour ("what did the device last see before this process died?"), so they exec
//!   `target/<profile>/examples/type-keys` and point it at
//!   [`nanokvm::serial::fake::FakeCh9329`] — a scripted CH9329 on a pty (§9.3). **No hardware is
//!   touched: the "serial port" is a pty**, and the one test that runs the real device path,
//!   `--dry-run`, opens nothing at all.
//!
//! `cargo test` builds examples, so the binary is on disk by the time these run. Running this
//! target alone (`cargo test --test type_keys`) does not necessarily build it; do
//! `cargo build --examples` first if the assertion in [`example_binary`] fires.

#[allow(dead_code)] // `main`'s half of the module — Args, run, dry_run, the guard — is unused here.
#[path = "../examples/type_keys/script.rs"]
mod script;

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use winit::keyboard::KeyCode;

use nanokvm::proto::frame::Frame;
use nanokvm::proto::keymap::hid_key;
use nanokvm::proto::report::{modifier, KeyboardReport};
use nanokvm::proto::{cmd, encode, HidKey};
use nanokvm::serial::fake::{Behaviour, FakeCh9329, DEFAULT_GET_INFO_PAYLOAD};

use script::{compile, hex, Fixtures, Step};

// ---------------------------------------------------------------------------
// Layer 1: the script compiler, in process.
// ---------------------------------------------------------------------------

fn script(tokens: &[&str]) -> Vec<String> {
    tokens.iter().map(|s| (*s).to_string()).collect()
}

fn reports(steps: &[Step]) -> Vec<KeyboardReport> {
    steps
        .iter()
        .filter_map(|s| match s {
            Step::Send(send) => Some(send.report),
            Step::Wait(_) => None,
        })
        .collect()
}

fn press(modifiers: u8, key: KeyCode) -> KeyboardReport {
    let Some(HidKey::Usage(usage)) = hid_key(key) else {
        panic!("{key:?} has no usage in proto::keymap");
    };
    KeyboardReport {
        modifiers,
        keys: [usage, 0, 0, 0, 0, 0],
    }
}

/// The whole point of compile-then-send: a script with one bad token yields no reports, so
/// there is nothing for a caller to half-deliver. Breaks if resolution moves into the send
/// loop, or if an unknown name is silently skipped.
#[test]
fn an_unknown_key_name_is_rejected_and_produces_nothing() {
    let err = compile(&script(&["text:ok", "key:anykey"])).unwrap_err();
    assert!(
        format!("{err:#}").contains("anykey"),
        "the error must name the offending key: {err:#}"
    );
}

/// Same rule for characters the declared layout cannot produce (§10.2: fail loudly, name the
/// character, never approximate). Breaks if an unmappable char is dropped or substituted.
#[test]
fn an_unmappable_character_is_rejected_and_produces_nothing() {
    for bad in ["text:naïve", "text:tab\there", "text:line\nbreak"] {
        let err = compile(&script(&["key:enter", bad])).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no US-QWERTY key produces"),
            "{bad:?} should be refused by the layout table, got: {msg}"
        );
    }
    assert!(compile(&script(&["text:naive"])).is_ok());
}

/// `chord:ctrl+alt+t` is exactly a press with both modifier bits and one release. Breaks if a
/// chord grows a separate modifier-only report, forgets the release, or loses a modifier.
#[test]
fn a_chord_is_one_press_with_the_modifier_bits_and_one_release() {
    let steps = compile(&script(&["chord:ctrl+alt+t"])).expect("chord compiles");
    let want = vec![
        press(modifier::LEFT_CTRL | modifier::LEFT_ALT, KeyCode::KeyT),
        KeyboardReport::RELEASE_ALL,
    ];
    assert_eq!(reports(&steps), want);
    // The bits are proto's, not this file's: 0x01 | 0x04.
    assert_eq!(modifier::LEFT_CTRL | modifier::LEFT_ALT, 0x05);
}

/// One press+release per character, in order. Breaks if a character is coalesced into a
/// multi-key report, or if the releases are batched to the end.
#[test]
fn text_is_one_press_and_release_per_character_in_order() {
    let steps = compile(&script(&["text:ab"])).expect("text compiles");
    assert_eq!(
        reports(&steps),
        vec![
            press(0, KeyCode::KeyA),
            KeyboardReport::RELEASE_ALL,
            press(0, KeyCode::KeyB),
            KeyboardReport::RELEASE_ALL,
        ]
    );
}

/// A capital is the same physical key with shift held — the report the `kb_shift_a_press`
/// fixture describes. Breaks if the shift bit is dropped or applied to the wrong key.
#[test]
fn a_capital_holds_shift_on_the_same_physical_key() {
    let steps = compile(&script(&["text:A"])).expect("text compiles");
    assert_eq!(
        reports(&steps),
        vec![
            press(modifier::LEFT_SHIFT, KeyCode::KeyA),
            KeyboardReport::RELEASE_ALL,
        ]
    );
}

/// `wait:` paces, it does not type. Breaks if a wait ever emits a report.
#[test]
fn wait_produces_no_report() {
    let steps = compile(&script(&["wait:250"])).expect("wait compiles");
    assert_eq!(steps, vec![Step::Wait(Duration::from_millis(250))]);
    assert!(reports(&steps).is_empty());
}

/// The encoder is checked against `fixtures/packets/ch9329.toml`, which is the authority
/// (§9.1); the bytes are read from it, never retyped here.
///
/// The file carries no Enter-press packet, so `key:enter` contributes only its release. Two
/// frames in this script do match — the shifted `A` press and the release-all — and the
/// assertion on the match count is what stops this test passing vacuously if a rename or a
/// payload change means nothing lines up any more.
#[test]
fn encoded_frames_match_the_fixture_packets() {
    let fixtures = Fixtures::load().expect("fixtures/packets/ch9329.toml");
    let steps = compile(&script(&["text:A", "key:enter"])).expect("script compiles");
    let mut matched: Vec<&str> = Vec::new();
    for report in reports(&steps) {
        let payload = report.payload();
        let Some(fixture) = fixtures.keyboard(&payload) else {
            continue;
        };
        let frame = encode(cmd::SEND_KB_GENERAL_DATA, &payload).expect("encodes");
        assert_eq!(
            hex(&frame),
            fixture.frame.to_ascii_uppercase(),
            "frame for fixture {}",
            fixture.name
        );
        matched.push(&fixture.name);
    }
    matched.sort_unstable();
    matched.dedup();
    assert_eq!(matched, vec!["kb_release_all", "kb_shift_a_press"]);
}

/// The one hard rule of this instrument, checked against the source rather than trusted.
///
/// Both halves of the example are searched: splitting `main` out of the core would otherwise have
/// halved this test's reach without anyone noticing.
#[test]
fn the_example_contains_no_mouse_path() {
    let sources = [
        include_str!("../examples/type-keys.rs"),
        include_str!("../examples/type_keys/script.rs"),
    ];
    // Split so this test's own text does not satisfy the search it performs.
    let needle = concat!("SEND_", "MS");
    for source in sources {
        assert_eq!(
            source.matches(needle).count(),
            0,
            "{needle} must not appear: no mouse report is ever built in the type-keys example"
        );
    }
}

// ---------------------------------------------------------------------------
// Layer 2: the safety machinery, against the built binary and a fake CH9329.
//
// These are the four properties the adversarial review found untested (finding R2): `run`,
// `pause`, the `Released` guard and the `GET_INFO` gate, plus a dry-run check of the layout
// table through the real binary. They were written by the reviewer and are kept as-is.
// ---------------------------------------------------------------------------

/// Where `cargo test` puts the example binary, found from this test binary's own path
/// (`target/<profile>/deps/type_keys-<hash>`).
fn example_binary() -> PathBuf {
    let mut dir = std::env::current_exe().expect("current_exe");
    dir.pop(); // deps
    if dir.ends_with("deps") {
        dir.pop();
    }
    let path = dir.join("examples").join("type-keys");
    assert!(
        path.exists(),
        "{} is missing; `cargo test` builds it, but `cargo test --test type_keys` alone may not — \
         run `cargo build --examples` first",
        path.display()
    );
    path
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

fn wait_for_exit(child: &mut Child, limit: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => return Some(status),
            None if Instant::now() >= deadline => return None,
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

fn drain(child: &mut Child) -> String {
    let mut out = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut out);
    }
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut out);
    }
    out
}

fn spawn_typer(fake: &FakeCh9329, extra: &[&str], tokens: &[&str]) -> Child {
    let mut cmd = Command::new(example_binary());
    cmd.arg("--serial").arg(fake.slave_path());
    cmd.args(extra);
    cmd.args(tokens);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.spawn().expect("spawn type-keys")
}

/// D2-R1. SIGINT mid-script must leave no key held: the run aborts and the `Drop` guard's
/// release-all is the last thing on the wire (§2.6, §2.6.1).
///
/// Fails if the guard never runs, if the interrupt is only noticed after the whole script, or if
/// the last frame the device saw was a press.
#[test]
fn review_sigint_midscript_leaves_no_key_held() {
    let fake = FakeCh9329::spawn(Behaviour::default()).expect("spawn fake");
    // 20 reports at the default 40 ms pacing: ~800 ms, long enough to interrupt in the middle.
    let mut child = spawn_typer(&fake, &[], &["text:aaaaaaaaaa"]);

    assert!(
        wait_until(Duration::from_secs(5), || keyboard_frames(&fake).len() >= 3),
        "the script never started sending"
    );
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGINT);
    }

    let status = wait_for_exit(&mut child, Duration::from_secs(5));
    let output = drain(&mut child);
    let status = match status {
        Some(s) => s,
        None => {
            let _ = child.kill();
            panic!("type-keys did not exit within 5 s of SIGINT\n{output}");
        }
    };

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
}

/// D2-R2. `target_connected == false` must send nothing at all — not even the guard's
/// release-all, since the guard is created after the gate.
///
/// Fails if any `SEND_KB_GENERAL_DATA` frame reaches the device when the gate refuses.
#[test]
fn review_get_info_gate_sends_no_keyboard_report() {
    let mut payload = DEFAULT_GET_INFO_PAYLOAD;
    payload[1] = 0x00; // target_connected = false
    let fake = FakeCh9329::spawn(Behaviour {
        get_info_payload: payload.to_vec(),
        ..Behaviour::default()
    })
    .expect("spawn fake");

    let mut child = spawn_typer(&fake, &[], &["chord:ctrl+alt+t", "text:sudo reboot"]);
    let status = wait_for_exit(&mut child, Duration::from_secs(10));
    let output = drain(&mut child);
    let status = status.unwrap_or_else(|| {
        let _ = child.kill();
        panic!("type-keys did not exit\n{output}");
    });

    assert!(
        !status.success(),
        "the gate did not refuse a disconnected target\n{output}"
    );
    assert_eq!(
        keyboard_frames(&fake).len(),
        0,
        "keyboard reports went out to a target the device says is not connected\n{output}"
    );
}

/// D2-R3. A device error mid-script must abort, report how far it got, and still release
/// (§2.8 item 3, §2.6).
///
/// The fake times out one reply after the script has started, so the failure lands in the middle
/// rather than on the first report. Fails if a press is the last thing the device saw, or if the
/// message does not name the progress.
#[test]
fn review_a_midscript_failure_aborts_and_still_releases() {
    let fake = FakeCh9329::spawn(Behaviour::default()).expect("spawn fake");
    let mut child = spawn_typer(&fake, &[], &["text:abcdef"]);

    assert!(
        wait_until(Duration::from_secs(5), || keyboard_frames(&fake).len() >= 2),
        "the script never started sending"
    );
    fake.drop_next_reply();

    let status = wait_for_exit(&mut child, Duration::from_secs(15));
    let output = drain(&mut child);
    let status = status.unwrap_or_else(|| {
        let _ = child.kill();
        panic!("type-keys hung after a dropped reply\n{output}");
    });

    let frames = keyboard_frames(&fake);
    assert!(
        !status.success(),
        "a script that lost a report exited 0\n{output}"
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
}

/// D2-R4. A key held when the process is terminated by anything other than SIGINT must not stay
/// held (review finding R1).
///
/// Before the fix, only SIGINT was handled: SIGTERM and SIGHUP kept their default disposition, so
/// the process died between a press and its release with no `Drop` running and no release-all on
/// the wire — a key held down on a live console. A harness that shells out to this instrument with
/// a timeout sends SIGTERM; closing the terminal sends SIGHUP.
///
/// Fails while the last frame the device saw is a press.
#[test]
fn review_sigterm_midscript_leaves_a_key_held() {
    let fake = FakeCh9329::spawn(Behaviour::default()).expect("spawn fake");
    // 300 ms between reports, so a signal delivered "at random" lands inside a press/release gap
    // rather than needing to hit a 40 ms window.
    let mut child = spawn_typer(&fake, &["--delay-ms", "300"], &["text:aaaaaa"]);

    assert!(
        wait_until(Duration::from_secs(5), || {
            keyboard_frames(&fake)
                .last()
                .is_some_and(|f| !is_release_all(f))
        }),
        "the script never got as far as a press"
    );
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    let status = wait_for_exit(&mut child, Duration::from_secs(5));
    let output = drain(&mut child);
    if status.is_none() {
        let _ = child.kill();
        panic!("type-keys ignored SIGTERM\n{output}");
    }

    let frames = keyboard_frames(&fake);
    assert!(
        frames.last().is_some_and(is_release_all),
        "SIGTERM killed the process with a key pressed and no release-all followed it. Last \
         report the device saw: {:?}\n{output}",
        frames.last().map(|f| f.data.clone())
    );
}

/// The same property for SIGHUP, the other signal the fix added. A terminal closing under a live
/// script is the everyday way this happens, and it used to be fatal in the same way SIGTERM was.
///
/// Fails while the last frame the device saw is a press.
#[test]
fn sighup_midscript_leaves_no_key_held() {
    let fake = FakeCh9329::spawn(Behaviour::default()).expect("spawn fake");
    let mut child = spawn_typer(&fake, &["--delay-ms", "300"], &["text:aaaaaa"]);

    assert!(
        wait_until(Duration::from_secs(5), || {
            keyboard_frames(&fake)
                .last()
                .is_some_and(|f| !is_release_all(f))
        }),
        "the script never got as far as a press"
    );
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGHUP);
    }
    let status = wait_for_exit(&mut child, Duration::from_secs(5));
    let output = drain(&mut child);
    if status.is_none() {
        let _ = child.kill();
        panic!("type-keys ignored SIGHUP\n{output}");
    }

    let frames = keyboard_frames(&fake);
    assert!(
        frames.last().is_some_and(is_release_all),
        "SIGHUP killed the process with a key pressed and no release-all followed it. Last \
         report the device saw: {:?}\n{output}",
        frames.last().map(|f| f.data.clone())
    );
}

/// D2-R5. The US-QWERTY table, checked against the characters the Stage 2 measurement actually
/// needs, through `--dry-run` on the real binary.
///
/// This is a *verification*, not a suspected defect: it pins the shift bit and the usage of every
/// character in `sudo reboot`, `xrandr --output HDMI-1 --mode 1280x720` and the shifted symbols
/// `- : / _ | ~`, so a later edit to the table cannot quietly change what gets typed at a root
/// prompt. It is also the only coverage of `dry_run` itself: it asserts the frame rows it prints.
#[test]
fn review_the_us_qwerty_table_types_the_stage2_commands() {
    // (char, expected modifier byte, expected HID usage) — US QWERTY, HID usage tables ch. 10.
    let expect: &[(char, u8, u8)] = &[
        ('s', 0x00, 0x16),
        ('u', 0x00, 0x18),
        ('d', 0x00, 0x07),
        ('o', 0x00, 0x12),
        (' ', 0x00, 0x2c),
        ('r', 0x00, 0x15),
        ('e', 0x00, 0x08),
        ('b', 0x00, 0x05),
        ('t', 0x00, 0x17),
        ('x', 0x00, 0x1b),
        ('a', 0x00, 0x04),
        ('n', 0x00, 0x11),
        ('H', 0x02, 0x0b),
        ('D', 0x02, 0x07),
        ('M', 0x02, 0x10),
        ('I', 0x02, 0x0c),
        ('-', 0x00, 0x2d),
        ('1', 0x00, 0x1e),
        ('m', 0x00, 0x10),
        ('p', 0x00, 0x13),
        ('2', 0x00, 0x1f),
        ('8', 0x00, 0x25),
        ('0', 0x00, 0x27),
        ('7', 0x00, 0x24),
        (':', 0x02, 0x33),
        ('/', 0x00, 0x38),
        ('_', 0x02, 0x2d),
        ('|', 0x02, 0x31),
        ('~', 0x02, 0x35),
    ];

    let text: String = expect.iter().map(|&(c, _, _)| c).collect();
    let out = Command::new(example_binary())
        .args(["--serial", "/dev/null", "--dry-run"])
        .arg(format!("text:{text}"))
        .output()
        .expect("run --dry-run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Every press row is `57 AB 00 02 08 <mods> 00 <usage> 00 00 00 00 00 <sum>`.
    let presses: Vec<Vec<u8>> = stdout
        .lines()
        .filter_map(|line| {
            // Take the longest trailing run of two-hex-digit words: that is the frame.
            let words: Vec<&str> = line.split_whitespace().collect();
            // The row number can itself be "57", so match the three-byte header, not one word.
            let start = words.windows(3).position(|w| w == ["57", "AB", "00"])?;
            let frame: Vec<u8> = words[start..]
                .iter()
                .map_while(|w| {
                    (w.len() == 2)
                        .then(|| u8::from_str_radix(w, 16).ok())
                        .flatten()
                })
                .collect();
            (frame.len() == 14).then_some(frame)
        })
        .filter(|f| f[..3] == [0x57, 0xAB, 0x00])
        .filter(|f| f[5..13] != [0u8; 8])
        .collect();

    assert_eq!(
        presses.len(),
        expect.len(),
        "expected one press frame per character\n{stdout}"
    );
    for (frame, &(c, mods, usage)) in presses.iter().zip(expect) {
        assert_eq!(
            (frame[5], frame[7]),
            (mods, usage),
            "{c:?} encoded as modifiers {:#04x} usage {:#04x}, want {mods:#04x}/{usage:#04x}",
            frame[5],
            frame[7]
        );
    }
}
