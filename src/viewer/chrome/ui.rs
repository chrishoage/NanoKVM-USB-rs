//! The pill, its popovers, and the commands they produce (plan §12 Stage 4b).
//!
//! # Shape
//!
//! One floating, translucent, draggable pill over the video, collapsible to a grip, with four
//! click popovers drawn in the same [`egui::Area`] directly beneath it — Video, Keyboard, Mouse
//! and Audio. Drawing the popover inside the pill's own area rather than in a second one is what
//! makes the hit test a single rectangle the user can also drag as one object.
//!
//! **Every control carries a tooltip, and every disabled control's tooltip says why it is
//! disabled.** That is a requirement of §12 Stage 4b and it is asserted, control by control, by
//! `tests/viewer_chrome_ui.rs` driving this with `egui_kittest` — no window, no compositor.
//!
//! # Text, not icons
//!
//! The reference uses Lucide glyphs. This uses words. The pill is 34 px of vertical space and the
//! popovers are lists of named settings, where "Absolute" reads better than a glyph does; every
//! control has a mandatory tooltip anyway; and 4b's exit criterion is a *measured* per-frame cost
//! that a second font atlas only inflates. `egui_phosphor 0.11` pins `egui ^0.33` exactly and is
//! the one-line change if that is ever wanted.
//!
//! # This builds the UI; it does not perform anything
//!
//! Like [`crate::viewer::state`], the output is a list of [`ChromeCommand`]s for the event loop to
//! perform. Nothing here touches the producer, the pipeline or the config file.

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
    /// Start a clipboard paste (§12 Stage 4c). The `Shift+Pause` chord produces the same thing
    /// without going through the UI at all — see `crate::viewer::input_map`.
    Paste,
}

/// Whether the Keyboard popover's Paste item can do anything, and what its tooltip says when it
/// cannot (§12 Stage 4b: every disabled control's tooltip says why; §12 Stage 4c).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PasteAvailability {
    /// A data-control manager answered and the session is captured: a paste can be started.
    Ready,
    /// The session is not captured, so `Producer::submit` would refuse (§2.6). The default,
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

    /// The tooltip. For every disabled case it is the reason, which is what §12 Stage 4b requires
    /// of every disabled control. The enabled one names the chord and the layout because §12 Stage
    /// 4c asks for both: the chord so the trigger is discoverable without opening the menu, and
    /// the layout because §10.2's rule is that it is *stated*, never assumed.
    fn tooltip(&self, chord: &str, layout: &str, release_key: &str) -> String {
        match self {
            PasteAvailability::Ready => format!(
                "Type the clipboard into the target, one key every {} ms — the rate `nanokvm \
                 type` uses. Compiled against the declared target layout {layout}; a character \
                 that layout cannot reach refuses the whole paste and names it, before anything \
                 is sent (§10.2). There is no limit on how much may be pasted — the rate and the \
                 cancel are the limit — but a clipboard over 16 MiB is refused rather than read \
                 into memory. {chord} does the same without the menu; {release_key} cancels a \
                 running paste.",
                crate::script::REPORT_DELAY_MS,
            ),
            PasteAvailability::NotCaptured => "Input is not captured, so nothing can be typed. \
                 Click in the window, or press Enter, to capture."
                .to_string(),
            PasteAvailability::Unsupported { reason } => reason.clone(),
            PasteAvailability::Running => format!(
                "A paste is running. {release_key} cancels it and releases everything (§2.6)."
            ),
        }
    }
}

/// Everything the Keyboard popover shows about pasting.
///
/// A struct rather than five fields on [`ChromeFacts`] because they are one subject and because
/// the kittest tests build them directly.
#[derive(Debug, Clone, Default)]
pub struct PasteFacts {
    pub availability: PasteAvailability,
    /// The declared target layout, as the tooltip states it (§10.2).
    pub layout: String,
    /// How far a running paste has got, if one is.
    pub progress: Option<PasteProgress>,
    /// How the last paste ended, if there was one.
    pub last: Option<PasteOutcome>,
    /// Why the last attempt was refused before anything was sent, with its offender list (§10.2).
    pub refusal: Option<Refusal>,
}

/// The read-only facts one UI build is given. Everything here is measured or configured
/// elsewhere; the UI only displays it.
pub struct ChromeFacts<'a> {
    /// The viewer's release binding, spelled as the user presses it. §12 Stage 4b moves this from
    /// the title into the pill; the title keeps it too, because the title stays the fallback.
    pub release_key: &'a str,
    /// The capture phase. Keys can only be sent to the target while captured, so the shortcut
    /// items are disabled otherwise and say so.
    pub capture: CaptureState,
    /// The modes **the device enumerated**, not a fixed list (§12 Stage 4b). Empty means the
    /// enumeration has not answered — the node was busy, or is not there.
    pub modes: &'a [(u32, u32)],
    /// The mode currently negotiated, marked in the list.
    pub current_mode: Option<(u32, u32)>,
    /// **The live pointer mode**, marked in the Mouse popover.
    ///
    /// The pointer the event loop is actually driving, which is not the same thing as
    /// [`Config::mouse_mode`]: `--pointer rel` changes the live mode for one run and is never
    /// written back. Rendering the persisted value instead showed "Absolute" while the pointer was
    /// relative, and could then write that stale value to the file.
    pub pointer: MouseMode,
    /// The resolution of the last decoded frame, from its own JPEG header (§6, A6). Shown beside
    /// the negotiated mode because the two can legitimately disagree for a few frames.
    pub frame_size: Option<(u32, u32)>,
    /// 4a's counters, as of this frame.
    pub audio: &'a ChromeAudio,
    /// Pasting (§12 Stage 4c). See [`PasteFacts`].
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
/// The popover is **not** persisted: reopening the viewer with a menu hanging over the video is
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

    /// Open a popover without a click.
    ///
    /// The one caller is the hidden `--chrome-popover` development flag, which exists so the §12
    /// Stage 4b measurement *"the event-loop handling p50/p95 with a popover open"* can be taken
    /// on a desk where nothing can drive the pointer. It is not a user-facing feature and there is
    /// no matching "close": the grip and a second click on the button already do that.
    pub fn open_popover(&mut self, popover: Popover) {
        self.config.menu_open = true;
        self.popover = Some(popover);
    }

    /// Which popover the name `--chrome-popover` takes refers to, or `None` for a name that is not
    /// one. Here rather than in `main` so the flag cannot name a popover that does not exist.
    pub fn popover_by_name(name: &str) -> Option<Popover> {
        Popover::ALL
            .into_iter()
            .find(|p| p.label().eq_ignore_ascii_case(name))
    }

    /// Whether a popover is open — the routing rule's `popover_open`, read synchronously on the
    /// event loop rather than from egui's one-frame-old opinion.
    pub fn popover_open(&self) -> bool {
        self.popover.is_some()
    }

    /// 4b ships no modal (§12 Stage 4b puts the settings modal out of scope), so this is always
    /// false. It exists because the routing rule has the clause and a caller should not have to
    /// know which of the two flags is currently a constant.
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
            // Translucent, rounded, over the video: `bg-neutral-800/70` in the reference.
            egui::Frame::new()
                .fill(egui::Color32::from_rgba_unmultiplied(38, 38, 38, 217))
                .corner_radius(egui::CornerRadius::same(10))
                .inner_margin(egui::Margin::symmetric(8, 6))
                .stroke(egui::Stroke::new(
                    1.0,
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

        // **A click outside dismisses an open popover.** `route_pointer` already sends every click
        // to the chrome while a popover is open — precisely so the dismissing click cannot land on
        // a live desktop — but routing it here was only half of it: nothing closed the menu, so the
        // click was swallowed and the popover stayed up, still taking the keyboard. egui's own
        // `clicked_elsewhere` is the test, against the area's rectangle, which is the pill and the
        // popover together.
        if self.popover.is_some() && response.response.clicked_elsewhere() {
            self.popover = None;
        }

        // Read the position back out of egui's memory: a drag moved the area, and the pill's
        // resting place is one of the per-user choices the plan asks to persist. Written to the
        // config here and to the file by the caller, on change — never per frame.
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
            // Words, not glyphs — and not only for the reason the module docs give. egui's
            // bundled fonts have no `≡` or `⋮`: a hamburger drawn from them renders as a tofu
            // box, which the snapshot under `tests/snapshots/` showed the first time it was tried.
            // An icon font would fix that and cost an atlas; two short words cost nothing.
            let grip = ui.add(egui::Button::new(if self.config.menu_open {
                GRIP_EXPANDED
            } else {
                GRIP_COLLAPSED
            }));
            let grip = grip.on_hover_text(if self.config.menu_open {
                "Collapse the menu to its grip. The pill can be dragged anywhere."
            } else {
                "Expand the menu. The pill can be dragged anywhere."
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
            // §12 Stage 4b: the release key shown where the title used to show it. The title still
            // shows it too — the title stays the fallback for everything §2.8 and §6.1 surface.
            ui.label(format!("release: {}", facts.release_key))
                .on_hover_text(format!(
                    "{} releases the keyboard and mouse back to this desktop, and sends a full \
                     key-and-button release to the target. On niri, Mod+Escape does the same. \
                     This binding is never forwarded.",
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

    /// Resolutions, **enumerated from the device** (§12 Stage 4b: "not the reference's fixed
    /// list"), with the negotiated one marked.
    fn video_popover(
        &mut self,
        ui: &mut egui::Ui,
        facts: &ChromeFacts<'_>,
        commands: &mut Vec<ChromeCommand>,
    ) {
        if facts.modes.is_empty() {
            ui.add_enabled(false, egui::Button::new("No modes enumerated"))
                .on_disabled_hover_text(
                    "The device did not answer VIDIOC_ENUM_FRAMESIZES: the node is busy, or it \
                     is not there. The list is read from the device, never assumed.",
                );
            return;
        }
        for &(w, h) in facts.modes {
            let current = facts.current_mode == Some((w, h));
            let response = ui.add(egui::Button::selectable(current, format!("{w} x {h}")));
            let response = if current {
                response.on_hover_text(format!(
                    "{w} x {h} is the mode currently negotiated with the device (S_FMT)."
                ))
            } else {
                response.on_hover_text(format!(
                    "Renegotiate capture at {w} x {h}. The stream is reopened, so the picture \
                     blanks for a moment; the target's own resolution is not touched."
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
        // §6, A6: the frame's own header is the authority on its size, and it can disagree with
        // the negotiated mode for a few frames after a change. Showing both is how that reads as
        // the transient it is rather than as a fault.
        let frame = match facts.frame_size {
            Some((w, h)) => format!("{w} x {h}"),
            None => "none yet".to_string(),
        };
        ui.label(format!("frames arriving at {frame}"))
            .on_hover_text(
                "Read from each frame's own JPEG start-of-frame header, which is the only \
                 authority on it (§6). After a mode change the device emits a few frames at the \
                 previous size, so this catches up a moment after the list above.",
            );
    }

    /// Paste (§12 Stage 4c) and the two built-in shortcuts.
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

        // The progress line and the cancel hint, while one runs (§12 Stage 4c). `n / total keys,
        // ~s remaining` comes from the job itself, so the number on screen is the number the state
        // machine is at rather than a second count kept here.
        if let Some(progress) = paste.progress {
            ui.label(format!("paste: {progress}")).on_hover_text(
                "How many key transitions of this paste have been accepted by the input queue. \
                 The time is the steps left at the paced rate; a step whose report is identical \
                 to the last costs no transaction, so the real time is a little under it.",
            );
            ui.label(format!("{} cancels", facts.release_key))
                .on_hover_text(format!(
                    "{} stops the paste and sends a full key-and-button release (§2.6). \
                     {chord} does the same while a paste is running: it never restarts one.",
                    facts.release_key
                ));
        }

        // A refusal: the sentence, then every offending character on its own line (§10.2 — "name
        // them, all of them, so a long clipboard is fixed in one pass").
        if let Some(refusal) = &paste.refusal {
            ui.label(format!("paste refused: {refusal}")).on_hover_text(
                "Nothing was sent. The whole paste is compiled before the first key, so a \
                 character the declared layout cannot reach refuses all of it rather than \
                 typing most of it (§10.2, §2.8 item 3).",
            );
            for offender in refusal.offenders() {
                ui.label(format!("  {offender}")).on_hover_text(
                    "A character with no key on the declared target layout. Its position is \
                     counted in characters from the start of the clipboard.",
                );
            }
        } else if let Some(last) = &paste.last {
            ui.label(format!("last paste: {last}")).on_hover_text(
                "How the last paste ended. A cancelled or failed one names how far it got: a \
                 partially delivered sequence is never reported as success (§2.8 item 3).",
            );
        }

        ui.separator();

        // Keys only reach the target while the session is captured: `Producer::submit` refuses
        // otherwise (§2.6), so the item is disabled rather than clicked into a refusal.
        let captured = facts.capture == CaptureState::Captured;
        for builtin in Builtin::ALL {
            let response = ui.add_enabled(captured, egui::Button::new(builtin.label()));
            let response = response
                .on_hover_text(format!(
                    "Send {} to the target. Compiled by the same chord compiler `nanokvm key` \
                     uses, and every key it presses is released again.",
                    builtin.label()
                ))
                .on_disabled_hover_text(
                    "Input is not captured, so nothing can be sent to the target. Click in the \
                     window, or press Enter, to capture.",
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
                    "The pointer lands on the pixel it is over. The default: relative motion \
                     cannot reach a specific coordinate, because the target applies its own \
                     pointer acceleration (§3.4, measured).",
                ),
                (
                    MouseMode::Relative,
                    "Relative",
                    "Motion deltas, with the pointer locked to this window. For a target whose \
                     pointer does not follow absolute reports. While locked, only the release \
                     key gets out.",
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
                "Hide this desktop's pointer over the video, so only the target's own cursor is \
                 visible. Relative mode hides it anyway while the pointer is locked.",
            );
        if cursor.changed() {
            // The command carries the new value and the event loop performs it, config included.
            // Setting it here as well made the payload dead and gave the setting two owners.
            commands.push(ChromeCommand::SetCursorHidden(hidden));
        }

        let mut inverted = self.config.wheel_direction == WheelDirection::Inverted;
        let wheel = ui
            .checkbox(&mut inverted, "Invert the wheel")
            .on_hover_text(
                "Send a wheel detent the other way. Off, the host's sign is passed through \
                 unchanged — winit and the HID wheel field use the same convention.",
            );
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

    /// Mute, and 4a's three counters plus the configured depth.
    fn audio_popover(
        &mut self,
        ui: &mut egui::Ui,
        facts: &ChromeFacts<'_>,
        commands: &mut Vec<ChromeCommand>,
    ) {
        let audio = facts.audio;
        ui.label(format!("audio: {}", audio.state.label()))
            .on_hover_text(
                "What the audio path is doing. The dongle's card is paired by USB topology, the \
                 same way the video and serial nodes are (§8).",
            );

        let running = audio.state.is_running();
        // The **live** flag, not the persisted setting — the same rule review item 2 applied to
        // the pointer mode. The playback thread reads this atomic once a period, so it is the
        // only thing that can answer "is it muted right now"; `config.audio_muted` is what will
        // be remembered for the next run and is written by the command handler, not here.
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
            .on_hover_text(
                "Whole periods dropped because the ring was full — the dongle's sample clock is \
                 ahead of this host's.",
            );
        ui.label(format!("underruns {}", audio.underruns))
            .on_hover_text(
                "Periods of silence played because the ring was empty — this host's sample clock \
                 is ahead of the dongle's.",
            );
        ui.label(format!("drift corrections {}", audio.drift_corrections))
            .on_hover_text(
                "Periods inserted or dropped at the ring's bounds to absorb the two crystals' \
                 drift. Expected, and counted separately from the two above for that reason.",
            );
        ui.label(format!(
            "periods played {} / silent {} / muted {}",
            audio.played, audio.silent, audio.muted_periods
        ))
        .on_hover_text(
            "What was written to the sink, split three ways. Silence written because the ring \
             was empty is not audio, and neither is a mute the user asked for, so neither is \
             counted as played — a single number would make a session that played nothing but \
             silence look perfect.",
        );
        match audio.depth() {
            Some(depth) => ui.label(format!("buffer {depth}")).on_hover_text(
                "The **configured** depth, periods x frames. Not an end-to-end latency: this \
                 desk cannot measure one, and §5.5 forbids claiming it.",
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

    /// Every state of the Paste item: exactly one is enabled, and every disabled one's tooltip is
    /// the reason it is disabled (§12 Stage 4b). The enabled one states the chord and the layout,
    /// which §12 Stage 4c and §10.2 respectively require.
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
        // The one bound a user can meet that the plan does not have (`super::clipboard`): stated
        // here, as a memory bound, rather than met as a surprise at 16 MiB.
        assert!(
            ready.contains("16 MiB"),
            "the read bound is stated: {ready}"
        );
        assert!(
            ready.contains("no limit on how much may be pasted"),
            "and is not mistaken for a cap on the paste: {ready}"
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
    /// `input_map` does not look at (§12 Stage 4c: the never-forwarded surface stays one key).
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
        assert!(!ui.modal_open(), "4b ships no modal");
    }
}
