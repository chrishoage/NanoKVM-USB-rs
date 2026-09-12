//! MJPEG capture, decoding, and frame-source interfaces.
//!
//! Dimensions come from each JPEG header and are bounded before allocation. The driver
//! can report the requested format while the device still emits another resolution.

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

/// Maximum accepted JPEG width. The 4096 × 2304 limit covers the device's advertised
/// modes while bounding allocations from untrusted header dimensions.
pub const MAX_WIDTH: u32 = 4096;

/// The tallest frame this client will accept from a JPEG start-of-frame header. See
/// [`MAX_WIDTH`].
pub const MAX_HEIGHT: u32 = 2304;

/// The largest RGBA output any acceptable frame can require: `MAX_WIDTH * MAX_HEIGHT * 4`,
/// 36 MiB. No decode scratch buffer in this module ever grows past this.
pub const MAX_RGBA_BYTES: usize = MAX_WIDTH as usize * MAX_HEIGHT as usize * 4;

/// Whether JPEG dimensions are nonzero and within the allocation limits.
pub fn dimensions_in_range(width: u32, height: u32) -> bool {
    width <= MAX_WIDTH && height <= MAX_HEIGHT
}

/// Owned compressed frame with dimensions from its JPEG header.
///
/// `captured_at` is the V4L2 monotonic timestamp; sequence numbers are advisory.
pub struct CompressedFrame {
    /// The complete JPEG, `bytesused` bytes long. Owned; never a view into an mmap
    /// buffer.
    pub jpeg: Vec<u8>,
    /// Width from this frame's SOF header.
    pub width: u32,
    /// Height from this frame's SOF header.
    pub height: u32,
    /// V4L2 buffer timestamp, `CLOCK_MONOTONIC`.
    pub captured_at: Duration,
    /// V4L2 sequence counter. It does not reliably identify resolution changes.
    pub sequence: u32,
}

impl fmt::Debug for CompressedFrame {
    /// Omit image bytes from debug output to keep diagnostics bounded.
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

/// Decoded RGBA8 frame ready for upload.
///
/// Capture timestamp and sequence are preserved from the compressed frame.
pub struct DecodedFrame {
    /// `width * height * 4` bytes of RGBA8.
    pub rgba: Vec<u8>,
    /// Width, cross-checked against the JPEG header dimensions by [`Decoder::decode`].
    pub width: u32,
    /// Height, cross-checked against the JPEG header dimensions by [`Decoder::decode`].
    pub height: u32,
    /// The originating frame's V4L2 buffer timestamp, `CLOCK_MONOTONIC`.
    pub captured_at: Duration,
    /// The originating frame's V4L2 buffer sequence counter.
    pub sequence: u32,
}

impl DecodedFrame {
    /// An empty frame with no allocation. The decode thread keeps one of these as scratch and
    /// grows its `rgba` once.
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
    /// Omit pixel data from debug output to keep diagnostics bounded.
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

/// Capture failure conditions. HDMI signal state is unavailable on this device
/// and is never inferred from image content.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// The device has disconnected.
    #[error("capture device disconnected")]
    Disconnected,
    /// No buffer arrived within the timeout. This does not establish HDMI signal loss.
    #[error("no frame within {0:?}")]
    Timeout(Duration),
    /// A frame arrived but is not usable: no parseable start-of-frame header, zero-length, or
    /// dimensions the header calls impossible.
    #[error("unusable frame: {0}")]
    BadFrame(String),
    /// A device I/O error, `EIO` among them. Survivable — the capture thread counts and retries.
    #[error("capture I/O error: {0}")]
    Io(#[source] std::io::Error),
    /// The driver refused or changed the requested format or frame interval.
    #[error("capture configuration: {0}")]
    Config(String),
}

/// A source of owned compressed frames, injectable for hardware-free pipeline tests.
pub trait FrameSource: Send {
    /// Wait for the next owned frame.
    ///
    /// Implementations must bound the wait and return [`CaptureError::Timeout`] on a stall.
    fn next_frame(&mut self) -> Result<CompressedFrame, CaptureError>;

    /// A one-line description for logs and error messages, e.g. the node and negotiated mode.
    fn describe(&self) -> String;

    /// Restart streaming without reopening or renegotiating the device.
    ///
    /// The default is a successful no-op. V4L2 rebuilds the stream to clear buffer errors.
    /// [`CaptureError::Disconnected`] requests a reopen; other errors count as failed
    /// restarts and polling continues.
    fn restart(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }

    /// The negotiated dimensions, or `None` for sources without format negotiation.
    ///
    /// The pipeline reads this once per open for its format watchdog. The value must stay
    /// stable for the source's lifetime, including across `restart()`. Actual frame
    /// dimensions still come from each JPEG header. `None` disables the size watchdog.
    fn negotiated_dimensions(&self) -> Option<(u32, u32)> {
        None
    }
}

/// Opens replacement frame sources after disconnection.
///
/// Return failures promptly; the pipeline retries with exponential backoff. A source
/// may temporarily fail with a missing, busy, or not-yet-bound device during replug.
pub trait SourceOpener: Send {
    /// Open the device and start streaming, or say why not.
    fn open(&mut self) -> Result<Box<dyn FrameSource>, CaptureError>;

    /// A one-line description of *what would be opened*, for logs and the window title. Unlike
    /// [`FrameSource::describe`] this must work with no device present.
    fn describe(&self) -> String;

    /// Set the format for the next open.
    ///
    /// Returns `false` when renegotiation is unsupported, leaving the current stream intact.
    /// A successful request is applied by reopening because V4L2 rejects format changes
    /// while streaming. The default is `false`.
    fn set_format(&mut self, _width: u32, _height: u32, _fps: u32) -> bool {
        false
    }
}

/// Recover poisoned locks: these mutexes hold independent counters or frame slots,
/// so propagating poisoning would disable otherwise usable capture state.
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
    //! Committed capture fixtures for unit tests.

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

    /// Change only the JPEG header dimensions to exercise allocation bounds against
    /// a structurally valid frame with a corrupt size.
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
