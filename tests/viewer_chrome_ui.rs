//! The chrome's widgets, driven with no window and no compositor (plan §12 Stage 4b).
//!
//! §12 Stage 4b's exit criterion for the UI is behavioural — *"a tooltip that appears is evidence;
//! a widget that exists is not"* — so these drive the real `ChromeUi::build` through
//! `egui_kittest`, which runs egui headless and exposes the result as an AccessKit tree. Hovering
//! a control and then **finding its tooltip text in the tree** is the evidence; so is finding a
//! disabled control's *reason* there.
//!
//! Nothing here opens a device, a window or a config file.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use egui_kittest::kittest::{NodeT, Queryable};
use egui_kittest::Harness;

use nanokvm::audio::pcm::{FnSinkOpener, FnSourceOpener, RecordingSink, ScriptedSource};
use nanokvm::audio::{AudioConfig, AudioHandle, PcmSink, PcmSource, RingConfig};

use nanokvm::script::Layout;
use nanokvm::viewer::chrome::{
    AudioState, ChromeAudio, ChromeCommand, ChromeFacts, ChromeUi, Config, MouseMode,
    PasteAvailability, PasteFacts, PasteOutcome, PasteProgress, Refusal, GRIP_COLLAPSED,
    GRIP_EXPANDED,
};
use nanokvm::viewer::state::CaptureState;

/// Everything one build needs, owned, because the harness's closure outlives any borrow of a
/// local.
struct Facts {
    capture: CaptureState,
    modes: Vec<(u32, u32)>,
    current_mode: Option<(u32, u32)>,
    /// The **live** pointer mode, which is the event loop's and not the config file's.
    pointer: MouseMode,
    frame_size: Option<(u32, u32)>,
    audio: ChromeAudio,
    paste: PasteFacts,
}

impl Default for Facts {
    fn default() -> Self {
        Facts {
            capture: CaptureState::Captured,
            modes: vec![(1920, 1080), (1280, 720), (640, 480)],
            current_mode: Some((1920, 1080)),
            pointer: MouseMode::Absolute,
            frame_size: Some((1920, 1080)),
            audio: ChromeAudio::default(),
            paste: PasteFacts {
                availability: PasteAvailability::Ready,
                layout: "us".to_string(),
                ..PasteFacts::default()
            },
        }
    }
}

/// The harness's state: the chrome under test, the facts it is built against, and the commands it
/// produced — which is how a click is asserted on its *consequence* rather than on a widget having
/// been touched.
struct App {
    ui: ChromeUi,
    facts: Facts,
    commands: Vec<ChromeCommand>,
    /// Whether this harness plays the event loop and performs the commands.
    ///
    /// `false` is how "the command is the only thing that changes the setting" is asserted: with
    /// nobody performing them, a UI that still mutated the shared config behind the command's back
    /// would show up as a changed setting (review items 2 and 8).
    perform: bool,
}

/// Build one frame **and perform what it asked for**, exactly as `App::perform_chrome` does.
///
/// Performing the commands here is what makes these tests evidence about the shipped behaviour
/// rather than about the widgets alone: the live pointer mode and the persisted `cursor_hidden`
/// are changed by the *event loop*, from the command's payload, and a test that skipped that step
/// would still pass with the payload thrown away (review items 2 and 8).
fn build(ctx: &egui::Context, app: &mut App) {
    let out = app.ui.build(
        ctx,
        &ChromeFacts {
            release_key: "Pause",
            capture: app.facts.capture,
            modes: &app.facts.modes,
            current_mode: app.facts.current_mode,
            pointer: app.facts.pointer,
            frame_size: app.facts.frame_size,
            audio: &app.facts.audio,
            paste: &app.facts.paste,
        },
    );
    for command in out.commands.iter().filter(|_| app.perform) {
        match command {
            ChromeCommand::SetMouseMode(mode) => {
                app.facts.pointer = *mode;
                app.ui.config.mouse_mode = *mode;
            }
            ChromeCommand::SetCursorHidden(hidden) => app.ui.config.cursor_hidden = *hidden,
            ChromeCommand::SetMuted(muted) => {
                // Both halves, exactly as `App::perform_chrome` does: the remembered setting and
                // the live flag the 4a playback thread reads once a period.
                app.ui.config.audio_muted = *muted;
                app.facts.audio.set_muted(*muted);
            }
            _ => {}
        }
    }
    app.commands.extend(out.commands);
}

fn harness_with(facts: Facts) -> Harness<'static, App> {
    harness_with_config(facts, Config::default())
}

fn harness_with_config(facts: Facts, config: Config) -> Harness<'static, App> {
    Harness::builder()
        .with_size(egui::vec2(900.0, 700.0))
        .build_state(
            build,
            App {
                ui: ChromeUi::new(config),
                facts,
                commands: Vec::new(),
                perform: true,
            },
        )
}

fn harness() -> Harness<'static, App> {
    harness_with(Facts::default())
}

/// Hover a control and run until egui has settled, so the tooltip's delay has elapsed.
fn hover(harness: &mut Harness<'static, App>, label: &str) {
    harness.get_by_label(label).hover();
    harness.run();
    harness.run();
}

/// Whether any node in the tree carries this text — which is how a tooltip shows up: egui paints
/// it into its own layer, and AccessKit reports it as a label.
///
/// `query_all`, not `query`: several nodes legitimately contain the same substring — the Video
/// popover has a `1920 x 1080` button *and* a `frames arriving at 1920 x 1080` label — and "is it
/// on screen" is the question, not "is it unique".
fn shows(harness: &Harness<'static, App>, text: &str) -> bool {
    harness.query_all_by_label_contains(text).next().is_some()
}

fn open(harness: &mut Harness<'static, App>, popover: &str) {
    harness.get_by_label(popover).click();
    harness.run();
}

/// Clicking Video opens the popover, and the chrome's own state — the one `route_key` reads
/// synchronously — reports it. Not egui's opinion: ours.
#[test]
fn clicking_video_opens_the_popover_and_the_state_reports_it() {
    let mut h = harness();
    h.run();
    assert!(
        !h.state().ui.popover_open(),
        "nothing is open to begin with"
    );

    open(&mut h, "Video");
    assert!(h.state().ui.popover_open(), "popover_open must be true");
    // And the popover's contents are on screen: the enumerated modes, not a fixed list.
    assert!(shows(&h, "1920 x 1080"), "the negotiated mode");
    assert!(shows(&h, "1280 x 720"));
    assert!(shows(&h, "640 x 480"));

    // Clicking it again closes it.
    open(&mut h, "Video");
    assert!(!h.state().ui.popover_open());
}

/// A resolution the device enumerated is clickable, and its consequence is the command the event
/// loop performs. The mode already negotiated produces nothing — reopening the device to arrive at
/// the size it is already streaming would blank the picture for no reason.
#[test]
fn choosing_a_mode_asks_the_pipeline_to_renegotiate() {
    let mut h = harness();
    h.run();
    open(&mut h, "Video");

    h.get_by_label("1920 x 1080").click();
    h.run();
    assert!(
        h.state().commands.is_empty(),
        "the current mode must not reopen the device: {:?}",
        h.state().commands
    );

    h.get_by_label("1280 x 720").click();
    h.run();
    assert_eq!(
        h.state().commands,
        vec![ChromeCommand::SetResolution {
            width: 1280,
            height: 720
        }]
    );
}

/// Hovering a control shows its tooltip. The Video button is the plain, enabled case.
#[test]
fn hovering_a_control_shows_its_tooltip() {
    let mut h = harness();
    h.run();
    assert!(
        !shows(&h, "Capture resolution, as the device reports it"),
        "the tooltip must not be on screen before it is hovered"
    );
    hover(&mut h, "Video");
    assert!(
        shows(&h, "Capture resolution, as the device reports it"),
        "hovering Video must show its tooltip"
    );
}

/// **The requirement in §12 Stage 4b**: every disabled control's tooltip says *why*. The Paste
/// item has three disabled reasons (§12 Stage 4c) and every one of them is on screen when hovered,
/// and none of them fires.
#[test]
fn the_disabled_paste_items_tooltip_states_the_reason() {
    for (availability, needle) in [
        (PasteAvailability::NotCaptured, "Input is not captured"),
        (
            PasteAvailability::Unsupported {
                reason: "this compositor has no data-control protocol".to_string(),
            },
            "no data-control protocol",
        ),
        (PasteAvailability::Running, "A paste is running"),
    ] {
        let mut h = harness_with(Facts {
            paste: PasteFacts {
                availability: availability.clone(),
                layout: "us".to_string(),
                ..PasteFacts::default()
            },
            ..Facts::default()
        });
        h.run();
        open(&mut h, "Keyboard");
        hover(&mut h, "Paste");
        assert!(
            shows(&h, needle),
            "{availability:?} must say why it is disabled"
        );
        // And it is really disabled: clicking it produces nothing.
        h.get_by_label("Paste").click();
        h.run();
        assert!(
            !h.state().commands.contains(&ChromeCommand::Paste),
            "a disabled Paste must not fire under {availability:?}: {:?}",
            h.state().commands
        );
    }
}

/// The enabled case: the tooltip states the chord and the declared layout (§12 Stage 4c, §10.2),
/// and a click asks the event loop to paste.
#[test]
fn an_enabled_paste_item_states_the_chord_and_the_layout_and_fires() {
    let mut h = harness();
    h.run();
    open(&mut h, "Keyboard");
    hover(&mut h, "Paste");
    assert!(shows(&h, "Shift+Pause"), "the chord must be discoverable");
    assert!(shows(&h, "layout us"), "§10.2: the layout is stated");
    // The 16 MiB read bound is the one limit a user can meet that the plan does not have, so the
    // item that offers the paste says it (`chrome::clipboard`: a memory bound, not a pacing cap).
    assert!(
        shows(&h, "16 MiB"),
        "the read bound is stated where it is met"
    );
    assert!(
        shows(&h, "no limit on how much may be pasted"),
        "and is not mistaken for a cap on the paste"
    );

    h.get_by_label("Paste").click();
    h.run();
    assert!(
        h.state().commands.contains(&ChromeCommand::Paste),
        "{:?}",
        h.state().commands
    );
}

/// A running paste shows `n / total keys, ~s remaining` and the cancel hint (§12 Stage 4c).
#[test]
fn a_running_paste_shows_its_progress_and_how_to_cancel() {
    let mut h = harness_with(Facts {
        paste: PasteFacts {
            availability: PasteAvailability::Running,
            layout: "us".to_string(),
            progress: Some(PasteProgress {
                sent: 37,
                total: 214,
                remaining: std::time::Duration::from_secs(7),
            }),
            ..PasteFacts::default()
        },
        ..Facts::default()
    });
    h.run();
    open(&mut h, "Keyboard");
    assert!(shows(&h, "37 / 214 keys"), "the progress line");
    assert!(shows(&h, "~7 s remaining"), "the expected duration");
    assert!(shows(&h, "Pause cancels"), "the cancel hint");
}

/// A refusal names every offending character, on its own line, with where it is (§10.2: "name them
/// all, so a long clipboard is fixed in one pass"). And it says nothing was sent.
#[test]
fn a_refused_paste_lists_every_offending_character() {
    let refusal = Refusal::Unreachable {
        layout: Layout::Us,
        chars: vec![('\u{e9}', 4), ('\u{2192}', 17)],
    };
    let mut h = harness_with(Facts {
        paste: PasteFacts {
            availability: PasteAvailability::Ready,
            layout: "us".to_string(),
            refusal: Some(refusal),
            ..PasteFacts::default()
        },
        ..Facts::default()
    });
    h.run();
    open(&mut h, "Keyboard");
    assert!(shows(&h, "2 characters are not reachable"), "the sentence");
    assert!(shows(&h, "no keyboard report was sent"), "§2.8 item 3");
    assert!(shows(&h, "U+00E9"), "the first offender");
    assert!(shows(&h, "at position 4"), "and where it is");
    assert!(shows(&h, "U+2192"), "the second offender");
    assert!(shows(&h, "at position 17"));
}

/// With no refusal outstanding, the last outcome is what the popover shows — and a cancelled or
/// failed one names how far it got (§2.8 item 3).
#[test]
fn the_last_outcome_is_shown_with_its_progress() {
    for (outcome, needle) in [
        (PasteOutcome::Done { total: 214 }, "done, 214 keys"),
        (
            PasteOutcome::Cancelled {
                sent: 37,
                total: 214,
            },
            "cancelled after 37 of 214 keys",
        ),
        (
            PasteOutcome::Failed {
                reason: "link is down".to_string(),
                sent: 37,
                total: 214,
            },
            "failed after 37 of 214 keys: link is down",
        ),
    ] {
        let mut h = harness_with(Facts {
            paste: PasteFacts {
                availability: PasteAvailability::Ready,
                layout: "us".to_string(),
                last: Some(outcome.clone()),
                ..PasteFacts::default()
            },
            ..Facts::default()
        });
        h.run();
        open(&mut h, "Keyboard");
        assert!(shows(&h, needle), "{outcome:?} must be on screen");
    }
}

/// A refusal takes the line: showing "the last paste was done" beside "this one was refused" is
/// one line too many, and the one that matters is the refusal.
#[test]
fn a_refusal_replaces_the_last_outcome_rather_than_joining_it() {
    let mut h = harness_with(Facts {
        paste: PasteFacts {
            availability: PasteAvailability::Ready,
            layout: "us".to_string(),
            last: Some(PasteOutcome::Done { total: 214 }),
            refusal: Some(Refusal::Empty),
            ..PasteFacts::default()
        },
        ..Facts::default()
    });
    h.run();
    open(&mut h, "Keyboard");
    assert!(shows(&h, "paste refused"), "the refusal is shown");
    assert!(!shows(&h, "last paste:"), "and the outcome is not");
}

/// The other disabled case, and the one a user meets first: the shortcuts cannot be sent while
/// The other disabled case, and the one a user meets first: the shortcuts cannot be sent while
/// input is not captured, and the tooltip says how to capture.
#[test]
fn the_shortcuts_are_disabled_until_input_is_captured_and_say_so() {
    let mut h = harness_with(Facts {
        capture: CaptureState::Released,
        ..Facts::default()
    });
    h.run();
    open(&mut h, "Keyboard");
    hover(&mut h, "Ctrl+Alt+Del");
    assert!(
        shows(&h, "Input is not captured"),
        "a disabled shortcut must say why"
    );
    h.get_by_label("Ctrl+Alt+Del").click();
    h.run();
    assert!(
        h.state().commands.is_empty(),
        "nothing may be sent while released: {:?}",
        h.state().commands
    );
}

/// Captured, the same item fires — and what it fires is the command the event loop turns into key
/// transitions through the producer.
#[test]
fn a_captured_session_can_send_a_built_in_shortcut() {
    let mut h = harness();
    h.run();
    open(&mut h, "Keyboard");
    h.get_by_label("Ctrl+Alt+Del").click();
    h.run();
    assert_eq!(
        h.state().commands,
        vec![ChromeCommand::SendShortcut(
            nanokvm::viewer::chrome::Builtin::CtrlAltDel
        )]
    );
}

/// The pill collapses to a grip: the four popover buttons go away, and the grip stays.
#[test]
fn the_pill_collapses_to_a_grip() {
    let mut h = harness();
    h.run();
    for label in ["Video", "Keyboard", "Mouse", "Audio"] {
        assert!(
            h.query_by_label(label).is_some(),
            "{label} should be on the expanded pill"
        );
    }
    // The grip is the only control with this label, expanded or not.
    h.get_by_label(GRIP_EXPANDED).click();
    h.run();
    assert!(!h.state().ui.config.menu_open, "the pill is collapsed");
    for label in ["Video", "Keyboard", "Mouse", "Audio"] {
        assert!(
            h.query_by_label(label).is_none(),
            "{label} must be gone when collapsed"
        );
    }
    assert!(
        h.query_by_label(GRIP_COLLAPSED).is_some(),
        "the grip stays, or the menu could never be reopened"
    );
}

/// Collapsing with a popover open closes it. Left open, `route_key`'s clause 5 would keep sending
/// the target's keystrokes to a menu that is not on screen.
#[test]
fn collapsing_closes_an_open_popover() {
    let mut h = harness();
    h.run();
    open(&mut h, "Mouse");
    assert!(h.state().ui.popover_open());
    h.get_by_label(GRIP_EXPANDED).click();
    h.run();
    assert!(!h.state().ui.popover_open());
}

/// The Audio section shows the counters from a `ChromeAudio` snapshot, and the configured depth
/// with its provenance (§5.5 — never an end-to-end latency).
#[test]
fn the_audio_section_shows_the_snapshots_counters() {
    let mut h = harness_with(Facts {
        audio: ChromeAudio {
            state: AudioState::On,
            overruns: 7,
            underruns: 3,
            drift_corrections: 11,
            periods: 4,
            frames_per_period: 480,
            ..ChromeAudio::default()
        },
        ..Facts::default()
    });
    h.run();
    open(&mut h, "Audio");
    assert!(shows(&h, "audio: on"));
    assert!(shows(&h, "overruns 7"));
    assert!(shows(&h, "underruns 3"));
    assert!(shows(&h, "drift corrections 11"));
    assert!(shows(&h, "4 x 480"), "the configured depth");
    assert!(shows(&h, "configured"), "§5.5: say what the number is");
}

/// **The 4a seam, end to end.** A real `AudioHandle` over the in-memory PCM fakes moves real
/// periods; its real `AudioSnapshot` is converted by the shipped `ChromeAudio::from_snapshot`;
/// and the popover renders *those* numbers. Then the Mute checkbox is clicked and the assertion
/// is on the **shared atomic the playback thread reads** — not on a command having been emitted.
///
/// A test that filled a `ChromeAudio` in by hand would pin nothing about the merge; this one
/// fails if a counter is wired to the wrong field, or if the mute toggle stops reaching 4a.
#[test]
fn a_snapshot_from_a_real_audio_handle_renders_and_the_mute_reaches_the_shared_flag() {
    let config = AudioConfig {
        ring: RingConfig {
            periods: 4,
            period_frames: 2,
        },
        device_periods: 2,
        reopen_backoff: Duration::from_millis(10),
        reopen_backoff_cap: Duration::from_millis(10),
        stop_deadline: Duration::from_millis(250),
        prefill_periods: 0,
    };
    // A source that never stops producing and a sink that takes everything: the ring is two
    // periods deep against a producer with no pacing, so overruns are what this arrangement is
    // *for* — they are a real counter moving, not a number typed into a struct.
    let source = FnSourceOpener::new("seam", |_| {
        Ok(
            Box::new(ScriptedSource::new("seam", std::iter::repeat_n(7i16, 4096)))
                as Box<dyn PcmSource>,
        )
    });
    let sink = FnSinkOpener::new("seam", |_| {
        Ok(Box::new(RecordingSink::new()) as Box<dyn PcmSink>)
    });
    let mut handle = AudioHandle::spawn(Box::new(source), Box::new(sink), config);

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let s = handle.snapshot();
        if s.periods_played > 0 && s.counts.overruns > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let snapshot = handle.snapshot();
    let mute = handle.mute_flag();
    assert!(
        snapshot.periods_played > 0 && snapshot.counts.overruns > 0,
        "the fakes were supposed to produce both: {snapshot:?}"
    );

    let audio = ChromeAudio::from_snapshot(&snapshot, config.ring, mute.clone());
    let mut h = harness_with(Facts {
        audio,
        ..Facts::default()
    });
    h.run();
    open(&mut h, "Audio");
    assert!(shows(&h, "audio: on"), "no condition, so it is on");
    assert!(
        shows(&h, &format!("overruns {}", snapshot.counts.overruns)),
        "4a's own overrun count"
    );
    assert!(
        shows(&h, &format!("periods played {}", snapshot.periods_played)),
        "the played/silent/muted split, from the real snapshot"
    );
    assert!(shows(&h, "4 x 2"), "the configured depth, from the ring");

    assert!(!mute.load(Ordering::Relaxed));
    h.get_by_label("Mute").click();
    h.run();
    assert!(
        mute.load(Ordering::Relaxed),
        "the toggle must reach the flag the 4a playback thread reads, not only the settings file"
    );
    handle.stop();
}

/// D2: a side that is opening says so and names which side, and the Mute toggle is disabled
/// because there is no stream yet. "Opening" and "on" must never read the same.
#[test]
fn an_opening_side_is_named_and_cannot_be_muted() {
    let mut h = harness_with(Facts {
        audio: ChromeAudio {
            state: AudioState::Opening {
                side: "playback".to_string(),
            },
            ..ChromeAudio::default()
        },
        ..Facts::default()
    });
    h.run();
    open(&mut h, "Audio");
    assert!(shows(&h, "opening (playback)"), "which side, by name");
    hover(&mut h, "Mute");
    assert!(
        shows(&h, "There is no audio to mute"),
        "nothing has opened yet, so the toggle says why it cannot act"
    );
}

/// An absent stream shows 4a's own words and the mute toggle is disabled, with the reason as its
/// tooltip.
#[test]
fn absent_audio_disables_the_mute_toggle_and_says_why() {
    let reason = "hw:8 is busy: another client holds the card";
    let mut h = harness_with(Facts {
        audio: ChromeAudio {
            state: AudioState::Absent {
                reason: reason.to_string(),
            },
            ..ChromeAudio::default()
        },
        ..Facts::default()
    });
    h.run();
    open(&mut h, "Audio");
    assert!(shows(&h, reason), "the reason, verbatim");
    hover(&mut h, "Mute");
    assert!(
        shows(&h, "There is no audio to mute"),
        "the disabled tooltip must give the reason"
    );
    h.get_by_label("Mute").click();
    h.run();
    assert!(
        h.state().commands.is_empty(),
        "a disabled Mute must not fire: {:?}",
        h.state().commands
    );
}

/// The Mouse popover's three controls each produce their command, and the chrome remembers them —
/// which is what the event loop then writes to the settings file.
#[test]
fn the_mouse_popover_changes_and_remembers_the_settings() {
    let mut h = harness();
    h.run();
    open(&mut h, "Mouse");

    h.get_by_label("Relative").click();
    h.run();
    assert_eq!(
        h.state().ui.config.mouse_mode,
        nanokvm::viewer::chrome::MouseMode::Relative
    );

    h.get_by_label("Hide the host cursor").click();
    h.run();
    assert!(h.state().ui.config.cursor_hidden);

    h.get_by_label("Invert the wheel").click();
    h.run();
    assert_eq!(
        h.state().ui.config.wheel_direction,
        nanokvm::viewer::chrome::WheelDirection::Inverted
    );

    assert_eq!(
        h.state().commands,
        vec![
            ChromeCommand::SetMouseMode(nanokvm::viewer::chrome::MouseMode::Relative),
            ChromeCommand::SetCursorHidden(true),
            ChromeCommand::SetWheelDirection(nanokvm::viewer::chrome::WheelDirection::Inverted),
        ]
    );
}

/// **Review item 2.** The live pointer mode is the one truth. `--pointer rel` changes what the
/// event loop is doing without touching the settings file, and the popover has to show *that*:
/// rendering the persisted value showed "Absolute" while the pointer was relative, so the entry
/// the user was already in fired a mode change, and the stale value could be written back.
#[test]
fn the_mouse_popover_marks_the_live_pointer_mode_not_the_persisted_one() {
    // The `--pointer rel` case: live relative, remembered absolute.
    let mut h = harness_with_config(
        Facts {
            pointer: MouseMode::Relative,
            ..Facts::default()
        },
        Config {
            mouse_mode: MouseMode::Absolute,
            ..Config::default()
        },
    );
    h.run();
    open(&mut h, "Mouse");

    h.get_by_label("Relative").click();
    h.run();
    assert!(
        h.state().commands.is_empty(),
        "Relative is already the live mode; clicking it must do nothing: {:?}",
        h.state().commands
    );
    assert_eq!(
        h.state().ui.config.mouse_mode,
        MouseMode::Absolute,
        "a command-line override is not persisted by itself"
    );

    // And switching in the UI does change the live mode, and is remembered.
    h.get_by_label("Absolute").click();
    h.run();
    assert_eq!(
        h.state().commands,
        vec![ChromeCommand::SetMouseMode(MouseMode::Absolute)]
    );
    assert_eq!(h.state().facts.pointer, MouseMode::Absolute);
}

/// **Review items 2 and 8.** The UI asks; the event loop performs. With nothing performing the
/// commands, neither setting moves — which is what makes the command's payload load-bearing rather
/// than decoration beside a config the UI had already written.
#[test]
fn the_mouse_popover_only_asks_and_never_writes_the_settings_itself() {
    let mut h = harness();
    h.state_mut().perform = false;
    h.run();
    open(&mut h, "Mouse");

    h.get_by_label("Relative").click();
    h.run();
    h.get_by_label("Hide the host cursor").click();
    h.run();

    assert_eq!(
        h.state().commands,
        vec![
            ChromeCommand::SetMouseMode(MouseMode::Relative),
            ChromeCommand::SetCursorHidden(true),
        ]
    );
    assert_eq!(
        h.state().ui.config.mouse_mode,
        MouseMode::Absolute,
        "the pointer mode must be changed by the command, not behind it"
    );
    assert!(
        !h.state().ui.config.cursor_hidden,
        "the cursor setting must be changed by the command, not behind it"
    );
}

/// **Review item 5.** A click outside an open popover dismisses it. `route_pointer` sends that
/// click to the chrome precisely so it cannot land on a live desktop — but until now nothing
/// closed the menu, so the click was swallowed and the popover stayed up, still taking the
/// keyboard from the target.
#[test]
fn clicking_outside_an_open_popover_closes_it() {
    let mut h = harness();
    h.run();
    open(&mut h, "Video");
    assert!(h.state().ui.popover_open());

    // Far from the pill, which sits at the default (16, 16) and is a few hundred points wide.
    let away = egui::pos2(860.0, 660.0);
    h.hover_at(away);
    h.run();
    h.drag_at(away);
    h.run();
    h.drop_at(away);
    h.run();

    assert!(
        !h.state().ui.popover_open(),
        "a click outside the popover must dismiss it"
    );
    assert!(
        h.state().commands.is_empty(),
        "dismissing must not perform anything: {:?}",
        h.state().commands
    );
}

/// The complement: a click *inside* the popover — on one of its own controls — does not dismiss
/// it. A menu that closed on every click could not be used.
#[test]
fn clicking_inside_an_open_popover_leaves_it_open() {
    let mut h = harness();
    h.run();
    open(&mut h, "Mouse");
    h.get_by_label("Invert the wheel").click();
    h.run();
    assert!(h.state().ui.popover_open(), "the popover must stay open");
    assert_eq!(
        h.state().commands,
        vec![ChromeCommand::SetWheelDirection(
            nanokvm::viewer::chrome::WheelDirection::Inverted
        )]
    );
}

/// The release key is shown where the title used to show it (§12 Stage 4b), and its tooltip says
/// what it does — including that it is never forwarded.
#[test]
fn the_pill_shows_the_release_key() {
    let mut h = harness();
    h.run();
    assert!(shows(&h, "release: Pause"));
    hover(&mut h, "release: Pause");
    assert!(shows(&h, "never forwarded"));
}

/// A device that enumerated nothing gets a disabled item saying so, not an empty menu and not a
/// guess at a list (§12 Stage 4b: the modes come from the device).
#[test]
fn no_enumerated_modes_is_stated_rather_than_guessed() {
    let mut h = harness_with(Facts {
        modes: Vec::new(),
        current_mode: None,
        ..Facts::default()
    });
    h.run();
    open(&mut h, "Video");
    hover(&mut h, "No modes enumerated");
    assert!(shows(&h, "VIDIOC_ENUM_FRAMESIZES"));
}

/// **Every control on the pill and in every popover has a tooltip.** Asserted by walking each
/// popover and hovering every button and checkbox in it, rather than by a list here that could
/// drift from the UI — a control added without a tooltip fails this.
#[test]
fn every_control_has_a_tooltip() {
    let mut h = harness_with(Facts {
        audio: ChromeAudio {
            state: AudioState::On,
            ..ChromeAudio::default()
        },
        ..Facts::default()
    });
    h.run();

    let controls: Vec<(&str, Vec<&str>)> = vec![
        (
            "",
            vec![GRIP_EXPANDED, "Video", "Keyboard", "Mouse", "Audio"],
        ),
        ("Video", vec!["1920 x 1080", "1280 x 720", "640 x 480"]),
        ("Keyboard", vec!["Paste", "Win+Tab", "Ctrl+Alt+Del"]),
        (
            "Mouse",
            vec![
                "Absolute",
                "Relative",
                "Hide the host cursor",
                "Invert the wheel",
            ],
        ),
        ("Audio", vec!["Mute"]),
    ];

    for (popover, labels) in controls {
        if !popover.is_empty() {
            open(&mut h, popover);
        }
        for label in labels {
            // Count the nodes before and after the hover: a tooltip is a new node in the tree, and
            // this catches a control with no tooltip whatever its wording is.
            let before = h.root().children().count();
            hover(&mut h, label);
            let after = h.root().children().count();
            assert!(
                after > before,
                "{label} (in the {} popover) shows no tooltip on hover",
                if popover.is_empty() { "pill" } else { popover }
            );
            // Move the pointer away so the next hover starts from nothing showing.
            h.hover_at(egui::pos2(880.0, 680.0));
            h.run();
        }
    }
}

// ---- the visual evidence ------------------------------------------------------------------
//
// Snapshot PNGs under `tests/snapshots/`, rendered through the same `egui-wgpu` the viewer uses.
// They are evidence of what the chrome *looks* like, which no AccessKit assertion above can be:
// "the pill is translucent and the popover hangs under it" is a picture or it is nothing.
//
// They are compared with a per-pixel threshold (egui_kittest's default 0.6, which is what makes
// them survive a different wgpu backend). `UPDATE_SNAPSHOTS=1 cargo test --test viewer_chrome_ui
// -- --ignored` rewrites them after a deliberate visual change; the `.new.png` and `.diff.png`
// files a failure leaves behind are gitignored.
//
// **They are `#[ignore]`d**, and the fourteen behavioural tests above are not. These four ask
// `wgpu` for an adapter and render on it: on a machine with no GPU and no software rasteriser —
// a container, a CI runner without lavapipe — `Harness::builder().wgpu()` fails, and a test
// suite that cannot run without a display is not one `cargo test` should be running by default.
// Everything above is headless and needs nothing. Run these deliberately:
//
// ```
// cargo test --test viewer_chrome_ui -- --ignored
// ```

fn snapshot_harness(facts: Facts) -> Harness<'static, App> {
    Harness::builder()
        .with_size(egui::vec2(560.0, 360.0))
        .with_pixels_per_point(1.0)
        .wgpu()
        .build_state(
            build,
            App {
                ui: ChromeUi::new(Config::default()),
                facts,
                commands: Vec::new(),
                perform: true,
            },
        )
}

#[test]
#[ignore = "needs a wgpu adapter (a real GPU or lavapipe): cargo test --test viewer_chrome_ui -- --ignored"]
fn snapshot_the_pill() {
    let mut h = snapshot_harness(Facts::default());
    h.run();
    h.snapshot("chrome-pill");
}

#[test]
#[ignore = "needs a wgpu adapter (a real GPU or lavapipe): cargo test --test viewer_chrome_ui -- --ignored"]
fn snapshot_the_video_popover() {
    let mut h = snapshot_harness(Facts::default());
    h.run();
    open(&mut h, "Video");
    h.snapshot("chrome-video-popover");
}

#[test]
#[ignore = "needs a wgpu adapter (a real GPU or lavapipe): cargo test --test viewer_chrome_ui -- --ignored"]
fn snapshot_the_audio_popover() {
    let mut h = snapshot_harness(Facts {
        audio: ChromeAudio {
            state: AudioState::On,
            overruns: 7,
            underruns: 3,
            drift_corrections: 11,
            periods: 4,
            frames_per_period: 480,
            ..ChromeAudio::default()
        },
        ..Facts::default()
    });
    h.run();
    open(&mut h, "Audio");
    h.snapshot("chrome-audio-popover");
}

#[test]
#[ignore = "needs a wgpu adapter (a real GPU or lavapipe): cargo test --test viewer_chrome_ui -- --ignored"]
fn snapshot_the_collapsed_grip() {
    let mut h = snapshot_harness(Facts::default());
    h.run();
    h.get_by_label(GRIP_EXPANDED).click();
    h.run();
    h.snapshot("chrome-collapsed-grip");
}
