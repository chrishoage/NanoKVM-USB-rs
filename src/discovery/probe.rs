//! Telling a capture node from a metadata node (§8, §6).
//!
//! sysfs cannot answer this. The dongle's UVC function registers **two** `/dev/video*` nodes on
//! one USB device — `/dev/video4` and `/dev/video5` on this desk — and they are indistinguishable
//! from the USB device tree, which is the only thing [`super::sysfs`] can see. `index` does not
//! separate them either: it counts nodes per driver instance, so it is `0` and `1` for both a
//! two-capture-node webcam and a capture-plus-metadata pair.
//!
//! The distinction lives behind `VIDIOC_QUERYCAP`, in `device_caps` — the per-node field, not the
//! per-device `capabilities` field, which ORs every node's caps together and therefore claims
//! both `VIDEO_CAPTURE` and `META_CAPTURE` on *both* nodes. Measured on this desk:
//!
//! ```text
//! /dev/video4  capabilities 0x84a00001  device_caps 0x04200001  VIDEO_CAPTURE|STREAMING|EXT_PIX_FORMAT
//! /dev/video5  capabilities 0x84a00001  device_caps 0x04a00000  META_CAPTURE |STREAMING|EXT_PIX_FORMAT
//! ```
//!
//! `v4l::Capabilities::capabilities` is built from `v4l2_capability.device_caps`, so it is
//! already the per-node field despite the name.
//!
//! ## Why this is a trait, and why it is called as rarely as possible
//!
//! Answering it means **opening a device node**, which is a side effect on hardware that may not
//! be ours. The contract for this slice makes the same call for the serial link — a listing must
//! not open the CH9329 to read `GET_USB_STRING` — and the reasoning is identical here:
//! `/dev/video0`–`3` on this desk belong to the user's webcam. [`super::inventory`] therefore
//! probes a node only when the answer can change the outcome; see the module docs on
//! [`super::inventory`] for the rule.

use std::io;
use std::path::Path;

/// Whether a `/dev/video*` node can actually stream video.
///
/// Errors are not failures of discovery. `EACCES` (not in the `video` group) and `EBUSY` are both
/// ordinary, and §8's policy is to list what was found and let the user pass `--video`, never to
/// silently drop a node the user can see in `/dev`.
pub trait NodeProbe {
    /// `true` when `device_caps` has `VIDEO_CAPTURE` and `STREAMING` and not `META_CAPTURE`.
    fn is_capture_node(&self, dev: &Path) -> Result<bool, io::Error>;
}

/// The real `VIDIOC_QUERYCAP` probe.
///
/// Opens the node **`O_RDONLY | O_NONBLOCK`**, issues one ioctl and closes it. It never calls
/// `REQBUFS`, `S_FMT` or `STREAMON`, so it cannot disturb another process streaming from the same
/// node.
///
/// `v4l::Device::with_path` would be the obvious way to do this, but it hard-codes `O_RDWR`, and
/// `v4l::Device` cannot be built from a descriptor (`Handle::new` is private in v4l 0.14). The
/// write capability buys nothing here: `QUERYCAP` is an `_IOR` that every V4L2 driver answers on
/// a read-only handle, while `O_RDWR` turns a node the caller can only read — a `video` group
/// membership not yet in effect, an ACL that grants read — into an `EACCES` for a question that
/// could have been answered. That path degrades gracefully (the node stays eligible), so this is
/// a capability regained rather than a bug fixed, and it is what the slice contract asked for:
/// "opening `/dev/video4` read-only for QUERYCAP is allowed".
#[derive(Clone, Copy, Debug, Default)]
pub struct RealProbe;

impl NodeProbe for RealProbe {
    fn is_capture_node(&self, dev: &Path) -> Result<bool, io::Error> {
        use v4l::capability::Flags;

        let fd = v4l::v4l2::open(dev, libc::O_RDONLY | libc::O_NONBLOCK)?;
        let caps = query_caps(fd);
        // The descriptor is ours and nothing else holds it; a close error cannot change the
        // answer, and failing over one would turn a successful QUERYCAP into "unknown".
        let _ = v4l::v4l2::close(fd);

        let f = caps?.capabilities;
        Ok(f.contains(Flags::VIDEO_CAPTURE)
            && f.contains(Flags::STREAMING)
            && !f.contains(Flags::META_CAPTURE))
    }
}

/// One `VIDIOC_QUERYCAP` on an already-open descriptor.
///
/// This is the body of `v4l::Device::query_caps`, which is the only part of `v4l::Device` this
/// module wants. `v4l::capability::Capabilities` is built from `v4l2_capability.device_caps` —
/// the per-node field — despite the name of the struct member it lands in.
fn query_caps(fd: std::os::raw::c_int) -> io::Result<v4l::capability::Capabilities> {
    // SAFETY: VIDIOC_QUERYCAP writes exactly one `v4l2_capability` through the pointer it is
    // given and reads nothing else. `caps` is a live, aligned, zeroed value of that type, and
    // `fd` is an open V4L2 descriptor that outlives the call.
    unsafe {
        let mut caps: v4l::v4l_sys::v4l2_capability = std::mem::zeroed();
        v4l::v4l2::ioctl(
            fd,
            v4l::v4l2::vidioc::VIDIOC_QUERYCAP,
            &mut caps as *mut _ as *mut std::os::raw::c_void,
        )?;
        Ok(v4l::capability::Capabilities::from(caps))
    }
}
