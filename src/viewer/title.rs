//! Window-title status for input, capture, release delivery, and audio.
//!
//! The title remains visible when the menu is collapsed. Stalls, disconnections, and size
//! mismatches have distinct messages because they require different recovery actions.

use std::time::Duration;

use crate::audio::{RingConfig, RingCounts};
use crate::capture::PipelineState;
use crate::viewer::state::Session;
use crate::viewer::RELEASE_KEY;

/// The title's fixed part.
pub const APP_TITLE: &str = "NanoKVM-USB";

/// What the title says about audio.
///
/// Three states, because those are the three things a user can do something about: it is off
/// because they asked for it to be off, it is on (possibly muted), or it is unavailable and the
/// reason is the actionable part — a card another process holds is fixed differently from a
/// dongle that is not plugged in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AudioTitle<'a> {
    /// `--no-audio`. Nothing was opened and no thread is running.
    Off,
    /// Running. The counters are shown only when one of them is nonzero, so a healthy session
    /// says nothing about them — the same rule the rest of this title follows.
    On {
        muted: bool,
        counts: RingCounts,
        /// Configured buffer capacity, not measured audio latency.
        ring: RingConfig,
    },
    /// Not running, and why. The reason is a borrowed condition message, so the wording comes
    /// from [`crate::audio::AudioError`] and this function invents none of it.
    Unavailable(&'a str),
    /// Audio open or first transfer is pending. Overdue attempts become unavailable
    /// through audio snapshot supervision.
    Opening(&'a str),
}

/// How long frames must be absent before the title says so. Matches the pipeline's
/// own stall threshold, so the title and [`PipelineState::Stalled`] change together.
pub const STALL_NOTICE: Duration = Duration::from_millis(500);

/// Everything the title is a function of, measured by the caller.
#[derive(Debug, Clone, Copy)]
pub struct TitleFacts<'a> {
    /// Selected presentation mode for diagnostics.
    pub present_mode: &'a str,
    /// The capture session: its phase, and why the last one ended.
    pub session: Session,
    /// What the capture pipeline is doing.
    pub pipeline: PipelineState,
    /// How long the capture device has been gone, while it is being reopened.
    pub disconnected_for: Option<Duration>,
    /// How long since the last frame, or `None` if none has ever arrived.
    pub frames_stalled_for: Option<Duration>,
    /// [`crate::input::Stats::link_down`].
    pub link_down: bool,
    /// How long the serial link has been down (`Stats::down_since`).
    pub link_down_for: Option<Duration>,
    /// [`crate::input::Stats::reconnect_attempts`].
    pub reconnect_attempts: u64,
    /// Whether the last release-all failed to reach the target.
    pub release_unsent: bool,
    /// [`crate::capture::PipelineStats::format_mismatch_accepted`] is nonzero: the capture
    /// watchdog has, at least once, given up on establishing the negotiated mode.
    pub format_mismatch_accepted: bool,
    /// [`crate::capture::PipelineStats::last_resolution`] — the size of the last frame, from its
    /// own SOF header.
    pub video_size: Option<(u32, u32)>,
    /// [`crate::capture::PipelineStats::negotiated_dimensions`] — what `S_FMT` committed to.
    pub negotiated_size: Option<(u32, u32)>,
    /// What audio is doing.
    pub audio: AudioTitle<'a>,
}

/// The window title for these facts.
pub fn compose(f: &TitleFacts) -> String {
    let mut title = format!(
        "{APP_TITLE} — {} — {}",
        f.present_mode,
        f.session.capture().title_fragment(RELEASE_KEY)
    );

    match f.pipeline {
        // Report capture recovery and outage duration.
        PipelineState::Reconnecting => {
            title.push_str(" — capture device gone, reconnecting");
            if let Some(since) = f.disconnected_for {
                title.push_str(&format!(" ({}s)", since.as_secs()));
            }
        }
        // The moment between noticing the loss and starting to retry: sub-millisecond in the
        // common case, and honest while it lasts — nothing is being attempted yet.
        PipelineState::Disconnected => title.push_str(" — capture device disconnected"),
        PipelineState::Stopped => title.push_str(" — capture stopped"),
        PipelineState::Running | PipelineState::Stalled => match f.frames_stalled_for {
            Some(since) if since >= STALL_NOTICE => {
                title.push_str(&format!(" — frames stopped {}s ago", since.as_secs()));
            }
            Some(_) => {}
            None => title.push_str(" — waiting for the first frame"),
        },
    }

    if f.link_down {
        title.push_str(" — serial DOWN, reconnecting");
        match f.link_down_for {
            Some(since) => title.push_str(&format!(
                " ({}s, {} attempts)",
                since.as_secs(),
                f.reconnect_attempts
            )),
            // `down_since` and `link_down` come out of one critical section, so this is the
            // startup case — down since before there was anything to time.
            None => title.push_str(&format!(" ({} attempts)", f.reconnect_attempts)),
        }
    }

    if let Some(notice) = f.session.notice() {
        title.push_str(" — ");
        title.push_str(notice.title_fragment());
    }

    // Require both exhausted recovery and a current mismatch. A cumulative
    // counter alone would keep the warning after the size recovered.
    if f.format_mismatch_accepted {
        if let (Some(video), Some(negotiated)) = (f.video_size, f.negotiated_size) {
            if video != negotiated {
                title.push_str(&format!(
                    " — video {}x{}, negotiated {}x{} not established",
                    video.0, video.1, negotiated.0, negotiated.1
                ));
            }
        }
    }

    title.push_str(&audio_fragment(&f.audio));

    if f.release_unsent {
        title.push_str(" — release UNSENT — target may still hold keys");
    }

    title
}

/// Format audio state, nonzero counters, and configured capacity. Disabled audio
/// shows only the off state.
fn audio_fragment(audio: &AudioTitle) -> String {
    match audio {
        AudioTitle::Off => " — audio off".to_string(),
        AudioTitle::Unavailable(why) => format!(" — audio unavailable: {why}"),
        AudioTitle::Opening(side) => format!(" — audio opening ({side})…"),
        AudioTitle::On {
            muted,
            counts,
            ring,
        } => {
            let state = if *muted { "audio muted" } else { "audio on" };
            // Label buffer capacity as configured so it cannot be mistaken for measured latency.
            let mut fragment = format!(
                " — {state} (buffer {}x{} frames configured)",
                ring.periods, ring.period_frames
            );
            // Include nonzero failure counters without cluttering healthy status.
            if counts.any() {
                fragment.push_str(&format!(
                    ", overruns {}, underruns {}, drift {}/{}",
                    counts.overruns, counts.underruns, counts.drift_drops, counts.drift_inserts
                ));
            }
            fragment
        }
    }
}
