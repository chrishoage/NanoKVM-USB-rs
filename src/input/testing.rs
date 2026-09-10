//! An in-memory fake [`Link`] for testing the input path without a device (§9.3).
//!
//! It records every `(cmd, payload)` handed to it, in order, so that assertions can be made on
//! **the frame sequence** rather than on internal counters. Recording happens on entry to
//! `transact`, before the configured behaviour is applied, so a frame that the transport then
//! failed to write still appears — the record is "what the writer handed to the link", which is
//! exactly the boundary §2.6.1 draws.
//!
//! Behaviour is controllable per call through a script, with a default for calls the script does
//! not cover:
//!
//! - [`Behaviour::Ack`] — a success reply, `cmd | 0x80`.
//! - [`Behaviour::Stall`] — park inside `transact` until the test releases it. This is how a
//!   stalled writer (§2.8) and the cancellation race (§2.6) are tested. On release the call
//!   applies whatever *default* is configured then, so a test can stall-then-fail; scripted
//!   entries are reserved for the calls that follow.
//! - [`Behaviour::Fail`] — `LinkError::Down`, a transport failure.
//! - [`Behaviour::Timeout`] — `LinkError::Timeout`, returned immediately; no test ever sleeps.
//! - [`Behaviour::DeviceError`] — `LinkError::Device`, a `cmd | 0xC0` reply with a code.
//!
//! Every wait here is a `Condvar` rendezvous with a generous failsafe deadline. A test that
//! reaches a deadline fails; nothing uses a sleep for synchronisation.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::link::{Link, LinkError, Reply};
use crate::proto::report::{KeyboardReport, MouseAbsReport, MouseRelReport};

/// How long a rendezvous waits before giving up and failing the test.
const FAILSAFE: Duration = Duration::from_secs(10);

/// One frame as the writer handed it to the link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedFrame {
    /// The command byte (`proto::cmd::*`).
    pub cmd: u8,
    /// The raw payload, exactly as `report.payload()` produced it.
    pub payload: Vec<u8>,
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
    default: Behaviour,
    script: VecDeque<Behaviour>,
    /// Calls that have entered a stall, ever.
    entered: u64,
    /// Stalls the test has released, ever.
    released: u64,
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
            default: Behaviour::Ack,
            script: VecDeque::new(),
            entered: 0,
            released: 0,
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
        st.frames.push(RecordedFrame {
            cmd,
            payload: payload.to_vec(),
        });
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
        drop(st);

        match behaviour {
            Behaviour::Ack | Behaviour::Stall => Ok(Reply {
                cmd: cmd | 0x80,
                data: Vec::new(),
            }),
            Behaviour::Fail => Err(LinkError::Down("fake link: transport down".into())),
            Behaviour::Timeout => Err(LinkError::Timeout { cmd, timeout }),
            Behaviour::DeviceError(code) => Err(LinkError::Device { cmd, code }),
        }
    }
}

// ---------------------------------------------------------------- expectation helpers

/// The expected frame for a keyboard report. Built through [`KeyboardReport::payload`] rather
/// than from retyped bytes: `proto` owns the encoding and `fixtures/packets/ch9329.toml` is the
/// authority for *that*, while these tests are about order and content (§9.1, §9.2 item 1).
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
/// `MOUSE_RELEASE_ALL` report (§2.6).
pub fn release_frames_rel() -> Vec<RecordedFrame> {
    vec![kb_frame(0, [0; 6]), rel_frame(0, 0, 0, 0)]
}

/// The three frames a release-all produces in `Abs` mode: zeroed keyboard, an absolute report at
/// the last position with all buttons up, then [`MOUSE_RELEASE_ALL`] (§2.6).
pub fn release_frames_abs(x: u16, y: u16) -> Vec<RecordedFrame> {
    vec![
        kb_frame(0, [0; 6]),
        abs_frame(0, x, y, 0),
        rel_frame(0, 0, 0, 0),
    ]
}

/// Park the writer inside a transact so a test can fill the queue deterministically.
///
/// Submits one relative motion event — recorded as frame 0 of every test that uses this — scripts
/// the link to stall on it, and returns once the writer is parked inside that call with the queue
/// empty. Everything submitted afterwards accumulates in the queue under the §2.2 coalescing
/// rules until [`FakeControl::release_stalls`] is called, which is what makes the coalescing and
/// ordering assertions deterministic without a single sleep.
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

/// Block until the writer has completed at least `count` cancellation sequences (§2.6) and return
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
