//! Reopenable serial link sources.
//!
//! A source supplies fresh links after transport failure. Explicit paths stay fixed;
//! USB-topology rediscovery is supplied by the discovery adapter.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use crate::link::{Link, LinkError, LinkSource, Reply};
use crate::serial::link::SerialLink;
use crate::serial::port::OpenOptions;

/// Reopen serial at one fixed path. This implements explicit `--serial` selection;
/// use the discovery adapter when node renumbering must be followed.
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
    /// Create a source and its unsolicited-frame receiver. The receiver survives link
    /// replacement and is returned once. Its channel is unbounded; callers should drain
    /// notifications or drop the receiver when they are no longer needed.
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
