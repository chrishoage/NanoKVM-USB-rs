//! The two audio threads, their counters, their shutdown and their failure isolation
//! (plan §4.1 rev 5, §12 Stage 4a).
//!
//! No ALSA and no sound card. Everything here drives [`nanokvm::audio::AudioHandle`] through the
//! [`PcmSource`]/[`PcmSink`] seam with the fakes in `nanokvm::audio::pcm`, which is the only way
//! §12 Stage 4a's four failure claims can be tested at all: a missing card, an `EBUSY`, a card
//! that disappears mid-session and a playback sink that vanishes are not states a test may
//! produce on the user's hardware.
//!
//! Every one of those four has a test here **whose fake fails in a way that would break the claim
//! if the isolation were missing** — an opener that never succeeds, a source that dies after
//! three periods, a sink that dies after two — rather than a test that merely calls the happy
//! path and checks nothing blew up.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nanokvm::audio::pcm::{
    FnSinkOpener, FnSourceOpener, RecordingSink, ScriptedSource, WedgedSink, WedgedSource,
    WedgedSourceOpener,
};
use nanokvm::audio::{
    AudioConfig, AudioError, AudioHandle, AudioSide, PcmSink, PcmSource, RingConfig,
};

/// Tiny periods: the tests assert on which period went where, not on audio.
fn config() -> AudioConfig {
    AudioConfig {
        ring: RingConfig {
            periods: 4,
            period_frames: 2,
        },
        device_periods: 2,
        reopen_backoff: Duration::from_millis(10),
        // The cap is the same as the base here: these tests want dozens of retries inside a
        // second, and the escalation itself is `reopen_delay`'s own unit test.
        reopen_backoff_cap: Duration::from_millis(10),
        stop_deadline: Duration::from_millis(250),
        // No prefill: these tests are about the plumbing and the counters, and a cushion would
        // only add a wait to every one of them. The cushion's own reason to exist is the drift
        // policy, which `tests/audio_ring.rs` exercises directly.
        prefill_periods: 0,
    }
}

/// Poll until `done` or the deadline passes. Returns whether it happened.
///
/// Threads and wall-clock sleeps are the one thing a test cannot make deterministic, so the rule
/// here is the repo's: wait for the *consequence*, generously, and never sleep a fixed amount and
/// hope. A second is three orders of magnitude more than any of these need.
fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!("timed out waiting for {what}");
}

/// A source that yields the markers and then blocks until the handle is stopped, so the test sees
/// exactly the periods it asked for and no filler.
struct FiniteSource {
    markers: std::collections::VecDeque<i16>,
}

impl PcmSource for FiniteSource {
    fn read_period(&mut self, out: &mut [i16]) -> Result<(), AudioError> {
        match self.markers.pop_front() {
            Some(v) => {
                out.fill(v);
                Ok(())
            }
            // Nothing more is coming. Sleeping rather than spinning keeps the test's CPU honest;
            // returning `Ok` with silence would put filler into the assertion below.
            None => {
                std::thread::sleep(Duration::from_millis(5));
                Err(AudioError::Io {
                    device: "finite source".to_string(),
                    why: "the script is finished".to_string(),
                })
            }
        }
    }

    fn describe(&self) -> String {
        "finite source".to_string()
    }
}

#[test]
fn every_captured_period_reaches_the_sink_in_the_order_it_was_captured() {
    let sink = RecordingSink::new();
    let recorded = sink.clone();
    let mut audio = AudioHandle::spawn(
        Box::new(FnSourceOpener::new("finite", |n| {
            if n == 0 {
                Ok(Box::new(FiniteSource {
                    markers: (1..=5).collect(),
                }))
            } else {
                // One session only: a reopened source would append more periods and the
                // assertion below would be about the fake rather than about the plumbing.
                Err(AudioError::NoCard("one session only".to_string()))
            }
        })),
        Box::new(FnSinkOpener::new("recording", move |_| {
            Ok(Box::new(recorded.clone()) as Box<dyn PcmSink>)
        })),
        // A ring with room for the whole script, so this test is about order and not about the
        // overrun policy — which has its own test below.
        AudioConfig {
            ring: RingConfig {
                periods: 8,
                period_frames: 2,
            },
            ..config()
        },
    );

    wait_for("five periods to be played", || sink.markers().len() >= 5);
    audio.stop();

    assert_eq!(
        &sink.markers()[..5],
        &[1, 2, 3, 4, 5],
        "the ring is a FIFO of whole periods"
    );
    let snapshot = audio.snapshot();
    assert!(snapshot.periods_captured >= 5);
    assert_eq!(
        snapshot.counts.overruns, 0,
        "five periods cannot overrun a ring of eight"
    );
}

/// §12 Stage 4a: "a missing card … logs **once**, sets a surfaced condition, and changes nothing
/// about video or input". The fake here never succeeds, so the retry loop runs dozens of times;
/// without the once-only rule that is dozens of identical warnings in the user's log.
#[test]
fn a_card_that_is_never_there_is_one_condition_however_many_times_it_is_retried() {
    let opener = FnSourceOpener::new("absent", |_| {
        Err(AudioError::NoCard(
            "no video node could be paired with a serial node by USB topology".to_string(),
        ))
    });
    let opens = opener.opens();
    let mut audio = AudioHandle::spawn(
        Box::new(opener),
        Box::new(FnSinkOpener::new("recording", |_| {
            Ok(Box::new(RecordingSink::new()) as Box<dyn PcmSink>)
        })),
        AudioConfig {
            reopen_backoff: Duration::from_millis(1),
            ..config()
        },
    );

    wait_for("several open attempts", || {
        opens.load(Ordering::Relaxed) >= 5
    });
    let snapshot = audio.snapshot();
    audio.stop();

    assert_eq!(
        snapshot.conditions_logged,
        1,
        "{} open attempts produced {} conditions",
        opens.load(Ordering::Relaxed),
        snapshot.conditions_logged
    );
    let condition = snapshot.condition.expect("the condition must be surfaced");
    assert_eq!(condition.side, AudioSide::Capture);
    assert!(
        condition.message.contains("USB topology"),
        "discovery's own explanation must survive into the condition: {}",
        condition.message
    );
}

/// `EBUSY` is a different condition from a missing card and says a different thing, because the
/// user fixes it differently: on a PipeWire desk something is recording from the node.
#[test]
fn a_busy_card_is_surfaced_as_busy_and_names_the_device() {
    let mut audio = AudioHandle::spawn(
        Box::new(FnSourceOpener::new("busy", |_| {
            Err(AudioError::Busy {
                device: "hw:8".to_string(),
                why: "Device or resource busy".to_string(),
            })
        })),
        Box::new(FnSinkOpener::new("recording", |_| {
            Ok(Box::new(RecordingSink::new()) as Box<dyn PcmSink>)
        })),
        config(),
    );
    wait_for("the condition", || audio.snapshot().condition.is_some());
    let condition = audio.snapshot().condition.expect("set above");
    audio.stop();

    assert_eq!(condition.kind, "busy");
    assert!(condition.message.contains("hw:8"), "{}", condition.message);
    assert!(
        condition.message.contains("Another client holds the card"),
        "{}",
        condition.message
    );
}

/// A card that works and then vanishes. The condition is raised, the thread reopens, and — the
/// part that matters — when the card comes back the condition clears and audio resumes, without
/// anything outside this module having been involved.
#[test]
fn a_card_that_disappears_mid_session_is_surfaced_and_then_recovers() {
    let sink = RecordingSink::new();
    let recorded = sink.clone();
    let mut audio = AudioHandle::spawn(
        Box::new(FnSourceOpener::new("flaky", |n| match n {
            // Three periods, then the device is gone for ever.
            0 => Ok(Box::new(ScriptedSource::new("before", [1, 2, 3]).then(
                Err(AudioError::Gone {
                    device: "hw:8".to_string(),
                    why: "No such device".to_string(),
                }),
            ))),
            1 => Err(AudioError::Gone {
                device: "hw:8".to_string(),
                why: "No such device".to_string(),
            }),
            // ... and then it is replugged, as `hw:9`, which is what C13 says happens, and it
            // keeps producing — so the condition clearing below is the recovery and not a gap
            // between two failures.
            _ => Ok(Box::new(ScriptedSource::new(
                "after",
                std::iter::repeat_n(7, 100_000),
            ))),
        })),
        Box::new(FnSinkOpener::new("recording", move |_| {
            Ok(Box::new(recorded.clone()) as Box<dyn PcmSink>)
        })),
        config(),
    );

    wait_for("the loss to be surfaced", || {
        audio
            .snapshot()
            .condition
            .is_some_and(|c| c.kind == "gone" && c.side == AudioSide::Capture)
    });
    wait_for("the replugged card's audio", || sink.markers().contains(&7));
    let snapshot = audio.snapshot();
    audio.stop();

    assert!(
        snapshot.condition.is_none(),
        "a recovered card clears its own condition: {:?}",
        snapshot.condition
    );
    assert!(
        sink.markers().starts_with(&[1, 2, 3]),
        "the periods captured before the loss are still played: {:?}",
        sink.markers()
    );
    assert!(snapshot.capture_opens >= 2, "{snapshot:?}");
}

/// The fourth of §12 Stage 4a's four: the *playback* sink goes away. Capture must carry on —
/// which is exactly what a bounded, dropping ring is for — and the condition must name the
/// playback side rather than blaming the dongle.
#[test]
fn a_playback_sink_that_vanishes_does_not_stop_capture() {
    let opener = FnSourceOpener::new("steady", |_| {
        Ok(Box::new(ScriptedSource::new("steady", 1..=i16::MAX)) as Box<dyn PcmSource>)
    });
    let sink_opens = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&sink_opens);
    let mut audio = AudioHandle::spawn(
        Box::new(opener),
        Box::new(FnSinkOpener::new("vanishing", move |n| {
            counter.fetch_add(1, Ordering::Relaxed);
            if n == 0 {
                Ok(Box::new(RecordingSink::new().failing_after(2)) as Box<dyn PcmSink>)
            } else {
                Err(AudioError::Gone {
                    device: "default".to_string(),
                    why: "the PipeWire daemon went away".to_string(),
                })
            }
        })),
        config(),
    );

    // The sink fails mid-write first, and the *reopen* then fails too; wait for the second, so
    // the assertion below is about a sink that is gone rather than about one write that failed.
    wait_for(
        "the playback condition to name the sink that will not reopen",
        || {
            audio
                .snapshot()
                .condition
                .is_some_and(|c| c.side == AudioSide::Playback && c.message.contains("default"))
        },
    );
    let before = audio.snapshot().periods_captured;
    wait_for("capture to keep going regardless", || {
        audio.snapshot().periods_captured > before + 4
    });
    let snapshot = audio.snapshot();
    audio.stop();

    assert!(
        snapshot.counts.overruns > 0,
        "a ring nobody is draining must drop periods rather than block the capture thread: \
         {snapshot:?}"
    );
    let condition = snapshot.condition.expect("set above");
    assert_eq!(condition.side, AudioSide::Playback);
    assert!(
        condition.message.contains("default"),
        "{}",
        condition.message
    );
}

/// Mute silences the output and keeps the ring flowing, so unmuting resumes on live audio rather
/// than on a backlog — and an overrun is never an artefact of having been muted.
#[test]
fn mute_silences_the_output_without_stalling_the_ring() {
    let sink = RecordingSink::new();
    let recorded = sink.clone();
    let mut audio = AudioHandle::spawn(
        Box::new(FnSourceOpener::new("steady", |_| {
            Ok(Box::new(ScriptedSource::new(
                "steady",
                std::iter::repeat_n(9, 10_000),
            )) as Box<dyn PcmSource>)
        })),
        Box::new(FnSinkOpener::new("recording", move |_| {
            Ok(Box::new(recorded.clone()) as Box<dyn PcmSink>)
        })),
        config(),
    );

    assert!(!audio.muted());
    wait_for("some live audio", || sink.markers().contains(&9));
    audio.set_muted(true);
    assert!(audio.muted());

    let at_mute = sink.markers().len();
    wait_for("more periods while muted", || {
        sink.markers().len() > at_mute + 8
    });
    let snapshot = audio.snapshot();
    audio.stop();

    let after_mute = &sink.markers()[at_mute + 2..];
    assert!(
        after_mute.iter().all(|m| *m == 0),
        "muted output is silence: {after_mute:?}"
    );
    assert!(
        snapshot.periods_played + snapshot.periods_muted > at_mute as u64,
        "the ring keeps flowing while muted, so nothing backs up: {snapshot:?}"
    );
    // And the muted periods are counted as muted rather than as audio the user heard: a single
    // "played" counter would report a muted session as a perfect one.
    assert!(snapshot.periods_muted > 0, "{snapshot:?}");
    // The flag is an atomic anything can flip, and nothing in Stage 4a flips it — the control
    // lives in the chrome (§12 Stage 4b) and no key is bound to it here.
    let flag = audio.mute_flag();
    flag.store(false, Ordering::Relaxed);
    assert!(!audio.muted());
}

/// Shutdown is bounded and does not depend on either device being healthy: the capture thread is
/// in a backoff and the playback thread is waiting on an empty ring, which is the worst case.
#[test]
fn stopping_is_prompt_even_with_nothing_working() {
    let mut audio = AudioHandle::spawn(
        Box::new(FnSourceOpener::new("absent", |_| {
            Err(AudioError::NoCard("nothing here".to_string()))
        })),
        Box::new(FnSinkOpener::new("recording", |_| {
            Ok(Box::new(RecordingSink::new()) as Box<dyn PcmSink>)
        })),
        AudioConfig {
            // A backoff far longer than the test's patience: a `stop` that waited for it would
            // fail here, which is the point.
            reopen_backoff: Duration::from_secs(30),
            ..config()
        },
    );
    wait_for("the condition", || audio.snapshot().condition.is_some());

    let started = Instant::now();
    audio.stop();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "stop took {:?}; it must not wait out a backoff",
        started.elapsed()
    );
    // Idempotent: `Drop` calls it again on the way out of this function.
    audio.stop();
}

/// A silent period is handed to the sink when the ring is empty, and it is counted as an
/// underrun rather than skipped — the playback device must be fed on time or it underruns
/// itself, which is a click rather than a counter.
#[test]
fn an_empty_ring_plays_silence_and_counts_it() {
    let sink = RecordingSink::new();
    let recorded = sink.clone();
    let mut audio = AudioHandle::spawn(
        Box::new(FnSourceOpener::new("one", |n| {
            if n == 0 {
                Ok(Box::new(FiniteSource {
                    markers: [5].into_iter().collect(),
                }) as Box<dyn PcmSource>)
            } else {
                Err(AudioError::NoCard("nothing more".to_string()))
            }
        })),
        Box::new(FnSinkOpener::new("recording", move |_| {
            Ok(Box::new(recorded.clone()) as Box<dyn PcmSink>)
        })),
        AudioConfig {
            // A short ring depth is also the playback thread's patience before it gives up
            // waiting and writes silence, so this keeps the test brief.
            ring: RingConfig {
                periods: 4,
                period_frames: 48,
            },
            ..config()
        },
    );

    wait_for("silence after the one real period", || {
        audio.snapshot().counts.underruns > 0
    });
    let snapshot = audio.snapshot();
    audio.stop();

    assert!(snapshot.counts.underruns > 0);
    assert!(
        sink.markers().contains(&5),
        "the one real period was still played: {:?}",
        sink.markers()
    );
    assert!(
        sink.written().iter().all(|p| p.len() == 96),
        "every period written is exactly one period long, silence included"
    );
}

/// A full ring drops the oldest period and counts it, rather than blocking the capture thread —
/// the property that makes a stalled playback sink harmless.
#[test]
fn a_stalled_sink_makes_the_ring_drop_rather_than_the_capture_thread_block() {
    /// A sink that never returns, the way a wedged device behaves.
    struct WedgedSink {
        stopped: Arc<Mutex<bool>>,
    }
    impl PcmSink for WedgedSink {
        fn write_period(&mut self, _samples: &[i16]) -> Result<(), AudioError> {
            while !*self.stopped.lock().expect("not poisoned") {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(AudioError::Gone {
                device: "wedged".to_string(),
                why: "released by the test".to_string(),
            })
        }
        fn describe(&self) -> String {
            "wedged sink".to_string()
        }
    }

    let stopped = Arc::new(Mutex::new(false));
    let flag = Arc::clone(&stopped);
    let mut audio = AudioHandle::spawn(
        Box::new(FnSourceOpener::new("steady", |_| {
            Ok(Box::new(ScriptedSource::new(
                "steady",
                std::iter::repeat_n(3, 100_000),
            )) as Box<dyn PcmSource>)
        })),
        Box::new(FnSinkOpener::new("wedged", move |_| {
            Ok(Box::new(WedgedSink {
                stopped: Arc::clone(&flag),
            }) as Box<dyn PcmSink>)
        })),
        config(),
    );

    wait_for("the ring to start dropping", || {
        audio.snapshot().counts.overruns > 10
    });
    let captured = audio.snapshot().periods_captured;
    assert!(
        captured > 10,
        "the capture thread must keep reading while the sink is wedged"
    );

    // Release the sink so `stop` does not have to wait it out, then stop.
    *stopped.lock().expect("not poisoned") = true;
    audio.stop();
}

/// **Both sides failing at once is two conditions, not one slot they fight over.** An unplugged
/// dongle and a `default` whose daemon restarted are one event on this desk, and with a single
/// condition between them each side's retry overwrites the other's: every retry then looks new,
/// every retry logs, `conditions_logged` counts retries, and the title flaps between the two
/// several times a second. Dozens of retries here, two log lines, and a title that always says
/// the same thing.
#[test]
fn two_sides_failing_at_once_are_two_conditions_however_often_they_are_retried() {
    let source = FnSourceOpener::new("absent", |_| {
        Err(AudioError::NoCard(
            "the dongle is not plugged in".to_string(),
        ))
    });
    let sink = FnSinkOpener::new("gone", |_| {
        Err(AudioError::Gone {
            device: "default".to_string(),
            why: "the PipeWire daemon went away".to_string(),
        })
    });
    let (source_opens, sink_opens) = (source.opens(), sink.opens());
    let mut audio = AudioHandle::spawn(Box::new(source), Box::new(sink), config());

    wait_for("dozens of retries on both sides", || {
        source_opens.load(Ordering::Relaxed) >= 30 && sink_opens.load(Ordering::Relaxed) >= 30
    });
    let snapshot = audio.snapshot();
    audio.stop();

    assert_eq!(
        snapshot.conditions_logged,
        2,
        "{} capture and {} playback attempts produced {} conditions",
        source_opens.load(Ordering::Relaxed),
        sink_opens.load(Ordering::Relaxed),
        snapshot.conditions_logged,
    );
    assert_eq!(
        snapshot.recoveries_logged, 0,
        "nothing recovered, so nothing may say it did"
    );
    // Deterministic, not whichever thread retried last: capture is the side a user can usually do
    // something about, so it is the one the title shows.
    let condition = snapshot.condition.expect("both sides are failing");
    assert_eq!(condition.side, AudioSide::Capture);
}

/// A card that opens and then fails its first read — a half-dead USB device — every cycle. The
/// open is **not** the recovery: treating it as one produces a "recovered" line and a warning
/// twice a second for the rest of the session, which is the same log spam the once-only rule
/// exists to prevent.
#[test]
fn an_open_that_never_reads_is_one_warning_and_never_a_recovery() {
    let opener = FnSourceOpener::new("half dead", |_| {
        Ok(Box::new(
            ScriptedSource::new("half dead", []).then(Err(AudioError::Io {
                device: "hw:8".to_string(),
                why: "Input/output error".to_string(),
            })),
        ) as Box<dyn PcmSource>)
    });
    let opens = opener.opens();
    let mut audio = AudioHandle::spawn(
        Box::new(opener),
        Box::new(FnSinkOpener::new("recording", |_| {
            Ok(Box::new(RecordingSink::new()) as Box<dyn PcmSink>)
        })),
        config(),
    );

    wait_for("a dozen open-then-fail cycles", || {
        opens.load(Ordering::Relaxed) >= 12
    });
    let snapshot = audio.snapshot();
    audio.stop();

    assert_eq!(
        snapshot.conditions_logged,
        1,
        "{} cycles produced {} conditions",
        opens.load(Ordering::Relaxed),
        snapshot.conditions_logged
    );
    assert_eq!(
        snapshot.recoveries_logged, 0,
        "no period ever arrived, so nothing recovered"
    );
    assert!(snapshot.condition.is_some(), "and it is still surfaced");
}

/// The one failure [`AudioHandle::stop`]'s deadline exists for: a device call that never returns,
/// which is what an ALSA PCM whose card was yanked can do. Nothing releases these fakes. `stop`
/// must come back anyway — `main.rs` drops audio on every shutdown path, after the release-all,
/// and a side channel may not hold the process open (§4.1 rev 5, §2.6).
#[test]
fn stopping_does_not_wait_for_a_device_call_that_never_returns() {
    let mut audio = AudioHandle::spawn(
        Box::new(FnSourceOpener::new("wedged", |_| {
            Ok(Box::new(WedgedSource::default()) as Box<dyn PcmSource>)
        })),
        Box::new(FnSinkOpener::new("wedged", |_| {
            Ok(Box::new(WedgedSink::default()) as Box<dyn PcmSink>)
        })),
        AudioConfig {
            stop_deadline: Duration::from_millis(100),
            ..config()
        },
    );
    wait_for("both devices to be open and stuck", || {
        let s = audio.snapshot();
        s.capture_opens >= 1 && s.playback_opens >= 1
    });

    let started = Instant::now();
    audio.stop();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "stop took {:?} with both devices wedged; it must detach rather than wait",
        started.elapsed()
    );
    // Idempotent even when both threads were detached; `Drop` calls it again on the way out.
    audio.stop();
}

/// The playback side's patience is **one period**, not one ring depth. The device buffer is
/// `device_periods` periods; a thread that waited the whole ring for a period that is not coming
/// would let the sound card run dry first, which is an XRUN — a click and an ALSA state to
/// recover from — in place of a counted period of silence.
///
/// The ring here is deliberately deep (8 × 100 ms) so the two are three quarters of a second
/// apart and the assertion is not about scheduling noise.
#[test]
fn silence_is_written_within_one_period_rather_than_one_ring_depth() {
    let sink = RecordingSink::new();
    let recorded = sink.clone();
    let mut audio = AudioHandle::spawn(
        // No card at all, so the ring stays empty and every period is silence.
        Box::new(FnSourceOpener::new("absent", |_| {
            Err(AudioError::NoCard("nothing here".to_string()))
        })),
        Box::new(FnSinkOpener::new("recording", move |_| {
            Ok(Box::new(recorded.clone()) as Box<dyn PcmSink>)
        })),
        AudioConfig {
            ring: RingConfig {
                periods: 8,
                period_frames: 4800, // 100 ms each: a depth of 800 ms.
            },
            ..config()
        },
    );

    let started = Instant::now();
    wait_for("the first period of silence", || !sink.written().is_empty());
    let elapsed = started.elapsed();
    let snapshot = audio.snapshot();
    audio.stop();

    assert!(
        elapsed < Duration::from_millis(400),
        "the first silent period took {elapsed:?}; one period is 100 ms and the whole ring is \
         800 ms, so this waited the ring out"
    );
    assert!(snapshot.counts.underruns > 0, "{snapshot:?}");
    assert_eq!(
        snapshot.periods_played, 0,
        "no real period was ever captured, so none may be counted as played"
    );
    assert!(
        snapshot.periods_silent > 0,
        "silence is counted as silence: {snapshot:?}"
    );
}

// ---- the prefill against a real sound card's own buffer ---------------------------------------

/// A sink that behaves like a sound card: it has a buffer `depth` periods deep that drains in
/// real time, so the **first `depth` writes do not block** and everything after them is paced.
///
/// Every other fake in this file takes a period instantly, which is exactly the property that
/// hides the defect below: a prefill is a cushion only if something downstream stops accepting
/// periods. This one stops.
struct DeviceBufferedSink {
    depth: u32,
    interval: Duration,
    /// When everything written so far will have finished playing. A device that has run dry is
    /// this value in the past, which is why it is clamped forward on every write.
    played_until: Instant,
}

impl DeviceBufferedSink {
    fn new(depth: usize, interval: Duration) -> Self {
        DeviceBufferedSink {
            depth: depth.max(1) as u32,
            interval,
            played_until: Instant::now(),
        }
    }
}

impl PcmSink for DeviceBufferedSink {
    fn write_period(&mut self, _samples: &[i16]) -> Result<(), AudioError> {
        let now = Instant::now();
        self.played_until = self.played_until.max(now);
        // There is room for one more once the queue is down to `depth - 1` periods.
        let ready = self
            .played_until
            .checked_sub(self.interval * (self.depth - 1))
            .unwrap_or(now);
        if ready > now {
            std::thread::sleep(ready - now);
        }
        self.played_until += self.interval;
        Ok(())
    }

    fn describe(&self) -> String {
        format!("device-buffered sink, {} periods", self.depth)
    }
}

/// A source that produces one period per `interval`, like a card at a fixed sample rate.
struct PacedSource {
    interval: Duration,
    next: Instant,
}

impl PcmSource for PacedSource {
    fn read_period(&mut self, out: &mut [i16]) -> Result<(), AudioError> {
        let now = Instant::now();
        if self.next > now {
            std::thread::sleep(self.next - now);
        }
        // Advanced by exactly one interval, never clamped to `now`: a card that was not read
        // from for a while has the samples waiting, so a late reader catches up rather than
        // losing the time. Clamping here would make this fake run measurably slower than the
        // sink below and the test would be measuring `thread::sleep`'s overshoot as drift.
        self.next += self.interval;
        out.fill(1);
        Ok(())
    }

    fn describe(&self) -> String {
        "paced source".to_string()
    }
}

/// **The prefill has to survive the sink's own buffer, or it is not a cushion.**
///
/// Found on hardware on 2026-09-11 and reproduced here. The playback side primes the ring with
/// `prefill_periods` and then starts writing — and the first `device_periods` of those writes do
/// not block, because the sound card's buffer is empty. With the two equal, the prefill is
/// transferred straight into the card, the ring is left at zero sitting on its low-water mark,
/// and [`nanokvm::audio::DriftPolicy`] duly inserts periods of silence for a clock difference
/// that has not happened. On the desk that was two inserts and two silent periods in the first
/// second of every run, against a card and a host whose clocks had not had time to differ by a
/// single sample.
///
/// It matters because §12 Stage 4a asks for the drift rate to be **measured** from those
/// counters. A counter that moves at startup is a rate that is wrong.
///
/// Both halves are run here, because "this configuration is quiet" says nothing on its own: the
/// same fakes, the same timings, and the only difference is the one period of prefill.
#[test]
fn a_prefill_no_deeper_than_the_sinks_own_buffer_is_no_cushion_at_all() {
    // 96 frames is 2 ms at 48 kHz, and the fakes below are paced at 2 ms, so the ring's own
    // sense of time (`RingConfig::depth`, which assumes 48 kHz, and the prefill deadline derived
    // from it) agrees with the wall clock the fakes run on. The other tests in this file use
    // two-frame periods because they assert on *which* period went where; this one asserts on
    // timing, so it cannot.
    let interval = Duration::from_millis(2);
    let device_periods = 4;

    let run = |prefill: usize| {
        let mut audio = AudioHandle::spawn(
            Box::new(FnSourceOpener::new("paced", move |_| {
                Ok(Box::new(PacedSource {
                    interval,
                    next: Instant::now(),
                }) as Box<dyn PcmSource>)
            })),
            Box::new(FnSinkOpener::new("device", move |_| {
                Ok(Box::new(DeviceBufferedSink::new(device_periods, interval)) as Box<dyn PcmSink>)
            })),
            AudioConfig {
                ring: RingConfig {
                    periods: 8,
                    period_frames: 96,
                },
                device_periods,
                prefill_periods: prefill,
                ..config()
            },
        );
        // Long enough for the drift policy's hold of 4 x 8 = 32 observations to complete several
        // times over: 32 periods of 2 ms is 64 ms.
        wait_for("500 periods to be captured", || {
            audio.snapshot().periods_captured >= 500
        });
        let snapshot = audio.snapshot();
        audio.stop();
        snapshot
    };

    let starved = run(device_periods);
    let cushioned = run(device_periods + 2);

    assert!(
        starved.counts.drift_inserts > 0,
        "the fake does not reproduce the defect, so the other half of this test proves nothing: \
         with a prefill of {device_periods} and a device buffer of {device_periods} the ring must \
         end up empty. {starved:?}"
    );
    assert_eq!(
        cushioned.counts.drift_inserts, 0,
        "a prefill two periods deeper than the device buffer leaves two periods in the ring, off \
         the low-water mark, and nothing may be inserted: {cushioned:?}"
    );
    assert_eq!(
        cushioned.counts.drift_drops, 0,
        "and nothing may be dropped either: two clocks that are the same clock do not drift"
    );
    assert_eq!(
        cushioned.periods_silent, 0,
        "no silence may reach the sink when the source never missed a period: {cushioned:?}"
    );
}

/// **Hardware defect D2.** A side whose open never returns must not look like a side that is
/// working.
///
/// D2, observed once on this desk: the playback thread sat inside `snd_pcm_open` for six seconds
/// and logged nothing. No condition was raised, `playback_opens` stayed at 0, and the title still
/// said `audio on`. "Has not started" and "working" were indistinguishable, which is exactly the
/// surfacing gap §2.8 exists to forbid.
///
/// Two claims, in order:
///
/// 1. while the open is outstanding the side reports itself as **opening** — which the title and
///    the Audio popover render;
/// 2. an open still outstanding after [`AudioConfig::reopen_backoff_cap`] — the longest this
///    module ever waits for anything — becomes an ordinary condition, **logged once** however
///    many times it is read.
///
/// The fake is `WedgedSourceOpener`, whose `open` never returns, so `stop` detaches its thread at
/// the deadline; that is the shipped behaviour for a wedged device call and is asserted here too.
#[test]
fn an_open_that_never_returns_is_surfaced_rather_than_looking_like_a_working_side() {
    let config = AudioConfig {
        // The supervision limit. Short so the test takes milliseconds rather than ten seconds;
        // `reopen_delay`'s own escalation is unit-tested elsewhere.
        reopen_backoff_cap: Duration::from_millis(100),
        ..config()
    };
    let mut audio = AudioHandle::spawn(
        Box::new(WedgedSourceOpener::default()),
        // The playback side opens normally, so what is asserted below is the *capture* side's
        // own state rather than "nothing is working".
        Box::new(FnSinkOpener::new("ok", |_| {
            Ok(Box::new(RecordingSink::new()) as Box<dyn PcmSink>)
        })),
        config,
    );

    // 1. Opening, before the limit has passed.
    wait_for("the capture side to report itself as opening", || {
        let s = audio.snapshot();
        s.opening[0] && s.condition.is_none()
    });
    let opening = audio.snapshot();
    assert_eq!(
        opening.opening_side(),
        Some(AudioSide::Capture),
        "the side that is opening must be named: {opening:?}"
    );
    assert_eq!(
        opening.capture_opens, 0,
        "nothing has opened — that is the whole point of D2"
    );

    // 2. Past the limit it is a condition, and it is one condition however often it is read.
    wait_for("the stuck open to become a condition", || {
        audio.snapshot().condition.is_some()
    });
    let stuck = audio.snapshot();
    let condition = stuck.condition.clone().expect("a condition");
    assert_eq!(condition.side, AudioSide::Capture);
    assert_eq!(condition.kind, "open failed", "{condition:?}");
    assert!(
        condition.message.contains("has not returned"),
        "the message must say what is wrong, not only that something is: {condition:?}"
    );
    assert_eq!(
        stuck.opening_side(),
        None,
        "past the limit it is not opening any more, it is stuck, and the condition is the more \
         specific statement"
    );

    // Read it thirty more times: the supervision runs on every read, and "logged once" is a claim
    // about `conditions_logged` rather than about `log::warn!`.
    for _ in 0..30 {
        let _ = audio.snapshot();
        std::thread::sleep(Duration::from_millis(2));
    }
    let after = audio.snapshot();
    assert_eq!(
        after.conditions_logged, 1,
        "one stuck open is one logged condition, however many readers look at it: {after:?}"
    );
    assert_eq!(after.recoveries_logged, 0, "nothing recovered: {after:?}");

    // And the shutdown is still bounded: the thread is inside a call that will never return, so
    // `stop` reports it and detaches rather than waiting (the same rule as `WedgedSource`).
    let started = Instant::now();
    audio.stop();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "stop() waited {:?} for a thread that cannot come back",
        started.elapsed()
    );
}
