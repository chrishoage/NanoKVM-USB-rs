//! The window, the renderer, Wayland shortcut inhibition, pointer capture and input
//! translation (plan §1.5, §4.1, §5.3–§5.5, §12 Stage 1).
//!
//! # Thread ownership
//!
//! §4.1's table, the row this module owns, plus the two Stage 0 added:
//!
//! ```text
//!   ┌──────────────────┐  Arc<Slot<DecodedFrame>>  ┌─────────────────────────────┐
//!   │ nanokvm-decode   │ ────────────────────────▶ │ nanokvm-render              │
//!   │ (capture module) │      one pending frame    │ owns instance/adapter/device│
//!   └──────────────────┘          (§5.2)           │ /queue/surface; blocks in   │
//!                                                  │ present() ~99% of a frame   │
//!   ┌──────────────────┐                           └──────────┬──────────────────┘
//!   │ nanokvm-wayland  │  inhibitor active/inactive           │ RenderShared
//!   │ second wl queue  │ ──────────────┐                      │ (resize in,
//!   └──────────────────┘               ▼                      ▼  view + stats out)
//!                          ┌────────────────────────────────────────┐
//!                          │ event loop thread (winit)              │
//!                          │ NEVER touches wgpu after setup (§5.4)  │
//!                          └───────────────┬────────────────────────┘
//!                                          │ Producer::submit (non-blocking, §2.9)
//!                                          ▼
//!                                    input writer thread
//! ```
//!
//! - **The render thread is not an optimisation.** Under the only vblank-paced mode niri offers,
//!   `present()` blocks for about 6.9 ms of every 6.94 ms frame; a renderer on the event loop
//!   would starve input roughly 99 % of the time (§5.4, measured).
//! - **The event loop owns all input and never blocks.** [`crate::input::Producer::submit`] is
//!   non-blocking, and [`crate::input::Producer::engage`] is retried rather than waited on.
//! - **The Wayland dispatch thread services a *second* `wl_event_queue`** on winit's own
//!   `wl_display`. Sharing winit's queue is what fails (§1.5).
//!
//! # Layout
//!
//! - [`input_map`] — pure winit-event to [`crate::input::Event`] translation. No I/O, unit tested.
//! - [`state`]     — the capture state machine as a pure `reduce`, so §2.6's trigger list is
//!   testable without a display.
//! - [`title`]     — the window title as a pure function of what is true, so §6.1's and §2.8's
//!   wording is asserted rather than eyeballed.
//! - [`render`]    — the render thread, the wgpu surface, and the letterbox geometry.
//! - [`wayland`]   — `zwp_keyboard_shortcuts_inhibit_v1` on a second event queue (§1.5).
//! - [`app`]       — the winit `ApplicationHandler` that wires them together.

pub mod app;
pub mod input_map;
pub mod render;
pub mod state;
pub mod title;
pub mod wayland;

pub use app::{run, PointerMode, ViewerConfig};

/// The key that releases capture, spelled as the user sees it.
///
/// §12 Stage 1: the compositor's own escape (`Mod+Escape` on niri) is configuration this client
/// does not control, so "the viewer must therefore also provide its own release binding and show
/// it on screen". `Pause` is that binding: it is intercepted in [`input_map::map_key`] and never
/// forwarded to the target.
pub const RELEASE_KEY: &str = "Pause";
