//! Fake transports and controllable link sources for input tests.
//!
//! Tests can pause transactions at frame boundaries and observe exact report order without
//! using serial hardware or relying on scheduler timing.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::link::{Link, LinkError, LinkSource, Reply};
use crate::proto::report::{KeyboardReport, MouseAbsReport, MouseRelReport};

/// How long a rendezvous waits before giving up and failing the test.
const FAILSAFE: Duration = Duration::from_secs(10);

/// One frame as the writer handed it to the link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedFrame {
    /// The command byte (`proto::cmd::*`).
    pub cmd: u8,
    /// The raw payload, as `report.payload()` produced it.
    pub payload: Vec<u8>,
}

/// Ordered link operations: report, resynchronization, or refused call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeCall {
    /// [`Link::resync`] — the writer asked the transport to finish any torn frame.
    Resync,
    /// One [`Link::transact`].
    Frame(RecordedFrame),
    /// A call made to a link that had already hung up ([`FakeControl::hang_up`]), refused before
    /// any byte could have left: `Some(cmd)` for a `transact`, `None` for a `resync`.
    ///
    /// It is recorded rather than swallowed because "the writer never wrote to the dead link" is
    /// the central claim of the idle-loss path, and a claim that cannot fail is not a test: a fake
    /// that dropped these calls on the floor left `calls().len()` and `frame_count()` unchanged
    /// whether the writer stayed silent or hammered the corpse. Count them with
    /// [`FakeControl::refused`].
    Refused { cmd: Option<u8> },
}

/// What the fake does with a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behaviour {
    /// Reply `cmd | 0x80` with no data.
    Ack,
    /// Block inside `transact` until released by the test.
    Stall,
    /// `LinkError::Down` — the transport is gone.
    Fail,
    /// `LinkError::Timeout` — written, never answered.
    Timeout,
    /// `LinkError::Device` with this error code — the link is healthy, the frame was rejected.
    DeviceError(u8),
}

#[derive(Debug)]
struct FakeState {
    frames: Vec<RecordedFrame>,
    /// Frames and `resync` calls together, in order.
    calls: Vec<FakeCall>,
    default: Behaviour,
    script: VecDeque<Behaviour>,
    /// Calls that have entered a stall, ever.
    entered: u64,
    /// Stalls the test has released, ever.
    released: u64,
    /// Payload returned by GET_INFO; defaults to a valid recorded reply.
    get_info_payload: Vec<u8>,
    /// Make [`Link::resync`] fail, as a port that died between `open` and the preamble would.
    fail_resync: bool,
    /// The transport has hung up: [`Link::is_down`] says so and every call fails without a byte
    /// leaving, because nothing reaches a port that is gone. Set by [`FakeControl::hang_up`].
    hung_up: bool,
    /// Calls refused because [`FakeState::hung_up`] was set, ever. Also recorded in `calls` as
    /// [`FakeCall::Refused`]; the counter is what an assertion reads.
    refused: u64,
    /// Pause resynchronization before the reconnect drain so tests can seed stale entries.
    stall_resync: bool,
    /// `resync` calls, ever.
    resyncs: u64,
}

#[derive(Debug)]
struct FakeShared {
    state: Mutex<FakeState>,
    cv: Condvar,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The [`Link`] half. Move this into `input::spawn`.
#[derive(Debug)]
pub struct FakeLink {
    shared: Arc<FakeShared>,
}

/// The test's half: inspect the recorded frames and steer the behaviour. Cloneable and `Send`,
/// so a test can drive the fake from several threads.
#[derive(Debug, Clone)]
pub struct FakeControl {
    shared: Arc<FakeShared>,
}

/// Build a fake link and its control handle.
pub fn fake_link() -> (FakeLink, FakeControl) {
    let shared = Arc::new(FakeShared {
        state: Mutex::new(FakeState {
            frames: Vec::new(),
            calls: Vec::new(),
            default: Behaviour::Ack,
            script: VecDeque::new(),
            entered: 0,
            released: 0,
            get_info_payload: crate::serial::fake::DEFAULT_GET_INFO_PAYLOAD.to_vec(),
            fail_resync: false,
            hung_up: false,
            refused: 0,
            stall_resync: false,
            resyncs: 0,
        }),
        cv: Condvar::new(),
    });
    (
        FakeLink {
            shared: Arc::clone(&shared),
        },
        FakeControl { shared },
    )
}

impl FakeControl {
    /// Every frame handed to the link so far, in order.
    pub fn frames(&self) -> Vec<RecordedFrame> {
        lock(&self.shared.state).frames.clone()
    }

    /// How many frames have been handed to the link.
    pub fn frame_count(&self) -> usize {
        lock(&self.shared.state).frames.len()
    }

    /// Every frame and every [`Link::resync`], in the order the writer issued them.
    pub fn calls(&self) -> Vec<FakeCall> {
        lock(&self.shared.state).calls.clone()
    }

    /// Number of resynchronization requests.
    pub fn resyncs(&self) -> u64 {
        lock(&self.shared.state).resyncs
    }

    /// How many calls this link refused because it had hung up.
    ///
    /// This is the assertion the idle-loss path rests on. "The writer discovered the loss
    /// without writing" is only a claim a test can falsify if a call to a dead link leaves a
    /// trace: `frame_count()` and `calls().len()` alone cannot tell a writer that stayed silent
    /// from one whose frames the fake ate. Zero here is the silence; anything else is the writer
    /// having probed a corpse.
    pub fn refused(&self) -> u64 {
        lock(&self.shared.state).refused
    }

    /// Device-info reply payload; a short payload makes commissioning fail.
    pub fn set_get_info_payload(&self, payload: &[u8]) {
        lock(&self.shared.state).get_info_payload = payload.to_vec();
    }

    /// Make [`Link::resync`] report the transport gone.
    pub fn fail_resync(&self, fail: bool) {
        lock(&self.shared.state).fail_resync = fail;
    }

    /// Disconnect without requiring a write, modeling reader-detected idle loss.
    pub fn hang_up(&self) {
        lock(&self.shared.state).hung_up = true;
        self.shared.cv.notify_all();
    }

    /// Pause the next resync until released, exposing the pre-drain commissioning boundary.
    pub fn stall_resync(&self, stall: bool) {
        lock(&self.shared.state).stall_resync = stall;
    }

    /// The behaviour applied to calls the script does not cover.
    pub fn set_behaviour(&self, behaviour: Behaviour) {
        lock(&self.shared.state).default = behaviour;
        self.shared.cv.notify_all();
    }

    /// Queue behaviours for the next calls, consumed in order before the default applies.
    pub fn script<I: IntoIterator<Item = Behaviour>>(&self, behaviours: I) {
        let mut st = lock(&self.shared.state);
        st.script.extend(behaviours);
    }

    /// Block until at least `n` frames have been recorded. Fails the test on the failsafe
    /// deadline rather than hanging forever.
    pub fn wait_for_frames(&self, n: usize) {
        self.wait_until(|st| st.frames.len() >= n, &format!("{n} frames"));
    }

    /// Block until at least `n` calls are parked in a stall.
    pub fn wait_for_stalled(&self, n: u64) {
        self.wait_until(
            |st| st.entered.saturating_sub(st.released) >= n,
            &format!("{n} stalled calls"),
        );
    }

    /// Release every call currently parked in a stall.
    pub fn release_stalls(&self) {
        let mut st = lock(&self.shared.state);
        st.released = st.entered;
        drop(st);
        self.shared.cv.notify_all();
    }

    fn wait_until<F: Fn(&FakeState) -> bool>(&self, done: F, what: &str) {
        let deadline = Instant::now() + FAILSAFE;
        let mut st = lock(&self.shared.state);
        while !done(&st) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "fake link: timed out waiting for {what}"
            );
            let (guard, _) = self
                .shared
                .cv
                .wait_timeout(st, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            st = guard;
        }
    }
}

impl Link for FakeLink {
    fn transact(&mut self, cmd: u8, payload: &[u8], timeout: Duration) -> Result<Reply, LinkError> {
        let mut st = lock(&self.shared.state);
        if st.hung_up {
            // No frame is recorded — the bytes would not have reached anything, and `SerialLink`
            // does the same, refusing in `write_raw` before it writes — but the call itself is,
            // so that "the writer never touched the dead link" is an assertion that can fail.
            st.refused += 1;
            st.calls.push(FakeCall::Refused { cmd: Some(cmd) });
            drop(st);
            self.shared.cv.notify_all();
            return Err(LinkError::Down("fake link: hung up".into()));
        }
        let frame = RecordedFrame {
            cmd,
            payload: payload.to_vec(),
        };
        st.frames.push(frame.clone());
        st.calls.push(FakeCall::Frame(frame));
        let mut behaviour = st.script.pop_front().unwrap_or(st.default);
        self.shared.cv.notify_all();

        if behaviour == Behaviour::Stall {
            st.entered += 1;
            let me = st.entered;
            self.shared.cv.notify_all();
            while st.released < me {
                st = self
                    .shared
                    .cv
                    .wait(st)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            // Re-read the *default* on the way out, so a test can stall a call and then decide
            // what it returns. The script is left alone: its entries belong to the calls that
            // follow, not to this one. A still-stalling default acks rather than re-parking.
            behaviour = st.default;
            if behaviour == Behaviour::Stall {
                behaviour = Behaviour::Ack;
            }
        }
        // Device-info requests need a payload; other successful reports acknowledge without one.
        let data = if cmd == crate::proto::cmd::GET_INFO {
            st.get_info_payload.clone()
        } else {
            Vec::new()
        };
        drop(st);

        match behaviour {
            Behaviour::Ack | Behaviour::Stall => Ok(Reply {
                cmd: cmd | 0x80,
                data,
            }),
            Behaviour::Fail => Err(LinkError::Down("fake link: transport down".into())),
            Behaviour::Timeout => Err(LinkError::Timeout { cmd, timeout }),
            Behaviour::DeviceError(code) => Err(LinkError::Device { cmd, code }),
        }
    }

    /// Recorded, never scripted: the preamble is bytes at a chip, not a request with a reply, and
    /// the only interesting failure is the transport having gone in the meantime.
    fn resync(&mut self) -> Result<(), LinkError> {
        let mut st = lock(&self.shared.state);
        if st.hung_up {
            st.refused += 1;
            st.calls.push(FakeCall::Refused { cmd: None });
            drop(st);
            self.shared.cv.notify_all();
            return Err(LinkError::Down("fake link: hung up".into()));
        }
        st.resyncs += 1;
        st.calls.push(FakeCall::Resync);
        self.shared.cv.notify_all();
        if st.stall_resync {
            st.entered += 1;
            let me = st.entered;
            self.shared.cv.notify_all();
            while st.released < me {
                st = self
                    .shared
                    .cv
                    .wait(st)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        }
        let failed = st.fail_resync;
        drop(st);
        self.shared.cv.notify_all();
        if failed {
            return Err(LinkError::Down(
                "fake link: transport down before resync".into(),
            ));
        }
        Ok(())
    }

    /// What [`FakeControl::hang_up`] set, and nothing else. The default `false` is what every
    /// other test here relies on: a fake link is healthy until something says otherwise.
    fn is_down(&self) -> bool {
        lock(&self.shared.state).hung_up
    }
}

// ---------------------------------------------------------------- expectation helpers

/// The expected frame for a keyboard report. Built through [`KeyboardReport::payload`] rather
/// than from retyped bytes: `proto` owns the encoding and `fixtures/packets/ch9329.toml` is the
/// authority for *that*, while these tests are about order and content.
pub fn kb_frame(modifiers: u8, keys: [u8; 6]) -> RecordedFrame {
    RecordedFrame {
        cmd: crate::proto::cmd::SEND_KB_GENERAL_DATA,
        payload: KeyboardReport { modifiers, keys }.payload().to_vec(),
    }
}

/// The expected frame for an absolute mouse report.
pub fn abs_frame(buttons: u8, x: u16, y: u16, wheel: i8) -> RecordedFrame {
    RecordedFrame {
        cmd: crate::proto::cmd::SEND_MS_ABS_DATA,
        payload: MouseAbsReport {
            buttons,
            x,
            y,
            wheel,
        }
        .payload()
        .to_vec(),
    }
}

/// The expected frame for a relative mouse report.
pub fn rel_frame(buttons: u8, dx: i8, dy: i8, wheel: i8) -> RecordedFrame {
    RecordedFrame {
        cmd: crate::proto::cmd::SEND_MS_REL_DATA,
        payload: MouseRelReport {
            buttons,
            dx,
            dy,
            wheel,
        }
        .payload()
        .to_vec(),
    }
}

/// The two frames a release-all produces in `Rel` mode: zeroed keyboard, then the
/// `MOUSE_RELEASE_ALL` report.
pub fn release_frames_rel() -> Vec<RecordedFrame> {
    vec![kb_frame(0, [0; 6]), rel_frame(0, 0, 0, 0)]
}

/// The three frames a release-all produces in `Abs` mode: zeroed keyboard, an absolute report at
/// the last position with all buttons up, then [`crate::proto::report::MOUSE_RELEASE_ALL`].
pub fn release_frames_abs(x: u16, y: u16) -> Vec<RecordedFrame> {
    vec![
        kb_frame(0, [0; 6]),
        abs_frame(0, x, y, 0),
        rel_frame(0, 0, 0, 0),
    ]
}

/// Pause the writer inside a relative-motion transaction, leaving the queue empty
/// so tests can fill it deterministically.
pub fn stall_writer(producer: &crate::input::Producer, control: &FakeControl) {
    control.script([Behaviour::Stall]);
    producer
        .submit(crate::input::Event::PointerRel { dx: 1, dy: 0 })
        .expect("gate event must be accepted by a fresh producer");
    control.wait_for_stalled(1);
}

/// The frame [`stall_writer`] leaves at index 0.
pub fn gate_frame() -> RecordedFrame {
    rel_frame(0, 1, 0, 0)
}

/// Block until the writer has completed at least `count` cancellation sequences and return
/// the most recent record.
///
/// The rendezvous a test needs when the *writer* is what triggers the cancellation — a transport
/// failure, say — so there is no epoch bump the caller could have waited on. Panics on the
/// failsafe deadline rather than hanging. The production surface for the same fact is
/// [`crate::input::Producer::stats`].
pub fn wait_for_cancellations(
    producer: &crate::input::Producer,
    count: u64,
) -> crate::input::ReleaseRecord {
    use std::sync::atomic::Ordering;

    let shared = &producer.shared;
    let deadline = Instant::now() + FAILSAFE;
    let mut q = crate::input::shared::lock(&shared.queue);
    loop {
        if shared.counters.cancellations.load(Ordering::SeqCst) >= count {
            drop(q);
            return lock(&shared.last_release).expect("a completed sequence records a release");
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for {count} cancellation sequences"
        );
        let (guard, _) = shared
            .wake
            .wait_timeout(q, remaining)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        q = guard;
    }
}

/// Inject a key-down directly at `epoch` to test defensive reconnect draining.
/// Public submission cannot create this state while the link is down.
pub fn force_enqueue_key(
    producer: &crate::input::Producer,
    key: crate::proto::report::HidKey,
    epoch: u64,
) {
    use crate::input::queue::EntryKind;

    let shared = &producer.shared;
    let mut q = crate::input::shared::lock(&shared.queue);
    q.push_barrier(
        EntryKind::Key { key, down: true },
        epoch,
        Instant::now(),
        &shared.counters,
    )
    .expect("the test queue has room");
    drop(q);
    shared.wake.notify_all();
}

// ---------------------------------------------------------------- a scripted `LinkSource`

/// The state behind [`FakeSource`] and [`FakeSourceControl`].
#[derive(Debug)]
struct SourceState {
    /// Answers for the next `open` calls, in order: `Some(link)` yields it, `None` refuses.
    script: VecDeque<Option<FakeLink>>,
    /// What happens once the script runs out: refuse for ever, or keep making fresh links.
    refuse_when_empty: bool,
    /// When each `open` was called. The backoff assertions are made on the gaps between these.
    attempts: Vec<Instant>,
    /// Park inside `open` after recording the attempt, until the test releases it. This is how a
    /// test lands something in the window between the writer's last cancellation check and the
    /// link arriving — the window `WriterHandle::shutdown` must not be stuck behind.
    hold: bool,
    /// Controls for links the source made itself, once the script ran out.
    auto: Vec<FakeControl>,
    /// The [`FakeSource`] itself has been dropped. The rendezvous for "whoever was holding this
    /// source has finished with it", which is how a test observes that a link opened after a
    /// shutdown was dropped rather than used.
    dropped: bool,
}

#[derive(Debug)]
struct SourceShared {
    state: Mutex<SourceState>,
    cv: Condvar,
}

/// A [`LinkSource`] whose answers a test writes in advance.
///
/// Move it into [`crate::input::spawn_with_source`]; steer it through the
/// [`FakeSourceControl`] returned alongside.
#[derive(Debug)]
pub struct FakeSource {
    shared: Arc<SourceShared>,
}

/// The test's half of a [`FakeSource`].
#[derive(Debug, Clone)]
pub struct FakeSourceControl {
    shared: Arc<SourceShared>,
}

/// A scripted source and its control. The script starts empty and refusing, so a source nothing
/// was queued on is a device that is simply not there.
pub fn fake_source() -> (FakeSource, FakeSourceControl) {
    let shared = Arc::new(SourceShared {
        state: Mutex::new(SourceState {
            script: VecDeque::new(),
            refuse_when_empty: true,
            attempts: Vec::new(),
            hold: false,
            auto: Vec::new(),
            dropped: false,
        }),
        cv: Condvar::new(),
    });
    (
        FakeSource {
            shared: Arc::clone(&shared),
        },
        FakeSourceControl { shared },
    )
}

impl FakeSourceControl {
    /// Queue a fresh link, and return the control that inspects it. The link is created now, so a
    /// test can script its behaviour before the writer ever opens it.
    pub fn push_link(&self) -> FakeControl {
        let (link, control) = fake_link();
        lock(&self.shared.state).script.push_back(Some(link));
        control
    }

    /// Queue `n` refusals — the device is not back yet.
    pub fn push_failures(&self, n: usize) {
        let mut st = lock(&self.shared.state);
        for _ in 0..n {
            st.script.push_back(None);
        }
    }

    /// After the script runs out, keep producing fresh links instead of refusing. Their controls
    /// arrive in [`FakeSourceControl::auto_links`].
    pub fn yield_links_when_empty(&self) {
        lock(&self.shared.state).refuse_when_empty = false;
    }

    /// Controls for the links the source made after its script ran out, oldest first.
    pub fn auto_links(&self) -> Vec<FakeControl> {
        lock(&self.shared.state).auto.clone()
    }

    /// Park every `open` from now on after it has recorded its attempt, until
    /// [`FakeSourceControl::release_opens`].
    ///
    /// A real `open` is `open(2)` plus a `tcsetattr` on a device that may have just re-enumerated,
    /// and the writer cannot interrupt it; this is how that is reproduced without hardware. Pair it
    /// with [`FakeSourceControl::wait_for_attempts`], which is satisfied as the open is entered.
    pub fn hold_opens(&self) {
        lock(&self.shared.state).hold = true;
        self.shared.cv.notify_all();
    }

    /// Let every parked `open` return, and stop parking new ones.
    pub fn release_opens(&self) {
        lock(&self.shared.state).hold = false;
        self.shared.cv.notify_all();
    }

    /// Block until the [`FakeSource`] has been dropped.
    ///
    /// Its holder is the writer, or — while an open is in flight — the thread performing it. So
    /// this is how a test waits for that thread to have finished with the link it opened, which is
    /// the moment after which "it was never written to" is final rather than merely not-yet.
    pub fn wait_for_source_dropped(&self) {
        let deadline = Instant::now() + FAILSAFE;
        let mut st = lock(&self.shared.state);
        while !st.dropped {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "fake source: timed out waiting for the source to be dropped"
            );
            let (guard, _) = self
                .shared
                .cv
                .wait_timeout(st, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            st = guard;
        }
    }

    /// How many times the writer has asked for a link.
    pub fn attempts(&self) -> usize {
        lock(&self.shared.state).attempts.len()
    }

    /// When each of those asks happened.
    pub fn attempt_times(&self) -> Vec<Instant> {
        lock(&self.shared.state).attempts.clone()
    }

    /// Block until the writer has asked for a link at least `n` times.
    pub fn wait_for_attempts(&self, n: usize) {
        let deadline = Instant::now() + FAILSAFE;
        let mut st = lock(&self.shared.state);
        while st.attempts.len() < n {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "fake source: timed out waiting for {n} open attempts, saw {}",
                st.attempts.len()
            );
            let (guard, _) = self
                .shared
                .cv
                .wait_timeout(st, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            st = guard;
        }
    }
}

impl LinkSource for FakeSource {
    fn open(&mut self) -> Result<Box<dyn Link>, LinkError> {
        let mut st = lock(&self.shared.state);
        st.attempts.push(Instant::now());
        self.shared.cv.notify_all();
        // Recorded first, then parked: `wait_for_attempts` is what tells a test the writer is
        // inside this call, and it would be useless if it only fired on the way out.
        while st.hold {
            st = self
                .shared
                .cv
                .wait(st)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        let scripted = st.script.pop_front();
        let answer = match scripted {
            Some(Some(link)) => Some(link),
            Some(None) => None,
            None if st.refuse_when_empty => None,
            None => {
                let (link, control) = fake_link();
                st.auto.push(control);
                Some(link)
            }
        };
        drop(st);
        self.shared.cv.notify_all();
        match answer {
            Some(link) => Ok(Box::new(link)),
            None => Err(LinkError::Down("fake source: no device".into())),
        }
    }

    fn describe(&self) -> String {
        "a scripted fake source".to_string()
    }
}

impl Drop for FakeSource {
    fn drop(&mut self) {
        lock(&self.shared.state).dropped = true;
        self.shared.cv.notify_all();
    }
}

// ---------------------------------------------------------------- reconnect rendezvous

/// Block until the writer has commissioned at least `n` replacement links, i.e. until
/// `Stats::reconnects >= n`. The initial link is not one of them.
pub fn wait_for_reconnects(producer: &crate::input::Producer, n: u64) {
    use std::sync::atomic::Ordering;

    let shared = &producer.shared;
    let deadline = Instant::now() + FAILSAFE;
    let mut q = crate::input::shared::lock(&shared.queue);
    loop {
        if shared.counters.reconnects.load(Ordering::SeqCst) >= n {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for {n} reconnects (attempts so far: {})",
            shared.counters.reconnect_attempts.load(Ordering::SeqCst)
        );
        let (guard, _) = shared
            .wake
            .wait_timeout(q, remaining)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        q = guard;
    }
}

/// Expected commissioning operations: preamble, keyboard and relative-mouse
/// releases, then device information.
pub fn commission_calls() -> Vec<FakeCall> {
    let mut calls = vec![FakeCall::Resync];
    calls.extend(release_frames_rel().into_iter().map(FakeCall::Frame));
    calls.push(FakeCall::Frame(RecordedFrame {
        cmd: crate::proto::cmd::GET_INFO,
        payload: Vec::new(),
    }));
    calls
}
