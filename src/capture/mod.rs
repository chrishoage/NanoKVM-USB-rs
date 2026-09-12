//! V4L2 MJPEG capture, JPEG decode and the frame handoffs (plan §4, §5.2, §6).
//!
//! The shape of this module is fixed by three measured facts:
//!
//! - The device offers **only** `MJPG`, so decode is unconditionally on the hot path (§6, A1).
//! - Frame dimensions come from the JPEG start-of-frame header of *that* frame, never from
//!   `G_FMT`: after an idle gap the device emits up to eight frames at the previous resolution
//!   with the new `sizeimage` and no error flag (§6, A6).
//! - `S_PARM` must be called explicitly or the driver keeps its own default rate (§6, A7).
//!
//! Layout:
//!
//! - [`jpeg`]      — start-of-frame header parsing. Pure, never panics.
//! - [`decoder`]   — zune-jpeg, RGBA out, strict mode, reused output buffer (§1.3, A19).
//! - [`handoff`]   — [`Slot`], the one-pending-owned-frame handoff (§5.2).
//! - [`v4l2`]      — [`V4l2Source`], the real capture device.
//! - [`synthetic`] — [`SyntheticSource`], the no-hardware source (§4, §9.3).
//! - [`pipeline`]  — the capture and decode threads (§4.1).
//!
//! Every frame handed across a boundary **owns its bytes**. Nothing in this module ever hands
//! out a reference into an mmap buffer that will be requeued (§5.2, §9.2 item 8).

pub mod decoder;
pub mod handoff;
pub mod jpeg;
pub mod pipeline;
pub mod synthetic;
pub mod v4l2;

pub use decoder::{DecodeError, Decoder};
pub use handoff::Slot;
pub use jpeg::JpegHeaderError;
pub use pipeline::{Pipeline, PipelineConfig, PipelineHandle, PipelineState, PipelineStats};
pub use synthetic::SyntheticSource;
pub use v4l2::{BytesUsedStats, V4l2Opener, V4l2Source};

use std::fmt;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

/// The widest frame this client will accept from a JPEG start-of-frame header (§6, A6).
///
/// A6 makes the frame's own SOF header the only authority on its size, and that stays true —
/// this is a **sanity ceiling, not a format check**. It exists because the header is untrusted
/// bytes off a USB link: A6's stale frames and A15's torn transfers can flip a byte in the SOF,
/// and two bytes there are the difference between a 1920x1080 frame and a 16384x16384 one. The
/// device's largest advertised mode is 3840x2160 (§6), so 4096 wide by 2304 high covers every
/// mode it can produce with slack, and refuses everything else before a single byte is
/// allocated.
pub const MAX_WIDTH: u32 = 4096;

/// The tallest frame this client will accept from a JPEG start-of-frame header. See
/// [`MAX_WIDTH`].
pub const MAX_HEIGHT: u32 = 2304;

/// The largest RGBA output any acceptable frame can require: `MAX_WIDTH * MAX_HEIGHT * 4`,
/// 36 MiB. No decode scratch buffer in this module ever grows past this.
pub const MAX_RGBA_BYTES: usize = MAX_WIDTH as usize * MAX_HEIGHT as usize * 4;

/// Whether a start-of-frame header's dimensions are inside the [`MAX_WIDTH`] ceiling.
///
/// Every caller of [`jpeg::dimensions`] on device bytes checks this and turns a failure into
/// [`CaptureError::BadFrame`], so an oversized header is counted as the unusable frame it is
/// rather than being paid for in memory first.
pub fn dimensions_in_range(width: u32, height: u32) -> bool {
    width <= MAX_WIDTH && height <= MAX_HEIGHT
}

/// One compressed frame, copied out of the capture buffer and owned outright (§5.2).
///
/// `width`/`height` are read from this frame's own JPEG start-of-frame header, never from
/// `G_FMT` (§6, A6). `captured_at` is the V4L2 buffer timestamp on `CLOCK_MONOTONIC`, which
/// Stage 0 verified is always `TIMESTAMP_MONOTONIC | TSTAMP_SRC_SOE` and directly subtractable
/// from a local `CLOCK_MONOTONIC` read (§5.5).
pub struct CompressedFrame {
    /// The complete JPEG, exactly `bytesused` bytes long. Owned; never a view into an mmap
    /// buffer (§9.2 item 8).
    pub jpeg: Vec<u8>,
    /// Width from this frame's SOF header.
    pub width: u32,
    /// Height from this frame's SOF header.
    pub height: u32,
    /// V4L2 buffer timestamp, `CLOCK_MONOTONIC` (§5.5).
    pub captured_at: Duration,
    /// V4L2 buffer sequence counter. Advisory only — A6 says it carries no reliable tell about
    /// a resolution change, so never infer dimensions from it.
    pub sequence: u32,
}

impl fmt::Debug for CompressedFrame {
    /// Prints the byte count rather than the bytes; a 1080p frame is ~170 KB (§6).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompressedFrame")
            .field("jpeg_len", &self.jpeg.len())
            .field("width", &self.width)
            .field("height", &self.height)
            .field("captured_at", &self.captured_at)
            .field("sequence", &self.sequence)
            .finish()
    }
}

/// One decoded frame, RGBA8, upload-ready (§1.3: zune emits RGBA directly; repacking costs 34 %).
///
/// `captured_at` and `sequence` are carried through unchanged from the [`CompressedFrame`] this
/// was decoded from, so freshness is measured against capture and not against decode (§5.5).
pub struct DecodedFrame {
    /// `width * height * 4` bytes of RGBA8.
    pub rgba: Vec<u8>,
    /// Width, cross-checked against the JPEG header dimensions by [`Decoder::decode`].
    pub width: u32,
    /// Height, cross-checked against the JPEG header dimensions by [`Decoder::decode`].
    pub height: u32,
    /// The originating frame's V4L2 buffer timestamp, `CLOCK_MONOTONIC` (§5.5).
    pub captured_at: Duration,
    /// The originating frame's V4L2 buffer sequence counter.
    pub sequence: u32,
}

impl DecodedFrame {
    /// An empty frame with no allocation. The decode thread keeps one of these as scratch and
    /// grows its `rgba` once (§1.3: decode into a reused buffer).
    pub fn empty() -> Self {
        DecodedFrame {
            rgba: Vec::new(),
            width: 0,
            height: 0,
            captured_at: Duration::ZERO,
            sequence: 0,
        }
    }

    /// The number of bytes a frame of this size must carry: `width * height * 4`.
    pub fn expected_len(width: u32, height: u32) -> usize {
        width as usize * height as usize * 4
    }
}

impl Default for DecodedFrame {
    fn default() -> Self {
        Self::empty()
    }
}

impl fmt::Debug for DecodedFrame {
    /// Prints the byte count rather than 8 MB of pixels.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecodedFrame")
            .field("rgba_len", &self.rgba.len())
            .field("width", &self.width)
            .field("height", &self.height)
            .field("captured_at", &self.captured_at)
            .field("sequence", &self.sequence)
            .finish()
    }
}

/// Why a frame did not arrive.
///
/// The three conditions §6.1 asks to distinguish collapse to two on this hardware: signal loss
/// is not reportable (A5), so there is deliberately no variant for it and nothing here is ever
/// inferred from pixel content.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// The node is gone — `ENODEV`. Say so; Stage 2 owns rediscovery (§6.1).
    #[error("capture device disconnected")]
    Disconnected,
    /// No buffer became ready within the dequeue timeout. The capture stalled; that is *not*
    /// evidence the signal is gone (§6.1). Survivable — the capture thread keeps polling.
    #[error("no frame within {0:?}")]
    Timeout(Duration),
    /// A frame arrived but is not usable: no parseable start-of-frame header, zero-length, or
    /// dimensions the header calls impossible.
    #[error("unusable frame: {0}")]
    BadFrame(String),
    /// A device I/O error, `EIO` among them. Survivable — the capture thread counts and retries.
    #[error("capture I/O error: {0}")]
    Io(#[source] std::io::Error),
    /// The requested configuration was refused or silently altered: a driver that returns a
    /// format or frame interval other than the one asked for (§6 — never accept a different
    /// mode silently).
    #[error("capture configuration: {0}")]
    Config(String),
}

/// A source of compressed frames. The one seam §4 justifies for capture: it lets the pipeline,
/// the decoder and eventually the viewer run against [`SyntheticSource`] with no hardware (§9.3).
pub trait FrameSource: Send {
    /// Block until the next frame is available, or fail.
    ///
    /// Implementations must return an owned frame and must not block indefinitely: a stall has
    /// to surface as [`CaptureError::Timeout`] so §6.1 S1-2 is observable rather than a hang.
    fn next_frame(&mut self) -> Result<CompressedFrame, CaptureError>;

    /// A one-line description for logs and error messages, e.g. the node and negotiated mode.
    fn describe(&self) -> String;

    /// Stop and restart streaming on the same device, without reopening it (§6.1: a capture
    /// stall gets "attempt restart", as distinct from a disconnection's "attempt rediscovery").
    ///
    /// This is the cheap half of recovery: the node is still there and the fd is still valid,
    /// but no buffer has come back for a while. `V4l2Source` answers with the B2 rebuild —
    /// `STREAMOFF`, re-prime, `STREAMON` — which is also the only thing that clears vb2's
    /// sticky queue-error state. Every other source has nothing to restart, so the default is
    /// to do nothing and succeed: a source that cannot stall cannot need this.
    ///
    /// # Errors
    ///
    /// [`CaptureError::Disconnected`] if the node turned out to be gone, which the pipeline
    /// takes as a disconnection rather than a failed restart. Anything else is counted and the
    /// pipeline keeps polling.
    fn restart(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }

    /// The dimensions this source *negotiated* with the device, if it negotiated any.
    ///
    /// This is **not** a source of frame dimensions — A6 keeps the JPEG start-of-frame header of
    /// each frame as the only authority on that size, and that is unchanged. It is the other
    /// half of a comparison: what `S_FMT` committed to, so the pipeline can notice that the
    /// device is streaming something else. Measured on 2026-09-11, a USB reset can leave the
    /// dongle's *stream* in its power-on 640x480 mode with the reopen's UVC commit lost (the
    /// V4L2 `ERROR` flag on the first buffer is the tell) while `G_FMT` still reports the
    /// 1920x1080 that was asked for; a fresh `STREAMON` re-commits and fixes it. See
    /// [`super::pipeline::PipelineConfig::format_mismatch_grace`].
    ///
    /// **Read once per device open, not per frame or per restart.** The pipeline calls this when
    /// it arms the format watchdog on a freshly opened source, and it is entitled to assume the
    /// answer does not move under it while that source lives: `restart()` is documented above as
    /// *not* reopening the device, so an implementation whose `restart()` renegotiated the format
    /// would be changing something the pipeline reads exactly once.
    ///
    /// The default is `None` — "this source negotiated nothing", which is the truth for
    /// [`super::SyntheticSource`] and for every test double — and a `None` here leaves the
    /// pipeline's format watchdog inert, since there is nothing to compare against.
    fn negotiated_dimensions(&self) -> Option<(u32, u32)> {
        None
    }
}

/// Produces a fresh [`FrameSource`] after the previous one disconnected (§6.1: "attempt
/// rediscovery"). The pipeline owns one for its whole life, so recovery needs no help from the
/// caller and no reference to a device the caller has already lost.
///
/// The pipeline calls [`SourceOpener::open`] once at startup and then once per reopen attempt,
/// spacing the attempts with an exponential backoff. **Failure is the normal case while a
/// device is coming back**: `ENOENT` (the node is not there yet), `ENODEV` (it is there but the
/// driver has not bound), and `EBUSY` (the driver is mid-probe) all mean "not yet", so an
/// implementation reports them and is called again rather than being expected to block.
pub trait SourceOpener: Send {
    /// Open the device and start streaming, or say why not.
    fn open(&mut self) -> Result<Box<dyn FrameSource>, CaptureError>;

    /// A one-line description of *what would be opened*, for logs and the window title. Unlike
    /// [`FrameSource::describe`] this must work with no device present.
    fn describe(&self) -> String;

    /// Change the mode the **next** [`SourceOpener::open`] will negotiate (§12 Stage 4b).
    ///
    /// This is the smallest runtime format switch the chrome's Video popover needs, and it is
    /// deliberately shaped as "change what a reopen will ask for" rather than "change the format
    /// of the open device". V4L2 refuses `S_FMT` on a streaming node, and the pipeline already
    /// owns a reopen path that does `S_FMT`/`S_PARM`/`STREAMON` correctly and counts it
    /// (`Step::Reopen`, C14's format watchdog); a second route into the driver would be a second
    /// copy of the one negotiation this client does not want two of.
    ///
    /// Returns `false` for an opener that cannot renegotiate — every test double, and
    /// [`super::pipeline::Pipeline::start`]'s one-shot wrapper, which has no device to reopen. A
    /// `false` means the pipeline leaves the current mode alone and says so, rather than
    /// destroying a working stream to reopen at the same size.
    ///
    /// The default is `false`: an opener that has not thought about this cannot renegotiate.
    fn set_format(&mut self, _width: u32, _height: u32, _fps: u32) -> bool {
        false
    }
}

/// Lock-poisoning policy for this module.
///
/// Every mutex here guards plain counters or an `Option<Frame>`; no invariant spans an unwind,
/// so a poisoned lock carries no corrupt state worth refusing. Recover the guard instead of
/// panicking, so that one panicking thread cannot wedge the capture path (§6.1 S1-1: a capture
/// failure must never tear down the rest of the client).
pub(crate) fn lock_or_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source with nothing to restart must succeed rather than fail, or every synthetic source
    /// would count a failed restart the moment the pipeline's stall timer fired.
    #[test]
    fn the_default_restart_succeeds_and_does_nothing() {
        struct Inert;
        impl FrameSource for Inert {
            fn next_frame(&mut self) -> Result<CompressedFrame, CaptureError> {
                Err(CaptureError::Timeout(Duration::ZERO))
            }
            fn describe(&self) -> String {
                "inert".to_string()
            }
        }
        let mut s = Inert;
        assert!(s.restart().is_ok());
        assert!(s.restart().is_ok(), "restart must be repeatable");
    }
}

#[cfg(test)]
pub(crate) mod testsupport {
    //! Fixture access for the unit tests. `fixtures/frames/absrange/` holds real 1920x1080
    //! frames off this device (§9.1: captures are fixtures, and tests read them rather than
    //! carrying transcribed copies).

    use std::path::{Path, PathBuf};

    /// Every `.jpg` under `fixtures/frames/<sub>`, sorted. `sub` may be empty for the top level.
    pub fn fixture_paths(sub: &str) -> Vec<PathBuf> {
        let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/frames");
        if !sub.is_empty() {
            dir.push(sub);
        }
        let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jpg"))
            .collect();
        out.sort();
        out
    }

    /// Read one fixture.
    pub fn fixture_bytes(path: &Path) -> Vec<u8> {
        std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    /// Rewrite a real frame's start-of-frame Y/X fields in place, which is what a flipped byte
    /// in an A6 stale frame or an A15 torn transfer does to them. Everything else is left
    /// alone, so the result is a well-formed JPEG that lies about its size — the exact input
    /// the [`super::MAX_WIDTH`] ceiling exists for.
    pub fn patch_sof(bytes: &mut [u8], height: u16, width: u16) {
        let mut at = 2usize; // past SOI
        loop {
            assert_eq!(bytes[at], 0xFF, "lost the marker stream at {at}");
            let marker = bytes[at + 1];
            let len = u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]) as usize;
            let is_sof = (0xC0..=0xCF).contains(&marker)
                && marker != 0xC4
                && marker != 0xC8
                && marker != 0xCC;
            if is_sof {
                bytes[at + 5..at + 7].copy_from_slice(&height.to_be_bytes());
                bytes[at + 7..at + 9].copy_from_slice(&width.to_be_bytes());
                return;
            }
            at += 2 + len;
        }
    }
}
