//! ALSA capture and playback adapters.
//!
//! Capture requires the device's native S16_LE, stereo, 48 kHz format without resampling.
//! Playback uses the host's `default` PCM and permits conversion. Non-blocking I/O with
//! bounded waits lets the workers observe shutdown between periods.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ::alsa::pcm::{Access, Format, HwParams, State, PCM};
use ::alsa::{Direction, ValueOr};

use super::{
    AudioConfig, AudioError, AudioSide, PcmSink, PcmSinkOpener, PcmSource, PcmSourceOpener,
    CHANNELS, SAMPLE_RATE,
};

/// The ALSA device the host's own output is spelled as. See the module docs.
pub const PLAYBACK_DEVICE: &str = "default";

/// What one side asks ALSA for, and what it will accept back.
///
/// Two fields and one rule each, because the two sides are not symmetrical: the capture side is
/// reading a device with one format and a conversion there would be a silent corruption
/// of the thing being measured, while the playback side is a guest in the user's own output chain
/// and conversion is that chain's job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HwPlan {
    /// Fail the open unless ALSA grants [`SAMPLE_RATE`] exactly.
    pub exact_rate: bool,
    /// Let the device's `plug` layer resample. `false` calls `set_rate_resample(false)`.
    pub allow_resample: bool,
}

impl HwPlan {
    /// PCM configuration for this side.
    pub fn for_side(side: AudioSide) -> HwPlan {
        match side {
            // The dongle offers S16_LE/2ch/48000 and nothing else, so anything else means this is
            // not the device this client thinks it is.
            AudioSide::Capture => HwPlan {
                exact_rate: true,
                allow_resample: false,
            },
            // `default` is the user's own output: it is allowed to be a `plug` in front of
            // hardware that cannot do 48 kHz, and resampling is how that works.
            AudioSide::Playback => HwPlan {
                exact_rate: false,
                allow_resample: true,
            },
        }
    }
}

/// Turn an `alsa::Error` into the condition it is, keeping the device in the message.
///
/// The errno is what separates "somebody else has the card" from "the card is gone", and those
/// two want different words in front of a user: one is fixed by closing something, the other by
/// plugging something in.
fn classify(device: &str, context: &str, error: ::alsa::Error) -> AudioError {
    let why = format!("{context}: {error}");
    let device = device.to_string();
    match error.errno() {
        libc::EBUSY => AudioError::Busy { device, why },
        libc::ENODEV | libc::ENOENT | libc::ENXIO | libc::EIO => AudioError::Gone { device, why },
        _ => AudioError::Io { device, why },
    }
}

/// Configure one PCM and check that it really got what was asked for.
fn configure(
    pcm: &PCM,
    device: &str,
    config: &AudioConfig,
    side: AudioSide,
) -> Result<(), AudioError> {
    let plan = HwPlan::for_side(side);
    let hw = HwParams::any(pcm).map_err(|e| classify(device, "hw_params", e))?;
    let set = |what: &str, r: Result<(), ::alsa::Error>| -> Result<(), AudioError> {
        r.map_err(|e| {
            let why = format!("{what}: {e}");
            match e.errno() {
                libc::EBUSY => AudioError::Busy {
                    device: device.to_string(),
                    why,
                },
                _ => AudioError::Open {
                    device: device.to_string(),
                    why,
                },
            }
        })
    };
    set(
        "access RW_INTERLEAVED",
        hw.set_access(Access::RWInterleaved),
    )?;
    set("format S16_LE", hw.set_format(Format::s16()))?;
    set("2 channels", hw.set_channels(CHANNELS as u32))?;
    if !plan.allow_resample {
        set("no resampling", hw.set_rate_resample(false))?;
    }
    set(
        "48000 Hz",
        hw.set_rate(SAMPLE_RATE, ValueOr::Nearest).map(|_| ()),
    )?;
    hw.set_period_size_near(config.ring.period_frames as i64, ValueOr::Nearest)
        .map_err(|e| classify(device, "period size", e))?;
    hw.set_buffer_size_near((config.ring.period_frames * config.device_periods) as i64)
        .map_err(|e| classify(device, "buffer size", e))?;
    pcm.hw_params(&hw)
        .map_err(|e| classify(device, "applying hw params", e))?;

    // Read back. The device has one format; anything else means this is not the device we mean.
    let got_rate = hw.get_rate().map_err(|e| classify(device, "get_rate", e))?;
    let got_channels = hw
        .get_channels()
        .map_err(|e| classify(device, "get_channels", e))?;
    if got_channels != CHANNELS as u32 || (plan.exact_rate && got_rate != SAMPLE_RATE) {
        return Err(AudioError::Open {
            device: device.to_string(),
            why: format!(
                "asked for {SAMPLE_RATE} Hz / {CHANNELS} ch S16_LE and got {got_rate} Hz / \
                 {got_channels} ch; this device is not the one this client expects"
            ),
        });
    }

    // The *granted* sizes, which are not necessarily the asked-for ones: both setters are
    // `_near`. They are logged rather than enforced because ALSA is entitled to round them, and
    // the number that matters afterwards — what the device buffer is, next to the ring's
    // configured depth — is otherwise invisible in a session log.
    let period = hw
        .get_period_size()
        .map_err(|e| classify(device, "get_period_size", e))?;
    let buffer = hw
        .get_buffer_size()
        .map_err(|e| classify(device, "get_buffer_size", e))?;
    log::info!(
        "audio: {side} {device} granted {period} frames per period, {buffer} frames of buffer \
         ({got_rate} Hz, {got_channels} ch, resampling {})",
        if plan.allow_resample {
            "allowed"
        } else {
            "disabled"
        }
    );
    // The ring hands over fixed `period_frames` buffers whatever ALSA granted, so a divergence on
    // the capture side means every read spans a fraction of a device period. That is survivable —
    // `read_period` fills the buffer across as many device periods as it takes — but it is not
    // something to discover from a latency figure months later.
    if side == AudioSide::Capture && period != config.ring.period_frames as i64 {
        log::warn!(
            "audio: {device} granted a period of {period} frames where the ring uses {}; reads \
             will span device periods. Nothing is lost, but the configured depth no longer lines \
             up with the device's own buffering",
            config.ring.period_frames
        );
    }
    Ok(())
}

/// How long one `snd_pcm_wait` may block, in milliseconds: one period, floored.
///
/// This is what makes shutdown bounded. The PCMs are opened non-blocking and waited on, so
/// every device call this module makes returns within a period whether or not any audio arrived;
/// the loop around it re-checks the stop flag and goes back in. A blocking `readi` on a card that
/// has been yanked returns when the driver says so and not before, which is a thread
/// [`crate::audio::AudioHandle::stop`] could only detach.
fn wait_ms(config: &AudioConfig) -> u32 {
    let period = (config.ring.period_frames as u64 * 1000 / SAMPLE_RATE as u64) as u32;
    period.max(5)
}

/// Wait for the device, or for the caller to stop.
///
/// `Ok(true)` means the PCM is ready; `Ok(false)` means the timeout passed and the caller should
/// look at its stop flag again. `EAGAIN` from a non-blocking PCM is that same "not yet".
fn wait_ready(pcm: &PCM, device: &str, timeout_ms: u32) -> Result<bool, AudioError> {
    match pcm.wait(Some(timeout_ms)) {
        Ok(ready) => Ok(ready),
        Err(e) if e.errno() == libc::EAGAIN => Ok(false),
        Err(e) => {
            // An xrun surfaces here as well as from `readi`/`writei`, and is recoverable in
            // place: it is the kernel's own buffer, not the ring's.
            pcm.try_recover(e, true)
                .map_err(|e| classify(device, "recovering after a wait", e))?;
            Ok(false)
        }
    }
}

/// The error a device call returns when it was interrupted by the shutdown flag rather than by
/// anything the device did. The audio loops drop it without reporting a condition.
fn stopping(device: &str) -> AudioError {
    AudioError::Io {
        device: device.to_string(),
        why: "the audio threads are stopping".to_string(),
    }
}

/// Capture from the dongle's card, at the one format it has.
pub struct AlsaCapture {
    pcm: PCM,
    device: String,
    wait_ms: u32,
    /// Set when the audio threads are stopping; checked between waits. `None` for a caller that
    /// never handed one over — a hardware test reading a fixed number of periods.
    stop: Option<Arc<AtomicBool>>,
}

impl AlsaCapture {
    /// Open `device` (an ALSA name such as `hw:8`) for capture.
    pub fn open(device: &str, config: &AudioConfig) -> Result<Self, AudioError> {
        Self::open_with_stop(device, config, None)
    }

    /// The same, with the flag that interrupts a wait (see `wait_ms`).
    pub fn open_with_stop(
        device: &str,
        config: &AudioConfig,
        stop: Option<Arc<AtomicBool>>,
    ) -> Result<Self, AudioError> {
        // Non-blocking: every wait below is bounded, so no device call can outlive a shutdown.
        let pcm = PCM::new(device, Direction::Capture, true)
            .map_err(|e| classify(device, "opening for capture", e))?;
        configure(&pcm, device, config, AudioSide::Capture)?;
        pcm.prepare()
            .map_err(|e| classify(device, "preparing the capture stream", e))?;
        // A blocking capture stream starts on its first `readi`; a non-blocking one has to be
        // told, or the first wait has nothing to wait for.
        if pcm.state() != State::Running {
            pcm.start()
                .map_err(|e| classify(device, "starting the capture stream", e))?;
        }
        Ok(AlsaCapture {
            pcm,
            device: device.to_string(),
            wait_ms: wait_ms(config),
            stop,
        })
    }

    fn stopping(&self) -> bool {
        self.stop.as_ref().is_some_and(|s| s.load(Ordering::SeqCst))
    }
}

impl PcmSource for AlsaCapture {
    fn read_period(&mut self, out: &mut [i16]) -> Result<(), AudioError> {
        let frames = out.len() / CHANNELS;
        let mut done = 0usize;
        while done < frames {
            if self.stopping() {
                return Err(stopping(&self.device));
            }
            if !wait_ready(&self.pcm, &self.device, self.wait_ms)? {
                continue; // nothing yet; the check above is why this cannot spin for ever.
            }
            let io = self
                .pcm
                .io_i16()
                .map_err(|e| classify(&self.device, "io_i16", e))?;
            match io.readi(&mut out[done * CHANNELS..]) {
                // No progress and no error: wait again rather than spin. On a non-blocking PCM
                // this is ordinary — the wait said ready, another reader took the frames.
                Ok(0) => continue,
                Ok(n) => done += n,
                Err(e) if e.errno() == libc::EAGAIN => continue,
                Err(e) => {
                    // An overrun is the kernel's buffer filling because this thread was late; it
                    // is recoverable in place and is *not* the ring's overrun counter, which
                    // counts what this module itself dropped.
                    self.pcm
                        .try_recover(e, true)
                        .map_err(|e| classify(&self.device, "recovering the capture stream", e))?;
                    if self.pcm.state() != State::Running {
                        self.pcm.start().map_err(|e| {
                            classify(&self.device, "restarting the capture stream", e)
                        })?;
                    }
                }
            }
        }
        Ok(())
    }

    fn describe(&self) -> String {
        format!("ALSA {} S16_LE {SAMPLE_RATE} Hz {CHANNELS} ch", self.device)
    }
}

/// Play to the host's own output.
pub struct AlsaPlayback {
    pcm: PCM,
    device: String,
    wait_ms: u32,
    stop: Option<Arc<AtomicBool>>,
}

impl AlsaPlayback {
    pub fn open(device: &str, config: &AudioConfig) -> Result<Self, AudioError> {
        Self::open_with_stop(device, config, None)
    }

    pub fn open_with_stop(
        device: &str,
        config: &AudioConfig,
        stop: Option<Arc<AtomicBool>>,
    ) -> Result<Self, AudioError> {
        let pcm = PCM::new(device, Direction::Playback, true)
            .map_err(|e| classify(device, "opening for playback", e))?;
        // [`HwPlan::for_side`]: `default` is allowed to be a `plug` that resamples for hardware
        // that cannot do 48 kHz. That is the one place in this module where a conversion is
        // acceptable, because it is the user's own output chain and not the dongle's stream.
        configure(&pcm, device, config, AudioSide::Playback)?;
        pcm.prepare()
            .map_err(|e| classify(device, "preparing the playback stream", e))?;
        Ok(AlsaPlayback {
            pcm,
            device: device.to_string(),
            wait_ms: wait_ms(config),
            stop,
        })
    }

    fn stopping(&self) -> bool {
        self.stop.as_ref().is_some_and(|s| s.load(Ordering::SeqCst))
    }
}

impl PcmSink for AlsaPlayback {
    fn write_period(&mut self, samples: &[i16]) -> Result<(), AudioError> {
        let frames = samples.len() / CHANNELS;
        let mut done = 0usize;
        while done < frames {
            if self.stopping() {
                return Err(stopping(&self.device));
            }
            if !wait_ready(&self.pcm, &self.device, self.wait_ms)? {
                continue; // no room yet.
            }
            let io = self
                .pcm
                .io_i16()
                .map_err(|e| classify(&self.device, "io_i16", e))?;
            match io.writei(&samples[done * CHANNELS..]) {
                Ok(0) => continue,
                Ok(n) => done += n,
                Err(e) if e.errno() == libc::EAGAIN => continue,
                Err(e) => {
                    self.pcm
                        .try_recover(e, true)
                        .map_err(|e| classify(&self.device, "recovering the playback stream", e))?;
                }
            }
        }
        Ok(())
    }

    fn describe(&self) -> String {
        format!("ALSA {} S16_LE {SAMPLE_RATE} Hz {CHANNELS} ch", self.device)
    }
}

/// The opener for ALSA `default`, which never changes name and so needs no discovery.
pub struct DefaultSinkOpener {
    device: String,
    config: AudioConfig,
    stop: Option<Arc<AtomicBool>>,
}

impl DefaultSinkOpener {
    pub fn new(config: AudioConfig) -> Self {
        DefaultSinkOpener {
            device: PLAYBACK_DEVICE.to_string(),
            config,
            stop: None,
        }
    }

    /// A sink other than `default`. Nothing on the command line reaches this yet; it exists so a
    /// hardware run can be routed to a null sink instead of the user's speakers.
    pub fn named(device: &str, config: AudioConfig) -> Self {
        DefaultSinkOpener {
            device: device.to_string(),
            config,
            stop: None,
        }
    }
}

impl PcmSinkOpener for DefaultSinkOpener {
    fn open(&mut self) -> Result<Box<dyn PcmSink>, AudioError> {
        Ok(Box::new(AlsaPlayback::open_with_stop(
            &self.device,
            &self.config,
            self.stop.clone(),
        )?))
    }

    fn describe(&self) -> String {
        format!("ALSA {}", self.device)
    }

    fn on_stop_flag(&mut self, stop: Arc<AtomicBool>) {
        self.stop = Some(stop);
    }
}

/// Open one ALSA card for capture. The function [`crate::discovery::reopen`]'s opener calls once
/// it has resolved *which* card, kept separate so that resolution can be tested without ALSA.
pub fn open_capture(
    device: &str,
    config: &AudioConfig,
    stop: Option<Arc<AtomicBool>>,
) -> Result<Box<dyn PcmSource>, AudioError> {
    Ok(Box::new(AlsaCapture::open_with_stop(device, config, stop)?))
}

/// A [`PcmSourceOpener`] for one fixed ALSA name. Used only where discovery has been bypassed —
/// a hardware test naming `hw:8` directly.
pub struct FixedSourceOpener {
    device: String,
    config: AudioConfig,
    stop: Option<Arc<AtomicBool>>,
}

impl FixedSourceOpener {
    pub fn new(device: &str, config: AudioConfig) -> Self {
        FixedSourceOpener {
            device: device.to_string(),
            config,
            stop: None,
        }
    }
}

impl PcmSourceOpener for FixedSourceOpener {
    fn open(&mut self) -> Result<Box<dyn PcmSource>, AudioError> {
        open_capture(&self.device, &self.config, self.stop.clone())
    }

    fn describe(&self) -> String {
        format!("ALSA {}", self.device)
    }

    fn on_stop_flag(&mut self, stop: Arc<AtomicBool>) {
        self.stop = Some(stop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Capture must keep the native format; host playback may convert. Equal host and
    /// device rates would hide an accidentally reversed policy.
    #[test]
    fn only_the_playback_side_may_resample() {
        let capture = HwPlan::for_side(AudioSide::Capture);
        assert!(!capture.allow_resample, "hw:N is a byte copy or an error");
        assert!(capture.exact_rate);

        let playback = HwPlan::for_side(AudioSide::Playback);
        assert!(
            playback.allow_resample,
            "`default` may be a plug in front of hardware that cannot do 48 kHz"
        );
        assert!(
            !playback.exact_rate,
            "a rate other than 48 kHz on the user's own output is not this client's business"
        );
    }

    /// The wait is one period, so a stopping thread is never more than that away from noticing —
    /// and never zero, which would be a spin.
    #[test]
    fn the_device_wait_is_one_period_and_never_zero() {
        let mut config = AudioConfig::default();
        assert_eq!(wait_ms(&config), 10, "480 frames at 48 kHz is 10 ms");
        config.ring.period_frames = 1;
        assert_eq!(wait_ms(&config), 5, "floored rather than a busy loop");
    }
}
