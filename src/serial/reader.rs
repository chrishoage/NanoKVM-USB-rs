//! Incremental serial reader and reply matcher.
//!
//! Unsolicited device-info frames update lock state. Timed-out replies must not satisfy a
//! later transaction of the same command, so the matcher tracks a late-reply window.
//! Dropping an unsolicited receiver does not stop transport reads.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::link::Reply;
use crate::proto::frame::{Event, Parser};

/// Serial framing, matching, and transport counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SerialStats {
    /// Well-formed frames the parser produced, matched and unsolicited together.
    pub frames_rx: u64,
    /// Frames diverted to the unsolicited channel because no in-flight request claimed them.
    /// Includes `late_replies` and `unsolicited_dropped`.
    pub unsolicited: u64,
    /// The subset of `unsolicited` that answered a request which had already timed out.
    pub late_replies: u64,
    /// Unsolicited frames whose receiver was dropped. Reading continues so transaction
    /// replies remain usable even when nobody consumes notifications.
    pub unsolicited_dropped: u64,
    /// Resynchronisation windows entered: `transact` calls held back after a preceding timeout
    /// until the link went quiet. One per
    /// timeout that was followed by another `transact`.
    ///
    /// This is the *receive*-side window and has nothing to do with
    /// [`SerialStats::resyncs_sent`], which counts writes aimed at the chip's own parser.
    pub resyncs: u64,
    /// Zero-byte preambles written by [`crate::link::Link::resync`] to finish a frame the chip's
    /// receive parser may still be waiting on. One per call; the bytes themselves are also
    /// counted in [`SerialStats::bytes_tx`].
    pub resyncs_sent: u64,
    /// Candidate frames whose checksum did not verify. On this device a burst of these
    /// means the link was overloaded.
    pub bad_checksum: u64,
    /// Incomplete frames expired after the silence interval.
    pub truncated: u64,
    /// Bytes discarded while hunting for a frame header. Bytes already reported inside a
    /// bad-checksum candidate are not counted twice — see [`Parser`].
    pub garbage_bytes: u64,
    /// Frames written by `transact`, successful or not.
    pub transacts: u64,
    /// `transact` calls that got no matching reply in time.
    pub timeouts: u64,
    /// Error frames (`cmd | 0xC0`) surfaced as `LinkError::Device`.
    pub device_errors: u64,
    pub bytes_tx: u64,
    pub bytes_rx: u64,
}

/// The live counters. Separate from [`SerialStats`] so a reader can bump one without a lock.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub frames_rx: AtomicU64,
    pub unsolicited: AtomicU64,
    pub late_replies: AtomicU64,
    pub unsolicited_dropped: AtomicU64,
    pub resyncs: AtomicU64,
    pub resyncs_sent: AtomicU64,
    pub bad_checksum: AtomicU64,
    pub truncated: AtomicU64,
    pub garbage_bytes: AtomicU64,
    pub transacts: AtomicU64,
    pub timeouts: AtomicU64,
    pub device_errors: AtomicU64,
    pub bytes_tx: AtomicU64,
    pub bytes_rx: AtomicU64,
}

/// Increment a counter. Relaxed is right: counters are diagnostics, never a synchronisation
/// mechanism, and no decision anywhere in this module reads one.
fn bump(counter: &AtomicU64, by: u64) {
    counter.fetch_add(by, Ordering::Relaxed);
}

impl Counters {
    pub(crate) fn snapshot(&self) -> SerialStats {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        SerialStats {
            frames_rx: get(&self.frames_rx),
            unsolicited: get(&self.unsolicited),
            late_replies: get(&self.late_replies),
            unsolicited_dropped: get(&self.unsolicited_dropped),
            resyncs: get(&self.resyncs),
            resyncs_sent: get(&self.resyncs_sent),
            bad_checksum: get(&self.bad_checksum),
            truncated: get(&self.truncated),
            garbage_bytes: get(&self.garbage_bytes),
            transacts: get(&self.transacts),
            timeouts: get(&self.timeouts),
            device_errors: get(&self.device_errors),
            bytes_tx: get(&self.bytes_tx),
            bytes_rx: get(&self.bytes_rx),
        }
    }
}

/// The matcher's mutable state. One `transact` runs at a time (`&mut self` on the trait method),
/// so there is at most one in-flight request and at most one waiter.
#[derive(Debug, Default)]
pub(crate) struct Inner {
    /// Command byte of the request currently awaiting a reply, if any.
    pub inflight: Option<u8>,
    /// The reply the reader matched, waiting to be collected by the waiter.
    pub reply: Option<Reply>,
    /// Why the link is unusable, once it is. Set once and never cleared: reconnect is and
    /// builds a *new* link rather than reviving this one.
    pub down: Option<String>,
    /// Command byte of the most recent `transact` that timed out and may still be answered. A
    /// frame answering it is that request's late reply rather than this request's, and
    /// [`route`] checks this slot before [`Inner::inflight`] for that reason.
    ///
    /// One slot is enough because `transact` takes `&mut self`, so timeouts cannot overlap; a
    /// second timeout simply replaces the first, and a very late reply to the first is then
    /// counted as an ordinary device push.
    ///
    /// It is cleared by the first frame that answers it, inside the resynchronisation window or
    /// after it: the device answers each request once, so one diversion settles
    /// the account. Inside the window that arrival also restarts the window; after it, the
    /// diversion is what keeps the stale reply out of the request now in flight.
    pub timed_out: Option<u8>,
    /// When the resynchronisation window last restarted: the moment of the timeout, or the
    /// arrival of a late reply during the window. `None` once `transact` has waited it out.
    ///
    /// The reader only ever pushes this forward; `transact` owns opening and closing it. That
    /// split is deliberate — the reader must never decide how long the writer waits.
    pub quiet_since: Option<Instant>,
    /// Set by [`route`] when the frame it diverted as a late reply would *also* have answered the
    /// request currently in flight.
    ///
    /// This is what stops a single permanently lost reply from poisoning a command byte for ever.
    /// Without it: request *n*'s reply is lost, so request *n+1*'s reply is diverted as *n*'s late
    /// reply, so *n+1* times out and arms the slot again, so *n+2*'s reply is diverted… For every
    /// later request of that command. With it, the transact that had its own answer taken away
    /// declines to arm the slot: the device has now answered as many times as it was asked, the
    /// accounts balance, and the next request matches normally. The cost of a lost reply is
    /// therefore one extra `Timeout`, which the writer treats as degraded and survives,
    /// rather than an endless run of them.
    pub answered_inflight_late: bool,
    /// Set as the reader thread leaves, from a `Drop` guard so that a panic sets it too.
    ///
    /// Two callers depend on it. `SerialLink::shutdown` bounds its join on it. And
    /// `<SerialLink as Link>::is_down` treats it as link loss in its own right: the reader thread
    /// is the only thing that can ever deliver a reply, so once it has gone, nothing on this link
    /// will be answered again — whatever ended it, and whether or not it recorded a `down` reason
    /// on the way out.
    pub reader_finished: bool,
}

/// State shared between `SerialLink` and its reader thread.
#[derive(Debug)]
pub(crate) struct Shared {
    inner: Mutex<Inner>,
    /// Signalled on: a matched reply, the link going down, and the reader thread finishing.
    pub signal: Condvar,
    pub stop: AtomicBool,
    pub counters: Counters,
}

impl Shared {
    pub(crate) fn new() -> Shared {
        Shared {
            inner: Mutex::new(Inner::default()),
            signal: Condvar::new(),
            stop: AtomicBool::new(false),
            counters: Counters::default(),
        }
    }

    /// Take the matcher lock.
    ///
    /// Lock-poisoning policy for the whole `serial` module. A poisoned mutex means some thread
    /// panicked while holding it. Every field of [`Inner`] is plain data written in one statement
    /// under the lock; there is no multi-step invariant a panic can leave half-applied, so there
    /// is nothing to protect the next caller from. Propagating the panic instead would take down
    /// the input writer and leave the target holding whatever keys it holds, which is
    /// strictly worse. So the guard is recovered with `into_inner`, and this is the only place in
    /// non-test code in this module that decides that.
    pub(crate) fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Mark the link unusable and wake the waiter, if any. The first reason recorded wins: it is
    /// the one that describes the actual failure, later ones are consequences of it.
    pub(crate) fn mark_down(&self, reason: String) {
        let mut inner = self.lock();
        if inner.down.is_none() {
            log::warn!("serial link down: {reason}");
            inner.down = Some(reason);
        }
        drop(inner);
        self.signal.notify_all();
    }

    pub(crate) fn down_reason(&self) -> Option<String> {
        self.lock().down.clone()
    }
}

/// Route a valid frame, checking timed-out requests before the current waiter.
///
/// Replies have no sequence ID. This ordering may sacrifice a fresh reply as late,
/// but prevents an old acknowledgement from falsely completing a new transaction.
/// Unmatched frames go to the unsolicited channel.
fn route(shared: &Shared, unsolicited: &Sender<Reply>, reply: Reply) {
    let mut inner = shared.lock();

    let late = inner.timed_out.is_some_and(|cmd| reply.answers(cmd));
    if late {
        // One frame retires the slot: the device answers each request once, so
        // once the stale reply is accounted for the next frame of that command really is the next
        // request's.
        inner.timed_out = None;
        if let Some(since) = inner.quiet_since.as_mut() {
            // The window is still open, and the link has just proved it is not quiet. Restart it
            // from this arrival so the next request goes out into silence rather than into the
            // tail of the device's backlog.
            *since = Instant::now();
        }
        if inner.inflight.is_some_and(|cmd| reply.answers(cmd)) {
            inner.answered_inflight_late = true;
        }
    } else if let Some(cmd) = inner.inflight {
        if reply.answers(cmd) {
            inner.inflight = None;
            inner.reply = Some(reply);
            drop(inner);
            shared.signal.notify_all();
            return;
        }
    }
    drop(inner);

    if late {
        bump(&shared.counters.late_replies, 1);
        log::debug!(
            "late reply {:#04x} arrived after its transact timed out; treating it as unsolicited",
            reply.cmd
        );
    } else {
        log::debug!(
            "unsolicited frame {:#04x} ({} bytes)",
            reply.cmd,
            reply.data.len()
        );
    }
    bump(&shared.counters.unsolicited, 1);
    if unsolicited.send(reply).is_err() {
        // Nobody is listening. That is not a reason to stop listening: the reader is also the only
        // thing that can match a reply or notice a hang-up, so stopping here would leave every
        // later `transact` to burn its whole timeout against a link still claiming to be healthy.
        // Count the frame and carry on.
        bump(&shared.counters.unsolicited_dropped, 1);
        log::debug!("no unsolicited receiver; frame counted and dropped, reader continues");
    }
}

/// Feed one parser event to the matcher and the counters.
fn handle(shared: &Shared, unsolicited: &Sender<Reply>, event: Event) {
    let c = &shared.counters;
    match event {
        Event::Frame(frame) => {
            bump(&c.frames_rx, 1);
            route(shared, unsolicited, frame.into_reply());
        }
        Event::BadChecksum { raw } => {
            bump(&c.bad_checksum, 1);
            log::debug!(
                "checksum mismatch over {} bytes, resynchronising",
                raw.len()
            );
        }
        Event::Truncated { raw } => {
            bump(&c.truncated, 1);
            log::debug!(
                "abandoning {} buffered bytes after a silence; the rest of this frame is not coming",
                raw.len()
            );
        }
        Event::Garbage { bytes } => {
            bump(&c.garbage_bytes, bytes as u64);
            log::debug!("discarded {bytes} bytes while hunting for a frame header");
        }
    }
}

/// The reader thread body. Returns when stopped, or when the port fails with anything other than a
/// timeout.
///
/// It does not return because the unsolicited receiver went away: see [`route`]. The reader is
/// the only thing that can match a reply, retire a partial frame or notice a hang-up, so its life
/// is tied to the port's, never to who happens to be listening.
///
/// `port` is taken as a plain [`Read`](std::io::Read) — reading is all this thread ever does with
/// it — so that the EOF arm below can be exercised without a device or a pty, neither of which can
/// produce it. `Box<dyn SerialPort>` satisfies the bound unchanged.
pub(crate) fn run(
    shared: Arc<Shared>,
    mut port: impl std::io::Read,
    unsolicited: Sender<Reply>,
    silence_before_expire: Duration,
) {
    // `reader_finished` is set from a `Drop`, not from the bottom of this function, so that it is
    // true on *every* exit — a panic in the parser or the matcher included. Two things read it and
    // both need that: `SerialLink::shutdown` bounds its join on it, and `Link::is_down` treats a
    // reader that has gone as link loss. A panicking reader that left the flag clear would leave
    // the writer reporting a healthy link that can never answer again.
    struct Finished(Arc<Shared>);
    impl Drop for Finished {
        fn drop(&mut self) {
            let mut inner = self.0.lock();
            inner.reader_finished = true;
            drop(inner);
            self.0.signal.notify_all();
            log::debug!("serial reader thread finished");
        }
    }
    let _finished = Finished(Arc::clone(&shared));

    let mut parser = Parser::new();
    let mut buf = [0u8; 512];
    // When the stream last carried a byte. Only meaningful while the parser holds a partial frame.
    let mut last_byte_at = Instant::now();

    while !shared.stop.load(Ordering::SeqCst) {
        match port.read(&mut buf) {
            Ok(0) => {
                // EOF, not silence. `read` only returns here after the poll said the fd was
                // readable, and a readable fd with nothing to read is a hang-up: that is exactly
                // what the tty layer does to a vhangup'd CDC-ACM node (`hung_up_tty_read`), which
                // is what an unplug of this dongle produces. Nothing more is ever coming on this
                // fd, so this is link loss — and treating it as "no data this round" would spin
                // this thread on a core with no sleep in the loop, while every later `transact`
                // burned its whole timeout against a link still claiming to be healthy.
                shared.mark_down("read returned EOF: the port hung up".to_string());
                break;
            }
            Ok(n) => {
                bump(&shared.counters.bytes_rx, n as u64);
                last_byte_at = Instant::now();
                parser.push(&buf[..n]);
                while let Some(event) = parser.next_event() {
                    handle(&shared, &unsolicited, event);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                // Expire incomplete bytes only after sustained silence, allowing split valid frames
                // to finish while retiring the malformed five-byte reply.
                if parser.pending_len() > 0 && last_byte_at.elapsed() >= silence_before_expire {
                    if let Some(event) = parser.expire_partial() {
                        handle(&shared, &unsolicited, event);
                    }
                }
            }
            Err(e) => {
                // ENODEV, EIO, broken pipe: what an unplug or a hang-up produces. The link is
                // gone; wake whoever is waiting rather than letting them run out their timeout.
                shared.mark_down(format!("read failed: {e}"));
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A read half that hands over `first` and then reports EOF for ever — the shape a vhangup'd
    /// tty presents (`POLLHUP`, then `read` returning 0). The pty fake cannot produce it: on a pty
    /// `serialport` sees the hang-up in `revents` and reports `BrokenPipe` before it ever reaches
    /// the `read`, which is the *other* arm. So the EOF arm gets the only double that can reach
    /// it, and nothing else.
    struct ThenEof {
        first: Vec<u8>,
    }

    impl std::io::Read for ThenEof {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.first.is_empty() {
                return Ok(0);
            }
            let n = self.first.len().min(buf.len());
            buf[..n].copy_from_slice(&self.first[..n]);
            self.first.drain(..n);
            Ok(n)
        }
    }

    /// EOF after a readable poll is a hang-up: the link goes down with a reason, the thread ends,
    /// and it does not sit in the loop reading zero bytes for ever.
    #[test]
    fn a_read_that_returns_eof_takes_the_link_down_and_ends_the_thread() {
        let shared = Arc::new(Shared::new());
        let (tx, _rx) = std::sync::mpsc::channel();
        let port = ThenEof {
            // One frame first, so the thread is demonstrably alive and matching before the EOF.
            first: crate::proto::encode(crate::proto::cmd::GET_INFO | 0x80, &[0u8; 8])
                .expect("a well-formed reply frame"),
        };

        run(Arc::clone(&shared), port, tx, Duration::from_millis(10));

        assert_eq!(shared.counters.frames_rx.load(Ordering::Relaxed), 1);
        let reason = shared.down_reason().expect("EOF must mark the link down");
        assert!(reason.contains("EOF"), "{reason}");
        assert!(
            shared.lock().reader_finished,
            "the thread must leave its loop rather than spin on a dead fd"
        );
    }

    /// A read half that panics — standing in for any way the reader thread can die without
    /// recording a verdict: a panic in the parser, in `route`, or in a `handle` arm.
    struct Panics;

    impl std::io::Read for Panics {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            panic!("the reader thread died without a verdict");
        }
    }

    /// `reader_finished` means "the thread has gone", not "the thread returned".
    ///
    /// It is set from a `Drop` guard for this case: a reader that unwinds records no `down`
    /// reason, so if the flag were set at the bottom of `run` a panicking reader would leave both
    /// signals clear — `SerialLink::shutdown` would wait out its whole join bound, and
    /// `<SerialLink as Link>::is_down` would report a link that can never answer again as healthy.
    #[test]
    fn a_reader_that_panics_still_reports_itself_finished() {
        let shared = Arc::new(Shared::new());
        let (tx, _rx) = std::sync::mpsc::channel();

        // The panic is the point of the test; its backtrace on stderr is not.
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run(Arc::clone(&shared), Panics, tx, Duration::from_millis(10));
        }));
        std::panic::set_hook(hook);

        assert!(result.is_err(), "the double must have panicked");
        assert!(
            shared.lock().reader_finished,
            "a reader that unwound out of its loop is still a reader that has gone"
        );
        assert_eq!(
            shared.down_reason(),
            None,
            "and it recorded no reason, which is exactly why the flag has to carry the verdict"
        );
    }
}
