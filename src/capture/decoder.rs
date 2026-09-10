//! MJPEG decode: zune-jpeg, RGBA out, strict mode, one reused output buffer (§1.3, A19).
//!
//! Every setting here was measured rather than chosen:
//!
//! - **RGBA output.** zune emits RGBA through a dedicated AVX2 kernel at no cost. Decoding to
//!   RGB and repacking on the CPU costs **34 %** — 0.80 ms per frame at 1080p (A19).
//! - **Strict mode.** Without it a truncated frame decodes as `Ok` with a partial image. A6's
//!   stale frames and A15's torn writes make silently accepting partial data the wrong default,
//!   and strict mode costs nothing measurable (A19).
//! - **A reused output buffer.** `decode_into` on a `Vec` we keep, not `decode()` returning a
//!   fresh one.
//! - **One decode thread**, which is §4.1's row: more threads raise aggregate throughput but
//!   never reduce per-frame latency, and §5.2's one-pending-frame rule leaves them nothing to do.
//!
//! Measured budget at the primary mode (A19, Ryzen 9 9950X3D): 1080p median **2.40 ms**, 6.9x
//! margin at 60 fps.
//!
//! ## The dimension ceiling
//!
//! A6 says the frame's own SOF header is the only authority on its size, so the header decides
//! how large the output buffer has to be. That makes two bytes of untrusted device data the
//! sole input to an allocation, and A6's stale frames and A15's torn transfers are exactly the
//! things that flip such a byte. zune's own default limit is 16384x16384 — a **1 GiB** zeroed
//! allocation and a ~1 s decode from one corrupt frame — so this module clamps to
//! [`MAX_WIDTH`]x[`MAX_HEIGHT`] in two independent places: [`DecoderOptions::set_max_width`]
//! and `set_max_height` inside zune, and an explicit check in [`Decoder::decode`] that runs
//! **before** anything is allocated. The capture sources check the same ceiling against the
//! header at dequeue time, so an oversized frame is normally refused before it ever reaches
//! here. Note this is a ceiling, not a format check: the dimensions still come from the header
//! and never from `G_FMT`.

use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

use super::{CompressedFrame, DecodedFrame, MAX_HEIGHT, MAX_RGBA_BYTES, MAX_WIDTH};

/// Why a frame did not decode.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// zune refused the frame. In strict mode this is what a truncated or corrupt frame
    /// produces, which is the point (§1.3).
    #[error("jpeg decode failed: {0}")]
    Jpeg(String),
    /// The decoder read a different size out of the headers than [`super::jpeg::dimensions`]
    /// did. The two parsers disagreeing means the frame is not what it claims; refuse it rather
    /// than hand the renderer a buffer whose shape is uncertain (A6).
    #[error("dimension disagreement: header says {header_w}x{header_h}, decoder says {decoder_w}x{decoder_h}")]
    DimensionMismatch {
        header_w: u32,
        header_h: u32,
        decoder_w: u32,
        decoder_h: u32,
    },
    /// zune's required output size is not `width * height * 4`, so the colorspace request did
    /// not take effect. Never expected; refusing beats writing a wrongly-shaped buffer.
    #[error("decoder wants {wanted} bytes for {width}x{height} RGBA, expected {expected}")]
    UnexpectedBufferSize {
        width: u32,
        height: u32,
        wanted: usize,
        expected: usize,
    },
    /// The headers parsed but the decoder would not state a size.
    #[error("decoder reported no dimensions")]
    NoDimensions,
    /// The frame's header claims a size beyond the [`MAX_WIDTH`]x[`MAX_HEIGHT`] sanity ceiling.
    ///
    /// Refused **before** the output buffer is sized, because the whole point is that the size
    /// is what the corrupt header controls. See the module docs.
    #[error("frame claims {width}x{height}, over the {max_width}x{max_height} ceiling")]
    DimensionsTooLarge {
        width: u32,
        height: u32,
        max_width: u32,
        max_height: u32,
    },
}

/// A reusable MJPEG decoder (§1.3).
///
/// Holds the measured [`DecoderOptions`] and nothing else: zune's `JpegDecoder` borrows its
/// input, so it is constructed per frame while the *expensive* thing — the RGBA output buffer —
/// lives in the caller's [`DecodedFrame`] and is reused across frames.
///
/// One of these per decode thread. There is exactly one decode thread (§4.1).
pub struct Decoder {
    options: DecoderOptions,
}

impl Decoder {
    /// A decoder with the A19 settings — RGBA colorspace, strict mode on — plus the
    /// [`MAX_WIDTH`]x[`MAX_HEIGHT`] ceiling.
    ///
    /// zune's own defaults are 16384x16384, which is 1 GiB of zeroed scratch from one corrupt
    /// SOF byte. Lowering them here means zune refuses such a header while parsing it, before
    /// this module has computed anything from it.
    pub fn new() -> Self {
        Decoder {
            options: DecoderOptions::default()
                .jpeg_set_out_colorspace(ColorSpace::RGBA)
                .set_strict_mode(true)
                .set_max_width(MAX_WIDTH as usize)
                .set_max_height(MAX_HEIGHT as usize),
        }
    }

    /// The options in force, so a test can assert the A19 settings rather than trust them.
    pub fn options(&self) -> &DecoderOptions {
        &self.options
    }

    /// Decode `frame` into `out`, reusing `out.rgba`'s allocation.
    ///
    /// `out.rgba` is resized to exactly `width * height * 4`; growing within the existing
    /// capacity does not reallocate, so a steady-state pipeline at a fixed resolution allocates
    /// once. On success `out` carries the frame's dimensions and its `captured_at`/`sequence`
    /// copied through unchanged, so freshness is still measured from capture (§5.5).
    ///
    /// The decoder's own header dimensions are checked against the [`CompressedFrame`]'s, which
    /// came from [`super::jpeg::dimensions`] over the same bytes. A6 makes the frame's header
    /// the only authority on its size, so the two must agree. That cross-check compares two
    /// parsers of the *same* bytes, so it catches a parser disagreement and nothing else — the
    /// [`MAX_WIDTH`]x[`MAX_HEIGHT`] ceiling below is what catches a corrupt SOF, and it is
    /// applied before any allocation.
    ///
    /// # Errors
    ///
    /// A truncated frame is an error, not a partial image — that is strict mode doing its job
    /// (§1.3). A frame over the ceiling is [`DecodeError::DimensionsTooLarge`]. In every error
    /// case `out` keeps the contents and the capacity it had, so a rejected frame costs nothing
    /// and leaves the decoder ready for the next one; the caller must not present `out`.
    pub fn decode(
        &mut self,
        frame: &CompressedFrame,
        out: &mut DecodedFrame,
    ) -> Result<(), DecodeError> {
        // First, and before anything is allocated or parsed: the claimed size. `frame.width`
        // and `frame.height` came from this frame's own SOF header, which is untrusted device
        // data (A6), and they are the only input to the allocation below.
        too_large(frame.width, frame.height)?;

        let mut dec = JpegDecoder::new_with_options(ZCursor::new(&frame.jpeg), self.options);
        dec.decode_headers()
            .map_err(|e| DecodeError::Jpeg(e.to_string()))?;

        let (dw, dh) = dec.dimensions().ok_or(DecodeError::NoDimensions)?;
        let (dw, dh) = (dw as u32, dh as u32);
        // zune's own `max_width`/`max_height` should already have refused this; check anyway,
        // because this is the value the output buffer is sized from.
        too_large(dw, dh)?;
        if dw != frame.width || dh != frame.height {
            return Err(DecodeError::DimensionMismatch {
                header_w: frame.width,
                header_h: frame.height,
                decoder_w: dw,
                decoder_h: dh,
            });
        }

        let expected = DecodedFrame::expected_len(dw, dh);
        // Belt to the ceiling's braces: whatever the two checks above did, the buffer is never
        // sized past `MAX_RGBA_BYTES`.
        if expected > MAX_RGBA_BYTES {
            return Err(DecodeError::DimensionsTooLarge {
                width: dw,
                height: dh,
                max_width: MAX_WIDTH,
                max_height: MAX_HEIGHT,
            });
        }
        let wanted = dec.output_buffer_size().ok_or(DecodeError::NoDimensions)?;
        if wanted != expected {
            return Err(DecodeError::UnexpectedBufferSize {
                width: dw,
                height: dh,
                wanted,
                expected,
            });
        }

        // Reuse: `resize` keeps the existing allocation whenever capacity allows, in both
        // directions. After the first frame at a given mode this is free.
        if out.rgba.len() != expected {
            out.rgba.resize(expected, 0);
        }
        dec.decode_into(&mut out.rgba)
            .map_err(|e| DecodeError::Jpeg(e.to_string()))?;

        out.width = dw;
        out.height = dh;
        out.captured_at = frame.captured_at;
        out.sequence = frame.sequence;
        Ok(())
    }
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

/// `Err(DimensionsTooLarge)` if `width`x`height` is over the sanity ceiling (§6, A6).
fn too_large(width: u32, height: u32) -> Result<(), DecodeError> {
    if super::dimensions_in_range(width, height) {
        Ok(())
    } else {
        Err(DecodeError::DimensionsTooLarge {
            width,
            height,
            max_width: MAX_WIDTH,
            max_height: MAX_HEIGHT,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::jpeg;
    use crate::capture::testsupport::{fixture_bytes, fixture_paths, patch_sof};
    use std::time::{Duration, Instant};

    /// Build a `CompressedFrame` the way [`super::super::v4l2::V4l2Source`] does: dimensions
    /// from this frame's own header (A6).
    fn frame_from(bytes: Vec<u8>, sequence: u32) -> CompressedFrame {
        let (w, h) = jpeg::dimensions(&bytes).expect("fixture header");
        CompressedFrame {
            jpeg: bytes,
            width: w,
            height: h,
            captured_at: Duration::from_millis(u64::from(sequence)),
            sequence,
        }
    }

    /// A fixture frame whose SOF has been rewritten to claim `width`x`height`.
    fn frame_claiming(width: u16, height: u16) -> CompressedFrame {
        let mut bytes = fixture_bytes(&fixture_paths("absrange")[0]);
        patch_sof(&mut bytes, height, width);
        let (w, h) = jpeg::dimensions(&bytes).expect("the patched header still parses");
        assert_eq!((w, h), (u32::from(width), u32::from(height)));
        CompressedFrame {
            jpeg: bytes,
            width: w,
            height: h,
            captured_at: Duration::ZERO,
            sequence: 0,
        }
    }

    #[test]
    fn options_are_the_a19_settings() {
        let d = Decoder::new();
        assert!(d.options().strict_mode(), "A19: strict mode must be on");
        assert_eq!(
            d.options().jpeg_get_out_colorspace(),
            ColorSpace::RGBA,
            "A19: RGBA out, repacking costs 34 %"
        );
    }

    #[test]
    fn options_carry_the_dimension_ceiling_not_zunes_16384_default() {
        let d = Decoder::new();
        assert_eq!(d.options().max_width(), MAX_WIDTH as usize);
        assert_eq!(d.options().max_height(), MAX_HEIGHT as usize);
        assert!(
            d.options().max_width() < 16384,
            "zune's default ceiling is 1 GiB of scratch from one corrupt SOF"
        );
    }

    /// Reviewer blocker 1. A single flipped byte in the SOF used to buy a 64 MiB allocation, a
    /// slow decode and a 4096x4096 frame delivered to the renderer with `decode_errors == 0`.
    /// It must now be refused **before** the scratch buffer is sized.
    #[test]
    fn a_sof_over_the_ceiling_is_refused_before_the_scratch_is_allocated() {
        let mut dec = Decoder::new();
        let mut out = DecodedFrame::empty();

        // Decode an honest frame first, so the scratch has a real 1080p allocation to protect.
        let good = frame_from(fixture_bytes(&fixture_paths("absrange")[0]), 0);
        dec.decode(&good, &mut out).expect("honest frame");
        let honest_len = out.rgba.len();
        let honest_cap = out.rgba.capacity();
        assert_eq!(honest_len, 1920 * 1080 * 4);

        let big = frame_claiming(4096, 4096);
        let r = dec.decode(&big, &mut out);
        assert!(
            matches!(
                r,
                Err(DecodeError::DimensionsTooLarge {
                    width: 4096,
                    height: 4096,
                    ..
                })
            ),
            "a 4096x4096 SOF decoded as {r:?}"
        );
        assert_eq!(
            out.rgba.capacity(),
            honest_cap,
            "the scratch grew for a frame that was refused"
        );
        assert!(
            out.rgba.capacity() <= MAX_RGBA_BYTES,
            "the scratch is over the ceiling: {}",
            out.rgba.capacity()
        );
        assert_eq!(out.rgba.len(), honest_len, "the scratch was resized");
        assert_eq!(
            (out.width, out.height),
            (1920, 1080),
            "the refused frame's dimensions leaked into the output"
        );

        // And the decoder is still usable: the next good frame decodes normally.
        dec.decode(&good, &mut out).expect("decoder still works");
        assert_eq!((out.width, out.height), (1920, 1080));
        assert_eq!(out.rgba.len(), 1920 * 1080 * 4);
        assert_eq!(out.rgba.capacity(), honest_cap, "no reallocation happened");
    }

    /// zune's own former ceiling. This is the 1 GiB / ~1 s case; it must now cost nothing.
    #[test]
    fn zunes_own_16384_ceiling_is_never_reached() {
        let mut dec = Decoder::new();
        let mut out = DecodedFrame::empty();
        let t = Instant::now();
        let r = dec.decode(&frame_claiming(16384, 16384), &mut out);
        assert!(
            matches!(r, Err(DecodeError::DimensionsTooLarge { .. })),
            "{r:?}"
        );
        assert_eq!(out.rgba.capacity(), 0, "something was allocated anyway");
        assert!(
            t.elapsed() < Duration::from_millis(100),
            "the refusal took {:?}; it should not have parsed anything",
            t.elapsed()
        );
    }

    /// The ceiling is a ceiling, not a format check: the largest advertised mode still decodes,
    /// and dimensions still come from the header rather than from `G_FMT` (A6).
    #[test]
    fn the_largest_advertised_mode_is_inside_the_ceiling() {
        assert!(
            super::super::dimensions_in_range(3840, 2160),
            "§6: 4K is an advertised mode"
        );
        assert!(super::super::dimensions_in_range(MAX_WIDTH, MAX_HEIGHT));
        assert!(!super::super::dimensions_in_range(
            MAX_WIDTH + 1,
            MAX_HEIGHT
        ));
        assert!(!super::super::dimensions_in_range(
            MAX_WIDTH,
            MAX_HEIGHT + 1
        ));

        let mut dec = Decoder::new();
        let mut out = DecodedFrame::empty();
        let paths: Vec<_> = fixture_paths("")
            .into_iter()
            .filter(|p| p.to_string_lossy().contains("mjpeg-3840x2160-01"))
            .collect();
        assert_eq!(paths.len(), 1, "the 4K fixture is missing");
        let f = frame_from(fixture_bytes(&paths[0]), 0);
        dec.decode(&f, &mut out).expect("4K decodes");
        assert_eq!((out.width, out.height), (3840, 2160));
        assert!(out.rgba.capacity() <= MAX_RGBA_BYTES);
    }

    /// A frame just one row over the ceiling is refused, so the boundary is where it says.
    #[test]
    fn one_row_over_the_ceiling_is_refused() {
        let mut dec = Decoder::new();
        let mut out = DecodedFrame::empty();
        let r = dec.decode(&frame_claiming(64, (MAX_HEIGHT + 1) as u16), &mut out);
        assert!(
            matches!(r, Err(DecodeError::DimensionsTooLarge { .. })),
            "{r:?}"
        );
        let r = dec.decode(&frame_claiming((MAX_WIDTH + 1) as u16, 64), &mut out);
        assert!(
            matches!(r, Err(DecodeError::DimensionsTooLarge { .. })),
            "{r:?}"
        );
    }

    #[test]
    fn every_fixture_decodes_to_1920x1080_rgba() {
        let mut dec = Decoder::new();
        let mut out = DecodedFrame::empty();
        let paths = fixture_paths("absrange");
        assert!(paths.len() >= 10);
        for (i, p) in paths.iter().enumerate() {
            let frame = frame_from(fixture_bytes(p), i as u32);
            dec.decode(&frame, &mut out)
                .unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            assert_eq!((out.width, out.height), (1920, 1080), "{}", p.display());
            assert_eq!(out.rgba.len(), 1920 * 1080 * 4, "{}", p.display());
            assert_eq!(out.sequence, i as u32);
            assert_eq!(out.captured_at, Duration::from_millis(i as u64));
        }
    }

    #[test]
    fn the_output_buffer_is_reused_across_frames() {
        let mut dec = Decoder::new();
        let mut out = DecodedFrame::empty();
        let paths = fixture_paths("absrange");
        let first = frame_from(fixture_bytes(&paths[0]), 0);
        dec.decode(&first, &mut out).expect("first decode");
        let ptr = out.rgba.as_ptr();
        let cap = out.rgba.capacity();
        for (i, p) in paths.iter().enumerate().take(4) {
            let f = frame_from(fixture_bytes(p), i as u32);
            dec.decode(&f, &mut out).expect("decode");
        }
        assert_eq!(ptr, out.rgba.as_ptr(), "buffer was reallocated");
        assert_eq!(cap, out.rgba.capacity());
    }

    #[test]
    fn a_truncated_frame_is_an_error_not_a_partial_image() {
        // A19: without strict mode this returns Ok with the bottom of the image left as
        // whatever was in the buffer, which is exactly the failure the KVM must not show.
        let mut dec = Decoder::new();
        let mut out = DecodedFrame::empty();
        let bytes = fixture_bytes(&fixture_paths("absrange")[0]);
        let cut = bytes.len() * 6 / 10;
        let frame = frame_from(bytes[..cut].to_vec(), 7);
        let r = dec.decode(&frame, &mut out);
        assert!(
            matches!(r, Err(DecodeError::Jpeg(_))),
            "truncated frame decoded as {r:?}"
        );
    }

    #[test]
    fn several_truncation_points_all_error() {
        let mut dec = Decoder::new();
        let mut out = DecodedFrame::empty();
        let bytes = fixture_bytes(&fixture_paths("absrange")[0]);
        for pct in [30usize, 50, 60, 80, 95, 99] {
            let cut = bytes.len() * pct / 100;
            let frame = frame_from(bytes[..cut].to_vec(), 0);
            assert!(
                dec.decode(&frame, &mut out).is_err(),
                "{pct}% of the frame decoded without error"
            );
        }
    }

    #[test]
    fn a_lying_header_dimension_is_refused() {
        let mut dec = Decoder::new();
        let mut out = DecodedFrame::empty();
        let mut frame = frame_from(fixture_bytes(&fixture_paths("absrange")[0]), 0);
        frame.width = 1280; // as if G_FMT had been trusted (A6)
        assert!(matches!(
            dec.decode(&frame, &mut out),
            Err(DecodeError::DimensionMismatch { .. })
        ));
    }

    #[test]
    fn garbage_is_an_error() {
        let mut dec = Decoder::new();
        let mut out = DecodedFrame::empty();
        let frame = CompressedFrame {
            jpeg: vec![0xFF; 512],
            width: 1920,
            height: 1080,
            captured_at: Duration::ZERO,
            sequence: 0,
        };
        assert!(dec.decode(&frame, &mut out).is_err());
    }

    /// Spot-check of A19's 2.40 ms median at 1080p. Ignored: it is a measurement, not a
    /// pass/fail property, and a loaded CI box would make any threshold a lie.
    ///
    /// `cargo test -p nanokvm --lib -- --ignored --nocapture decode_timing`
    #[test]
    #[ignore = "timing measurement, run explicitly"]
    fn decode_timing_spot_check() {
        let mut dec = Decoder::new();
        let mut out = DecodedFrame::empty();
        let frame = frame_from(fixture_bytes(&fixture_paths("absrange")[0]), 0);
        // Warm up: first decode grows the output buffer.
        dec.decode(&frame, &mut out).expect("warmup");
        let mut samples = Vec::with_capacity(100);
        for _ in 0..100 {
            let t = Instant::now();
            dec.decode(&frame, &mut out).expect("decode");
            samples.push(t.elapsed().as_secs_f64() * 1e3);
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        println!(
            "decode {}x{}: n=100 min {:.3} ms  median {:.3} ms  max {:.3} ms  (A19 median: 2.40 ms)",
            out.width,
            out.height,
            samples[0],
            samples[samples.len() / 2],
            samples[samples.len() - 1]
        );
    }
}
