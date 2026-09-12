//! JPEG header parsing without decoding the image.
//!
//! The capture device can emit frames at an old resolution after format negotiation.
//! Reading the start-of-frame marker provides dimensions for the bytes received.

/// Why a JPEG header could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JpegHeaderError {
    /// The data does not begin with a start-of-image marker.
    #[error("no SOI marker at the start of the frame")]
    NotJpeg,
    /// The marker stream ran off the end of the buffer before a start-of-frame segment.
    #[error("truncated before the start-of-frame segment")]
    Truncated,
    /// A byte appeared where a marker was expected — the marker stream is not parseable.
    #[error("expected a marker at offset {offset}, found {found:#04x}")]
    NotAMarker { offset: usize, found: u8 },
    /// A length-prefixed segment declared a length below the two bytes of the length field.
    #[error("segment for marker {marker:#04x} declares an impossible length {length}")]
    BadSegmentLength { marker: u8, length: u16 },
    /// End-of-image or start-of-scan reached with no start-of-frame segment seen.
    #[error("no start-of-frame segment before {stopped_at:#04x}")]
    NoStartOfFrame { stopped_at: u8 },
    /// A start-of-frame segment declared a zero width or height.
    #[error("start-of-frame declares {width}x{height}")]
    ZeroDimension { width: u32, height: u32 },
}

const MARKER_SOI: u8 = 0xD8;
const MARKER_EOI: u8 = 0xD9;
const MARKER_SOS: u8 = 0xDA;
const MARKER_DHT: u8 = 0xC4;
const MARKER_JPG: u8 = 0xC8;
const MARKER_DAC: u8 = 0xCC;
const MARKER_TEM: u8 = 0x01;

/// Read `(width, height)` from the first JPEG start-of-frame marker.
///
/// Skips length-prefixed segments and standalone markers, accepting fill bytes under
/// ITU-T T.81 §B.1.1.3. The segment stores height before width.
///
/// # Errors
///
/// Returns [`JpegHeaderError`] for malformed, truncated, or SOF-less input. Parsing the
/// header alone does not establish that the image data is complete.
pub fn dimensions(jpeg: &[u8]) -> Result<(u32, u32), JpegHeaderError> {
    let mut at = 0usize;

    match read_marker(jpeg, &mut at)? {
        MARKER_SOI => {}
        _ => return Err(JpegHeaderError::NotJpeg),
    }

    loop {
        let marker = read_marker(jpeg, &mut at)?;
        match marker {
            // Standalone markers: no length, no payload.
            MARKER_SOI | MARKER_TEM | 0xD0..=0xD7 => continue,
            // Entropy-coded data begins here, and every SOF precedes every SOS.
            MARKER_SOS => return Err(JpegHeaderError::NoStartOfFrame { stopped_at: marker }),
            MARKER_EOI => return Err(JpegHeaderError::NoStartOfFrame { stopped_at: marker }),
            // In the 0xC0..=0xCF range but not frame headers.
            MARKER_DHT | MARKER_JPG | MARKER_DAC => {
                skip_segment(jpeg, &mut at, marker)?;
            }
            // Every remaining SOFn. Layout: Lf(2) P(1) Y(2) X(2) Nf(1)...
            0xC0..=0xCF => {
                let length = read_u16(jpeg, at).ok_or(JpegHeaderError::Truncated)?;
                if length < 8 {
                    return Err(JpegHeaderError::BadSegmentLength { marker, length });
                }
                let height = read_u16(jpeg, at + 3).ok_or(JpegHeaderError::Truncated)? as u32;
                let width = read_u16(jpeg, at + 5).ok_or(JpegHeaderError::Truncated)? as u32;
                if width == 0 || height == 0 {
                    // Zero height requires a later DNL marker, which this parser does not support.
                    return Err(JpegHeaderError::ZeroDimension { width, height });
                }
                return Ok((width, height));
            }
            _ => {
                skip_segment(jpeg, &mut at, marker)?;
            }
        }
    }
}

/// Read one marker code, consuming any run of `0xFF` fill bytes before it (T.81 §B.1.1.3).
/// `*at` is left just past the marker code.
fn read_marker(data: &[u8], at: &mut usize) -> Result<u8, JpegHeaderError> {
    let mut p = *at;
    match data.get(p) {
        None => return Err(JpegHeaderError::Truncated),
        Some(&0xFF) => {}
        Some(&found) => return Err(JpegHeaderError::NotAMarker { offset: p, found }),
    }
    while data.get(p) == Some(&0xFF) {
        p += 1;
    }
    match data.get(p) {
        None => Err(JpegHeaderError::Truncated),
        // 0xFF00 is a stuffed byte and only ever appears inside entropy-coded data, which this
        // walk never enters. Here it is garbage.
        Some(&0x00) => Err(JpegHeaderError::NotAMarker {
            offset: p,
            found: 0x00,
        }),
        Some(&m) => {
            *at = p + 1;
            Ok(m)
        }
    }
}

/// Skip a length-prefixed segment. `*at` points at the two-byte length on entry and just past
/// the segment on exit.
fn skip_segment(data: &[u8], at: &mut usize, marker: u8) -> Result<(), JpegHeaderError> {
    let length = read_u16(data, *at).ok_or(JpegHeaderError::Truncated)?;
    if length < 2 {
        return Err(JpegHeaderError::BadSegmentLength { marker, length });
    }
    let next = at
        .checked_add(length as usize)
        .ok_or(JpegHeaderError::Truncated)?;
    if next > data.len() {
        return Err(JpegHeaderError::Truncated);
    }
    *at = next;
    Ok(())
}

/// Big-endian `u16` at `off`, or `None` if either byte is past the end.
fn read_u16(data: &[u8], off: usize) -> Option<u16> {
    let hi = *data.get(off)?;
    let lo = *data.get(off.checked_add(1)?)?;
    Some(u16::from_be_bytes([hi, lo]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::testsupport::{fixture_bytes, fixture_paths};

    #[test]
    fn every_absrange_fixture_reports_1920x1080() {
        let paths = fixture_paths("absrange");
        assert!(paths.len() >= 10, "fixture corpus is missing: {paths:?}");
        for p in &paths {
            let bytes = fixture_bytes(p);
            let got = dimensions(&bytes).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            assert_eq!(got, (1920, 1080), "{}", p.display());
        }
    }

    #[test]
    fn the_720p_corpus_reports_1280x720() {
        // A second size proves the parser reads the segment rather than recognising one file.
        let paths: Vec<_> = fixture_paths("")
            .into_iter()
            .filter(|p| p.to_string_lossy().contains("mjpeg-1280x720"))
            .collect();
        assert!(!paths.is_empty(), "no 720p fixtures found");
        for p in &paths {
            assert_eq!(
                dimensions(&fixture_bytes(p)),
                Ok((1280, 720)),
                "{}",
                p.display()
            );
        }
    }

    #[test]
    fn a_frame_truncated_after_the_header_still_parses() {
        // Header dimensions remain readable when truncation occurs after the SOF marker.
        let bytes = fixture_bytes(&fixture_paths("absrange")[0]);
        let cut = bytes.len() * 6 / 10;
        assert_eq!(dimensions(&bytes[..cut]), Ok((1920, 1080)));
    }

    #[test]
    fn a_frame_truncated_inside_the_header_is_an_error_not_a_panic() {
        let bytes = fixture_bytes(&fixture_paths("absrange")[0]);
        // This device's frames carry two DQT segments and then SOF0 at offset 140, so any cut
        // before 147 (SOF0 + length + precision + height + width) is inside the header.
        for cut in [0usize, 1, 2, 3, 8, 40, 100, 141, 143, 146] {
            let r = dimensions(&bytes[..cut.min(bytes.len())]);
            assert!(r.is_err(), "cut={cut} unexpectedly parsed as {r:?}");
        }
    }

    #[test]
    fn garbage_is_an_error() {
        assert_eq!(dimensions(&[]), Err(JpegHeaderError::Truncated));
        assert!(matches!(
            dimensions(b"not a jpeg at all"),
            Err(JpegHeaderError::NotAMarker { .. })
        ));
        assert!(matches!(
            dimensions(&[0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00]),
            Err(JpegHeaderError::NotJpeg)
        ));
        // A well-formed marker stream that reaches EOI without a SOF.
        assert!(matches!(
            dimensions(&[0xFF, 0xD8, 0xFF, 0xD9]),
            Err(JpegHeaderError::NoStartOfFrame { stopped_at: 0xD9 })
        ));
        // SOS before SOF.
        assert!(matches!(
            dimensions(&[0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02]),
            Err(JpegHeaderError::NoStartOfFrame { stopped_at: 0xDA })
        ));
    }

    #[test]
    fn a_segment_length_below_two_is_rejected_rather_than_looping() {
        // A zero-length segment must fail rather than leave the cursor unchanged.
        assert!(matches!(
            dimensions(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x00, 0xFF, 0xD9]),
            Err(JpegHeaderError::BadSegmentLength { length: 0, .. })
        ));
        assert!(matches!(
            dimensions(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x01, 0xFF, 0xD9]),
            Err(JpegHeaderError::BadSegmentLength { length: 1, .. })
        ));
    }

    /// Minimal well-formed marker stream: SOI, one APP0-ish segment, SOF0 640x480.
    fn synthetic_header() -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8];
        v.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x04, 0xAA, 0xBB]); // APPn, 2 payload bytes
        v.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x03, 0x00]); // DQT, 1 payload byte
        v.extend_from_slice(&[
            0xFF, 0xC0, 0x00, 0x0B, 0x08, // SOF0, Lf=11, P=8
            0x01, 0xE0, // Y = 480
            0x02, 0x80, // X = 640
            0x01, 0x01, 0x11, 0x00,
        ]);
        v
    }

    #[test]
    fn synthetic_header_reads_height_then_width() {
        // The segment stores height first; getting this backwards is the classic bug and would
        // silently report 480x640.
        assert_eq!(dimensions(&synthetic_header()), Ok((640, 480)));
    }

    #[test]
    fn leading_fill_bytes_before_soi_are_skipped() {
        for pad in 1..8usize {
            let mut v = vec![0xFFu8; pad];
            v.extend_from_slice(&synthetic_header());
            assert_eq!(dimensions(&v), Ok((640, 480)), "pad={pad}");
        }
    }

    #[test]
    fn fill_bytes_between_markers_are_skipped() {
        let mut v = vec![0xFF, 0xD8];
        v.extend_from_slice(&[0xFF, 0xFF, 0xFF]); // fill run
        v.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x04, 0xAA, 0xBB]);
        v.extend_from_slice(&[0xFF, 0xFF]); // more fill
        v.extend_from_slice(&[
            0xFF, 0xC1, 0x00, 0x0B, 0x08, 0x01, 0xE0, 0x02, 0x80, 0x01, 0x01, 0x11, 0x00,
        ]);
        assert_eq!(dimensions(&v), Ok((640, 480)));
    }

    #[test]
    fn restart_and_tem_markers_carry_no_length() {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0x01, 0xFF, 0xD0, 0xFF, 0xD7];
        v.extend_from_slice(&[
            0xFF, 0xC2, 0x00, 0x0B, 0x08, 0x01, 0xE0, 0x02, 0x80, 0x01, 0x01, 0x11, 0x00,
        ]);
        assert_eq!(dimensions(&v), Ok((640, 480)));
    }

    #[test]
    fn dht_in_the_sof_range_is_skipped_not_read_as_a_frame_header() {
        // 0xC4 sits inside 0xC0..=0xCF; reading it as a SOF would report the Huffman table's
        // bytes as a resolution.
        let mut v = vec![0xFF, 0xD8];
        v.extend_from_slice(&[0xFF, 0xC4, 0x00, 0x09, 1, 2, 3, 4, 5, 6, 7]);
        v.extend_from_slice(&[
            0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x01, 0xE0, 0x02, 0x80, 0x01, 0x01, 0x11, 0x00,
        ]);
        assert_eq!(dimensions(&v), Ok((640, 480)));
    }

    #[test]
    fn zero_dimensions_are_refused() {
        let mut v = vec![0xFF, 0xD8];
        v.extend_from_slice(&[
            0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x00, 0x02, 0x80, 0x01, 0x01, 0x11, 0x00,
        ]);
        assert!(matches!(
            dimensions(&v),
            Err(JpegHeaderError::ZeroDimension { .. })
        ));
    }

    mod props {
        use super::dimensions;
        use proptest::collection::vec;
        use proptest::prelude::*;

        proptest! {
            /// Device bytes are untrusted; arbitrary input must terminate without panic.
            #[test]
            fn arbitrary_bytes_never_panic(data in vec(any::<u8>(), 0..4096)) {
                let _ = dimensions(&data);
            }

            /// Byte streams biased towards marker bytes, which is where the interesting paths
            /// are: a fuzzer over uniform bytes almost never generates a valid segment chain.
            #[test]
            fn marker_soup_never_panics(data in vec(
                prop_oneof![
                    Just(0xFFu8),
                    Just(0x00u8),
                    Just(0xD8u8),
                    Just(0xC0u8),
                    Just(0xC4u8),
                    Just(0xDAu8),
                    any::<u8>(),
                ], 0..512)) {
                let _ = dimensions(&data);
            }
        }
    }

    #[test]
    fn every_prefix_of_a_real_frame_is_handled() {
        // Exhaustive over the header region of a real capture, which is where truncation
        // lands when a USB transfer is cut short.
        let bytes = fixture_bytes(&fixture_paths("absrange")[0]);
        for cut in 0..bytes.len().min(2048) {
            let _ = dimensions(&bytes[..cut]);
        }
    }
}
