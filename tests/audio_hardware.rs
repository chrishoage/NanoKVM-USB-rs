//! Audio capture, playback, and recovery against the real dongle.
//!
//! These tests type commands into a focused target terminal and require the local
//! PipeWire/PulseAudio tools. Run one binary at a time with `--test-threads=1`.
//! Playback must be verified on the temporary null sink before emitting a tone.
//! A requested `PIPEWIRE_NODE` can fall back to the user's speakers.
//!
//! Capture checks bracket a 1 kHz tone with silent controls and discard the first
//! stale period. Playback checks record the sink monitor. USB reset tests require
//! explicit authorization. See `docs/development.md` for local device restrictions.

#![cfg(feature = "hardware")]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nanokvm::audio::alsa::{AlsaCapture, DefaultSinkOpener, FixedSourceOpener};
use nanokvm::audio::pcm::PcmSource;
use nanokvm::audio::ring::SAMPLE_RATE;
use nanokvm::audio::tone::{find_tone, mono, ToneReport, DEFAULT_REFERENCES, TONE_RATIO};
use nanokvm::audio::{AudioConfig, AudioHandle, AudioSide, AudioSnapshot};
use nanokvm::discovery::{discover, CardResolver, Constraints, RealProbe, RealSysfs, ResolvedCard};

#[path = "support/child_guard.rs"]
mod child_guard;
use child_guard::ChildGuard;

/// Held by every test in this file for its whole body: the dongle's card, its serial node, the
/// temporary null sink and `PIPEWIRE_NODE` are all one-at-a-time resources, and the harness runs
/// tests in parallel unless told otherwise. `unwrap_or_else(|e| e.into_inner)` because a test
/// that panicked while holding it has poisoned it and the next one should still run — it takes
/// the desk as it finds it and every guard in this file has already tidied up on the way out.
static DESK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The dongle's nodes on the recorded test setup (CLAUDE.md), asserted rather than used: every device below is
/// resolved through `discovery`, and these two only make a renumbered desk fail with an
/// explanation instead of quietly measuring something else.
const VIDEO: &str = "/dev/video4";
const SERIAL: &str = "/dev/ttyACM1";

/// The tone the target is asked to play, and the one looked for in the capture.
const TONE_HZ: f32 = 1000.0;

/// The temporary PipeWire sink every playback test writes to. Never the user's speakers.
const NULL_SINK: &str = "nanokvm-test";

/// How long to wait after Ctrl+C before trusting a capture to be free of the tone.
///
/// Measured on the recorded test setup 2026-09-11 and not a guess. `speaker-test` negotiates
/// `buffer_size = 48000` on the Pi — a full second of tone already queued in the target's own
/// ALSA buffer when Ctrl+C arrives — and a capture started 1.5 s later still found it at ratio 30.
/// A 40-second recording taken straight after Ctrl+C, sliced into two-second windows, is at the
/// noise floor (rms 3.1e-5) in *every* window, so the target itself is quiet within about a
/// second; eight seconds is generous and costs nothing.
///
/// What is left after that is not the target and not a tail: it is the device's own stale
/// prefix, which [`STALE_PERIODS`] measures and discards.
const TONE_SETTLE: Duration = Duration::from_secs(8);

// ---- logging --------------------------------------------------------------------------------

/// Send the library's `log` output to stderr, so the granted ALSA period and buffer sizes — which
/// `src/audio/alsa.rs` logs at info and which nothing off hardware can check — land in the run's
/// transcript.
fn init_logging() {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(false)
        .try_init();
}

// ---- the desk -------------------------------------------------------------------------------

/// The dongle's sound card, resolved the way the client resolves it: discovery finds the pair,
/// and the card is the one on the capture node's own USB device.
///
/// Nothing here hardcodes `hw:8`. The card number is read back out of the `card<N>` directory
/// name on every resolve, because card numbers renumber on replug as `/dev` names do
/// , and a run on a desk where they have renumbered must still measure the right card.
fn resolve_card() -> ResolvedCard {
    let sysfs = RealSysfs::default();
    let probe = RealProbe;
    let pair = discover(&sysfs, &probe, &Constraints::default())
        .unwrap_or_else(|e| panic!("discovery failed on a desk with the dongle plugged in:\n{e}"));

    assert_eq!(
        pair.video.dev,
        Path::new(VIDEO),
        "discovery picked a capture node this test does not expect on this desk; \
         /dev/video0-3 are the user's hardware and must never be opened (CLAUDE.md)"
    );
    assert_eq!(pair.serial.dev, Path::new(SERIAL), "the paired serial node");

    let usb = &pair
        .video
        .usb
        .as_ref()
        .expect("the capture node resolves to a USB device")
        .sysfs;
    let card = CardResolver::real(usb.clone())
        .resolve()
        .unwrap_or_else(|e| {
            panic!(
                "no sound card paired with {}:\n{e}",
                pair.video.dev.display()
            )
        });
    println!("[desk] capture node {VIDEO} on {}", usb.display());
    println!("[desk] sound card {} — {}", card.device, card.card);
    card
}

/// The PipeWire source node that fronts this card, found by the card number PipeWire itself
/// records (`alsa.card = "<N>"`), never by the product string — "USB2 Video" becomes "USB3 Video"
/// on a SuperSpeed port (CLAUDE.md).
fn pipewire_source_for(card: &ResolvedCard) -> String {
    let number = card
        .card
        .number()
        .expect("the paired card's directory is named card<N>");
    let out = Command::new("pactl")
        .args(["list", "sources"])
        .output()
        .expect("run pactl list sources");
    assert!(out.status.success(), "pactl list sources failed");
    let text = String::from_utf8_lossy(&out.stdout);

    let mut name: Option<String> = None;
    let mut found: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Name: ") {
            name = Some(rest.to_string());
        }
        if line == format!("alsa.card = \"{number}\"") {
            found = name.clone();
        }
    }
    found.unwrap_or_else(|| {
        panic!("no PipeWire source reports alsa.card = \"{number}\"; is pipewire running?")
    })
}

/// Where this run's recordings go. `NANOKVM_AUDIO_OUT` if set — the hardware run keeps them as
/// evidence — and the system temporary directory otherwise, so the test never writes into the
/// repository.
fn out_dir() -> PathBuf {
    std::env::var_os("NANOKVM_AUDIO_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

// ---- the null sink --------------------------------------------------------------------------

/// A temporary PipeWire null sink, unloaded when this value is dropped.
///
/// RULES.md: "NEVER play sound through the user's speakers […] `pactl unload-module` afterwards,
/// ALWAYS, including on failure." A `Drop` is the only construction that survives the dozen
/// `assert!`s below, any one of which unwinds straight past a tidy-up at the end of the function.
struct NullSink {
    module: String,
}

impl NullSink {
    fn load() -> NullSink {
        let out = Command::new("pactl")
            .args([
                "load-module",
                "module-null-sink",
                &format!("sink_name={NULL_SINK}"),
            ])
            .output()
            .expect("run pactl load-module");
        assert!(
            out.status.success(),
            "pactl load-module failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let module = String::from_utf8_lossy(&out.stdout).trim().to_string();
        println!("[sink] loaded module-null-sink {NULL_SINK} as module {module}");
        NullSink { module }
    }

    /// Point ALSA `default` at this sink for everything opened afterwards in this process.
    ///
    /// The variable is read by the `pipewire` ALSA plugin when the PCM is opened, so it has to be
    /// set before [`AudioHandle::spawn`] and it stays set for the rest of the binary — which is
    /// harmless, because every test here that opens a sink wants the same answer, and none of
    /// them may ever open the user's.
    fn route_alsa_default_here(&self) {
        std::env::set_var("PIPEWIRE_NODE", NULL_SINK);
        println!("[sink] PIPEWIRE_NODE={NULL_SINK}: ALSA `default` now opens on the null sink");
    }

    /// This sink's PipeWire object id, as `pactl` numbers it.
    fn object_id(&self) -> String {
        let out = Command::new("pactl")
            .args(["list", "short", "sinks"])
            .output()
            .expect("run pactl list short sinks");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find(|l| l.split('\t').nth(1) == Some(NULL_SINK))
            .and_then(|l| l.split('\t').next())
            .unwrap_or_else(|| panic!("the sink {NULL_SINK} is not listed"))
            .to_string()
    }

    /// Assert that this process's own playback landed here, by asking PipeWire where
    /// it put the stream.
    ///
    /// RULES.md's rule is "never play sound through the user's speakers", and `PIPEWIRE_NODE` is
    /// a *request*: a client whose target cannot be resolved is connected to the default sink
    /// instead, silently, which on the recorded test setup is the user's. That is the one failure in this file
    /// that would break a rule rather than a test, so it is checked rather than assumed — and it
    /// also tells a silent monitor recording ("we played to the wrong place") apart from a silent
    /// capture ("the target was not making a sound"), which cost a sweep to work out.
    fn assert_our_playback_landed_here(&self) {
        let want = self.object_id();
        let out = Command::new("pactl")
            .args(["list", "sink-inputs"])
            .output()
            .expect("run pactl list sink-inputs");
        let text = String::from_utf8_lossy(&out.stdout);

        let mut sink: Option<String> = None;
        let mut ours: Vec<(String, String)> = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("Sink: ") {
                sink = Some(rest.to_string());
            }
            // The ALSA plugin names the stream after the process, which here is this test binary.
            if let Some(rest) = line.strip_prefix("node.name = ") {
                let name = rest.trim_matches('"').to_string();
                if name.starts_with("alsa_playback.audio_hardware") {
                    ours.push((sink.clone().unwrap_or_default(), name));
                }
            }
        }
        assert!(
            !ours.is_empty(),
            "this process has no playback stream in PipeWire at all, so nothing can be said \
             about where it went:\n{text}"
        );
        for (on, name) in &ours {
            assert_eq!(
                on, &want,
                "{name} is on sink {on}, not on {NULL_SINK} ({want}). PIPEWIRE_NODE is a request \
                 that PipeWire may ignore, and ignoring it means this played on the user's own \
                 output — stop and find out why before running this again"
            );
            println!("[sink] {name} is on sink {on} = {NULL_SINK}, as required");
        }
    }

    /// Record this sink's monitor for `secs`, into a WAV, and return the samples.
    ///
    /// `parecord`, not `pw-record`: measured on the recorded test setup, `pw-record --target <sink>.monitor`
    /// links nothing and writes digital silence (see the module docs).
    fn record_monitor(&self, path: &Path, secs: u64) -> Vec<i16> {
        let mut cmd = Command::new("timeout");
        cmd.args([
            "-s",
            "INT",
            &secs.to_string(),
            "parecord",
            "-d",
            &format!("{NULL_SINK}.monitor"),
            "--rate=48000",
            "--channels=2",
            "--format=s16le",
        ])
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
        let mut child = ChildGuard::spawn("parecord", &mut cmd).expect("spawn parecord");
        let status = child
            .wait_bounded(Duration::from_secs(secs + 10))
            .expect("wait for parecord");
        assert!(
            status.is_some(),
            "parecord did not finish within {}s",
            secs + 10
        );
        read_wav(path)
    }
}

impl Drop for NullSink {
    fn drop(&mut self) {
        match Command::new("pactl")
            .args(["unload-module", &self.module])
            .status()
        {
            Ok(s) if s.success() => println!("[sink] unloaded module {}", self.module),
            // Said out loud rather than hidden: a module left loaded is a change to the user's
            // desk, and the run's report has to be able to show it was undone.
            other => eprintln!(
                "[sink] FAILED to unload module {}: {other:?} — check `pactl list short modules`",
                self.module
            ),
        }
    }
}

// ---- the target's tone ----------------------------------------------------------------------

/// `speaker-test` running on the target, stopped when this value is dropped.
///
/// Typed through the built binary, as `tests/cli_keys.rs` drives it, so the serial
/// protocol, the layout compiler and the release-all are the shipped ones and not a copy. `key
/// ctrl+c` and `type` both send a full release on their own way out, which is where this test's
/// "always send a release-all before exiting" (CLAUDE.md) comes from.
struct TargetTone {
    running: bool,
}

impl TargetTone {
    /// Type the command and press Enter.
    fn start() -> TargetTone {
        // `-l 0` loops for ever: the tone has to outlast a capture that is started afterwards.
        nanokvm(&["type", "speaker-test -t sine -f 1000 -c 2 -l 0"]);
        nanokvm(&["key", "enter"]);
        println!("[target] speaker-test -t sine -f 1000 -c 2 -l 0 started");
        TargetTone { running: true }
    }

    /// Ctrl+C, and then a release-all. Idempotent, and also run by [`Drop`].
    fn stop(&mut self) {
        if !self.running {
            return;
        }
        self.running = false;
        nanokvm(&["key", "ctrl+c"]);
        println!("[target] Ctrl+C sent; speaker-test stopped");
    }
}

impl Drop for TargetTone {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Run the built `nanokvm` binary and require it to succeed.
fn nanokvm(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_nanokvm"))
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run nanokvm {args:?}: {e}"));
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    // `Device or resource busy` from the serial node is almost never a bug in what is under test:
    // it is something else on the recorded test setup holding the dongle — another viewer, another agent, a
    // previous run that has not exited. Say that rather than leaving the reader to work it out
    // from an ALSA-shaped test failing on a tty.
    assert!(
        out.status.success(),
        "nanokvm {args:?} failed:\n{stderr}{}",
        if stderr.contains("Device or resource busy") {
            "\nSomething else holds the dongle's serial node. Check `fuser /dev/ttyACM1` and run \
             this again when it is free; nothing above is evidence about the audio path."
        } else {
            ""
        }
    );
    // The release-all is a claim the CLI makes on its own stderr; showing it here is how the run
    // records that the target was left with nothing held (CLAUDE.md).
    for line in stderr.lines().filter(|l| l.contains("release-all")) {
        println!("[target] {line}");
    }
    stderr
}

// ---- capture --------------------------------------------------------------------------------

/// How many periods at the start of a freshly opened capture stream are stale — audio the
/// device had from before the open.
///
/// Measured on the recorded test setup 2026-09-11, and the audio analogue of "up to eight frames at the
/// previous resolution"in a two-second capture taken eight seconds after the target's tone was
/// stopped, the whole of the remaining energy is in the first 10 ms — about 2 ms of the tone at
/// full level — and every half-second window after it is the noise floor (rms 3.1e-5). It is a
/// fixed quantity, not a decaying tail: captures taken 4 s and 8 s after Ctrl+C returned the same
/// power to within 1%.
///
/// Two milliseconds fits inside one period, so one period is discarded. It matters because 2 ms
/// of a loud tone averaged over a two-second capture is a ratio near 100, which is
/// [`TONE_RATIO`] — a negative control that measures the previous test's tone is not a control at
/// all, and one sweep produced that (ratio 85, against a threshold of 100).
const STALE_PERIODS: usize = 1;

/// Read `want` of audio off `device`, one period at a time, through the shipped [`AlsaCapture`],
/// discarding [`STALE_PERIODS`] at the front and reporting what was in them.
///
/// Capture only. This opens no sink, so a test that only wants to know what the dongle hears
/// cannot accidentally put it anywhere.
///
/// The stale prefix is *reported* rather than silently dropped, every time, so the device
/// behaviour stays visible in the transcript of every run rather than being a comment somewhere
/// claiming it was measured once.
fn capture_for(label: &str, device: &str, config: &AudioConfig, want: Duration) -> Vec<i16> {
    let mut source = AlsaCapture::open(device, config)
        .unwrap_or_else(|e| panic!("open {device} for capture: {e}"));
    println!("[capture] {label}: {}", source.describe());

    let periods = (want.as_secs_f64() * f64::from(SAMPLE_RATE) / config.ring.period_frames as f64)
        .ceil() as usize;
    let total = periods + STALE_PERIODS;
    let mut period = vec![0i16; config.ring.period_samples()];
    let mut pcm = Vec::with_capacity(total * config.ring.period_samples());
    let started = Instant::now();
    for i in 0..total {
        source
            .read_period(&mut period)
            .unwrap_or_else(|e| panic!("period {i} of {total} from {device}: {e}"));
        pcm.extend_from_slice(&period);
    }
    let head = STALE_PERIODS * config.ring.period_samples();
    let stale = find_tone(
        &mono(&pcm[..head]),
        SAMPLE_RATE,
        TONE_HZ,
        &DEFAULT_REFERENCES,
    );
    let pcm = pcm.split_off(head);
    println!(
        "[capture] {label}: {} periods ({:.2} s of audio) read in {:.2} s, after discarding {} \
         stale period(s) whose own rms was {:.6} (ratio {:.2})",
        periods,
        pcm.len() as f64 / f64::from(SAMPLE_RATE) / 2.0,
        started.elapsed().as_secs_f64(),
        STALE_PERIODS,
        stale.rms,
        stale.ratio,
    );
    if let Some(dir) = std::env::var_os("NANOKVM_AUDIO_OUT") {
        let path = PathBuf::from(dir).join(format!("capture-{}.wav", slug(label)));
        write_wav(&path, &pcm);
    }
    pcm
}

/// A filename-safe version of a label.
fn slug(label: &str) -> String {
    label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Print aggregate and half-second tone ratios. Short windows expose stale samples
/// that a whole-recording average can hide.
fn report_on(label: &str, pcm: &[i16]) -> ToneReport {
    let samples = mono(pcm);
    let report = find_tone(&samples, SAMPLE_RATE, TONE_HZ, &DEFAULT_REFERENCES);
    println!(
        "[tone] {label}: {report:?}\n[tone] {label}: ratio {:.4} vs TONE_RATIO {TONE_RATIO}, \
         rms {:.6}, present = {}",
        report.ratio,
        report.rms,
        report.present(TONE_RATIO)
    );
    let window = SAMPLE_RATE as usize / 2;
    for (i, chunk) in samples.chunks(window).enumerate() {
        if chunk.len() < window / 2 {
            continue;
        }
        let w = find_tone(chunk, SAMPLE_RATE, TONE_HZ, &DEFAULT_REFERENCES);
        println!(
            "[tone]   {label} +{:.1}s: ratio {:>12.2}  rms {:.6}",
            i as f64 * 0.5,
            w.ratio,
            w.rms
        );
    }
    report
}

/// Write interleaved stereo `i16` as a PCM WAV, so a run keeps what it measured.
fn write_wav(path: &Path, pcm: &[i16]) {
    let data = pcm.len() * 2;
    let mut out = Vec::with_capacity(44 + data);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + data) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&2u16.to_le_bytes()); // stereo
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE * 4).to_le_bytes()); // byte rate
    out.extend_from_slice(&4u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data as u32).to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, out).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    println!("[wav] wrote {}", path.display());
}

// ---- a tiny WAV reader ----------------------------------------------------------------------

/// Read a PCM WAV file as interleaved `i16`.
///
/// small and tolerant of the declared `data` size: a recorder that is
/// stopped by a signal may leave the chunk length short or unwritten, and a reader that trusted
/// it would silently return a fraction of the audio — which would look like "the sink got
/// silence", the defect this file exists to be able to see.
fn read_wav(path: &Path) -> Vec<i16> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert!(
        bytes.len() > 44 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE",
        "{} is not a RIFF/WAVE file ({} bytes)",
        path.display(),
        bytes.len()
    );

    let u16_at = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
    let u32_at =
        |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);

    let mut pos = 12;
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    let mut data: Option<(usize, usize)> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32_at(pos + 4) as usize;
        let body = pos + 8;
        if id == b"fmt " && body + 16 <= bytes.len() {
            fmt = Some((
                u16_at(body),
                u16_at(body + 2),
                u32_at(body + 4),
                u16_at(body + 14),
            ));
        } else if id == b"data" {
            // The declared size, clamped to what is there.
            let available = bytes.len() - body;
            data = Some((body, size.min(available)));
            break;
        }
        pos = body + size + (size & 1);
    }

    let (format, channels, rate, bits) = fmt.unwrap_or_else(|| panic!("no fmt chunk"));
    let (start, len) = data.unwrap_or_else(|| panic!("no data chunk"));
    assert_eq!(format, 1, "{}: not uncompressed PCM", path.display());
    assert_eq!(bits, 16, "{}: not 16-bit", path.display());
    assert_eq!(channels, 2, "{}: not stereo", path.display());
    assert_eq!(rate, SAMPLE_RATE, "{}: not 48 kHz", path.display());

    let samples: Vec<i16> = bytes[start..start + len]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes(*c))
        .collect();
    println!(
        "[wav] {}: {} samples, {:.2} s",
        path.display(),
        samples.len(),
        samples.len() as f64 / f64::from(SAMPLE_RATE) / 2.0
    );
    samples
}

// ---- snapshot helpers -----------------------------------------------------------------------

fn print_snapshot(label: &str, s: &AudioSnapshot) {
    println!(
        "[stats {label}] captured {} played {} silent {} muted {} | overruns {} underruns {} | \
         drift dropped {} inserted {} | pushed {} popped {} | opens capture {} playback {} | \
         conditions {} recoveries {} | condition {}",
        s.periods_captured,
        s.periods_played,
        s.periods_silent,
        s.periods_muted,
        s.counts.overruns,
        s.counts.underruns,
        s.counts.drift_drops,
        s.counts.drift_inserts,
        s.counts.pushed,
        s.counts.popped,
        s.capture_opens,
        s.playback_opens,
        s.conditions_logged,
        s.recoveries_logged,
        s.condition
            .as_ref()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "none".to_string()),
    );
}

/// Every thread name of another process, as the kernel sees them (truncated to 15 bytes).
fn threads_of(pid: u32) -> Vec<String> {
    let mut names = Vec::new();
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return names;
    };
    for entry in entries.flatten() {
        if let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) {
            names.push(comm.trim().to_string());
        }
    }
    names
}

/// The names of the threads [`AudioHandle`] spawns, as the kernel sees them.
fn audio_threads() -> Vec<String> {
    let mut names = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
        return names;
    };
    for entry in entries.flatten() {
        if let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) {
            let comm = comm.trim().to_string();
            if comm.starts_with("nanokvm-audio") {
                names.push(comm);
            }
        }
    }
    names.sort();
    names
}

/// Poll `check` until it is true or `limit` has passed. Returns how long it took.
fn wait_until(limit: Duration, mut check: impl FnMut() -> bool) -> Option<Duration> {
    let started = Instant::now();
    while started.elapsed() < limit {
        if check() {
            return Some(started.elapsed());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

// ---- the exit criterion ---------------------------------------------------------------------

/// Bracket the tone recording with quiet recordings to reject persistent hum and
/// false-positive detectors.
#[test]
#[ignore = "needs the dongle, the target, and the target's HDMI audio"]
fn tone_at_1khz_is_captured_from_the_target() {
    let _desk = DESK.lock().unwrap_or_else(|e| e.into_inner());
    init_logging();
    let card = resolve_card();
    let config = AudioConfig::default();

    let before = report_on(
        "before (nothing playing)",
        &capture_for("before", &card.device, &config, Duration::from_secs(2)),
    );
    assert!(
        !before.present(TONE_RATIO),
        "there is already a 1 kHz peak on this desk with nothing playing — the measurement below \
         would prove nothing: {before:?}"
    );

    let mut tone = TargetTone::start();
    // The Pi's desktop takes a moment to start `speaker-test`, and `speaker-test` itself prints a
    // preamble before the first sine.
    std::thread::sleep(Duration::from_secs(2));

    let during = report_on(
        "with the tone",
        &capture_for("during", &card.device, &config, Duration::from_secs(3)),
    );
    assert!(
        during.present(TONE_RATIO),
        "no 1 kHz peak in the captured PCM while the target plays one. The detector is unit \
         tested against synthetic sines including a quiet one under noise, so this is about the \
         audio path: {during:?}"
    );

    tone.stop();
    std::thread::sleep(TONE_SETTLE);
    let quiet = capture_for("after", &card.device, &config, Duration::from_secs(2));
    let after = report_on("after Ctrl+C", &quiet);
    assert!(
        !after.present(TONE_RATIO),
        "the 1 kHz peak survived Ctrl+C on the target, so it did not come from the target: \
         {after:?}"
    );
    // And not merely below the threshold: at the floor. `capture_for` has already discarded the
    // device's stale first period, so what is left is only what the card produced after the open,
    // and the margin against the tone above is five orders of magnitude rather than a factor of
    // two.
    assert!(
        after.ratio < 10.0,
        "everything the card produced after the open must be the noise floor: {after:?}"
    );

    println!(
        "[verdict] ratio with the tone {:.0}, without it {:.2} before and {:.2} after; \
         TONE_RATIO is {TONE_RATIO}",
        during.ratio, before.ratio, after.ratio
    );
}

/// Record the host sink monitor while the complete audio path runs. A temporary
/// null sink verifies playback without sending the test tone to the user’s speakers.
#[test]
#[ignore = "needs the dongle, the target, and PipeWire on the host"]
fn playback_reaches_the_host_sink() {
    let _desk = DESK.lock().unwrap_or_else(|e| e.into_inner());
    init_logging();
    let card = resolve_card();
    let sink = NullSink::load();
    sink.route_alsa_default_here();
    let config = AudioConfig::default();

    let mut tone = TargetTone::start();
    // Three seconds, not two: the Pi's desktop has to start `speaker-test`, which prints a
    // preamble and negotiates a one-second buffer before the first sine leaves it. One sweep
    // recorded a silent sink here with the capture side counting periods perfectly, which is what
    // "the tone had not started yet" looks like from the host.
    std::thread::sleep(Duration::from_secs(3));

    let mut audio = AudioHandle::spawn(
        Box::new(FixedSourceOpener::new(&card.device, config)),
        Box::new(DefaultSinkOpener::new(config)),
        config,
    );

    // Let both sides open and the playback prefill drain through before recording: a monitor
    // recorded during the prefill is mostly silence by design, not by defect.
    std::thread::sleep(Duration::from_secs(2));
    sink.assert_our_playback_landed_here();
    let running = audio_threads();
    assert_eq!(
        running.len(),
        2,
        "both audio threads must be running; /proc/self/task shows {running:?} \
         (the kernel truncates a thread name to 15 bytes, so these are prefixes)"
    );

    let wav = out_dir().join("audio-hardware-sink-monitor.wav");
    let recorded = sink.record_monitor(&wav, 4);

    let snapshot = audio.snapshot();
    print_snapshot("playback", &snapshot);
    audio.stop();

    let report = report_on("the host's sink monitor", &recorded);
    // A silent monitor has two possible causes and they are fixed in different places: the sink
    // got nothing, or the target was not making a sound in the first place. The card is free now
    // that the handle has stopped and the tone is still playing, so ask it rather than guess —
    // one sweep was lost to a run where `speaker-test` had not started, and the counters cannot
    // tell the difference because a period of silence is still a period.
    if !report.present(TONE_RATIO) {
        report_on(
            "the card directly, while the sink monitor was silent",
            &capture_for("diagnosis", &card.device, &config, Duration::from_secs(2)),
        );
    }
    tone.stop();

    assert!(
        report.present(TONE_RATIO),
        "the sink received no 1 kHz tone. The capture side saw {} periods and the playback side \
         wrote {} real and {} silent, so this is about what reached the sink: {report:?}",
        snapshot.periods_captured,
        snapshot.periods_played,
        snapshot.periods_silent
    );

    // The counters have to agree with the recording: a run that "played" nothing but silence
    // would be a defect even if the recording somehow found a peak.
    assert!(
        snapshot.periods_played > 0,
        "no real period was written to the sink: {snapshot:?}"
    );
    assert!(
        snapshot.periods_played > snapshot.periods_silent,
        "the sink got more silence than audio: {} real, {} silent",
        snapshot.periods_played,
        snapshot.periods_silent
    );
    assert_eq!(snapshot.conditions_logged, 0, "no condition was expected");
    assert_eq!(
        snapshot.capture_opens, 1,
        "the card should have been opened exactly once"
    );
    assert_eq!(snapshot.playback_opens, 1, "and the sink exactly once");
    assert!(
        audio_threads().is_empty(),
        "stop left an audio thread behind: {:?}",
        audio_threads()
    );
}

// ---- drift ----------------------------------------------------------------------------------

/// Measure capture/playback drift over `NANOKVM_DRIFT_SECS` seconds (at least ten
/// minutes by default). Record correction counters and occupancy after warmup;
/// the separate clocks can fill or drain the ring even without xruns.
/// See docs/hardware.md for the recorded run and its limitations.
#[test]
#[ignore = "takes ten minutes and needs the dongle"]
fn drift_over_ten_minutes() {
    let _desk = DESK.lock().unwrap_or_else(|e| e.into_inner());
    init_logging();
    let card = resolve_card();
    let sink = NullSink::load();
    sink.route_alsa_default_here();
    let config = AudioConfig::default();

    let secs: u64 = std::env::var("NANOKVM_DRIFT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600);
    println!(
        "[drift] running for {secs}s with ring {}x{} frames ({:?})",
        config.ring.periods,
        config.ring.period_frames,
        config.ring.depth()
    );

    let mut audio = AudioHandle::spawn(
        Box::new(FixedSourceOpener::new(&card.device, config)),
        Box::new(DefaultSinkOpener::new(config)),
        config,
    );

    // The steady state starts after both sides have opened and the prefill has drained. Every
    // rate below is computed from this mark, not from `spawn`, so the prefill is not counted as
    // drift.
    std::thread::sleep(Duration::from_secs(30));
    sink.assert_our_playback_landed_here();
    let mark = Instant::now();
    let first = audio.snapshot();
    print_snapshot("t=0 (mark)", &first);

    let mut last = first.clone();
    let mut elapsed = Duration::ZERO;
    while elapsed < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_secs(30));
        elapsed = mark.elapsed();
        last = audio.snapshot();
        print_snapshot(&format!("t={:.0}s", elapsed.as_secs_f64()), &last);
        assert!(
            last.condition.is_none(),
            "audio went into a condition during the drift run: {:?}",
            last.condition
        );
    }

    let elapsed = mark.elapsed().as_secs_f64();
    print_snapshot("final", &last);
    audio.stop();

    let frames = config.ring.period_frames as f64;
    let rate = f64::from(SAMPLE_RATE);

    let corrections = last.counts.drift_drops as i64
        - first.counts.drift_drops as i64
        - (last.counts.drift_inserts as i64 - first.counts.drift_inserts as i64);
    let ppm_corrections = (corrections as f64 * frames / elapsed) / rate * 1e6;

    let written =
        |s: &AudioSnapshot| (s.periods_played + s.periods_silent + s.periods_muted) as i64;
    // The level the ring is holding: everything pushed that has not been popped, dropped as an
    // overrun or dropped as a drift correction.
    let level = |s: &AudioSnapshot| {
        s.counts.pushed as i64
            - s.counts.popped as i64
            - s.counts.overruns as i64
            - s.counts.drift_drops as i64
    };
    let d_captured = last.periods_captured as i64 - first.periods_captured as i64;
    let d_written = written(&last) - written(&first);
    let balance = d_captured - d_written;
    let ppm_balance = (balance as f64 * frames / elapsed) / rate * 1e6;
    let d_level = level(&last) - level(&first);
    let resolution = (frames / elapsed) / rate * 1e6;

    println!(
        "\n[drift] over {elapsed:.1}s of steady state, {d_captured} periods captured and \
         {d_written} written:\n\
         [drift]   corrections:   {corrections:+} net periods dropped by the policy \
         => {ppm_corrections:+.1} ppm\n\
         [drift]   full balance:  {balance:+} net periods (captured - written) \
         => {ppm_balance:+.1} ppm\n\
         [drift]   the two differ by level {d_level:+} + overruns {:+} - underruns {:+}, which is \
         the conservation law; the full balance is the one to trust\n\
         [drift]   resolution {resolution:.1} ppm (one whole period over this window)\n\
         [drift]   a positive rate means the dongle's clock is faster than the host's.",
        last.counts.overruns as i64 - first.counts.overruns as i64,
        last.counts.underruns as i64 - first.counts.underruns as i64,
    );

    // The law itself, which is worth asserting rather than only printing: if it does not hold,
    // one of the counters is not counting what its name says and every number above is fiction.
    assert_eq!(
        balance,
        d_level
            + (last.counts.drift_drops as i64 - first.counts.drift_drops as i64)
            + (last.counts.overruns as i64 - first.counts.overruns as i64)
            - (last.counts.drift_inserts as i64 - first.counts.drift_inserts as i64)
            - (last.counts.underruns as i64 - first.counts.underruns as i64),
        "captured - written must equal the ring's own account of where the difference went; \
         first {first:?} last {last:?}"
    );

    // The one thing the run must not show: a steady state that is not steady. An xrun on either
    // side in a ten-minute run with nothing else happening is a defect, not drift — drift is what
    // the corrections are for, and `DriftPolicy::reset` makes an xrun *not* count as
    // drift so this number stays readable.
    assert_eq!(
        last.counts.overruns - first.counts.overruns,
        0,
        "the ring overran in steady state"
    );
    assert_eq!(
        last.counts.underruns - first.counts.underruns,
        0,
        "the ring underran in steady state"
    );
    assert!(
        last.periods_captured - first.periods_captured > 0,
        "nothing was captured at all"
    );
    assert!(
        audio_threads().is_empty(),
        "stop left an audio thread behind: {:?}",
        audio_threads()
    );
}

// ---- failure isolation on real hardware -------------------------------------------------------

/// Hold the capture card with `pw-record` to exercise busy-device reporting,
/// retry logging, and continued video/input operation.
#[test]
#[ignore = "needs the dongle and PipeWire on the host"]
fn busy_card_is_reported_once_and_changes_nothing() {
    let _desk = DESK.lock().unwrap_or_else(|e| e.into_inner());
    init_logging();
    let card = resolve_card();
    let source = pipewire_source_for(&card);
    println!("[busy] holding the card through PipeWire source {source}");
    let sink = NullSink::load();
    sink.route_alsa_default_here();
    let config = AudioConfig::default();

    let mut holder = Some(
        ChildGuard::spawn(
            "pw-record",
            Command::new("pw-record")
                .args(["--target", &source, "/dev/null"])
                // Not inherited. `PIPEWIRE_NODE` is set in this process to keep our own
                // playback off the user's speakers, and it overrides `--target` in any PipeWire
                // client that reads it — so a holder spawned with it inherited records the null
                // sink's monitor and never touches the card, and the test then measures nothing
                // while looking like it passed. Found that way on 2026-09-11.
                .env_remove("PIPEWIRE_NODE")
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        )
        .expect("spawn pw-record"),
    );
    // PipeWire opens the ALSA device when the stream starts, not when the client connects.
    std::thread::sleep(Duration::from_secs(2));

    let mut audio = AudioHandle::spawn(
        Box::new(FixedSourceOpener::new(&card.device, config)),
        Box::new(DefaultSinkOpener::new(config)),
        config,
    );

    let stats = audio.stats();
    let seen = wait_until(Duration::from_secs(15), || {
        stats.condition_of(AudioSide::Capture).is_some()
    });
    let condition = stats.condition_of(AudioSide::Capture).unwrap_or_else(|| {
        panic!(
            "the capture side never reported a condition while {source} held the card; \
             snapshot: {:?}",
            stats.snapshot()
        )
    });
    println!(
        "[busy] condition after {:?}: {condition} (kind {:?})",
        seen, condition.kind
    );
    assert_eq!(
        condition.kind, "busy",
        "a held card must be reported as busy, not as {:?}: {condition}",
        condition.kind
    );
    assert!(
        condition.message.contains(&card.device),
        "the condition must name the device: {condition}"
    );

    // Several more retries at the 500 ms backoff, and still one condition. This is the "log once"
    // claim, asserted on the counter the log line is driven from.
    std::thread::sleep(Duration::from_secs(4));
    let busy = audio.snapshot();
    print_snapshot("busy", &busy);
    assert_eq!(
        busy.conditions_logged, 1,
        "one outage must be one logged condition, however many opens were retried: {busy:?}"
    );
    assert_eq!(
        busy.periods_captured, 0,
        "nothing can have been captured while another client holds the card"
    );
    assert!(
        busy.capture_opens == 0,
        "no capture open can have succeeded: {busy:?}"
    );

    // Release the card. `ChildGuard`'s `Drop` kills and reaps, so the card is genuinely free
    // — not merely signalled — before the assertions about recovery start.
    drop(holder.take());
    std::thread::sleep(Duration::from_secs(1));
    println!("[busy] holder stopped; waiting for the card to come back");

    let recovered = wait_until(Duration::from_secs(20), || {
        stats.condition_of(AudioSide::Capture).is_none() && stats.snapshot().periods_captured > 0
    })
    .unwrap_or_else(|| {
        panic!(
            "audio never recovered after the holder went away: {:?}",
            stats.snapshot()
        )
    });
    let after = audio.snapshot();
    print_snapshot("recovered", &after);
    println!("[busy] recovered after {recovered:?}");
    assert_eq!(
        after.recoveries_logged, 1,
        "recovery is logged exactly once: {after:?}"
    );
    assert_eq!(
        after.conditions_logged, 1,
        "and no new condition was invented on the way back: {after:?}"
    );

    let stopping = Instant::now();
    audio.stop();
    let took = stopping.elapsed();
    println!("[busy] stop took {took:?}");
    assert!(
        took < config.stop_deadline + Duration::from_millis(500),
        "stop overran its deadline: {took:?}"
    );
    assert!(
        audio_threads().is_empty(),
        "stop left an audio thread behind: {:?}",
        audio_threads()
    );
}

/// Run the viewer with `--no-audio` and inspect its threads and logs. Capture,
/// decode, and serial workers must run without audio workers, while normal
/// statistics and clean shutdown remain available.
#[test]
#[ignore = "needs the dongle, the target and a compositor"]
fn no_audio_preserves_video_and_input() {
    let _desk = DESK.lock().unwrap_or_else(|e| e.into_inner());
    init_logging();
    let log = out_dir().join("viewer-no-audio.log");
    let file =
        std::fs::File::create(&log).unwrap_or_else(|e| panic!("create {}: {e}", log.display()));

    // The chrome persists its settings to `$XDG_CONFIG_HOME/nanokvm/config.toml`
    // and writes once on the first run, so a viewer started without this would leave a file in
    // the user's own `~/.config`. Pointing it at a scratch directory is the same rule every
    // other run of the viewer on the recorded test setup follows; it is here rather than in `out_dir` because
    // this is the only test in this file that starts the shipped binary as a window.
    // The directory name contains neither "audio" nor a word this test greps for:
    // setting `XDG_CONFIG_HOME` also redirects the Vulkan loader's layer search, which prints the
    // paths it looked in, and a path with "audio" in it lands in the "one line mentions
    // audio" assertion below.
    let config_root = out_dir().join("nk-settings-root");
    std::fs::create_dir_all(&config_root)
        .unwrap_or_else(|e| panic!("create {}: {e}", config_root.display()));

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nanokvm"));
    cmd.args(["--no-audio", "--exit-after", "6", "--stats-interval", "2"])
        .env("RUST_LOG", "info")
        .env("XDG_CONFIG_HOME", &config_root)
        .stdout(Stdio::null())
        .stderr(Stdio::from(file));
    let mut viewer = ChildGuard::spawn("nanokvm --no-audio", &mut cmd).expect("spawn the viewer");
    let pid = viewer.pid().expect("the viewer has a pid");

    // Sampled while it is up, several times: a single sample taken before the threads would have
    // been spawned proves nothing.
    let mut seen: Vec<String> = Vec::new();
    for _ in 0..8 {
        std::thread::sleep(Duration::from_millis(500));
        for name in threads_of(pid) {
            if !seen.contains(&name) {
                seen.push(name);
            }
        }
    }
    seen.sort();
    println!("[--no-audio] threads seen: {seen:?}");

    let status = viewer
        .wait_bounded(Duration::from_secs(20))
        .expect("wait for the viewer");
    let status = status.expect("the viewer must exit at --exit-after, not hang");
    let text =
        std::fs::read_to_string(&log).unwrap_or_else(|e| panic!("read {}: {e}", log.display()));

    assert!(
        !seen.iter().any(|n| n.starts_with("nanokvm-audio")),
        "--no-audio must spawn no audio thread, and these were running: {seen:?}"
    );
    // The kernel truncates a thread name to 15 bytes, so these are the prefixes of
    // `nanokvm-capture`, `nanokvm-decode` and `nanokvm-serial-writer`.
    for worker_name in ["nanokvm-capture", "nanokvm-decode", "nanokvm-serial-"] {
        assert!(
            seen.iter().any(|n| n == worker_name),
            "the viewer thread {worker_name} was not running, so the sample above proves nothing \
             about audio: {seen:?}"
        );
    }
    // A crash here is almost certainly not about audio, and the message says so: with
    // `--no-audio` there is no audio thread, no card is opened, and `src/viewer/render.rs` is
    // untouched by. What the recorded test setup produces instead is a shutdown race — see the
    // hardware-run notes: when the compositor is not scheduling the surface (a locked session),
    // `present` blocks, the render thread overruns its 500 ms join deadline and is detached, and
    // the detached thread then drops the wgpu/EGL instance after the main thread has closed the
    // Wayland connection. Observed once in seven runs, with the core dump naming
    // `wgpu_hal::gles::egl::Inner::drop` under `wl_proxy_marshal_flags`.
    assert!(
        status.success(),
        "the viewer exited {status}.{}",
        if text.contains("has not finished within") {
            " Its log contains the render thread's detach warning, so this is the viewer \
             shutdown race and not an audio failure: every audio assertion above passed. \
             `coredumpctl info` on this pid will show whether it died in the detached render \
             thread's wgpu teardown."
        } else {
            ""
        }
    );

    let audio_lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("audio") && !l.contains("audio off"))
        .collect();
    assert_eq!(
        audio_lines.len(),
        1,
        "exactly one line may mention audio — the one that says it is off — and the title's own \
         `audio off` is not it. Got: {audio_lines:?}"
    );
    assert!(
        audio_lines[0].contains("disabled by --no-audio"),
        "and that line is the disabled notice: {}",
        audio_lines[0]
    );
    assert!(
        !text.contains("stats audio:"),
        "no audio statistics line may be printed when there is no audio"
    );
    assert!(
        text.contains("stats capture:") && text.contains("stats input:"),
        "the viewer statistics lines must still be there, or this proves nothing"
    );
    assert!(
        text.contains("audio off"),
        "the title must say audio is off rather than leaving it out"
    );
    println!("[--no-audio] {}", audio_lines[0]);
    println!("[--no-audio] log kept at {}", log.display());
}

/// Verify audio re-pairs after card removal while video and input continue.
/// Requires `NANOKVM_ALLOW_REPLUG` and write access to the selected usbfs node.
/// The recorded setup could not run this test because usbfs access required
/// privilege; do not count that deferred run as a hardware pass.
#[test]
#[ignore = "needs write access to usbfs and NANOKVM_ALLOW_REPLUG=1; see the doc comment"]
fn card_vanishing_mid_session() {
    let _desk = DESK.lock().unwrap_or_else(|e| e.into_inner());
    init_logging();
    let card = resolve_card();

    let dry = Command::new("python3")
        .args(["scripts/usb-replug.py", "--dry-run", "video"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run usb-replug.py --dry-run");
    let resolved = String::from_utf8_lossy(&dry.stdout).to_string();
    println!("{resolved}");
    let usbfs = resolved
        .lines()
        .find(|l| l.starts_with("resolved video"))
        .and_then(|l| {
            l.split_whitespace()
                .find(|w| w.starts_with("/dev/bus/usb/"))
        })
        .map(PathBuf::from)
        .expect("the script resolves the dongle to a usbfs node");

    let writable = std::fs::OpenOptions::new().write(true).open(&usbfs);
    let allowed = std::env::var("NANOKVM_ALLOW_REPLUG").as_deref() == Ok("1");
    if writable.is_err() || !allowed {
        println!(
            "\n=====================================================================\n\
             SKIPPED: card_vanishing_mid_session did not run.\n\
               usbfs node:            {}\n\
               writable:              {:?}\n\
               NANOKVM_ALLOW_REPLUG:  {}\n\
             It needs both. Without write access to the usbfs node the kernel cannot be asked to\n\
             unbind the audio interface, and a full-device reset is refused by policy while the\n\
             desk's /dev names are the ones every hardware test here hardcodes (CLAUDE.md).\n\
             The card resolved fine ({}), so everything but the replug is in place.\n\
             =====================================================================\n",
            usbfs.display(),
            writable.as_ref().map(|_| "yes").map_err(|e| e.to_string()),
            if allowed { "1" } else { "unset" },
            card.device,
        );
        return;
    }

    panic!(
        "the preconditions are met but the replug body is not written: this desk could not run \
         it, so writing it blind would be a claim with no measurement behind it. Write it against \
         `scripts/usb-replug.py video` and the `DiscoveringCardOpener`, and assert: the condition \
         appears once, video and input keep working, and the card is reopened under whatever \
         number it comes back as."
    );
}
