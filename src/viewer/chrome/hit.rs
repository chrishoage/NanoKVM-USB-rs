//! Menu hit testing across physical pixels and egui points.
//!
//! Scale conversion must use the same factor as layout. Invalid scale factors fall back
//! to one to avoid division by zero or non-finite hit rectangles.

/// The chrome's rectangles for one frame, in egui points, with the scale that maps them to the
/// physical pixels winit reports.
///
/// Rebuilt every frame the UI is built. Empty means "no chrome on screen", which is the honest
/// state before the first UI build and the reason [`HitAreas::hits`] answers `false` then.
#[derive(Debug, Clone)]
pub struct HitAreas {
    rects: Vec<egui::Rect>,
    pixels_per_point: f32,
}

impl Default for HitAreas {
    fn default() -> Self {
        HitAreas {
            rects: Vec::new(),
            // Never 0: it is a divisor. 1.0 is the unscaled identity, which is also what a
            // hit test against an empty rectangle list means.
            pixels_per_point: 1.0,
        }
    }
}

impl HitAreas {
    /// An empty set at this scale. `pixels_per_point` comes from `egui::FullOutput`, which is the
    /// same number handed to `egui_wgpu`'s `ScreenDescriptor`, so the rectangle and the pixels it
    /// was drawn into cannot disagree.
    ///
    /// A non-finite or non-positive scale is replaced by 1.0 rather than trusted: it is a divisor,
    /// and a zero would put every point of the window inside every rectangle.
    pub fn new(pixels_per_point: f32) -> Self {
        HitAreas {
            rects: Vec::new(),
            pixels_per_point: if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
                pixels_per_point
            } else {
                1.0
            },
        }
    }

    /// Add one rectangle, in points.
    pub fn push(&mut self, rect: egui::Rect) {
        self.rects.push(rect);
    }

    /// The scale these rectangles are expressed against.
    pub fn pixels_per_point(&self) -> f32 {
        self.pixels_per_point
    }

    /// Whether the chrome currently covers anything at all.
    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    /// Whether `physical` — a winit `PhysicalPosition`, origin at the window's top-left — is
    /// inside any of the rectangles.
    ///
    /// The rectangle's far edge counts as inside (`egui::Rect::contains` is inclusive on both
    /// bounds). That is deliberate for a control the user is dragging: losing the grip because the
    /// pointer reached the last pixel of it would be a bug, and the cost of the extra row is that
    /// one pixel column of the target is not clickable while the pill sits over it — which is the
    /// same cost the pill itself already imposes.
    pub fn hits(&self, physical: (f64, f64)) -> bool {
        if self.rects.is_empty() {
            return false;
        }
        let p = egui::pos2(
            (physical.0 / f64::from(self.pixels_per_point)) as f32,
            (physical.1 / f64::from(self.pixels_per_point)) as f32,
        );
        self.rects.iter().any(|r| r.contains(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pill() -> egui::Rect {
        // A 200x34 pill at (12, 12) in points — the reference's 34 px bar.
        egui::Rect::from_min_size(egui::pos2(12.0, 12.0), egui::vec2(200.0, 34.0))
    }

    #[test]
    fn nothing_is_hit_before_the_first_ui_build() {
        let areas = HitAreas::new(1.0);
        assert!(areas.is_empty());
        assert!(!areas.hits((20.0, 20.0)));
    }

    /// At 1.0 the physical pixel and the point are the same number.
    #[test]
    fn hit_test_at_one_pixel_per_point() {
        let mut areas = HitAreas::new(1.0);
        areas.push(pill());
        assert!(areas.hits((12.0, 12.0)), "the top-left corner is inside");
        assert!(areas.hits((100.0, 30.0)));
        assert!(areas.hits((212.0, 46.0)), "the far corner is inside");
        assert!(
            !areas.hits((11.0, 30.0)),
            "one pixel to the left is outside"
        );
        assert!(!areas.hits((100.0, 47.0)), "one pixel below is outside");
    }

    /// At 1.5 the same pill occupies 1.5x as many physical pixels, and the hit test has to divide
    /// before comparing. A test that passed at 1.0 by accident fails here.
    #[test]
    fn hit_test_at_one_and_a_half_pixels_per_point() {
        let mut areas = HitAreas::new(1.5);
        areas.push(pill());
        // The pill covers physical x 18..=318, y 18..=69.
        assert!(areas.hits((18.0, 18.0)));
        assert!(areas.hits((318.0, 69.0)));
        assert!(
            !areas.hits((17.0, 30.0)),
            "just left of the scaled rectangle"
        );
        assert!(!areas.hits((330.0, 30.0)), "past the scaled right edge");
        // The point that would hit at 1.0 and must not at 1.5: physical (212, 46) is point
        // (141.3, 30.7), which is inside — so use one that is outside in points at 1.5.
        assert!(
            !areas.hits((10.0, 10.0)),
            "inside at no scale, and outside at this one"
        );
    }

    /// A degenerate scale cannot be allowed to make the whole window a hit: it is a divisor.
    #[test]
    fn a_degenerate_scale_falls_back_to_one() {
        for bad in [0.0, -1.5, f32::NAN, f32::INFINITY] {
            let areas = HitAreas::new(bad);
            assert_eq!(areas.pixels_per_point(), 1.0, "{bad}");
        }
    }

    /// Several rectangles: the pill plus the popover hanging below it. A pointer in either is the
    /// chrome's.
    #[test]
    fn any_rectangle_counts() {
        let mut areas = HitAreas::new(1.0);
        areas.push(pill());
        areas.push(egui::Rect::from_min_size(
            egui::pos2(12.0, 50.0),
            egui::vec2(180.0, 120.0),
        ));
        assert!(areas.hits((100.0, 20.0)), "in the pill");
        assert!(areas.hits((100.0, 120.0)), "in the popover");
        assert!(!areas.hits((100.0, 400.0)), "below both");
    }
}
