//! Bounded Wayland clipboard reads on a separate connection.
//!
//! Data-control access does not depend on keyboard focus. A single outstanding fetch,
//! read deadline, and byte limit bound work from a slow or oversized clipboard owner.
//! Clipboard content is never logged.

use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use wl_clipboard_rs::paste::{get_contents, get_mime_types, ClipboardType, Error, MimeType, Seat};

/// Maximum clipboard bytes accepted. Bounds allocation from another process
/// before text compilation and paced delivery.
const MAX_BYTES: usize = 16 * 1024 * 1024;

/// How long the clipboard's owner is given to write it.
///
/// The same 3 s the event loop gives the whole preparation ([`super::paste::PREPARE_TIMEOUT`]), so
/// the reader thread gives up at about the moment the paste that asked for it does, rather than
/// outliving it by some other number.
pub const READ_DEADLINE: Duration = super::paste::PREPARE_TIMEOUT;

/// Whether a clipboard can be read at all, and why not when it cannot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// A data-control manager is there. It says nothing about the clipboard having anything in it
    /// — that is only known at trigger time, and an empty clipboard is a failed paste, not an
    /// unavailable one.
    Ready,
    /// No data-control manager, or the connection could not be made. The string is what the
    /// disabled menu item's tooltip says.
    Unsupported { reason: String },
}

impl Availability {
    /// Probe the compositor once, at startup.
    ///
    /// Asks for the MIME types on the clipboard, not its contents: that is one roundtrip, it
    /// reads no user data, and it fails with [`Error::MissingProtocol`] on precisely the
    /// compositors the menu item must be disabled for.
    ///
    /// An empty clipboard, a clipboard holding nothing textual, and a compositor with no seat are
    /// all [`Availability::Ready`]: the mechanism is there, and what is on the clipboard is a
    /// question for the moment the user asks to paste, by which time it may be something else.
    pub fn probe() -> Availability {
        match get_mime_types(ClipboardType::Regular, Seat::Unspecified) {
            // The types themselves are not read: knowing the protocol answered is the whole
            // question, and a list of MIME types is still a fact about what the user has copied.
            Ok(_) => Availability::Ready,
            Err(Error::ClipboardEmpty | Error::NoMimeType | Error::NoSeats) => Availability::Ready,
            Err(e) => Availability::Unsupported {
                reason: unsupported_reason(&e),
            },
        }
    }

    /// Whether a paste can be attempted.
    pub fn is_ready(&self) -> bool {
        matches!(self, Availability::Ready)
    }
}

/// The sentence a disabled Paste item shows, for an error that means the clipboard cannot be read.
///
/// The crate's own `Display` is the body of it in every case; the `MissingProtocol` arm adds what
/// the user can do about it, because "a required Wayland protocol is not supported" is a true
/// sentence that leaves a reader with nowhere to go.
fn unsupported_reason(e: &Error) -> String {
    match e {
        Error::MissingProtocol { name, version } => format!(
            "clipboard access requires {name} version {version}. The compositor does not provide it; paste is unavailable."
        ),
        other => format!("the clipboard could not be read: {other}"),
    }
}

/// What a fetch produced. The `Ok` side is the clipboard text and is never logged (module
/// docs); the `Err` side is a sentence for the chrome.
pub type Fetched = Result<String, String>;

/// The one-at-a-time admission for clipboard reader threads, and the tally of abandoned ones.
///
/// A type rather than two bare statics so the tests can each own one: the rule under test is about
/// what a *second* fetch does while a first is outstanding, and tests in one binary run in
/// parallel threads of one process.
#[derive(Debug)]
pub struct FetchGate {
    outstanding: AtomicBool,
    abandoned: AtomicUsize,
}

impl Default for FetchGate {
    fn default() -> FetchGate {
        FetchGate::new()
    }
}

impl FetchGate {
    pub const fn new() -> FetchGate {
        FetchGate {
            outstanding: AtomicBool::new(false),
            abandoned: AtomicUsize::new(0),
        }
    }

    /// Whether a reader thread is still running. A paste asked for while this is true is refused.
    pub fn is_outstanding(&self) -> bool {
        self.outstanding.load(Ordering::SeqCst)
    }

    /// How many fetches were dropped before they answered, since the process started.
    pub fn abandoned(&self) -> usize {
        self.abandoned.load(Ordering::SeqCst)
    }
}

/// The process's gate. One clipboard reader thread at a time, for the whole viewer.
static GATE: FetchGate = FetchGate::new();

/// An in-flight clipboard read, on its own thread.
#[derive(Debug)]
pub struct Fetch {
    rx: Receiver<Fetched>,
    /// Kept rather than detached, so a finished reader is reaped and an unfinished one is
    /// recognisable as abandoned (module docs). `None` only after [`Drop`] has taken it.
    join: Option<JoinHandle<()>>,
    gate: &'static FetchGate,
    /// The answer has been taken by [`Fetch::poll`], so the thread is on its last instructions.
    claimed: bool,
}

impl Fetch {
    /// Start an asynchronous UTF-8 clipboard read. Uses [`MimeType::Text`] selection
    /// within one connection; refuses when another fetch is still outstanding.
    pub fn spawn() -> Result<Fetch, String> {
        Fetch::start(&GATE, read_clipboard)
    }

    /// [`Fetch::spawn`] against a given gate and a given blocking read, for the tests.
    fn start(
        gate: &'static FetchGate,
        read: impl FnOnce() -> Fetched + Send + 'static,
    ) -> Result<Fetch, String> {
        if gate
            .outstanding
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(format!(
                "a clipboard read is still pending. It times out after {} s; try again when it finishes",
                READ_DEADLINE.as_secs()
            ));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        // Named, so it is recognisable in a backtrace or in `top` next to `nanokvm-render`.
        let spawned = std::thread::Builder::new()
            .name("nanokvm-clipboard".to_string())
            .spawn(move || {
                // The receiver is gone when the event loop gave up on its own deadline or the
                // window closed. Nothing to report to and nothing to clean up.
                let _ = tx.send(read());
                // Released last, and only here: while this is set no second reader may start, and
                // that is the window in which one would pile up behind a hung peer.
                gate.outstanding.store(false, Ordering::SeqCst);
            });
        match spawned {
            Ok(join) => Ok(Fetch {
                rx,
                join: Some(join),
                gate,
                claimed: false,
            }),
            Err(e) => {
                gate.outstanding.store(false, Ordering::SeqCst);
                Err(format!("could not start the clipboard reader: {e}"))
            }
        }
    }

    /// The answer, if it has arrived. Never blocks.
    ///
    /// A disconnected channel means the reader thread died without sending — a panic — and is
    /// reported as a failure rather than waited on for ever.
    pub fn poll(&mut self) -> Option<Fetched> {
        match self.rx.try_recv() {
            Ok(fetched) => {
                self.claimed = true;
                Some(fetched)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.claimed = true;
                Some(Err(
                    "the clipboard reader stopped without an answer".to_string()
                ))
            }
        }
    }
}

impl Drop for Fetch {
    fn drop(&mut self) {
        let Some(join) = self.join.take() else {
            return;
        };
        // A thread that has answered is on its last instructions — the send is the second-to-last
        // thing it does — so this join is bounded by them and not by the peer.
        if self.claimed || join.is_finished() {
            let _ = join.join();
            return;
        }
        let n = self.gate.abandoned.fetch_add(1, Ordering::SeqCst) + 1;
        log::warn!(
            "clipboard read canceled; waiting for its {} s deadline before another paste can start ({n} canceled reads)",
            READ_DEADLINE.as_secs()
        );
    }
}

/// The blocking half, run on [`Fetch`]'s thread.
fn read_clipboard() -> Fetched {
    let (pipe, mime) = match get_contents(ClipboardType::Regular, Seat::Unspecified, MimeType::Text)
    {
        Ok(got) => got,
        Err(Error::ClipboardEmpty) => return Err("the clipboard is empty".to_string()),
        Err(Error::NoMimeType) => {
            return Err("the clipboard holds no text; only text can be typed".to_string())
        }
        Err(e) => return Err(unsupported_reason(&e)),
    };
    let bytes = read_bounded(pipe, READ_DEADLINE, MAX_BYTES)?;
    // The MIME type is a type name, not content, so it may be reported. `MimeType::Text` only ever
    // selects a text type, and a text type that is not valid UTF-8 is a source's mistake rather
    // than something to approximate around.
    String::from_utf8(bytes).map_err(|_| {
        format!("the clipboard's {mime} is not valid UTF-8, so it cannot be typed as characters")
    })
}

/// Read `src` to EOF, giving up at `deadline` and refusing past `max` bytes.
///
/// The fd is polled before every read rather than read blindly, because `read_to_end` on a
/// pipe whose peer writes a chunk and then stops blocks for as long as the peer is alive — which,
/// for a clipboard, is another application's whole lifetime. On either refusal `src` is dropped by
/// returning, which closes this end of the pipe and is what lets the thread finish.
///
/// Generic over the source so the tests can hand it a pipe nobody ever writes to; the shipped
/// caller passes `wl-clipboard-rs`'s own `PipeReader`.
fn read_bounded<R: Read + AsFd>(
    mut src: R,
    deadline: Duration,
    max: usize,
) -> Result<Vec<u8>, String> {
    let give_up = Instant::now() + deadline;
    let mut bytes = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match poll_readable(src.as_fd(), give_up) {
            Ok(true) => {}
            Ok(false) => {
                return Err(format!(
                    "clipboard read timed out after {} s ({} bytes received)",
                    deadline.as_secs(),
                    bytes.len()
                ))
            }
            Err(e) => return Err(format!("reading the clipboard: {e}")),
        }
        match src.read(&mut buf) {
            Ok(0) => return Ok(bytes),
            Ok(n) => {
                if bytes.len() + n > max {
                    return Err(format!(
                        "clipboard exceeds the {} size limit ({} bytes received)",
                        human_bytes(max),
                        bytes.len() + n
                    ));
                }
                bytes.extend_from_slice(&buf[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("reading the clipboard: {e}")),
        }
    }
}

/// Wait for `fd` to become readable, until `deadline`. `Ok(false)` is the deadline.
///
/// The same shape as `serial::fake::poll_readable`, minus its hangup handling: a clipboard pipe
/// whose writer has closed is an end of file, not an error — that is how a short clipboard
/// arrives — so every ready revent goes on to the read, which reports 0 for EOF. A signal is not a
/// timeout either, so `EINTR` waits out the rest of the deadline here rather than being reported
/// as a peer that did not answer.
fn poll_readable(fd: BorrowedFd<'_>, deadline: Instant) -> std::io::Result<bool> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        let mut pfd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = left.as_millis().min(i32::MAX as u128) as libc::c_int;
        // SAFETY: `&mut pfd` is a live array of the one `pollfd` the count claims.
        let rc = unsafe { libc::poll(&mut pfd, 1, millis) };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if rc > 0 {
            return Ok(true);
        }
    }
}

/// A byte count as a person reads it, for the one refusal that names [`MAX_BYTES`].
fn human_bytes(n: usize) -> String {
    const MIB: usize = 1024 * 1024;
    if n >= MIB && n.is_multiple_of(MIB) {
        format!("{} MiB", n / MIB)
    } else {
        format!("{n} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// The reason a compositor without data-control gets is the crate's fact plus somewhere to go.
    /// Every disabled control must say why it is disabled, and "a required Wayland
    /// protocol is not supported" on its own does not.
    #[test]
    fn a_missing_protocol_reads_as_a_reason_and_a_remedy() {
        let reason = unsupported_reason(&Error::MissingProtocol {
            name: "zwlr_data_control_manager_v1",
            version: 1,
        });
        assert!(reason.contains("zwlr_data_control_manager_v1"), "{reason}");
        assert!(reason.contains("version 1"), "{reason}");
        assert!(reason.contains("clipboard access requires"), "{reason}");
        assert!(
            reason.contains("compositor does not provide it"),
            "{reason}"
        );
    }

    /// Anything else is the crate's own sentence, prefixed so the reader knows what failed.
    #[test]
    fn another_failure_carries_the_crates_own_words() {
        let reason = unsupported_reason(&Error::PrimarySelectionUnsupported);
        assert!(
            reason.starts_with("the clipboard could not be read: "),
            "{reason}"
        );
        assert!(reason.contains("primary selection"), "{reason}");
    }

    /// The bound exists to stop an unrelated process driving an unbounded allocation here; it is
    /// not "no length cap", which is about how much a user may paste.
    #[test]
    fn the_read_bound_is_far_past_any_paste() {
        // At 40 ms a key and two keys a character, 16 MiB of ASCII is over two weeks of typing —
        // so the cap that binds a real paste is ("the rate and the cancel are the
        // cap"), never this one.
        let seconds = (MAX_BYTES as f64) * 2.0 * (crate::script::REPORT_DELAY_MS as f64) / 1000.0;
        assert!(seconds > 14.0 * 24.0 * 3600.0, "{seconds} s");
        assert_eq!(
            human_bytes(MAX_BYTES),
            "16 MiB",
            "the refusal names it as this"
        );
    }

    /// Oversize refusal must identify the limit and received byte count.
    #[test]
    fn a_clipboard_past_the_bound_is_refused_by_the_numbers() {
        let (reader, writer) = std::io::pipe().expect("a pipe");
        (&writer).write_all(&[b'x'; 32]).expect("the peer writes");
        drop(writer);
        let e = read_bounded(reader, Duration::from_secs(5), 8).expect_err("past the bound");
        assert!(e.contains("8 bytes"), "the limit it met: {e}");
        assert!(e.contains("32 bytes received"), "what it saw: {e}");
        assert!(e.contains("size limit"), "a memory bound, said as one: {e}");
    }

    /// A clipboard that fits is read whole, and a peer that closes without writing is an empty
    /// read rather than a timeout.
    #[test]
    fn a_clipboard_that_fits_is_read_to_the_end() {
        let (reader, writer) = std::io::pipe().expect("a pipe");
        (&writer).write_all(b"hello").expect("the peer writes");
        drop(writer);
        assert_eq!(
            read_bounded(reader, Duration::from_secs(5), MAX_BYTES).expect("read"),
            b"hello"
        );

        let (reader, writer) = std::io::pipe().expect("a pipe");
        drop(writer);
        assert_eq!(
            read_bounded(reader, Duration::from_secs(5), MAX_BYTES).expect("read"),
            b""
        );
    }

    /// A peer that never writes costs a deadline and nothing more. The writer end is held open
    /// for the whole test, which is what a hung clipboard owner looks like: `read_to_end` would
    /// block on it for that application's lifetime.
    #[test]
    fn a_peer_that_never_writes_times_out_rather_than_blocking() {
        let (reader, _writer) = std::io::pipe().expect("a pipe");
        let started = Instant::now();
        let e = read_bounded(reader, Duration::from_millis(120), MAX_BYTES).expect_err("timeout");
        let took = started.elapsed();
        assert!(e.contains("read timed out"), "{e}");
        assert!(e.contains("0 bytes received"), "{e}");
        assert!(
            took >= Duration::from_millis(100),
            "gave up early: {took:?}"
        );
        assert!(took < Duration::from_secs(5), "did not give up: {took:?}");
    }

    /// One fetch at a time, and the thread ends at its deadline. Without the gate every
    /// trigger against a hung clipboard owner would leave another thread and another pipe behind
    /// for the life of the process; without the deadline they would never exit at all.
    #[test]
    fn a_second_fetch_is_refused_while_one_is_outstanding_and_the_first_ends_at_its_deadline() {
        static GATE: FetchGate = FetchGate::new();
        let (reader, writer) = std::io::pipe().expect("a pipe");
        let mut first = Fetch::start(&GATE, move || {
            read_bounded(reader, Duration::from_millis(150), MAX_BYTES)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
        })
        .expect("the first fetch starts");

        let refusal = Fetch::start(&GATE, || Ok(String::new())).expect_err("one at a time");
        assert!(refusal.contains("read is still pending"), "{refusal}");
        assert!(refusal.contains("try again"), "somewhere to go: {refusal}");

        let answer = spin_for(Duration::from_secs(5), || first.poll()).expect("it answers");
        let reason = answer.expect_err("the peer never wrote");
        assert!(reason.contains("read timed out"), "{reason}");

        // The thread is on its way out, and the gate is free again once it is gone — which is what
        // "no thread is leaked" means here.
        assert!(
            spin_for(Duration::from_secs(5), || (!GATE.is_outstanding())
                .then_some(()))
            .is_some(),
            "the reader thread did not exit"
        );
        assert_eq!(GATE.abandoned(), 0, "it answered, so it was not abandoned");
        drop(first);
        drop(writer);
        Fetch::start(&GATE, || Ok(String::new())).expect("the gate is free again");
    }

    /// A fetch dropped before it answered — the event loop gave up on its own deadline — is
    /// counted once, so a log says how many times this has happened rather than nothing at all.
    #[test]
    fn an_abandoned_fetch_is_counted_once() {
        static GATE: FetchGate = FetchGate::new();
        let (reader, _writer) = std::io::pipe().expect("a pipe");
        let fetch = Fetch::start(&GATE, move || {
            read_bounded(reader, Duration::from_millis(200), MAX_BYTES)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
        })
        .expect("it starts");
        assert_eq!(GATE.abandoned(), 0);
        drop(fetch);
        assert_eq!(GATE.abandoned(), 1, "dropped before it answered");
        // And it still exits on its own, releasing the gate.
        assert!(
            spin_for(Duration::from_secs(5), || (!GATE.is_outstanding())
                .then_some(()))
            .is_some(),
            "the abandoned reader thread did not exit"
        );
    }

    /// Poll `f` until it answers or `limit` passes. The reader threads have no rendezvous of their
    /// own — that is the point of them — so the tests above wait on a bounded spin rather than
    /// hanging when something is wrong.
    fn spin_for<T>(limit: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(got) = f() {
                return Some(got);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// `Ready` is the only state a paste may be attempted from.
    #[test]
    fn only_ready_is_ready() {
        assert!(Availability::Ready.is_ready());
        assert!(!Availability::Unsupported {
            reason: "no".to_string()
        }
        .is_ready());
    }
}
