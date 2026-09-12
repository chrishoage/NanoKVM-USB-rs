//! Keyboard chord, text, and macro delivery.
//!
//! Compile the entire input before opening the port. Send reports sequentially and stop
//! on failure, reporting partial progress; replay after reconnect could repeat a command.
//! A release guard attempts to clear held keys on exit, including errors and caught signals.
//!
//! Text is mapped for the declared target layout. CapsLock policy is checked against a
//! fresh device reading before any keyboard report is sent. Dry runs use the same compiler
//! and fixture comparisons without opening a device.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::cli::{install_signal_handlers, interrupted, out};
use crate::link::Link;
use crate::proto::cmd;
use crate::proto::report::KeyboardReport;
use crate::script::{
    compile_key, compile_macro, compile_type, render, CapsLock, Fixtures, Layout, Script, Step,
    GRAMMAR,
};
use crate::serial::SerialLink;

/// Reply window for one report. The same 500 ms `tests/serial_hardware.rs` uses, which is two
/// orders of magnitude above the measured 4 ms ack.
const TIMEOUT: Duration = Duration::from_millis(500);

/// Longest uninterruptible sleep. A `wait` step and the inter-report delay are served in slices
/// this long, so a signal arriving during a long wait is noticed promptly rather than after it.
const SLICE: Duration = Duration::from_millis(10);

/// What to do when the target reports CapsLock on.
///
/// The host cannot turn it off without changing the target's state, and it cannot see it change
/// afterwards either, so this is the user's call and not a guess.
#[derive(Copy, Clone, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum CapsLockPolicy {
    Refuse,
    Ignore,
    Compensate,
}

#[derive(clap::Args, Debug)]
#[command(after_help = GRAMMAR)]
pub struct KeyArgs {
    /// Chords to send, in order, each pressed and then fully released.
    #[arg(value_name = "CHORD", required = true)]
    pub chords: Vec<String>,

    /// Milliseconds between keyboard reports.
    #[arg(long, value_name = "MS", default_value_t = crate::script::REPORT_DELAY_MS)]
    pub delay_ms: u64,

    /// Print reports without opening devices or sending input.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(clap::Args, Debug)]
#[command(after_help = GRAMMAR)]
pub struct TypeArgs {
    /// Text to type; use `-` to read stdin.
    #[arg(value_name = "TEXT", required = true)]
    pub text: String,

    /// Target keyboard layout, independent of the host layout. Unsupported characters
    /// stop the command before typing. [default: us]
    // Keep omission distinct from an explicit layout so a macro directive can supply it.
    #[arg(long, value_name = "LAYOUT")]
    pub layout: Option<String>,

    /// Milliseconds between keyboard reports.
    #[arg(long, value_name = "MS", default_value_t = crate::script::REPORT_DELAY_MS)]
    pub delay_ms: u64,

    /// Print reports without opening devices or sending input.
    #[arg(long)]
    pub dry_run: bool,

    /// What to do when the target reports CapsLock on: refuse (letters would come out inverted),
    /// ignore (send as if it were off), or compensate (invert shift on letters).
    #[arg(long, value_enum, default_value_t = CapsLockPolicy::Refuse)]
    pub caps_lock: CapsLockPolicy,
}

#[derive(clap::Args, Debug)]
#[command(after_help = GRAMMAR)]
pub struct MacroArgs {
    /// Macro file to run.
    #[arg(value_name = "FILE")]
    pub file: PathBuf,

    /// Target keyboard layout, independent of the host layout. Unsupported characters
    /// stop the command before typing. [default: us]
    // `None` means the file decides: a `layout` directive supplies it, and its absence means
    // `Layout::default`. Given here, a directive that disagrees is an error rather than a silent
    // override — see `compile_macro`.
    #[arg(long, value_name = "LAYOUT")]
    pub layout: Option<String>,

    /// Milliseconds between keyboard reports.
    #[arg(long, value_name = "MS", default_value_t = crate::script::REPORT_DELAY_MS)]
    pub delay_ms: u64,

    /// Print reports without opening devices or sending input.
    #[arg(long)]
    pub dry_run: bool,

    /// What to do when the target reports CapsLock on: refuse (letters would come out inverted),
    /// ignore (send as if it were off), or compensate (invert shift on letters).
    #[arg(long, value_enum, default_value_t = CapsLockPolicy::Refuse)]
    pub caps_lock: CapsLockPolicy,
}

/// CapsLock refusal after preamble and device query, but before any keyboard report.
const CAPS_LOCK_REFUSAL: &str = "the target reports CapsLock ON; letters would be typed inverted. \
     Turn it off with \"nanokvm key capslock\", or pass --caps-lock ignore|compensate. No \
     keyboard report was sent.";

/// Script compiled for CapsLock off and, for text, with compensation. Both versions
/// are prepared before opening serial so compilation cannot fail during delivery.
struct Compiled {
    /// The script as the caller asked for it, with CapsLock assumed off.
    script: Script,
    /// The same source under [`CapsLock::Compensate`]. `None` for `key`: a chord names a key, not
    /// a character, so the target's CapsLock does not change what it means.
    compensated: Option<Script>,
}

impl Compiled {
    /// Whether the target's CapsLock changes what this script types.
    ///
    /// Asked of the compiler rather than restated here: the two compilations differ when
    /// some report's shift bit depends on CapsLock, which is `script`'s own definition of "a
    /// letter". Nothing in the CLI has to know which characters those are.
    fn affected_by_caps_lock(&self) -> bool {
        self.compensated
            .as_ref()
            .is_some_and(|alt| *alt != self.script)
    }
}

/// Everything about *how* a compiled script is delivered, as opposed to what it says.
struct Delivery<'a> {
    serial: &'a Path,
    delay: Duration,
    dry_run: bool,
    caps_lock: CapsLockPolicy,
}

/// Send one or more chords, each pressed with its modifiers and then fully released.
///
/// Chords are key forwarding, so the declared layout only decides what a single-character chord
/// key such as `ctrl+c` means, and the target's CapsLock decides nothing at all.
pub fn run_key(args: &KeyArgs, serial: &Path) -> Result<()> {
    let compiled = Compiled {
        script: compile_key(&args.chords, Layout::default())?,
        compensated: None,
    };
    deliver(
        &compiled,
        &Delivery {
            serial,
            delay: Duration::from_millis(args.delay_ms),
            dry_run: args.dry_run,
            // A chord names a key, not a character; the policy has nothing to decide.
            caps_lock: CapsLockPolicy::Ignore,
        },
    )
}

/// Type text, one press and release per character, against the declared target layout.
pub fn run_type(args: &TypeArgs, serial: &Path) -> Result<()> {
    let layout = declared_layout(args.layout.as_deref())?.unwrap_or_default();
    let text = text_to_type(args)?;
    let compiled = Compiled {
        script: compile_type(&text, layout, CapsLock::Off)?,
        compensated: Some(compile_type(&text, layout, CapsLock::Compensate)?),
    };
    deliver(
        &compiled,
        &Delivery {
            serial,
            delay: Duration::from_millis(args.delay_ms),
            dry_run: args.dry_run,
            caps_lock: args.caps_lock,
        },
    )
}

/// Run a macro file of `key`, `type` and `wait` steps.
///
/// The layout is passed through as the `Option` the command line gave: `None` leaves the file's
/// own `layout` directive free to supply it, which is the whole point of the directive.
pub fn run_macro(args: &MacroArgs, serial: &Path) -> Result<()> {
    let layout = declared_layout(args.layout.as_deref())?;
    let source = std::fs::read_to_string(&args.file).with_context(|| {
        format!(
            "reading the macro file {}. Nothing was opened and nothing was sent.",
            args.file.display()
        )
    })?;
    let compiled = Compiled {
        script: compile_macro(&source, layout, CapsLock::Off)?,
        compensated: Some(compile_macro(&source, layout, CapsLock::Compensate)?),
    };
    deliver(
        &compiled,
        &Delivery {
            serial,
            delay: Duration::from_millis(args.delay_ms),
            dry_run: args.dry_run,
            caps_lock: args.caps_lock,
        },
    )
}

/// The `--layout` value parsed, or `None` where the flag was not given.
///
/// Separate from `Layout::from_str` only so that both callers spell the "not given" case the same
/// way: an unknown name is the layout module's error, unchanged.
fn declared_layout(name: Option<&str>) -> Result<Option<Layout>> {
    match name {
        Some(name) => Ok(Some(name.parse::<Layout>()?)),
        None => Ok(None),
    }
}

/// The text `type` was given: its argument, or all of stdin when that argument is `-`.
///
/// Read verbatim, to EOF, trailing newline included. `echo | nanokvm type -` therefore ends
/// in an Enter, which is the only way a piped script can press one; silently trimming it would
/// make a whole keystroke unreachable and would differ from the argument form for no reason
/// visible to the user. `--dry-run` shows what would be sent.
fn text_to_type(args: &TypeArgs) -> Result<String> {
    if args.text != "-" {
        return Ok(args.text.clone());
    }
    // A `type -` typed at a terminal reads the terminal, and an unannounced read to EOF looks
    // like a hang. Said on stderr so it cannot end up in anything piped.
    if stdin_is_a_terminal() {
        out::note("reading text from stdin until EOF (Ctrl-D)");
    }
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .context("reading the text to type from stdin. Nothing was opened and nothing was sent.")?;
    Ok(text)
}

/// Whether descriptor 0 is a terminal.
fn stdin_is_a_terminal() -> bool {
    // SAFETY: `isatty` reads no memory and only inspects the descriptor, which is always valid to
    // ask about — 0 may be closed, and the answer is then simply 0.
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

/// Open, gate, guard, send — in that order, which is the order the safety argument depends on.
///
/// Every step before the guard exists is one that must be able to refuse without having sent
/// anything: `--dry-run` opens nothing, the port is opened only after the compile succeeded, the
/// `GET_INFO` gate and the CapsLock policy are both checked before [`Released`] is constructed,
/// and from that point on every exit path emits a release-all.
fn deliver(compiled: &Compiled, how: &Delivery) -> Result<()> {
    if how.dry_run {
        // Nothing is opened, so nothing can be sent — not even the node this command was handed,
        // which on a dry run is never resolved at all (see `main.rs`). The authority file names
        // the frames it has; a shipped binary has no source tree to read it from, which is
        // why its absence is ordinary and the rows simply go unnamed.
        out::block(&render(&compiled.script, Fixtures::load().ok().as_ref())?);
        return Ok(());
    }

    // SIGINT, SIGTERM and SIGHUP all become the same flag, and the flag becomes an abort plus the
    // guard's release-all. Installed before the port is opened, so no signal can arrive at a
    // moment when a key is held and the handler is not yet in place.
    install_signal_handlers();

    let mut link = SerialLink::open(how.serial).with_context(|| {
        format!(
            "opening the CH9329 serial link on {}. Check the path and that you can write to it \
             (it is usually owned by the 'uucp' or 'dialout' group).",
            how.serial.display()
        )
    })?;

    // Finish any prior torn frame before sending the first request.
    link.resync().with_context(|| {
        format!(
            "resynchronising the CH9329 frame parser on {}",
            how.serial.display()
        )
    })?;

    // GET_INFO first: it is the one thing that proves a CH9329 is there and answering,
    // and `target_connected` is the difference between typing into a console and typing into
    // nothing. Checked before the guard exists, so refusing to run sends no frame at all.
    let info = link
        .get_info(TIMEOUT)
        .with_context(|| format!("GET_INFO on {} did not answer", how.serial.display()))?;
    // One wording for the lock bits, shared with `devices --probe` and the viewer's startup line
    // (`DeviceInfo`'s `Display`): `caps=on`, never `caps=true`.
    out::line(&info.to_string());
    if !info.target_connected {
        bail!(
            "the device reports no target on its HID side: the keystrokes would go nowhere. \
             Refusing to run. No keyboard report was sent."
        );
    }

    let script = choose_encoding(compiled, info.caps_lock, how.caps_lock)?;

    let mut link = Released(link);
    let sent = run(&mut link, script, how.delay)?;
    out::line(&format!(
        "delivered {sent} reports; the effect is on the target's screen, not in this ack"
    ));
    Ok(())
}

/// Which of the two compilations goes on the wire, given what `GET_INFO` said and what the user
/// asked for.
///
/// A CapsLock the script does not care about is not a reason to refuse anything, so the policy is
/// consulted only when the target reports it on *and* the two compilations differ. Refusing here
/// happens before the guard exists, which is what makes "nothing was sent" true.
fn choose_encoding(
    compiled: &Compiled,
    caps_lock_on: bool,
    policy: CapsLockPolicy,
) -> Result<&Script> {
    if !caps_lock_on || !compiled.affected_by_caps_lock() {
        return Ok(&compiled.script);
    }
    match policy {
        CapsLockPolicy::Refuse => bail!(CAPS_LOCK_REFUSAL),
        CapsLockPolicy::Ignore => Ok(&compiled.script),
        CapsLockPolicy::Compensate => {
            // On stderr, because it is a note about the run and not part of what the run produced
            // — and because the target's CapsLock may have changed since GET_INFO answered, which
            // no ack can tell us.
            out::note("compensating for CapsLock: shift inverted on letters");
            Ok(compiled
                .compensated
                .as_ref()
                .expect("a script affected by CapsLock has a compensated compilation"))
        }
    }
}

/// Owns the link and sends a keyboard release-all when it goes out of scope, however it goes out
/// of scope — clean exit, `?`, panic, or a caught signal the send loop turns into an error.
///
/// Keyboard only. The mouse half that `tests/serial_hardware.rs` sends is absent on purpose:
/// these commands never press a button, so they have nothing to release, and building a mouse
/// report at all is the thing they are forbidden to do.
struct Released(SerialLink);

impl Drop for Released {
    fn drop(&mut self) {
        let payload = KeyboardReport::RELEASE_ALL.payload();
        match self
            .0
            .transact(cmd::SEND_KB_GENERAL_DATA, &payload, TIMEOUT)
        {
            Ok(_) => out::note("[release-all] keyboard report sent"),
            // An unsent release leaves target state uncertain and must be reported.
            Err(e) => out::note(&format!("[release-all] keyboard report UNSENT: {e}")),
        }
    }
}

/// Sleep in [`SLICE`]-sized pieces, giving up as soon as a caught signal has been seen.
///
/// The deadline is representable because `wait` is bounded at compile time (`script::compile`'s
/// hour limit), so this addition cannot overflow `Instant` — which is what a `wait` of `u64::MAX`
/// milliseconds used to do, panicking inside the send loop.
fn pause(total: Duration) -> Result<()> {
    let deadline = Instant::now() + total;
    loop {
        if interrupted() {
            bail!("interrupted");
        }
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(());
        };
        std::thread::sleep(left.min(SLICE));
    }
}

/// Deliver the compiled script, blocking on each acknowledgement.
///
/// Every error names how far the script got, because "it failed" and "it failed after typing
/// `sudo reb`" are different facts to the person holding the console.
fn run(link: &mut Released, script: &Script, delay: Duration) -> Result<usize> {
    let total = script.report_count();
    let mut sent = 0usize;
    for step in &script.steps {
        if interrupted() {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn compiled(text: &str) -> Compiled {
        Compiled {
            script: compile_type(text, Layout::Us, CapsLock::Off).expect("compiles"),
            compensated: Some(compile_type(text, Layout::Us, CapsLock::Compensate).expect("ditto")),
        }
    }

    /// The policy is consulted only where CapsLock would change what is typed, which is what the
    /// two compilations differing means. A script of digits is not a reason to refuse anything.
    #[test]
    fn only_a_script_whose_encoding_depends_on_capslock_is_affected() {
        assert!(compiled("hello").affected_by_caps_lock());
        assert!(!compiled("1234!").affected_by_caps_lock());
        // `key` has no second compilation at all: a chord names a key, not a character.
        assert!(!Compiled {
            script: compile_key(&["ctrl+alt+t".to_string()], Layout::Us).expect("compiles"),
            compensated: None,
        }
        .affected_by_caps_lock());
    }

    /// The refusal happens before the guard exists, so its message has to be the whole of what
    /// the user gets: what is wrong, both ways to overrule it, and what did or did not reach the
    /// target. The port is already open and `GET_INFO` already answered by this point, so the
    /// promise it makes is about keyboard reports and not about the wire.
    #[test]
    fn refuse_is_the_default_and_names_both_ways_out() {
        let err = choose_encoding(&compiled("hello"), true, CapsLockPolicy::Refuse).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("CapsLock ON"), "{msg}");
        assert!(msg.contains("nanokvm key capslock"), "{msg}");
        assert!(msg.contains("--caps-lock ignore|compensate"), "{msg}");
        assert!(msg.contains("No keyboard report was sent."), "{msg}");
        assert!(
            !msg.contains("Nothing was sent"),
            "the port was opened and queried, so 'nothing' is a claim this cannot make: {msg}"
        );
    }

    #[test]
    fn ignore_sends_the_script_as_compiled_and_compensate_sends_the_other_one() {
        let compiled = compiled("hello");
        let ignored = choose_encoding(&compiled, true, CapsLockPolicy::Ignore).expect("sends");
        assert_eq!(ignored, &compiled.script);
        let compensated =
            choose_encoding(&compiled, true, CapsLockPolicy::Compensate).expect("sends");
        assert_eq!(compensated, compiled.compensated.as_ref().expect("some"));
        assert_ne!(compensated, &compiled.script);
    }

    /// With CapsLock off there is nothing to decide, and `refuse` — the default — must not turn
    /// an ordinary run into an error.
    #[test]
    fn capslock_off_sends_the_ordinary_compilation_whatever_the_policy_says() {
        let compiled = compiled("hello");
        for policy in [
            CapsLockPolicy::Refuse,
            CapsLockPolicy::Ignore,
            CapsLockPolicy::Compensate,
        ] {
            let chosen = choose_encoding(&compiled, false, policy).expect("sends");
            assert_eq!(chosen, &compiled.script, "{policy:?}");
        }
    }
}
