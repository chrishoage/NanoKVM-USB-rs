//! Absolute-coordinate mapping against the measured 4096-divisor device law.

use nanokvm::proto::report::{abs_coord, MouseAbsReport, ABS_DIVISOR, ABS_MAX};

/// The measured device law: what pixel a coordinate lands on.
fn device_pixel(v: u16, extent: u32) -> u32 {
    let effective = u32::from(v & 0x1FFF).min(u32::from(ABS_MAX));
    effective * extent / ABS_DIVISOR
}

/// Extents from recorded capture modes.
const EXTENTS: [u32; 12] = [
    640, 480, 720, 576, 1280, 720, 1920, 1080, 2560, 1440, 3840, 2160,
];

/// Every extent, plus 4096 itself, the largest extent the 12-bit field can address exactly.
const EXACT_EXTENTS: [u32; 13] = [
    640, 480, 720, 576, 1280, 720, 1920, 1080, 2560, 1440, 3840, 2160, 4096,
];

/// Every supported pixel maps back to itself under the measured device law.
#[test]
fn every_pixel_round_trips_through_the_device_law() {
    for extent in EXACT_EXTENTS {
        for px in 0..extent {
            let v = abs_coord(px, extent);
            assert_eq!(
                device_pixel(v, extent),
                px,
                "extent {extent}, pixel {px}, sent {v}"
            );
        }
    }
}

/// The finding that motivated the formula change, kept as a regression guard: the pixel-centre
/// mapping `((2*px + 1) * 2048) / extent` is one short for 1536 of 3840 columns and 96 of 2160
/// rows. If anyone reintroduces it, this test names the damage.
#[test]
fn the_pixel_centre_formula_is_not_an_exact_inverse_at_4k() {
    let centre = |px: u32, extent: u32| -> u16 {
        (((2 * u64::from(px) + 1) * 2048) / u64::from(extent)).min(4095) as u16
    };
    let short = |extent: u32| {
        (0..extent)
            .filter(|&px| device_pixel(centre(px, extent), extent) != px)
            .count()
    };
    assert_eq!(short(3840), 1536, "unreachable columns at 3840");
    assert_eq!(short(2160), 96, "unreachable rows at 2160");
    assert_eq!(short(1920), 0);
}

#[test]
fn the_mapping_is_monotonic_and_in_range() {
    for extent in EXTENTS {
        let mut previous = 0;
        for px in 0..extent {
            let v = abs_coord(px, extent);
            assert!(
                v <= ABS_MAX,
                "extent {extent}, pixel {px}: {v} exceeds full scale"
            );
            assert!(
                v >= previous,
                "extent {extent}, pixel {px}: {v} < {previous}"
            );
            previous = v;
        }
    }
}

/// Half scale maps to the center using divisor 4096.
#[test]
fn the_centre_pixel_lands_on_the_centre() {
    let v = abs_coord(960, 1920);
    assert_eq!(
        device_pixel(v, 1920),
        960,
        "the centre pixel must round-trip"
    );
    assert!(
        v.abs_diff(2048) <= 1,
        "sent {v}, expected the middle of the 4096-step field"
    );
    assert_eq!(device_pixel(2048, 1920), 960, "the measured law itself");
    assert_eq!(device_pixel(2048, 1080), 540);
}

/// Clamp in the encoder because larger values can wrap at the device.
#[test]
fn a_pixel_past_the_edge_clamps_to_the_last_pixel() {
    // 4094, not 4095: ceil(1919 * 4096 / 1920) = 4094 lands on the last column, 1919, and is the
    // smallest value that does. Full scale 4095 is only sent when the extent divides into 4096
    // favourably (3840 does; 1920 does not).
    assert_eq!(abs_coord(u32::MAX, 1920), abs_coord(1919, 1920));
    assert_eq!(abs_coord(u32::MAX, 1920), 4094);
    assert_eq!(
        device_pixel(abs_coord(u32::MAX, 1920), 1920),
        1919,
        "must land on the last column"
    );

    for extent in EXTENTS {
        let last = abs_coord(extent - 1, extent);
        assert_eq!(abs_coord(u32::MAX, extent), last, "extent {extent}");
        assert_eq!(abs_coord(extent, extent), last, "extent {extent}");
        assert!(last <= ABS_MAX);
    }
}

/// A zero extent is a degenerate window size, not a crash.
#[test]
fn a_zero_extent_is_the_origin() {
    assert_eq!(abs_coord(0, 0), 0);
    assert_eq!(abs_coord(u32::MAX, 0), 0);
}

/// Absolute reports must retain the mode byte and clamp coordinates.
#[test]
fn the_absolute_report_clamps_whatever_it_is_given() {
    for x in [0u16, 1, 2048, 4095, 4096, 8191, 8192, 32767, u16::MAX] {
        let p = MouseAbsReport {
            buttons: 0,
            x,
            y: x,
            wheel: 0,
        }
        .payload();
        assert_eq!(p.len(), 7);
        assert_eq!(p[0], MouseAbsReport::MODE);
        assert_eq!(u16::from_le_bytes([p[2], p[3]]), x.min(ABS_MAX), "x = {x}");
        assert_eq!(u16::from_le_bytes([p[4], p[5]]), x.min(ABS_MAX), "y = {x}");
    }
}
