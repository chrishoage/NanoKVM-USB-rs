//! Translating winit events into [`crate::input::Event`]s (plan §2.5, §3.4, §10.2).
//!
//! Everything here is a pure function or a small accumulator. Nothing in this module touches the
//! window, the producer or the clock, so all of it is unit tested without a display.
//!
//! Two rules from the plan are enforced here rather than downstream:
//!
//! - **Host key repeat is discarded** (§2.5). The target's own operating system generates
//!   repetition from the held HID state; forwarding host repeats doubles it.
//! - **Physical keys, never text** (§10.2). Only [`winit::keyboard::PhysicalKey::Code`] is
//!   translated, through [`crate::proto::keymap::hid_key`]. Text injection needs a declared target
//!   layout and is not part of Stage 1.

use winit::event::{ElementState, MouseButton, MouseScrollDelta};
use winit::keyboard::{KeyCode, PhysicalKey};

use crate::input::Event;
use crate::proto::keymap::hid_key;
use crate::proto::report::{abs_coord, button};
use crate::proto::HidKey;
use crate::viewer::render::Rect;

/// Logical pixels of `PixelDelta` scrolling per wheel detent.
///
/// The HID wheel field counts detents, not pixels, so a pixel-precise device (a touchpad) has to
/// be quantised somewhere. 20 px is roughly one notch of a mouse wheel on this desktop and is a
/// round number chosen by feel, not by measurement — there is no hardware fact behind it.
pub const PIXELS_PER_DETENT: f64 = 20.0;

/// What a key event means to the viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// Forward this to the target.
    Forward { key: HidKey, down: bool },
    /// The release key was pressed: leave capture. Never forwarded (§12 Stage 1).
    Release,
    /// Deliberately swallowed: a host auto-repeat (§2.5), or the release key's own key-up.
    Swallowed,
    /// A physical key this client does not map. Counted and logged; the target never sees it.
    Unmapped,
}

/// Translate one `WindowEvent::KeyboardInput` key event.
///
/// Takes the three fields of `winit::event::KeyEvent` that matter rather than the event itself:
/// `KeyEvent` carries a platform-private field that cannot be constructed outside winit, so a
/// function taking the whole event could not be unit tested.
///
/// `Pause` is intercepted as the release binding and is never forwarded in either direction, so a
/// target cannot see half of it. Everything else goes through the keymap; a key with no HID usage
/// is [`KeyAction::Unmapped`] rather than an error, because a key that maps to nothing is simply
/// never sent.
pub fn map_key(physical_key: PhysicalKey, state: ElementState, repeat: bool) -> KeyAction {
    // §2.5: discard host repeat before anything else, including the release key — a held release
    // key must not re-trigger release once per repeat.
    if repeat {
        return KeyAction::Swallowed;
    }
    let PhysicalKey::Code(code) = physical_key else {
        return KeyAction::Unmapped;
    };
    if code == KeyCode::Pause {
        return match state {
            ElementState::Pressed => KeyAction::Release,
            ElementState::Released => KeyAction::Swallowed,
        };
    }
    match hid_key(code) {
        Some(key) => KeyAction::Forward {
            key,
            down: state.is_pressed(),
        },
        None => KeyAction::Unmapped,
    }
}

/// The HID usage of `Enter`, which engages capture from the keyboard when nothing is captured.
///
/// Taken from the keymap rather than written down, so it cannot drift from what is forwarded.
pub fn enter_key() -> Option<HidKey> {
    hid_key(KeyCode::Enter)
}

/// Map a winit mouse button to its CH9329 button bit (Appendix).
///
/// `Other(_)` is ignored: the report has exactly five bits and inventing a mapping for a sixth
/// button would send a press the user did not make on someone else's console.
pub fn map_button(b: MouseButton) -> Option<u8> {
    Some(match b {
        MouseButton::Left => button::LEFT,
        MouseButton::Right => button::RIGHT,
        MouseButton::Middle => button::MIDDLE,
        MouseButton::Back => button::BACK,
        MouseButton::Forward => button::FORWARD,
        MouseButton::Other(_) => return None,
    })
}

/// Turn a cursor position into an absolute coordinate pair for the target (§3.4).
///
/// `pos` is in physical pixels with the window's top-left as the origin, `rect` is the video
/// rectangle inside that window, and `frame` is the decoded frame's own dimensions — which come
/// from its JPEG header and can change under us (§6, A6).
///
/// Positions outside the rectangle are clamped to its edge rather than dropped: the pointer has
/// to be able to reach the last row and column of the target's screen, and a cursor a pixel past
/// the edge means the user is still pushing that way. A degenerate rectangle or frame yields
/// `None`, because there is no defensible coordinate to send and the device would happily place
/// the pointer somewhere arbitrary on a live console.
pub fn map_cursor(pos: (f64, f64), rect: Rect, frame: (u32, u32)) -> Option<(u16, u16)> {
    if rect.width <= 0.0 || rect.height <= 0.0 || frame.0 == 0 || frame.1 == 0 {
        return None;
    }
    let axis = |v: f64, origin: f32, extent: f32, pixels: u32| -> u16 {
        let t = ((v - f64::from(origin)) / f64::from(extent)).clamp(0.0, 1.0);
        // `floor` then clamp: t == 1.0 is the far edge, which is the last pixel, not one past it.
        let px = (t * f64::from(pixels)).floor().min(f64::from(pixels - 1)) as u32;
        abs_coord(px, pixels)
    };
    Some((
        axis(pos.0, rect.x, rect.width, frame.0),
        axis(pos.1, rect.y, rect.height, frame.1),
    ))
}

/// [`map_cursor`] with the video rectangle computed from the window size rather than supplied.
///
/// This is the whole of the event loop's cursor mapping, kept here as a pure function so it can be
/// asserted without a display. Splitting it this way is the point: the rectangle must be derived
/// from the window size the event loop *currently* holds, because that is the number a resize
/// changes and the one the event loop learns first. A rectangle carried across from the render
/// thread is up to a frame wait plus a present behind, and a cursor mapped through a stale
/// rectangle reaches the wrong pixel on a live console (§3.4).
pub fn map_cursor_in_window(
    pos: (f64, f64),
    window: (u32, u32),
    frame: (u32, u32),
) -> Option<(u16, u16)> {
    map_cursor(pos, crate::viewer::render::letterbox(window, frame)?, frame)
}

/// Accumulates fractional relative motion so slow movement is not rounded away (§2.2: relative
/// motion is an accumulating delta and must be summed, never replaced).
///
/// winit delivers `DeviceEvent::MouseMotion` deltas as `f64`. The report carries whole units, so
/// the fraction is kept here and folded into the next event rather than discarded — otherwise a
/// pointer moved slowly enough never moves at all.
#[derive(Debug, Clone, Copy, Default)]
pub struct RelAccumulator {
    remainder_x: f64,
    remainder_y: f64,
}

impl RelAccumulator {
    /// Fold one motion delta in and emit whole units, if any have accumulated.
    pub fn push(&mut self, delta: (f64, f64)) -> Option<Event> {
        self.remainder_x += delta.0;
        self.remainder_y += delta.1;
        let dx = self.remainder_x.trunc();
        let dy = self.remainder_y.trunc();
        self.remainder_x -= dx;
        self.remainder_y -= dy;
        if dx == 0.0 && dy == 0.0 {
            return None;
        }
        // Saturating casts: a delta beyond i32 is not a real mouse movement, and the writer
        // splits anything past the report range across reports anyway (§2.3).
        Some(Event::PointerRel {
            dx: dx as i32,
            dy: dy as i32,
        })
    }
}

/// Accumulates wheel scrolling into whole detents.
///
/// **Sign.** winit documents positive `MouseScrollDelta` values as meaning "the content being
/// scrolled should move right and down", i.e. the wheel turned away from the user. The HID wheel
/// field uses the same convention: `+1` is away from the user. So the sign is passed through
/// unchanged, and this comment is the verification — there is no negation anywhere below.
#[derive(Debug, Clone, Copy, Default)]
pub struct WheelAccumulator {
    pixels: f64,
}

impl WheelAccumulator {
    /// Fold one scroll event in and emit whole detents, if any have accumulated.
    pub fn push(&mut self, delta: MouseScrollDelta) -> Option<Event> {
        let detents = match delta {
            // A line-delta device already counts detents; round and take it.
            MouseScrollDelta::LineDelta(_, y) => f64::from(y).round(),
            MouseScrollDelta::PixelDelta(p) => {
                self.pixels += p.y;
                let d = (self.pixels / PIXELS_PER_DETENT).trunc();
                self.pixels -= d * PIXELS_PER_DETENT;
                d
            }
        };
        if detents == 0.0 {
            return None;
        }
        Some(Event::Wheel {
            delta: detents as i32,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::dpi::PhysicalPosition;
    use winit::event::ElementState;
    use winit::keyboard::{NativeKeyCode, PhysicalKey};

    fn key(code: KeyCode, state: ElementState, repeat: bool) -> KeyAction {
        map_key(PhysicalKey::Code(code), state, repeat)
    }

    fn rect(x: f32, y: f32, width: f32, height: f32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn host_repeat_is_discarded() {
        // §2.5: the target's OS repeats from held state; forwarding host repeats doubles it.
        assert_eq!(
            key(KeyCode::KeyA, ElementState::Pressed, true),
            KeyAction::Swallowed
        );
    }

    #[test]
    fn a_mapped_key_is_forwarded_both_ways() {
        let down = key(KeyCode::KeyA, ElementState::Pressed, false);
        let up = key(KeyCode::KeyA, ElementState::Released, false);
        let usage = hid_key(KeyCode::KeyA).expect("KeyA is mapped");
        assert_eq!(
            down,
            KeyAction::Forward {
                key: usage,
                down: true
            }
        );
        assert_eq!(
            up,
            KeyAction::Forward {
                key: usage,
                down: false
            }
        );
    }

    #[test]
    fn modifiers_arrive_as_modifier_bits() {
        let a = key(KeyCode::ShiftLeft, ElementState::Pressed, false);
        assert_eq!(
            a,
            KeyAction::Forward {
                key: HidKey::Modifier(crate::proto::report::modifier::LEFT_SHIFT),
                down: true,
            }
        );
    }

    #[test]
    fn pause_is_intercepted_and_never_forwarded() {
        // The release binding. Both halves are swallowed so the target cannot see half a keypress.
        assert_eq!(
            key(KeyCode::Pause, ElementState::Pressed, false),
            KeyAction::Release
        );
        assert_eq!(
            key(KeyCode::Pause, ElementState::Released, false),
            KeyAction::Swallowed
        );
        // And it is a key the keymap would otherwise happily forward, which is why interception
        // has to happen here.
        assert!(hid_key(KeyCode::Pause).is_some());
    }

    #[test]
    fn held_release_key_does_not_retrigger() {
        assert_eq!(
            key(KeyCode::Pause, ElementState::Pressed, true),
            KeyAction::Swallowed
        );
    }

    #[test]
    fn unmapped_keys_are_reported_not_sent() {
        assert_eq!(
            key(KeyCode::Fn, ElementState::Pressed, false),
            KeyAction::Unmapped
        );
        assert_eq!(
            map_key(
                PhysicalKey::Unidentified(NativeKeyCode::Unidentified),
                ElementState::Pressed,
                false
            ),
            KeyAction::Unmapped
        );
    }

    #[test]
    fn buttons_map_to_their_report_bits() {
        assert_eq!(map_button(MouseButton::Left), Some(button::LEFT));
        assert_eq!(map_button(MouseButton::Right), Some(button::RIGHT));
        assert_eq!(map_button(MouseButton::Middle), Some(button::MIDDLE));
        assert_eq!(map_button(MouseButton::Back), Some(button::BACK));
        assert_eq!(map_button(MouseButton::Forward), Some(button::FORWARD));
        assert_eq!(map_button(MouseButton::Other(9)), None);
        // The five bits are distinct, which is the property the report depends on.
        let bits = [
            button::LEFT,
            button::RIGHT,
            button::MIDDLE,
            button::BACK,
            button::FORWARD,
        ];
        let or = bits.iter().fold(0u8, |a, b| a | b);
        assert_eq!(or.count_ones(), 5);
    }

    #[test]
    fn wheel_sign_is_passed_through() {
        // winit: +y means the content moves down, i.e. the wheel turned away from the user.
        // HID: +1 is away from the user. Same direction, no negation.
        let mut w = WheelAccumulator::default();
        assert_eq!(
            w.push(MouseScrollDelta::LineDelta(0.0, 1.0)),
            Some(Event::Wheel { delta: 1 })
        );
        assert_eq!(
            w.push(MouseScrollDelta::LineDelta(0.0, -1.0)),
            Some(Event::Wheel { delta: -1 })
        );
        // Horizontal scrolling has no field in the report; it is dropped, not folded into y.
        assert_eq!(w.push(MouseScrollDelta::LineDelta(5.0, 0.0)), None);
    }

    #[test]
    fn pixel_scrolling_accumulates_into_detents() {
        let mut w = WheelAccumulator::default();
        let step = PhysicalPosition::new(0.0, PIXELS_PER_DETENT / 2.0);
        assert_eq!(w.push(MouseScrollDelta::PixelDelta(step)), None);
        assert_eq!(
            w.push(MouseScrollDelta::PixelDelta(step)),
            Some(Event::Wheel { delta: 1 })
        );
        // The remainder carries: two more halves make the next detent, nothing is lost.
        assert_eq!(w.push(MouseScrollDelta::PixelDelta(step)), None);
        assert_eq!(
            w.push(MouseScrollDelta::PixelDelta(step)),
            Some(Event::Wheel { delta: 1 })
        );
    }

    #[test]
    fn relative_motion_keeps_its_remainder() {
        let mut r = RelAccumulator::default();
        assert_eq!(r.push((0.4, 0.0)), None);
        assert_eq!(r.push((0.4, 0.0)), None);
        // 1.2 accumulated: one whole unit out, 0.2 kept.
        assert_eq!(r.push((0.4, 0.0)), Some(Event::PointerRel { dx: 1, dy: 0 }));
        assert_eq!(r.push((0.7, 0.0)), None);
        assert_eq!(r.push((0.2, 0.0)), Some(Event::PointerRel { dx: 1, dy: 0 }));
    }

    #[test]
    fn relative_motion_handles_both_axes_and_signs() {
        let mut r = RelAccumulator::default();
        assert_eq!(
            r.push((-3.5, 2.5)),
            Some(Event::PointerRel { dx: -3, dy: 2 })
        );
        // -0.5 and +0.5 remain; another -0.5/+0.5 completes a whole unit each way.
        assert_eq!(
            r.push((-0.5, 0.5)),
            Some(Event::PointerRel { dx: -1, dy: 1 })
        );
    }

    #[test]
    fn cursor_maps_the_top_left_corner_to_the_origin() {
        let r = rect(100.0, 50.0, 1920.0, 1080.0);
        assert_eq!(
            map_cursor((100.0, 50.0), r, (1920, 1080)),
            Some((abs_coord(0, 1920), abs_coord(0, 1080)))
        );
    }

    #[test]
    fn cursor_maps_the_far_edge_to_the_last_pixel() {
        let r = rect(0.0, 0.0, 1920.0, 1080.0);
        // Exactly on the far edge, and one pixel inside it, are both the last pixel.
        let last = Some((abs_coord(1919, 1920), abs_coord(1079, 1080)));
        assert_eq!(map_cursor((1920.0, 1080.0), r, (1920, 1080)), last);
        assert_eq!(map_cursor((1919.5, 1079.5), r, (1920, 1080)), last);
    }

    #[test]
    fn cursor_outside_the_rectangle_is_clamped_to_it() {
        let r = rect(100.0, 50.0, 400.0, 300.0);
        assert_eq!(
            map_cursor((-500.0, -500.0), r, (1920, 1080)),
            Some((abs_coord(0, 1920), abs_coord(0, 1080)))
        );
        assert_eq!(
            map_cursor((9999.0, 9999.0), r, (1920, 1080)),
            Some((abs_coord(1919, 1920), abs_coord(1079, 1080)))
        );
    }

    #[test]
    fn cursor_scales_through_a_letterboxed_rectangle() {
        // Video drawn at half size, offset. The centre of the rectangle is the centre pixel.
        let r = rect(200.0, 100.0, 960.0, 540.0);
        let (x, y) = map_cursor((200.0 + 480.0, 100.0 + 270.0), r, (1920, 1080)).expect("mapped");
        assert_eq!((x, y), (abs_coord(960, 1920), abs_coord(540, 1080)));
    }

    #[test]
    fn cursor_on_a_degenerate_rectangle_or_frame_is_none() {
        assert_eq!(
            map_cursor((0.0, 0.0), rect(0.0, 0.0, 0.0, 100.0), (1920, 1080)),
            None
        );
        assert_eq!(
            map_cursor((0.0, 0.0), rect(0.0, 0.0, 100.0, 0.0), (1920, 1080)),
            None
        );
        assert_eq!(
            map_cursor((0.0, 0.0), rect(0.0, 0.0, 100.0, 100.0), (0, 1080)),
            None
        );
        assert_eq!(
            map_cursor((0.0, 0.0), rect(0.0, 0.0, 100.0, 100.0), (1920, 0)),
            None
        );
    }

    #[test]
    fn enter_is_mapped_and_is_a_usage_not_a_modifier() {
        match enter_key() {
            Some(HidKey::Usage(u)) => assert_eq!(u, 0x28),
            other => panic!("Enter should be a HID usage, got {other:?}"),
        }
    }
}
