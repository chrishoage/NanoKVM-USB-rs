//! Read-only video capability queries.
//!
//! Capture and metadata siblings have the same USB topology. Per-node device capabilities
//! are needed to distinguish them; the per-device capabilities field includes both.

use std::io;
use std::path::Path;

/// Query whether a video node supports capture. Probe errors remain visible in the
/// inventory and do not silently remove a node from consideration.
pub trait NodeProbe {
    /// `true` when `device_caps` has `VIDEO_CAPTURE` and `STREAMING` and not `META_CAPTURE`.
    fn is_capture_node(&self, dev: &Path) -> Result<bool, io::Error>;
}

/// Read-only `VIDIOC_QUERYCAP` adapter.
///
/// Opens with `O_RDONLY | O_NONBLOCK`, queries capabilities, then closes. Using a raw
/// descriptor avoids v4l's read/write open requirement for this read-only operation.
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

/// Probe for recorded sysfs trees; opens no device and returns `Unsupported`.
///
/// Recorded paths can name unrelated hardware on the test host. An inconclusive result
/// keeps candidate nodes visible without pretending to know which one carries video.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoProbe;

impl NodeProbe for NoProbe {
    fn is_capture_node(&self, _dev: &Path) -> Result<bool, io::Error> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "not probed: --sysfs-root names a recorded tree, whose /dev names belong to this \
             machine's own devices",
        ))
    }
}

/// One `VIDIOC_QUERYCAP` on an already-open descriptor.
///
/// This is the body of `v4l::Device::query_caps`, which is the only part of `v4l::Device` this
/// module wants. `v4l::capability::Capabilities` is built from `v4l2_capability.device_caps` —
/// the per-node field — despite the name of the struct member it lands in.
fn query_caps(fd: std::os::raw::c_int) -> io::Result<v4l::capability::Capabilities> {
    // SAFETY: VIDIOC_QUERYCAP writes one `v4l2_capability` through the pointer it is
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
