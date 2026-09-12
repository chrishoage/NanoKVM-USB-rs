//! Floating viewer controls, input routing, settings, and clipboard paste.
//!
//! Widgets emit commands; the event loop applies them. Routing is independent of egui's
//! consumed flag so menu interaction cannot steal target input or leave keys held.

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
