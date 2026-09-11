//! The window title, as a pure function of what is currently true (plan §2.6.1, §2.8, §6.1,
//! §12 Stage 1).
//!
//! The title is this client's whole user interface for everything that is not a pixel: whether
//! input is being forwarded, how to get out, whether frames stopped, whether the transport is
//! gone, and whether a release-all failed to reach the target. §2.8 requires those conditions to
//! be *surfaced* rather than counted, and §6.1 requires them to be told apart — which means the
//! wording is behaviour, not decoration, and it needs tests that fail when it regresses.
//!
//! So the composition lives here, with no window, no clock and no handle in scope: `App` measures
//! the facts and this turns them into a string. Every elapsed time arrives as a [`Duration`]
//! already, because a function that read the clock could not be asserted against.
//!
//! # The distinctions the wording has to keep
//!
//! - **A stall is not a disconnection (§6.1 S2-1).** Frames stopping while the device is still
//!   there is "frames stopped N s ago"; the device going away is "capture device gone,
//!   reconnecting". Different faults, different remedies — one is the target's output or the
//!   capture chip, the other is a cable — and a title that said "no video" to both would be
//!   exactly the conflation §6.1 forbids.
//! - **Neither ever claims the signal is gone.** This hardware cannot report that, and pixel
//!   content is never evidence of it (A5).
//! - **"Down" is not "gone" any more (§2.7).** A serial link that went away is being reopened,
//!   and the title says so, with how long it has been trying — the number that tells a user
//!   whether to go and look at the cable.
//! - **A session that ended by failure says why (§2.8).** See [`super::state::Notice`].
//! - **An accepted format mismatch is a condition, not a counter (C5).** When the capture
//!   watchdog has given up on getting the device into the mode `S_FMT` negotiated, the window is
//!   showing a 640x480 image on a 1080p session for the rest of the run. That is more visible to
//!   the user than to the log, so it belongs here, next to everything else that is wrong — and
//!   it goes away by itself the moment a frame at the negotiated size arrives, because the
//!   fragment is a function of the two sizes rather than of the counter that got them compared.

use std::time::Duration;

use crate::capture::PipelineState;
use crate::viewer::state::Session;
use crate::viewer::RELEASE_KEY;

/// The title's fixed part.
pub const APP_TITLE: &str = "NanoKVM-USB";

/// How long frames must be absent before the title says so (§6.1 S1-2). Matches the pipeline's
/// own stall threshold, so the title and [`PipelineState::Stalled`] change together.
pub const STALL_NOTICE: Duration = Duration::from_millis(500);

/// Everything the title is a function of, measured by the caller.
#[derive(Debug, Clone, Copy)]
pub struct TitleFacts<'a> {
    /// The wgpu present mode, for the §5.4 measurement it belongs to.
    pub present_mode: &'a str,
    /// The capture session: its phase, and why the last one ended (§2.8).
    pub session: Session,
    /// What the capture pipeline is doing (§6.1).
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
    /// Whether the last release-all failed to reach the target (§2.6.1).
    pub release_unsent: bool,
    /// [`crate::capture::PipelineStats::format_mismatch_accepted`] is nonzero: the capture
    /// watchdog has, at least once, given up on establishing the negotiated mode.
    pub format_mismatch_accepted: bool,
    /// [`crate::capture::PipelineStats::last_resolution`] — the size of the last frame, from its
    /// own SOF header (A6).
    pub video_size: Option<(u32, u32)>,
    /// [`crate::capture::PipelineStats::negotiated_dimensions`] — what `S_FMT` committed to.
    pub negotiated_size: Option<(u32, u32)>,
}

/// The window title for these facts.
pub fn compose(f: &TitleFacts) -> String {
    let mut title = format!(
        "{APP_TITLE} — {} — {}",
        f.present_mode,
        f.session.capture().title_fragment(RELEASE_KEY)
    );

    match f.pipeline {
        // §6.1 S2-4: the client is doing something about it, and for how long.
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

    // C5: the watchdog gave up *and* the frames are still the wrong size. Both halves matter —
    // the counter alone would keep saying so long after the device came right, and the sizes
    // alone would flag A6's benign transient on every reopen.
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

    if f.release_unsent {
        title.push_str(" — release UNSENT — target may still hold keys");
    }

    title
}
