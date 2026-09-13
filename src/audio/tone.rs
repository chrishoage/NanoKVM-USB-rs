//! Goertzel tone detection for audio verification.
//!
//! Compare energy at the expected frequency with nearby reference frequencies. Silent
//! controls distinguish a working audio path from stale samples or background noise.

use super::CHANNELS;

/// One channel's samples from an interleaved buffer, as `f32` in roughly ±1.
///
/// `channel` beyond the buffer's channel count yields an empty vector rather than panicking: this
/// runs inside a test harness whose input came off a wire.
pub fn channel_of(interleaved: &[i16], channels: usize, channel: usize) -> Vec<f32> {
    if channels == 0 || channel >= channels {
        return Vec::new();
    }
    interleaved
        .iter()
        .skip(channel)
        .step_by(channels)
        .map(|s| *s as f32 / i16::MAX as f32)
        .collect()
}

/// Both channels averaged, which is what a mono tone played to both speakers looks like.
pub fn mono(interleaved: &[i16]) -> Vec<f32> {
    interleaved
        .as_chunks::<CHANNELS>()
        .0
        .iter()
        .map(|frame| {
            frame
                .iter()
                .map(|s| *s as f32 / i16::MAX as f32)
                .sum::<f32>()
                / CHANNELS as f32
        })
        .collect()
}

/// Goertzel power at `freq_hz`, normalised by sample count so it does not depend on how long the
/// capture was.
///
/// Returns 0.0 for an empty buffer or a frequency that is not below the Nyquist limit, both of
/// which are caller errors that must not become panics inside a hardware test.
pub fn power_at(samples: &[f32], sample_rate: u32, freq_hz: f32) -> f32 {
    if samples.is_empty()
        || sample_rate == 0
        || freq_hz <= 0.0
        || freq_hz * 2.0 >= sample_rate as f32
    {
        return 0.0;
    }
    let omega = 2.0 * std::f32::consts::PI * freq_hz / sample_rate as f32;
    let coeff = 2.0 * omega.cos();
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for &x in samples {
        let s0 = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    // |X(k)|^2 without the final complex rotation, which the magnitude does not need.
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    power.max(0.0) / samples.len() as f32
}

/// What [`find_tone`] concluded.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ToneReport {
    /// The frequency asked about.
    pub freq_hz: f32,
    /// Goertzel power there.
    pub power: f32,
    /// The median power across the reference frequencies — the floor this is being judged against.
    pub reference: f32,
    /// `power / reference`. The verdict is a threshold on this, not on `power`, because the
    /// absolute level depends on the target's volume and on the dongle's gain, neither of which
    /// this client sets.
    pub ratio: f32,
    /// Root-mean-square of the samples, so "the tone is missing" can be told apart from "there
    /// were no samples at all" — a silent capture has a ratio of 0/0 and nothing else to say.
    pub rms: f32,
}

impl ToneReport {
    /// Whether the tone is present at `min_ratio` times the reference floor.
    pub fn present(&self, min_ratio: f32) -> bool {
        self.rms > 0.0 && self.ratio >= min_ratio
    }
}

/// The reference frequencies [`find_tone`] compares against when the caller names none.
///
/// not harmonics or subharmonics of anything: a square-ish tone from a cheap DAC has
/// energy at 2f and 3f, and a floor measured there would flatter the result. These are unrelated
/// frequencies in the same band, all comfortably inside the dongle's 48 kHz.
pub const DEFAULT_REFERENCES: [f32; 6] = [137.0, 311.0, 613.0, 1907.0, 3121.0, 5003.0];

/// Look for `freq_hz` in `samples`, against a floor measured at `references`.
///
/// The floor is the median rather than the mean, so one reference frequency that happens to
/// land on real content in the target's audio cannot lift the whole floor and hide the tone.
pub fn find_tone(
    samples: &[f32],
    sample_rate: u32,
    freq_hz: f32,
    references: &[f32],
) -> ToneReport {
    let power = power_at(samples, sample_rate, freq_hz);
    let mut floor: Vec<f32> = references
        .iter()
        .filter(|f| (**f - freq_hz).abs() > 1.0)
        .map(|f| power_at(samples, sample_rate, *f))
        .collect();
    floor.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let reference = match floor.len() {
        0 => 0.0,
        n => floor[n / 2],
    };
    let rms = if samples.is_empty() {
        0.0
    } else {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    };
    // A perfectly clean synthetic signal has a floor of zero, and a ratio of `inf` is
    // useless to print. Floor it at something far below any real noise floor instead.
    let reference = reference.max(1e-12);
    ToneReport {
        freq_hz,
        power,
        reference,
        ratio: power / reference,
        rms,
    }
}

/// The ratio a 1 kHz tone recorded through this dongle is expected to clear.
///
/// A sine at moderate level against the noise floor of a USB capture chip is orders of magnitude,
/// not a factor of two; 100 is a threshold that a real tone clears with room to spare and that
/// noise, hum or an unrelated desktop sound does not. It is a starting point for the hardware run
/// to confirm or move, and the run should record the ratio it saw rather than only that
/// it passed.
pub const TONE_RATIO: f32 = 100.0;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::SAMPLE_RATE;

    /// `seconds` of a sine at `freq`, as interleaved stereo i16 — the shape the capture side
    /// produces.
    fn sine(freq: f32, seconds: f32, amplitude: f32) -> Vec<i16> {
        let frames = (SAMPLE_RATE as f32 * seconds) as usize;
        let mut out = Vec::with_capacity(frames * CHANNELS);
        for n in 0..frames {
            let t = n as f32 / SAMPLE_RATE as f32;
            let v = (amplitude * (2.0 * std::f32::consts::PI * freq * t).sin() * i16::MAX as f32)
                as i16;
            for _ in 0..CHANNELS {
                out.push(v);
            }
        }
        out
    }

    #[test]
    fn a_1khz_sine_is_found_at_1khz() {
        let pcm = sine(1000.0, 0.5, 0.3);
        let report = find_tone(&mono(&pcm), SAMPLE_RATE, 1000.0, &DEFAULT_REFERENCES);
        assert!(
            report.present(TONE_RATIO),
            "a clean 1 kHz sine must be found: {report:?}"
        );
        assert!(report.rms > 0.1, "{report:?}");
    }

    #[test]
    fn a_400hz_sine_is_not_mistaken_for_1khz() {
        let pcm = sine(400.0, 0.5, 0.3);
        let report = find_tone(&mono(&pcm), SAMPLE_RATE, 1000.0, &DEFAULT_REFERENCES);
        assert!(
            !report.present(TONE_RATIO),
            "1 kHz must not be found in a 400 Hz tone: {report:?}"
        );
        // And the detector finds the tone that *is* there, so a failure above is about the
        // frequency and not about the detector being broken.
        let right = find_tone(&mono(&pcm), SAMPLE_RATE, 400.0, &DEFAULT_REFERENCES);
        assert!(right.present(TONE_RATIO), "{right:?}");
    }

    #[test]
    fn silence_is_not_a_tone() {
        let report = find_tone(
            &mono(&vec![0i16; 4800]),
            SAMPLE_RATE,
            1000.0,
            &DEFAULT_REFERENCES,
        );
        assert_eq!(report.rms, 0.0);
        assert!(!report.present(TONE_RATIO), "{report:?}");
    }

    /// The one that matters for the hardware run: a tone buried in noise at a level a listener
    /// would call quiet is still found, so a pass does not depend on the target's volume.
    #[test]
    fn a_quiet_tone_under_noise_is_still_found() {
        let mut pcm = sine(1000.0, 1.0, 0.05);
        // A cheap deterministic pseudo-noise; no rand dependency, and the same every run.
        let mut state = 0x1234_5678u32;
        for s in pcm.iter_mut() {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let noise = ((state >> 16) as i16) / 64;
            *s = s.saturating_add(noise);
        }
        let report = find_tone(&mono(&pcm), SAMPLE_RATE, 1000.0, &DEFAULT_REFERENCES);
        assert!(
            report.present(TONE_RATIO),
            "a quiet tone under noise must still clear the threshold: {report:?}"
        );
    }

    #[test]
    fn a_channel_can_be_taken_out_of_an_interleaved_buffer() {
        let interleaved: Vec<i16> = vec![100, -100, 200, -200, 300, -300];
        assert_eq!(
            channel_of(&interleaved, 2, 0),
            vec![
                100.0 / i16::MAX as f32,
                200.0 / i16::MAX as f32,
                300.0 / i16::MAX as f32
            ]
        );
        assert!(channel_of(&interleaved, 2, 5).is_empty());
        assert!(channel_of(&interleaved, 0, 0).is_empty());
        // Left and right cancel, so the mono mix of this buffer is silence.
        assert_eq!(mono(&interleaved), vec![0.0, 0.0, 0.0]);
    }

    #[test]
    fn nothing_here_panics_on_a_degenerate_request() {
        assert_eq!(power_at(&[], SAMPLE_RATE, 1000.0), 0.0);
        assert_eq!(power_at(&[0.1, 0.2], 0, 1000.0), 0.0);
        assert_eq!(power_at(&[0.1, 0.2], SAMPLE_RATE, -5.0), 0.0);
        // Above Nyquist.
        assert_eq!(power_at(&[0.1, 0.2], SAMPLE_RATE, 30_000.0), 0.0);
    }
}
