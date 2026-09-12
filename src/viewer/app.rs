//! Viewer event loop and subsystem coordination.
//!
//! Input routing is decided before handing events to egui. Capture and release edges,
//! paste cancellation, and target key-up events must retain their meaning over the menu.
//! The event loop submits work without waiting for serial, decode, or presentation.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{CursorGrabMode, Window, WindowId};

use crate::audio::AudioHandle;
use crate::capture::{PipelineHandle, PipelineStats};
use crate::input::{Producer, ReleaseOutcome, Stats};
use crate::viewer::chrome::{
    self, AudioState, ChromeAudio, ChromeCommand, ChromeFacts, ChromeFrame, ChromeSender, ChromeUi,
    HitAreas, PasteAvailability, PasteFacts, PasteJob, PasteOutcome, PointerKind, Refusal,
    RouteInputs, Sink,
};
use crate::viewer::input_map::{self, KeyAction, RelAccumulator, WheelAccumulator};
use crate::viewer::render::{self, Gpu, RenderShared, RenderStats};
use crate::viewer::state::{
    reduce, self_release_trigger, Action, CaptureState, ReleaseNotice, Session, Trigger,
};
use crate::viewer::title::{self, AudioTitle, TitleFacts, APP_TITLE};
use crate::viewer::wayland::{InhibitEvent, ShortcutInhibit};

/// Wayland application id, so the compositor can match window rules to this client.
const APP_ID: &str = "nanokvm-usb";

/// How often the title and the release-outcome check run while released or captured.
const TICK: Duration = Duration::from_millis(250);
/// How often they run while `Engaging`, where a retry of [`Producer::engage`] is pending and the
/// writer's acknowledgement is expected within a few milliseconds.
const ENGAGING_TICK: Duration = Duration::from_millis(10);
/// Minimum interval between UI builds. An immediate egui repaint request can
/// otherwise create a busy loop; rendering faster than the display wastes event-loop time.
const CHROME_MIN_REPAINT: Duration = Duration::from_millis(16);

/// Deadline for clipboard fetch and fresh device information before paste preparation fails.
const PASTE_PREPARE_TIMEOUT: Duration = chrome::paste::PREPARE_TIMEOUT;

/// Event-handling latency samples kept for the stats line.
const LATENCY_SAMPLES: usize = 4096;
/// Maximum render join time before detaching an unresponsive compositor call.
const RENDER_JOIN: Duration = Duration::from_millis(500);

/// How the pointer is delivered to the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PointerMode {
    /// Absolute positioning through [`crate::proto::abs_coord`]. The default: relative
    /// motion cannot reach a specific coordinate, because the target applies pointer acceleration
    /// and magnitudes do not survive.
    #[default]
    Absolute,
    /// Relative motion, with the pointer locked to the window. For targets whose pointer does not
    /// follow absolute reports.
    Relative,
}

/// Viewer startup settings, device modes, and development controls.
#[derive(Debug, Clone)]
pub struct ViewerConfig {
    /// The pointer mode at startup. The chrome's Mouse popover can change it while running, which
    /// is why `App` keeps its own copy rather than reading this.
    pub pointer: PointerMode,
    /// How often to log the statistics line; `None` disables it.
    pub stats_interval: Option<Duration>,
    /// Quit cleanly after this long. A development flag.
    pub exit_after: Option<Duration>,
    /// Validated persistent settings, loaded before the window is created.
    pub chrome: chrome::Config,
    /// Where to write them back when the user changes one. `None` means nothing is remembered:
    /// a process with no `HOME` and no `XDG_CONFIG_HOME` has nowhere to put a file.
    pub chrome_store: Option<chrome::Store>,
    /// The capture modes the device enumerated, for the Video popover. Empty when the
    /// enumeration could not be made; the popover says so rather than offering a guess.
    pub video_modes: Vec<(u32, u32)>,
    /// Optional panel opened at startup for development measurements.
    pub chrome_popover: Option<chrome::Popover>,
    /// The frame rate the modes are offered at — what `--fps` asked for, carried through so a
    /// resolution change renegotiates at the rate the user chose rather than the driver's own
    /// default.
    pub fps: u32,
    /// Engage input at startup through the ordinary consumed Enter edge. Development only.
    pub capture_on_start: bool,
    /// Start a clipboard paste the moment capture engages. A development flag
    /// (`--paste-on-capture`, hidden), for the same reason as above.
    pub paste_on_capture: bool,
    /// Inject the normal release action after this paste duration. Development only.
    pub paste_cancel_after: Option<Duration>,
}

/// Things that reach the event loop from other threads.
///
/// Only the Wayland dispatch thread needs to wake the loop. The render thread's failures are
/// polled instead, on the tick: it has no proxy of its own and a fatal render error is not urgent
/// to the quarter-second.
#[derive(Debug, Clone, Copy)]
pub enum UserEvent {
    /// The compositor changed the inhibitor's state.
    Inhibit(InhibitEvent),
}

/// Run the viewer until close, development deadline, or interruption.
///
/// The event loop polls `interrupted`; signal handlers do not perform cleanup directly.
pub fn run(
    pipeline: &PipelineHandle,
    producer: Producer,
    audio: Option<&AudioHandle>,
    config: ViewerConfig,
    interrupted: &'static AtomicBool,
) -> anyhow::Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let mut app = App::new(pipeline, producer, audio, config, proxy, interrupted);
    let loop_result = event_loop.run_app(&mut app);
    // `run_app` consumes the event loop, so winit's `wl_display` is disconnected by the time it
    // returns. The ordinary teardown already happened in `exiting`, while the loop was alive; this
    // one is the belt-and-braces pass for a loop that failed before ever getting there, and it
    // must not touch Wayland (see `viewer::wayland`'s module docs).
    app.abandon_display();
    app.teardown();

    match (app.fatal.take(), loop_result) {
        // The app's own fatal error names the actual failure; a winit error alongside it is
        // usually just the loop noticing. Report the specific one and log the other.
        (Some(e), Err(loop_error)) => {
            log::debug!("the event loop also reported: {loop_error}");
            Err(e)
        }
        (Some(e), Ok(())) => Err(e),
        (None, Err(loop_error)) => Err(anyhow::Error::new(loop_error).context("the event loop")),
        (None, Ok(())) => Ok(()),
    }
}

/// Paste lifecycle. Preparation waits asynchronously for clipboard text and fresh
/// lock state; delivery starts only after both permit compilation.
enum PasteState {
    Idle,
    Preparing {
        fetch: chrome::clipboard::Fetch,
        /// The clipboard text once it has arrived. Never logged (`chrome::clipboard`).
        text: Option<String>,
        /// `Stats::device_info_generation` as it was when the refresh was asked for. The reading
        /// is fresh once this has moved.
        info_generation: u64,
        deadline: Instant,
    },
    Running(PasteJob),
}

impl PasteState {
    /// Whether host keys must be dropped, which is true from the moment the user asks — a
    /// keystroke landing between the trigger and the first typed key is as much a corruption of
    /// the text as one landing in the middle of it.
    fn is_active(&self) -> bool {
        !matches!(self, PasteState::Idle)
    }
}

struct App<'a> {
    pipeline: &'a PipelineHandle,
    producer: Producer,
    /// Optional audio handle for status and controls. The event loop never waits for
    /// audio recovery or propagates its failures into video or input.
    audio: Option<&'a AudioHandle>,
    config: ViewerConfig,
    proxy: EventLoopProxy<UserEvent>,
    interrupted: &'static AtomicBool,

    window: Option<Arc<Window>>,
    render_shared: Arc<RenderShared>,
    render_thread: Option<std::thread::JoinHandle<()>>,
    present_mode: &'static str,
    inhibit: Option<ShortcutInhibit>,

    /// The capture state machine's whole state, including the consumed-click bookkeeping that used
    /// to live here as a separate flag and drift out of step with it (see [`crate::viewer::state`]).
    session: Session,
    grab: Option<CursorGrabMode>,
    /// The window's size in physical pixels, straight from `Resized`. The event loop knows this
    /// immediately, which is why the letterbox rectangle is computed here rather than read from
    /// the render thread.
    window_size: (u32, u32),
    /// Whether the pointer is known to be over the surface. `set_cursor_grab` returning `Ok` only
    /// means the request was sent — `zwp_locked_pointer_v1` takes effect once the pointer is
    /// inside — so the cursor is hidden on that confirmation, never on the `Ok`.
    pointer_inside: bool,
    cursor_hidden: bool,
    /// Teardown has already run. It is called from `exiting` and again from `run`; the second call
    /// happens after the event loop — and with it the window and the Wayland display — is gone, so
    /// it must do nothing rather than call into either.
    torn_down: bool,
    rel: RelAccumulator,
    wheel: WheelAccumulator,

    // ---- the chrome --------------------------------------------------------
    // The egui context. Cloned into the UI build; never sent to the render thread.
    egui_ctx: egui::Context,
    /// `None` until `resumed` has a window to bind it to.
    egui_state: Option<egui_winit::State>,
    /// The pill's own state and the settings it persists.
    chrome: ChromeUi,
    /// The render thread's end of the chrome channel, held until `resumed` hands it over.
    chrome_rx: Option<chrome::ChromeReceiver>,
    chrome_tx: ChromeSender,
    /// The rectangles the chrome covered last build, for the routing rule's
    /// `pointer_over_chrome`. Ours, in physical pixels — see [`crate::viewer::chrome::hit`].
    hit: HitAreas,
    /// Whether the pointer was inside them when it was last seen.
    pointer_over_chrome: bool,
    /// The last cursor position in physical pixels, so the hit test can be redone after a build
    /// moved the pill without waiting for the pointer to move.
    last_cursor: Option<(f64, f64)>,
    /// Something happened that the chrome should be rebuilt for. Rebuilding on every
    /// `about_to_wait` would tessellate once per `CursorMoved`, which is a thousand times a second
    /// of work for a pill that did not change.
    chrome_dirty: bool,
    /// When egui next wants to be run again, from `viewport_output[ROOT].repaint_delay` — the
    /// tooltip fade-in and every other animation. Folded into `about_to_wait`'s `WaitUntil`, so
    /// every timer in this client is still on the event loop.
    repaint_at: Option<Instant>,
    /// The settings as last written, so a write happens on change rather than every frame.
    saved_chrome: chrome::Config,
    /// The mode the pipeline last reported negotiating, for the Video popover's marker. Refreshed
    /// on the tick from [`PipelineStats::negotiated_dimensions`].
    negotiated: Option<(u32, u32)>,
    /// Input presses forwarded by this event loop. Routing needs synchronous ownership
    /// information; the writer's held state may lag behind the queue.
    outstanding: chrome::Outstanding,
    /// The host's modifier state, from `ModifiersChanged`. Read by `input_map::map_key` for the
    /// one chord this client has: `Shift+`the release key starts a paste.
    modifiers: ModifiersState,
    /// Whether a data-control clipboard could be read at all, probed once at startup. The Paste
    /// item is disabled with this as its tooltip when it could not.
    paste_available: chrome::clipboard::Availability,
    /// The paste in flight, if any.
    paste: PasteState,
    /// How the last paste ended, for the chrome's last-outcome line.
    paste_last: Option<PasteOutcome>,
    /// Why the last attempt was refused before anything was sent, with its offender list.
    /// Cleared when a paste starts, so the chrome never shows a stale refusal beside a
    /// running paste.
    paste_refusal: Option<Refusal>,
    /// When the `--paste-cancel-after-ms` development flag should feed the release key.
    paste_cancel_at: Option<Instant>,
    /// `--capture-on-start` has already been acted on. A development flag that fired twice would
    /// capture, release and capture again.
    capture_on_start_done: bool,
    /// `--paste-on-capture` has already been acted on, for the same reason.
    paste_on_capture_done: bool,
    /// UI build and tessellation durations, reported separately from input-handler time.
    chrome_build_us: VecDeque<u64>,

    started: Instant,
    next_tick: Instant,
    last_stats: Instant,
    last_captured: u64,
    latency_us: VecDeque<u64>,
    unmapped_keys: u64,
    /// Latest release notice and deduplication state.
    release_notice: ReleaseNotice,
    /// Last observed reconnect count, used to log each completed recovery once.
    last_reconnects: u64,
    title: String,
    fatal: Option<anyhow::Error>,
}

impl<'a> App<'a> {
    fn new(
        pipeline: &'a PipelineHandle,
        producer: Producer,
        audio: Option<&'a AudioHandle>,
        config: ViewerConfig,
        proxy: EventLoopProxy<UserEvent>,
        interrupted: &'static AtomicBool,
    ) -> Self {
        let now = Instant::now();
        let (chrome_tx, chrome_rx) = chrome::channel();
        let saved_chrome = config.chrome;
        let mut chrome = ChromeUi::new(saved_chrome);
        if let Some(popover) = config.chrome_popover {
            chrome.open_popover(popover);
        }
        // The remembered mute is applied to the live flag once, here, so that the popover's
        // checkbox — which reads the flag, not the file — agrees with the setting on the first
        // frame. Without this the setting would be remembered and never honoured.
        if let Some(handle) = audio {
            handle.set_muted(saved_chrome.audio_muted);
        }
        App {
            pipeline,
            producer,
            audio,
            config,
            proxy,
            interrupted,
            egui_ctx: egui::Context::default(),
            egui_state: None,
            chrome,
            chrome_rx: Some(chrome_rx),
            chrome_tx,
            hit: HitAreas::default(),
            pointer_over_chrome: false,
            last_cursor: None,
            chrome_dirty: true,
            repaint_at: None,
            saved_chrome,
            negotiated: None,
            outstanding: chrome::Outstanding::default(),
            modifiers: ModifiersState::empty(),
            // Probed once, here, before a window exists: it is a Wayland roundtrip that asks the
            // compositor for the clipboard's MIME types and reads no user data, and the answer —
            // "is there a data-control manager" — cannot change while this process runs.
            paste_available: chrome::clipboard::Availability::probe(),
            paste: PasteState::Idle,
            paste_last: None,
            paste_refusal: None,
            paste_cancel_at: None,
            capture_on_start_done: false,
            paste_on_capture_done: false,
            chrome_build_us: VecDeque::with_capacity(LATENCY_SAMPLES),
            window: None,
            render_shared: Arc::new(RenderShared::new()),
            render_thread: None,
            present_mode: "unconfigured",
            inhibit: None,
            session: Session::new(),
            grab: None,
            window_size: (0, 0),
            pointer_inside: false,
            cursor_hidden: false,
            torn_down: false,
            rel: RelAccumulator::default(),
            wheel: WheelAccumulator::default(),
            started: now,
            next_tick: now,
            last_stats: now,
            last_captured: 0,
            latency_us: VecDeque::with_capacity(LATENCY_SAMPLES),
            unmapped_keys: 0,
            release_notice: ReleaseNotice::default(),
            last_reconnects: 0,
            title: String::new(),
            fatal: None,
        }
    }

    /// Tell the Wayland handle its display is gone, so teardown does not call into it.
    ///
    /// Only `run` uses this, after `run_app` has returned and taken winit's connection with it.
    fn abandon_display(&mut self) {
        if let Some(i) = self.inhibit.as_mut() {
            i.abandon_display();
        }
    }

    /// Stop rendering and release compositor input capture. Idempotent.
    ///
    /// The join is bounded because presentation may block while the surface is hidden.
    /// GPU destruction is deferred to process exit to avoid a closed Wayland connection.
    fn teardown(&mut self) {
        if self.torn_down {
            // `run` calls this again after `run_app` has returned, by which point neither the
            // window nor the display exists to be called into.
            return;
        }
        self.torn_down = true;
        self.ungrab();
        // Dropped, not merely disarmed: this also stops and joins the Wayland dispatch thread,
        // which must not outlive winit's display (see `viewer::wayland`'s module docs).
        drop(self.inhibit.take());
        self.render_shared.stop();
        if let Some(h) = self.render_thread.take() {
            if self.render_shared.wait_finished(RENDER_JOIN) {
                if h.join().is_err() {
                    log::error!("nanokvm-render thread panicked");
                }
            } else {
                // The render thread retains GPU objects until exit, avoiding destructors on winit
                // proxies after the display closes.
                log::warn!(
                    "the render thread has not finished within {RENDER_JOIN:?} — it is most \
                     likely blocked in present() on a surface the compositor is not scheduling. \
                     Detaching it so shutdown can continue; the release-all has already been \
                     requested. It holds no GPU objects it will try to destroy afterwards."
                );
                drop(h);
            }
        }
    }

    /// Run one trigger through the state machine and perform what it asks for.
    fn feed(&mut self, el: &ActiveEventLoop, trigger: Trigger) {
        let (next, actions) = reduce(self.session, trigger);
        if next.capture() != self.session.capture() {
            log::debug!(
                "capture: {:?} -> {:?} on {trigger:?}",
                self.session.capture(),
                next.capture()
            );
            self.title.clear();
            if next.capture() == CaptureState::Engaging {
                // Shorten the tick on engagement so recapture does not wait through the idle interval.
                self.next_tick = Instant::now() + ENGAGING_TICK;
            }
        }
        self.session = next;
        for action in actions {
            self.perform(el, action);
        }
        // Start the development paste only once, after capture is fully engaged.
        if self.config.paste_on_capture
            && !self.paste_on_capture_done
            && self.session.capture() == CaptureState::Captured
        {
            self.paste_on_capture_done = true;
            log::info!("--paste-on-capture: capture engaged; starting a clipboard paste");
            self.start_paste();
        }
    }

    fn perform(&mut self, el: &ActiveEventLoop, action: Action) {
        match action {
            Action::ArmInhibitor => {
                // Absence was logged once at startup; it is not repeated on every capture.
                if let Some(i) = self.inhibit.as_mut() {
                    i.arm();
                }
            }
            Action::DisarmInhibitor => {
                if let Some(i) = self.inhibit.as_mut() {
                    i.disarm();
                }
            }
            Action::GrabPointer => self.grab_pointer(),
            Action::ReleasePointer => self.ungrab(),
            Action::TryEngage => {
                // Never blocks: `NotYetAcked` keeps the state machine in `Engaging` and the next
                // event or tick retries.
                let trigger = match self.producer.engage() {
                    Ok(()) => Trigger::EngageSucceeded,
                    Err(e) => {
                        log::debug!("engage deferred: {e}");
                        Trigger::EngageDeferred
                    }
                };
                self.feed(el, trigger);
            }
            Action::ReleaseAll(reason) => {
                log::info!("release-all requested: {reason:?}");
                // Record paste cancellation before release-all so its progress reflects the release
                // decision and its held keys are cleared by that same release.
                self.cancel_paste(reason);
                self.producer.request_release_all(reason);
                // The target holds nothing after this, so nothing is owed a release any more.
                self.outstanding.clear();
                self.rel = RelAccumulator::default();
                self.wheel = WheelAccumulator::default();
            }
            Action::Forward(event) => {
                // Recorded before the submit, and whatever the submit does: a refused press is
                // followed by a release-all, which clears the set again.
                match event {
                    crate::input::Event::Key { key, down } => self.outstanding.set_key(key, down),
                    crate::input::Event::Button { button, down } => {
                        self.outstanding.set_button(button, down)
                    }
                    _ => {}
                }
                if let Err(e) = self.producer.submit(event) {
                    // Overflow cancels the session; retaining the notice explains why recapture is needed.
                    log::warn!("input submission refused: {e}");
                    self.feed(el, Trigger::SubmitFailed(e));
                }
            }
            Action::Exit => el.exit(),
        }
    }

    /// Request relative pointer lock. Success confirms the request was sent, not that
    /// the compositor granted it. Capture state must still handle later deactivation.
    fn grab_pointer(&mut self) {
        if self.config.pointer != PointerMode::Relative {
            return;
        }
        let Some(w) = self.window.as_ref() else {
            return;
        };
        for mode in [CursorGrabMode::Locked, CursorGrabMode::Confined] {
            match w.set_cursor_grab(mode) {
                Ok(()) => {
                    log::info!("pointer grab requested: {mode:?} (requested, not yet in force)");
                    self.grab = Some(mode);
                    self.hide_cursor_if_confirmed();
                    return;
                }
                Err(e) => log::debug!("pointer grab {mode:?} refused: {e}"),
            }
        }
        log::warn!(
            "this compositor supports neither a locked nor a confined pointer; relative motion \
             will stop at the window edge"
        );
    }

    /// Hide the cursor if a grab is up and the pointer has been confirmed over the surface.
    /// Idempotent; called from every place that can supply the confirmation.
    fn hide_cursor_if_confirmed(&mut self) {
        // The chrome's "hide the host cursor" setting is the other reason to hide it, and both go
        // through one place so the two cannot disagree about the current state.
        self.apply_cursor_policy();
    }

    fn ungrab(&mut self) {
        let Some(w) = self.window.as_ref() else {
            return;
        };
        if self.grab.take().is_some() {
            if let Err(e) = w.set_cursor_grab(CursorGrabMode::None) {
                log::warn!("releasing the pointer grab failed: {e}");
            }
        }
        // Not unconditionally visible: the user may have asked for it hidden in the Mouse
        // popover, and a release must not overrule a setting it knows nothing about.
        self.apply_cursor_policy();
    }

    // ---- the chrome --------------------------------------------------------

    /// The routing rule's inputs, as of right now. Every field is read from this thread's own
    /// state, synchronously — which is the whole reason the UI build is on this thread.
    fn route_inputs(&self) -> RouteInputs {
        RouteInputs {
            capture: self.session.capture(),
            pointer: self.config.pointer,
            popover_open: self.chrome.popover_open(),
            modal_open: self.chrome.modal_open(),
            pointer_over_chrome: self.pointer_over_chrome,
            // Treat preparation as active paste so another request cannot overlap it.
            paste_running: self.paste.is_active(),
            // The grab is a *request* until the compositor honours it, and a compositor that
            // refused both modes leaves this false: the pointer is free, the pill is reachable and
            // a click on it must not be forwarded to the target as a real button press.
            pointer_locked: self.grab.is_some(),
            outstanding: self.outstanding,
            // What the *paste* is holding, so clause 2 does not mistake it for something the host
            // pressed and forward a host key-up into the middle of a character.
            paste_held: match &self.paste {
                PasteState::Running(job) => job.held(),
                _ => chrome::Outstanding::default(),
            },
        }
    }

    /// Hand one raw `WindowEvent` to `egui_winit`.
    ///
    /// The `EventResponse` is discarded. Routing is decided before this is
    /// called, by [`crate::viewer::chrome::route`], and `consumed` is wrong for a KVM in three
    /// measured ways — the module docs there list them, starting with Tab.
    fn give_to_egui(&mut self, event: &WindowEvent) {
        let (Some(window), Some(state)) = (self.window.clone(), self.egui_state.as_mut()) else {
            return;
        };
        let _ = state.on_window_event(&window, event);
    }

    /// Build UI audio facts from the current worker snapshot and mute flag.
    fn audio(&self) -> ChromeAudio {
        let Some(handle) = self.audio else {
            // `--no-audio`: nothing was started, and the user asked for that. Distinct from
            // `Absent`, which means it should be running and is not.
            return ChromeAudio {
                state: AudioState::Off,
                ..ChromeAudio::default()
            };
        };
        ChromeAudio::from_snapshot(&handle.snapshot(), handle.config().ring, handle.mute_flag())
    }

    /// Build one frame of chrome, tessellate it, post it to the render thread, and perform
    /// whatever the user asked for.
    ///
    /// Runs on the event loop. That costs the handling latency the `--stats-interval` line
    /// reports, and it is the number that makes the choice falsifiable.
    fn build_chrome(&mut self, el: &ActiveEventLoop) {
        let started = Instant::now();
        let Some(window) = self.window.clone() else {
            return;
        };
        let Some(state) = self.egui_state.as_mut() else {
            return;
        };
        let raw = state.take_egui_input(&window);

        // Everything the UI reads, gathered before the closure so the borrows stay disjoint.
        let capture = self.session.capture();
        let frame_size = self.render_shared.frame_size();
        let audio = self.audio();
        let negotiated = self.negotiated;
        // Show the live mode because a CLI override can differ from saved settings.
        let pointer = chrome::MouseMode::from(self.config.pointer);
        let paste = self.paste_facts();
        let modes = std::mem::take(&mut self.config.video_modes);
        let ctx = self.egui_ctx.clone();
        let ui = &mut self.chrome;

        let mut built = None;
        let full = ctx.run(raw, |ctx| {
            built = Some(ui.build(
                ctx,
                &ChromeFacts {
                    release_key: super::RELEASE_KEY,
                    capture,
                    modes: &modes,
                    current_mode: negotiated,
                    pointer,
                    frame_size,
                    audio: &audio,
                    paste: &paste,
                },
            ));
        });
        self.config.video_modes = modes;
        let out = built.expect("egui::Context::run always runs the closure once");

        if let Some(state) = self.egui_state.as_mut() {
            state.handle_platform_output(&window, full.platform_output);
        }
        let pixels_per_point = full.pixels_per_point;
        let primitives = ctx.tessellate(full.shapes, pixels_per_point);

        self.hit = out.hit;
        // The pill may have been dragged out from under the pointer, so the hit test is redone
        // against the position last seen rather than waiting for the next motion.
        if let Some(pos) = self.last_cursor {
            let was_over = self.pointer_over_chrome;
            self.pointer_over_chrome = self.hit.hits(pos);
            if was_over != self.pointer_over_chrome {
                self.apply_cursor_policy();
            }
        }

        self.chrome_tx.send(ChromeFrame {
            primitives,
            textures_delta: full.textures_delta,
            pixels_per_point,
            size_in_pixels: [self.window_size.0.max(1), self.window_size.1.max(1)],
        });

        // Use egui’s repaint deadline even while video is idle. MAX means no deadline;
        // ZERO requests an immediate event-loop wake.
        self.repaint_at = full
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.repaint_delay)
            .filter(|d| *d != Duration::MAX)
            .map(|d| Instant::now() + d.max(CHROME_MIN_REPAINT));

        // Measured before the commands run: a command is the *consequence* of the build, and
        // folding a `request_format` into the build's cost would make the number mean two things.
        let us = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        if self.chrome_build_us.len() == LATENCY_SAMPLES {
            self.chrome_build_us.pop_front();
        }
        self.chrome_build_us.push_back(us);

        for command in out.commands {
            self.perform_chrome(el, command);
        }
        self.persist_chrome();
        self.chrome_dirty = false;
    }

    /// Do what a popover asked for. The UI never performs anything itself.
    fn perform_chrome(&mut self, el: &ActiveEventLoop, command: ChromeCommand) {
        match command {
            ChromeCommand::SetResolution { width, height } => {
                // The UI marks the negotiated mode and does not fire for it, but the marker is one
                // tick old and the command is performed here; asking for the mode the device is
                // already streaming would reopen the node and blank the picture for nothing.
                if !format_change_needed(self.negotiated, (width, height)) {
                    log::debug!(
                        "chrome: {width}x{height} is already negotiated; not reopening the device"
                    );
                    return;
                }
                log::info!("chrome: renegotiating capture at {width}x{height}");
                self.pipeline.request_format(width, height, self.config.fps);
            }
            ChromeCommand::SendShortcut(builtin) => self.send_shortcut(el, builtin),
            ChromeCommand::SetMouseMode(mode) => {
                // Persist UI choices only. A one-run CLI override must not become a saved preference.
                self.config.pointer = mode.into();
                self.chrome.config.mouse_mode = mode;
                log::info!("chrome: pointer mode is now {:?}", self.config.pointer);
                // A mode change mid-session has to move the grab with it, or relative mode would
                // be selected with a free pointer and absolute mode with a locked one.
                if self.session.capture() == CaptureState::Captured {
                    match self.config.pointer {
                        PointerMode::Relative => self.grab_pointer(),
                        PointerMode::Absolute => self.ungrab(),
                    }
                }
                self.rel = RelAccumulator::default();
            }
            ChromeCommand::SetCursorHidden(hidden) => {
                log::info!(
                    "chrome: host cursor {}",
                    if hidden { "hidden" } else { "shown" }
                );
                // Authoritative, like `SetResolution` and `SendShortcut`: the checkbox reports
                // what the user asked for and this performs it. The UI mutating the shared config
                // itself made the payload dead and left two places able to disagree.
                self.chrome.config.cursor_hidden = hidden;
                self.apply_cursor_policy();
            }
            ChromeCommand::SetWheelDirection(direction) => {
                log::info!("chrome: wheel direction is now {direction:?}");
                self.wheel = WheelAccumulator::default();
            }
            ChromeCommand::SetMuted(muted) => {
                log::info!("chrome: audio {}", if muted { "muted" } else { "unmuted" });
                // Apply mute to both the playback flag and the saved setting.
                self.chrome.config.audio_muted = muted;
                if let Some(handle) = self.audio {
                    handle.set_muted(muted);
                }
            }
            ChromeCommand::Paste => self.start_paste(),
        }
    }

    /// Submit built-in shortcut transitions through the ordinary producer and reducer,
    /// preserving release and overflow handling.
    fn send_shortcut(&mut self, el: &ActiveEventLoop, builtin: chrome::Builtin) {
        let events = match chrome::shortcut::events(builtin) {
            Ok(e) => e,
            // Structurally impossible — `chrome::shortcut`'s tests compile both — and reported
            // rather than unwrapped, because a panicking menu item in a viewer holding someone's
            // console is not a trade worth making.
            Err(e) => {
                log::error!(
                    "the built-in shortcut {} does not compile: {e}",
                    builtin.chord()
                );
                return;
            }
        };
        log::info!("chrome: sending {} ({})", builtin.label(), builtin.chord());
        for event in events {
            let crate::input::Event::Key { key, down } = event else {
                continue;
            };
            self.feed(el, Trigger::Key { key, down });
        }
    }

    // ---- clipboard paste -----------------------------------------------------

    /// What the Keyboard popover's Paste item is, right now.
    ///
    /// Ordered most specific first: a running paste is a running paste whatever else is true, a
    /// compositor with no data-control can never paste, and capture is the last thing that can be
    /// fixed by the user in one click.
    fn paste_availability(&self) -> PasteAvailability {
        if self.paste.is_active() {
            return PasteAvailability::Running;
        }
        if let chrome::clipboard::Availability::Unsupported { reason } = &self.paste_available {
            return PasteAvailability::Unsupported {
                reason: reason.clone(),
            };
        }
        if self.session.capture() != CaptureState::Captured {
            return PasteAvailability::NotCaptured;
        }
        PasteAvailability::Ready
    }

    /// Everything the Keyboard popover shows about pasting.
    fn paste_facts(&self) -> PasteFacts {
        PasteFacts {
            availability: self.paste_availability(),
            layout: crate::script::Layout::from(self.chrome.config.layout).to_string(),
            progress: match &self.paste {
                PasteState::Running(job) => Some(job.progress()),
                _ => None,
            },
            last: self.paste_last.clone(),
            refusal: self.paste_refusal.clone(),
        }
    }

    /// Refuse a paste, with the sentence the chrome shows. Nothing has been sent.
    fn refuse_paste(&mut self, refusal: Refusal) {
        // The refusal is what the user sees next, so the previous outcome goes: two lines saying
        // different things about "the last paste" is one line too many.
        //
        // `log_line` and not `Display`: the offending characters are the user's clipboard, and a
        // log is durable and gets pasted into bug reports. The count goes to the log and the
        // characters stay in the popover, in front of the person who copied them
        // (`chrome::clipboard`'s "never logged, printed or persisted").
        log::warn!("paste refused: {}", refusal.log_line());
        self.paste_refusal = Some(refusal);
        self.paste_last = None;
        self.paste = PasteState::Idle;
        self.paste_cancel_at = None;
        self.chrome_dirty = true;
    }

    /// Ask for the clipboard and for a fresh `GET_INFO`, and join them in [`PasteState::Preparing`].
    ///
    /// Both halves are started here and neither is waited for. Nothing is compiled yet, which
    /// is what makes every refusal below a refusal before anything was sent.
    fn start_paste(&mut self) {
        if self.paste.is_active() {
            log::debug!("a paste is already in flight; the trigger is ignored");
            return;
        }
        if self.session.capture() != CaptureState::Captured {
            self.refuse_paste(Refusal::NotCaptured);
            return;
        }
        if let chrome::clipboard::Availability::Unsupported { reason } = &self.paste_available {
            let reason = reason.clone();
            self.refuse_paste(Refusal::Clipboard { reason });
            return;
        }
        // One clipboard reader at a time: a hung clipboard owner leaves a thread winding down to
        // its own deadline, and a second trigger in that window is refused by name rather than
        // piling another thread and another pipe onto it (`chrome::clipboard`).
        let fetch = match chrome::clipboard::Fetch::spawn() {
            Ok(fetch) => fetch,
            Err(reason) => {
                self.refuse_paste(Refusal::Clipboard { reason });
                return;
            }
        };
        // Read the baseline atomically with the refresh request. Sampling afterward could
        // miss a completed refresh and wait for another generation unnecessarily.
        let info_generation = self.producer.refresh_device_info();
        log::info!("paste: reading the clipboard and refreshing the target's lock bits");
        self.paste = PasteState::Preparing {
            fetch,
            text: None,
            info_generation,
            deadline: Instant::now() + PASTE_PREPARE_TIMEOUT,
        };
        self.paste_refusal = None;
        self.paste_cancel_at = self.config.paste_cancel_after.map(|d| Instant::now() + d);
        self.chrome_dirty = true;
    }

    /// Advance whatever the paste is doing. Called from `about_to_wait`, on every pass.
    fn advance_paste(&mut self, el: &ActiveEventLoop) {
        let now = Instant::now();
        // The `--paste-cancel-after-ms` development flag, taking the release key's own path so
        // that what an unattended run exercises is the shipping cancel.
        if self.paste_cancel_at.is_some_and(|at| now >= at) && self.paste.is_active() {
            self.paste_cancel_at = None;
            log::info!("--paste-cancel-after-ms elapsed; feeding the release key");
            self.feed(el, Trigger::ReleaseKey);
            return;
        }
        match &self.paste {
            PasteState::Idle => {}
            PasteState::Preparing { .. } => self.advance_preparing(now),
            PasteState::Running(_) => self.advance_running(el, now),
        }
    }

    /// Every deadline a paste owns, for `about_to_wait`'s `WaitUntil`.
    ///
    /// Every timer in this client is on the event loop (`about_to_wait`'s docs), and a paste has
    /// three: the next key transition's pace, the preparation deadline, and the
    /// `--paste-cancel-after-ms` development flag. A `Preparing` paste is polled on the ordinary
    /// tick as well, because the two answers it waits for arrive from other threads with no
    /// deadline of their own.
    fn paste_wakeups(&self) -> Vec<Instant> {
        let mut out = Vec::new();
        match &self.paste {
            PasteState::Idle => {}
            PasteState::Preparing { deadline, .. } => {
                out.push(*deadline);
                // A short poll while preparing: the clipboard and the `GET_INFO` both land on
                // other threads and neither wakes this one.
                out.push(Instant::now() + ENGAGING_TICK);
            }
            PasteState::Running(job) => {
                if let Some(at) = job.due_at() {
                    out.push(at);
                }
            }
        }
        if let Some(at) = self.paste_cancel_at {
            out.push(at);
        }
        out
    }

    /// Join the clipboard read and the `GET_INFO` refresh, then compile — or refuse.
    fn advance_preparing(&mut self, now: Instant) {
        // Read outside the borrow of `self.paste`: `stats()` takes the writer's link mutex, and
        // holding a mutable borrow of the state across it would not compile anyway.
        let stats = self.producer.stats();
        let mut clipboard_error = None;
        let (ready, timed_out) = match &mut self.paste {
            PasteState::Preparing {
                fetch,
                text,
                info_generation,
                deadline,
            } => {
                if text.is_none() {
                    match fetch.poll() {
                        Some(Ok(fetched)) => *text = Some(fetched),
                        Some(Err(reason)) => clipboard_error = Some(reason),
                        None => {}
                    }
                }
                let fresh = stats.device_info_generation != *info_generation;
                let ready = match (&*text, fresh) {
                    (Some(t), true) => Some(t.clone()),
                    _ => None,
                };
                (ready, now >= *deadline)
            }
            _ => return,
        };
        if let Some(reason) = clipboard_error {
            self.refuse_paste(Refusal::Clipboard { reason });
            return;
        }
        if let Some(text) = ready {
            // Stale is unknown, not off. A refresh that could not be served
            // — no link, a transaction that failed, a reply that did not parse — advances the
            // generation so this is not wedged, and says so; a reading from some earlier minute
            // wearing a fresh number is what must not decide whether letters are typed.
            let caps_lock = if stats.device_info_stale {
                None
            } else {
                stats.device_info.map(|i| i.caps_lock)
            };
            self.compile_and_start(text, caps_lock);
            return;
        }
        if timed_out {
            // Which half is missing is the useful half of this message: "the clipboard did not
            // answer" and "the device did not answer" send a reader to different places — and they
            // are different refusals, because the device half is a `LockStateUnknown` with a cause
            // and not a clipboard problem at all.
            let refusal = match &self.paste {
                PasteState::Preparing { text: None, .. } => Refusal::Clipboard {
                    reason: format!(
                        "the clipboard was not delivered within {} s; the application that owns \
                         it did not answer",
                        PASTE_PREPARE_TIMEOUT.as_secs()
                    ),
                },
                _ => Refusal::DeviceTimeout,
            };
            self.refuse_paste(refusal);
        }
    }

    /// Compile the clipboard under the declared layout and the lock bits, and start the job.
    ///
    /// The text reaches the log only as a byte length — never as itself and never as anything
    /// derived from it, a digest included (`chrome::clipboard`'s module docs).
    fn compile_and_start(&mut self, text: String, caps_lock: chrome::paste::CapsLockState) {
        let layout = crate::script::Layout::from(self.chrome.config.layout);
        log::info!(
            "paste: {} bytes on the clipboard, layout {layout}, target CapsLock {}",
            text.len(),
            match caps_lock {
                Some(true) => "on",
                Some(false) => "off",
                None => "unknown",
            }
        );
        let body = match chrome::paste::compile(&text, layout, caps_lock) {
            Ok(events) => events,
            Err(refusal) => {
                self.refuse_paste(refusal);
                return;
            }
        };
        let job = PasteJob::new(self.outstanding, body, chrome::paste::PACE);
        log::info!(
            "paste: {} key transitions ({} of them releasing what was held), ~{} s at {} ms a key",
            job.total(),
            job.prelude(),
            job.expected_duration().as_secs(),
            crate::script::REPORT_DELAY_MS
        );
        self.paste = PasteState::Running(job);
        self.paste_last = None;
        self.paste_refusal = None;
        self.chrome_dirty = true;
    }

    /// Submit at most one due paste transition. Refusal enters the reducer's release
    /// path, which ends the paste with partial progress and the corresponding reason.
    fn advance_running(&mut self, el: &ActiveEventLoop, now: Instant) {
        if self.session.capture() != CaptureState::Captured {
            self.fail_paste("input capture ended under the paste".to_string());
            return;
        }
        let step = match &self.paste {
            PasteState::Running(job) => job.next_step(now),
            _ => return,
        };
        let trigger = match step {
            None => None,
            Some(crate::input::Event::Key { key, down }) => Some(Trigger::Key { key, down }),
            // The prelude's button releases (`chrome::paste`), and only releases: a *press* would
            // be a click this client never sends by itself, so it ends the job instead.
            Some(crate::input::Event::Button {
                button,
                down: false,
            }) => Some(Trigger::Button {
                mask: button,
                down: false,
            }),
            Some(other) => {
                self.fail_paste(format!(
                    "a paste step was not a key or a button release: {other:?}"
                ));
                return;
            }
        };
        if let Some(trigger) = trigger {
            self.feed(el, trigger);
            match &mut self.paste {
                PasteState::Running(job) => job.delivered(now),
                // `feed` ended the paste itself — a release-all, raised by a refused submission or
                // by the user — in which case the outcome is already recorded.
                _ => return,
            }
            self.chrome_dirty = true;
        }
        self.finish_paste_if_over();
    }

    /// Move a finished job's outcome into the chrome and go back to [`PasteState::Idle`].
    fn finish_paste_if_over(&mut self) {
        let done = match &self.paste {
            PasteState::Running(job) => !job.is_running(),
            _ => false,
        };
        if !done {
            return;
        }
        let PasteState::Running(job) = std::mem::replace(&mut self.paste, PasteState::Idle) else {
            return;
        };
        let outcome = job.outcome().cloned();
        if let Some(outcome) = outcome {
            log::info!("paste {outcome}");
            self.paste_last = Some(outcome);
        }
        self.paste_cancel_at = None;
        self.chrome_dirty = true;
    }

    /// End a running paste as a failure. A no-op when none is running.
    fn fail_paste(&mut self, reason: String) {
        if let PasteState::Running(job) = &mut self.paste {
            job.fail(reason);
        }
        self.finish_paste_if_over();
    }

    /// End paste before release-all, preserving progress and the cancellation or failure reason.
    fn cancel_paste(&mut self, reason: crate::input::ReleaseReason) {
        match std::mem::replace(&mut self.paste, PasteState::Idle) {
            PasteState::Idle => {}
            PasteState::Preparing { .. } => {
                log::info!("paste abandoned before anything was compiled ({reason:?})");
                self.paste_refusal = None;
                self.paste_last = None;
            }
            PasteState::Running(mut job) => {
                let cancellation = job.end(chrome::paste::ending_for(reason));
                log::info!(
                    "paste {} ({reason:?}); the release-all lifts what it still held",
                    cancellation.outcome
                );
                debug_assert!(
                    !self.outstanding.is_empty() || cancellation.still_held.is_empty(),
                    "the job holds keys the viewer does not know about"
                );
                self.paste_last = Some(cancellation.outcome);
            }
        }
        self.paste_cancel_at = None;
        self.chrome_dirty = true;
    }

    /// Hide or show the host cursor, by [`cursor_should_hide`]. Idempotent; called from every
    /// place that can change one of its four inputs.
    fn apply_cursor_policy(&mut self) {
        let Some(w) = self.window.as_ref() else {
            return;
        };
        let want_hidden = cursor_should_hide(
            self.chrome.config.cursor_hidden,
            self.pointer_over_chrome,
            self.grab.is_some(),
            self.pointer_inside,
        );
        if want_hidden != self.cursor_hidden {
            w.set_cursor_visible(!want_hidden);
            self.cursor_hidden = want_hidden;
        }
    }

    /// Persist changed settings, advancing saved state only on success. Save errors are
    /// reported without closing the viewer.
    fn persist_chrome(&mut self) {
        chrome::config::persist(
            self.config.chrome_store.as_ref(),
            &self.chrome.config,
            &mut self.saved_chrome,
        );
    }

    /// Apply the chrome's wheel-direction setting to one wheel event.
    ///
    /// `Natural` passes the sign through unchanged, which is correct without this function:
    /// winit's positive means "content moves down" and the HID wheel field agrees. `Inverted` is
    /// the user saying their target scrolls the other way.
    fn apply_wheel_direction(&self, event: crate::input::Event) -> crate::input::Event {
        match event {
            crate::input::Event::Wheel { delta } => crate::input::Event::Wheel {
                delta: delta.saturating_mul(self.chrome.config.wheel_direction.sign()),
            },
            other => other,
        }
    }

    fn record_latency(&mut self, started: Instant) {
        let us = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        if self.latency_us.len() == LATENCY_SAMPLES {
            self.latency_us.pop_front();
        }
        self.latency_us.push_back(us);
    }

    /// Map pointer coordinates through the event loop's current video rectangle.
    /// Render-thread geometry can lag after a resize and would misplace target input.
    fn cursor_event(&self, position: winit::dpi::PhysicalPosition<f64>) -> Option<Trigger> {
        let frame = self.render_shared.frame_size()?;
        let (x, y) =
            input_map::map_cursor_in_window((position.x, position.y), self.window_size, frame)?;
        Some(Trigger::Motion(crate::input::Event::PointerAbs { x, y }))
    }

    /// The 250 ms housekeeping pass: title, release outcome, deadlines, and the stats line.
    ///
    /// The two exit paths feed `CloseRequested` on every tick until the loop stops, which
    /// can take several. Only the first one releases: the reducer latches the close, so the
    /// epoch is bumped once rather than every 250 ms.
    fn tick(&mut self, el: &ActiveEventLoop) {
        if self.interrupted.load(Ordering::SeqCst) {
            if !self.session.is_closing() {
                log::info!("interrupted; releasing and exiting");
            }
            self.feed(el, Trigger::CloseRequested);
            return;
        }
        if let Some(after) = self.config.exit_after {
            if self.started.elapsed() >= after {
                if !self.session.is_closing() {
                    log::info!("--exit-after {:?} elapsed; exiting", after);
                }
                self.feed(el, Trigger::CloseRequested);
                return;
            }
        }
        // `--capture-on-start`, the other development flag: the Enter edge a user's
        // own keypress produces, fed to the reducer. `capturing_edge` consumes the press and eats
        // the up, so the target sees neither half — this captures, it does not type.
        if self.config.capture_on_start && !self.capture_on_start_done && self.window.is_some() {
            self.capture_on_start_done = true;
            if let Some(key) = input_map::enter_key() {
                log::info!("--capture-on-start: engaging capture without a keypress");
                self.feed(el, Trigger::Key { key, down: true });
                self.feed(el, Trigger::Key { key, down: false });
            }
        }
        let stats = self.producer.stats();
        if self.session.capture() == CaptureState::Engaging {
            self.feed(el, Trigger::Retry);
        } else if self.session.capture() == CaptureState::Captured {
            // Detect producer-driven cancellation even when no host event was submitted.
            if let Some(trigger) = self_release_trigger(self.producer.is_engaged(), stats.link_down)
            {
                log::warn!("the input path ended the session by itself ({trigger:?}); releasing");
                self.feed(el, trigger);
            }
        }
        // One pipeline snapshot per tick, shared by the title and the stats line: the two must
        // not disagree about what the capture path is doing, and sampling twice is how they
        // would.
        let capture = self.pipeline.stats();
        // The Video popover's marker, from the one place that knows it: what the pipeline says it
        // negotiated with the device (`S_FMT`), not the size the last frame's header reported —
        // the two legitimately disagree for a few frames after a change.
        self.negotiated = capture.negotiated_dimensions;
        self.check_release_outcome(&stats);
        self.report_reconnect(&stats);
        self.update_title(&stats, &capture);
        if let Some(interval) = self.config.stats_interval {
            if self.last_stats.elapsed() >= interval {
                self.log_stats(&stats, &capture);
            }
        }
    }

    /// Log each reconnect with the newly queried device information.
    fn report_reconnect(&mut self, stats: &Stats) {
        if stats.reconnects <= self.last_reconnects {
            return;
        }
        self.last_reconnects = stats.reconnects;
        match stats.device_info {
            // `DeviceInfo`'s own `Display`, so this line spells the lock bits the way every
            // other command does: `caps=on`, never `caps=true`.
            Some(info) => log::info!(
                "serial link back after {} reconnect(s), {} attempt(s): {info}. Input stays \
                 released until you capture again.",
                stats.reconnects,
                stats.reconnect_attempts
            ),
            // The writer clears `link_down` only once `GET_INFO` has answered, so this is a
            // race with the snapshot rather than a device that said nothing.
            None => log::info!(
                "serial link back after {} reconnect(s), {} attempt(s)",
                stats.reconnects,
                stats.reconnect_attempts
            ),
        }
    }

    /// An unsent release must remain visible because the target may still hold input.
    fn check_release_outcome(&mut self, stats: &Stats) {
        let was_raised = self.release_notice.raised();
        if let Some(record) = self.release_notice.observe(stats.last_release) {
            match record.outcome {
                ReleaseOutcome::Unsent => log::warn!(
                    "release-all for epoch {} ({:?}) was UNSENT: the target may still be holding \
                     keys",
                    record.epoch,
                    record.reason
                ),
                // A submitted reconnect release clears the earlier unsent warning.
                ReleaseOutcome::Submitted => log::debug!(
                    "release-all for epoch {} ({:?}) submitted",
                    record.epoch,
                    record.reason
                ),
            }
        }
        if self.release_notice.raised() != was_raised {
            self.title.clear();
        }
    }

    /// Measure the facts and hand them to [`title::compose`], which owns the wording.
    ///
    /// Setting the title is a Wayland round trip, so it is done only when the text actually
    /// changed — which is also why the elapsed seconds are rendered at whole-second granularity:
    /// at 4 Hz ticks that is one call per second while something is wrong, and none at all while
    /// everything is fine.
    fn update_title(&mut self, stats: &Stats, capture: &PipelineStats) {
        // Held for the length of the `compose` call, because `AudioTitle::Unavailable` borrows
        // the condition's own wording rather than restating it here (`audio::AudioError` owns
        // what a failure is called).
        let audio_reason;
        let audio_opening;
        let audio = match self.audio {
            None => AudioTitle::Off,
            Some(handle) => {
                let snapshot = handle.snapshot();
                match (&snapshot.condition, snapshot.opening_side()) {
                    (Some(condition), _) => {
                        audio_reason = condition.to_string();
                        AudioTitle::Unavailable(&audio_reason)
                    }
                    // Keep unfinished opens distinct from working audio in diagnostics.
                    (None, Some(side)) => {
                        audio_opening = side.to_string();
                        AudioTitle::Opening(&audio_opening)
                    }
                    (None, None) => AudioTitle::On {
                        muted: handle.muted(),
                        counts: snapshot.counts,
                        ring: handle.config().ring,
                    },
                }
            }
        };
        let title = title::compose(&TitleFacts {
            present_mode: self.present_mode,
            session: self.session,
            pipeline: capture.state,
            disconnected_for: capture.disconnected_since.map(|t| t.elapsed()),
            frames_stalled_for: self.pipeline.frames_stalled_for(),
            link_down: stats.link_down,
            link_down_for: stats.down_since.map(|t| t.elapsed()),
            reconnect_attempts: stats.reconnect_attempts,
            release_unsent: self.release_notice.raised(),
            format_mismatch_accepted: capture.format_mismatch_accepted > 0,
            video_size: capture.last_resolution,
            negotiated_size: capture.negotiated_dimensions,
            audio,
        });
        if title != self.title {
            if let Some(w) = self.window.as_ref() {
                w.set_title(&title);
            }
            // Log title changes so recovery state can be checked without observing the window.
            log::info!("title: {title}");
            self.title = title;
        }
    }

    /// Report capture, presentation, input, and recovery measurements.
    ///
    /// Handler duration and presentation duration are separate measurements; neither
    /// is end-to-end input latency. Audio statistics are included when enabled.
    fn log_stats(&mut self, input: &Stats, p: &PipelineStats) {
        let elapsed = self
            .last_stats
            .elapsed()
            .as_secs_f64()
            .max(f64::MIN_POSITIVE);
        self.last_stats = Instant::now();

        let captured = p.frames_captured;
        let fps = (captured.saturating_sub(self.last_captured)) as f64 / elapsed;
        self.last_captured = captured;

        let RenderStats {
            presented,
            surface_recoveries,
            present_p50_us,
            present_max_us,
            age_p50_us,
            age_max_us,
            egui_p50_us,
            egui_max_us,
        } = self.render_shared.take_stats();

        let mut lat: Vec<u64> = self.latency_us.drain(..).collect();
        lat.sort_unstable();
        let mut build: Vec<u64> = self.chrome_build_us.drain(..).collect();
        build.sort_unstable();
        let pick = |v: &[u64], q: f64| -> u64 {
            if v.is_empty() {
                0
            } else {
                v[((v.len() - 1) as f64 * q).round() as usize]
            }
        };
        let ms = |us: u64| us as f64 / 1000.0;

        let resolution = match p.last_resolution {
            Some((w, h)) => format!("{w}x{h}"),
            None => "none yet".to_string(),
        };
        log::info!(
            "stats capture: {fps:.1} fps, dropped pre/post {}/{}, decode errors {}, capture \
             errors {} | reopens {} in {} attempts, stream restarts {}, format restarts {} \
             ({} reopens, accepted {}), {} resolution changes (now {resolution}) | \
             capture-to-submit age \
             p50 {:.1} max {:.1} ms | present p50 {:.1} max {:.1} ms ({presented} presented, \
             {surface_recoveries} recoveries) | chrome draw p50 {egui_p50_us} max \
             {egui_max_us} µs",
            p.dropped_pre_decode,
            p.dropped_post_decode,
            p.decode_errors,
            p.capture_errors,
            p.reopens,
            p.reopen_attempts,
            p.stream_restarts,
            p.format_mismatch_restarts,
            p.format_mismatch_reopens,
            p.format_mismatch_accepted,
            p.resolution_changes,
            ms(age_p50_us),
            ms(age_max_us),
            ms(present_p50_us),
            ms(present_max_us),
        );

        // "down N ms" only while it means something: on a healthy link there is no outage to
        // time, and a permanent `down 0 ms` would train the reader to ignore the field.
        let down = match input.down_since {
            Some(since) => format!(", down {} ms", since.elapsed().as_millis()),
            None => String::new(),
        };
        log::info!(
            "stats input: queue {}/{} max-in-queue {:.1} ms | reports kb {} abs {} rel {} | \
             coalesced abs {} rel {} wheel {} | cancellations {} overflows {} unmapped keys {} | \
             serial reconnects {} in {} attempts{down} | event-loop handling p50 {} p99 {} max \
             {} µs ({} samples) | chrome build p50 {} p95 {} max {} µs ({} builds)",
            input.queue_depth,
            input.max_queue_depth,
            input.max_time_in_queue.as_secs_f64() * 1000.0,
            input.kb_reports,
            input.abs_reports,
            input.rel_reports,
            input.abs_replaced,
            input.rel_summed,
            input.wheel_summed,
            input.cancellations,
            input.overflows,
            self.unmapped_keys,
            input.reconnects,
            input.reconnect_attempts,
            pick(&lat, 0.50),
            pick(&lat, 0.99),
            lat.last().copied().unwrap_or(0),
            lat.len(),
            pick(&build, 0.50),
            pick(&build, 0.95),
            build.last().copied().unwrap_or(0),
            build.len(),
        );

        // Report audio counters for drift measurements, labeling capacity as configured.
        if let Some(handle) = self.audio {
            let a = handle.snapshot();
            let ring = handle.config().ring;
            let condition = match &a.condition {
                Some(c) => format!(" | {c}"),
                None => String::new(),
            };
            log::info!(
                "stats audio: captured {} played {} silent {} muted {} periods | overruns {} \
                 underruns {} | drift dropped {} inserted {} | opens capture {} playback {} | \
                 buffer {}x{} frames configured ({:?}), muted {}{condition}",
                a.periods_captured,
                a.periods_played,
                a.periods_silent,
                a.periods_muted,
                a.counts.overruns,
                a.counts.underruns,
                a.counts.drift_drops,
                a.counts.drift_inserts,
                a.capture_opens,
                a.playback_opens,
                ring.periods,
                ring.period_frames,
                ring.depth(),
                handle.muted(),
            );
        }
    }
}

/// Hide the host cursor for explicit hiding or relative lock, except where visible
/// viewer controls need a pointer.
fn cursor_should_hide(
    cursor_hidden: bool,
    pointer_over_chrome: bool,
    grabbed: bool,
    pointer_inside: bool,
) -> bool {
    (cursor_hidden && !pointer_over_chrome) || (grabbed && pointer_inside)
}

/// Whether egui also needs the key event to keep its own held-key state consistent.
/// Target-owned releases must not leave a stale press in egui.
fn key_also_goes_to_egui(sink: Sink, state: winit::event::ElementState) -> bool {
    sink != Sink::Chrome && state == winit::event::ElementState::Released
}

/// Whether a resolution the user chose is worth a renegotiation.
///
/// `false` for the mode the device says it has already negotiated: reopening the node to arrive
/// where the stream already is blanks the picture for a second and counts a format change that
/// changed nothing. `None` — nothing negotiated yet — is always worth asking for, because there is
/// no evidence to the contrary.
fn format_change_needed(negotiated: Option<(u32, u32)>, wanted: (u32, u32)) -> bool {
    negotiated != Some(wanted)
}

impl ApplicationHandler<UserEvent> for App<'_> {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title(APP_TITLE)
            .with_resizable(true)
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
        // The Wayland app id is a platform extension; on any other backend this is a no-op we
        // simply do not get.
        #[cfg(target_os = "linux")]
        let attributes = {
            use winit::platform::wayland::WindowAttributesExtWayland;
            attributes.with_name(APP_ID, APP_ID)
        };
        let window = match el.create_window(attributes) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                self.fatal = Some(anyhow::anyhow!("could not create a window: {e}"));
                el.exit();
                return;
            }
        };

        // wgpu setup is the one and only time this thread touches the GPU.
        let gpu = match Gpu::create(Arc::clone(&window)) {
            Ok(g) => g,
            Err(e) => {
                self.fatal = Some(e);
                el.exit();
                return;
            }
        };
        self.present_mode = gpu.present_mode();

        // The chrome's event translator. `None` for the accesskit adapter: nothing here drives a
        // screen reader, and `egui_kittest` supplies its own in the UI tests.
        self.egui_state = Some(egui_winit::State::new(
            self.egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &*window,
            Some(window.scale_factor() as f32),
            None,
            None,
        ));

        let chrome_rx = self.chrome_rx.take().unwrap_or_else(|| chrome::channel().1);
        match render::spawn(
            gpu,
            self.pipeline.output(),
            Arc::clone(&self.render_shared),
            chrome_rx,
        ) {
            Ok(h) => self.render_thread = Some(h),
            Err(e) => {
                self.fatal = Some(anyhow::anyhow!("could not start the render thread: {e}"));
                el.exit();
                return;
            }
        }

        // Shortcut inhibition is optional: without it the compositor keeps its own shortcuts and
        // everything else still works.
        let proxy = self.proxy.clone();
        match ShortcutInhibit::new(
            Arc::clone(&window),
            Arc::new(move |event| {
                // Wakes the event loop; a closed loop means we are shutting down anyway.
                let _ = proxy.send_event(UserEvent::Inhibit(event));
            }),
        ) {
            Ok(i) => self.inhibit = Some(i),
            Err(e) => log::warn!(
                "running without keyboard shortcut inhibition: {e:#}. Compositor shortcuts will \
                 not reach the target."
            ),
        }

        // The first `Resized` usually arrives promptly, but the cursor mapping must not depend on
        // that: seed the size from the window itself.
        let size = window.inner_size();
        self.window_size = (size.width, size.height);
        self.window = Some(window);
        self.next_tick = Instant::now();
        el.set_control_flow(ControlFlow::WaitUntil(self.next_tick));
    }

    /// Route state notifications and input before egui processing. Release edges retain
    /// the owner of their press; local controls must not strand target input.
    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let entered = Instant::now();
        if matches!(
            event,
            WindowEvent::Resized(_)
                | WindowEvent::ScaleFactorChanged { .. }
                | WindowEvent::Focused(_)
                | WindowEvent::CursorEntered { .. }
                | WindowEvent::CursorLeft { .. }
                | WindowEvent::ModifiersChanged(_)
                | WindowEvent::ThemeChanged(_)
        ) {
            self.give_to_egui(&event);
            self.chrome_dirty = true;
        }
        match event {
            WindowEvent::CloseRequested => self.feed(el, Trigger::CloseRequested),
            WindowEvent::Resized(size) => {
                // The render thread owns surface reconfiguration; this only posts the size. The
                // size is also kept here, because the cursor mapping is computed on this thread
                // from it and must be right on the very next `CursorMoved`.
                self.window_size = (size.width, size.height);
                self.render_shared.request_resize(size.width, size.height);
            }
            // Pause hidden-surface presentation because the compositor may withhold callbacks.
            WindowEvent::Occluded(occluded) => {
                log::debug!("window occluded: {occluded}");
                self.render_shared.set_occluded(occluded);
            }
            // The one chord this client has reads this. Tracked here rather than
            // from the key events themselves because winit's own state is the authority on what is
            // held, including modifiers pressed before the window had focus.
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            // `set_cursor_grab` returning `Ok` is not proof the pointer is over the surface;
            // this is.
            WindowEvent::CursorEntered { .. } => {
                self.pointer_inside = true;
                self.hide_cursor_if_confirmed();
            }
            WindowEvent::CursorLeft { .. } => {
                self.pointer_inside = false;
                // The pointer is off the surface, so it is off the pill too. Leaving this set
                // would route the next key to a chrome the pointer is no longer near.
                self.pointer_over_chrome = false;
                self.last_cursor = None;
                self.apply_cursor_policy();
            }
            // Release explicitly on focus loss; compositor inhibitor deactivation is not guaranteed.
            WindowEvent::Focused(false) => self.feed(el, Trigger::FocusLost),
            // `event: ref key` rather than `ref event`, so the whole `WindowEvent` is still
            // nameable inside the arm: `Sink::Chrome` hands `egui_winit` the event as winit
            // delivered it, never a reconstruction — a rebuilt event would need a `DeviceId` this
            // code has no way to forge.
            WindowEvent::KeyboardInput { event: ref key, .. } => {
                // Resolve a repeated paste chord as cancellation before ordinary key routing.
                let action = chrome::resolve_paste_chord(
                    input_map::map_key(key.physical_key, key.state, key.repeat, self.modifiers),
                    self.paste.is_active(),
                );
                let physical_key = key.physical_key;
                let sink = chrome::route_key(&self.route_inputs(), action);
                if key_also_goes_to_egui(sink, key.state) {
                    self.give_to_egui(&event);
                }
                match sink {
                    Sink::Chrome => {
                        self.give_to_egui(&event);
                        self.chrome_dirty = true;
                    }
                    // Count unmapped keys even while paste suppresses forwarding, so unsupported
                    // hardware remains visible in diagnostics.
                    Sink::Dropped => {
                        if action == KeyAction::Unmapped {
                            self.unmapped_keys += 1;
                            log::debug!("unmapped physical key: {physical_key:?}");
                        }
                    }
                    Sink::Target => match action {
                        KeyAction::Forward { key, down } => {
                            self.feed(el, Trigger::Key { key, down })
                        }
                        // Use the common release action so every release trigger also cancels paste.
                        KeyAction::Release => self.feed(el, Trigger::ReleaseKey),
                        KeyAction::Paste => self.start_paste(),
                        // `route_key` sends these to `Dropped`; the arms are here so the match is
                        // exhaustive without a catch-all that could hide a future variant.
                        KeyAction::Swallowed | KeyAction::Unmapped => {}
                    },
                }
            }
            // Reported as the plain transition it is. Whether a left press captures — and whether
            // its release is the one that must be swallowed with it — is the reducer's decision,
            // because a copy of that rule out here is what let the two views of a click drift
            // apart and leave LEFT held on the target (see `viewer::state`).
            WindowEvent::MouseInput { state, button, .. } => {
                // Mapped before routing, because the rule needs the button's identity: the release
                // of a press the target is holding is owed to the target however the pointer has
                // moved since (`chrome::Outstanding`). A button this client does not map has mask
                // 0, which is never outstanding.
                let mask = input_map::map_button(button).unwrap_or(0);
                let down = state.is_pressed();
                match chrome::route_pointer(
                    &self.route_inputs(),
                    PointerKind::Button { mask, down },
                ) {
                    Sink::Chrome => {
                        self.give_to_egui(&event);
                        self.chrome_dirty = true;
                    }
                    Sink::Dropped => {}
                    Sink::Target => {
                        if mask != 0 {
                            self.feed(el, Trigger::Button { mask, down });
                        }
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                match chrome::route_pointer(&self.route_inputs(), PointerKind::Wheel) {
                    Sink::Chrome => {
                        self.give_to_egui(&event);
                        self.chrome_dirty = true;
                    }
                    Sink::Dropped => {}
                    Sink::Target => {
                        if let Some(e) = self.wheel.push(delta) {
                            self.feed(el, Trigger::Motion(self.apply_wheel_direction(e)));
                        }
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.last_cursor = Some((position.x, position.y));
                let was_over = self.pointer_over_chrome;
                self.pointer_over_chrome = self.hit.hits((position.x, position.y));
                // egui hears every motion, whichever way the routing goes: a hover highlight
                // it was never told to clear stays lit, and the pill would look pressed while the
                // pointer is over the target. Only the rebuild is gated — on the pointer being
                // over the chrome or having just left it — so motion over the video costs nothing.
                self.give_to_egui(&event);
                if was_over != self.pointer_over_chrome {
                    // The cursor is hidden over the video and shown over the pill, so crossing the
                    // edge is when that has to be re-decided (`cursor_should_hide`).
                    self.apply_cursor_policy();
                }
                if was_over || self.pointer_over_chrome {
                    self.chrome_dirty = true;
                }
                if chrome::route_pointer(&self.route_inputs(), PointerKind::Motion) == Sink::Target
                    // Relative mode takes its motion from `DeviceEvent::MouseMotion`; under a
                    // locked pointer winit delivers no `CursorMoved` at all.
                    && self.config.pointer == PointerMode::Absolute
                {
                    if let Some(t) = self.cursor_event(position) {
                        self.feed(el, t);
                    }
                }
            }
            _ => {}
        }
        self.record_latency(entered);
    }

    fn device_event(&mut self, el: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        let entered = Instant::now();
        if let DeviceEvent::MouseMotion { delta } = event {
            if self.config.pointer == PointerMode::Relative {
                // Relative motion arriving is the other confirmation that the pointer is really
                // over the surface and the lock is in force.
                self.pointer_inside = true;
                self.hide_cursor_if_confirmed();
                if let Some(e) = self.rel.push(delta) {
                    self.feed(el, Trigger::Motion(e));
                }
            }
        }
        self.record_latency(entered);
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: UserEvent) {
        match event {
            // `active` is not a focus signal and means nothing here; `inactive` is the user's
            // Mod+Escape, a compositor-driven request to be let out.
            UserEvent::Inhibit(InhibitEvent::Active) => {}
            UserEvent::Inhibit(InhibitEvent::Inactive) => {
                log::info!("compositor deactivated the shortcut inhibitor; releasing capture");
                self.feed(el, Trigger::InhibitorInactive);
            }
        }
    }

    /// Schedule statistics and UI repaints on the event loop’s shared deadline.
    /// The render thread stays reactive; `RedrawRequested` is unused.
    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let now = Instant::now();
        if now >= self.next_tick {
            self.tick(el);
            let interval = if self.session.capture() == CaptureState::Engaging {
                ENGAGING_TICK
            } else {
                TICK
            };
            self.next_tick = Instant::now() + interval;
            // The tick moved the facts the pill shows — capture state, the audio counters, the
            // negotiated mode — so it is also a reason to rebuild.
            self.chrome_dirty = true;
        }
        // The render thread cannot exit the loop by itself; check its fatal slot on the tick.
        if self.fatal.is_none() {
            if let Some(msg) = self.render_shared.fatal() {
                self.fatal = Some(anyhow::anyhow!("{msg}"));
                self.feed(el, Trigger::CloseRequested);
            }
        }
        // The paste's clock, folded into the same loop as everything else: a job
        // is advanced by at most one key transition per pass, and the pass that runs it is this
        // one. Before the chrome build, so a step taken now is on screen in the same frame.
        self.advance_paste(el);
        let repaint_due = self.repaint_at.is_some_and(|at| Instant::now() >= at);
        if (self.chrome_dirty || repaint_due) && self.window.is_some() {
            self.build_chrome(el);
        }
        let mut wake = self.next_tick;
        if let Some(at) = self.repaint_at {
            wake = wake.min(at);
        }
        for at in self.paste_wakeups() {
            wake = wake.min(at);
        }
        el.set_control_flow(ControlFlow::WaitUntil(wake));
    }

    fn exiting(&mut self, _el: &ActiveEventLoop) {
        // This is where teardown has to happen, not in `run` afterwards: winit calls `exiting`
        // while the event loop — and therefore its `wl_display` — is still alive, and the shortcut
        // inhibitor borrows that display (see `viewer::wayland`'s module docs). `run_app` consumes
        // the loop, so by the time `run` regains control there is nothing left to disarm against.
        self.teardown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hide the host cursor over video only; local controls still need a visible pointer.
    #[test]
    fn the_hidden_cursor_comes_back_over_the_chrome() {
        // The setting on, the pointer over the video: hidden, which is the point of the setting.
        assert!(cursor_should_hide(true, false, false, true));
        // The same setting, the pointer over the pill: shown.
        assert!(!cursor_should_hide(true, true, false, true));
    }

    /// The grab's half of the policy is independent of the setting and of the chrome: under a lock
    /// the chrome cannot be reached at all (`chrome::route`), so `pointer_over_chrome` is stale by
    /// definition and must not bring the cursor back.
    #[test]
    fn a_confirmed_grab_hides_the_cursor_whatever_the_setting_says() {
        assert!(cursor_should_hide(false, false, true, true));
        assert!(cursor_should_hide(false, true, true, true));
        // A grab that is only *requested* — the pointer is not over the surface yet — hides
        // nothing: the cursor would vanish while it is still over another window.
        assert!(!cursor_should_hide(false, false, true, false));
        // And with neither reason, it stays visible.
        assert!(!cursor_should_hide(false, false, false, true));
    }

    /// Clear egui’s held-key state when releases bypass it for the target.
    /// Otherwise a later local event can inherit stale modifiers.
    #[test]
    fn every_key_release_is_also_given_to_egui() {
        use winit::event::ElementState::{Pressed, Released};
        for sink in [Sink::Target, Sink::Dropped] {
            assert!(
                key_also_goes_to_egui(sink, Released),
                "a release routed to {sink:?} must still reach egui"
            );
            assert!(
                !key_also_goes_to_egui(sink, Pressed),
                "a press routed to {sink:?} must not also reach egui"
            );
        }
        // The chrome's own arm hands the event over; a second copy would be a duplicate event.
        assert!(!key_also_goes_to_egui(Sink::Chrome, Released));
        assert!(!key_also_goes_to_egui(Sink::Chrome, Pressed));
    }

    /// Selecting the current mode must not interrupt an established stream.
    #[test]
    fn the_negotiated_mode_is_not_renegotiated() {
        assert!(
            format_change_needed(None, (1920, 1080)),
            "nothing negotiated yet: there is no evidence against asking"
        );
        assert!(format_change_needed(Some((1920, 1080)), (1280, 720)));
        assert!(!format_change_needed(Some((1280, 720)), (1280, 720)));
    }
}
