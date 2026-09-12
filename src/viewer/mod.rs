//! Wayland viewer with independent input, capture, rendering, and audio.
//!
//! The event loop handles capture state and UI commands. Blocking presentation runs on
//! a render thread so an occluded or locked window cannot delay input release.

pub mod app;
pub mod chrome;
pub mod input_map;
pub mod render;
pub mod state;
pub mod title;
pub mod wayland;

pub use app::{run, PointerMode, ViewerConfig};

/// Viewer release key, consumed locally and shown in the UI. It remains available
/// independently of the compositor's configured escape binding.
pub const RELEASE_KEY: &str = "Pause";
