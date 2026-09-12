//! Clipboard paste: the mapper and the job (plan §12 Stage 4c, §2.6, §2.8, §2.9, §10.2).
//!
//! Pure. No clipboard, no window, no producer, no clock read — [`PasteJob`] is advanced with an
//! [`Instant`] the caller supplies — so every rule below can be asserted without a display or a
//! device. The I/O halves are [`super::clipboard`] (the source) and `viewer::app` (the sink).
//!
//! # One mapper, one rule (§10.2 rev 5)
//!
//! *"Clipboard paste is text injection with the clipboard as its source. The reference client's
//! paste is a separate ASCII-to-US-usage table that skips non-ASCII silently, drops `\r`, and has
//! no cap; ours goes through `script::compile` under the same layout and the same
//! unreachable-character policy as `nanokvm type`, so there is one mapper and one rule."*
//!
//! So [`compile`] calls [`crate::script::compile_type`] — the shipped one, the one
//! `tests/cli_keys.rs` drives against hardware — and differences its reports into key transitions
//! with [`super::shortcut::transitions`], which is the same function the two built-in shortcuts
//! use. Nothing here has a usage table of its own.
//!
//! Two normalisations happen first, and they are the only liberties taken with the text:
//!
//! - **`\r\n` and a bare `\r` become `\n`**, which the layout maps to Enter. §12 Stage 4c asks for
//!   this by name. A clipboard copied from a Windows editor, or from a `<textarea>`, carries CRLF,
//!   and the alternative to normalising is refusing every such clipboard as unreachable.
//! - **Nothing else.** A tab is a tab: [`crate::script::Layout::key_for_char`] has a key for it
//!   (`'\n'` and `'\t'` are the two control characters with one), so it is typed as Tab rather
//!   than expanded to spaces. What a shell's readline does with a Tab is the target's business.
//!
//! # Reject before sending anything (§10.2, §2.8 item 3)
//!
//! A character the declared layout cannot reach fails the **whole** paste, with every offender
//! named and located, before one key is submitted. That is §10.2's rule and it is the reference's
//! behaviour inverted: the reference skips a character it cannot type and pastes the rest, which
//! on a console means a command that is *almost* what the user copied. The compile happens before
//! any step is handed to the producer, so "nothing was sent" is structural rather than a promise.
//!
//! # CapsLock: refuse, and why refuse rather than compensate
//!
//! D2 gave `nanokvm type` three policies — `refuse` (the default), `ignore`, `compensate` — and
//! the viewer's paste takes **refuse**, with no way to override it from the chrome. Three reasons,
//! in the order they weigh:
//!
//! 1. **A paste has no length cap and therefore no bounded duration.** `type` is one short
//!    argument delivered in a second or two, so the lock state it read a moment ago is still true
//!    when the last character lands. A paste is whatever is on the clipboard: at
//!    [`crate::script::REPORT_DELAY_MS`] a two-thousand-character paste runs for minutes, and
//!    `compensate` is a decision made once at the start and then applied to every letter. If
//!    anything toggles CapsLock during those minutes — the target's own software, a second
//!    keyboard, the user — the compensation inverts from a fix into the bug, silently, for the
//!    remainder. Refusing has no such failure mode: the state is read, and either the paste does
//!    not start or it starts from a state that was actually off.
//! 2. **`compensate` types something other than what was copied.** It sends the *opposite* shift
//!    bit and relies on the target to invert it back. That is an approximation of the kind §10.2
//!    rules out everywhere else in this client, accepted for `type` only because a human typed the
//!    flag for that one invocation and can look at the result. Nobody types a flag for a menu item.
//! 3. **It is `type`'s own default.** One rule, and the wording of the refusal is D2's wording, so
//!    a user who has met the CLI's refusal meets the same sentence and the same remedy here.
//!
//! **The wrong case can never be typed silently**, and that is structural rather than a claim:
//! [`compile`] compiles the text twice, once under [`CapsLock::Off`] and once under
//! [`CapsLock::Compensate`], and refuses whenever the target reports CapsLock on *and the two
//! differ* — which is the compiler's own definition of "this text contains a character CapsLock
//! changes", never a list of letters restated here. A clipboard of pure punctuation therefore
//! pastes with CapsLock on, correctly, because the lock bit cannot change what it types.
//!
//! It also refuses when the lock state is **unknown**. The viewer asks the writer for a fresh
//! `GET_INFO` before every paste ([`crate::input::Producer::refresh_device_info`]) precisely so
//! this is not decided from the reading the link happened to be commissioned with; a device that
//! did not answer leaves the question open, and an open question about the target's keyboard is
//! not something to type through.
//!
//! # Admission: a job, advanced by the event loop (§2.9, §2.8)
//!
//! [`PasteJob`] is a state machine and not a loop. D1 established that a *script* is a blocking,
//! paced sequence of acknowledged transactions on its own link — and that path is not available
//! here for the reason `super::shortcut`'s docs give: the viewer's writer thread owns the serial
//! link, and a second writer on a chip with no inter-byte timeout (§5.1) is the "a truncated
//! serial write corrupts the next command" hazard with two guns. Nor may the event loop block
//! (§5.4). So the paste is submitted through the viewer's own [`crate::input::Producer`], one key
//! transition per [`PasteJob::next_step`], at [`crate::script::REPORT_DELAY_MS`] — the rate
//! `nanokvm type` runs at, from the one constant both read.
//!
//! Feeding it one step at a time is also what keeps the bounded queue bounded (§2.8): one barrier
//! is submitted per 40 ms against a writer that acknowledges a keyboard report in about 4 ms
//! (A11), so the queue depth a paste produces is one. A refusal is still possible — an overflow
//! raised by something else, a link that died — and a refusal is a **job failure** surfaced in the
//! chrome, never a dropped keystroke: §2.8 item 3, "never a partially delivered sequence reported
//! as success".
//!
//! # The prelude, and why it is not a §2.6 release-all
//!
//! §12 Stage 4c asks for an explicit release at the start "so nothing the user still held (e.g.
//! the Shift of the trigger chord) corrupts the text". It cannot be
//! [`crate::input::Producer::request_release_all`]: that is a §2.6 *cancellation*, which
//! disengages the producer and ends the capture session — the paste would release the keyboard and
//! then have nowhere to type. What it is instead is an explicit **up edge for everything the
//! target is holding because this viewer forwarded the press**, which is exactly
//! [`super::route::Outstanding`]: every key, and then every mouse button. The last of those ups
//! leaves the target's keyboard and mouse reports zeroed, which is the state a release-all would
//! have left them in, and the session is still engaged.
//!
//! The buttons are in it for the same reason as the keys, only worse: a paste runs for minutes and
//! a button left down is a drag on a live desktop for all of them. Nothing new is *pressed* — a
//! release for a button this viewer already forwarded a press for is the completion of that press,
//! never a click.
//!
//! It is a prefix of the job's own steps, so it is counted in the progress and paced like the
//! rest; and it is a no-op on the wire when nothing is held, because the writer's held-state
//! tracker emits no report for a transition that changes nothing (§2.5).
//!
//! # And it ends with nothing held
//!
//! [`crate::script::Script::tap`] is always a press followed by `RELEASE_ALL`, and
//! [`super::shortcut::transitions`] differences that into an explicit up for every modifier and
//! every usage the press put down. [`tests::a_completed_paste_ends_with_nothing_held`] replays a
//! whole job through [`PasteJob::held`] and asserts it is empty — the same assertion
//! `shortcut::tests::every_builtin_ends_with_nothing_held` makes, for the same reason.

use std::time::{Duration, Instant};

use super::route::Outstanding;
use crate::input::{Event, ReleaseReason};
use crate::script::{compile_type, CapsLock, CompileError, Layout, REPORT_DELAY_MS};

/// The rate a paste is typed at: [`crate::script::REPORT_DELAY_MS`] per key transition.
pub const PACE: Duration = Duration::from_millis(REPORT_DELAY_MS);

/// How long the two answers a paste waits for — the clipboard and a fresh `GET_INFO` — are given
/// before it is refused.
///
/// Here rather than in `viewer::app` because [`Refusal::DeviceTimeout`] names it in the sentence
/// the user reads, and a refusal saying "within 3 s" while the event loop waited for some other
/// number would be a sentence the code no longer means.
pub const PREPARE_TIMEOUT: Duration = Duration::from_secs(3);

/// The sentence every refusal ends with (§2.8 item 3: a refusal is never a partial paste).
///
/// Lower case because most variants reach it after a semicolon; the two that reach it after a full
/// stop capitalise it themselves, which is D2's own wording in the CLI. What
/// [`tests::every_refusal_ends_with_the_assurance`] asserts is the sentence, not its first letter.
const NOTHING_WAS_SENT: &str = "no keyboard report was sent.";

/// The chord that starts a paste, spelled from the viewer's release binding.
///
/// Derived rather than written down, so it cannot disagree with the key
/// [`crate::viewer::input_map::map_key`] actually looks at: §12 Stage 4c puts the trigger on the
/// **already-reserved** release key so the never-forwarded surface stays one key, and a menu
/// advertising a different one would send the user to a key that does nothing.
pub fn chord(release_key: &str) -> String {
    format!("Shift+{release_key}")
}

/// Why a paste was refused, before anything was sent.
///
/// Every variant is a sentence the chrome shows. They are enumerated rather than stringly typed
/// because the tests assert on the *reason*, and because [`Refusal::Unreachable`] carries the
/// offender list the chrome renders as its own lines (§10.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Keys can only reach the target while the session is captured; `Producer::submit` refuses
    /// otherwise (§2.6).
    NotCaptured,
    /// The compositor has no data-control protocol, or the read failed. Carries
    /// [`super::clipboard`]'s own sentence.
    Clipboard { reason: String },
    /// The clipboard held text, and it was empty. Nothing to type is not a failure worth a stack
    /// trace, but it is worth saying: a paste that did nothing and said nothing looks broken.
    Empty,
    /// §10.2: at least one character has no key on the declared layout. **Nothing was sent.**
    Unreachable {
        layout: Layout,
        /// Each offending character once, with the 1-based position of its first occurrence, in
        /// the order they first occur — [`CompileError::Unreachable`]'s own list.
        chars: Vec<(char, usize)>,
    },
    /// The target reports CapsLock on and the text contains characters it would invert (D2).
    CapsLockOn,
    /// The device has not answered `GET_INFO`, so the lock state is unknown.
    LockStateUnknown,
    /// The device did not answer the `GET_INFO` refresh within [`PREPARE_TIMEOUT`].
    ///
    /// Its own variant rather than a [`Refusal::Clipboard`] carrying a sentence: the two halves of
    /// the preparation time out for different reasons and send a reader to different places — one
    /// is the application that owns the clipboard, the other is the link to the dongle — and a
    /// device timeout is a [`Refusal::LockStateUnknown`] with a cause, not a clipboard problem.
    DeviceTimeout,
    /// Any other compile failure. Structurally unreachable for `compile_type` — its only errors
    /// are `NothingToType`, `Unreachable` and `Unforwarded` — and carried rather than unwrapped,
    /// because a panicking menu item in a viewer holding someone's console is not a trade worth
    /// making.
    Compile { reason: String },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::NotCaptured => write!(
                f,
                "input is not captured, so nothing can be typed. Click in the window, or press \
                 Enter, to capture; {NOTHING_WAS_SENT}"
            ),
            // The reason is another module's sentence and may or may not be punctuated, so the
            // trailing stop is taken off before the one assurance is put on.
            Refusal::Clipboard { reason } => write!(
                f,
                "{}; {NOTHING_WAS_SENT}",
                reason.trim_end().trim_end_matches(['.', ';'])
            ),
            Refusal::Empty => write!(
                f,
                "the clipboard is empty; there is nothing to type, and {NOTHING_WAS_SENT}"
            ),
            Refusal::Unreachable { layout, chars } => {
                let n = chars.len();
                let subject = if n == 1 {
                    "character is"
                } else {
                    "characters are"
                };
                write!(
                    f,
                    "{n} {subject} not reachable on layout {layout}. Text injection needs a key \
                     for every character (§10.2); {NOTHING_WAS_SENT}"
                )
            }
            // D2's words, and D2's remedies. The CLI's refusal says the same thing, so a user who
            // has met one has met both.
            Refusal::CapsLockOn => f.write_str(
                "the target reports CapsLock ON; letters would be typed inverted. Turn it off on \
                 the target, or with \"nanokvm key capslock\". No keyboard report was sent.",
            ),
            Refusal::LockStateUnknown => f.write_str(
                "the device has not reported the target's lock bits, so whether letters would \
                 arrive inverted is unknown. No keyboard report was sent.",
            ),
            Refusal::DeviceTimeout => write!(
                f,
                "the device did not answer GET_INFO within {} s, so the target's lock bits are \
                 unknown and whether letters would arrive inverted cannot be decided. No keyboard \
                 report was sent.",
                PREPARE_TIMEOUT.as_secs()
            ),
            Refusal::Compile { reason } => write!(f, "{reason}; {NOTHING_WAS_SENT}"),
        }
    }
}

impl Refusal {
    /// The offending characters, one line each, for the chrome to list under the refusal.
    ///
    /// Empty for every variant but [`Refusal::Unreachable`]. Each line names the character as the
    /// user can recognise it, its codepoint — the only unambiguous name available without a
    /// Unicode name table — and where it is.
    pub fn offenders(&self) -> Vec<String> {
        let Refusal::Unreachable { chars, .. } = self else {
            return Vec::new();
        };
        chars
            .iter()
            .map(|&(c, at)| format!("{} at position {at}", describe_char(c)))
            .collect()
    }

    /// What this refusal is **logged** as, which is deliberately not what the chrome shows.
    ///
    /// [`Refusal::offenders`] names characters out of the user's clipboard, and a log is durable,
    /// shared and often pasted into a bug report — so the log gets the count and a pointer to the
    /// window, and the characters themselves stay in the popover, on screen, in front of the
    /// person who copied them (`super::clipboard`'s "never logged, printed or persisted").
    ///
    /// [`std::fmt::Display`] is content-free for every variant — [`Refusal::Unreachable`] carries
    /// its offenders in a field the sentence does not interpolate — so this is the sentence plus,
    /// when there are offenders, where to look for them.
    pub fn log_line(&self) -> String {
        let n = self.offenders().len();
        if n == 0 {
            return self.to_string();
        }
        format!("{self} {n} unreachable characters; see the Keyboard popover")
    }
}

/// One character, named so a person can find it in their clipboard.
///
/// The codepoint is always there because it is the only unambiguous identifier; a printable
/// character is also shown as itself, and the handful of control characters that survive
/// normalisation are named, because `'\u{7}'` tells nobody anything.
fn describe_char(c: char) -> String {
    let name = match c {
        '\t' => Some("tab"),
        '\n' => Some("newline"),
        '\r' => Some("carriage return"),
        '\0' => Some("null"),
        '\x1b' => Some("escape"),
        '\u{a0}' => Some("no-break space"),
        '\u{2028}' => Some("line separator"),
        '\u{feff}' => Some("byte order mark"),
        _ => None,
    };
    match name {
        Some(name) => format!("{name} (U+{:04X})", c as u32),
        None if c.is_control() => format!("control character (U+{:04X})", c as u32),
        None => format!("{c:?} (U+{:04X})", c as u32),
    }
}

/// `\r\n` and a bare `\r` become `\n`; nothing else changes.
///
/// Separate and public so the rule is testable on its own, and so nothing else in the file is
/// tempted to normalise a second time.
pub fn normalise(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            // `\r\n` is one newline, not two: consuming the `\n` is what makes that true.
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

/// The target's lock bits as the viewer knows them, or `None` when the device has not said.
///
/// A `bool` would conflate "the device says CapsLock is off" with "nobody has asked", and those
/// are the two sides of [`Refusal::LockStateUnknown`].
pub type CapsLockState = Option<bool>;

/// Turn clipboard text into the key transitions that type it, or refuse.
///
/// Every refusal happens here, before a single step exists, which is what makes "nothing was sent"
/// structural (§2.8 item 3).
pub fn compile(
    text: &str,
    layout: Layout,
    caps_lock: CapsLockState,
) -> Result<Vec<Event>, Refusal> {
    let text = normalise(text);
    if text.is_empty() {
        return Err(Refusal::Empty);
    }
    let script = match compile_type(&text, layout, CapsLock::Off) {
        Ok(script) => script,
        Err(CompileError::Unreachable { layout, chars }) => {
            return Err(Refusal::Unreachable { layout, chars })
        }
        Err(CompileError::NothingToType) => return Err(Refusal::Empty),
        Err(e) => {
            return Err(Refusal::Compile {
                reason: e.to_string(),
            })
        }
    };

    // The CapsLock decision, asked of the compiler rather than restated (module docs). The second
    // compilation cannot fail where the first succeeded — it differs only in a shift bit — but the
    // error is propagated rather than unwrapped for the same reason as above.
    let compensated =
        compile_type(&text, layout, CapsLock::Compensate).map_err(|e| Refusal::Compile {
            reason: e.to_string(),
        })?;
    let caps_matters = compensated != script;
    match caps_lock {
        None if caps_matters => return Err(Refusal::LockStateUnknown),
        Some(true) if caps_matters => return Err(Refusal::CapsLockOn),
        _ => {}
    }

    Ok(super::shortcut::transitions(&script))
}

/// How a paste ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteOutcome {
    /// Every step was submitted.
    Done { total: usize },
    /// The release key cancelled it (§2.6). Carries how far it got, because "it was cancelled" and
    /// "it was cancelled after 37 of 214 keys" are different facts to the person at the console.
    Cancelled { sent: usize, total: usize },
    /// A step was refused, or the session ended under it (§2.8 item 3: never a partially delivered
    /// sequence reported as success).
    Failed {
        reason: String,
        sent: usize,
        total: usize,
    },
}

impl std::fmt::Display for PasteOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PasteOutcome::Done { total } => write!(f, "done, {total} keys"),
            PasteOutcome::Cancelled { sent, total } => {
                write!(f, "cancelled after {sent} of {total} keys")
            }
            PasteOutcome::Failed {
                reason,
                sent,
                total,
            } => write!(f, "failed after {sent} of {total} keys: {reason}"),
        }
    }
}

/// How a §2.6 release-all ends the paste it interrupts.
///
/// A release-all is raised for seven reasons and they are not the same event: the user asking for
/// their keyboard back is a cancellation, and a queue that overflowed or a link that died is a
/// **failure** — §2.8 item 3's "never a partially delivered sequence reported as success" applies
/// to the words as much as to the keys. The viewer cannot tell them apart after the fact, because
/// the release runs synchronously inside the reducer's action; so the reason is carried in, here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ending {
    /// The user, or a clean shutdown. [`PasteOutcome::Cancelled`].
    Cancelled,
    /// The session failed under the paste. [`PasteOutcome::Failed`], with this sentence.
    Failed(String),
}

/// What a release-all raised for `reason` does to a paste that is running under it.
///
/// The split is "did a person ask for this?". `UserRequested` is the release key — which is also
/// the cancel — `FocusLost` is the user's own window switch, and `Shutdown` is the user closing
/// the window; those three are cancellations and say how far the paste got. Everything else is the
/// session failing underneath it (§2.8, §2.6.1, §2.7), and a paste that stopped because the queue
/// overflowed or the cable went must say so, not report itself as cancelled.
pub fn ending_for(reason: ReleaseReason) -> Ending {
    match reason {
        ReleaseReason::UserRequested | ReleaseReason::FocusLost | ReleaseReason::Shutdown => {
            Ending::Cancelled
        }
        ReleaseReason::Overflow => Ending::Failed(
            "the input queue overflowed under the paste (§2.8); re-capture deliberately and paste \
             again"
                .to_string(),
        ),
        ReleaseReason::LinkDown => Ending::Failed(
            "the link to the device failed under the paste (§2.6.1); what had been sent may not \
             have reached the target"
                .to_string(),
        ),
        ReleaseReason::Reconnected => Ending::Failed(
            "the link was replaced under the paste (§2.7), so the target's HID state was \
             resynchronised; re-capture deliberately and paste again"
                .to_string(),
        ),
        ReleaseReason::CaptureReleased => {
            Ending::Failed("input capture ended under the paste".to_string())
        }
    }
}

/// What a cancellation produced, and what the caller still owes the target.
///
/// `#[must_use]` because the second field is an obligation: a caller that takes the outcome and
/// drops this has cancelled a paste with keys still down on someone else's console. The viewer
/// discharges it with §2.6's release-all, which the release key already triggers through the
/// reducer; this type is what makes the obligation visible to a reader and to a test.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "the keys in `still_held` are down on the target until something releases them (§2.6)"]
pub struct Cancellation {
    pub outcome: PasteOutcome,
    /// What the paste had pressed and not yet released when it was cancelled.
    pub still_held: Outstanding,
}

/// How far a running paste has got, for the chrome's progress line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasteProgress {
    pub sent: usize,
    pub total: usize,
    /// Steps left times the pace. An estimate, and named as one in the line: a step whose report
    /// is identical to the last changes nothing and costs no transaction (§2.5), so the real time
    /// is a little under this.
    pub remaining: Duration,
}

impl std::fmt::Display for PasteProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} / {} keys, ~{} s remaining",
            self.sent,
            self.total,
            self.remaining.as_secs()
        )
    }
}

/// A paste in progress: a pure state machine over a list of key transitions.
///
/// The caller advances it — [`PasteJob::next_step`] to ask what is due, [`PasteJob::delivered`]
/// when the producer accepted it, [`PasteJob::fail`] when it did not, [`PasteJob::cancel`] on the
/// release key — and reads [`PasteJob::progress`] for the chrome. It reads no clock: every method
/// that needs the time is given it, which is what lets a whole paste be replayed in a test in no
/// time at all.
#[derive(Debug, Clone)]
pub struct PasteJob {
    steps: Vec<Event>,
    /// How many leading steps are the release prelude (module docs). Kept so the chrome can say
    /// what the job is made of and so a test can assert the prelude is first.
    prelude: usize,
    sent: usize,
    pace: Duration,
    /// When the next step may go out. `None` means "now" — the first step is not delayed, because
    /// the pace is a gap *between* reports.
    next_due: Option<Instant>,
    held: Outstanding,
    outcome: Option<PasteOutcome>,
}

impl PasteJob {
    /// A job that releases everything in `outstanding` and then types `body`.
    ///
    /// `outstanding` is what the target holds because this viewer forwarded the press — typically
    /// the Shift of the `Shift+Pause` chord that started the paste. The prelude is the up edge of
    /// each of them (module docs): every key, then every **button**, because "the same state a
    /// release-all would have left" includes the mouse buttons and a paste that left one down
    /// would leave a live desktop dragging for the minutes it runs for.
    pub fn new(outstanding: Outstanding, body: Vec<Event>, pace: Duration) -> PasteJob {
        let mut prelude: Vec<Event> = outstanding
            .held_keys()
            .into_iter()
            .map(|key| Event::Key { key, down: false })
            .collect();
        prelude.extend(
            outstanding
                .held_buttons()
                .into_iter()
                .map(|button| Event::Button {
                    button,
                    down: false,
                }),
        );
        let prelude_len = prelude.len();
        let mut steps = prelude;
        steps.extend(body);
        PasteJob {
            steps,
            prelude: prelude_len,
            sent: 0,
            pace,
            next_due: None,
            held: Outstanding::default(),
            outcome: None,
        }
    }

    /// Every step, prelude included.
    pub fn total(&self) -> usize {
        self.steps.len()
    }

    /// How many leading steps release what the user was holding.
    pub fn prelude(&self) -> usize {
        self.prelude
    }

    /// How many steps have been accepted by the producer.
    pub fn sent(&self) -> usize {
        self.sent
    }

    /// How many are left.
    pub fn remaining(&self) -> usize {
        self.total() - self.sent
    }

    /// The whole paste at the paced rate — §12 Stage 4c: "state the expected duration in the
    /// progress line".
    pub fn expected_duration(&self) -> Duration {
        self.pace * u32::try_from(self.total()).unwrap_or(u32::MAX)
    }

    /// What is left of it.
    pub fn remaining_duration(&self) -> Duration {
        self.pace * u32::try_from(self.remaining()).unwrap_or(u32::MAX)
    }

    /// The line the chrome shows while this runs.
    pub fn progress(&self) -> PasteProgress {
        PasteProgress {
            sent: self.sent,
            total: self.total(),
            remaining: self.remaining_duration(),
        }
    }

    /// Whether it is still going.
    pub fn is_running(&self) -> bool {
        self.outcome.is_none()
    }

    /// How it ended, once it has.
    pub fn outcome(&self) -> Option<&PasteOutcome> {
        self.outcome.as_ref()
    }

    /// What the target is holding because of this paste.
    pub fn held(&self) -> Outstanding {
        self.held
    }

    /// The step due at `now`, or `None` when the job is finished or the pace has not elapsed.
    ///
    /// Pure: it neither mutates nor reads a clock. The caller submits what it returns and then
    /// calls [`PasteJob::delivered`] or [`PasteJob::fail`].
    pub fn next_step(&self, now: Instant) -> Option<Event> {
        if self.outcome.is_some() {
            return None;
        }
        if self.next_due.is_some_and(|due| now < due) {
            return None;
        }
        self.steps.get(self.sent).copied()
    }

    /// When the next step becomes due, for the event loop's `WaitUntil`. `None` while a step is
    /// due now or the job is over.
    pub fn due_at(&self) -> Option<Instant> {
        if self.outcome.is_some() {
            return None;
        }
        self.next_due
    }

    /// The producer accepted the step [`PasteJob::next_step`] returned.
    ///
    /// Advances the progress, records what the target now holds, and starts the pace for the next
    /// one. The job completes here, on the last step, so a caller that only ever polls sees the
    /// outcome without a separate "finish" call it could forget.
    pub fn delivered(&mut self, now: Instant) {
        if self.outcome.is_some() {
            return;
        }
        match self.steps.get(self.sent) {
            Some(&Event::Key { key, down }) => self.held.set_key(key, down),
            // The prelude's button releases, and nothing else: `compile` emits key transitions
            // only, so a button *press* here would be a construction bug.
            Some(&Event::Button {
                button,
                down: false,
            }) => self.held.set_button(button, false),
            _ => {
                // Anything else is a construction bug and is reported rather than silently
                // skipped.
                self.fail(
                    "a paste step was neither a key transition nor a button release".to_string(),
                );
                return;
            }
        }
        self.sent += 1;
        self.next_due = Some(now + self.pace);
        if self.sent == self.steps.len() {
            self.outcome = Some(PasteOutcome::Done { total: self.sent });
        }
    }

    /// The producer refused a step, or the session ended under the job (§2.8 item 3).
    pub fn fail(&mut self, reason: String) {
        if self.outcome.is_some() {
            return;
        }
        self.outcome = Some(PasteOutcome::Failed {
            reason,
            sent: self.sent,
            total: self.steps.len(),
        });
    }

    /// A §2.6 release-all ended it, as [`ending_for`] reads the reason.
    ///
    /// Returns what is still held, which the caller must release — the release-all that ended the
    /// job is what discharges it. Ending an already-finished job is a no-op that reports the
    /// outcome it already had.
    pub fn end(&mut self, ending: Ending) -> Cancellation {
        if self.outcome.is_none() {
            self.outcome = Some(match ending {
                Ending::Cancelled => PasteOutcome::Cancelled {
                    sent: self.sent,
                    total: self.steps.len(),
                },
                Ending::Failed(reason) => PasteOutcome::Failed {
                    reason,
                    sent: self.sent,
                    total: self.steps.len(),
                },
            });
        }
        Cancellation {
            outcome: self.outcome.clone().expect("set just above"),
            still_held: self.held,
        }
    }

    /// The release key cancelled it (§2.6): [`PasteJob::end`] with [`Ending::Cancelled`].
    pub fn cancel(&mut self) -> Cancellation {
        self.end(Ending::Cancelled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::report::modifier;
    use crate::proto::HidKey;
    use std::collections::BTreeSet;

    fn events(text: &str) -> Vec<Event> {
        compile(text, Layout::Us, Some(false)).expect("compiles")
    }

    fn job(text: &str) -> PasteJob {
        PasteJob::new(Outstanding::default(), events(text), PACE)
    }

    /// Replay a transition list through a held set, asserting every release matches a press.
    fn replay(events: &[Event]) -> BTreeSet<(bool, u8)> {
        let mut held = BTreeSet::new();
        for e in events {
            let Event::Key { key, down } = e else {
                panic!("a paste must emit key events only, got {e:?}");
            };
            let id = match key {
                HidKey::Modifier(b) => (true, *b),
                HidKey::Usage(u) => (false, *u),
            };
            if *down {
                assert!(held.insert(id), "{id:?} was pressed while already held");
            } else {
                held.remove(&id);
            }
        }
        held
    }

    // ---- normalisation -------------------------------------------------------------------

    #[test]
    fn crlf_and_bare_cr_both_become_one_newline() {
        assert_eq!(normalise("a\r\nb"), "a\nb");
        assert_eq!(normalise("a\rb"), "a\nb");
        assert_eq!(normalise("a\r\n\r\nb"), "a\n\nb");
        assert_eq!(
            normalise("a\n\rb"),
            "a\n\nb",
            "a bare CR after an LF is its own"
        );
        assert_eq!(normalise("a\r"), "a\n");
        assert_eq!(normalise("plain"), "plain");
    }

    #[test]
    fn a_tab_is_a_tab_and_is_not_expanded() {
        // The layout has a key for it, so it is typed rather than approximated with spaces.
        assert_eq!(normalise("a\tb"), "a\tb");
        let tab = crate::proto::keymap::hid_key(
            crate::script::keynames::key_by_name("tab").expect("tab is a key name"),
        )
        .expect("tab has a usage");
        assert!(
            events("\t").contains(&Event::Key {
                key: tab,
                down: true
            }),
            "a tab must be typed as Tab"
        );
    }

    #[test]
    fn a_crlf_clipboard_compiles_to_the_same_keys_as_an_lf_one() {
        assert_eq!(events("one\r\ntwo"), events("one\ntwo"));
    }

    // ---- the unreachable-character policy (§10.2) -----------------------------------------

    #[test]
    fn an_unreachable_character_refuses_the_whole_paste_and_names_it() {
        let err = compile("caf\u{e9} au lait", Layout::Us, Some(false)).expect_err("must refuse");
        let Refusal::Unreachable { layout, ref chars } = err else {
            panic!("expected Unreachable, got {err:?}");
        };
        assert_eq!(layout, Layout::Us);
        assert_eq!(chars, &[('\u{e9}', 4)]);
        let offenders = err.offenders();
        assert_eq!(offenders.len(), 1);
        assert!(offenders[0].contains("U+00E9"), "{offenders:?}");
        assert!(offenders[0].contains("at position 4"), "{offenders:?}");
        let message = err.to_string();
        assert!(message.contains("layout us"), "{message}");
        assert!(message.contains("no keyboard report was sent"), "{message}");
    }

    #[test]
    fn every_offender_is_listed_once_in_the_order_it_first_occurs() {
        let err = compile("\u{e9}x\u{2192}y\u{e9}", Layout::Us, Some(false)).expect_err("refuses");
        let offenders = err.offenders();
        assert_eq!(offenders.len(), 2, "{offenders:?}");
        assert!(offenders[0].contains("U+00E9") && offenders[0].contains("at position 1"));
        assert!(offenders[1].contains("U+2192") && offenders[1].contains("at position 3"));
    }

    /// A control character that survives normalisation is named rather than shown, because
    /// `'\u{7}'` in a menu tells nobody anything.
    #[test]
    fn a_control_character_offender_is_named() {
        let err = compile("a\u{7}b", Layout::Us, Some(false)).expect_err("refuses");
        let offenders = err.offenders();
        assert_eq!(offenders.len(), 1);
        assert!(
            offenders[0].starts_with("control character (U+0007)"),
            "{offenders:?}"
        );
    }

    #[test]
    fn every_printable_ascii_is_reachable() {
        // The hardware exit criterion's own text: 0x20..=0x7E, which must compile.
        let text: String = (0x20u8..=0x7e).map(char::from).collect();
        let events = compile(&text, Layout::Us, Some(false)).expect("all printable ASCII compiles");
        assert!(replay(&events).is_empty(), "nothing may be left held");
    }

    #[test]
    fn an_empty_clipboard_is_refused_rather_than_typed() {
        assert_eq!(compile("", Layout::Us, Some(false)), Err(Refusal::Empty));
    }

    // ---- CapsLock (D2) ---------------------------------------------------------------------

    /// The load-bearing one: with the target's CapsLock on, letters are **never** typed. Not
    /// inverted, not compensated — refused, with the lock state named.
    #[test]
    fn caps_lock_on_refuses_text_with_letters_and_names_the_state() {
        let err = compile("echo hello", Layout::Us, Some(true)).expect_err("must refuse");
        assert_eq!(err, Refusal::CapsLockOn);
        let message = err.to_string();
        assert!(message.contains("CapsLock ON"), "{message}");
        assert!(message.contains("nanokvm key capslock"), "{message}");
        assert!(
            message.contains("No keyboard report was sent."),
            "{message}"
        );
    }

    /// …and with it off, the same text compiles, so the refusal is about the lock bit and not
    /// about the text.
    #[test]
    fn caps_lock_off_types_the_same_text() {
        assert!(compile("echo hello", Layout::Us, Some(false)).is_ok());
    }

    /// Text the lock bit cannot change is typed with CapsLock on. The question is asked of the
    /// compiler — two compilations, compared — so "which characters does CapsLock affect" is never
    /// restated here and cannot drift from `script`.
    #[test]
    fn caps_lock_on_still_types_text_with_no_letters_in_it() {
        assert!(compile("1234 !@#$ ...", Layout::Us, Some(true)).is_ok());
    }

    /// An unknown lock state is refused exactly like a known-on one, for the same reason: the
    /// alternative is typing letters that may arrive inverted without saying so.
    #[test]
    fn an_unknown_lock_state_refuses_letters_but_not_punctuation() {
        assert_eq!(
            compile("echo hello", Layout::Us, None),
            Err(Refusal::LockStateUnknown)
        );
        assert!(compile("1234", Layout::Us, None).is_ok());
    }

    /// The wrong case can never be typed silently, over every lock state there is: for any text,
    /// either the compile refuses or the transitions it produced are byte-identical to the ones a
    /// known-off CapsLock produces. There is no third answer in which something else goes out.
    #[test]
    fn the_wrong_case_is_never_typed_silently() {
        for text in ["Hello", "echo A", "ABC", "a", "1234", "!@#", "mIxEd CaSe"] {
            let reference = compile(text, Layout::Us, Some(false));
            for state in [Some(true), None] {
                match compile(text, Layout::Us, state) {
                    Ok(events) => assert_eq!(
                        Ok(events),
                        reference,
                        "{text:?} under {state:?} typed something other than what was copied"
                    ),
                    Err(e) => assert!(
                        matches!(e, Refusal::CapsLockOn | Refusal::LockStateUnknown),
                        "{text:?} under {state:?}: {e:?}"
                    ),
                }
            }
        }
    }

    // ---- the job ---------------------------------------------------------------------------

    /// The `every_builtin_ends_with_nothing_held` analogue: run a whole paste and nothing is down.
    #[test]
    fn a_completed_paste_ends_with_nothing_held() {
        let mut j = job("Hello, World!\n");
        let mut now = Instant::now();
        while let Some(step) = j.next_step(now) {
            assert!(matches!(step, Event::Key { .. }));
            j.delivered(now);
            now += PACE;
        }
        assert_eq!(j.outcome(), Some(&PasteOutcome::Done { total: j.total() }));
        assert!(
            j.held().is_empty(),
            "a finished paste left {:?} held",
            j.held()
        );
        assert_eq!(j.remaining(), 0);
    }

    /// The prelude is the up edge of everything the target already holds, and it comes first.
    #[test]
    fn the_job_begins_by_releasing_what_the_user_was_holding() {
        let mut outstanding = Outstanding::default();
        outstanding.set_key(HidKey::Modifier(modifier::LEFT_SHIFT), true);
        outstanding.set_key(HidKey::Usage(0x04), true);
        let mut j = PasteJob::new(outstanding, events("a"), PACE);
        assert_eq!(j.prelude(), 2, "one step per held key");
        let now = Instant::now();
        let first = j.next_step(now).expect("a step is due at once");
        assert!(
            matches!(first, Event::Key { down: false, .. }),
            "the first step must be a release, got {first:?}"
        );
        // Both prelude steps are releases, and neither is a press.
        j.delivered(now);
        let second = j.next_step(now + PACE).expect("the second prelude step");
        assert!(
            matches!(second, Event::Key { down: false, .. }),
            "{second:?}"
        );
    }

    /// With nothing held there is no prelude, because there is nothing to release.
    #[test]
    fn an_empty_outstanding_set_produces_no_prelude() {
        assert_eq!(job("a").prelude(), 0);
    }

    /// **The prelude releases held buttons too, or it is not a release-all.** A paste runs for
    /// minutes; a button the viewer forwarded a press for and never released is a drag on a live
    /// desktop for every one of them. Only releases are ever emitted — a press would be a click
    /// this client never sends by itself.
    #[test]
    fn the_prelude_releases_the_buttons_the_target_is_holding() {
        use crate::proto::report::button;
        let mut outstanding = Outstanding::default();
        outstanding.set_key(HidKey::Modifier(modifier::LEFT_SHIFT), true);
        outstanding.set_button(button::LEFT, true);
        outstanding.set_button(button::RIGHT, true);

        let mut j = PasteJob::new(outstanding, events("a"), PACE);
        assert_eq!(j.prelude(), 3, "one step per held key and per held button");
        let mut now = Instant::now();
        let mut prelude = Vec::new();
        for _ in 0..j.prelude() {
            prelude.push(j.next_step(now).expect("a prelude step"));
            j.delivered(now);
            now += PACE;
        }
        assert!(
            prelude.contains(&Event::Button {
                button: button::LEFT,
                down: false
            }) && prelude.contains(&Event::Button {
                button: button::RIGHT,
                down: false
            }),
            "both buttons must be released: {prelude:?}"
        );
        assert!(
            prelude.iter().all(|e| matches!(
                e,
                Event::Key { down: false, .. } | Event::Button { down: false, .. }
            )),
            "the prelude presses nothing: {prelude:?}"
        );
        // And the target is left holding neither by the end of the job.
        while j.next_step(now).is_some() {
            j.delivered(now);
            now += PACE;
        }
        assert!(j.held().is_empty(), "{:?} left held", j.held());
    }

    #[test]
    fn the_pace_is_a_gap_between_steps_and_the_first_one_is_not_delayed() {
        let mut j = job("ab");
        let t0 = Instant::now();
        assert!(j.next_step(t0).is_some(), "the first step is due at once");
        j.delivered(t0);
        assert!(j.next_step(t0).is_none(), "the second must wait the pace");
        assert!(j.next_step(t0 + PACE / 2).is_none());
        assert!(j.next_step(t0 + PACE).is_some());
        assert_eq!(j.due_at(), Some(t0 + PACE));
    }

    #[test]
    fn the_expected_duration_is_the_step_count_times_the_pace() {
        let j = job("hello");
        assert_eq!(j.expected_duration(), PACE * j.total() as u32);
        assert_eq!(j.remaining_duration(), j.expected_duration());
    }

    #[test]
    fn the_progress_line_reads_as_keys_and_seconds() {
        let mut j = job("hello");
        let now = Instant::now();
        j.delivered(now);
        let line = j.progress().to_string();
        assert!(line.starts_with("1 / "), "{line}");
        assert!(line.contains("keys, ~"), "{line}");
        assert!(line.ends_with(" s remaining"), "{line}");
    }

    /// Cancel at every point there is. The outcome names how far it got, and what is still held is
    /// exactly the replay of the delivered prefix — which the caller's release-all then clears.
    #[test]
    fn cancelling_from_any_state_reports_its_progress_and_leaves_nothing_held_after_the_release() {
        let body = events("Hi!\n");
        for cut in 0..=body.len() {
            let mut j = PasteJob::new(Outstanding::default(), body.clone(), PACE);
            let mut now = Instant::now();
            for _ in 0..cut {
                j.delivered(now);
                now += PACE;
            }
            let total = j.total();
            let cancellation = j.cancel();
            if cut == total {
                // A job that had already finished keeps the outcome it had: cancelling after the
                // last step did not cancel anything.
                assert_eq!(cancellation.outcome, PasteOutcome::Done { total });
            } else {
                assert_eq!(
                    cancellation.outcome,
                    PasteOutcome::Cancelled { sent: cut, total }
                );
            }
            assert!(!j.is_running());
            assert!(
                j.next_step(now).is_none(),
                "a cancelled job offers no more steps"
            );
            // What the caller owes, and the §2.6 release-all discharging it.
            let mut owed = cancellation.still_held;
            assert_eq!(owed, replay_outstanding(&body[..cut]));
            owed.clear();
            assert!(owed.is_empty());
        }
    }

    /// The same replay as `replay`, in the type the routing rule uses.
    fn replay_outstanding(events: &[Event]) -> Outstanding {
        let mut held = Outstanding::default();
        for e in events {
            if let Event::Key { key, down } = e {
                held.set_key(*key, *down);
            }
        }
        held
    }

    #[test]
    fn a_failed_step_ends_the_job_and_says_how_far_it_got() {
        let mut j = job("hello");
        let now = Instant::now();
        j.delivered(now);
        j.delivered(now);
        j.fail("input queue overflowed".to_string());
        let PasteOutcome::Failed {
            ref reason,
            sent,
            total,
        } = *j.outcome().expect("failed")
        else {
            panic!("expected Failed, got {:?}", j.outcome());
        };
        assert_eq!((sent, total), (2, j.total()));
        assert!(reason.contains("overflow"), "{reason}");
        assert!(
            j.next_step(now).is_none(),
            "a failed job offers no more steps"
        );
        assert!(j
            .outcome()
            .expect("failed")
            .to_string()
            .contains("failed after 2 of"));
    }

    #[test]
    fn an_outcome_is_final() {
        let mut j = job("hi");
        j.fail("first".to_string());
        j.fail("second".to_string());
        let _ = j.cancel();
        assert!(
            j.outcome().expect("failed").to_string().contains("first"),
            "the first outcome wins: {:?}",
            j.outcome()
        );
    }

    /// Every outcome reads as a sentence with its numbers in it.
    #[test]
    fn the_outcome_lines_carry_their_progress() {
        assert_eq!(
            PasteOutcome::Done { total: 12 }.to_string(),
            "done, 12 keys"
        );
        assert_eq!(
            PasteOutcome::Cancelled { sent: 3, total: 12 }.to_string(),
            "cancelled after 3 of 12 keys"
        );
        assert_eq!(
            PasteOutcome::Failed {
                reason: "link is down".to_string(),
                sent: 3,
                total: 12
            }
            .to_string(),
            "failed after 3 of 12 keys: link is down"
        );
    }

    /// A refusal that is not about the text still says nothing was sent, so the chrome never shows
    /// a failure a reader could take for a partial paste.
    #[test]
    fn every_refusal_says_what_it_means() {
        assert!(Refusal::NotCaptured.to_string().contains("not captured"));
        assert!(Refusal::Empty.to_string().contains("nothing to type"));
        assert!(Refusal::Clipboard {
            reason: "no data-control".to_string()
        }
        .to_string()
        .contains("no data-control"));
        assert!(Refusal::Compile {
            reason: "wat".to_string()
        }
        .to_string()
        .contains("no keyboard report was sent"));
        // Only the unreachable-character refusal has offenders to list.
        assert!(Refusal::NotCaptured.offenders().is_empty());
    }

    /// Every refusal there is, for the exhaustive assertions below. Adding a variant without
    /// adding it here is a compile error, which is the point of the destructuring match.
    fn every_refusal() -> Vec<Refusal> {
        let all = vec![
            Refusal::NotCaptured,
            Refusal::Clipboard {
                reason: "the clipboard is empty".to_string(),
            },
            Refusal::Empty,
            Refusal::Unreachable {
                layout: Layout::Us,
                chars: vec![('\u{e9}', 4), ('\u{2192}', 17)],
            },
            Refusal::CapsLockOn,
            Refusal::LockStateUnknown,
            Refusal::DeviceTimeout,
            Refusal::Compile {
                reason: "wat".to_string(),
            },
        ];
        for r in &all {
            match r {
                Refusal::NotCaptured
                | Refusal::Clipboard { .. }
                | Refusal::Empty
                | Refusal::Unreachable { .. }
                | Refusal::CapsLockOn
                | Refusal::LockStateUnknown
                | Refusal::DeviceTimeout
                | Refusal::Compile { .. } => {}
            }
        }
        all
    }

    /// **Every refusal ends with the assurance**, because the sentence a user is owed when a paste
    /// does nothing is that nothing was *half* done (§2.8 item 3). It was missing from three of
    /// them, and the one the 3 s device timeout used to raise was one of the three.
    #[test]
    fn every_refusal_ends_with_the_assurance() {
        for refusal in every_refusal() {
            let line = refusal.to_string();
            assert!(
                line.to_lowercase().ends_with(NOTHING_WAS_SENT),
                "{refusal:?} does not end with the assurance: {line}"
            );
        }
    }

    /// **A refusal never carries the clipboard into the log.** The offending characters are the
    /// user's own text; the popover lists them and the log gets a count and somewhere to look.
    #[test]
    fn a_logged_refusal_carries_no_character_out_of_the_clipboard() {
        let refusal = Refusal::Unreachable {
            layout: Layout::Us,
            chars: vec![('\u{e9}', 4), ('\u{2192}', 17), ('\u{7}', 19)],
        };
        for line in [refusal.to_string(), refusal.log_line()] {
            for offender in ['\u{e9}', '\u{2192}', '\u{7}'] {
                assert!(
                    !line.contains(offender),
                    "{offender:?} reached a log line: {line}"
                );
            }
        }
        let logged = refusal.log_line();
        assert!(logged.contains("3 unreachable characters"), "{logged}");
        assert!(logged.contains("Keyboard popover"), "{logged}");
        // The popover's own list is where they are named, and it still names them.
        assert!(refusal.offenders().iter().any(|o| o.contains('\u{e9}')));
        // Everything else is logged as it reads.
        assert_eq!(
            Refusal::CapsLockOn.log_line(),
            Refusal::CapsLockOn.to_string()
        );
    }

    /// The two halves of the preparation timeout are two different refusals: one sends a reader to
    /// the application that owns the clipboard, the other to the link.
    #[test]
    fn the_device_half_of_the_timeout_is_not_a_clipboard_refusal() {
        let device = Refusal::DeviceTimeout.to_string();
        assert!(device.contains("GET_INFO"), "{device}");
        assert!(
            device.contains(&PREPARE_TIMEOUT.as_secs().to_string()),
            "it names the deadline it waited: {device}"
        );
        assert!(device.contains("lock bits"), "{device}");
        assert_ne!(Refusal::DeviceTimeout, Refusal::LockStateUnknown);
    }

    // ---- how a release-all ends a running paste (§2.6, §2.8 item 3) -------------------------

    /// **A session that failed under a paste is a failed paste, named.** The release-all runs
    /// inside the reducer's action, so the reason has to be read here or not at all; reporting an
    /// overflow as "cancelled" tells the user they stopped it themselves.
    #[test]
    fn a_release_all_for_a_failure_ends_the_job_as_failed() {
        for (reason, needle) in [
            (ReleaseReason::Overflow, "overflow"),
            (ReleaseReason::LinkDown, "link to the device failed"),
            (ReleaseReason::Reconnected, "link was replaced"),
            (ReleaseReason::CaptureReleased, "capture ended"),
        ] {
            let mut j = job("hello");
            let now = Instant::now();
            j.delivered(now);
            j.delivered(now);
            let cancellation = j.end(ending_for(reason));
            let PasteOutcome::Failed {
                reason: ref why,
                sent,
                ..
            } = cancellation.outcome
            else {
                panic!(
                    "{reason:?} must fail the paste, got {:?}",
                    cancellation.outcome
                );
            };
            assert_eq!(sent, 2, "it says how far it got");
            assert!(why.contains(needle), "{reason:?}: {why}");
        }
    }

    /// And the user's own triggers are cancellations, which is what the release key is.
    #[test]
    fn a_release_all_the_user_asked_for_ends_the_job_as_cancelled() {
        for reason in [
            ReleaseReason::UserRequested,
            ReleaseReason::FocusLost,
            ReleaseReason::Shutdown,
        ] {
            assert_eq!(ending_for(reason), Ending::Cancelled, "{reason:?}");
            let mut j = job("hello");
            j.delivered(Instant::now());
            assert_eq!(
                j.end(ending_for(reason)).outcome,
                PasteOutcome::Cancelled {
                    sent: 1,
                    total: j.total()
                },
                "{reason:?}"
            );
        }
    }
}
