//! Compiled keyboard reports checked against independently recorded packet fixtures.

use nanokvm::proto::report::KeyboardReport;
use nanokvm::proto::{cmd, encode};
use nanokvm::script::{compile_key, compile_type, CapsLock, Fixtures, Layout, Script, Step};

fn chords(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

fn payloads(script: &Script) -> Vec<[u8; 8]> {
    script
        .steps
        .iter()
        .filter_map(|s| match s {
            Step::Send(out) => Some(out.report.payload()),
            Step::Wait(_) => None,
        })
        .collect()
}

/// Leading two-hex-digit tokens only, stopping at the first token that is not one. Mirrors
/// `tests/proto_fixtures.rs::hex_prefix` and `scripts/check-fixtures.py::parse`, so a `frame`
/// value may carry a trailing comment.
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

/// Cover every keyboard packet fixture with compiled scripts so fixture changes
/// cannot silently leave an expected report untested.
#[test]
fn every_keyboard_fixture_is_produced_by_a_script_and_encodes_to_its_frame() {
    let fixtures = Fixtures::load().expect("fixtures/packets/ch9329.toml");

    // `shift+a` is the shifted-A press; `capslock` is the CapsLock press; both scripts end in the
    // release-all, which is the third keyboard packet in the file.
    let mut produced: Vec<[u8; 8]> = Vec::new();
    produced.extend(payloads(
        &compile_key(&chords(&["shift+a", "capslock"]), Layout::Us).expect("chords compile"),
    ));
    produced.extend(payloads(
        &compile_type("A", Layout::Us, CapsLock::Off).expect("text compiles"),
    ));

    let keyboard: Vec<_> = fixtures
        .packets()
        .iter()
        .filter(|p| p.cmd == cmd::SEND_KB_GENERAL_DATA)
        .collect();
    assert!(
        keyboard.len() >= 3,
        "the authority lost its keyboard packets"
    );

    let mut matched: Vec<&str> = Vec::new();
    for fixture in keyboard {
        assert!(
            produced.iter().any(|p| p[..] == fixture.data[..]),
            "no script above produces {}; add one rather than dropping the check",
            fixture.name
        );
        let frame = encode(cmd::SEND_KB_GENERAL_DATA, &fixture.data).expect("encodes");
        assert_eq!(
            frame,
            frame_bytes(&fixture.frame),
            "frame for fixture {}",
            fixture.name
        );
        matched.push(&fixture.name);
    }
    matched.sort_unstable();
    assert_eq!(
        matched,
        vec!["kb_capslock_press", "kb_release_all", "kb_shift_a_press"]
    );
}

/// A compiled script's last report is a release-all, whatever it compiled from. The sender's
/// guard is what guarantees a release on an *abnormal* exit; this is the ordinary path,
/// where the script itself must not leave a key standing.
#[test]
fn a_compiled_script_always_ends_in_a_release_all() {
    let scripts = [
        compile_key(&chords(&["ctrl+alt+t"]), Layout::Us).expect("chord"),
        compile_key(&chords(&["super"]), Layout::Us).expect("modifier-only chord"),
        compile_type("sudo reboot\n", Layout::Us, CapsLock::Off).expect("text"),
    ];
    for script in scripts {
        let last = payloads(&script).pop().expect("a script sends something");
        assert_eq!(last, KeyboardReport::RELEASE_ALL.payload());
    }
}

// The no-mouse rule is checked in `tests/cli_keys.rs`, over `src/cli/` and `src/script/` in
// one test: the version that lived here read `src/script/` alone, which left the commands that
// use it — `shot.rs`, `devices.rs` — unsearched. One test, both directories, every file.
