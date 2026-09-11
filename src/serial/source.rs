//! [`SerialLinkSource`]: the writer's supply of fresh serial transports (§2.7, §12 Stage 2).
//!
//! # Why the unsolicited receiver outlives the link
//!
//! A [`SerialLink`] owns one port, one reader thread and one unsolicited channel, and it never
//! comes back after a transport failure — reconnect builds a *new* link (§4.1, §2.7). But the
//! consumer of the unsolicited `0x81` lock-state push is long-lived: it is the one end-to-end
//! check that needs no video (A12), it is what proves the *target* processed a keystroke, and in
//! the client it is a logger thread started once at startup. If that consumer held the receiver
//! belonging to link *n*, a reconnect would leave it holding a permanently disconnected channel
//! while link *n+1*'s pushes went to a receiver nobody owns — the evidence would silently stop
//! arriving at exactly the moment something interesting had happened.
//!
//! So the channel the consumer holds belongs to the **source**, not to any link.
//! [`SerialLinkSource::new`] returns it once, and every link the source opens gets a small
//! forwarder thread that drains that link's own receiver into it. A forwarder ends when its link's
//! reader thread ends and its sender hangs up — which is what dropping the link does — so the
//! threads are as short-lived as the links are, and never outlive them. A forwarder also ends if
//! the long-lived consumer drops its receiver, which is allowed and costs only the A12 evidence
//! (see [`SerialLink::take_unsolicited`]).
//!
//! Nothing here decides *when* to reopen. That is the writer's, because §2.7's sequence needs the
//! input queue and the epoch bookkeeping (see [`crate::input::spawn_with_source`]).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use crate::link::{Link, LinkError, LinkSource, Reply};
use crate::serial::link::SerialLink;
use crate::serial::port::OpenOptions;

/// Opens [`SerialLink`]s on one fixed path, on demand (§2.7).
///
/// **This is the explicit-path case, and only that.** It reopens the name it was given and never
/// looks for the device anywhere else, which is what `--serial` promises (§8: an explicit override
/// is honoured permanently).
///
/// It used to say here that a fixed path was good enough in general, because the kernel gives the
/// CH9329 bridge the same node back across a replug on this desk. **That was falsified by
/// measurement** (H-A4): the kernel hands back the lowest *free* name, and it is only free if
/// nothing still holds it. A writer that was idle when the device went away kept its fd open, so
/// `acm_port_destruct` never ran, the tty index was never released, and `/dev/ttyACM1` came back
/// as `/dev/ttyACM2` — permanently. Reopening by name then retries a node that no longer exists,
/// for ever. The same-name outcome is a race the client happened to keep winning while it was
/// writing at the moment of the unplug, not a property to build reconnect on.
///
/// So the identity-based reopen is [`crate::discovery::reopen::DiscoveringLinkSource`], which
/// wraps this type and re-resolves the node by §8 before every open. That is what the binary uses
/// and what a hardware test that survives a replug must use.
pub struct SerialLinkSource {
    path: PathBuf,
    opts: OpenOptions,
    /// The sending half of the channel handed to the long-lived consumer by
    /// [`SerialLinkSource::new`]. Cloned into every forwarder.
    unsolicited: Sender<Reply>,
    /// One forwarder per link opened. Reaped on the next `open`, so a source that reconnects for
    /// a week does not accumulate handles for threads that ended days ago.
    forwarders: Vec<JoinHandle<()>>,
}

impl SerialLinkSource {
    /// A source for `path`, and the unsolicited-frame receiver that survives every reopen.
    ///
    /// The receiver is returned separately, and exactly once, so that its lifetime cannot be
    /// confused with any one link's. It is unbounded: the device is silent unless something
    /// happens (forty seconds of idle listening produced zero bytes, A12), so a consumer that
    /// drains it occasionally is fine and one that drops it is safe — see the module
    /// documentation.
    pub fn new(path: &Path, opts: OpenOptions) -> (SerialLinkSource, Receiver<Reply>) {
        let (tx, rx) = mpsc::channel();
        (
            SerialLinkSource {
                path: path.to_path_buf(),
                opts,
                unsolicited: tx,
                forwarders: Vec::new(),
            },
            rx,
        )
    }

    /// The path this source reopens.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Join the forwarders whose links have already gone. Never blocks: `is_finished` is checked
    /// first, so every `join` here is on a thread that has already returned.
    fn reap(&mut self) {
        let mut live = Vec::with_capacity(self.forwarders.len());
        for handle in self.forwarders.drain(..) {
            if handle.is_finished() {
                let _ = handle.join();
            } else {
                live.push(handle);
            }
        }
        self.forwarders = live;
    }
}

impl LinkSource for SerialLinkSource {
    fn open(&mut self) -> Result<Box<dyn Link>, LinkError> {
        self.reap();
        let mut link = SerialLink::open_with(&self.path, self.opts)?;
        let from_link = link.take_unsolicited();
        let to_consumer = self.unsolicited.clone();
        let handle = std::thread::Builder::new()
            .name("nanokvm-serial-unsol".to_string())
            .spawn(move || {
                // Ends when the link's reader thread drops its sender (the link was dropped), or
                // when the long-lived consumer drops its receiver. Both are ordinary.
                for reply in from_link {
                    if to_consumer.send(reply).is_err() {
                        log::debug!("unsolicited consumer went away; forwarder ending");
                        return;
                    }
                }
                log::debug!("link closed; unsolicited forwarder ending");
            })
            .map_err(|e| LinkError::Down(format!("spawn unsolicited forwarder thread: {e}")))?;
        self.forwarders.push(handle);
        Ok(Box::new(link))
    }

    fn describe(&self) -> String {
        format!("{} at {} baud", self.path.display(), self.opts.baud)
    }
}

impl std::fmt::Debug for SerialLinkSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SerialLinkSource")
            .field("path", &self.path)
            .field("forwarders", &self.forwarders.len())
            .finish_non_exhaustive()
    }
}
