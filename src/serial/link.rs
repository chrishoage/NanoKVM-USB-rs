//! [`SerialLink`]: the real transport, and the crate's only implementation of
//! [`Link`](crate::link::Link) against hardware (§4, §12 Stage 1).

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::link::{Link, LinkError, Reply};
use crate::proto::frame::DeviceInfo;
use crate::proto::{cmd, encode};
use crate::serial::port::{self, OpenOptions};
use crate::serial::reader::{self, SerialStats, Shared};

/// Command byte of the device's lock-state push, which is also `GET_INFO`'s reply (Appendix).
const LOCK_STATE: u8 = cmd::GET_INFO | 0x80;

/// How long [`SerialLink::drop`] waits for the reader thread before giving up on it.
const JOIN_BOUND: Duration = Duration::from_secs(1);

/// How many zero bytes [`SerialLink::resync`] writes to finish a frame the chip may still be
/// waiting on (§5.1, §11 "Resynchronisation after a torn write").
///
/// **Why sixteen, and why zeros.** A frame is `57 AB ADDR CMD LEN DATA[LEN] SUM` with `LEN <= 8`
/// on every command this client sends, so the longest remainder any tear can leave outstanding is
/// the ten bytes after `CMD` — `LEN`, eight payload bytes and the checksum — and every earlier
/// tear owes fewer. Sixteen covers that with six bytes to spare and still costs only 2.8 ms of
/// wire time at 57600 baud.
///
/// Zeros are chosen because `0x00` is not `0x57`, so every byte the chip does *not* need as a
/// remainder is discarded by a parser hunting for a header rather than starting a new command.
/// And a frame completed with zeros is almost always rejected: the checksum of the stale header
/// plus zeros matches the zero byte that lands in `SUM` with probability 1/256, so 255 times out
/// of 256 the chip answers `cmd | 0xC0` with `0xE4` and moves on. In the remaining case it applies
/// an all-zero keyboard report (every key released) or an all-zero mouse report (no motion, no
/// button) — both of which the release-all that follows in §2.7 step 2 makes moot. That is the
/// whole safety argument, and H-A1/H-A2 in `tests/serial_reconnect_hardware.rs` check it against
/// the real chip rather than trusting it.
pub const RESYNC_PREAMBLE_LEN: usize = 16;

/// A framed link to the CH9329 over a serial port.
///
/// One [`SerialLink`] owns one port, one reader thread and one in-flight request. `transact` takes
/// `&mut self`, so "one in flight" is a type-level fact rather than a convention, which is what
/// lets the matcher hold a single slot instead of a table.
///
/// # What it guarantees
///
/// - **A frame is written whole or the link is abandoned.** The chip has no inter-byte timeout, so
///   a torn write is consumed as the head of the *next* command (§5.1, A15). A write that fails is
///   therefore not a retryable error: the link goes down and stays down.
/// - **Replies are matched by command byte** (A12, Appendix). Anything else the device says goes
///   to [`SerialLink::unsolicited`], never to the waiting caller.
/// - **Error frames are surfaced**, as [`LinkError::Device`]. Upstream's short checksum silently
///   discards every one of them, leaving it unable to tell a reported error from silence (§3.1,
///   A14); repeating that is the specific bug this type exists to avoid.
///
/// # What it does not do
///
/// - It does not reconnect. That is Stage 2 (§2.7, §12) and needs the input writer's cooperation
///   — queue discarded, release-all re-sent — so it cannot live down here alone. The port-opening
///   code is in a separate module so it can be re-run without disturbing the matcher.
/// - It does not send release-all on drop. Release-all is the writer's cancellation sequence, with
///   an epoch and an outcome attached (§2.6, §2.6.1); a transport that quietly injected reports of
///   its own would break that accounting.
/// - **An acknowledgement is not evidence of effect** (§3.4, A17). `Ok(reply)` means the chip
///   parsed the frame. The device ACKs malformed mouse reports and does nothing.
pub struct SerialLink {
    /// The write half. The reader thread owns a `try_clone` of the same port, with its own,
    /// much shorter timeout — see [`SerialLink::open_with`].
    port: Box<dyn serialport::SerialPort>,
    shared: Arc<Shared>,
    unsolicited: Receiver<Reply>,
    reader: Option<JoinHandle<()>>,
    /// How long the link must be quiet after a timeout before the next request goes out
    /// ([`OpenOptions::quiet_after_timeout`]). Held here because `transact` does the waiting.
    quiet_after_timeout: Duration,
    /// Kept for diagnostics and for Stage 2's reopen.
    path: std::path::PathBuf,
}

impl SerialLink {
    /// Open `path` with the settings confirmed on hardware and start the reader thread.
    ///
    /// No handshake is performed. §12 (Stage 1) is explicit that nothing beyond `GET_INFO` is
    /// required to reach a working state, and `GET_INFO` is the caller's to send when it wants the
    /// answer — see [`SerialLink::get_info`].
    pub fn open(path: &Path) -> Result<SerialLink, LinkError> {
        SerialLink::open_with(path, OpenOptions::default())
    }

    /// [`SerialLink::open`] with the port settings and reader timings spelled out (§5.1).
    ///
    /// The two halves of the port get **different timeouts**, and the order below is what makes
    /// that possible: `serialport` keeps one timeout per handle and uses it for reads and writes
    /// alike, and `try_clone` copies whatever is set at the moment of the clone. So the read half
    /// is cloned off while the handle still carries [`OpenOptions::read_timeout`] — the reader's
    /// 25 ms heartbeat — and the handle this type keeps for writing is then moved to
    /// [`OpenOptions::write_timeout`]. Sharing the reader's heartbeat with the writer would make
    /// a 25 ms stall in `write_all` tear a frame, and a torn frame is eaten as the head of the
    /// next command (§5.1, A15).
    pub fn open_with(path: &Path, opts: OpenOptions) -> Result<SerialLink, LinkError> {
        let mut port = port::open(path, &opts)?;
        let read_half = port
            .try_clone()
            .map_err(|e| LinkError::Down(format!("clone {}: {e}", path.display())))?;
        port.set_timeout(opts.write_timeout).map_err(|e| {
            LinkError::Down(format!(
                "set write timeout {:?} on {}: {e}",
                opts.write_timeout,
                path.display()
            ))
        })?;

        let shared = Arc::new(Shared::new());
        let (tx, rx) = mpsc::channel();
        let thread_shared = Arc::clone(&shared);
        let silence = opts.silence_before_expire;
        let reader = std::thread::Builder::new()
            .name("nanokvm-serial-rx".to_string())
            .spawn(move || reader::run(thread_shared, read_half, tx, silence))
            .map_err(|e| LinkError::Down(format!("spawn reader thread: {e}")))?;

        log::debug!(
            "opened {} at {} baud, read timeout {:?}, write timeout {:?}",
            path.display(),
            opts.baud,
            opts.read_timeout,
            opts.write_timeout
        );
        Ok(SerialLink {
            port,
            shared,
            unsolicited: rx,
            reader: Some(reader),
            quiet_after_timeout: opts.quiet_after_timeout,
            path: path.to_path_buf(),
        })
    }

    /// The path this link was opened on.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Why the link is unusable, or `None` while it is healthy.
    ///
    /// A link goes down on a failed write (§5.1, A15) or a read error that is not a timeout —
    /// `ENODEV`, `EIO`, a hang-up. It never comes back up: reconnect builds a new link (§2.7).
    pub fn is_down(&self) -> Option<String> {
        self.shared.down_reason()
    }

    /// Counters for everything the link has seen, including everything it discarded (§2.8, §5.1).
    pub fn stats(&self) -> SerialStats {
        self.shared.counters.snapshot()
    }

    /// Frames the device sent that no request had asked for.
    ///
    /// In practice this is the `0x81` lock-state push, which arrives about 15 ms after a lock-key
    /// change and is proof the *target* processed a keystroke, since the lock bits originate in
    /// the target's own HID output report (A12, Appendix). It is the one end-to-end check that
    /// needs no video. Decode with [`SerialLink::lock_state_from`].
    ///
    /// A reply that arrived after its `transact` gave up also lands here rather than being
    /// discarded, and is counted in [`SerialStats::late_replies`]. The channel is unbounded, which
    /// is safe because the device is silent unless something happens — forty seconds of idle
    /// listening produced zero bytes (A12) — but a caller that never drains it will accumulate
    /// one `Reply` per lock-key change for the life of the link. Dropping the receiver is the
    /// other way to stay bounded, and it costs nothing but the frames themselves: see
    /// [`SerialLink::take_unsolicited`].
    pub fn unsolicited(&self) -> &Receiver<Reply> {
        &self.unsolicited
    }

    /// Move the unsolicited-frame receiver out of the link so another thread can drain it while
    /// the link itself lives inside the input writer (§4.1). After this call
    /// [`SerialLink::unsolicited`] returns a receiver that is permanently disconnected.
    ///
    /// **The receiver must be drained or dropped, and both are safe.** The channel is unbounded,
    /// so an owner that holds it and never reads it is the one case that grows without limit —
    /// one `Reply` per lock-key change (A12). An owner that drops it instead loses those frames
    /// and nothing else: the reader counts each one in [`SerialStats::unsolicited_dropped`] and
    /// keeps reading, so replies still match, hang-ups are still noticed and the link stays up.
    /// Only the A12 evidence — proof the *target* processed a lock key — is given up.
    pub fn take_unsolicited(&mut self) -> Receiver<Reply> {
        let (_disconnected_tx, dummy) = std::sync::mpsc::channel();
        std::mem::replace(&mut self.unsolicited, dummy)
    }

    /// Decode a `0x81` frame — solicited or pushed — as [`DeviceInfo`] (Appendix).
    ///
    /// `None` if the reply is not a lock-state frame, or is one whose payload is too short to
    /// carry the documented fields. Device-supplied lengths are never trusted (§3.2).
    pub fn lock_state_from(reply: &Reply) -> Option<DeviceInfo> {
        if reply.cmd != LOCK_STATE {
            return None;
        }
        DeviceInfo::parse(&reply.data).ok()
    }

    /// `GET_INFO`: version, target-connected flag and lock bits (Appendix).
    ///
    /// A reply too short to carry those fields is reported as [`LinkError::Io`] with
    /// [`std::io::ErrorKind::InvalidData`]. That is a deliberate choice where the plan is silent:
    /// it is not [`LinkError::Device`] (the device reported no error) and it is not
    /// [`LinkError::Timeout`] (a reply did arrive), and `Io` already means "the transport produced
    /// something unusable". Note it does *not* mark the link down — a malformed payload is a
    /// device-behaviour problem, not a dead cable, and the link recovers immediately (§3.1).
    pub fn get_info(&mut self, timeout: Duration) -> Result<DeviceInfo, LinkError> {
        let reply = self.transact(cmd::GET_INFO, &[], timeout)?;
        DeviceInfo::parse(&reply.data).map_err(|e| {
            LinkError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("GET_INFO reply: {e}"),
            ))
        })
    }

    /// Write bytes straight at the port, outside the framing layer.
    ///
    /// The one primitive `transact` and [`SerialLink::resync`] share, so that "a failed write puts
    /// the link down and stays down" (§5.1, A15) is stated once. `what` names the write for the
    /// diagnostic the failure records.
    fn write_raw(&mut self, bytes: &[u8], what: &str) -> Result<(), LinkError> {
        if let Some(reason) = self.shared.lock().down.clone() {
            return Err(LinkError::Down(reason));
        }
        if let Err(e) = self.port.write_all(bytes).and_then(|()| self.port.flush()) {
            self.shared.mark_down(format!(
                "write of the {}-byte {what} failed: {e}",
                bytes.len()
            ));
            return Err(LinkError::Io(e));
        }
        self.shared
            .counters
            .bytes_tx
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    /// Write arbitrary bytes at the port, for hardware tests that must reproduce a torn write.
    ///
    /// Not part of the supported surface: nothing in the client may write anything that is not a
    /// whole frame (§5.1, A15), and this exists only so `tests/serial_reconnect_hardware.rs` can
    /// manufacture the damage [`SerialLink::resync`] claims to repair. Tearing a frame on purpose
    /// through the real transport is the only honest way to test the repair.
    #[doc(hidden)]
    pub fn write_raw_for_test(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
        self.write_raw(bytes, "deliberate raw test write")
    }

    /// Hold the writer back until the link has gone quiet after a [`LinkError::Timeout`] — the
    /// resynchronisation window (§3.1, [`OpenOptions::quiet_after_timeout`]).
    ///
    /// The problem it solves is that the device's reply to a request that already gave up is the
    /// same bytes as the reply to the next request of that command, and the input writer sends the
    /// same command back to back all day (§2.6, A11). Registering the next request while a stale
    /// reply is still in flight makes the two indistinguishable *and* simultaneous, which is the
    /// one situation the matcher cannot reason its way out of. So the request waits: 50 ms of
    /// silence is about three mouse round trips (17.0 ms, A11), by which point a reply that has
    /// not arrived is not going to.
    ///
    /// **The wait is bounded at twice `quiet_after_timeout`, measured from entry** — one window
    /// for the silence, plus one more for a single late reply to land and restart it. That is also
    /// the natural maximum, because the slot the restart depends on is retired by the very frame
    /// that restarts the window; the bound is enforced rather than assumed so that a device which
    /// will not stop talking delays the writer by 100 ms with the defaults instead of wedging it,
    /// which matters because this runs on the thread that delivers keystrokes and release-alls
    /// (§2.6). Past the bound the request goes out anyway, and the reader's timed-out slot is what
    /// protects it: still armed, it diverts the first frame answering the stale command rather
    /// than letting it pass as this request's reply.
    ///
    /// A link that went down while waiting is reported immediately rather than after the window.
    fn settle_after_timeout(&mut self) -> Result<(), LinkError> {
        let mut inner = self.shared.lock();
        if inner.quiet_since.is_none() {
            return Ok(());
        }
        self.shared.counters.resyncs.fetch_add(1, Ordering::Relaxed);
        let hard_deadline = Instant::now() + 2 * self.quiet_after_timeout;
        loop {
            if let Some(reason) = inner.down.clone() {
                inner.quiet_since = None;
                return Err(LinkError::Down(reason));
            }
            let Some(quiet_since) = inner.quiet_since else {
                break;
            };
            let now = Instant::now();
            let quiet_deadline = quiet_since + self.quiet_after_timeout;
            let wake_at = quiet_deadline.min(hard_deadline);
            let Some(remaining) = wake_at.checked_duration_since(now) else {
                if hard_deadline <= now {
                    log::debug!(
                        "resynchronisation window hit its {:?} bound with the link still talking",
                        2 * self.quiet_after_timeout
                    );
                }
                break;
            };
            let (guard, _) = self
                .shared
                .signal
                .wait_timeout(inner, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner = guard;
        }
        inner.quiet_since = None;
        Ok(())
    }

    /// Stop the reader thread and wait a bounded time for it to notice.
    ///
    /// The thread checks the stop flag once per read timeout, so it returns within roughly
    /// [`OpenOptions::read_timeout`]. If it somehow does not, the handle is dropped rather than
    /// blocking the caller for ever — a wedged reader must not stop the viewer from exiting.
    fn shutdown(&mut self) {
        let Some(handle) = self.reader.take() else {
            return;
        };
        self.shared.stop.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + JOIN_BOUND;
        let mut inner = self.shared.lock();
        while !inner.reader_finished {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            let (guard, _) = self
                .shared
                .signal
                .wait_timeout(inner, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner = guard;
        }
        let finished = inner.reader_finished;
        drop(inner);
        if finished {
            // The thread has left its loop, so this cannot block.
            let _ = handle.join();
        } else {
            log::warn!("serial reader thread did not stop within {JOIN_BOUND:?}; detaching it");
        }
    }
}

impl Link for SerialLink {
    /// Write one frame and wait for the reply that answers it (§2.6, §3.1, A12, A15).
    ///
    /// Ordering matters and is not incidental: the in-flight slot is registered **before** the
    /// write, because at 3.98 ms for `GET_INFO` and 4.15 ms for a keyboard report (A11, Appendix)
    /// the reply can be routed before the writing thread is scheduled again. Registering after the
    /// write would lose it.
    ///
    /// **After a timeout this call blocks before it writes anything**, until the link has been
    /// quiet for [`OpenOptions::quiet_after_timeout`] and for at most twice that — see
    /// [`SerialLink::settle_after_timeout`], which explains why a stale reply cannot otherwise be
    /// told from a fresh one. It costs one quiet window per timeout, and timeouts are rare.
    fn transact(&mut self, cmd: u8, payload: &[u8], timeout: Duration) -> Result<Reply, LinkError> {
        let frame = encode(cmd, payload)
            .map_err(|e| LinkError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, e)))?;

        self.settle_after_timeout()?;

        {
            let mut inner = self.shared.lock();
            if let Some(reason) = &inner.down {
                return Err(LinkError::Down(reason.clone()));
            }
            inner.reply = None;
            inner.inflight = Some(cmd);
            inner.answered_inflight_late = false;
        }
        self.shared
            .counters
            .transacts
            .fetch_add(1, Ordering::Relaxed);

        // One `write_all` for the whole frame, then a flush. Anything less than the whole frame on
        // the wire costs the *following* command too (§5.1, A15), so a failure here is not a
        // retryable hiccup: the chip's parser may now be mid-frame and the link is not trusted.
        if let Err(e) = self.write_raw(&frame, &format!("frame for command {cmd:#04x}")) {
            self.shared.lock().inflight = None;
            return Err(e);
        }

        let deadline = Instant::now() + timeout;
        let mut inner = self.shared.lock();
        loop {
            if let Some(reply) = inner.reply.take() {
                drop(inner);
                return if reply.is_error() {
                    // Never dropped: this is the §3.1 bug, stated as a requirement.
                    let code = reply.error_code().unwrap_or(0);
                    self.shared
                        .counters
                        .device_errors
                        .fetch_add(1, Ordering::Relaxed);
                    log::debug!("device rejected {cmd:#04x} with code {code:#04x}");
                    Err(LinkError::Device { cmd, code })
                } else {
                    Ok(reply)
                };
            }
            if let Some(reason) = inner.down.clone() {
                inner.inflight = None;
                return Err(LinkError::Down(reason));
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                // Give up, and remember the command: a reply that turns up now is late, not a
                // device push, and above all it must not be handed to the *next* transact.
                //
                // Unless the device has already answered it. If a frame answering this command was
                // diverted as the *previous* request's late reply while this one was in flight,
                // the device has now replied as many times as it was asked and arming the slot
                // again would steal the next request's reply too, and the one after that (§3.1).
                // The reader sets that flag; here it means "let the next request match normally".
                inner.inflight = None;
                inner.timed_out = if inner.answered_inflight_late {
                    log::debug!(
                        "{cmd:#04x} timed out but was answered late for its predecessor; not arming"
                    );
                    None
                } else {
                    Some(cmd)
                };
                inner.answered_inflight_late = false;
                // Open the resynchronisation window either way: the next request waits for the
                // link to fall quiet before it goes out.
                inner.quiet_since = Some(Instant::now());
                drop(inner);
                self.shared
                    .counters
                    .timeouts
                    .fetch_add(1, Ordering::Relaxed);
                log::debug!("no reply to {cmd:#04x} within {timeout:?}");
                return Err(LinkError::Timeout { cmd, timeout });
            };
            let (guard, _) = self
                .shared
                .signal
                .wait_timeout(inner, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner = guard;
        }
    }

    /// Write [`RESYNC_PREAMBLE_LEN`] zero bytes in one `write_all` (§5.1, §2.7 step 3a).
    ///
    /// One write, not sixteen: the same A15 rule applies to the preamble as to a frame, since a
    /// preamble that is itself torn leaves the parser exactly where it was found. The size and
    /// safety argument is on [`RESYNC_PREAMBLE_LEN`]; the bytes are counted in
    /// [`SerialStats::bytes_tx`] like any other write, and each call in
    /// [`SerialStats::resyncs_sent`].
    fn resync(&mut self) -> Result<(), LinkError> {
        self.write_raw(&[0u8; RESYNC_PREAMBLE_LEN], "resynchronisation preamble")?;
        self.shared
            .counters
            .resyncs_sent
            .fetch_add(1, Ordering::Relaxed);
        log::debug!(
            "wrote a {RESYNC_PREAMBLE_LEN}-byte zero preamble on {}",
            self.path.display()
        );
        Ok(())
    }

    /// The reader thread's verdict, handed to the writer without writing anything.
    ///
    /// The read half is where a hang-up is noticed first — `serialport` polls before it reads and
    /// reports `POLLHUP` as `BrokenPipe`, so [`reader::run`] marks the link down within one read
    /// timeout of the device going away — and until this existed that verdict stopped here.
    ///
    /// **A reader thread that has ended is itself link loss**, whether or not it recorded a
    /// reason. It is the only thing that can route a reply (`reader::route`), so once it has gone
    /// nothing on this link will ever be answered again: every later `transact` would burn its
    /// whole timeout and be classified `Timeout` — degraded, not down — so input would stop
    /// working while the client went on reporting the link up, which is the shape C10 exists to
    /// forbid. C10 itself is untouched by this: its late-reply slot is a property of a *running*
    /// reader, and `reader_finished` is false for every moment C10 describes.
    ///
    /// The inherent [`SerialLink::is_down`] still answers from the reason alone, because it exists
    /// to say *why* and a reader that simply stopped has no why to give.
    fn is_down(&self) -> bool {
        let inner = self.shared.lock();
        inner.down.is_some() || inner.reader_finished
    }
}

impl Drop for SerialLink {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl std::fmt::Debug for SerialLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SerialLink")
            .field("path", &self.path)
            .field("down", &self.is_down())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serial::fake::{Behaviour, FakeCh9329};

    /// **A reader thread that has gone is link loss, reason or no reason.**
    ///
    /// The reader is the only thing that routes a reply (`reader::route`), so once it has left,
    /// nothing on this link will ever be answered again. If `Link::is_down` answered from the
    /// `down` reason alone, a reader that ended without recording one — a panic in the parser or
    /// the matcher — would leave the writer reporting a healthy link on which every later
    /// `transact` burns its whole timeout and is classified `Timeout`, i.e. *degraded*, never
    /// *down*: input stops working while the client says the cable is fine. C10's late-reply slot
    /// is untouched, because that is a property of a reader still in its loop.
    ///
    /// The reader is stood down here by setting the flag its `Drop` guard sets, which is what a
    /// thread that unwound out of the loop leaves behind. No hardware: the far end is a pty.
    #[test]
    fn a_finished_reader_thread_is_link_loss_even_with_no_reason_recorded() {
        let fake = FakeCh9329::spawn(Behaviour::default()).expect("open a pty pair");
        let link = SerialLink::open(&fake.slave_path()).expect("open the slave end");
        assert!(!Link::is_down(&link), "a fresh link over a live pty is up");

        // What a reader that ended without a verdict leaves: the guard's flag, and no reason.
        let mut inner = link.shared.lock();
        inner.reader_finished = true;
        drop(inner);

        assert!(
            link.is_down().is_none(),
            "the inherent accessor exists to say *why*, and a reader that simply stopped has no \
             why to give"
        );
        assert!(
            Link::is_down(&link),
            "but the writer's question is 'can this link still answer me', and it cannot"
        );
        fake.stop();
    }
}
