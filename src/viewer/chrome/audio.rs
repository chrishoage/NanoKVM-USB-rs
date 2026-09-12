//! Audio status and controls derived from worker snapshots.
//!
//! Opening and unavailable states remain distinct from playback. Buffer depth is configured
//! capacity, not a measurement of sound latency.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::audio::{AudioSnapshot, RingConfig};

/// What the audio path is doing, as one word for the popover's header line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioState {
    /// Playing.
    On,
    /// Running, but the user has muted it. Distinct from [`AudioState::Off`]: the stream is still
    /// open and the counters still move, which is why the counters stay visible when muted.
    Muted,
    /// Disabled by `--no-audio`.
    Off,
    /// Capture or playback has not transferred its first period. Overdue opening
    /// becomes unavailable through snapshot supervision.
    Opening { side: String },
    /// Unavailable audio with the capture or playback failure reason.
    Absent { reason: String },
}

impl Default for AudioState {
    /// Absent, because a viewer that has not started an audio thread has no stream — and saying
    /// "off" would claim the user asked for that.
    fn default() -> Self {
        AudioState::Absent {
            reason: "audio has not been started".to_string(),
        }
    }
}

impl AudioState {
    /// The word for the popover's first line.
    ///
    /// A `Cow` rather than a `&str` because [`AudioState::Opening`] is the one state whose
    /// sentence is assembled from the side's name; everything else borrows.
    pub fn label(&self) -> Cow<'_, str> {
        match self {
            AudioState::On => Cow::Borrowed("on"),
            AudioState::Muted => Cow::Borrowed("muted"),
            AudioState::Off => Cow::Borrowed("off (--no-audio)"),
            AudioState::Opening { side } => Cow::Owned(format!("opening ({side})…")),
            AudioState::Absent { reason } => Cow::Borrowed(reason),
        }
    }

    /// Whether playback is running and the mute control can affect it.
    pub fn is_running(&self) -> bool {
        matches!(self, AudioState::On | AudioState::Muted)
    }
}

/// One reading of the audio path, taken on the event loop and handed to the UI build.
#[derive(Debug, Clone)]
pub struct ChromeAudio {
    /// What it is doing.
    pub state: AudioState,
    /// Periods dropped because the ring was full.
    pub overruns: u64,
    /// Periods of silence played because the ring was empty — playback is ahead of capture.
    pub underruns: u64,
    /// Periods inserted or dropped to correct sustained clock drift, separate from xruns.
    pub drift_corrections: u64,
    /// Real audio periods written to the sink.
    pub played: u64,
    /// Periods written as silence because the ring had nothing to give. Shown beside `played`
    /// rather than folded into it: a session that "played" 30 000 periods of which 29 000 were
    /// silence is a broken one, and one counter says it went perfectly.
    pub silent: u64,
    /// Periods written as silence because the user muted it. Not a failure and not audio, so it
    /// is neither of the two above.
    pub muted_periods: u64,
    /// Configured ring capacity in periods; not measured latency.
    pub periods: u32,
    /// Frames per period, the other half of the depth.
    pub frames_per_period: u32,
    /// The playback thread's mute flag. Shared, so the toggle takes effect on the next period
    /// rather than on the next chrome frame.
    pub mute: Arc<AtomicBool>,
}

impl Default for ChromeAudio {
    /// Unavailable audio before an audio handle exists.
    fn default() -> Self {
        ChromeAudio {
            state: AudioState::Absent {
                reason: "audio is not wired up in this build".to_string(),
            },
            overruns: 0,
            underruns: 0,
            drift_corrections: 0,
            played: 0,
            silent: 0,
            muted_periods: 0,
            periods: 0,
            frames_per_period: 0,
            mute: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl ChromeAudio {
    /// Build UI audio state from a worker snapshot and its live mute flag.
    pub fn from_snapshot(
        snapshot: &AudioSnapshot,
        ring: RingConfig,
        mute: Arc<AtomicBool>,
    ) -> ChromeAudio {
        let state = match (&snapshot.condition, snapshot.opening_side()) {
            (Some(condition), _) => AudioState::Absent {
                reason: condition.to_string(),
            },
            (None, Some(side)) => AudioState::Opening {
                side: side.to_string(),
            },
            (None, None) if mute.load(Ordering::Relaxed) => AudioState::Muted,
            (None, None) => AudioState::On,
        };
        ChromeAudio {
            state,
            overruns: snapshot.counts.overruns,
            underruns: snapshot.counts.underruns,
            // The menu combines correction directions; detailed statistics retain each
            // direction for drift-rate measurements.
            drift_corrections: snapshot.counts.drift_drops + snapshot.counts.drift_inserts,
            played: snapshot.periods_played,
            silent: snapshot.periods_silent,
            muted_periods: snapshot.periods_muted,
            periods: ring.periods as u32,
            frames_per_period: ring.period_frames as u32,
            mute,
        }
    }

    /// The configured depth as one phrase: `4 x 480 frames (1920 frames)`.
    ///
    /// `None` when nothing has been configured, so the popover can say "not configured" rather
    /// than print `0 x 0`, which reads like a fault.
    pub fn depth(&self) -> Option<String> {
        if self.periods == 0 || self.frames_per_period == 0 {
            return None;
        }
        Some(format!(
            "{} x {} frames ({} frames configured)",
            self.periods,
            self.frames_per_period,
            u64::from(self.periods) * u64::from(self.frames_per_period)
        ))
    }

    /// Whether the shared flag currently says muted.
    pub fn is_muted(&self) -> bool {
        self.mute.load(Ordering::Relaxed)
    }

    /// Set the shared flag. Takes effect on the playback thread's next period.
    pub fn set_muted(&self, muted: bool) {
        self.mute.store(muted, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_depth_is_periods_times_frames_with_its_provenance() {
        let a = ChromeAudio {
            periods: 4,
            frames_per_period: 480,
            ..ChromeAudio::default()
        };
        let depth = a.depth().expect("configured");
        assert!(depth.contains("4 x 480"), "{depth}");
        assert!(depth.contains("1920"), "{depth}");
        assert!(
            depth.contains("configured"),
            "label the configured capacity: {depth}"
        );
    }

    #[test]
    fn an_unconfigured_depth_is_none_rather_than_zero() {
        assert!(ChromeAudio::default().depth().is_none());
        assert!(ChromeAudio {
            periods: 4,
            ..ChromeAudio::default()
        }
        .depth()
        .is_none());
    }

    #[test]
    fn muting_goes_through_the_shared_flag() {
        let a = ChromeAudio::default();
        assert!(!a.is_muted());
        a.set_muted(true);
        assert!(
            a.mute.load(Ordering::Relaxed),
            "the playback worker’s mute flag"
        );
        assert!(a.is_muted());
    }

    /// Only a running stream can be muted; the other two states make the toggle inert, which is
    /// what its disabled tooltip has to explain.
    #[test]
    fn only_a_running_stream_is_mutable() {
        assert!(AudioState::On.is_running());
        assert!(AudioState::Muted.is_running());
        assert!(!AudioState::Off.is_running());
        assert!(!AudioState::Absent {
            reason: "card8 is busy".to_string()
        }
        .is_running());
    }

    /// Identify the opening side and disable mute until playback is available.
    #[test]
    fn an_opening_side_is_named_and_is_not_mutable() {
        let state = AudioState::Opening {
            side: "playback".to_string(),
        };
        assert_eq!(state.label(), "opening (playback)…");
        assert!(
            !state.is_running(),
            "there is no stream yet, so nothing to mute"
        );
        assert_ne!(
            state.label(),
            AudioState::On.label(),
            "\"opening\" and \"on\" must not read the same: unfinished opens need a distinct status"
        );
    }

    /// Preserve the audio subsystem’s failure reason in the menu.
    #[test]
    fn an_absent_stream_shows_the_reason_verbatim() {
        let state = AudioState::Absent {
            reason: "hw:8 is busy: another client holds the card".to_string(),
        };
        assert_eq!(state.label(), "hw:8 is busy: another client holds the card");
    }
}
