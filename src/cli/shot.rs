//! `nanokvm shot` — one frame to a file (§12 Stage 3).
//!
//! Opens the video node and nothing else: a screenshot needs no serial link, and the serial node
//! is a separate USB device whose fd, once opened, keeps its tty index across a replug (C13).
//!
//! The two flags that look like fussiness are both measured behaviour. `--skip` exists because
//! the device emits up to eight frames at the *previous* resolution after an idle period (A6),
//! and `--any-size` because the size that matters is the one in the frame's own JPEG header,
//! never `G_FMT` — after a USB reset the driver reports the negotiated mode while the device
//! streams 640x480 (C14).
//!
//! # The policy is a pure function, and the counters are printed
//!
//! Choosing which frame to keep is [`pick_frame`], which sees a [`FrameSource`] and a
//! [`ShotPolicy`] and nothing else, so every rule below is tested against
//! [`SyntheticSource`](crate::capture::SyntheticSource) rather than against the dongle. What it
//! discarded on the way is carried out in [`Shot`] and printed on the success line: a shot that
//! threw away 8 stale frames and restarted the stream is a different event from one that took
//! the first frame it saw, and §3.4's rule — never claim an effect the code did not observe —
//! cuts both ways. The counters are the evidence that the policy did anything at all.
//!
//! # What is written
//!
//! A `.jpg`/`.jpeg` shot is **the device's own bytes, byte for byte** — not a re-encode — so it
//! is exactly the frame whose SOF header the size came from. A `.png` one is that frame decoded
//! by the pipeline's own [`Decoder`] (RGBA, strict mode: a truncated frame is an error, not a
//! half image) and re-encoded. Either way the file appears atomically, written to a `.tmp` in
//! the same directory and renamed, so a reader that is watching the path never sees half a
//! frame.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

use crate::capture::{jpeg, CaptureError, CompressedFrame, DecodedFrame, Decoder, FrameSource};
use crate::capture::{V4l2Source, MAX_HEIGHT, MAX_WIDTH};
use crate::cli::{install_signal_handlers, interrupted, out};

/// How many *consecutive* frames at a size other than the negotiated one buy one stream restart.
///
/// C14's watchdog in [`PipelineConfig`](crate::capture::PipelineConfig) fires on two gates,
/// `format_mismatch_frames` (12 consecutive frames, chosen to sit above A6's eight-frame benign
/// transient at any frame rate) and `format_mismatch_grace` (500 ms of patience with a device
/// that is settling). A single shot has no second gate to offer — `--timeout` is the only clock
/// here — so the frame count does both jobs and is set where 500 ms puts it at the 60 fps this
/// client negotiates: 30 frames. That is still comfortably above the eight-frame transient at
/// any rate, which is the part of the pipeline's reasoning that has to survive.
const RESTART_AFTER: u32 = 30;

/// How many mismatched frames in total end the attempt with an error naming both sizes.
///
/// Three times [`RESTART_AFTER`], ≈1.5 s at 60 fps. The restart is the whole remedy a shot can
/// afford — the pipeline's ladder of three restarts, one escalation reopen and then acceptance
/// (C14) is for a session that goes on running — so once one restart has had 60 further frames
/// to take effect, the honest answer is the error and `--any-size`, not a silently wrong-sized
/// screenshot.
const GIVE_UP_AFTER: u32 = 90;

/// How long to pause after a source error before asking again.
///
/// [`V4l2Source`] blocks up to its own dequeue timeout, so in production this never runs. It is
/// here for the source that fails *instantly*: without it a `--timeout 10` against a node that
/// is erroring out of every `DQBUF` is ten seconds of a hot loop.
const ERROR_PAUSE: Duration = Duration::from_millis(1);

/// Longest `--timeout` this accepts, in seconds.
///
/// An hour, and checked **before the node is opened**, for the same reason a macro's `wait` is
/// bounded at compile time: `Instant::now() + Duration::from_secs(u64::MAX)` overflows and panics,
/// and here it would do so with the video node already open. A screenshot that is worth waiting
/// more than an hour for is a stream, not a shot.
const MAX_TIMEOUT_SECS: u64 = 3600;

#[derive(clap::Args, Debug)]
pub struct ShotArgs {
    /// Where to write the JPEG. Defaults to a timestamped name in the working directory.
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,

    /// Frames to discard after streaming starts — the device can emit up to eight frames at the
    /// previous resolution after an idle period (A6).
    #[arg(long, value_name = "N", default_value_t = 8)]
    pub skip: u32,

    /// Accept a frame whose header size differs from the negotiated size instead of restarting
    /// the stream and failing.
    #[arg(long)]
    pub any_size: bool,

    /// Give up after this many seconds without an acceptable frame.
    #[arg(long, value_name = "SECS", default_value_t = 10)]
    pub timeout: u64,
}

/// What [`pick_frame`] will and will not accept.
#[derive(Debug, Clone)]
pub struct ShotPolicy {
    /// Frames to discard before looking at any of them (A6).
    pub skip: u32,
    /// Take the first well-formed frame whatever its header says, and never restart the stream.
    pub any_size: bool,
    /// Consecutive mismatched frames that buy one [`FrameSource::restart`]. See
    /// [`RESTART_AFTER`].
    pub restart_after: u32,
    /// Mismatched frames in total before giving up. See [`GIVE_UP_AFTER`].
    pub give_up_after: u32,
}

impl ShotPolicy {
    /// The policy a command line asks for. Only `skip` and `any_size` are the user's; the two
    /// mismatch bounds are C14's, and are constants because a screenshot that needs them tuned
    /// is a bug report, not a flag.
    pub fn from_args(args: &ShotArgs) -> ShotPolicy {
        ShotPolicy {
            skip: args.skip,
            any_size: args.any_size,
            restart_after: RESTART_AFTER,
            give_up_after: GIVE_UP_AFTER,
        }
    }
}

impl Default for ShotPolicy {
    fn default() -> Self {
        ShotPolicy {
            skip: 8,
            any_size: false,
            restart_after: RESTART_AFTER,
            give_up_after: GIVE_UP_AFTER,
        }
    }
}

/// The frame that was kept, and what was thrown away to get it.
pub struct Shot {
    /// The complete JPEG as the device sent it.
    pub jpeg: Vec<u8>,
    /// Width from *this frame's* start-of-frame header (§6, A6), never from `G_FMT`.
    pub width: u32,
    /// Height from this frame's start-of-frame header.
    pub height: u32,
    /// Every frame **delivered** before this one that was not a size mismatch: the `--skip`
    /// frames, the truncated ones, and the ones whose header could not be read. One counter
    /// because the success line reports one number; a mismatch is counted separately below
    /// because it is the only one that carries a remedy.
    pub skipped: u32,
    /// Frames the *source* refused before they got here ([`CaptureError::BadFrame`]).
    ///
    /// Separate from `skipped` because `--skip` is a budget of frames to *look at and throw away*
    /// (A6's stale ones), and a frame that never arrived is not one of them. Sharing a counter
    /// made three bad frames eat three of the eight skips, so the shot was a stale frame the
    /// policy had been told to discard.
    pub discarded: u32,
    /// Frames whose header size differed from the negotiated size.
    pub mismatched: u32,
    /// Whether [`FrameSource::restart`] was called (at most once, see [`ShotPolicy`]).
    pub restarted: bool,
}

impl std::fmt::Debug for Shot {
    /// Prints the byte count rather than the bytes; a 1080p frame is ~170 KB (§6).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shot")
            .field("jpeg_len", &self.jpeg.len())
            .field("width", &self.width)
            .field("height", &self.height)
            .field("skipped", &self.skipped)
            .field("discarded", &self.discarded)
            .field("mismatched", &self.mismatched)
            .field("restarted", &self.restarted)
            .finish()
    }
}

/// Why no frame was kept.
#[derive(Debug, thiserror::Error)]
pub enum ShotError {
    /// The deadline passed with nothing acceptable. Says how far it got, because "timed out" on
    /// its own cannot distinguish a node that sent nothing from one that sent 600 frames this
    /// policy refused.
    #[error(
        "no usable frame within {waited:?} ({skipped} frame(s) skipped, {discarded} refused by \
         the source, {mismatched} at another size)"
    )]
    Timeout {
        waited: Duration,
        skipped: u32,
        discarded: u32,
        mismatched: u32,
    },
    /// The node went away mid-shot. Fatal here: Stage 2's rediscovery belongs to a session that
    /// keeps running, and a screenshot has nothing to keep running for.
    #[error("the capture device disconnected after {skipped} frame(s)")]
    Disconnected { skipped: u32 },
    /// Frames kept arriving at a size the device never committed to (C14) and one restart did
    /// not fix it.
    #[error(
        "the device is streaming {saw_width}x{saw_height} but {want_width}x{want_height} was \
         negotiated; {mismatched} frames at the wrong size{}. The stream is stuck, not the \
         device (C14) — replug the dongle, or pass --any-size to keep the frame as it is",
        if *restarted { ", and restarting the stream did not fix it" } else { "" }
    )]
    WrongSize {
        want_width: u32,
        want_height: u32,
        saw_width: u32,
        saw_height: u32,
        mismatched: u32,
        restarted: bool,
    },
    /// The source refused its configuration. Not survivable by waiting: nothing about a
    /// `S_FMT`/`S_PARM` refusal changes while this loop spins.
    #[error("capture configuration: {0}")]
    Config(String),
    /// SIGINT arrived while waiting. Exit 1, with the node released.
    #[error("interrupted")]
    Interrupted,
}

/// Take frames until one is worth keeping, or until `deadline`.
///
/// The rules, in the order they are applied to each frame, and why each one is here:
///
/// 1. The first `policy.skip` frames go straight in the bin. A6: after an idle gap the device
///    emits **up to eight frames at the previous resolution**, with the new `sizeimage` and no
///    error flag, and a stream that has just been started is exactly that case.
/// 2. A frame that does not end in the end-of-image marker is truncated and is dropped. C12 saw
///    four of these at the signal transitions of a target reboot; strict decoding would refuse
///    them anyway, and a truncated JPEG written to a file is a corrupt screenshot that nothing
///    downstream can tell from a real one. See [`ends_in_eoi`].
/// 3. The dimensions come from *this frame's* SOF header (§6, A6). A header that will not parse
///    is the frame's problem, not the run's: drop it and count it.
/// 4. Unless `policy.any_size`, the header size is compared with
///    [`FrameSource::negotiated_dimensions`] — **not** as a source of dimensions, which A6
///    forbids, but as the other half of C14's comparison. A run of mismatches buys one
///    `restart()`, because `STREAMON` is what re-commits the format on a device whose commit was
///    lost; a source that negotiated nothing (`None`) leaves this inert, exactly as it leaves the
///    pipeline's watchdog inert.
///
/// A [`CaptureError::Timeout`] or [`CaptureError::Io`] from the source is retried until the
/// deadline — the deadline, not a retry count, is what bounds this — while
/// [`CaptureError::Disconnected`] and [`CaptureError::Config`] end it at once, since no amount of
/// asking again fixes either. A [`CaptureError::BadFrame`] is a frame the source itself refused;
/// it is counted as `discarded` and **does not spend the `--skip` budget**, which is a budget of
/// frames to look at (A6's stale ones) and not of frames that failed to arrive.
///
/// **A frame already in hand is examined before the deadline is consulted again.** The check used
/// to come first, so a good frame returned a millisecond past the deadline was thrown away
/// unlooked-at — having already cost the wait — and the real bound was the deadline plus the
/// source's dequeue timeout either way. So the order is: ask, judge what came back, and only then
/// let the clock end the attempt.
///
/// # Errors
///
/// Every [`ShotError`]; see its variants. Nothing here writes anything or touches the serial
/// side.
pub fn pick_frame(
    source: &mut dyn FrameSource,
    policy: &ShotPolicy,
    deadline: Instant,
) -> Result<Shot, ShotError> {
    let started = Instant::now();
    // Read once per source, as `negotiated_dimensions` documents, and not at all when the size
    // is not being checked.
    let negotiated = if policy.any_size {
        None
    } else {
        source.negotiated_dimensions()
    };

    let mut skipped = 0u32;
    let mut discarded = 0u32;
    let mut mismatched = 0u32;
    let mut consecutive = 0u32;
    let mut restarted = false;

    loop {
        if interrupted() {
            return Err(ShotError::Interrupted);
        }

        // The labelled block is the "this frame is no good, ask for another" path. It exists so
        // that the deadline check below runs after a frame has been looked at, never instead of
        // looking at one.
        'frame: {
            let frame = match source.next_frame() {
                Ok(frame) => frame,
                Err(CaptureError::Disconnected) => return Err(ShotError::Disconnected { skipped }),
                Err(CaptureError::Config(what)) => return Err(ShotError::Config(what)),
                Err(e) => {
                    if let CaptureError::BadFrame(what) = &e {
                        log::debug!("shot: the source refused a frame: {what}");
                        discarded = discarded.saturating_add(1);
                    }
                    pause(deadline);
                    break 'frame;
                }
            };

            if skipped < policy.skip {
                skipped += 1;
                break 'frame;
            }

            if !ends_in_eoi(&frame.jpeg) {
                log::debug!(
                    "shot: discarding a truncated frame of {} bytes (no end-of-image marker)",
                    frame.jpeg.len()
                );
                skipped = skipped.saturating_add(1);
                break 'frame;
            }

            // A6: the frame's own header, parsed here rather than taken from `frame.width`, so
            // this function's rule holds whatever a source chose to put in those fields.
            let (width, height) = match jpeg::dimensions(&frame.jpeg) {
                Ok(dims) => dims,
                Err(e) => {
                    log::debug!("shot: discarding a frame whose header will not parse: {e}");
                    skipped = skipped.saturating_add(1);
                    break 'frame;
                }
            };
            if !crate::capture::dimensions_in_range(width, height) {
                log::debug!(
                    "shot: discarding a frame claiming {width}x{height}, over the \
                     {MAX_WIDTH}x{MAX_HEIGHT} ceiling"
                );
                skipped = skipped.saturating_add(1);
                break 'frame;
            }

            if let Some((want_w, want_h)) = negotiated {
                if (width, height) != (want_w, want_h) {
                    mismatched = mismatched.saturating_add(1);
                    consecutive = consecutive.saturating_add(1);
                    if mismatched >= policy.give_up_after {
                        return Err(ShotError::WrongSize {
                            want_width: want_w,
                            want_height: want_h,
                            saw_width: width,
                            saw_height: height,
                            mismatched,
                            restarted,
                        });
                    }
                    if !restarted && consecutive >= policy.restart_after {
                        // C14: the stream is stuck, not the device, and a fresh `STREAMON` is what
                        // re-commits the format. Once only — a shot that has to do this twice has
                        // already answered the question.
                        log::warn!(
                            "shot: {consecutive} consecutive frames at {width}x{height} rather \
                             than the negotiated {want_w}x{want_h}; restarting the stream (C14)"
                        );
                        restarted = true;
                        consecutive = 0;
                        match source.restart() {
                            Ok(()) => {}
                            Err(CaptureError::Disconnected) => {
                                return Err(ShotError::Disconnected { skipped })
                            }
                            // The restart was the one remedy on offer and it has been spent; say
                            // so and keep looking, because the deadline still bounds this.
                            Err(e) => log::warn!("shot: the stream restart failed: {e}"),
                        }
                    }
                    break 'frame;
                }
            }

            return Ok(Shot {
                jpeg: frame.jpeg,
                width,
                height,
                skipped,
                discarded,
                mismatched,
                restarted,
            });
        }

        // Only now: the frame this loop had in hand has been judged (R8).
        let now = Instant::now();
        if now >= deadline {
            return Err(ShotError::Timeout {
                waited: now.saturating_duration_since(started),
                skipped,
                discarded,
                mismatched,
            });
        }
    }
}

/// Whether a frame carries the end-of-image marker `FF D9` at its end.
///
/// Trailing zero bytes are ignored before the check. Every one of the frames under
/// `fixtures/frames/` — the whole Stage 0 corpus, 640x480 to 3840x2160 — ends in `FF D9` with no
/// padding at all, so the tolerance costs nothing on this unit and covers only the driver that
/// rounds `bytesused` up to a boundary. Anything else is a truncated frame (C12).
pub fn ends_in_eoi(jpeg: &[u8]) -> bool {
    let end = jpeg
        .iter()
        .rposition(|&b| b != 0x00)
        .map_or(0, |last| last + 1);
    jpeg[..end].ends_with(&[0xFF, 0xD9])
}

/// Sleep briefly, never past `deadline`. See [`ERROR_PAUSE`].
fn pause(deadline: Instant) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let pause = ERROR_PAUSE.min(remaining);
    if !pause.is_zero() {
        std::thread::sleep(pause);
    }
}

/// Where the shot goes and in what format.
///
/// Parsed from the command line **before the video node is opened**, so a typo in the extension
/// costs nothing and opens nothing (§2.8 item 3 applied to this command: resolve the whole
/// request first, or do nothing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShotTarget {
    /// `-`: the JPEG bytes on stdout, and the success line on stderr so the two do not mix.
    Stdout,
    /// `.jpg`/`.jpeg`: the device's bytes, untouched.
    Jpeg(PathBuf),
    /// `.png`: decoded to RGBA and re-encoded.
    Png(PathBuf),
}

impl ShotTarget {
    /// What `[PATH]` asked for, or the default name.
    ///
    /// `now` is a parameter rather than a call to [`SystemTime::now`] so that the default name is
    /// a pure function of the clock and can be tested.
    ///
    /// # Errors
    ///
    /// An extension that is neither JPEG nor PNG. The message names both, because the alternative
    /// is a user who writes `shot screen.bmp`, sees a file appear, and finds a JPEG inside it.
    pub fn parse(path: Option<&Path>, now: SystemTime) -> Result<ShotTarget> {
        let Some(path) = path else {
            return Ok(ShotTarget::Jpeg(PathBuf::from(default_name(now))));
        };
        if path.as_os_str() == "-" {
            return Ok(ShotTarget::Stdout);
        }
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "jpg" | "jpeg" => Ok(ShotTarget::Jpeg(path.to_path_buf())),
            "png" => Ok(ShotTarget::Png(path.to_path_buf())),
            "" => bail!(
                "{} has no extension, so there is nothing to say which format to write. Use \
                 .jpg (the device's own bytes) or .png (decoded and re-encoded), or `-` for the \
                 JPEG on stdout.",
                path.display()
            ),
            other => bail!(
                "{} asks for a .{other} file, and the only formats this writes are .jpg (the \
                 device's own bytes) and .png (decoded and re-encoded). `-` writes the JPEG to \
                 stdout.",
                path.display()
            ),
        }
    }

    /// The name this target goes by on the success line.
    pub fn label(&self) -> String {
        match self {
            ShotTarget::Stdout => "stdout".to_string(),
            ShotTarget::Jpeg(p) | ShotTarget::Png(p) => p.display().to_string(),
        }
    }

    /// Write `shot`, returning the number of bytes written.
    ///
    /// A file appears atomically: the bytes go to a `.tmp` alongside it and are renamed over the
    /// target, so nothing ever observes a half-written frame. Stdout gets the JPEG and no rename,
    /// there being no such thing.
    ///
    /// # Errors
    ///
    /// Anything the filesystem says, naming both the target and the temporary; or a decode
    /// failure for a PNG, which with strict mode on is what a truncated or corrupt frame gives
    /// (§1.3, A19).
    pub fn write(&self, shot: &Shot) -> Result<usize> {
        match self {
            // The one place a broken stdout is an error rather than "stop printing": stdout *is*
            // the output here, and a JPEG that did not get through is a failed screenshot (see
            // `cli::out`).
            ShotTarget::Stdout => {
                let mut out = std::io::stdout().lock();
                out.write_all(&shot.jpeg)
                    .and_then(|()| out.flush())
                    .context("writing the JPEG to stdout")?;
                Ok(shot.jpeg.len())
            }
            ShotTarget::Jpeg(path) => {
                write_atomically(path, |file| file.write_all(&shot.jpeg))?;
                Ok(shot.jpeg.len())
            }
            ShotTarget::Png(path) => {
                let png = encode_png(shot)?;
                write_atomically(path, |file| file.write_all(&png))?;
                Ok(png.len())
            }
        }
    }
}

/// The default file name: `nanokvm-<UTC yyyymmdd-HHMMSS>.jpg`.
///
/// UTC rather than local time so that two shots taken either side of a daylight-saving change
/// still sort in the order they were taken.
pub fn default_name(now: SystemTime) -> String {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let (y, mo, d, h, mi, s) = utc_parts(secs);
    format!("nanokvm-{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}.jpg")
}

/// Civil `(year, month, day, hour, minute, second)` in UTC from a Unix timestamp.
///
/// Howard Hinnant's `civil_from_days`, which is exact for every day this can be handed: Unix time
/// has no leap seconds, so the day is always 86400 seconds long and the split is arithmetic.
/// Here rather than from a date crate because this is the only date the client ever formats.
fn utc_parts(unix_secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let (days, rem) = (unix_secs / 86_400, unix_secs % 86_400);
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day, rem / 3_600, (rem / 60) % 60, rem % 60)
}

/// Decode the shot and re-encode it as 8-bit RGBA PNG.
///
/// The decoder is the pipeline's own (§1.3, A19: RGBA out, strict mode, the
/// [`MAX_WIDTH`]x[`MAX_HEIGHT`] ceiling), so a frame this refuses is a frame the viewer would
/// also have refused — a PNG is never a second opinion on a JPEG the client could not read.
fn encode_png(shot: &Shot) -> Result<Vec<u8>> {
    let frame = CompressedFrame {
        jpeg: shot.jpeg.clone(),
        width: shot.width,
        height: shot.height,
        captured_at: Duration::ZERO,
        sequence: 0,
    };
    let mut decoded = DecodedFrame::empty();
    let mut decoder = Decoder::new();
    decoder.decode(&frame, &mut decoded).with_context(|| {
        format!(
            "decoding the captured {}x{} frame for PNG output",
            shot.width, shot.height
        )
    })?;

    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, decoded.width, decoded.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().context("writing the PNG header")?;
    writer
        .write_image_data(&decoded.rgba)
        .context("writing the PNG image data")?;
    writer.finish().context("finishing the PNG")?;
    Ok(out)
}

/// A temporary that unlinks itself unless it was renamed into place.
///
/// A `.tmp` left in the user's directory is litter that looks like a half-written screenshot, and
/// an `if let Err` cleanup only covers the error path: a panic between `create` and `rename` — or
/// a SIGTERM from the harness that ran the shot — leaves the file behind. A `Drop` covers all
/// three, and [`Temporary::kept`] is the one way out of it.
struct Temporary {
    path: PathBuf,
    renamed: bool,
}

impl Temporary {
    fn new(path: PathBuf) -> Temporary {
        Temporary {
            path,
            renamed: false,
        }
    }

    /// The rename succeeded, so there is nothing left at this path to remove.
    fn kept(&mut self) {
        self.renamed = true;
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        if self.renamed {
            return;
        }
        // Ignored deliberately: this runs while unwinding as often as not, and a `Drop` that
        // panicked there would abort the process and take the real failure's message with it.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Write through `encode` to a temporary in `path`'s directory, then rename it over `path`.
///
/// Same directory because `rename` is only atomic within one filesystem. The temporary carries
/// the pid so two shots racing for one path collide on the rename — where the loser's bytes are
/// simply replaced — rather than on a shared scratch file, where they would interleave. It is
/// removed on every path that does not reach the rename (see [`Temporary`]), and `sync_all` runs
/// before the rename so the file that appears has its contents behind it.
fn write_atomically(
    path: &Path,
    encode: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> Result<()> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let name = path
        .file_name()
        .with_context(|| format!("{} does not name a file to write", path.display()))?;
    let mut tmp = Temporary::new(dir.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    )));

    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp.path)?;
        encode(&mut file)?;
        file.sync_all()?;
        std::fs::rename(&tmp.path, path)
    })();
    match result {
        Ok(()) => {
            tmp.kept();
            Ok(())
        }
        Err(e) => Err(e).with_context(|| {
            format!(
                "writing {} (via the temporary {})",
                path.display(),
                tmp.path.display()
            )
        }),
    }
}

/// The one line printed on success.
///
/// The counters are not decoration: they are the only account of what the policy did, and a
/// screenshot that silently threw away 40 frames at the wrong size is a different thing from one
/// that took the first frame it was offered (§3.4 — report what was observed, and nothing else).
pub fn summary_line(target: &str, bytes: usize, shot: &Shot) -> String {
    format!(
        "wrote {target}: {}x{}, {bytes} bytes, skipped {} frames, {} at another size{}{}",
        shot.width,
        shot.height,
        shot.skipped,
        shot.mismatched,
        // Only when it happened: a frame the source refused is unusual, and on the ordinary run
        // (every hardware run so far) there are none to report.
        match shot.discarded {
            0 => String::new(),
            n => format!(", {n} refused by the source"),
        },
        if shot.restarted {
            ", stream restarted once"
        } else {
            ""
        }
    )
}

/// Open the video node, keep one frame, write it, and say what it cost.
///
/// The node is opened with the viewer's own negotiation — `S_FMT` verified and `S_PARM` called
/// explicitly, or 1080p silently runs at the driver's default rate (§6, A7) — so the frame is
/// the frame the viewer would have shown. It is **dropped before the file is written** and
/// before this returns on every path: an open fd keeps a video index, and a node whose index is
/// held renumbers on the next replug (CLAUDE.md, C13).
///
/// Nothing here opens, or needs, the serial node.
pub fn run(args: &ShotArgs, video: &Path, width: u32, height: u32, fps: u32) -> Result<()> {
    // First, and before the node is touched: a request that cannot be written, or waited for, is
    // not worth opening a device for (§2.8 item 3).
    let target = ShotTarget::parse(args.path.as_deref(), SystemTime::now())?;
    let timeout = wait_limit(args.timeout)?;

    // All three signals, not just SIGINT: a harness timeout (SIGTERM) or a closed terminal
    // (SIGHUP) arriving between `create` and `rename` would otherwise leave a `.tmp` behind.
    install_signal_handlers();

    let mut source = V4l2Source::open(video, width, height, fps).with_context(|| {
        format!(
            "opening {} for MJPEG {width}x{height}@{fps}. Check the node is the dongle's video \
             interface and that nothing else is streaming from it.",
            video.display()
        )
    })?;
    log::info!("{}: waiting for a frame", source.describe());

    let deadline = Instant::now() + timeout;
    let picked = pick_frame(&mut source, &ShotPolicy::from_args(args), deadline);
    // Before the write, and before any `?` below: the fd is the thing that must not be held.
    drop(source);
    let shot = picked?;

    let bytes = target.write(&shot)?;
    let line = summary_line(&target.label(), bytes, &shot);
    match target {
        // The JPEG is on stdout; the line goes where it cannot corrupt it.
        ShotTarget::Stdout => out::note(&line),
        _ => out::line(&line),
    }
    Ok(())
}

/// `--timeout` as a [`Duration`], refused before anything is opened if it cannot be waited for.
///
/// # Errors
///
/// A value above [`MAX_TIMEOUT_SECS`]. `--timeout 18446744073709551615` used to be accepted here
/// and to panic later, in `Instant::now() + Duration`, with the video node already open.
fn wait_limit(secs: u64) -> Result<Duration> {
    if secs > MAX_TIMEOUT_SECS {
        bail!(
            "--timeout {secs} is more than the {MAX_TIMEOUT_SECS} second maximum. A shot waits \
             for one frame; anything that needs longer is a stream, not a screenshot."
        );
    }
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// The corpus is the authority on what a frame from this device ends with (§9.1), so the
    /// rule is checked against it rather than against a retyped tail.
    #[test]
    fn every_fixture_frame_ends_in_the_end_of_image_marker() {
        let mut checked = 0;
        for sub in ["", "absrange"] {
            for path in crate::capture::testsupport::fixture_paths(sub) {
                let bytes = crate::capture::testsupport::fixture_bytes(&path);
                assert!(
                    ends_in_eoi(&bytes),
                    "{} does not end in FF D9",
                    path.display()
                );
                assert_eq!(
                    &bytes[bytes.len() - 2..],
                    &[0xFF, 0xD9],
                    "{} is padded; the tolerance in `ends_in_eoi` is load-bearing after all",
                    path.display()
                );
                checked += 1;
            }
        }
        assert!(checked > 10, "only {checked} fixtures were checked");
    }

    #[test]
    fn a_truncated_frame_has_no_end_of_image_marker() {
        let bytes = crate::capture::testsupport::fixture_bytes(
            &crate::capture::testsupport::fixture_paths("absrange")[0],
        );
        assert!(!ends_in_eoi(&bytes[..bytes.len() - 1]));
        assert!(!ends_in_eoi(&bytes[..bytes.len() - 2]));
        assert!(!ends_in_eoi(&[]));
        assert!(!ends_in_eoi(&[0xD9]));
    }

    #[test]
    fn trailing_zero_padding_is_tolerated_but_nothing_else_is() {
        let mut padded = vec![0xFF, 0xD8, 0xFF, 0xD9];
        padded.extend_from_slice(&[0, 0, 0, 0]);
        assert!(ends_in_eoi(&padded));
        padded.push(0x01);
        assert!(!ends_in_eoi(&padded), "trailing junk is not padding");
        assert!(!ends_in_eoi(&[0x00, 0x00]), "padding alone is not a frame");
    }

    #[test]
    fn the_default_name_is_the_utc_timestamp() {
        // 2026-09-11T13:45:07Z.
        assert_eq!(
            default_name(UNIX_EPOCH + Duration::from_secs(1_789_134_307)),
            "nanokvm-20260911-134507.jpg"
        );
        assert_eq!(default_name(UNIX_EPOCH), "nanokvm-19700101-000000.jpg");
    }

    /// Dates the arithmetic is most likely to get wrong: the epoch, a leap day, a century that
    /// is not a leap year, one that is, and a year end.
    #[test]
    fn the_civil_date_matches_the_calendar_at_its_awkward_points() {
        for (secs, want) in [
            (0u64, (1970, 1, 1, 0, 0, 0)),
            (951_782_400, (2000, 2, 29, 0, 0, 0)),
            (4_107_542_400, (2100, 3, 1, 0, 0, 0)),
            (1_709_164_800, (2024, 2, 29, 0, 0, 0)),
            (1_735_689_599, (2024, 12, 31, 23, 59, 59)),
            (1_789_134_307, (2026, 9, 11, 13, 45, 7)),
        ] {
            assert_eq!(utc_parts(secs), want, "at {secs}");
        }
    }

    proptest! {
        /// A date function that can emit month 13 or day 0 produces a file name that sorts
        /// wrongly and a timestamp nobody can read back. The range stops at the end of 9999,
        /// which is where a four-digit year stops being sortable.
        #[test]
        fn every_timestamp_yields_a_calendar_date_and_a_sortable_name(
            a in 0u64..=253_402_300_799,
            b in 0u64..=253_402_300_799,
        ) {
            let (y, mo, d, h, mi, s) = utc_parts(a);
            prop_assert!((1970..=9999).contains(&y), "year {y}");
            prop_assert!((1..=12).contains(&mo), "month {mo}");
            prop_assert!((1..=31).contains(&d), "day {d}");
            prop_assert!(h < 24 && mi < 60 && s < 60);

            let (first, second) = (a.min(b), a.max(b));
            let (early, late) = (
                default_name(UNIX_EPOCH + Duration::from_secs(first)),
                default_name(UNIX_EPOCH + Duration::from_secs(second)),
            );
            prop_assert!(early <= late, "{early} sorts after {late}");
        }
    }

    /// C4: the bound is a usage error before anything is opened, not a panic after. `Instant`
    /// arithmetic with `u64::MAX` seconds overflows, and it used to do so with the node open.
    #[test]
    fn a_timeout_that_cannot_be_waited_for_is_refused_before_anything_opens() {
        for secs in [u64::MAX, MAX_TIMEOUT_SECS + 1] {
            let err = wait_limit(secs).expect_err("refused");
            assert!(err.to_string().contains("--timeout"), "{err}");
            assert!(
                err.to_string().contains(&MAX_TIMEOUT_SECS.to_string()),
                "the limit must be named: {err}"
            );
        }
        assert_eq!(
            wait_limit(MAX_TIMEOUT_SECS).expect("the limit itself is allowed"),
            Duration::from_secs(MAX_TIMEOUT_SECS)
        );
        // And the deadline every accepted value produces is representable, which is the point.
        assert!(Instant::now()
            .checked_add(wait_limit(MAX_TIMEOUT_SECS).expect("allowed"))
            .is_some());
    }

    /// A scratch directory under `target/`, removed with its contents when this drops.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Scratch {
            // Under `target/`, which is gitignored, rather than in the system temp directory:
            // these tests write files and the repo keeps its scratch where the build artefacts
            // already are.
            let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target/tmp")
                .join(format!(
                    "shot-{label}-{}-{:?}",
                    std::process::id(),
                    std::thread::current().id()
                ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch dir");
            Scratch(dir)
        }

        fn leftovers(&self) -> Vec<String> {
            std::fs::read_dir(&self.0)
                .expect("scratch dir")
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// R6: a write that fails leaves no `.tmp` behind. An `if let Err` cleanup covered this one;
    /// the `Drop` guard is what extends it to the two below.
    #[test]
    fn a_failed_write_leaves_no_temporary() {
        let scratch = Scratch::new("failed");
        let path = scratch.0.join("frame.jpg");
        let err = write_atomically(&path, |_| Err(std::io::Error::other("the encoder gave up")))
            .expect_err("the write must fail");
        assert!(err.to_string().contains("frame.jpg"), "{err}");
        assert!(!path.exists(), "a failed write created the target");
        assert!(
            scratch.leftovers().is_empty(),
            "left behind: {:?}",
            scratch.leftovers()
        );
    }

    /// The case the `Drop` guard is actually for: an unwind between `create` and `rename`. A
    /// panic used to leave a `.tmp` in the user's directory looking like a half-written shot —
    /// and so does a SIGTERM, which is the same unwind by a different route (`install_signal_handlers`).
    #[test]
    fn a_panic_midwrite_leaves_no_temporary() {
        let scratch = Scratch::new("panic");
        let path = scratch.0.join("frame.png");
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = write_atomically(&path, |_| panic!("the encoder panicked"));
        }));
        assert!(caught.is_err(), "the panic must have propagated");
        assert!(!path.exists());
        assert!(
            scratch.leftovers().is_empty(),
            "left behind: {:?}",
            scratch.leftovers()
        );
    }

    /// The ordinary path still produces the file, and nothing beside it.
    #[test]
    fn a_successful_write_renames_the_temporary_into_place() {
        let scratch = Scratch::new("ok");
        let path = scratch.0.join("frame.jpg");
        write_atomically(&path, |f| f.write_all(b"bytes")).expect("write");
        assert_eq!(std::fs::read(&path).expect("read back"), b"bytes");
        assert_eq!(scratch.leftovers(), vec!["frame.jpg".to_string()]);
    }

    #[test]
    fn the_policy_takes_skip_and_any_size_from_the_command_line_and_the_rest_from_c14() {
        let args = ShotArgs {
            path: None,
            skip: 3,
            any_size: true,
            timeout: 10,
        };
        let policy = ShotPolicy::from_args(&args);
        assert_eq!(policy.skip, 3);
        assert!(policy.any_size);
        assert_eq!(policy.restart_after, RESTART_AFTER);
        assert_eq!(policy.give_up_after, GIVE_UP_AFTER);
        assert!(
            policy.restart_after > 8,
            "A6's eight-frame transient must not be able to trigger a restart"
        );
        assert!(policy.give_up_after > policy.restart_after);
    }
}
