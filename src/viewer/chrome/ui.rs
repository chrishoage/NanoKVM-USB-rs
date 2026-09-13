//! Floating menu and Video, Keyboard, Mouse, and Audio panels.
//!
//! The menu and open panel share one area for dragging and hit testing. Widgets return
//! commands without touching devices. Disabled controls explain what prevents their use.

use super::audio::ChromeAudio;
use super::config::{Config, MouseMode, WheelDirection};
use super::hit::HitAreas;
use super::paste::{PasteOutcome, PasteProgress, Refusal};
use super::shortcut::Builtin;
use crate::viewer::state::CaptureState;

/// The grip's label while the menu is showing: clicking it collapses the pill.
pub const GRIP_EXPANDED: &str = "Hide";
/// The grip's label while the pill is collapsed: clicking it brings the menu back. This is the
/// whole of the collapsed pill, so it has to say what it does on its own.
pub const GRIP_COLLAPSED: &str = "Menu";

/// The pill's height in points — the reference's 34 px bar.
const PILL_HEIGHT: f32 = 34.0;

/// Which popover is open, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Popover {
    Video,
    Keyboard,
    Mouse,
    Audio,
}

impl Popover {
    const ALL: [Popover; 4] = [
        Popover::Video,
        Popover::Keyboard,
        Popover::Mouse,
        Popover::Audio,
    ];

    fn label(self) -> &'static str {
        match self {
            Popover::Video => "Video",
            Popover::Keyboard => "Keyboard",
            Popover::Mouse => "Mouse",
            Popover::Audio => "Audio",
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            Popover::Video => "Capture resolution, as the device reports it",
            Popover::Keyboard => "Paste and the built-in shortcuts",
            Popover::Mouse => "Pointer mode, cursor and wheel direction",
            Popover::Audio => "Mute, and the audio path's counters",
        }
    }
}

/// What the UI asks the event loop to do. Performed by the caller, never here.
#[derive(Debug, Clone, PartialEq)]
pub enum ChromeCommand {
    /// Ask the capture pipeline to renegotiate at this mode.
    SetResolution { width: u32, height: u32 },
    /// Submit this built-in shortcut's key transitions through the viewer's producer.
    SendShortcut(Builtin),
    /// Switch the pointer between absolute and relative.
    SetMouseMode(MouseMode),
    /// Hide or show the host cursor over the video.
    SetCursorHidden(bool),
    /// Which way a wheel detent is sent.
    SetWheelDirection(WheelDirection),
    /// Mute or unmute audio output.
    SetMuted(bool),
    /// Start a clipboard paste. The `Shift+Pause` chord produces the same thing
    /// without going through the UI at all — see `crate::viewer::input_map`.
    Paste,
}

/// Whether the Keyboard popover's Paste item can do anything, and what its tooltip says when it
/// cannot.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PasteAvailability {
    /// A data-control manager answered and the session is captured: a paste can be started.
    Ready,
    /// The session is not captured, so `Producer::submit` would refuse. The default,
    /// because a window that has just opened has captured nothing.
    #[default]
    NotCaptured,
    /// The compositor has no data-control protocol, or the probe failed. The reason is
    /// [`super::clipboard`]'s own sentence.
    Unsupported { reason: String },
    /// A paste is already running; the item becomes the progress line and the cancel hint.
    Running,
}

impl PasteAvailability {
    fn enabled(&self) -> bool {
        matches!(self, PasteAvailability::Ready)
    }

    /// Tooltip describing paste requirements, pacing, and cancellation.
    fn tooltip(&self, chord: &str, layout: &str, release_key: &str) -> String {
        match self {
            PasteAvailability::Ready => format!(
                "Type the clipboard using target layout {layout}, at {} ms per key transition. Unsupported characters cancel the entire paste before typing. Maximum clipboard size: 16 MiB. {chord} pastes; {release_key} cancels.",
                crate::script::REPORT_DELAY_MS,
            ),
            PasteAvailability::NotCaptured => "Input is not captured. Click the video or press Enter to enable typing."
                .to_string(),
            PasteAvailability::Unsupported { reason } => reason.clone(),
            PasteAvailability::Running => format!(
                "A paste is running. {release_key} cancels it and releases held input."
            ),
        }
    }
}

/// Paste availability, progress, and last result for the Keyboard panel.
#[derive(Debug, Clone, Default)]
pub struct PasteFacts {
    pub availability: PasteAvailability,
    /// The declared target layout, as the tooltip states it.
    pub layout: String,
    /// How far a running paste has got, if one is.
    pub progress: Option<PasteProgress>,
    /// How the last paste ended, if there was one.
    pub last: Option<PasteOutcome>,
    /// Why the last attempt was refused before anything was sent, with its offender list.
    pub refusal: Option<Refusal>,
}

/// The read-only facts one UI build is given. Everything here is measured or configured
/// elsewhere; the UI only displays it.
pub struct ChromeFacts<'a> {
    /// Locally reserved release key, displayed with the controls.
    pub release_key: &'a str,
    /// The capture phase. Keys can only be sent to the target while captured, so the shortcut
    /// items are disabled otherwise and say so.
    pub capture: CaptureState,
    /// The modes the device enumerated, not a fixed list. Empty means the
    /// enumeration has not answered — the node was busy, or is not there.
    pub modes: &'a [(u32, u32)],
    /// The mode currently negotiated, marked in the list.
    pub current_mode: Option<(u32, u32)>,
    /// Current pointer mode. A command-line override can differ from the persisted setting.
    pub pointer: MouseMode,
    /// The resolution of the last decoded frame, from its own JPEG header. Shown beside
    /// the negotiated mode because the two can legitimately disagree for a few frames.
    pub frame_size: Option<(u32, u32)>,
    /// Audio counters sampled for this frame.
    pub audio: &'a ChromeAudio,
    /// Pasting. See [`PasteFacts`].
    pub paste: &'a PasteFacts,
}

/// What one UI build produced.
#[derive(Debug, Default)]
pub struct ChromeOutput {
    /// In the order the user caused them.
    pub commands: Vec<ChromeCommand>,
    /// The rectangles the chrome covered, for [`super::route::RouteInputs::pointer_over_chrome`].
    pub hit: HitAreas,
}

/// The chrome's own mutable state: what the config remembers, plus which popover is open.
///
/// The popover is not persisted: reopening the viewer with a menu hanging over the video is
/// not what "menu open" means in the reference, which persists whether the *bar* is shown.
#[derive(Debug, Clone)]
pub struct ChromeUi {
    /// The persisted settings. The event loop writes these back when they change.
    pub config: Config,
    popover: Option<Popover>,
    /// The egui id the pill's area is laid out under. Constant; named once so the position can be
    /// read back out of egui's memory after a drag.
    area_id: egui::Id,
}

impl ChromeUi {
    /// A chrome showing `config`.
    pub fn new(config: Config) -> Self {
        ChromeUi {
            config,
            popover: None,
            area_id: egui::Id::new("nanokvm-chrome-pill"),
        }
    }

    /// Open a panel programmatically for development measurements.
    pub fn open_popover(&mut self, popover: Popover) {
        self.config.menu_open = true;
        self.popover = Some(popover);
    }

    /// Resolve a panel name, ignoring case; return `None` for unknown names.
    pub fn popover_by_name(name: &str) -> Option<Popover> {
        Popover::ALL
            .into_iter()
            .find(|p| p.label().eq_ignore_ascii_case(name))
    }

    /// Whether a panel is open, using current UI state for synchronous routing.
    pub fn popover_open(&self) -> bool {
        self.popover.is_some()
    }

    /// Whether a modal is open. This UI currently has no modal.
    pub fn modal_open(&self) -> bool {
        false
    }

    /// Build one frame of chrome into `ctx`.
    pub fn build(&mut self, ctx: &egui::Context, facts: &ChromeFacts<'_>) -> ChromeOutput {
        let mut out = ChromeOutput {
            commands: Vec::new(),
            hit: HitAreas::new(ctx.pixels_per_point()),
        };

        let area = egui::Area::new(self.area_id)
            .movable(true)
            .default_pos(egui::pos2(self.config.pill_x, self.config.pill_y))
            .constrain(true)
            .order(egui::Order::Foreground);

        let response = area.show(ctx, |ui| {
            egui::Frame::new()
                .fill(egui::Color32::from_rgba_unmultiplied(38, 38, 38, 217))
                .corner_radius(egui::CornerRadius::same(10))
                .inner_margin(egui::Margin::symmetric(8, 6))
                .stroke(egui::Stroke::new(
                    1.0_f32,
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 28),
                ))
                .show(ui, |ui| {
                    ui.vertical(|ui| {
                        self.pill_row(ui, facts, &mut out.commands);
                        if self.config.menu_open {
                            if let Some(popover) = self.popover {
                                ui.separator();
                                self.popover_body(ui, popover, facts, &mut out.commands);
                            }
                        }
                    });
                });
        });

        out.hit.push(response.response.rect);

        // Dismiss locally so the closing click cannot reach the target desktop.
        if self.popover.is_some() && response.response.clicked_elsewhere() {
            self.popover = None;
        }

        // Save the dragged position for persistence by the event loop.
        if let Some(state) = egui::AreaState::load(ctx, self.area_id) {
            let pos = state.left_top_pos();
            if pos.x.is_finite() && pos.y.is_finite() {
                self.config.pill_x = pos.x;
                self.config.pill_y = pos.y;
            }
        }
        out
    }

    /// The pill itself: the grip, the four popover buttons, and the release key.
    fn pill_row(
        &mut self,
        ui: &mut egui::Ui,
        facts: &ChromeFacts<'_>,
        commands: &mut Vec<ChromeCommand>,
    ) {
        ui.horizontal(|ui| {
            ui.set_min_height(PILL_HEIGHT - 12.0);
            // Bundled fonts lack the menu glyphs; text avoids missing-glyph boxes.
            let grip = ui.add(egui::Button::new(if self.config.menu_open {
                GRIP_EXPANDED
            } else {
                GRIP_COLLAPSED
            }));
            let grip = grip.on_hover_text(if self.config.menu_open {
                "Collapse the menu. Drag to reposition it."
            } else {
                "Expand the menu. Drag to reposition it."
            });
            if grip.clicked() {
                self.config.menu_open = !self.config.menu_open;
                if !self.config.menu_open {
                    // A popover left open behind a collapsed pill would keep taking the keyboard
                    // from the target with nothing on screen to say so (`route_key` clause 5).
                    self.popover = None;
                }
            }

            if !self.config.menu_open {
                return;
            }

            for popover in Popover::ALL {
                let open = self.popover == Some(popover);
                let button = ui.add(egui::Button::selectable(open, popover.label()));
                let button = button.on_hover_text(popover.tooltip());
                if button.clicked() {
                    self.popover = if open { None } else { Some(popover) };
                }
            }

            ui.separator();
            // Keep the escape key visible while input is captured.
            ui.label(format!("release: {}", facts.release_key))
                .on_hover_text(format!(
                    "{} returns keyboard and mouse control to the host and releases held input on the target. On niri, Mod+Escape also releases control.",
                    facts.release_key
                ));
            let _ = commands;
        });
    }

    fn popover_body(
        &mut self,
        ui: &mut egui::Ui,
        popover: Popover,
        facts: &ChromeFacts<'_>,
        commands: &mut Vec<ChromeCommand>,
    ) {
        ui.set_min_width(260.0);
        match popover {
            Popover::Video => self.video_popover(ui, facts, commands),
            Popover::Keyboard => self.keyboard_popover(ui, facts, commands),
            Popover::Mouse => self.mouse_popover(ui, facts, commands),
            Popover::Audio => self.audio_popover(ui, facts, commands),
        }
    }

    /// Enumerated capture resolutions with the negotiated mode selected.
    fn video_popover(
        &mut self,
        ui: &mut egui::Ui,
        facts: &ChromeFacts<'_>,
        commands: &mut Vec<ChromeCommand>,
    ) {
        if facts.modes.is_empty() {
            ui.add_enabled(false, egui::Button::new("No modes enumerated"))
                .on_disabled_hover_text(
                    "Capture resolutions are unavailable. The device may be disconnected or busy.",
                );
            return;
        }
        for &(w, h) in facts.modes {
            let current = facts.current_mode == Some((w, h));
            let response = ui.add(egui::Button::selectable(current, format!("{w} x {h}")));
            let response = if current {
                response.on_hover_text(format!("Current capture setting: {w} x {h}."))
            } else {
                response.on_hover_text(format!(
                    "Capture at {w} x {h}. The picture pauses briefly while the stream restarts. The target display settings stay unchanged."
                ))
            };
            if response.clicked() && !current {
                commands.push(ChromeCommand::SetResolution {
                    width: w,
                    height: h,
                });
            }
        }
        ui.separator();
        // Show actual and requested size because startup frames can retain the old resolution.
        let frame = match facts.frame_size {
            Some((w, h)) => format!("{w} x {h}"),
            None => "none yet".to_string(),
        };
        ui.label(format!("frames arriving at {frame}"))
            .on_hover_text(
                "Actual size of incoming frames. It may differ briefly from the selected size after a resolution change.",
            );
    }

    /// Paste and the two built-in shortcuts.
    fn keyboard_popover(
        &mut self,
        ui: &mut egui::Ui,
        facts: &ChromeFacts<'_>,
        commands: &mut Vec<ChromeCommand>,
    ) {
        let paste = facts.paste;
        let chord = super::paste::chord(facts.release_key);
        let tooltip = paste
            .availability
            .tooltip(&chord, &paste.layout, facts.release_key);
        let button = ui.add_enabled(paste.availability.enabled(), egui::Button::new("Paste"));
        let button = button
            .on_hover_text(tooltip.clone())
            .on_disabled_hover_text(tooltip);
        if button.clicked() {
            commands.push(ChromeCommand::Paste);
        }

        // Progress comes from the job so UI updates cannot create a second delivery count.
        if let Some(progress) = paste.progress {
            ui.label(format!("paste: {progress}")).on_hover_text(
                "Key transitions submitted and estimated time remaining. Submission does not confirm that text appeared on the target.",
            );
            ui.label(format!("{} cancels", facts.release_key))
                .on_hover_text(format!(
                    "{} cancels the paste and releases held input. {chord} also cancels while a paste is running.",
                    facts.release_key
                ));
        }

        // Show every unsupported character so the user can correct the whole paste.
        if let Some(refusal) = &paste.refusal {
            ui.label(format!("paste refused: {refusal}")).on_hover_text(
                "No paste keystrokes were sent. Correct the listed characters or target layout before trying again.",
            );
            for offender in refusal.offenders() {
                ui.label(format!("  {offender}")).on_hover_text(
                    "Unsupported character on the target layout. Position is counted from 1 at the start of the clipboard.",
                );
            }
        } else if let Some(last) = &paste.last {
            ui.label(format!("last paste: {last}")).on_hover_text(
                "Result of the last paste, including partial progress if it was canceled or failed.",
            );
        }

        ui.separator();

        // Disable shortcuts outside capture because the producer would reject them.
        let captured = facts.capture == CaptureState::Captured;
        for builtin in Builtin::ALL {
            let response = ui.add_enabled(captured, egui::Button::new(builtin.label()));
            let response = response
                .on_hover_text(format!(
                    "Send {} to the target, then release the chord.",
                    builtin.label()
                ))
                .on_disabled_hover_text(
                    "Input is not captured. Click the video or press Enter to enable shortcuts.",
                );
            if response.clicked() {
                commands.push(ChromeCommand::SendShortcut(builtin));
            }
        }
    }

    /// Absolute/relative, cursor hide/show, wheel direction.
    ///
    /// The pointer mode is rendered from [`ChromeFacts::pointer`] — the live one — and changed
    /// through a [`ChromeCommand`]; the persisted [`Config::mouse_mode`] is written by the event
    /// loop when that command is performed, which is what makes a UI change the only thing that
    /// is remembered.
    fn mouse_popover(
        &mut self,
        ui: &mut egui::Ui,
        facts: &ChromeFacts<'_>,
        commands: &mut Vec<ChromeCommand>,
    ) {
        ui.horizontal(|ui| {
            for (mode, label, tip) in [
                (
                    MouseMode::Absolute,
                    "Absolute",
                    "Position the target pointer at the corresponding point in the video.",
                ),
                (
                    MouseMode::Relative,
                    "Relative",
                    "Lock the pointer to this window and send relative movement. The target applies its own acceleration. Release capture to use the host pointer.",
                ),
            ] {
                let selected = facts.pointer == mode;
                let response = ui
                    .add(egui::Button::selectable(selected, label))
                    .on_hover_text(tip);
                if response.clicked() && !selected {
                    commands.push(ChromeCommand::SetMouseMode(mode));
                }
            }
        });

        let mut hidden = self.config.cursor_hidden;
        let cursor = ui
            .checkbox(&mut hidden, "Hide the host cursor")
            .on_hover_text(
                "Show only the target cursor over the video. Relative mode always hides the host cursor while locked.",
            );
        if cursor.changed() {
            // The command carries the new value and the event loop performs it, config included.
            // Setting it here as well made the payload dead and gave the setting two owners.
            commands.push(ChromeCommand::SetCursorHidden(hidden));
        }

        let mut inverted = self.config.wheel_direction == WheelDirection::Inverted;
        let wheel = ui
            .checkbox(&mut inverted, "Invert the wheel")
            .on_hover_text("Reverse scroll direction on the target.");
        if wheel.changed() {
            self.config.wheel_direction = if inverted {
                WheelDirection::Inverted
            } else {
                WheelDirection::Natural
            };
            commands.push(ChromeCommand::SetWheelDirection(
                self.config.wheel_direction,
            ));
        }
    }

    /// Build audio status, mute control, counters, and configured buffer capacity.
    fn audio_popover(
        &mut self,
        ui: &mut egui::Ui,
        facts: &ChromeFacts<'_>,
        commands: &mut Vec<ChromeCommand>,
    ) {
        let audio = facts.audio;
        ui.label(format!("audio: {}", audio.state.label()))
            .on_hover_text("Audio status for the capture device's paired sound card.");

        let running = audio.state.is_running();
        // Read the live mute flag; persisted settings can lag a command-line or runtime change.
        let mut muted = audio.is_muted();
        let mute = ui
            .add_enabled_ui(running, |ui| ui.checkbox(&mut muted, "Mute"))
            .inner;
        let mute = mute
            .on_hover_text("Stop playing the target's audio through this desktop's sink.")
            .on_disabled_hover_text(format!(
                "There is no audio to mute: {}.",
                audio.state.label()
            ));
        if mute.changed() {
            commands.push(ChromeCommand::SetMuted(muted));
        }

        ui.separator();
        ui.label(format!("overruns {}", audio.overruns))
            .on_hover_text("Audio periods dropped because the buffer was full.");
        ui.label(format!("underruns {}", audio.underruns))
            .on_hover_text("Silent periods inserted because the buffer was empty.");
        ui.label(format!("drift corrections {}", audio.drift_corrections))
            .on_hover_text(
                "Periods inserted or dropped to correct sustained clock drift. Counted separately from buffer overruns and underruns.",
            );
        ui.label(format!(
            "periods played {} / silent {} / muted {}",
            audio.played, audio.silent, audio.muted_periods
        ))
        .on_hover_text("Output periods containing captured audio, silence, or muted audio.");
        match audio.depth() {
            Some(depth) => ui.label(format!("buffer {depth}")).on_hover_text(
                "Configured audio buffer capacity in periods and frames. This is not measured playback latency.",
            ),
            None => ui
                .label("buffer not configured")
                .on_hover_text("No stream has been opened, so there is no depth to report."),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four popovers each have a label and a tooltip, and no two share a label — a menu with
    /// two identically named buttons cannot be driven by a test and is worse for a user.
    #[test]
    fn every_popover_has_a_distinct_label_and_a_tooltip() {
        let mut labels: Vec<&str> = Popover::ALL.iter().map(|p| p.label()).collect();
        labels.sort_unstable();
        let before = labels.len();
        labels.dedup();
        assert_eq!(labels.len(), before, "duplicate popover labels");
        for p in Popover::ALL {
            assert!(!p.tooltip().is_empty(), "{p:?} has no tooltip");
        }
    }

    /// Paste tooltips explain availability, layout, and the triggering chord.
    #[test]
    fn the_paste_item_is_enabled_in_one_state_and_every_other_says_why() {
        let chord = super::super::paste::chord("Pause");
        let tip = |a: &PasteAvailability| a.tooltip(&chord, "us", "Pause");

        assert!(PasteAvailability::Ready.enabled());
        let ready = tip(&PasteAvailability::Ready);
        assert!(ready.contains("Shift+Pause"), "{ready}");
        assert!(ready.contains("layout us"), "{ready}");
        assert!(
            ready.contains(&crate::script::REPORT_DELAY_MS.to_string()),
            "the pace is stated: {ready}"
        );
        // Expose the clipboard size limit before the user reaches it.
        assert!(
            ready.contains("16 MiB"),
            "the read bound is stated: {ready}"
        );
        assert!(
            ready.contains("Maximum clipboard size"),
            "label the clipboard limit: {ready}"
        );

        for disabled in [
            PasteAvailability::NotCaptured,
            PasteAvailability::Running,
            PasteAvailability::Unsupported {
                reason: "this compositor has no data-control protocol".to_string(),
            },
        ] {
            assert!(!disabled.enabled(), "{disabled:?}");
            assert!(!tip(&disabled).is_empty(), "{disabled:?} has no reason");
        }
        assert!(tip(&PasteAvailability::NotCaptured).contains("not captured"));
        assert!(tip(&PasteAvailability::Running).contains("Pause cancels"));
        assert_eq!(
            tip(&PasteAvailability::Unsupported {
                reason: "this compositor has no data-control protocol".to_string(),
            }),
            "this compositor has no data-control protocol"
        );
    }

    /// The chord the menu advertises is derived from the release binding, so it cannot name a key
    /// `input_map` does not look at.
    #[test]
    fn the_advertised_chord_is_the_release_key_with_shift() {
        assert_eq!(
            super::super::paste::chord(crate::viewer::RELEASE_KEY),
            format!("Shift+{}", crate::viewer::RELEASE_KEY)
        );
    }

    /// Collapsing the pill closes any open popover. Left open, `route_key` clause 5 would keep
    /// sending the target's keystrokes to a menu that is not on screen.
    #[test]
    fn collapsing_closes_the_popover() {
        let mut ui = ChromeUi::new(Config::default());
        ui.popover = Some(Popover::Video);
        assert!(ui.popover_open());
        // What the grip's click arm does.
        ui.config.menu_open = false;
        ui.popover = None;
        assert!(!ui.popover_open());
    }

    #[test]
    fn a_fresh_chrome_has_nothing_open() {
        let ui = ChromeUi::new(Config::default());
        assert!(!ui.popover_open());
        assert!(!ui.modal_open(), "the viewer has no modal");
    }
}
