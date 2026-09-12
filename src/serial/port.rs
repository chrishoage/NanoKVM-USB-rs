//! Serial-port setup and bounded byte I/O.
//!
//! Enumeration belongs to discovery. This adapter opens only the selected path at the
//! bridge's configured baud rate.

use std::path::Path;
use std::time::Duration;

use crate::link::LinkError;
use crate::proto::BAUD;

/// Serial settings and independent read/write deadlines. Defaults are the measured
/// 57600 baud, 8N1, no flow control. Startup never reconfigures bridge baud.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenOptions {
    /// Line rate. Anything but [`BAUD`] produces silence on this device.
    pub baud: u32,
    /// Read timeout on the port itself. Short on purpose: it is the reader loop's heartbeat, the
    /// interval at which it notices silence, a stop request, and a hang-up. It is *not* a reply
    /// deadline — that is `transact`'s `timeout` argument.
    ///
    /// It applies to the reader's handle alone. `serialport` keeps one `timeout` per handle and
    /// uses it for both directions, so leaving the write half on this value would make a 25 ms
    /// stall in `write` tear a frame — and a torn frame is consumed as the head of the *next*
    /// command. [`OpenOptions::write_timeout`] is what the write half gets instead.
    pub read_timeout: Duration,
    /// Timeout per write call, applied after cloning the reader handle.
    ///
    /// This does not bound the complete frame or the subsequent `tcdrain` flush. A short
    /// timeout risks a torn frame, which can consume the next command as its remainder.
    pub write_timeout: Duration,
    /// Silence interval before expiring an incomplete frame. It must exceed inter-byte
    /// gaps in valid traffic; the default is 100 ms.
    pub silence_before_expire: Duration,
    /// Quiet interval after a transaction timeout. The next request waits at most twice
    /// this interval to avoid matching a late reply as fresh.
    pub quiet_after_timeout: Duration,
}

impl Default for OpenOptions {
    fn default() -> OpenOptions {
        OpenOptions {
            baud: BAUD,
            read_timeout: Duration::from_millis(25),
            write_timeout: Duration::from_secs(1),
            silence_before_expire: Duration::from_millis(100),
            quiet_after_timeout: Duration::from_millis(50),
        }
    }
}

/// Open `path` with the read timeout applied.
///
/// The caller clones the reader handle before applying the write timeout. Open failures
/// return [`LinkError::Down`] with the path because no usable transport exists.
pub(crate) fn open(
    path: &Path,
    opts: &OpenOptions,
) -> Result<Box<dyn serialport::SerialPort>, LinkError> {
    let name = path.to_string_lossy();
    serialport::new(name.as_ref(), opts.baud)
        .data_bits(serialport::DataBits::Eight)
        .parity(serialport::Parity::None)
        .stop_bits(serialport::StopBits::One)
        .flow_control(serialport::FlowControl::None)
        .timeout(opts.read_timeout)
        .open()
        .map_err(|e| LinkError::Down(format!("open {name} at {} baud: {e}", opts.baud)))
}
