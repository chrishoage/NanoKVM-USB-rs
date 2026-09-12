//! Synchronous CH9329 transactions over an independently running reader.
//!
//! One request is outstanding at a time. Replies match the command byte rather than their
//! arrival order. After a timeout, late replies are quarantined before another request of
//! the same command can be matched.
//!
//! Fresh links receive a zero preamble before commissioning: the bridge retains incomplete
//! frames indefinitely and can otherwise consume the next request as their remaining bytes.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::link::{Link, LinkError, Reply};
use crate::proto::frame::DeviceInfo;
use crate::proto::usb_string::{parse_usb_string, UsbStringKind, UsbStrings};
use crate::proto::{cmd, encode};
use crate::serial::port::{self, OpenOptions};
use crate::serial::reader::{self, SerialStats, Shared};

/// Command byte of the device's lock-state push, which is also `GET_INFO`'s reply.
const LOCK_STATE: u8 = cmd::GET_INFO | 0x80;

/// How long [`SerialLink::drop`] waits for the reader thread before giving up on it.
const JOIN_BOUND: Duration = Duration::from_secs(1);

/// Zero bytes written to finish an incomplete input frame before commissioning.
///
/// Sixteen covers the remaining bytes of supported keyboard and mouse reports; surplus
/// zeros cannot form a header. Release-all follows, and `GET_INFO` verifies realignment.
/// This is not a recovery guarantee for arbitrary bridge-configuration commands.
pub const RESYNC_PREAMBLE_LEN: usize = 16;

/// CH9329 link owning a port, reader thread, and one outstanding request.
///
/// Mutable transaction access prevents concurrent writes. The reader separately routes
/// unsolicited notifications and transaction replies.
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
    /// Kept for diagnostics and for reopen.
    path: std::path::PathBuf,
}

impl SerialLink {
    /// Open with default settings and start the reader. The caller performs commissioning
    /// and can query information through [`SerialLink::get_info`].
    pub fn open(path: &Path) -> Result<SerialLink, LinkError> {
        SerialLink::open_with(path, OpenOptions::default())
    }

    /// Open with explicit port settings and reader deadlines.
    ///
    /// Clone the read handle before setting the write timeout so each keeps its own deadline.
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
    /// A link goes down on a failed write or a read error that is not a timeout —
    /// `ENODEV`, `EIO`, a hang-up. It never comes back up: reconnect builds a new link.
    pub fn is_down(&self) -> Option<String> {
        self.shared.down_reason()
    }

    /// Counters for everything the link has seen, including everything it discarded.
    pub fn stats(&self) -> SerialStats {
        self.shared.counters.snapshot()
    }

    /// Receiver for unsolicited frames, usually target lock-state notifications.
    /// Returns `None` after [`SerialLink::take_unsolicited`] transfers ownership.
    pub fn unsolicited(&self) -> &Receiver<Reply> {
        &self.unsolicited
    }

    /// Take the unsolicited receiver so another thread can consume notifications while
    /// the writer owns the link. Subsequent calls return `None`.
    pub fn take_unsolicited(&mut self) -> Receiver<Reply> {
        let (_disconnected_tx, dummy) = std::sync::mpsc::channel();
        std::mem::replace(&mut self.unsolicited, dummy)
    }

    /// Decode a `0x81` frame — solicited or pushed — as [`DeviceInfo`].
    ///
    /// `None` if the reply is not a lock-state frame, or is one whose payload is too short to
    /// carry the documented fields. Device-supplied lengths are never trusted.
    pub fn lock_state_from(reply: &Reply) -> Option<DeviceInfo> {
        if reply.cmd != LOCK_STATE {
            return None;
        }
        DeviceInfo::parse(&reply.data).ok()
    }

    /// Query firmware, target connection, and lock state.
    ///
    /// A malformed payload returns [`LinkError::Io`] with `InvalidData`; it does not mark
    /// the transport down because a reply was received.
    pub fn get_info(&mut self, timeout: Duration) -> Result<DeviceInfo, LinkError> {
        let reply = self.transact(cmd::GET_INFO, &[], timeout)?;
        DeviceInfo::parse(&reply.data).map_err(|e| {
            LinkError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("GET_INFO reply: {e}"),
            ))
        })
    }

    /// Query manufacturer, product, and unit serial strings in three transactions.
    ///
    /// Validate each reply selector because all replies share command 0x8A. Malformed
    /// payloads return [`LinkError::Io`] with `InvalidData` without marking the link down.
    pub fn get_usb_strings(&mut self, timeout: Duration) -> Result<UsbStrings, LinkError> {
        let mut strings = UsbStrings::default();
        for kind in UsbStringKind::ALL {
            let reply = self.transact(cmd::GET_USB_STRING, &[kind.request_byte()], timeout)?;
            log::debug!("GET_USB_STRING {kind} reply payload: {:02X?}", reply.data);
            let text = parse_usb_string(kind, &reply.data).map_err(|e| {
                LinkError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("GET_USB_STRING {kind} reply: {e}"),
                ))
            })?;
            strings.set(kind, text);
        }
        Ok(strings)
    }

    /// Write bytes straight at the port, outside the framing layer.
    ///
    /// The one primitive `transact` and [`SerialLink::resync`] share, so that "a failed write puts
    /// the link down and stays down" is stated once. `what` names the write for the
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

    /// Write raw bytes for hardware tests that deliberately tear a frame. Production
    /// callers must send complete frames through the transaction interface.
    #[doc(hidden)]
    pub fn write_raw_for_test(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
        self.write_raw(bytes, "deliberate raw test write")
    }

    /// Wait for quiet after a timeout before registering another request.
    ///
    /// Old and fresh replies to the same command are indistinguishable. Wait for
    /// `quiet_after_timeout`, bounded by twice that interval; the late-reply slot remains
    /// armed if the bound expires. Link loss interrupts the wait.
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
    /// Send one complete frame and wait for its matching reply.
    ///
    /// Register the request before writing so a fast reply cannot arrive before its waiter.
    /// A previous timeout first incurs the bounded quiet window.
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
        // the wire costs the *following* command too, so a failure here is not a
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
                    // Device error replies must reach the caller; dropping them would turn rejection into timeout.
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
                // Remember the timed-out command so its eventual reply cannot satisfy the next request.
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

    /// Write the complete zero preamble and count its bytes and resynchronization attempt.
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

    /// Report transport loss without writing. A finished reader also means link loss
    /// because no thread remains to route transaction replies.
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

    /// A stopped reader cannot answer transactions even when no failure reason was recorded.
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
