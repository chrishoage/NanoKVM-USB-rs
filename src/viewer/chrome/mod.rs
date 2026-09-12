//! The on-screen chrome: a floating pill over the video, with popovers (plan §12 Stage 4b).
//!
//! # Where each piece runs
//!
//! ```text
//!   ┌──────────────────────────────────────────────┐
//!   │ event loop thread                            │
//!   │   egui::Context, egui_winit::State, ChromeUi │
//!   │   take_egui_input → run(build) → tessellate  │──ChromeFrame──┐
//!   │   route_key / route_pointer  (synchronous)   │   (mpsc)      │
//!   └──────────────────────────────────────────────┘               ▼
//!                                        ┌──────────────────────────────────────┐
//!                                        │ nanokvm-render                       │
//!                                        │   egui_wgpu::Renderer only:          │
//!                                        │   update_texture / update_buffers /  │
//!                                        │   render / free_texture              │
//!                                        └──────────────────────────────────────┘
//! ```
//!
//! **The UI build and the tessellation are on the event loop, not the render thread**, and that is
//! a §5.4 decision rather than a convenience. The routing rule needs `popover_open` *synchronously*
//! while deciding where the key in hand goes; moving the UI to the render thread would make that
//! predicate lag by a render frame and couple input routing to render timing, which is the exact
//! thing §5.4 exists to prevent. A ~34 px pill with a handful of widgets costs tens of
//! microseconds against the ~6.9 ms `present()` it sits beside, and the claim is falsifiable: the
//! `--stats-interval` line already reports event-loop handling p50/p99, so if the split were wrong
//! the number would move. It is reported with the chrome closed, open and with a popover open in
//! `docs/STAGE4_FINDINGS.md`.
//!
//! **`egui::Context` never crosses a thread.** Only [`ChromeFrame`] does, and only plain data is
//! in it.
//!
//! # Modules
//!
//! - [`route`]  — the pure rule: which of the target, the chrome or nowhere gets an event. Never
//!   reads `egui_winit::EventResponse::consumed`, and the module docs say why in three measured
//!   points.
//! - [`hit`]    — our own rectangle hit test, in physical pixels, with the points conversion.
//! - [`frame`]  — [`ChromeFrame`] and its coalescing. **Not** a drop-oldest [`crate::capture::Slot`]:
//!   a dropped `TexturesDelta` permanently corrupts the font atlas.
//! - [`config`] — `$XDG_CONFIG_HOME/nanokvm/config.toml`.
//! - [`audio`]  — the 4a snapshot the Audio popover is built against.
//! - [`shortcut`] — the two built-in chords, and the send path (and why it is not `STAGE3_FINDINGS`
//!   D1's).
//! - [`clipboard`] — the §12 Stage 4c clipboard source: `wl-clipboard-rs` over data-control, read
//!   on a short-lived thread. **Clipboard contents are never logged, printed or persisted.**
//! - [`paste`]  — the §12 Stage 4c mapper and the pure [`paste::PasteJob`] the event loop advances.
//! - [`ui`]     — the pill and the popovers.

pub mod audio;
pub mod clipboard;
pub mod config;
pub mod frame;
pub mod hit;
pub mod paste;
pub mod route;
pub mod shortcut;
pub mod ui;

pub use audio::{AudioState, ChromeAudio};
pub use config::{Config, ConfigError, MouseMode, PasteLayout, Store, WheelDirection};
pub use frame::{channel, ChromeFrame, ChromeReceiver, ChromeSender};
pub use hit::HitAreas;
pub use paste::{PasteJob, PasteOutcome, PasteProgress, Refusal};
pub use route::{
    resolve_paste_chord, route_key, route_pointer, Outstanding, PointerKind, RouteInputs, Sink,
};
pub use shortcut::Builtin;
pub use ui::{
    ChromeCommand, ChromeFacts, ChromeOutput, ChromeUi, PasteAvailability, PasteFacts, Popover,
    GRIP_COLLAPSED, GRIP_EXPANDED,
};
