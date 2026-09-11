//! The testable core of the `type-keys` example: the CLI shape, the US-QWERTY table, the script
//! compiler, the release-all guard, and the paced sender.
//!
//! This file is not a Cargo target of its own. It is `#[path]`-included by two crates, and the
//! reason is in the header of `examples/type-keys.rs`: the example binary includes it as `mod
//! script` to get a `main`, and `tests/type_keys.rs` includes it the same way so that a plain
//! `cargo test` runs the tests for everything below. Nothing here is `main`, and nothing here
//! opens a device except on the path `main` drives.
//!
//! Everything in it is documented where it is defined; the module doc that explains *why* this
//! instrument exists and what it refuses to do lives in `examples/type-keys.rs`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser as ClapParser;
use serde::Deserialize;
use winit::keyboard::KeyCode;

use nanokvm::link::Link;
use nanokvm::proto::keymap::hid_key;
use nanokvm::proto::report::{modifier, KeyboardReport};
use nanokvm::proto::{cmd, encode, HidKey};
use nanokvm::serial::SerialLink;

/// Reply window for one report. The same 500 ms `tests/serial_hardware.rs` uses, which is two
/// orders of magnitude above the measured 4 ms ack.
pub const TIMEOUT: Duration = Duration::from_millis(500);

/// Longest uninterruptible sleep. `wait:` and the inter-report delay are served in slices this
/// long so that a signal arriving during a long wait is noticed promptly rather than after it.
const SLICE: Duration = Duration::from_millis(10);

/// The signals that mean "stop, but stop safely". SIGINT is the operator at the keyboard; SIGTERM
/// is what a harness sends when its timeout fires, and `type-keys` exists to be driven by exactly
/// such a harness; SIGHUP is the terminal going away underneath it. All three arrive while a key
/// may be held down, and all three used to be three different outcomes: only SIGINT ran the
/// release-all, and the other two took their default disposition and killed the process between a
/// press and its release, leaving that key held on a live console. CLAUDE.md's rule is "always
/// send a release-all before exiting anything that sent input", with no exemption for which
/// signal the caller chose, so all three take the flag-and-release path (§2.6).
///
/// SIGKILL cannot be caught and is the documented exception: it leaves the key held, which is why
/// the operator's escape is SIGTERM.
const CAUGHT_SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

/// Set by the signal handler, polled by the send loop.
///
/// The handler may only do async-signal-safe things and this store is one. Everything the
/// interrupt actually causes — the abort, the release-all — happens on the main thread, which is
/// what makes the `Drop` guard the single place a release is emitted (§2.6).
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_interrupt(_signal: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// Route every signal in [`CAUGHT_SIGNALS`] to the flag the send loop polls.
///
/// Called once, before the port is opened and so before anything can be held down. Idempotent, so
/// a test may call it too.
pub fn install_signal_handlers() {
    for signal in CAUGHT_SIGNALS {
        // SAFETY: `on_interrupt` is an `extern "C"` function whose whole body is one atomic store,
        // which is async-signal-safe. The previous disposition is deliberately discarded; this
        // program installs the handler once and never restores it.
        unsafe {
            libc::signal(signal, on_interrupt as libc::sighandler_t);
        }
    }
}

#[derive(ClapParser, Debug)]
#[command(
    name = "type-keys",
    about = "Send a scripted key sequence to the NanoKVM-USB target (Stage 2 instrument)",
    after_help = concat!(
        "SCRIPT tokens, processed left to right:\n",
        "  text:<string>     type printable US-QWERTY ASCII, one press+release per character\n",
        "  key:<name>        one press+release of a named key\n",
        "  chord:<mods>+<k>  modifiers held while <k> is pressed and released\n",
        "  wait:<ms>         pause\n",
        "\n",
        "  key names:   enter esc tab space backspace delete home end pgup pgdn\n",
        "               up down left right f1..f12\n",
        "  modifiers:   ctrl alt shift super\n",
        "\n",
        "Examples:\n",
        "  chord:ctrl+alt+t 'text:sudo reboot' key:enter\n",
        "  chord:alt+f4\n",
        "\n",
        "The chord key may also be a single character (chord:ctrl+c). A literal '+' is not\n",
        "reachable as a chord key; write chord:shift+= instead.\n",
    )
)]
pub struct Args {
    /// Serial node of the CH9329 bridge, e.g. /dev/ttyACM1.
    #[arg(long, value_name = "PATH")]
    pub serial: PathBuf,

    /// Milliseconds between reports. Paces the target's desktop, not the chip.
    #[arg(long, value_name = "MS", default_value_t = 40)]
    pub delay_ms: u64,

    /// Print the frames that would be sent and exit. Opens no device and sends nothing.
    #[arg(long)]
    pub dry_run: bool,

    #[arg(value_name = "SCRIPT", required = true)]
    pub script: Vec<String>,
}

// -- the compiled script ----------------------------------------------------

/// One report and the script fragment that produced it, so a failure can name the keystroke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    pub label: String,
    pub report: KeyboardReport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Send(Outgoing),
    Wait(Duration),
}

/// US QWERTY, indexed by `c - 'a'`. The *usages* are not here — they come from `proto::keymap`.
#[rustfmt::skip]
const LETTERS: [KeyCode; 26] = {
    use KeyCode as K;
    [
        K::KeyA, K::KeyB, K::KeyC, K::KeyD, K::KeyE, K::KeyF, K::KeyG, K::KeyH, K::KeyI, K::KeyJ,
        K::KeyK, K::KeyL, K::KeyM, K::KeyN, K::KeyO, K::KeyP, K::KeyQ, K::KeyR, K::KeyS, K::KeyT,
        K::KeyU, K::KeyV, K::KeyW, K::KeyX, K::KeyY, K::KeyZ,
    ]
};

/// Indexed by the digit, so `DIGITS[0]` is the `0` key even though it sits after `9` on the row.
#[rustfmt::skip]
const DIGITS: [KeyCode; 10] = {
    use KeyCode as K;
    [
        K::Digit0, K::Digit1, K::Digit2, K::Digit3, K::Digit4, K::Digit5, K::Digit6, K::Digit7,
        K::Digit8, K::Digit9,
    ]
};

/// What each digit key produces with shift held, indexed by the digit (US QWERTY).
const SHIFTED_DIGITS: [char; 10] = [')', '!', '@', '#', '$', '%', '^', '&', '*', '('];

/// `(unshifted, shifted, key)` for the punctuation keys of a US QWERTY board.
#[rustfmt::skip]
const PUNCTUATION: &[(char, char, KeyCode)] = {
    use KeyCode as K;
    &[
        ('-',  '_', K::Minus),        ('=',  '+', K::Equal),
        ('[',  '{', K::BracketLeft),  (']',  '}', K::BracketRight),
        ('\\', '|', K::Backslash),    (';',  ':', K::Semicolon),
        ('\'', '"', K::Quote),        ('`',  '~', K::Backquote),
        (',',  '<', K::Comma),        ('.',  '>', K::Period),
        ('/',  '?', K::Slash),
    ]
};

/// The `key:` vocabulary named by the contract. Deliberately small: this is an instrument, and a
/// key that is not here is reachable from the viewer.
#[rustfmt::skip]
const NAMED_KEYS: &[(&str, KeyCode)] = {
    use KeyCode as K;
    &[
        ("enter", K::Enter),     ("esc",  K::Escape),    ("tab",   K::Tab),
        ("space", K::Space),     ("backspace", K::Backspace), ("delete", K::Delete),
        ("home",  K::Home),      ("end",  K::End),       ("pgup",  K::PageUp),
        ("pgdn",  K::PageDown),  ("up",   K::ArrowUp),   ("down",  K::ArrowDown),
        ("left",  K::ArrowLeft), ("right", K::ArrowRight),
        ("f1", K::F1), ("f2", K::F2), ("f3", K::F3), ("f4", K::F4),
        ("f5", K::F5), ("f6", K::F6), ("f7", K::F7), ("f8", K::F8),
        ("f9", K::F9), ("f10", K::F10), ("f11", K::F11), ("f12", K::F12),
    ]
};

/// `chord:` modifier names. Left-hand variants throughout: the target sees a modifier bit, and
/// only the bit, so the side is arbitrary and one choice is one fewer thing to get wrong.
#[rustfmt::skip]
const MODIFIER_KEYS: &[(&str, KeyCode)] = {
    use KeyCode as K;
    &[
        ("ctrl", K::ControlLeft), ("alt", K::AltLeft),
        ("shift", K::ShiftLeft),  ("super", K::SuperLeft),
    ]
};

/// The physical key that produces `c` on a US QWERTY board, and whether shift is held for it.
fn char_key(c: char) -> Option<(KeyCode, bool)> {
    if c.is_ascii_lowercase() {
        return Some((LETTERS[(c as u8 - b'a') as usize], false));
    }
    if c.is_ascii_uppercase() {
        return Some((LETTERS[(c as u8 - b'A') as usize], true));
    }
    if c.is_ascii_digit() {
        return Some((DIGITS[(c as u8 - b'0') as usize], false));
    }
    if let Some(d) = SHIFTED_DIGITS.iter().position(|&s| s == c) {
        return Some((DIGITS[d], true));
    }
    if c == ' ' {
        return Some((KeyCode::Space, false));
    }
    PUNCTUATION
        .iter()
        .find_map(|&(plain, shifted, key)| match c {
            _ if c == plain => Some((key, false)),
            _ if c == shifted => Some((key, true)),
            _ => None,
        })
}

fn named_key(name: &str) -> Option<KeyCode> {
    NAMED_KEYS
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|&(_, key)| key)
}

/// The modifier bit `name` holds, taken from `proto::keymap` rather than restated here.
fn modifier_bit(name: &str) -> Option<u8> {
    let key = MODIFIER_KEYS
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|&(_, key)| key)?;
    match hid_key(key) {
        Some(HidKey::Modifier(bit)) => Some(bit),
        _ => None,
    }
}

/// The keyboard-report usage for a physical key. A modifier has no usage — it is a bit of byte 0
/// (Appendix) — so asking for one here is a script error, not something to encode as a slot.
fn usage_of(key: KeyCode) -> Result<u8> {
    match hid_key(key) {
        Some(HidKey::Usage(usage)) => Ok(usage),
        Some(HidKey::Modifier(_)) => Err(anyhow!(
            "{key:?} is a modifier; hold it with chord: rather than pressing it alone"
        )),
        None => Err(anyhow!("proto::keymap does not forward {key:?}")),
    }
}

/// Press with `modifiers` held, then release everything. Two reports, always in that order: the
/// release is not optional, because a script that aborts between them leaves the key held.
fn tap(usage: u8, modifiers: u8, label: String, out: &mut Vec<Step>) {
    out.push(Step::Send(Outgoing {
        label,
        report: KeyboardReport {
            modifiers,
            keys: [usage, 0, 0, 0, 0, 0],
        },
    }));
    out.push(Step::Send(Outgoing {
        label: "release".to_string(),
        report: KeyboardReport::RELEASE_ALL,
    }));
}

/// Resolve the whole script to reports, or fail having produced none (§2.8 item 3).
///
/// Nothing in here does I/O and nothing in here sends: the caller opens the port only once this
/// has returned `Ok`, which is what makes "fully typeable or not attempted" structural rather
/// than a promise.
pub fn compile(tokens: &[String]) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        let n = i + 1;
        let (kind, rest) = token.split_once(':').ok_or_else(|| {
            anyhow!("script token {n} {token:?} has no `kind:` prefix (text:, key:, chord:, wait:)")
        })?;
        match kind {
            "text" => {
                if rest.is_empty() {
                    bail!("script token {n}: text: with nothing to type");
                }
                for c in rest.chars() {
                    let (key, shift) = char_key(c).ok_or_else(|| {
                        anyhow!(
                            "script token {n}: no US-QWERTY key produces {c:?}; this instrument \
                             forwards keys, it does not inject text (§10.2)"
                        )
                    })?;
                    let usage = usage_of(key).with_context(|| format!("script token {n}"))?;
                    let mods = if shift { modifier::LEFT_SHIFT } else { 0 };
                    tap(usage, mods, format!("text {c:?}"), &mut steps);
                }
            }
            "key" => {
                let key = named_key(rest).ok_or_else(|| {
                    anyhow!("script token {n}: unknown key name {rest:?}; see --help for the list")
                })?;
                let usage = usage_of(key).with_context(|| format!("script token {n}"))?;
                tap(usage, 0, format!("key {rest}"), &mut steps);
            }
            "chord" => {
                let (mods, key_name) = rest.rsplit_once('+').ok_or_else(|| {
                    anyhow!("script token {n}: chord:{rest} has no modifier; use key: instead")
                })?;
                let (key, shift) = match named_key(key_name) {
                    Some(key) => (key, false),
                    None => {
                        let mut chars = key_name.chars();
                        match (chars.next(), chars.next()) {
                            (Some(c), None) => char_key(c).ok_or_else(|| {
                                anyhow!("script token {n}: no US-QWERTY key produces {c:?}")
                            })?,
                            _ => bail!(
                                "script token {n}: {key_name:?} is neither a key name nor a \
                                 single character"
                            ),
                        }
                    }
                };
                let mut modifiers = if shift { modifier::LEFT_SHIFT } else { 0 };
                for name in mods.split('+') {
                    modifiers |= modifier_bit(name).ok_or_else(|| {
                        anyhow!(
                            "script token {n}: unknown modifier {name:?}; expected ctrl, alt, \
                             shift or super"
                        )
                    })?;
                }
                let usage = usage_of(key).with_context(|| format!("script token {n}"))?;
                tap(usage, modifiers, format!("chord {rest}"), &mut steps);
            }
            "wait" => {
                let ms: u64 = rest.parse().with_context(|| {
                    format!("script token {n}: wait:{rest} is not milliseconds")
                })?;
                steps.push(Step::Wait(Duration::from_millis(ms)));
            }
            other => bail!(
                "script token {n}: unknown kind {other:?}; expected text:, key:, chord: or wait:"
            ),
        }
    }
    if steps.is_empty() {
        bail!("the script produced no reports");
    }
    Ok(steps)
}

// -- fixtures, for naming frames --------------------------------------------

/// `fixtures/packets/ch9329.toml` is the authority for packet bytes (§9.1). It is read here so
/// `--dry-run` can *name* the frames it prints, and so the tests below can check the encoder
/// against it rather than against bytes retyped into this file.
#[derive(Debug, Default, Deserialize)]
pub struct Fixtures {
    #[serde(default)]
    packet: Vec<Fixture>,
}

#[derive(Debug, Deserialize)]
pub struct Fixture {
    pub name: String,
    pub cmd: u8,
    pub data: Vec<u8>,
    pub frame: String,
}

impl Fixtures {
    const PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/packets/ch9329.toml");

    pub fn load() -> Result<Fixtures> {
        let text =
            std::fs::read_to_string(Self::PATH).with_context(|| format!("read {}", Self::PATH))?;
        toml::from_str(&text).with_context(|| format!("parse {}", Self::PATH))
    }

    /// The fixture whose bytes are exactly this keyboard payload, if the file carries one.
    pub fn keyboard(&self, payload: &[u8]) -> Option<&Fixture> {
        self.packet
            .iter()
            .find(|p| p.cmd == cmd::SEND_KB_GENERAL_DATA && p.data == payload)
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

// -- the release-all guard --------------------------------------------------

/// Owns the link and sends a keyboard release-all when it goes out of scope, however it goes out
/// of scope — clean exit, `?`, panic, or a caught signal the send loop turns into an error (§2.6).
///
/// Keyboard only. The mouse half that `tests/serial_hardware.rs` sends is absent on purpose: this
/// binary never presses a button, so it has nothing to release, and building a mouse report at
/// all is the thing this instrument is forbidden to do.
pub struct Released(pub SerialLink);

impl Drop for Released {
    fn drop(&mut self) {
        let payload = KeyboardReport::RELEASE_ALL.payload();
        match self
            .0
            .transact(cmd::SEND_KB_GENERAL_DATA, &payload, TIMEOUT)
        {
            Ok(_) => eprintln!("[release-all] keyboard report sent"),
            // §2.6.1: an unsent release means the target may still be holding a key. Say so.
            Err(e) => eprintln!("[release-all] keyboard report UNSENT: {e}"),
        }
    }
}

// -- running ----------------------------------------------------------------

/// Sleep in [`SLICE`]-sized pieces, giving up as soon as one of [`CAUGHT_SIGNALS`] has been seen.
fn pause(total: Duration) -> Result<()> {
    let deadline = Instant::now() + total;
    loop {
        if INTERRUPTED.load(Ordering::SeqCst) {
            bail!("interrupted");
        }
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(());
        };
        std::thread::sleep(left.min(SLICE));
    }
}

/// Deliver the compiled script, blocking on each acknowledgement (§2.9).
///
/// Every error names how far the script got, because "it failed" and "it failed after typing
/// `sudo reb`" are different facts to the person holding the console (§2.8 item 3).
pub fn run(link: &mut Released, steps: &[Step], delay: Duration) -> Result<usize> {
    let total = steps.iter().filter(|s| matches!(s, Step::Send(_))).count();
    let mut sent = 0usize;
    for step in steps {
        if INTERRUPTED.load(Ordering::SeqCst) {
            bail!("interrupted after {sent} of {total} reports");
        }
        match step {
            Step::Wait(d) => {
                pause(*d).with_context(|| format!("after {sent} of {total} reports"))?
            }
            Step::Send(send) => {
                if sent > 0 {
                    pause(delay).with_context(|| format!("after {sent} of {total} reports"))?;
                }
                let started = Instant::now();
                link.0
                    .transact(cmd::SEND_KB_GENERAL_DATA, &send.report.payload(), TIMEOUT)
                    .with_context(|| {
                        format!(
                            "{} was not acknowledged; {sent} of {total} reports had been \
                             delivered, so the sequence is incomplete",
                            send.label
                        )
                    })?;
                sent += 1;
                log::debug!("{} acked in {:?}", send.label, started.elapsed());
            }
        }
    }
    Ok(sent)
}

pub fn dry_run(steps: &[Step]) -> Result<()> {
    let fixtures = Fixtures::load().unwrap_or_else(|e| {
        eprintln!("note: {e:#}; frames will be printed without fixture names");
        Fixtures::default()
    });
    let reports = steps.iter().filter(|s| matches!(s, Step::Send(_))).count();
    println!("dry run: {reports} reports, nothing sent, no device opened");
    for (i, step) in steps.iter().enumerate() {
        match step {
            Step::Wait(d) => println!(
                "{:>4}  {:<22}  --",
                i + 1,
                format!("wait {}ms", d.as_millis())
            ),
            Step::Send(send) => {
                let payload = send.report.payload();
                let frame = encode(cmd::SEND_KB_GENERAL_DATA, &payload)
                    .map_err(|e| anyhow!("encoding {}: {e}", send.label))?;
                let hex_frame = hex(&frame);
                // Where the authority has this packet, say so — and say it loudly if the encoder
                // and the authority disagree, rather than printing a name that is not the truth.
                let name = match fixtures.keyboard(&payload) {
                    None => "-".to_string(),
                    Some(f) if f.frame.eq_ignore_ascii_case(&hex_frame) => f.name.clone(),
                    Some(f) => format!("{} DISAGREES: fixture says {}", f.name, f.frame),
                };
                println!("{:>4}  {:<22}  {hex_frame}   {name}", i + 1, send.label);
            }
        }
    }
    Ok(())
}
