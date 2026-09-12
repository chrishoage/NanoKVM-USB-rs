//! What the chrome needs to know about audio, and nothing more (plan §12 Stage 4a, 4b).
//!
//! 4a owns the ALSA threads, the ring and the counters; 4b owns the section of the Mouse-and-Audio
//! popover that shows them and the mute toggle that drives them. This is the seam between the two,
//! and it is deliberately a **snapshot of plain data plus one shared flag**:
//!
//! - the chrome must never block on the audio threads, exactly as it must never block on the
//!   writer (§2.9), so everything it reads is copied out under 4a's own lock and handed over as
//!   [`ChromeAudio`];
//! - the one thing that goes the other way is the mute flag, which is an `AtomicBool` the playback
//!   thread reads per period — a channel for a boolean the user toggles twice a session would be
//!   ceremony.
//!
//! **4b was built against this rather than against 4a's real `audio::Status`, which is in another
//! tree.** The merge replaces the one place [`ChromeAudio`] is *constructed* — `App::audio` in
//! `viewer::app`, which today returns the default — with a read of 4a's real `audio::Status`.
//! Nothing in `ui.rs` changes, because nothing in `ui.rs` knows where the numbers came from.

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
    /// Deliberately not started — `--no-audio`.
    Off,
    /// A side is between "about to open its device" and "moved its first period" (4a's hardware
    /// defect D2). Carries the side's own name, `capture` or `playback`.
    ///
    /// It exists because the alternative is worse than useless: an open that never returns logs
    /// nothing, raises no condition and leaves the counters at zero, so the popover would say
    /// "on" about a stream that has never existed. Past 4a's supervision limit this state turns
    /// itself into an [`AudioState::Absent`] with 4a's own wording.
    Opening { side: String },
    /// It should be running and is not. The reason is 4a's own words: `EBUSY` (another client
    /// holds the card), a card that is not there, a card that went away, a playback sink that
    /// vanished. Shown verbatim, because "audio unavailable" without the cause is the kind of
    /// message that sends someone to the wrong place.
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

    /// Whether the mute toggle can do anything. Muting a stream that is not running would be a
    /// control with no effect, and §12 Stage 4b requires a disabled control's tooltip to say why.
    ///
    /// [`AudioState::Opening`] is **not** running: there is no stream yet, and a checkbox that
    /// took effect on a device that may never open is a control that lies about what it did.
    pub fn is_running(&self) -> bool {
        matches!(self, AudioState::On | AudioState::Muted)
    }
}

/// One reading of the audio path, taken on the event loop and handed to the UI build.
#[derive(Debug, Clone)]
pub struct ChromeAudio {
    /// What it is doing.
    pub state: AudioState,
    /// Periods dropped because the ring was full — the capture clock is ahead of playback (4a).
    pub overruns: u64,
    /// Periods of silence played because the ring was empty — playback is ahead of capture.
    pub underruns: u64,
    /// Periods inserted or dropped at the ring's bounds to absorb the two crystals' drift. 4a
    /// counts this separately from the two above because it is the *expected* correction rather
    /// than a failure, and a number that mixes them cannot be read.
    pub drift_corrections: u64,
    /// Periods of real audio written to the sink (4a's `periods_played`).
    pub played: u64,
    /// Periods written as silence because the ring had nothing to give. Shown beside `played`
    /// rather than folded into it: a session that "played" 30 000 periods of which 29 000 were
    /// silence is a broken one, and one counter says it went perfectly.
    pub silent: u64,
    /// Periods written as silence because the user muted it. Not a failure and not audio, so it
    /// is neither of the two above.
    pub muted_periods: u64,
    /// The **configured** buffer depth, as periods. §5.5: report the configured depth with its
    /// provenance, never an end-to-end latency this desk cannot measure.
    pub periods: u32,
    /// Frames per period, the other half of the depth.
    pub frames_per_period: u32,
    /// The playback thread's mute flag. Shared, so the toggle takes effect on the next period
    /// rather than on the next chrome frame.
    pub mute: Arc<AtomicBool>,
}

impl Default for ChromeAudio {
    /// Absent, with the reason a viewer built before 4a landed would truthfully give.
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
    /// Build the chrome's reading from 4a's own [`AudioSnapshot`] — **the seam, and the one
    /// place a `ChromeAudio` describing a running path is made.**
    ///
    /// A free function rather than a method on `App` so that it can be tested against a snapshot
    /// taken from a real [`crate::audio::AudioHandle`] rather than against a hand-written struct:
    /// the thing worth pinning is that 4a's counters arrive in the fields 4b renders, and a test
    /// that fills both sides in by hand pins nothing.
    ///
    /// The state is decided in the order a reader needs it:
    /// 1. **a condition** — 4a's own wording, verbatim, including the one the D2 supervisor
    ///    raises for an open that never returned;
    /// 2. **opening** — a side between "about to open" and "its first period" (D2);
    /// 3. **muted / on** — read from the shared flag, which is the live truth, not from the
    ///    persisted setting.
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
            // One number, because to a reader the two directions are one thing: how much the
            // policy had to do to keep the two clocks together. 4a keeps them apart in its own
            // counters and in the `stats audio:` line, which is where a drift *rate* is read off.
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
            "§5.5: say what it is: {depth}"
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
            "the flag the 4a thread reads"
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

    /// D2: while a side is opening the popover says so and names which side, and the mute
    /// toggle stays disabled — there is no stream to mute yet.
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
            "\"opening\" and \"on\" must not read the same: that is the whole of D2"
        );
    }

    /// An absent stream shows 4a's own words, not a generic phrase.
    #[test]
    fn an_absent_stream_shows_the_reason_verbatim() {
        let state = AudioState::Absent {
            reason: "hw:8 is busy: another client holds the card".to_string(),
        };
        assert_eq!(state.label(), "hw:8 is busy: another client holds the card");
    }
}
