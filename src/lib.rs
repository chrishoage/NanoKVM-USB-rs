//! Native Linux client for the Sipeed NanoKVM-USB dongle.
//!
//! Design of record: `docs/NATIVE_CLIENT_PLAN.md` (rev 4). Evidence: `docs/STAGE0_FINDINGS.md`.
//! Module layout follows plan §4:
//!
//! - [`proto`]   — CH9329 framing, HID report builders, keymap. Pure, no I/O.
//! - [`link`]    — the `Link` trait: the one seam between the input writer and a transport.
//! - [`input`]   — event classes, coalescing, held-state projection, release-all cancellation (§2).
//! - [`serial`]  — port open, reader thread matching replies by command byte, `Link` impl.
//! - [`capture`] — V4L2 streaming, owned frames, JPEG-header dimensions, decode, handoffs (§5, §6).
//! - [`viewer`]  — winit window, render thread, Wayland shortcut inhibit, input capture.

pub mod capture;
pub mod input;
pub mod link;
pub mod proto;
pub mod serial;
pub mod viewer;
