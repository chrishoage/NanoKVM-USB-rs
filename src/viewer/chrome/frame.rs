//! What crosses from the event loop to the render thread, and how several of them become one
//! (plan §4.1, §5.4, §12 Stage 4b).
//!
//! # Why this is not a [`crate::capture::Slot`]
//!
//! The video handoff is drop-oldest by design: a frame is a complete picture, so keeping the
//! newest and discarding the rest is exactly right (§5.2). **A chrome frame is not.**
//! [`egui::TexturesDelta`] is a *delta*: its `set` entries are the font-atlas pages egui grew this
//! frame — the glyphs a tooltip needed the first time it rendered — and its `free` entries are
//! pages to release after painting. Drop one and the atlas is permanently missing a page, which
//! shows up much later as blank or wrong glyphs in an unrelated widget.
//!
//! So the channel is an `mpsc` and the render thread **drains** it, appending every delta in
//! arrival order with [`egui::TexturesDelta::append`] while keeping only the newest tessellated
//! primitives. Primitives *are* a complete picture and may be dropped; deltas are not and may not.
//!
//! # What does not cross
//!
//! `egui::Context` never crosses. It is `Send + Sync`, but the routing rule needs
//! `popover_open` synchronously on the event loop while deciding where the key in hand goes
//! (§5.4: input handling must not be coupled to render timing), so the whole UI build and
//! tessellation stay on the event loop and only plain data goes over the wire.

use std::sync::mpsc::{Receiver, Sender, TryRecvError};

/// One tessellated frame of chrome, ready for `egui_wgpu::Renderer`.
///
/// Sent only when the chrome changed or egui asked for a repaint — never once per video frame.
pub struct ChromeFrame {
    /// The tessellated output of `egui::Context::tessellate`.
    pub primitives: Vec<egui::ClippedPrimitive>,
    /// Textures to upload before painting and to free after it. **Never dropped**; see the module
    /// docs.
    pub textures_delta: egui::TexturesDelta,
    /// The scale the primitives were tessellated at, straight from `egui::FullOutput`. The render
    /// thread hands it to `ScreenDescriptor`, and [`super::hit::HitAreas`] converts against the
    /// same number, so the drawn pill and the hit-tested pill cannot disagree.
    pub pixels_per_point: f32,
    /// The surface size the primitives were laid out for, in physical pixels.
    pub size_in_pixels: [u32; 2],
}

impl std::fmt::Debug for ChromeFrame {
    /// Counts, not contents: a frame carries a few thousand vertices and a font atlas page.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChromeFrame")
            .field("primitives", &self.primitives.len())
            .field("textures_set", &self.textures_delta.set.len())
            .field("textures_free", &self.textures_delta.free.len())
            .field("pixels_per_point", &self.pixels_per_point)
            .field("size_in_pixels", &self.size_in_pixels)
            .finish()
    }
}

impl ChromeFrame {
    /// Fold a newer frame into this one: every texture delta is kept, in order, and the newer
    /// frame's primitives, scale and size replace this one's.
    ///
    /// This is the whole coalescing rule, as a method so that the invariant — *deltas accumulate,
    /// pictures replace* — is stated once and asserted once.
    pub fn absorb(&mut self, newer: ChromeFrame) {
        let ChromeFrame {
            primitives,
            textures_delta,
            pixels_per_point,
            size_in_pixels,
        } = newer;
        self.textures_delta.append(textures_delta);
        self.primitives = primitives;
        self.pixels_per_point = pixels_per_point;
        self.size_in_pixels = size_in_pixels;
    }
}

/// The event loop's end of the channel.
///
/// [`ChromeSender::send`] never blocks and never fails in a way the caller must handle: a channel
/// whose receiver is gone means the render thread has finished, which is a shutdown and not an
/// error.
#[derive(Debug, Clone)]
pub struct ChromeSender(Sender<ChromeFrame>);

impl ChromeSender {
    /// Post one frame. Returns `false` once the render thread has gone.
    pub fn send(&self, frame: ChromeFrame) -> bool {
        self.0.send(frame).is_ok()
    }
}

/// The render thread's end of the channel.
pub struct ChromeReceiver(Receiver<ChromeFrame>);

impl ChromeReceiver {
    /// Take everything queued and coalesce it into one frame, or `None` if nothing is queued.
    ///
    /// Never blocks. The render thread already waits on the video slot for [`FRAME_WAIT`]; a
    /// second blocking wait here would make the chrome's clock the video's.
    ///
    /// [`FRAME_WAIT`]: crate::viewer::render
    pub fn drain(&self) -> Option<ChromeFrame> {
        let mut out: Option<ChromeFrame> = None;
        loop {
            match self.0.try_recv() {
                Ok(frame) => match out.as_mut() {
                    Some(acc) => acc.absorb(frame),
                    None => out = Some(frame),
                },
                // Disconnected is not an error here: the event loop has stopped sending, and
                // whatever is already in the channel is still worth drawing.
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return out,
            }
        }
    }
}

/// A chrome channel: the sender for the event loop, the receiver for the render thread.
///
/// Unbounded. The bound that matters is the send rate, and the event loop only sends when the
/// chrome changed or egui asked for a repaint — at most one per `about_to_wait`, and none at all
/// while the chrome is idle.
pub fn channel() -> (ChromeSender, ChromeReceiver) {
    let (tx, rx) = std::sync::mpsc::channel();
    (ChromeSender(tx), ChromeReceiver(rx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::epaint::{ImageDelta, TextureId};
    use egui::{ColorImage, TextureOptions};

    /// A one-pixel image delta under a given texture id, standing in for a font-atlas page.
    fn delta(id: u64) -> (TextureId, ImageDelta) {
        (
            TextureId::Managed(id),
            ImageDelta::full(
                ColorImage::new([1, 1], vec![egui::Color32::WHITE]),
                TextureOptions::LINEAR,
            ),
        )
    }

    fn frame(set: &[u64], free: &[u64], primitives: usize, size: [u32; 2]) -> ChromeFrame {
        ChromeFrame {
            primitives: (0..primitives)
                .map(|_| egui::ClippedPrimitive {
                    clip_rect: egui::Rect::EVERYTHING,
                    primitive: egui::epaint::Primitive::Mesh(egui::epaint::Mesh::default()),
                })
                .collect(),
            textures_delta: egui::TexturesDelta {
                set: set.iter().copied().map(delta).collect(),
                free: free.iter().copied().map(TextureId::Managed).collect(),
            },
            pixels_per_point: 1.0,
            size_in_pixels: size,
        }
    }

    /// The invariant the whole module exists for: appending two frames keeps **both** `set` maps,
    /// and only the newest primitives survive. A drop-oldest slot would keep only the second `set`
    /// and lose a font-atlas page for good.
    #[test]
    fn absorbing_keeps_both_texture_deltas_and_only_the_newest_primitives() {
        let mut first = frame(&[1], &[7], 3, [800, 600]);
        let second = frame(&[2], &[8], 5, [1280, 720]);
        first.absorb(second);

        let ids: Vec<TextureId> = first.textures_delta.set.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids,
            vec![TextureId::Managed(1), TextureId::Managed(2)],
            "both set entries survive, oldest first"
        );
        assert_eq!(
            first.textures_delta.free,
            vec![TextureId::Managed(7), TextureId::Managed(8)],
            "both free lists survive"
        );
        assert_eq!(first.primitives.len(), 5, "the newest picture replaces");
        assert_eq!(first.size_in_pixels, [1280, 720], "and so does its size");
    }

    /// Draining an empty channel is `None`, not a block and not a panic.
    #[test]
    fn draining_nothing_yields_nothing() {
        let (_tx, rx) = channel();
        assert!(rx.drain().is_none());
    }

    /// Three frames posted between two render passes arrive as one, with all three deltas.
    #[test]
    fn drain_coalesces_everything_queued() {
        let (tx, rx) = channel();
        assert!(tx.send(frame(&[1], &[], 1, [10, 10])));
        assert!(tx.send(frame(&[2], &[], 2, [20, 20])));
        assert!(tx.send(frame(&[3], &[9], 3, [30, 30])));
        let merged = rx.drain().expect("three frames were posted");
        assert_eq!(merged.textures_delta.set.len(), 3);
        assert_eq!(merged.textures_delta.free, vec![TextureId::Managed(9)]);
        assert_eq!(merged.primitives.len(), 3);
        assert_eq!(merged.size_in_pixels, [30, 30]);
        assert!(rx.drain().is_none(), "the channel is empty afterwards");
    }

    /// A render thread that has gone makes `send` answer `false` rather than fail the caller.
    #[test]
    fn sending_after_the_receiver_is_gone_is_reported_not_fatal() {
        let (tx, rx) = channel();
        drop(rx);
        assert!(!tx.send(frame(&[1], &[], 1, [10, 10])));
    }

    /// A frame already in the channel when the sender is dropped is still drawn: a disconnected
    /// channel is a shutdown, not a reason to throw away work already done.
    #[test]
    fn a_frame_queued_before_the_sender_went_away_is_still_drained() {
        let (tx, rx) = channel();
        assert!(tx.send(frame(&[1], &[], 2, [10, 10])));
        drop(tx);
        let merged = rx.drain().expect("the queued frame");
        assert_eq!(merged.primitives.len(), 2);
    }
}
