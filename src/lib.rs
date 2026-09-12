//! Native Linux client for the Sipeed NanoKVM-USB.
//!
//! The protocol and script compilers are independent of device I/O. The viewer combines
//! serial input, MJPEG capture, audio playback, and a Wayland window. See
//! `docs/architecture.md` for subsystem ownership and failure handling.

pub mod audio;
pub mod capture;
pub mod cli;
pub mod discovery;
pub mod input;
pub mod link;
pub mod proto;
pub mod script;
pub mod serial;
pub mod viewer;
