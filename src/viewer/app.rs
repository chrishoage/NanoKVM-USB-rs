//! The winit application: the window, the event loop, and everything wired to it (plan §2.6,
//! §4.1, §5.4, §5.5, §6.1, §12 Stage 1).
//!
//! # Thread ownership
//!
//! This is the event loop thread. It owns the window, the input [`Producer`], the capture state
//! machine and the shortcut inhibitor handle. It creates the wgpu objects once, in `resumed`,
//! and then **never touches wgpu again** (§5.4): the `nanokvm-render` thread owns them, and the
//! two communicate through [`RenderShared`].
//!
//! Nothing here blocks. [`Producer::submit`] is non-blocking (§2.9), [`Producer::engage`] is
//! retried rather than waited on (§2.6), and the render thread's `present()` — which blocks for
//! about 99 % of every frame — is on the other side of a slot. That separation is the whole point
//! of §5.4, and the `--stats-interval` line is its acceptance measurement: event-loop handling
//! latency in microseconds printed next to a present time of a whole frame period.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{CursorGrabMode, Window, WindowId};

use crate::capture::{PipelineHandle, PipelineState};
use crate::input::{Producer, ReleaseOutcome, Stats};
use crate::viewer::input_map::{self, KeyAction, RelAccumulator, WheelAccumulator};
use crate::viewer::render::{self, Gpu, RenderShared, RenderStats};
use crate::viewer::state::{reduce, self_release_trigger, Action, CaptureState, Session, Trigger};
use crate::viewer::wayland::{InhibitEvent, ShortcutInhibit};
use crate::viewer::RELEASE_KEY;

/// The window title's fixed part.
const APP_TITLE: &str = "NanoKVM-USB";
/// Wayland application id, so the compositor can match window rules to this client.
const APP_ID: &str = "nanokvm-usb";

/// How often the title and the release-outcome check run while released or captured.
const TICK: Duration = Duration::from_millis(250);
/// How often they run while `Engaging`, where a retry of [`Producer::engage`] is pending and the
/// writer's acknowledgement is expected within a few milliseconds (§5.1: 4–17 ms per report).
const ENGAGING_TICK: Duration = Duration::from_millis(10);
/// Event-handling latency samples kept for the stats line.
const LATENCY_SAMPLES: usize = 4096;
/// How long frames must be absent before the title says so (§6.1 S1-2). Matches the pipeline's
/// own stall threshold.
const STALL_NOTICE: Duration = Duration::from_millis(500);
/// How long teardown waits for the render thread before detaching it. Two `FRAME_WAIT` periods
/// plus room for one `present()`: enough for a renderer that is merely busy, and far short of the
/// unbounded wait a surface nobody is compositing produces (§5.4, measured at `present p50 999 ms`
/// on a locked session).
const RENDER_JOIN: Duration = Duration::from_millis(500);

/// How the pointer is delivered to the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PointerMode {
    /// Absolute positioning through [`crate::proto::abs_coord`] (§3.4). The default: relative
    /// motion cannot reach a specific coordinate, because the target applies pointer acceleration
    /// and magnitudes do not survive (§3.4, measured).
    #[default]
    Absolute,
    /// Relative motion, with the pointer locked to the window. For targets whose pointer does not
    /// follow absolute reports.
    Relative,
}

/// What the viewer needs from the command line.
#[derive(Debug, Clone, Copy)]
pub struct ViewerConfig {
    pub pointer: PointerMode,
    /// How often to log the statistics line; `None` disables it.
    pub stats_interval: Option<Duration>,
    /// Quit cleanly after this long. A development flag (§9.3: hardware runs must be bounded).
    pub exit_after: Option<Duration>,
}

/// Things that reach the event loop from other threads.
///
/// Only the Wayland dispatch thread needs to wake the loop. The render thread's failures are
/// polled instead, on the tick: it has no proxy of its own and a fatal render error is not urgent
/// to the quarter-second.
#[derive(Debug, Clone, Copy)]
pub enum UserEvent {
    /// The compositor changed the inhibitor's state (§1.5, q6c).
    Inhibit(InhibitEvent),
}

/// Run the viewer. Blocks until the window closes, the `--exit-after` deadline passes, or
/// `SIGINT` arrives.
///
/// `interrupted` is polled on the tick rather than acted on from the signal handler, because the
/// only async-signal-safe thing a handler may do is set a flag. The exit path is the same one
/// `CloseRequested` takes, so a release-all is always requested first (§2.6).
///
/// The teardown below runs whether or not the loop returned an error. A loop that failed still
/// leaves a render thread to stop and a pointer grab to give back, and the caller still has to
/// reach `WriterHandle::shutdown` — its release-all is the last thing that can unstick a key held
/// on the target (§2.6.1), so nothing here may return early past it.
pub fn run(
    pipeline: &PipelineHandle,
    producer: Producer,
    config: ViewerConfig,
    interrupted: &'static AtomicBool,
) -> anyhow::Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let mut app = App::new(pipeline, producer, config, proxy, interrupted);
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

struct App<'a> {
    pipeline: &'a PipelineHandle,
    producer: Producer,
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
    /// the render thread (§3.4; [`RenderShared::frame_size`]).
    window_size: (u32, u32),
    /// Whether the pointer is known to be over the surface. `set_cursor_grab` returning `Ok` only
    /// means the request was sent — `zwp_locked_pointer_v1` takes effect once the pointer is
    /// inside (q6b) — so the cursor is hidden on that confirmation, never on the `Ok`.
    pointer_inside: bool,
    cursor_hidden: bool,
    /// Teardown has already run. It is called from `exiting` and again from `run`; the second call
    /// happens after the event loop — and with it the window and the Wayland display — is gone, so
    /// it must do nothing rather than call into either.
    torn_down: bool,
    rel: RelAccumulator,
    wheel: WheelAccumulator,

    started: Instant,
    next_tick: Instant,
    last_stats: Instant,
    last_captured: u64,
    latency_us: VecDeque<u64>,
    unmapped_keys: u64,
    /// The epoch of the last release whose outcome has already been reported, so an `Unsent`
    /// notice is raised once per release rather than every tick (§2.6.1).
    reported_release_epoch: Option<u64>,
    release_notice: bool,
    title: String,
    fatal: Option<anyhow::Error>,
}

impl<'a> App<'a> {
    fn new(
        pipeline: &'a PipelineHandle,
        producer: Producer,
        config: ViewerConfig,
        proxy: EventLoopProxy<UserEvent>,
        interrupted: &'static AtomicBool,
    ) -> Self {
        let now = Instant::now();
        App {
            pipeline,
            producer,
            config,
            proxy,
            interrupted,
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
            reported_release_epoch: None,
            release_notice: false,
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

    /// Stop the render thread and give the compositor and the pointer back. Idempotent.
    ///
    /// **The join is bounded.** The render thread spends almost all of its life inside `present()`
    /// (§5.4), and `present()` waits on a compositor frame callback that never comes when the
    /// surface is not being composited — 999 ms at p50 on a locked session, and unbounded in
    /// principle. Joining unconditionally would hang the whole shutdown there, and everything that
    /// matters happens *after* it: the release-all has been requested but `main`'s
    /// `writer.shutdown()` is what waits for it and reports whether it was submitted (§2.6.1). So a
    /// renderer that has not come back within [`RENDER_JOIN`] is reported and detached; the process
    /// is exiting anyway.
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
                log::warn!(
                    "the render thread has not finished within {RENDER_JOIN:?} — it is most \
                     likely blocked in present() on a surface the compositor is not scheduling. \
                     Detaching it so shutdown can continue; the release-all has already been \
                     requested."
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
                // Entering `Engaging` shortens the tick immediately. Leaving `next_tick` where the
                // 250 ms cadence put it would give re-capture a dead zone of up to a whole tick in
                // which the retry never runs and every event that arrives is dropped (§2.6: no
                // input flows until the acknowledgement lands).
                self.next_tick = Instant::now() + ENGAGING_TICK;
            }
        }
        self.session = next;
        for action in actions {
            self.perform(el, action);
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
                // event or tick retries (§2.6).
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
                self.producer.request_release_all(reason);
                self.rel = RelAccumulator::default();
                self.wheel = WheelAccumulator::default();
            }
            Action::Forward(event) => {
                if let Err(e) = self.producer.submit(event) {
                    // §2.8: an overflow is a failed session, not a dropped event. Surface it and
                    // require deliberate recapture.
                    log::warn!("input submission refused: {e}");
                    self.feed(el, Trigger::SubmitFailed(e));
                }
            }
            Action::Exit => el.exit(),
        }
    }

    /// Lock the pointer for relative mode.
    ///
    /// `Ok(())` from `set_cursor_grab` means the request was *sent*, not that the pointer is
    /// locked: `zwp_locked_pointer_v1` only takes effect once the pointer is inside the surface
    /// (q6b, measured — in a run with the cursor outside the window both modes returned `Ok(())`
    /// and no relative motion was ever delivered). So `Ok` is recorded as "requested" and nothing
    /// visible follows from it: the cursor is hidden only once the pointer is confirmed to be over
    /// the surface, by `CursorEntered` or by relative motion actually arriving. Hiding on the `Ok`
    /// alone makes the cursor vanish while it is still over another window.
    ///
    /// Absolute mode deliberately does not grab. The target's own cursor is in the video, the
    /// host cursor is what points at it, and confining it would take the pointer away from the
    /// user with no gain.
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

    /// Hide the cursor if a grab is up **and** the pointer has been confirmed over the surface.
    /// Idempotent; called from every place that can supply the confirmation.
    fn hide_cursor_if_confirmed(&mut self) {
        if self.cursor_hidden || self.grab.is_none() || !self.pointer_inside {
            return;
        }
        if let Some(w) = self.window.as_ref() {
            w.set_cursor_visible(false);
            self.cursor_hidden = true;
            log::debug!("pointer confirmed over the surface; cursor hidden");
        }
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
        w.set_cursor_visible(true);
        self.cursor_hidden = false;
    }

    fn record_latency(&mut self, started: Instant) {
        let us = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        if self.latency_us.len() == LATENCY_SAMPLES {
            self.latency_us.pop_front();
        }
        self.latency_us.push_back(us);
    }

    /// Absolute pointer position, mapped through the video rectangle (§3.4).
    ///
    /// The rectangle is computed here, from **this thread's** window size and the frame dimensions
    /// the render thread published. Taking a ready-made rectangle from the render thread instead
    /// made every cursor position wrong for as long as it took that thread to notice the resize
    /// and present again — up to a frame wait plus a present, about 110 ms — and a cursor mapped
    /// through the old rectangle lands on the wrong pixel of a live console. The window size is
    /// known here the instant `Resized` arrives; the frame dimensions change only when the target's
    /// resolution does (§6, A6), so they are safe to read across the seam.
    ///
    /// `None` before the first frame or while the window is degenerate — there is no coordinate to
    /// send, and inventing one would move a pointer on a live console.
    fn cursor_event(&self, position: winit::dpi::PhysicalPosition<f64>) -> Option<Trigger> {
        let frame = self.render_shared.frame_size()?;
        let (x, y) =
            input_map::map_cursor_in_window((position.x, position.y), self.window_size, frame)?;
        Some(Trigger::Motion(crate::input::Event::PointerAbs { x, y }))
    }

    /// The 250 ms housekeeping pass: title, release outcome, deadlines, and the stats line.
    ///
    /// The two exit paths feed `CloseRequested` on every tick until the loop actually stops, which
    /// can take several. Only the first one releases: the reducer latches the close (§2.6), so the
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
        let stats = self.producer.stats();
        if self.session.capture() == CaptureState::Engaging {
            self.feed(el, Trigger::Retry);
        } else if self.session.capture() == CaptureState::Captured {
            // The producer can end the session by itself, with no event ever reaching this loop
            // (see `state::self_release_trigger`). Ask, and run the §2.6 release for whatever it
            // reports so the user is told to re-capture deliberately (§2.8).
            if let Some(trigger) = self_release_trigger(self.producer.is_engaged(), stats.link_down)
            {
                log::warn!("the input path ended the session by itself ({trigger:?}); releasing");
                self.feed(el, trigger);
            }
        }
        self.check_release_outcome(&stats);
        self.update_title(&stats);
        if let Some(interval) = self.config.stats_interval {
            if self.last_stats.elapsed() >= interval {
                self.log_stats(&stats);
            }
        }
    }

    /// §2.6.1: an `Unsent` release means the target may still be holding keys. Say so rather than
    /// hiding it behind cleared local state.
    fn check_release_outcome(&mut self, stats: &Stats) {
        let Some(record) = stats.last_release else {
            return;
        };
        if self.reported_release_epoch == Some(record.epoch) {
            return;
        }
        self.reported_release_epoch = Some(record.epoch);
        match record.outcome {
            ReleaseOutcome::Unsent => {
                log::warn!(
                    "release-all for epoch {} ({:?}) was UNSENT: the target may still be holding \
                     keys",
                    record.epoch,
                    record.reason
                );
                self.release_notice = true;
                self.title.clear();
            }
            ReleaseOutcome::Submitted => {
                log::debug!(
                    "release-all for epoch {} ({:?}) submitted",
                    record.epoch,
                    record.reason
                );
                if self.release_notice {
                    self.release_notice = false;
                    self.title.clear();
                }
            }
        }
    }

    fn update_title(&mut self, stats: &Stats) {
        let mut title = format!(
            "{APP_TITLE} — {} — {}",
            self.present_mode,
            self.session.capture().title_fragment(RELEASE_KEY)
        );
        // §6.1 S1-2: the tool says frames stopped arriving. It does not say the signal is gone —
        // this hardware cannot report that, and pixel content is never evidence of it (A5).
        match self.pipeline.state() {
            PipelineState::Disconnected => title.push_str(" — capture device disconnected"),
            PipelineState::Stopped => title.push_str(" — capture stopped"),
            _ => {
                if let Some(since) = self.pipeline.frames_stalled_for() {
                    if since >= STALL_NOTICE {
                        title.push_str(&format!(" — frames stopped {}s ago", since.as_secs()));
                    }
                } else {
                    title.push_str(" — waiting for the first frame");
                }
            }
        }
        if stats.link_down {
            title.push_str(" — serial DOWN");
        }
        if self.release_notice {
            title.push_str(" — release UNSENT — target may still hold keys");
        }
        if title != self.title {
            if let Some(w) = self.window.as_ref() {
                w.set_title(&title);
            }
            self.title = title;
        }
    }

    /// The Stage 1 exit measurement for §5.4.
    ///
    /// "Input latency must not correlate with render timing" is a behavioural claim, and this is
    /// the number that supports it: the event loop's own handling latency printed **next to** the
    /// render thread's present time. The render thread sits inside `present()` for a whole
    /// refresh period; if that were on the event loop, the handling figures would be milliseconds
    /// rather than microseconds.
    fn log_stats(&mut self, input: &Stats) {
        let elapsed = self
            .last_stats
            .elapsed()
            .as_secs_f64()
            .max(f64::MIN_POSITIVE);
        self.last_stats = Instant::now();

        let p = self.pipeline.stats();
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
        } = self.render_shared.take_stats();

        let mut lat: Vec<u64> = self.latency_us.drain(..).collect();
        lat.sort_unstable();
        let pick = |v: &[u64], q: f64| -> u64 {
            if v.is_empty() {
                0
            } else {
                v[((v.len() - 1) as f64 * q).round() as usize]
            }
        };
        let ms = |us: u64| us as f64 / 1000.0;

        log::info!(
            "stats: capture {fps:.1} fps (dropped pre-decode {} post-decode {}, decode errors {}, \
             capture errors {}) | capture-to-submit age p50 {:.1} ms max {:.1} ms | present p50 \
             {:.1} ms max {:.1} ms ({presented} presented, {surface_recoveries} surface \
             recoveries) | event-loop handling p50 {} µs p99 {} µs max {} µs ({} samples) | input \
             queue {}/{} max-in-queue {:.1} ms | reports kb {} abs {} rel {} | coalesced abs {} \
             rel {} wheel {} | cancellations {} overflows {} unmapped keys {}",
            p.dropped_pre_decode,
            p.dropped_post_decode,
            p.decode_errors,
            p.capture_errors,
            ms(age_p50_us),
            ms(age_max_us),
            ms(present_p50_us),
            ms(present_max_us),
            pick(&lat, 0.50),
            pick(&lat, 0.99),
            lat.last().copied().unwrap_or(0),
            lat.len(),
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
        );
    }
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

        // wgpu setup is the one and only time this thread touches the GPU (§5.4).
        let gpu = match Gpu::create(Arc::clone(&window)) {
            Ok(g) => g,
            Err(e) => {
                self.fatal = Some(e);
                el.exit();
                return;
            }
        };
        self.present_mode = gpu.present_mode();
        match render::spawn(gpu, self.pipeline.output(), Arc::clone(&self.render_shared)) {
            Ok(h) => self.render_thread = Some(h),
            Err(e) => {
                self.fatal = Some(anyhow::anyhow!("could not start the render thread: {e}"));
                el.exit();
                return;
            }
        }

        // Shortcut inhibition is optional: without it the compositor keeps its own shortcuts and
        // everything else still works (§1.5).
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
        // that: seed the size from the window itself (§3.4).
        let size = window.inner_size();
        self.window_size = (size.width, size.height);
        self.window = Some(window);
        self.next_tick = Instant::now();
        el.set_control_flow(ControlFlow::WaitUntil(self.next_tick));
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let entered = Instant::now();
        match event {
            WindowEvent::CloseRequested => self.feed(el, Trigger::CloseRequested),
            WindowEvent::Resized(size) => {
                // The render thread owns surface reconfiguration; this only posts the size. The
                // size is also kept here, because the cursor mapping is computed on this thread
                // from it and must be right on the very next `CursorMoved` (§3.4).
                self.window_size = (size.width, size.height);
                self.render_shared.request_resize(size.width, size.height);
            }
            // §5.4: while the compositor is not showing this surface it schedules no frame
            // callbacks, so `present()` blocks for as long as it likes. The render thread stops
            // presenting rather than parking in it, which is what lets teardown finish.
            WindowEvent::Occluded(occluded) => {
                log::debug!("window occluded: {occluded}");
                self.render_shared.set_occluded(occluded);
            }
            // q6b: `set_cursor_grab` returning `Ok` is not proof the pointer is over the surface;
            // this is.
            WindowEvent::CursorEntered { .. } => {
                self.pointer_inside = true;
                self.hide_cursor_if_confirmed();
            }
            WindowEvent::CursorLeft { .. } => self.pointer_inside = false,
            // §2.6 and §12 Stage 1: niri will not deactivate the inhibitor for us, so release on
            // focus loss is client-driven and hangs off exactly this event.
            WindowEvent::Focused(false) => self.feed(el, Trigger::FocusLost),
            WindowEvent::KeyboardInput { event, .. } => {
                match input_map::map_key(event.physical_key, event.state, event.repeat) {
                    KeyAction::Forward { key, down } => self.feed(el, Trigger::Key { key, down }),
                    KeyAction::Release => self.feed(el, Trigger::ReleaseKey),
                    KeyAction::Swallowed => {}
                    KeyAction::Unmapped => {
                        self.unmapped_keys += 1;
                        log::debug!("unmapped physical key: {:?}", event.physical_key);
                    }
                }
            }
            // Reported as the plain transition it is. Whether a left press captures — and whether
            // its release is the one that must be swallowed with it — is the reducer's decision,
            // because a copy of that rule out here is what let the two views of a click drift
            // apart and leave LEFT held on the target (see `viewer::state`).
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(mask) = input_map::map_button(button) {
                    self.feed(
                        el,
                        Trigger::Button {
                            mask,
                            down: state.is_pressed(),
                        },
                    );
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(e) = self.wheel.push(delta) {
                    self.feed(el, Trigger::Motion(e));
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                // Relative mode takes its motion from `DeviceEvent::MouseMotion`; under a locked
                // pointer winit delivers no `CursorMoved` at all (q6b).
                if self.config.pointer == PointerMode::Absolute {
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
                // over the surface and the lock is in force (q6b).
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
            // Mod+Escape, a compositor-driven request to be let out (q6c).
            UserEvent::Inhibit(InhibitEvent::Active) => {}
            UserEvent::Inhibit(InhibitEvent::Inactive) => {
                log::info!("compositor deactivated the shortcut inhibitor; releasing capture");
                self.feed(el, Trigger::InhibitorInactive);
            }
        }
    }

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
        }
        // The render thread cannot exit the loop by itself; check its fatal slot on the tick.
        if self.fatal.is_none() {
            if let Some(msg) = self.render_shared.fatal() {
                self.fatal = Some(anyhow::anyhow!("{msg}"));
                self.feed(el, Trigger::CloseRequested);
            }
        }
        el.set_control_flow(ControlFlow::WaitUntil(self.next_tick));
    }

    fn exiting(&mut self, _el: &ActiveEventLoop) {
        // **This** is where teardown has to happen, not in `run` afterwards: winit calls `exiting`
        // while the event loop — and therefore its `wl_display` — is still alive, and the shortcut
        // inhibitor borrows that display (see `viewer::wayland`'s module docs). `run_app` consumes
        // the loop, so by the time `run` regains control there is nothing left to disarm against.
        self.teardown();
    }
}
