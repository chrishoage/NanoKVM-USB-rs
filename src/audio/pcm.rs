//! PCM interfaces and test adapters.
//!
//! Sources and sinks exchange interleaved sample periods without exposing ALSA types.
//! Fakes model buffering, pacing, failure, and calls that never return.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use super::AudioError;

/// One period of PCM in, at the configured format.
pub trait PcmSource: Send {
    /// Fill `out` — one period, `RingConfig::period_samples` long — with the next
    /// captured samples. Blocks until the device has them.
    fn read_period(&mut self, out: &mut [i16]) -> Result<(), AudioError>;

    /// What this is, for the log line that names it.
    fn describe(&self) -> String;
}

/// One period of PCM out.
pub trait PcmSink: Send {
    /// Write one period. Blocks until the device has taken it.
    fn write_period(&mut self, samples: &[i16]) -> Result<(), AudioError>;

    fn describe(&self) -> String;
}

/// Opens fresh PCM sources after failure. The discovery adapter resolves the current
/// card number; retry timing belongs to the audio worker.
pub trait PcmSourceOpener: Send {
    fn open(&mut self) -> Result<Box<dyn PcmSource>, AudioError>;
    fn describe(&self) -> String;

    /// Take the flag the audio threads are stopped by, to hand to every device opened after it.
    ///
    /// Called once, by [`crate::audio::AudioHandle::spawn`], before either thread starts.
    /// [`PcmSource::read_period`] blocks in the device, and "wait for a period, but give up when
    /// the process is shutting down" cannot be expressed from outside the call — so the flag goes
    /// *into* the device, where the wait is. The default does nothing, which is right
    /// for every implementation whose blocking is bounded by construction (all the fakes below).
    fn on_stop_flag(&mut self, _stop: Arc<AtomicBool>) {}
}

/// Where a [`PcmSink`] comes from. The playback side has no discovery to do — it is always ALSA
/// `default` — but it still reopens, because a `default` that is a PipeWire node can go away when
/// the daemon restarts.
pub trait PcmSinkOpener: Send {
    fn open(&mut self) -> Result<Box<dyn PcmSink>, AudioError>;
    fn describe(&self) -> String;

    /// The sink side of [`PcmSourceOpener::on_stop_flag`], and for the same reason: a write into
    /// a device whose daemon has stopped answering blocks in the kernel.
    fn on_stop_flag(&mut self, _stop: Arc<AtomicBool>) {}
}

// ---- fakes ---------------------------------------------------------------------------------

/// A [`PcmSource`] that reads from a script.
///
/// Each entry is one `read_period` result, in order. When the script runs out the source keeps
/// returning `end`, which is how "the card was fine and then it vanished" is expressed: a few
/// good periods followed by an error that repeats for ever, as a dead device does.
pub struct ScriptedSource {
    script: VecDeque<Result<i16, AudioError>>,
    end: Result<i16, AudioError>,
    reads: Arc<AtomicU64>,
    name: String,
}

impl ScriptedSource {
    /// `values` are marker samples: each successful read fills the whole period with that value,
    /// so a test can assert *which* period came out where.
    pub fn new(name: &str, values: impl IntoIterator<Item = i16>) -> Self {
        ScriptedSource {
            script: values.into_iter().map(Ok).collect(),
            end: Ok(0),
            reads: Arc::new(AtomicU64::new(0)),
            name: name.to_string(),
        }
    }

    /// What every read past the end of the script returns. Use it to make a source die.
    pub fn then(mut self, end: Result<i16, AudioError>) -> Self {
        self.end = end;
        self
    }

    /// A counter of reads attempted, shared with the caller so it can be watched from outside.
    pub fn reads(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.reads)
    }
}

impl PcmSource for ScriptedSource {
    fn read_period(&mut self, out: &mut [i16]) -> Result<(), AudioError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let next = self.script.pop_front().unwrap_or_else(|| self.end.clone());
        let value = next?;
        out.fill(value);
        Ok(())
    }

    fn describe(&self) -> String {
        format!("scripted source {}", self.name)
    }
}

/// A [`PcmSink`] that records every period it was given.
#[derive(Clone, Default)]
pub struct RecordingSink {
    written: Arc<Mutex<Vec<Vec<i16>>>>,
    fail_after: Option<usize>,
}

impl RecordingSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Make the sink disappear after `n` successful writes.
    pub fn failing_after(mut self, n: usize) -> Self {
        self.fail_after = Some(n);
        self
    }

    /// Everything written so far, in order.
    pub fn written(&self) -> Vec<Vec<i16>> {
        self.written
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The first sample of every period written, which is the marker [`ScriptedSource`] set.
    pub fn markers(&self) -> Vec<i16> {
        self.written()
            .iter()
            .map(|p| p.first().copied().unwrap_or(0))
            .collect()
    }
}

impl PcmSink for RecordingSink {
    fn write_period(&mut self, samples: &[i16]) -> Result<(), AudioError> {
        let mut written = self.written.lock().unwrap_or_else(|e| e.into_inner());
        if self.fail_after.is_some_and(|n| written.len() >= n) {
            return Err(AudioError::Gone {
                device: "recording sink".to_string(),
                why: "the fake sink was configured to vanish here".to_string(),
            });
        }
        written.push(samples.to_vec());
        Ok(())
    }

    fn describe(&self) -> String {
        "recording sink".to_string()
    }
}

/// An opener whose devices are built by a closure, so every open produces a fresh one and the
/// failure cases are ordinary `Err`s the closure returns.
///
/// A closure rather than a list of prepared devices because "the card is not there" is not one
/// failed open: it is a failed open on *every* retry for as long as the dongle is unplugged, and
/// the rule under test is that all of them together produce one log line. The closure is
/// handed the open count so it can say "fail for ever", "fail three times then succeed", or
/// "succeed once and never again".
pub struct FnSourceOpener {
    name: String,
    opens: Arc<AtomicU64>,
    #[allow(clippy::type_complexity)]
    make: Box<dyn FnMut(u64) -> Result<Box<dyn PcmSource>, AudioError> + Send>,
}

impl FnSourceOpener {
    /// `make` is called with the open count, starting at 0.
    pub fn new(
        name: &str,
        make: impl FnMut(u64) -> Result<Box<dyn PcmSource>, AudioError> + Send + 'static,
    ) -> Self {
        FnSourceOpener {
            name: name.to_string(),
            opens: Arc::new(AtomicU64::new(0)),
            make: Box::new(make),
        }
    }

    /// How many times `open` has been called, shared with the caller so a test can watch the
    /// retry loop from outside it.
    pub fn opens(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.opens)
    }
}

impl PcmSourceOpener for FnSourceOpener {
    fn open(&mut self) -> Result<Box<dyn PcmSource>, AudioError> {
        let n = self.opens.fetch_add(1, Ordering::Relaxed);
        (self.make)(n)
    }

    fn describe(&self) -> String {
        format!("scripted source opener {}", self.name)
    }
}

/// The sink equivalent of [`FnSourceOpener`].
pub struct FnSinkOpener {
    name: String,
    opens: Arc<AtomicU64>,
    #[allow(clippy::type_complexity)]
    make: Box<dyn FnMut(u64) -> Result<Box<dyn PcmSink>, AudioError> + Send>,
}

impl FnSinkOpener {
    pub fn new(
        name: &str,
        make: impl FnMut(u64) -> Result<Box<dyn PcmSink>, AudioError> + Send + 'static,
    ) -> Self {
        FnSinkOpener {
            name: name.to_string(),
            opens: Arc::new(AtomicU64::new(0)),
            make: Box::new(make),
        }
    }

    pub fn opens(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.opens)
    }
}

impl PcmSinkOpener for FnSinkOpener {
    fn open(&mut self) -> Result<Box<dyn PcmSink>, AudioError> {
        let n = self.opens.fetch_add(1, Ordering::Relaxed);
        (self.make)(n)
    }

    fn describe(&self) -> String {
        format!("scripted sink opener {}", self.name)
    }
}

/// A device call that never returns — the one failure [`crate::audio::AudioHandle::stop`]'s
/// deadline exists for.
///
/// It stands in for an ALSA PCM whose card was yanked mid-call and which is sitting in the
/// kernel. Nothing releases it: a fake that could be released would test the release and not the
/// bound. Both [`WedgedSource`] and [`WedgedSink`] park on a condvar that is never notified, so
/// the thread costs nothing while it waits and the test's assertion is about `stop` returning
/// anyway.
#[derive(Default)]
pub struct Wedged {
    lock: Mutex<()>,
    never: Condvar,
}

impl Wedged {
    /// Block for ever. Returns only to satisfy the type.
    pub fn forever<T>(&self) -> T {
        let mut guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            guard = self.never.wait(guard).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// A [`PcmSource`] whose `read_period` never returns.
#[derive(Default)]
pub struct WedgedSource(Wedged);

impl PcmSource for WedgedSource {
    fn read_period(&mut self, _out: &mut [i16]) -> Result<(), AudioError> {
        self.0.forever()
    }

    fn describe(&self) -> String {
        "a capture device that has stopped answering".to_string()
    }
}

/// An opener whose `open` never returns — hardware defect fake.
///
/// on the recorded test setup the playback thread once sat inside `snd_pcm_open` and never came out. It
/// logged nothing, raised no condition and left `playback_opens` at 0, so the title and the
/// popover both said audio was on. That is the one failure shape the other fakes here cannot
/// produce: every one of them either succeeds or returns an `Err`, and the invisible failure is
/// the one that returns *neither*.
///
/// Nothing releases it, for [`Wedged`]'s reason: a fake that could be released would test the
/// release rather than the supervision. A test that uses it must expect
/// [`crate::audio::AudioHandle::stop`] to detach the thread at its deadline.
#[derive(Default)]
pub struct WedgedSourceOpener(Wedged);

impl PcmSourceOpener for WedgedSourceOpener {
    fn open(&mut self) -> Result<Box<dyn PcmSource>, AudioError> {
        self.0.forever()
    }

    fn describe(&self) -> String {
        "a capture device whose open never returns".to_string()
    }
}

/// Playback opener that never returns, for open-supervision tests.
#[derive(Default)]
pub struct WedgedSinkOpener(Wedged);

impl PcmSinkOpener for WedgedSinkOpener {
    fn open(&mut self) -> Result<Box<dyn PcmSink>, AudioError> {
        self.0.forever()
    }

    fn describe(&self) -> String {
        "a playback device whose open never returns".to_string()
    }
}

/// A [`PcmSink`] whose `write_period` never returns.
#[derive(Default)]
pub struct WedgedSink(Wedged);

impl PcmSink for WedgedSink {
    fn write_period(&mut self, _samples: &[i16]) -> Result<(), AudioError> {
        self.0.forever()
    }

    fn describe(&self) -> String {
        "a playback device that has stopped answering".to_string()
    }
}
