//! The real capture source: V4L2 MJPEG, mmap streaming, owned frames (§6, §5.2).
//!
//! Negotiation is deliberately unforgiving. `S_FMT` is a best-effort ioctl — the driver may
//! return something other than what was asked for — so the returned format is checked and a
//! mismatch is a [`CaptureError::Config`], never a silent downgrade (§6). `S_PARM` is called
//! explicitly and its reply checked, because `S_FMT` alone leaves 1080p at the driver's own
//! default (A7); on a USB 3 link that default is 240 fps.
//!
//! Frame handling follows §5.2 exactly: copy the compressed frame out, return the buffer to the
//! driver, hand on the owned copy. A19 measured that copy at 0.002 ms against 2.40 ms of decode,
//! so the lifetime complexity of keeping mmap buffers checked out buys nothing.
//!
//! ## Four things about the `v4l` 0.14 API that are load-bearing
//!
//! 1. **`CaptureStream::next` requeues the previously dequeued buffer at the top of the *next*
//!    call**, not when the borrow ends. So one of the four buffers is held between calls. That
//!    is correct for us — the copy is already done and nothing references the mapping — but it
//!    means "requeue immediately" is honoured by the copy, not by the ioctl order.
//!
//! 2. **Any failing `next` corrupts the stream permanently, not just a timeout.** This corrects
//!    B2, which named `Stream::set_timeout` as the problem. The timeout is one instance of a
//!    general fault: `next` queues its private `arena_index` *first*, then dequeues, and only
//!    assigns `arena_index` **after** `VIDIOC_DQBUF` returns success. So on *any* error out of
//!    `dequeue` — the crate's own timeout, or `EIO`, which V4L2 documents for temporary problems
//!    like signal loss — `arena_index` is left naming a buffer the driver now owns. The next
//!    call requeues that index, the driver answers `EINVAL`, and every call after that fails
//!    forever. One `EIO` would kill capture permanently, which is what §6.1 S1-2 forbids.
//!
//!    Two defences, and both are needed. This module polls the fd itself before calling `next`,
//!    so a stall issues no ioctl at all and cannot reach the fault (that is B2's fix, and it
//!    covers the timeout case only). And any error out of `next` other than `ENODEV` marks the
//!    stream **broken**; the following [`FrameSource::next_frame`] drops the `MmapStream` —
//!    whose `Drop` issues `STREAMOFF`, which also clears vb2's sticky queue-error state — and
//!    rebuilds it with `with_buffers`, re-primes and `STREAMON`s, counting the rebuild in
//!    [`V4l2Source::stream_rebuilds`]. `ENODEV` is a disconnection and is never rebuilt: the
//!    node is gone and Stage 2 owns rediscovery (§6.1).
//!
//! 3. **`Handle::poll` throws `revents` away.** It returns only the fd count, so `POLLERR`,
//!    `POLLHUP` and `POLLNVAL` are indistinguishable from `POLLIN`. Those conditions are
//!    level-triggered and sticky: a real stream error (vb2 sets the queue error and `DQBUF`
//!    returns `EIO`; uvcvideo raises `POLLERR` on disconnect) would make every poll return
//!    instantly and the capture loop spin at 100 % CPU while reporting `Stalled`. This module
//!    therefore calls `libc::poll` on the raw fd itself and reads `revents`: `POLLNVAL` or
//!    `POLLHUP` is [`CaptureError::Disconnected`]; `POLLERR` without `POLLIN` is
//!    [`CaptureError::Io`] with `EIO` and **no** `next` call; only `POLLIN` proceeds.
//!
//!    A sticky `POLLERR` is vb2's level-triggered queue error and repeats until `STREAMOFF`,
//!    so `next_frame` also marks the stream broken on it: the next call performs the same
//!    rebuild as item 2, which is the `STREAMOFF` that clears the condition. Until the rebuild
//!    succeeds the pipeline counts the error, backs off, and reports the stall (§6.1 S1-2).
//!
//! 4. **The crate's `Drop` impls panic**, and they run on the capture thread. `Stream::drop`
//!    calls `STREAMOFF` and `Arena::drop` calls `munmap` plus `REQBUFS(0)`; both ignore `ENODEV`
//!    and `panic!` on every other error. There is nothing to fix here — the alternative is
//!    leaking mappings — but it means a rebuild, or simply dropping a [`V4l2Source`], can
//!    unwind. `capture::pipeline`'s capture thread runs its loop body under `catch_unwind` for
//!    exactly this reason and turns a panic into [`CaptureError::Disconnected`] rather than
//!    letting one take the process down (§6.1 S1-1).
//!
//! The priming in [`V4l2Source::open`] exists for reason 1: queue every buffer but index 0 and
//! `STREAMON` up front, so the crate's `active` flag is set and its first `next` call requeues
//! the one buffer we deliberately left out rather than re-queueing all of them. `REQBUFS` may
//! grant fewer buffers than asked for, so the count is read back rather than assumed.

use std::os::raw::c_int;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use v4l::buffer::{Flags, Type};
use v4l::framesize::FrameSizeEnum;
use v4l::io::traits::{CaptureStream, Stream as StreamTrait};
use v4l::memory::Memory;
use v4l::prelude::*;
use v4l::video::Capture;
use v4l::{Format, FourCC};

use super::{
    jpeg, CaptureError, CompressedFrame, FrameSource, SourceOpener, MAX_HEIGHT, MAX_WIDTH,
};

/// The pixel format. §6/A1: the device advertises exactly one, and there is no uncompressed
/// path at any resolution.
pub const FOURCC_MJPG: &[u8; 4] = b"MJPG";

/// Buffer count. The Stage 0 spike used four and measured every advertised mode at its
/// advertised rate with it (§6).
pub const BUFFER_COUNT: u32 = 4;

/// Default dequeue timeout. Long enough not to fire between frames at any advertised rate
/// (the slowest, 25 fps, is 40 ms) and short enough that a stall is noticed promptly (§6.1).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(250);

/// How many buffer indices [`granted_buffers`] probes before giving up. `REQBUFS` may grant more
/// than requested; nothing sane grants more than this.
const BUFFER_PROBE_LIMIT: u32 = 64;

/// `bytesused` statistics. §1.3 and A19 both carry "instrument `bytesused` in Stage 1" forward:
/// the decode margins were measured against an idle, low-entropy desktop and nobody has yet
/// measured what a *working* target produces.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BytesUsedStats {
    /// Number of frames observed.
    pub count: u64,
    /// Smallest `bytesused` seen, 0 if none.
    pub min: u32,
    /// Largest `bytesused` seen.
    pub max: u32,
    /// Sum of all `bytesused`, for the mean.
    pub total: u64,
    /// The most recent frame's `bytesused`.
    pub last: u32,
}

impl BytesUsedStats {
    /// Fold one frame's size in.
    pub fn record(&mut self, bytes: u32) {
        if self.count == 0 || bytes < self.min {
            self.min = bytes;
        }
        if bytes > self.max {
            self.max = bytes;
        }
        self.count += 1;
        self.total += u64::from(bytes);
        self.last = bytes;
    }

    /// Mean bytes per frame, or 0.0 before the first frame.
    pub fn mean(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.total as f64 / self.count as f64
        }
    }
}

impl std::fmt::Display for BytesUsedStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "n={} min={} max={} mean={:.0} last={}",
            self.count,
            self.min,
            self.max,
            self.mean(),
            self.last
        )
    }
}

/// A live V4L2 MJPEG capture device (§6).
///
/// Streaming starts in [`V4l2Source::open`] and stops when this is dropped.
///
/// **Dropping this can panic.** The `v4l` crate's `Stream` and `Arena` destructors `panic!` on
/// any `STREAMOFF`/`munmap`/`REQBUFS` failure other than `ENODEV` — see module docs item 4 — and
/// they run wherever this value is dropped, which in production is the capture thread.
pub struct V4l2Source {
    path: PathBuf,
    // `Device` and the stream each hold an `Arc<Handle>`, so the fd outlives both; the stream's
    // lifetime parameter is unconstrained (its buffers are mmap'd, not borrowed from `dev`).
    // The device is kept by value rather than dropped because a rebuild needs it (module docs
    // item 2).
    dev: Device,
    /// `None` only between dropping a broken stream and building its replacement.
    stream: Option<MmapStream<'static>>,
    handle: Arc<v4l::device::Handle>,
    format: Format,
    fps: u32,
    timeout: Duration,
    bytes: BytesUsedStats,
    /// Flags from the most recent buffer, so a caller can confirm A6's `TIMESTAMP_MONOTONIC |
    /// TSTAMP_SRC_SOE` claim rather than assume it (§5.5).
    last_flags: Flags,
    frames: u64,
    /// Buffers `REQBUFS` actually granted, which may be fewer than [`BUFFER_COUNT`].
    buffers: u32,
    /// Set when `CaptureStream::next` failed. The next [`FrameSource::next_frame`] rebuilds the
    /// stream before touching it, because the crate has left `arena_index` naming a
    /// driver-owned buffer (module docs item 2).
    stream_broken: bool,
    stream_rebuilds: u64,
}

/// One dequeued buffer, already copied out of the mapping.
struct Dequeued {
    jpeg: Vec<u8>,
    captured_at: Duration,
    sequence: u32,
    flags: Flags,
    bytesused: u32,
    /// The driver claimed more `bytesused` than the mapping holds, so the copy is short.
    over_long: Option<usize>,
}

impl V4l2Source {
    /// Open `path`, negotiate MJPEG at `width`x`height`@`fps`, and start streaming.
    ///
    /// Both negotiations are verified:
    ///
    /// - `S_FMT` must return `MJPG` at exactly the requested size. A driver that substitutes a
    ///   different mode gets a [`CaptureError::Config`], not a shrug (§6).
    /// - `S_PARM` must return the requested interval. A7: without this call 1080p silently runs
    ///   at the driver default, so the interval before and after is logged either way.
    ///
    /// # Errors
    ///
    /// [`CaptureError::Disconnected`] if the node is not there, [`CaptureError::Config`] if the
    /// driver will not give us the requested mode, [`CaptureError::Io`] otherwise.
    pub fn open(path: &Path, width: u32, height: u32, fps: u32) -> Result<Self, CaptureError> {
        if fps == 0 {
            return Err(CaptureError::Config("fps must be non-zero".into()));
        }
        let dev = Device::with_path(path).map_err(map_io)?;

        let want = Format::new(width, height, FourCC::new(FOURCC_MJPG));
        let got = dev.set_format(&want).map_err(map_io)?;
        if got.fourcc != FourCC::new(FOURCC_MJPG) || got.width != width || got.height != height {
            return Err(CaptureError::Config(format!(
                "asked for MJPG {width}x{height}, driver returned {} {}x{} on {}",
                got.fourcc,
                got.width,
                got.height,
                path.display()
            )));
        }

        // A7: log what the driver was going to do on its own, then override it.
        let before = dev.params().map_err(map_io)?;
        log::info!(
            "{}: S_FMT gave MJPG {}x{} sizeimage={}; frame interval before S_PARM {}/{} ({:.3} fps)",
            path.display(),
            got.width,
            got.height,
            got.size,
            before.interval.numerator,
            before.interval.denominator,
            fps_of(before.interval.numerator, before.interval.denominator),
        );

        let after = dev
            .set_params(&v4l::video::capture::Parameters::with_fps(fps))
            .map_err(map_io)?;
        let (num, den) = (after.interval.numerator, after.interval.denominator);
        // The driver may express 60 fps as 1/60 or 2/120; compare the ratio, not the literals.
        if num == 0 || u64::from(den) != u64::from(fps) * u64::from(num) {
            return Err(CaptureError::Config(format!(
                "asked for {fps} fps, driver returned interval {num}/{den} ({:.3} fps) on {}",
                fps_of(num, den),
                path.display()
            )));
        }
        log::info!(
            "{}: frame interval after S_PARM {num}/{den} ({:.3} fps)",
            path.display(),
            fps_of(num, den)
        );

        let (stream, buffers) = start_stream(&dev, path)?;

        Ok(V4l2Source {
            path: path.to_path_buf(),
            handle: dev.handle(),
            dev,
            stream: Some(stream),
            format: got,
            fps,
            timeout: DEFAULT_TIMEOUT,
            bytes: BytesUsedStats::default(),
            last_flags: Flags::empty(),
            frames: 0,
            buffers,
            stream_broken: false,
            stream_rebuilds: 0,
        })
    }

    /// Change the dequeue timeout. Defaults to [`DEFAULT_TIMEOUT`].
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// `bytesused` statistics so far (§1.3: instrument this in Stage 1).
    pub fn bytes_stats(&self) -> BytesUsedStats {
        self.bytes
    }

    /// The format the driver actually gave us. Note this is **not** where frame dimensions come
    /// from — A6 says only the JPEG header of each frame is trustworthy — it is here for logs.
    pub fn negotiated_format(&self) -> Format {
        self.format
    }

    /// `G_FMT` **now**, on the live fd, rather than the copy taken at open time.
    ///
    /// This exists to answer §11 q13 ("does a target resolution change alter the negotiated UVC
    /// format, or does the device rescale internally?"). **Answered on 2026-09-11: it does not.**
    /// Through a 720p window on the target, `G_FMT` and the SOF dimensions both stayed
    /// `MJPG 1920x1080` — the device rescales internally (`docs/STAGE2_FINDINGS.md` §6 item 3).
    /// The call stays, because that answer is one unit on one link and is worth re-checking.
    /// A6 forbids taking frame dimensions from
    /// `G_FMT`, and that stands — this is not a source of dimensions, it is a *measurement of
    /// the driver's own opinion*, to be logged next to the SOF dimensions so the two can be seen
    /// to agree or disagree. `examples/capture-probe.rs` is the instrument that does it.
    ///
    /// # Errors
    ///
    /// Whatever the ioctl reports, classified by [`map_io`]: [`CaptureError::Disconnected`] once
    /// the node is gone.
    pub fn query_negotiated_format(&self) -> Result<Format, CaptureError> {
        self.dev.format().map_err(map_io)
    }

    /// `G_PARM`'s current frame interval as `(numerator, denominator)`, read now.
    ///
    /// The companion to [`V4l2Source::query_negotiated_format`] for the same q13 measurement:
    /// A7 says the driver keeps its own rate unless `S_PARM` is called, so a rate that has
    /// changed underneath us is worth seeing rather than assuming. Measured over a target reboot
    /// and a target mode change, it never moved off `1/60`.
    pub fn query_frame_interval(&self) -> Result<(u32, u32), CaptureError> {
        let p = self.dev.params().map_err(map_io)?;
        Ok((p.interval.numerator, p.interval.denominator))
    }

    /// Buffer flags from the most recent frame. Stage 0 verified every buffer reports
    /// `TIMESTAMP_MONOTONIC | TSTAMP_SRC_SOE` with the error bit clear (§5.5); this lets a
    /// hardware test assert that rather than take it on trust.
    pub fn last_flags(&self) -> Flags {
        self.last_flags
    }

    /// How many buffers `REQBUFS` granted. May be fewer than [`BUFFER_COUNT`].
    pub fn buffers(&self) -> u32 {
        self.buffers
    }

    /// How many times the stream has been torn down and rebuilt after a failed dequeue
    /// (module docs item 2). Zero on a healthy link; a rising count means the device is
    /// erroring out of `VIDIOC_DQBUF` and capture is recovering from it.
    pub fn stream_rebuilds(&self) -> u64 {
        self.stream_rebuilds
    }

    /// **Test hook.** Drop the stream and mark it broken, so the next [`FrameSource::next_frame`]
    /// takes the rebuild path. This is the only way to exercise that path against real hardware
    /// without inducing a genuine `EIO`, which needs an unplug.
    ///
    /// Dropping the stream here runs the crate's `Drop` impl on the calling thread, so this can
    /// panic for the reason in module docs item 4.
    pub fn force_rebuild_for_test(&mut self) {
        self.stream = None;
        self.stream_broken = true;
    }

    /// Tear the stream down and build a fresh one (module docs item 2).
    ///
    /// The old stream is dropped **first**: its `Drop` issues `STREAMOFF`, which is what clears
    /// vb2's sticky queue-error state, and `Arena::drop` unmaps the buffers and releases them
    /// with `REQBUFS(0)`. Only then is a new arena allocated, primed and started, exactly as
    /// [`V4l2Source::open`] does.
    ///
    /// On failure the source stays marked broken with no stream, so the following call tries
    /// again rather than dequeuing into a corrupt ring.
    fn rebuild_stream(&mut self) -> Result<(), CaptureError> {
        log::warn!(
            "{}: rebuilding the capture stream after a failed dequeue (rebuild {})",
            self.path.display(),
            self.stream_rebuilds + 1
        );
        self.stream = None;
        let (stream, buffers) = start_stream(&self.dev, &self.path)?;
        self.stream = Some(stream);
        self.buffers = buffers;
        self.stream_broken = false;
        self.stream_rebuilds += 1;
        log::info!(
            "{}: capture stream rebuilt with {buffers} buffers ({} rebuilds so far)",
            self.path.display(),
            self.stream_rebuilds
        );
        Ok(())
    }

    /// Dequeue one buffer and copy it out. The mapping is borrowed only inside this call.
    ///
    /// Every error here means `VIDIOC_QBUF`/`VIDIOC_DQBUF` failed, which is precisely the
    /// condition that leaves the crate's `arena_index` naming a driver-owned buffer; the caller
    /// marks the stream broken on all of them but `ENODEV`.
    fn dequeue(&mut self) -> Result<Dequeued, CaptureError> {
        let stream = match self.stream.as_mut() {
            Some(s) => s,
            // Only reachable if a rebuild failed and the caller dequeued anyway.
            None => {
                return Err(CaptureError::Io(std::io::Error::from_raw_os_error(
                    libc::EIO,
                )))
            }
        };
        let (buf, meta) = CaptureStream::next(stream).map_err(map_io)?;
        let used = meta.bytesused as usize;
        let copied = used.min(buf.len());
        // The one copy §5.2 asks for. 0.002 ms at 1080p (A19), against 2.40 ms of decode.
        let mut owned = Vec::with_capacity(copied);
        owned.extend_from_slice(&buf[..copied]);
        Ok(Dequeued {
            jpeg: owned,
            captured_at: timestamp_to_duration(meta.timestamp),
            sequence: meta.sequence,
            flags: meta.flags,
            bytesused: meta.bytesused,
            over_long: (used > buf.len()).then_some(buf.len()),
        })
    }

    /// Poll the fd for a ready buffer, restarting on `EINTR`.
    ///
    /// Returns `Ok(false)` on timeout. Polling here rather than inside the crate's `dequeue` is
    /// what makes a stall survivable — see module docs item 2.
    ///
    /// The deadline is computed with `checked_add`, because [`V4l2Source::set_timeout`] takes an
    /// arbitrary [`Duration`] and `Instant::now() + Duration::MAX` panics. An overflowing
    /// timeout is treated as "no deadline", which is the saturating reading of it.
    fn wait_readable(&self) -> Result<bool, CaptureError> {
        poll_readable(self.handle.fd(), self.timeout)
    }
}

/// Whether `deadline` has passed. `None` means no deadline, which never expires.
fn expired(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|d| Instant::now() >= d)
}

/// Wait for `fd` to have a readable buffer, for up to `timeout`.
///
/// `Ok(true)` means `POLLIN`; `Ok(false)` means the timeout expired with nothing ready.
/// `revents` is inspected rather than discarded, which is the whole point (module docs item 3):
///
/// - `POLLNVAL` or `POLLHUP` — the fd is closed or the device hung up: [`CaptureError::Disconnected`].
/// - `POLLERR` without `POLLIN` — a stream error with no buffer to collect: [`CaptureError::Io`]
///   with `EIO`, and the caller must **not** call `next`, which would only fail and corrupt the
///   ring.
/// - `POLLIN` — proceed, even if `POLLERR` is set alongside it: there is a buffer to take.
///
/// A free function rather than a method so a unit test can point it at a pipe.
fn poll_readable(fd: c_int, timeout: Duration) -> Result<bool, CaptureError> {
    let deadline = Instant::now().checked_add(timeout);
    loop {
        let remaining = match deadline {
            Some(d) => d.saturating_duration_since(Instant::now()),
            None => timeout,
        };
        // `poll` takes milliseconds as an i32 and reads a negative value as "block forever", so
        // clamp rather than let a huge timeout wrap into an infinite wait.
        let ms = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `poll` reads and writes exactly the one `pollfd` we pass, and `pfd` is a
        // live, correctly typed, exclusively borrowed local; the count of 1 matches the single
        // element. `fd` is owned by the `Arc<Handle>` this source holds (or, in tests, by the
        // caller) and stays open for the duration of the call. A closed or invalid fd is not
        // undefined behaviour for `poll`: it is reported as `POLLNVAL`, which is handled below.
        let ret = unsafe { libc::poll(&mut pfd, 1, ms) };
        if ret < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                if expired(deadline) {
                    return Ok(false);
                }
                continue;
            }
            return Err(map_io(e));
        }
        if ret == 0 {
            return Ok(false);
        }
        let revents = pfd.revents;
        if revents & (libc::POLLNVAL | libc::POLLHUP) != 0 {
            return Err(CaptureError::Disconnected);
        }
        if revents & libc::POLLIN != 0 {
            return Ok(true);
        }
        if revents & libc::POLLERR != 0 {
            // Sticky and level-triggered: returning `Ok(true)` here is what spins the capture
            // loop at 100 % CPU. Report it as the I/O error it is and issue no ioctl.
            return Err(CaptureError::Io(std::io::Error::from_raw_os_error(
                libc::EIO,
            )));
        }
        // Some other condition we did not ask for. Keep waiting rather than reporting a frame.
        if expired(deadline) {
            return Ok(false);
        }
    }
}

/// Allocate, prime and start a capture stream on `dev` (§6, module docs item 1).
///
/// Returns the stream and the number of buffers `REQBUFS` actually granted. Every buffer but
/// index 0 is queued and `STREAMON` is issued, which puts the crate's stream into the steady
/// state where `next` requeues the one buffer we hold and then dequeues.
///
/// # Errors
///
/// [`CaptureError::Config`] if the driver grants fewer than two buffers — one held by us and one
/// filling is the minimum that can stream at all — and [`CaptureError::Io`] or
/// [`CaptureError::Disconnected`] for the ioctls.
fn start_stream(dev: &Device, path: &Path) -> Result<(MmapStream<'static>, u32), CaptureError> {
    let mut stream: MmapStream<'static> =
        MmapStream::with_buffers(dev, Type::VideoCapture, BUFFER_COUNT).map_err(map_io)?;

    // `REQBUFS` is free to grant fewer (or more) buffers than asked for, and `MmapStream` does
    // not expose the count. Queueing an index the driver did not allocate is `EINVAL`, so read
    // the count back rather than priming `BUFFER_COUNT` of them and hoping.
    let granted = granted_buffers(dev.handle().fd())?;
    if granted < 2 {
        return Err(CaptureError::Config(format!(
            "REQBUFS granted {granted} buffer(s) on {}, asked for {BUFFER_COUNT};              streaming needs at least 2",
            path.display()
        )));
    }
    if granted != BUFFER_COUNT {
        log::warn!(
            "{}: REQBUFS granted {granted} buffers, asked for {BUFFER_COUNT}",
            path.display()
        );
    }

    for index in 1..granted as usize {
        CaptureStream::queue(&mut stream, index).map_err(map_io)?;
    }
    StreamTrait::start(&mut stream).map_err(map_io)?;
    Ok((stream, granted))
}

/// How many buffers the driver has allocated on `fd`, by `VIDIOC_QUERYBUF` on rising indices
/// until it answers `EINVAL`.
///
/// Read-only: `QUERYBUF` changes no driver state, so this is safe to run between `REQBUFS` and
/// the first `QBUF`.
fn granted_buffers(fd: c_int) -> Result<u32, CaptureError> {
    let mut count = 0u32;
    while count < BUFFER_PROBE_LIMIT {
        // SAFETY: `v4l2_buffer` is a plain C struct of integers and a union of integers, so an
        // all-zero value is a valid instance of it — that is how the `v4l` crate builds one
        // itself. `VIDIOC_QUERYBUF` is `_IOWR` over exactly this type, and the pointer is to a
        // live, exclusively borrowed local of that type, so the kernel reads and writes only
        // within it. `fd` is open for the duration of the call.
        let r = unsafe {
            let mut buf: v4l::v4l_sys::v4l2_buffer = std::mem::zeroed();
            buf.type_ = Type::VideoCapture as u32;
            buf.memory = Memory::Mmap as u32;
            buf.index = count;
            v4l::v4l2::ioctl(
                fd,
                v4l::v4l2::vidioc::VIDIOC_QUERYBUF,
                &mut buf as *mut _ as *mut std::os::raw::c_void,
            )
        };
        match r {
            Ok(()) => count += 1,
            // The index is past the last allocated buffer: that is the answer, not a failure.
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => break,
            Err(e) => return Err(map_io(e)),
        }
    }
    Ok(count)
}

impl std::fmt::Debug for V4l2Source {
    /// Summarises the node and negotiated mode; the mmap arena is not printable.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("V4l2Source")
            .field("path", &self.path)
            .field("fourcc", &self.format.fourcc.to_string())
            .field("width", &self.format.width)
            .field("height", &self.format.height)
            .field("fps", &self.fps)
            .field("timeout", &self.timeout)
            .field("buffers", &self.buffers)
            .field("frames", &self.frames)
            .field("bytes", &self.bytes)
            .field("stream_broken", &self.stream_broken)
            .field("stream_rebuilds", &self.stream_rebuilds)
            .finish()
    }
}

impl FrameSource for V4l2Source {
    /// Dequeue one frame, copy it out, and read its dimensions from its own JPEG header.
    ///
    /// Order matters and follows §5.2: poll, dequeue, copy into an owned `Vec`, release the
    /// mapping, *then* parse. The returned frame shares nothing with the driver's buffers, so
    /// §9.2 item 8 — "no pending frame references a requeued mmap buffer" — holds structurally
    /// rather than by discipline.
    ///
    /// # Errors
    ///
    /// [`CaptureError::Timeout`] when no buffer became ready: the capture stalled, which is
    /// **not** evidence that the signal is gone (§6.1, A5). [`CaptureError::Disconnected`] on
    /// `ENODEV` or a `POLLHUP`/`POLLNVAL` on the node. [`CaptureError::BadFrame`] when the frame
    /// carries no parseable start-of-frame header, or claims a size over the
    /// [`MAX_WIDTH`]x[`MAX_HEIGHT`] ceiling. [`CaptureError::Io`] for a stream error, after
    /// which the stream is marked broken and the *next* call rebuilds it (module docs item 2).
    /// All but `Disconnected` are survivable and the caller should keep polling.
    fn next_frame(&mut self) -> Result<CompressedFrame, CaptureError> {
        // A previous dequeue failed and left the crate's `arena_index` naming a driver-owned
        // buffer. Nothing can be dequeued until the stream is rebuilt (module docs item 2).
        if self.stream_broken {
            self.rebuild_stream()?;
        }

        match self.wait_readable() {
            Ok(true) => {}
            Ok(false) => return Err(CaptureError::Timeout(self.timeout)),
            Err(CaptureError::Io(e)) => {
                // A level-triggered `POLLERR` is vb2's sticky queue error: it repeats until
                // `STREAMOFF`, which is exactly what the rebuild path performs (module docs
                // item 3). Rebuilding here keeps S1-2's "survive and report" from degrading into
                // a permanent stall the moment the device hiccups.
                self.stream_broken = true;
                log::warn!(
                    "{}: poll reported a stream error ({e}); the stream will be rebuilt on the next frame",
                    self.path.display()
                );
                return Err(CaptureError::Io(e));
            }
            Err(e) => return Err(e),
        }

        let dq = match self.dequeue() {
            Ok(dq) => dq,
            Err(e) => {
                // `ENODEV` is a disconnection: the node is gone and there is nothing to rebuild
                // against. Everything else — `EIO` above all, which V4L2 documents for temporary
                // problems like signal loss — has corrupted the ring and must be rebuilt.
                if !matches!(e, CaptureError::Disconnected) {
                    self.stream_broken = true;
                    log::warn!(
                        "{}: dequeue failed ({e}); the stream will be rebuilt on the next frame",
                        self.path.display()
                    );
                }
                return Err(e);
            }
        };
        // The borrow of the mapping ended inside `dequeue`. Everything below is owned memory.

        self.bytes.record(dq.bytesused);
        self.last_flags = dq.flags;
        self.frames += 1;
        if self.frames == 1 {
            // Stage 0 verified these bits on this device; say so once rather than assume it
            // silently, and never fail on it — §6.1 forbids inventing signal state.
            log::info!(
                "{}: first buffer flags {:#010x} (expect TIMESTAMP_MONOTONIC|TSTAMP_SRC_SOE)",
                self.path.display(),
                u32::from(dq.flags)
            );
        }
        if dq.flags.contains(Flags::ERROR) {
            log::warn!(
                "{}: buffer {} carries the V4L2 ERROR flag",
                self.path.display(),
                dq.sequence
            );
        }

        let sequence = dq.sequence;
        if let Some(mapped) = dq.over_long {
            return Err(CaptureError::BadFrame(format!(
                "driver reported bytesused={} for a {mapped}-byte buffer",
                dq.bytesused
            )));
        }
        if dq.jpeg.is_empty() {
            return Err(CaptureError::BadFrame("zero-length frame".into()));
        }

        // A6: the frame's own header, every frame, never `G_FMT`.
        let (width, height) = jpeg::dimensions(&dq.jpeg).map_err(|e| {
            CaptureError::BadFrame(format!("sequence {sequence}, {} bytes: {e}", dq.bytesused))
        })?;
        // The ceiling. A6 keeps the header as the authority on the size; this only refuses a
        // size the device cannot produce, which is what a flipped SOF byte looks like.
        if !super::dimensions_in_range(width, height) {
            return Err(CaptureError::BadFrame(format!(
                "sequence {sequence}: header claims {width}x{height}, over the                  {MAX_WIDTH}x{MAX_HEIGHT} ceiling"
            )));
        }

        Ok(CompressedFrame {
            jpeg: dq.jpeg,
            width,
            height,
            captured_at: dq.captured_at,
            sequence,
        })
    }

    /// Stop and restart streaming on the same fd: the B2 rebuild, on demand (§6.1 "attempt
    /// restart").
    ///
    /// Identical to the rebuild a failed dequeue schedules — `STREAMOFF` (via the old stream's
    /// `Drop`), a fresh arena, re-prime, `STREAMON` — and counted in the same
    /// [`V4l2Source::stream_rebuilds`]. The pipeline calls this when no frame has arrived for
    /// `PipelineConfig::restart_after` and there is no error to explain it: vb2 can be sitting
    /// on a queue that will never complete another buffer, and only `STREAMOFF` clears that.
    ///
    /// **This can panic** for the reason in module docs item 4: dropping the old stream runs the
    /// crate's destructors on the calling thread. The capture thread's `catch_unwind` is what
    /// makes that survivable.
    fn restart(&mut self) -> Result<(), CaptureError> {
        self.stream_broken = true;
        self.rebuild_stream()
    }

    /// The mode `S_FMT` committed to at open time (§6: never accept a different mode silently).
    ///
    /// Deliberately the stored copy rather than a fresh `G_FMT`: this is on the frame path, and
    /// the live `G_FMT` is the *driver's* opinion, which the 2026-09-11 measurement showed can
    /// keep saying 1920x1080 while the device streams 640x480. What the pipeline's watchdog
    /// needs is what was asked for and acknowledged, which is exactly this.
    ///
    /// **Constant for the life of this source.** `self.format` is set once, by the `S_FMT` in
    /// [`V4l2Source::open`], and nothing else writes it: [`FrameSource::restart`] is
    /// `REQBUFS`/`QBUF`/`STREAMON` on the same fd with no `S_FMT` and no `S_PARM`, so a restart
    /// cannot renegotiate anything. The only way this value changes is a new `open()`, which is
    /// why the pipeline re-reads it on a reopen and nowhere else.
    fn negotiated_dimensions(&self) -> Option<(u32, u32)> {
        Some((self.format.width, self.format.height))
    }

    fn describe(&self) -> String {
        format!(
            "V4L2 {} {} {}x{} @{} fps, {} mmap buffers",
            self.path.display(),
            self.format.fourcc,
            self.format.width,
            self.format.height,
            self.fps,
            self.buffers
        )
    }
}

/// Opens [`V4l2Source`] on a fixed node, again and again (§6.1: "attempt rediscovery").
///
/// This is the path-based opener, and deliberately the dumb one: it knows a node, a mode and a
/// rate, and it re-runs exactly the negotiation [`V4l2Source::open`] performs. It does **not**
/// search for the device — §8's pairing rules are `discovery`'s job, and an opener that guessed
/// at a different node after a replug would be the "guessing" §8 forbids. A discovery-backed
/// opener wraps this one rather than replacing it.
///
/// Every failure mode of a device that is on its way back is reported rather than retried here:
/// the pipeline owns the backoff, so `open` returning `ENOENT`/`ENODEV`/`EBUSY` is the normal
/// answer while the driver probes, not an error worth special-casing.
pub struct V4l2Opener {
    path: PathBuf,
    width: u32,
    height: u32,
    fps: u32,
    timeout: Option<Duration>,
}

impl V4l2Opener {
    /// An opener for `path` at `width`x`height`@`fps`, the same arguments
    /// [`V4l2Source::open`] takes.
    pub fn new(path: impl Into<PathBuf>, width: u32, height: u32, fps: u32) -> Self {
        V4l2Opener {
            path: path.into(),
            width,
            height,
            fps,
            timeout: None,
        }
    }

    /// Apply [`V4l2Source::set_timeout`] to every source this opener produces. Without it each
    /// new source starts at [`DEFAULT_TIMEOUT`], which is what production wants and what a test
    /// that needs a shorter stall does not.
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = Some(timeout);
    }

    /// The node this opener will keep trying.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl SourceOpener for V4l2Opener {
    fn open(&mut self) -> Result<Box<dyn FrameSource>, CaptureError> {
        let mut source = V4l2Source::open(&self.path, self.width, self.height, self.fps)?;
        if let Some(t) = self.timeout {
            source.set_timeout(t);
        }
        Ok(Box::new(source))
    }

    /// Describes the *intent*, because this has to read sensibly with no device present — which
    /// is exactly when the pipeline is logging it.
    fn describe(&self) -> String {
        format!(
            "V4L2 {} MJPG {}x{} @{} fps",
            self.path.display(),
            self.width,
            self.height,
            self.fps
        )
    }

    /// Renegotiate at the next open (§12 Stage 4b). Nothing is committed here: `S_FMT` happens in
    /// [`V4l2Source::open`], where its reply is already checked against what was asked for and a
    /// mismatch is a [`CaptureError::Config`] rather than a silent downgrade (§6).
    fn set_format(&mut self, width: u32, height: u32, fps: u32) -> bool {
        self.width = width;
        self.height = height;
        self.fps = fps;
        true
    }
}

/// The MJPEG frame sizes the device at `path` enumerates (§12 Stage 4b).
///
/// `VIDIOC_ENUM_FRAMESIZES` for [`FOURCC_MJPG`] on a **second, non-streaming** handle, exactly as
/// [`query_format`] does and for the same reason: V4L2 allows many opens and one streaming owner,
/// this ioctl is read-only, and the pipeline may be mid-reopen. The handle is opened and closed
/// per call.
///
/// The chrome's resolution list is this, and not a list written down anywhere in this client (§12
/// Stage 4b: "resolutions *enumerated from the device*, not the reference's fixed list"). Sizes
/// past the [`MAX_WIDTH`]/[`MAX_HEIGHT`] sanity ceiling are dropped rather than offered: the
/// decoder would refuse a frame that big, so offering the mode would be offering a failure.
///
/// Stepwise and continuous ranges are reduced to their **maximum** only. The device advertises
/// discrete sizes (§6, measured), so this arm exists to be honest rather than to be used: a
/// continuous range has no natural list of entries and inventing one would put modes in the menu
/// that nothing measured.
///
/// # Errors
///
/// [`CaptureError::Disconnected`] when the node is gone, which is the answer during a replug.
pub fn enumerate_modes(path: &Path) -> Result<Vec<(u32, u32)>, CaptureError> {
    let dev = Device::with_path(path).map_err(map_io)?;
    let sizes = dev
        .enum_framesizes(FourCC::new(FOURCC_MJPG))
        .map_err(map_io)?;
    let mut out: Vec<(u32, u32)> = Vec::new();
    for frame in sizes {
        // `FrameSizeEnum::to_discrete` expands a stepwise range by its step, which for a 2x2 step
        // over 4K is hundreds of thousands of entries and a menu nobody can use. Taking the
        // maximum is the deliberate reduction the doc comment above describes.
        let (w, h) = match frame.size {
            FrameSizeEnum::Discrete(d) => (d.width, d.height),
            FrameSizeEnum::Stepwise(s) => (s.max_width, s.max_height),
        };
        if w == 0 || h == 0 || !super::dimensions_in_range(w, h) {
            continue;
        }
        if !out.contains(&(w, h)) {
            out.push((w, h));
        }
    }
    // Largest first: the reference lists them that way, and the mode a user reaches for after
    // 1080p is 720p rather than 640x480.
    out.sort_unstable_by_key(|&(w, h)| std::cmp::Reverse((w, h)));
    Ok(out)
}

/// `G_FMT` and `G_PARM` on a **second, non-streaming** handle to `path`.
///
/// The q13 instrument (`examples/capture-probe.rs`) has to report the negotiated format once a
/// second while the pipeline owns the streaming fd behind a `Box<dyn FrameSource>`, and while
/// that fd may be in the middle of being reopened. V4L2 allows many opens of a node and only
/// one streaming owner; both ioctls here are read-only, so this observes the driver without
/// touching the capture. The handle is opened and closed per call, so a probe that is killed
/// mid-query leaves nothing holding the node.
///
/// Returns the format and the frame interval as `(numerator, denominator)`.
///
/// # Errors
///
/// [`CaptureError::Disconnected`] when the node is gone — which is the answer during a replug,
/// and is why the probe prints `no device` rather than exiting.
pub fn query_format(path: &Path) -> Result<(Format, (u32, u32)), CaptureError> {
    let dev = Device::with_path(path).map_err(map_io)?;
    let format = dev.format().map_err(map_io)?;
    let params = dev.params().map_err(map_io)?;
    Ok((
        format,
        (params.interval.numerator, params.interval.denominator),
    ))
}

/// `v4l2_buffer.timestamp` to a `Duration` on `CLOCK_MONOTONIC` (§5.5).
///
/// Stage 0 verified the stamp is always `TIMESTAMP_MONOTONIC | TSTAMP_SRC_SOE`, so this is
/// directly comparable with a local `CLOCK_MONOTONIC` read. Negative components cannot occur on
/// a monotonic clock; they are clamped rather than allowed to wrap a `u64`.
fn timestamp_to_duration(ts: v4l::timestamp::Timestamp) -> Duration {
    let sec = ts.sec.max(0) as u64;
    let usec = ts.usec.clamp(0, 999_999) as u64;
    Duration::from_secs(sec) + Duration::from_micros(usec)
}

/// Frames per second from an interval fraction, 0.0 for a degenerate one.
fn fps_of(numerator: u32, denominator: u32) -> f64 {
    if numerator == 0 {
        0.0
    } else {
        f64::from(denominator) / f64::from(numerator)
    }
}

/// Classify an `io::Error` from an ioctl (§6.1: disconnection and stall are different things
/// and must be reported as themselves).
fn map_io(e: std::io::Error) -> CaptureError {
    match e.raw_os_error() {
        // The node went away: unplug, or the USB link dropped.
        Some(libc::ENODEV) | Some(libc::ENXIO) | Some(libc::ENOENT) => CaptureError::Disconnected,
        // A transfer error. Survivable; the capture thread counts it and keeps polling.
        Some(libc::EIO) => CaptureError::Io(e),
        _ if e.kind() == std::io::ErrorKind::TimedOut => CaptureError::Timeout(DEFAULT_TIMEOUT),
        _ => CaptureError::Io(e),
    }
}

/// Read `CLOCK_MONOTONIC` as a `Duration`, for comparing against a frame's `captured_at`.
///
/// This is the "capture-to-dequeue age" clock. §5.5 is emphatic that such a number is **not**
/// presentation latency, and that it carries a floor of about one frame period of USB transfer
/// time; label it accordingly wherever it is reported.
pub fn now_monotonic() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` writes a `struct timespec` through the pointer we give it, and
    // `ts` is a live, correctly typed, exclusively borrowed local. `CLOCK_MONOTONIC` is always
    // available on Linux, so the call cannot fail for any reason we could act on; a failure
    // would leave `ts` at its initialised zero value.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    Duration::new(
        ts.tv_sec.max(0) as u64,
        ts.tv_nsec.clamp(0, 999_999_999) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_stats_track_min_max_mean_and_last() {
        let mut s = BytesUsedStats::default();
        assert_eq!(s.mean(), 0.0);
        s.record(100);
        assert_eq!((s.count, s.min, s.max, s.last), (1, 100, 100, 100));
        s.record(300);
        s.record(200);
        assert_eq!((s.count, s.min, s.max, s.last), (3, 100, 300, 200));
        assert!((s.mean() - 200.0).abs() < 1e-9);
    }

    #[test]
    fn the_first_record_sets_min_rather_than_leaving_it_at_zero() {
        // A default-zero `min` that is never replaced is the classic bug in this shape.
        let mut s = BytesUsedStats::default();
        s.record(171_921);
        assert_eq!(s.min, 171_921);
    }

    #[test]
    fn a_timestamp_converts_to_a_monotonic_duration() {
        let ts = v4l::timestamp::Timestamp::new(12_345, 678_901);
        assert_eq!(
            timestamp_to_duration(ts),
            Duration::from_secs(12_345) + Duration::from_micros(678_901)
        );
    }

    #[test]
    fn a_negative_timestamp_clamps_rather_than_wrapping() {
        let ts = v4l::timestamp::Timestamp::new(-1, -1);
        assert_eq!(timestamp_to_duration(ts), Duration::ZERO);
    }

    #[test]
    fn interval_fractions_reduce_to_fps() {
        assert!((fps_of(1, 60) - 60.0).abs() < 1e-9);
        assert!((fps_of(2, 120) - 60.0).abs() < 1e-9);
        assert_eq!(fps_of(0, 60), 0.0);
    }

    #[test]
    fn enodev_is_disconnection_and_eio_is_not() {
        assert!(matches!(
            map_io(std::io::Error::from_raw_os_error(libc::ENODEV)),
            CaptureError::Disconnected
        ));
        assert!(matches!(
            map_io(std::io::Error::from_raw_os_error(libc::EIO)),
            CaptureError::Io(_)
        ));
        assert!(matches!(
            map_io(std::io::Error::from_raw_os_error(libc::EINVAL)),
            CaptureError::Io(_)
        ));
        assert!(matches!(
            map_io(std::io::Error::new(std::io::ErrorKind::TimedOut, "x")),
            CaptureError::Timeout(_)
        ));
    }

    #[test]
    fn the_monotonic_clock_advances() {
        let a = now_monotonic();
        let b = now_monotonic();
        assert!(b >= a);
        assert!(a > Duration::ZERO, "CLOCK_MONOTONIC read as zero");
    }

    /// A pipe that closes both ends on drop, so a poll test cannot leak fds.
    struct Pipe {
        read: c_int,
        write: c_int,
    }

    impl Pipe {
        fn new() -> Self {
            let mut fds = [0 as c_int; 2];
            // SAFETY: `pipe` writes exactly two `int`s through the pointer, and `fds` is a live,
            // correctly typed, exclusively borrowed local array of two of them.
            let r = unsafe { libc::pipe(fds.as_mut_ptr()) };
            assert_eq!(r, 0, "pipe(): {}", std::io::Error::last_os_error());
            Pipe {
                read: fds[0],
                write: fds[1],
            }
        }

        /// Close the write end, leaving the read end at EOF: `POLLHUP`.
        fn close_write(&mut self) {
            if self.write >= 0 {
                // SAFETY: `self.write` is an fd this `Pipe` owns and has not yet closed; it is
                // set to -1 immediately so it can never be closed twice.
                unsafe { libc::close(self.write) };
                self.write = -1;
            }
        }

        fn close_read(&mut self) {
            if self.read >= 0 {
                // SAFETY: as `close_write`, for the read end.
                unsafe { libc::close(self.read) };
                self.read = -1;
            }
        }

        /// What `poll` reports for the read end right now, without going through the classifier.
        fn read_revents(&self) -> i16 {
            let mut pfd = libc::pollfd {
                fd: self.read,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one live, correctly typed `pollfd`, and a count of 1 matching it.
            unsafe { libc::poll(&mut pfd, 1, 0) };
            pfd.revents
        }
    }

    impl Drop for Pipe {
        fn drop(&mut self) {
            self.close_read();
            self.close_write();
        }
    }

    /// `v4l::device::Handle::poll` discards `revents`, so the old `wait_readable` read a hung-up
    /// fd as "a buffer is ready". These conditions are level-triggered and sticky, so that spun
    /// the capture loop at 100 % CPU while the pipeline reported `Stalled`.
    #[test]
    fn a_hung_up_fd_is_disconnection_rather_than_a_ready_buffer() {
        let mut p = Pipe::new();
        p.close_write();
        let revents = p.read_revents();
        assert_ne!(
            revents & libc::POLLHUP,
            0,
            "the read end of a pipe with no writer should be POLLHUP, got {revents:#06x}"
        );
        let r = poll_readable(p.read, Duration::from_millis(50));
        assert!(
            matches!(r, Err(CaptureError::Disconnected)),
            "POLLHUP classified as {r:?}"
        );
    }

    /// The other half: a fd that never becomes readable must time out, once, having actually
    /// waited — not return "ready" and let the caller spin.
    #[test]
    fn a_never_ready_fd_times_out_after_actually_waiting() {
        let p = Pipe::new(); // write end still open, nothing written: never readable
        let timeout = Duration::from_millis(60);
        let t = Instant::now();
        let r = poll_readable(p.read, timeout);
        let elapsed = t.elapsed();
        assert!(matches!(r, Ok(false)), "expected a timeout, got {r:?}");
        assert!(
            elapsed >= timeout / 2,
            "poll returned after {elapsed:?}, so it did not wait"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "poll overshot: {elapsed:?}"
        );
    }

    /// A fd with data really is reported ready, so the classifier has not simply been made
    /// pessimistic. Also covers the `checked_add` path: `Duration::MAX` as a timeout must not
    /// panic computing a deadline (`Instant::now() + Duration::MAX` does).
    #[test]
    fn a_readable_fd_is_ready_even_with_an_overflowing_timeout() {
        let p = Pipe::new();
        let byte = [0u8; 1];
        // SAFETY: writing one byte from a live, correctly sized local buffer to an fd this
        // `Pipe` owns; the length matches the buffer.
        let n = unsafe { libc::write(p.write, byte.as_ptr() as *const libc::c_void, 1) };
        assert_eq!(n, 1, "write(): {}", std::io::Error::last_os_error());
        let t = Instant::now();
        assert!(matches!(poll_readable(p.read, Duration::MAX), Ok(true)));
        assert!(t.elapsed() < Duration::from_secs(1), "it blocked");
    }

    /// `Instant::checked_add` is what keeps an arbitrary `set_timeout` from panicking, and
    /// `expired(None)` is how "no deadline" is spelled.
    #[test]
    fn an_overflowing_timeout_yields_no_deadline_rather_than_a_panic() {
        assert!(Instant::now().checked_add(Duration::MAX).is_none());
        assert!(!expired(None));
        assert!(expired(Some(Instant::now() - Duration::from_millis(1))));
        assert!(!expired(Some(Instant::now() + Duration::from_secs(60))));
    }

    /// A closed fd is `POLLNVAL`, which must also read as disconnection rather than readiness.
    ///
    /// The fd number can be reused by another thread between the close and the poll, so the
    /// classification is only asserted when `POLLNVAL` really was reported.
    #[test]
    fn a_closed_fd_is_disconnection_when_it_reports_pollnval() {
        let mut p = Pipe::new();
        p.close_write();
        let fd = p.read;
        p.close_read();
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one live, correctly typed `pollfd`, count 1. A stale fd number is reported as
        // POLLNVAL by `poll`, not undefined behaviour.
        unsafe { libc::poll(&mut pfd, 1, 0) };
        if pfd.revents & libc::POLLNVAL != 0 {
            let r = poll_readable(fd, Duration::from_millis(10));
            assert!(
                matches!(r, Err(CaptureError::Disconnected)),
                "POLLNVAL classified as {r:?}"
            );
        } else {
            eprintln!("fd {fd} was reused before the poll; POLLNVAL not observed this run");
        }
    }

    #[test]
    fn opening_a_nonexistent_node_is_disconnection_not_a_panic() {
        let r = V4l2Source::open(Path::new("/dev/video-nanokvm-absent"), 1920, 1080, 60);
        assert!(matches!(r, Err(CaptureError::Disconnected)), "{r:?}");
    }

    /// The opener has to be usable, and honest, with no device present — that is the state it
    /// exists for. A `describe()` that needed a live source would print nothing exactly when the
    /// pipeline is logging "still trying".
    #[test]
    fn an_opener_on_an_absent_node_describes_itself_and_reports_disconnection() {
        let mut opener = V4l2Opener::new("/dev/video-nanokvm-absent", 1920, 1080, 60);
        let described = opener.describe();
        assert!(
            described.contains("/dev/video-nanokvm-absent") && described.contains("1920x1080"),
            "{described}"
        );
        assert_eq!(opener.path(), Path::new("/dev/video-nanokvm-absent"));
        // ENOENT on a node that is not there is `Disconnected`, which is what the pipeline's
        // reopen loop treats as "not back yet" and retries.
        let r = opener.open();
        assert!(
            matches!(r.as_ref().err(), Some(CaptureError::Disconnected)),
            "{:?}",
            r.err()
        );
        // And again: an opener is called over and over, so it must not be single-shot.
        assert!(matches!(
            opener.open().err(),
            Some(CaptureError::Disconnected)
        ));
    }

    /// The same classification through the read-only query path the probe uses.
    #[test]
    fn querying_an_absent_node_is_disconnection_not_a_panic() {
        let r = query_format(Path::new("/dev/video-nanokvm-absent"));
        assert!(matches!(r.as_ref().err(), Some(CaptureError::Disconnected)));
    }

    #[test]
    fn a_zero_frame_rate_is_refused_before_any_ioctl() {
        let r = V4l2Source::open(Path::new("/dev/video-nanokvm-absent"), 1920, 1080, 0);
        assert!(matches!(r, Err(CaptureError::Config(_))), "{r:?}");
    }
}
