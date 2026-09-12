//! Keyboard-shortcut inhibition on winit's Wayland connection.
//!
//! A separate event queue owns the inhibitor proxies while winit owns the display and
//! surface. Its dispatch thread must stop before that display is closed. Foreign-proxy
//! conversion requires the shared system Wayland backend used by winit.
//!
//! Compositor deactivation releases capture. Focus loss also releases explicitly because
//! the compositor need not deactivate the inhibitor when focus changes.

use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::{anyhow, Context};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_client::backend::{Backend, ObjectId, WaylandError};
use wayland_client::protocol::{wl_registry, wl_seat, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1 as inhibit_mgr,
    zwp_keyboard_shortcuts_inhibitor_v1 as inhibitor,
};
use winit::window::Window;

/// What the compositor said about the inhibitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InhibitEvent {
    /// Shortcuts are being delivered to this client. Not a statement about focus.
    Active,
    /// The compositor took inhibition back — on niri, the user pressed `Mod+Escape`. This is the
    /// compositor-driven "let me out" signal.
    Inactive,
}

/// How this module tells the event loop something happened. Called from the dispatch thread, so
/// it must be cheap and must not block — the app's implementation wakes winit with an
/// `EventLoopProxy`.
pub type Notify = Arc<dyn Fn(InhibitEvent) + Send + Sync>;

/// The dispatch thread's state: the globals it binds and the handles it reports through.
struct WlState {
    seat: Option<wl_seat::WlSeat>,
    manager: Option<inhibit_mgr::ZwpKeyboardShortcutsInhibitManagerV1>,
    active: Arc<AtomicBool>,
    notify: Notify,
}

impl Dispatch<wl_registry::WlRegistry, ()> for WlState {
    fn event(
        st: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_seat" => {
                // Version 7 is enough for an inhibitor; asking for more than the compositor
                // offers is a protocol error.
                st.seat = Some(registry.bind(name, version.min(7), qh, ()));
            }
            "zwp_keyboard_shortcuts_inhibit_manager_v1" => {
                st.manager = Some(registry.bind(name, 1, qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<inhibitor::ZwpKeyboardShortcutsInhibitorV1, ()> for WlState {
    fn event(
        st: &mut Self,
        _: &inhibitor::ZwpKeyboardShortcutsInhibitorV1,
        event: inhibitor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let reported = match event {
            inhibitor::Event::Active => InhibitEvent::Active,
            inhibitor::Event::Inactive => InhibitEvent::Inactive,
            // The interface is version 1 and `#[non_exhaustive]`; an unknown event says nothing
            // about the inhibitor's state, so nothing changes.
            _ => return,
        };
        st.active
            .store(reported == InhibitEvent::Active, Ordering::SeqCst);
        log::debug!("shortcut inhibitor: {reported:?}");
        (st.notify)(reported);
    }
}

macro_rules! ignore_events {
    ($($t:ty),*) => { $(
        impl Dispatch<$t, ()> for WlState {
            fn event(
                _: &mut Self,
                _: &$t,
                _: <$t as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    )* };
}
ignore_events!(
    wl_seat::WlSeat,
    inhibit_mgr::ZwpKeyboardShortcutsInhibitManagerV1
);

/// A keyboard-shortcut inhibitor for one window, armed and disarmed by the event loop.
///
/// Created once; [`ShortcutInhibit::arm`] and [`ShortcutInhibit::disarm`] are called on every
/// capture and release. Arming a second time is a no-op, so the caller does not have to track it.
pub struct ShortcutInhibit {
    /// Held so the surface these proxies wrap cannot be destroyed under them.
    _window: Arc<Window>,
    conn: Connection,
    qh: QueueHandle<WlState>,
    surface: wl_surface::WlSurface,
    seat: wl_seat::WlSeat,
    manager: inhibit_mgr::ZwpKeyboardShortcutsInhibitManagerV1,
    inhibitor: Option<inhibitor::ZwpKeyboardShortcutsInhibitorV1>,
    active: Arc<AtomicBool>,
    /// Asks the dispatch thread to leave its next poll. See the module docs.
    stop: Arc<AtomicBool>,
    dispatch: Option<JoinHandle<()>>,
    /// Set once winit's `wl_display` is known to be gone; every Wayland call is then skipped
    /// because the objects behind it no longer exist. See [`ShortcutInhibit::abandon_display`].
    display_gone: bool,
}

/// How long the dispatch thread waits in `poll` before re-checking the stop flag. Short enough
/// that a shutdown is not noticeably delayed, long enough that an idle client is not polling in a
/// tight loop.
const POLL_TIMEOUT_MS: libc::c_int = 50;

/// The dispatch thread body: a wakeable `blocking_dispatch`.
///
/// `EventQueue::blocking_dispatch` cannot be used here because it blocks in the socket read with
/// no timeout and no way to be woken; a thread inside it outlives the display it is reading (see
/// the module docs). This is the same sequence with the read broken into the two halves
/// wayland-client exposes for this: `prepare_read()` registers the intent to read and
/// hands over the socket fd, `poll` waits on that fd with a timeout, and `read()` performs the
/// synchronised read. Dropping the guard instead cancels the prepared read, which is what the
/// timeout path does before looping round to look at `stop`.
fn dispatch_loop(queue: &mut EventQueue<WlState>, state: &mut WlState, stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        if let Err(e) = queue.dispatch_pending(state) {
            log::debug!("wayland dispatch ended: {e}");
            return;
        }
        if let Err(e) = queue.flush() {
            log::debug!("wayland flush failed, dispatch ending: {e}");
            return;
        }
        // `None` means events are already queued: go round and dispatch them rather than sleeping.
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let mut fds = [libc::pollfd {
            fd: guard.connection_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        // SAFETY: `fds` is one initialised `pollfd` owned by this frame, and the count passed is
        // its length. The fd is borrowed from `guard`, which is alive for the whole call,
        // so it cannot be closed underneath `poll`. `poll` does not retain the pointer.
        let ready =
            unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, POLL_TIMEOUT_MS) };
        if ready < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            log::debug!("polling the wayland socket failed, dispatch ending: {err}");
            return;
        }
        if ready == 0 {
            // Timed out. Dropping `guard` cancels the prepared read; the loop re-checks `stop`.
            continue;
        }
        match guard.read() {
            Ok(_) => {}
            // Another thread got there first, or the readiness was spurious. Neither is an error.
            Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                log::debug!("reading the wayland socket ended the dispatch thread: {e}");
                return;
            }
        }
    }
    log::debug!("wayland dispatch queue ended");
}

impl ShortcutInhibit {
    /// Bind the inhibit manager on a second event queue over `window`'s Wayland connection.
    ///
    /// # Errors
    ///
    /// Every failure here is a reason to run *without* inhibition rather than to stop: the
    /// display is not Wayland (X11 or a headless run), the compositor does not advertise
    /// `zwp_keyboard_shortcuts_inhibit_manager_v1`, or the surface proxy could not be re-wrapped.
    /// Callers log the error and carry on; the only consequence is that the compositor keeps its
    /// own shortcuts.
    pub fn new(window: Arc<Window>, notify: Notify) -> anyhow::Result<Self> {
        let display = window
            .display_handle()
            .context("no display handle for this window")?
            .as_raw();
        let surface_handle = window
            .window_handle()
            .context("no window handle for this window")?
            .as_raw();
        let (display_ptr, surface_ptr) = match (display, surface_handle) {
            (RawDisplayHandle::Wayland(d), RawWindowHandle::Wayland(w)) => {
                (d.display.as_ptr(), w.surface.as_ptr())
            }
            (d, _) => {
                return Err(anyhow!(
                    "this window is not on Wayland ({d:?}); shortcut inhibition is a Wayland \
                     protocol and is unavailable"
                ))
            }
        };

        // SAFETY: `display_ptr` is the `wl_display` winit created for this window and still owns,
        // read out of a live `DisplayHandle` immediately above. `from_foreign_display` borrows it:
        // `Drop` will not call `wl_display_disconnect`, so winit's connection is unaffected. The borrow is only sound while winit's display lives, and winit disconnects
        // it when its event loop is dropped — which `run_app` does on return. The invariant this
        // relies on is therefore not the `Arc<Window>` below (a window handle does not keep the
        // connection open): it is that every user of this `Connection` — this struct's methods and
        // the dispatch thread it owns — is finished before the event loop is dropped. `Drop` here
        // enforces the thread half by joining it, and the viewer enforces the other half by
        // dropping this value from `ApplicationHandler::exiting`, which winit calls while the loop
        // is still alive. [`ShortcutInhibit::abandon_display`] is the escape hatch for a caller
        // that cannot promise that.
        let backend = unsafe { Backend::from_foreign_display(display_ptr.cast()) };
        let conn = Connection::from_backend(backend);

        // Use a separate queue on the same display. libwayland dispatches per queue, so
        // winit's dispatch and ours never collide. Sharing winit's queue is what fails.
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());

        let active = Arc::new(AtomicBool::new(false));
        let mut state = WlState {
            seat: None,
            manager: None,
            active: Arc::clone(&active),
            notify,
        };
        queue
            .roundtrip(&mut state)
            .context("registry roundtrip on our own event queue")?;

        let seat = state
            .seat
            .clone()
            .ok_or_else(|| anyhow!("the compositor advertises no wl_seat"))?;
        let manager = state.manager.clone().ok_or_else(|| {
            anyhow!(
                "the compositor does not advertise \
                 zwp_keyboard_shortcuts_inhibit_manager_v1; its own keyboard shortcuts will keep \
                 taking precedence over the target"
            )
        })?;

        // Re-wrap winit's wl_surface as a proxy on our connection.
        // SAFETY: `surface_ptr` is the live `wl_surface` winit created for this window and still
        // owns; it is valid for as long as `window`, which this struct holds an `Arc` to. The
        // interface passed matches the object's actual interface. This is only recognised if
        // winit and this crate resolve to the same `wayland-backend` build; if they do not,
        // `from_ptr` fails and is reported as an error rather than being assumed to have worked.
        let surface = unsafe {
            let id = ObjectId::from_ptr(wl_surface::WlSurface::interface(), surface_ptr.cast())
                .map_err(|e| {
                    anyhow!(
                        "could not wrap winit's wl_surface on our connection ({e}); this usually \
                         means two different wayland-backend versions are linked into the binary"
                    )
                })?;
            wl_surface::WlSurface::from_id(&conn, id)
                .map_err(|e| anyhow!("could not build a wl_surface proxy from winit's: {e}"))?
        };
        log::debug!(
            "wrapped winit's wl_surface on our own connection: id={} version={}",
            surface.id().protocol_id(),
            surface.version()
        );

        // The queue is serviced by its own thread, which this value owns and joins on drop: it
        // reads a display it does not own, so it must not outlive the owner (see the module docs).
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let dispatch = std::thread::Builder::new()
            .name("nanokvm-wayland".to_string())
            .spawn(move || dispatch_loop(&mut queue, &mut state, &thread_stop))
            .context("spawning the wayland dispatch thread")?;

        Ok(ShortcutInhibit {
            _window: window,
            conn,
            qh,
            surface,
            seat,
            manager,
            inhibitor: None,
            active,
            stop,
            dispatch: Some(dispatch),
            display_gone: false,
        })
    }

    /// Give up on the display without touching it: winit's `wl_display` is already disconnected.
    ///
    /// Every subsequent Wayland call is skipped — the objects behind them are gone — but the
    /// dispatch thread is still stopped and joined, because a thread reading a dead socket is a
    /// worse leak than an inhibitor the compositor has already forgotten (it forgets it: the
    /// client's connection went with the display). Only for a caller that reaches this value after
    /// the event loop has been dropped; the ordinary path is to drop it before that.
    pub fn abandon_display(&mut self) {
        self.display_gone = true;
    }

    /// Stop the dispatch thread and wait for it. At most one poll timeout.
    fn stop_dispatch(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.dispatch.take() {
            if handle.join().is_err() {
                log::error!("the nanokvm-wayland dispatch thread panicked");
            }
        }
    }

    /// Create the inhibitor, so compositor shortcuts reach this window instead of the compositor.
    ///
    /// Only ever called while the window is focused and the user has asked to
    /// capture. `active` follows shortly afterwards, but arming is not a claim that inhibition is
    /// in force — see [`ShortcutInhibit::is_active`].
    pub fn arm(&mut self) {
        if self.inhibitor.is_some() || self.display_gone {
            return;
        }
        self.inhibitor =
            Some(
                self.manager
                    .inhibit_shortcuts(&self.surface, &self.seat, &self.qh, ()),
            );
        if let Err(e) = self.conn.flush() {
            log::warn!("flushing the inhibit request failed: {e}");
        }
        log::debug!("shortcut inhibitor requested");
    }

    /// Destroy the inhibitor. Part of every release, and safe to call when not armed.
    pub fn disarm(&mut self) {
        let Some(i) = self.inhibitor.take() else {
            return;
        };
        self.active.store(false, Ordering::SeqCst);
        if self.display_gone {
            // The connection went with winit's display; the compositor has already dropped every
            // object on it. Destroying one now would write through a freed `wl_display`.
            return;
        }
        i.destroy();
        if let Err(e) = self.conn.flush() {
            log::warn!("flushing the inhibitor destroy failed: {e}");
        }
        log::debug!("shortcut inhibitor destroyed");
    }

    /// Whether the compositor last said `active`.
    ///
    /// Never used as a focus signal: it arrives for unfocused surfaces too, and niri leaves it
    /// `active` across a focus-out/focus-in cycle.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    /// Whether an inhibitor object currently exists.
    pub fn is_armed(&self) -> bool {
        self.inhibitor.is_some()
    }
}

impl Drop for ShortcutInhibit {
    /// Disarm, then stop and join the dispatch thread — in that order, so the destroy request is
    /// on the socket before the thread that flushes stops.
    ///
    /// A window that goes away with an inhibitor still up would leave the compositor's shortcuts
    /// inhibited for a surface nobody owns; a dispatch thread that outlives this value would be
    /// reading a `wl_display` winit is about to disconnect (see the module docs).
    fn drop(&mut self) {
        self.disarm();
        self.stop_dispatch();
    }
}
