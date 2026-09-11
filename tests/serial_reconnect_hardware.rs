//! The Stage 2 hardware tests for §2.7 (reconnect) and §5.1 (torn-write resynchronisation).
//! Feature-gated and `#[ignore]`d, never gating CI (§9.3).
//!
//! ```text
//! # H-A1..H-A3 — safe to run at any time; they write only to /dev/ttyACM1.
//! cargo test --features hardware --test serial_reconnect_hardware -- \
//!     --ignored --nocapture --test-threads=1 --skip h_a4
//!
//! # H-A4 — takes the serial node away and puts it back. Ask before running it.
//! NANOKVM_REPLUG_METHOD=reset cargo test --features hardware \
//!     --test serial_reconnect_hardware -- --ignored --nocapture --test-threads=1 h_a4
//! ```
//!
//! **Verified by consequence, never by acknowledgement** (§3.4, A17). The consequences asserted
//! here are: a `GET_INFO` reply that arrives (the chip's receive parser is realigned), the lock
//! bits it carries (unchanged by anything these tests do), and — in H-A4 — the target's CapsLock
//! bit flipping and flipping back, which originates in the *target's* own HID output report and is
//! therefore proof the target processed the keystroke.
//!
//! **They leave the target as they found it.** CapsLock is tapped twice or not at all, and every
//! test releases everything on every path out, including a panic. **No test sends a mouse
//! button**: a click on a live desktop can launch or destroy something.
//!
//! **The malformed bytes come from the fixture file, not from this source.**
//! `fixtures/packets/ch9329.toml`'s `device_receive_parser_has_no_inter_byte_timeout` records the
//! exact trigger Stage 0 measured, and retyping it here is how a test quietly stops testing the
//! anomaly it names (§9.1).

#![cfg(feature = "hardware")]

use std::io::{BufRead, BufReader};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nanokvm::discovery::{Constraints, DiscoveringLinkSource, NodeKind, NodeResolver};
use nanokvm::input::{
    spawn_with_source, Config, Event, ReleaseOutcome, ReleaseReason, WriterHandle,
};
use nanokvm::link::{Link, LinkError, Reply};
use nanokvm::proto::cmd;
use nanokvm::proto::frame::DeviceInfo;
use nanokvm::proto::report::{HidKey, KeyboardReport, MOUSE_RELEASE_ALL};
use nanokvm::serial::{OpenOptions, SerialLink, RESYNC_PREAMBLE_LEN};
use serde::Deserialize;

#[path = "support/child_guard.rs"]
mod child_guard;

/// The dongle's serial node on this desk (CLAUDE.md): the `1a86:55d3` CDC-ACM child of the
/// dongle's own `1a40:0101` hub. `/dev/ttyACM0` is the dock's unrelated CDC-ACM device and is
/// never opened.
///
/// H-A1..H-A3 open this name directly, which is honest: they neither take the device away nor
/// expect it back, so the name cannot change under them, and a run on a renumbered desk fails
/// with this comment as the explanation. **H-A4 does not use it except as the expectation it
/// prints**: it reopens by identity (§8), because a replug is exactly when the name moves — and
/// on the run that produced this slice it moved to `/dev/ttyACM2` and stayed there.
const PORT: &str = "/dev/ttyACM1";
/// The reply deadline every assertion here uses. §2.7's `get_info_timeout` default.
const TIMEOUT: Duration = Duration::from_millis(500);
/// CapsLock's HID usage (fixture `kb_capslock_press`).
const CAPS_LOCK: u8 = 0x39;

// -- fixtures ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Fixtures {
    #[serde(default)]
    packet: Vec<Entry>,
    #[serde(default)]
    disagreement: Vec<Disagreement>,
}

#[derive(Debug, Deserialize)]
struct Entry {
    name: String,
    data: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct Disagreement {
    name: String,
    #[serde(default)]
    trigger: String,
}

fn fixtures() -> Fixtures {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/packets/ch9329.toml");
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    toml::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

/// Leading two-hex-digit tokens only. Mirrors `scripts/check-fixtures.py::parse` and
/// `tests/serial_pty.rs::hex_prefix`.
fn hex_prefix(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for token in s.split_whitespace() {
        match u8::from_str_radix(token, 16) {
            Ok(b) if token.len() == 2 => out.push(b),
            _ => break,
        }
    }
    out
}

fn payload(name: &str) -> Vec<u8> {
    fixtures()
        .packet
        .into_iter()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("no fixture named {name}"))
        .data
}

/// The bytes Stage 0 measured as leaving the chip's parser mid-frame: a header claiming `LEN 8`
/// with only two payload bytes sent, which is what a `write_all` that failed after seven bytes
/// leaves on the wire (fixture `device_receive_parser_has_no_inter_byte_timeout`).
fn torn_frame() -> Vec<u8> {
    let name = "device_receive_parser_has_no_inter_byte_timeout";
    let bytes = hex_prefix(
        &fixtures()
            .disagreement
            .into_iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("no disagreement named {name}"))
            .trigger,
    );
    assert_eq!(
        bytes.len(),
        7,
        "the fixture's trigger should be the seven bytes of a torn keyboard frame, got {bytes:02X?}"
    );
    bytes
}

// -- the release-all guard --------------------------------------------------

/// Owns the link and leaves the chip clean when it goes out of scope, however it goes out of scope
/// (§2.6: release-all on clean shutdown; the same applies to a failed test).
///
/// It runs the §2.7 order: **preamble first, then the release**. These tests deliberately tear
/// frames, so a release-all sent without the preamble in front of it is exactly the frame that
/// gets eaten as the remainder — which would leave the target holding keys precisely when the test
/// failed.
struct Released(SerialLink);

impl Deref for Released {
    type Target = SerialLink;
    fn deref(&self) -> &SerialLink {
        &self.0
    }
}
impl DerefMut for Released {
    fn deref_mut(&mut self) -> &mut SerialLink {
        &mut self.0
    }
}

impl Drop for Released {
    fn drop(&mut self) {
        match self.0.resync() {
            Ok(()) => eprintln!("[release-all] resynchronisation preamble sent"),
            Err(e) => eprintln!("[release-all] preamble UNSENT: {e}"),
        }
        let kb = KeyboardReport::RELEASE_ALL.payload();
        let ms = MOUSE_RELEASE_ALL.payload();
        for (name, command, data) in [
            ("keyboard", cmd::SEND_KB_GENERAL_DATA, &kb[..]),
            ("mouse", cmd::SEND_MS_REL_DATA, &ms[..]),
        ] {
            match self.0.transact(command, data, TIMEOUT) {
                Ok(_) => eprintln!("[release-all] {name} report sent"),
                // §2.6.1: an unsent release is worth saying out loud, not hiding.
                Err(e) => eprintln!("[release-all] {name} report UNSENT: {e}"),
            }
        }
    }
}

fn open() -> Released {
    Released(SerialLink::open(Path::new(PORT)).expect("open /dev/ttyACM1"))
}

/// Drain and print everything the device pushed that nothing asked for. Both a diagnostic and the
/// A12 evidence channel: a `0xC2`/`0xE4` here is the chip reporting the checksum error a
/// zero-completed frame produces, which is the outcome the preamble's safety argument predicts.
fn drain_unsolicited(link: &Released, label: &str) -> Vec<Reply> {
    let mut seen = Vec::new();
    while let Ok(reply) = link.unsolicited().try_recv() {
        eprintln!(
            "[{label}] unsolicited {:#04x} data {:02X?}{}",
            reply.cmd,
            reply.data,
            match reply.error_code() {
                Some(code) => format!("  (error code {code:#04x})"),
                None => String::new(),
            }
        );
        seen.push(reply);
    }
    if seen.is_empty() {
        eprintln!("[{label}] no unsolicited frames");
    }
    seen
}

fn locks(info: &DeviceInfo) -> (bool, bool, bool) {
    (info.num_lock, info.caps_lock, info.scroll_lock)
}

// -- H-A1 -------------------------------------------------------------------

/// **H-A1 — zeros are harmless.** The preamble in front of a healthy link changes nothing the
/// device reports: `GET_INFO` still answers inside the deadline and the lock bits are the ones it
/// gave before.
#[test]
#[ignore = "needs the NanoKVM-USB dongle"]
fn h_a1_a_zero_preamble_leaves_a_healthy_link_untouched() {
    let mut link = open();

    let t = Instant::now();
    let before = link
        .get_info(TIMEOUT)
        .expect("GET_INFO before the preamble");
    println!("[H-A1] GET_INFO before: {before:?} in {:?}", t.elapsed());

    let t = Instant::now();
    link.resync().expect("the preamble is written");
    println!(
        "[H-A1] {RESYNC_PREAMBLE_LEN}-byte zero preamble written in {:?}",
        t.elapsed()
    );

    let t = Instant::now();
    let after = link.get_info(TIMEOUT).expect("GET_INFO after the preamble");
    let rtt = t.elapsed();
    println!("[H-A1] GET_INFO after: {after:?} in {rtt:?}");

    assert!(rtt < TIMEOUT, "GET_INFO took {rtt:?}");
    assert_eq!(
        locks(&before),
        locks(&after),
        "the preamble must not change the target's lock state"
    );
    drain_unsolicited(&link, "H-A1");
    println!("[H-A1] stats: {:?}", link.stats());
    assert_eq!(link.stats().resyncs_sent, 1);
    assert_eq!(link.is_down(), None);
}

// -- H-A2 -------------------------------------------------------------------

/// **H-A2 — a torn frame is recovered.** Tear a keyboard frame on purpose with the raw port, write
/// the preamble, and `GET_INFO` must answer inside the deadline. Then release everything and check
/// the lock bits are where they started.
#[test]
#[ignore = "needs the NanoKVM-USB dongle"]
fn h_a2_the_preamble_recovers_a_deliberately_torn_frame() {
    let mut link = open();
    let start = link.get_info(TIMEOUT).expect("GET_INFO at the start");
    println!("[H-A2] GET_INFO at the start: {start:?}");

    let torn = torn_frame();
    println!("[H-A2] tearing a keyboard frame: {torn:02X?}");
    link.write_raw_for_test(&torn)
        .expect("the torn bytes are written");

    let t = Instant::now();
    link.resync().expect("the preamble is written");
    println!("[H-A2] preamble written in {:?}", t.elapsed());

    let t = Instant::now();
    let recovered = link.get_info(TIMEOUT);
    let rtt = t.elapsed();
    println!("[H-A2] GET_INFO after the preamble: {recovered:?} in {rtt:?}");
    let recovered = recovered.expect("GET_INFO must answer once the torn frame is completed");
    assert!(rtt < TIMEOUT, "GET_INFO took {rtt:?}");

    // Whatever the chip made of the zero-completed frame lands here: a `0xC2`/`0xE4` checksum
    // error answers cmd 0x02, not GET_INFO, so it is diverted rather than mistaken for a reply.
    let pushed = drain_unsolicited(&link, "H-A2");
    println!(
        "[H-A2] {} unsolicited frame(s) after the repair; checksum errors: {}",
        pushed.len(),
        pushed
            .iter()
            .filter(|r| r.error_code() == Some(0xE4))
            .count()
    );

    // Release everything, then confirm the target is where it started.
    link.transact(
        cmd::SEND_KB_GENERAL_DATA,
        &payload("kb_release_all"),
        TIMEOUT,
    )
    .expect("release-all acknowledged");
    let end = link.get_info(TIMEOUT).expect("GET_INFO at the end");
    println!("[H-A2] GET_INFO at the end: {end:?}");
    assert_eq!(
        locks(&start),
        locks(&end),
        "the target must be left exactly as it was found"
    );
    assert_eq!(locks(&recovered), locks(&end));
    println!("[H-A2] stats: {:?}", link.stats());
    assert_eq!(link.is_down(), None);
}

// -- H-A3 -------------------------------------------------------------------

/// **H-A3 — the negative control.** A torn frame with *no* preamble: §5.1 says the next command is
/// eaten and the request times out. Then the preamble, and the same request must answer. That
/// second half is what shows the preamble is doing the work rather than time passing.
///
/// If the negative control does **not** time out, §5.1's claim is wrong for this firmware. That is
/// a finding, not a test failure: it is printed loudly and the preamble stays, because H-A1 has
/// already shown it costs nothing.
#[test]
#[ignore = "needs the NanoKVM-USB dongle"]
fn h_a3_a_torn_frame_without_a_preamble_eats_the_next_command() {
    let mut link = open();
    let start = link.get_info(TIMEOUT).expect("GET_INFO at the start");
    println!("[H-A3] GET_INFO at the start: {start:?}");

    let torn = torn_frame();
    println!("[H-A3] tearing a keyboard frame with NO preamble to follow: {torn:02X?}");
    link.write_raw_for_test(&torn)
        .expect("the torn bytes are written");

    let t = Instant::now();
    let swallowed = link.transact(cmd::GET_INFO, &[], TIMEOUT);
    let rtt = t.elapsed();
    match &swallowed {
        Err(LinkError::Timeout { .. }) => println!(
            "[H-A3] as §5.1 predicts: the torn frame ate the GET_INFO; it timed out after {rtt:?}"
        ),
        other => println!(
            "[H-A3] FINDING: §5.1 says this request should have been eaten, but the device \
             answered in {rtt:?}: {other:?}. The preamble is kept regardless — H-A1 shows it is \
             free — but §5.1's claim does not hold for this firmware as stated."
        ),
    }
    drain_unsolicited(&link, "H-A3");

    // The repair. Two `GET_INFO`s, not one, and the reason is the reader rather than the chip: the
    // timeout above armed `Inner::timed_out` for command `0x01` (§3.1, B6 defect 4), so the first
    // `0x81` frame to arrive afterwards is diverted as *that* request's late reply and this
    // transact times out too. That diversion is itself the evidence — an `0x81` arriving at all
    // means the chip's receive parser is realigned — so the test looks for it either way and then
    // asks again, which must be answered normally.
    //
    // Nothing in the reconnect path meets this: §2.7 builds a **new** `SerialLink`, whose matcher
    // has no armed slot, so its `GET_INFO` is matched on the first attempt.
    let t = Instant::now();
    link.resync().expect("the preamble is written");
    let first = link.get_info(TIMEOUT);
    let first_at = t.elapsed();
    println!("[H-A3] first GET_INFO after the preamble: {first:?} in {first_at:?}");
    let diverted = drain_unsolicited(&link, "H-A3")
        .iter()
        .filter(|r| r.answers(cmd::GET_INFO))
        .count();
    println!(
        "[H-A3] frames answering GET_INFO diverted as late replies to the swallowed request: \
         {diverted}"
    );

    let t = Instant::now();
    let recovered = link.get_info(TIMEOUT);
    println!("[H-A3] second GET_INFO: {recovered:?} in {:?}", t.elapsed());
    let recovered = recovered.expect(
        "GET_INFO must answer once the preamble has completed the torn frame; if neither attempt \
         is answered and nothing was diverted, the preamble did not free the parser",
    );
    assert!(
        first.is_ok() || diverted > 0,
        "the chip answered neither attempt and pushed nothing: the preamble did not realign it"
    );
    assert_eq!(
        locks(&start),
        locks(&recovered),
        "nothing in this test may change the target's lock state"
    );

    link.transact(
        cmd::SEND_KB_GENERAL_DATA,
        &payload("kb_release_all"),
        TIMEOUT,
    )
    .expect("release-all acknowledged");
    drain_unsolicited(&link, "H-A3");
    println!("[H-A3] stats: {:?}", link.stats());
    assert_eq!(link.is_down(), None);
}

// -- H-A4 -------------------------------------------------------------------

/// How the serial node is taken away. `reset` is the default; the orchestrator sets this to
/// whichever method actually makes `/dev/ttyACM1` disappear on this host.
fn replug_method() -> String {
    std::env::var("NANOKVM_REPLUG_METHOD").unwrap_or_else(|_| "reset".to_string())
}

fn replug_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/usb-replug.py")
}

/// Owns the writer and shuts it down — which is a §2.6 release-all — on every path out.
struct ReleasedWriter(Option<WriterHandle>);

impl Drop for ReleasedWriter {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            match handle.shutdown() {
                ReleaseOutcome::Submitted => eprintln!("[H-A4] release-all submitted on shutdown"),
                ReleaseOutcome::Unsent => eprintln!(
                    "[H-A4] the shutdown release-all was UNSENT: the target may still be holding \
                     keys"
                ),
            }
        }
    }
}

/// Poll a condition with a bounded deadline. Returns how long it took, or `None` at the deadline.
fn within<F: FnMut() -> bool>(limit: Duration, mut done: F) -> Option<Duration> {
    let started = Instant::now();
    while started.elapsed() < limit {
        if done() {
            return Some(started.elapsed());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    None
}

/// What the sampling thread saw of one outage.
///
/// `Stats::link_down` is an **edge**, not a level: the writer raises it and the reconnect clears
/// it. Anything that samples it only after the outage is over — which is what polling it after
/// `usb-replug.py` returns amounts to, since the script returns once the node is *back* — reads
/// `false` on a run where every part of the mechanism worked. So the edges are caught by a thread
/// that starts before anything is unplugged, and they carry their own instants.
#[derive(Debug, Default, Clone, Copy)]
struct Edges {
    /// The first sample in which `link_down` was true, and the `reconnects` count then.
    down_at: Option<Instant>,
    reconnects_at_down: u64,
    /// The first sample after that in which it was false again, and the count then.
    up_at: Option<Instant>,
    reconnects_at_up: u64,
    samples: u64,
}

/// How often the sampler reads `Stats`. Two orders of magnitude below the writer's 100 ms health
/// poll, so the down edge cannot be missed between samples, and cheap: `stats()` takes no lock the
/// writer holds across I/O.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(2);

/// Wait for the sampler to have recorded one of its edges. `None` at the deadline.
fn await_edge<F: Fn(Edges) -> Option<Instant>>(
    edges: &Arc<Mutex<Edges>>,
    limit: Duration,
    pick: F,
) -> Option<Instant> {
    let started = Instant::now();
    while started.elapsed() < limit {
        if let Some(at) = pick(*edges.lock().unwrap_or_else(|p| p.into_inner())) {
            return Some(at);
        }
        std::thread::sleep(SAMPLE_INTERVAL);
    }
    None
}

/// Parse one of `usb-replug.py`'s stamped lines — `_stamp` writes `+{N} ms` padded to 11 columns,
/// then the text — and return `N` for the first `<node> gone`.
///
/// That number is relative to the script's own `t0`, which is taken inside `do_operation`, *after*
/// the process has started and resolved its arguments. Added to the instant the child was spawned
/// it is therefore an **early** estimate of when the node really went away, which makes every
/// latency measured from it an upper bound. Labelled as one wherever it is printed.
fn gone_stamp_ms(line: &str) -> Option<u64> {
    let mut fields = line.split_whitespace();
    let ms = fields.next()?.strip_prefix('+')?;
    if fields.next()? != "ms" {
        return None;
    }
    let _node = fields.next()?;
    if fields.next()? != "gone" {
        return None;
    }
    ms.parse().ok()
}

/// The parser above, against the exact shapes `scripts/usb-replug.py::_stamp` emits. Needs no
/// hardware and is not `#[ignore]`d: a silent mis-parse would not fail H-A4, it would quietly
/// degrade its notice bound to the looser "from the spawn" one, which is the kind of rot a
/// measurement never announces.
#[test]
fn the_replug_scripts_gone_stamp_is_parsed_and_nothing_else_is() {
    assert_eq!(gone_stamp_ms("+1234 ms   /dev/ttyACM1 gone"), Some(1234));
    assert_eq!(gone_stamp_ms("+0 ms      /dev/video4 gone"), Some(0));
    assert_eq!(
        gone_stamp_ms("+7 ms      /dev/video4 back (3-2.2.2, devnum 40)"),
        None,
        "an arrival is not a departure"
    );
    assert_eq!(
        gone_stamp_ms("+3 ms      USBDEVFS_RESET /dev/bus/usb/003/034"),
        None
    );
    assert_eq!(gone_stamp_ms("OK: the nodes came back"), None);
    assert_eq!(gone_stamp_ms(""), None);
}

/// Wait for a `0x81` push reporting `want` on CapsLock (A12) — the target's own HID output report,
/// and therefore proof the *target* processed the keystroke rather than proof the chip parsed a
/// frame (§3.4, A17).
fn await_caps_push(rx: &Receiver<Reply>, want: bool, limit: Duration) -> Option<Duration> {
    let started = Instant::now();
    while started.elapsed() < limit {
        let remaining = limit.saturating_sub(started.elapsed());
        match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
            Ok(reply) => {
                if let Some(info) = nanokvm::serial::lock_state_from(&reply) {
                    println!("[H-A4] lock-state push: {info:?}");
                    if info.caps_lock == want {
                        return Some(started.elapsed());
                    }
                }
            }
            Err(_) => continue,
        }
    }
    None
}

/// **H-A4 — end to end on the real dongle.** Build the input path on a `DiscoveringLinkSource`,
/// take the serial node away with `scripts/usb-replug.py`, and watch the writer put it back
/// together: `link_down` true then false, exactly one reconnect, the release recorded
/// `Reconnected` / `Submitted`, device info re-queried — and then the consequence check, a
/// CapsLock tap through the *reconnected* writer that flips the target's lock bit and flips it
/// back.
///
/// **It reopens by identity, not by name** (§8, C6), because the first run of this test proved it
/// has to: the idle writer held `/dev/ttyACM1` open across the replug, so the kernel could not
/// free the tty index and the device came back as `/dev/ttyACM2`. A fixed-path source would have
/// retried a node that no longer existed for ever. The resolved path is printed either side, so a
/// rename is *recorded* rather than fatal.
///
/// **The two numbers it prints are the point of running it**, and how they are taken is most of
/// why this test is shaped the way it is. `ReconnectConfig::default`'s backoff is a guess until
/// this test says how long the writer takes to notice the loss and how long the device is away
/// (see the TODO on `src/input/mod.rs`).
///
/// `Stats::link_down` is an **edge**: the writer raises it and the reconnect clears it. So it is
/// sampled by a thread started *before* the replug, which timestamps both transitions — not
/// polled afterwards, which is what the first version of this test did and which measured nothing
/// at all. `usb-replug.py` returns once the node is *back*, so a poll that starts there can find
/// `link_down` already cleared on a run where every part of the mechanism worked, and the
/// "latency" it prints has its zero seconds after the hang-up rather than at it.
///
/// The notice latency is therefore reported as an **upper bound**, measured from the script's own
/// `+N ms  <node> gone` stamp (read off its piped stdout) added to the instant the child was
/// spawned — an instant that is necessarily no later than the real one, so the true latency is
/// smaller than the number printed. The second number, the outage itself, is the interval between
/// the two sampled edges. The bound is asserted against 5 s: the health poll is 100 ms, so
/// anything near that means the bounded idle wait is not working.
///
/// It never sends a mouse button and it taps CapsLock exactly twice, so the target ends where it
/// began.
#[test]
#[ignore = "needs the NanoKVM-USB dongle and takes its serial node away; ask before running"]
fn h_a4_the_writer_reconnects_after_a_real_replug_and_drives_the_target() {
    let script = replug_script();
    assert!(
        script.is_file(),
        "H-A4 needs {}; it is the orchestrator's to provide",
        script.display()
    );
    let method = replug_method();

    // Identity, not name: `NodeResolver::real` re-runs the whole of §8 on every open attempt, which
    // is what `src/main.rs` does and the only thing that survives the node being renumbered.
    let reporter = NodeResolver::real(NodeKind::Serial, Constraints::default());
    let before_path = reporter
        .resolve()
        .expect("§8 must name the dongle's serial node before the replug")
        .path;
    println!(
        "[H-A4] §8 resolves the serial node to {} now",
        before_path.display()
    );
    if before_path != Path::new(PORT) {
        println!(
            "[H-A4] note: that is not {PORT}, which is what this desk usually calls it \
             (CLAUDE.md). Not a failure — the point of resolving is that the name is not the \
             identity."
        );
    }

    let (source, unsolicited) = DiscoveringLinkSource::new(
        NodeResolver::real(NodeKind::Serial, Constraints::default()),
        OpenOptions::default(),
    );
    let (producer, writer) = spawn_with_source(Box::new(source), Config::default());
    let writer = ReleasedWriter(Some(writer));

    let start = producer
        .wait_for_link(Duration::from_secs(5))
        .expect("the dongle must be there before the replug");
    println!("[H-A4] link up: {start:?}");
    assert!(
        start.target_connected,
        "the target has not enumerated the HID"
    );

    // -- start sampling, *before* anything is unplugged ---------------------
    // This is the whole correction to the first version of this test, which polled `link_down`
    // only after the script returned. `link_down` is an edge; the script returns once the node is
    // back; so that poll could find nothing on a run where the mechanism worked perfectly, and
    // the "notice latency" it printed was measured from a clock started seconds after the
    // hang-up. The sampler below runs across the whole outage and timestamps both transitions.
    let edges: Arc<Mutex<Edges>> = Arc::new(Mutex::new(Edges::default()));
    let sampler_stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let p = producer.clone();
        let recorded = Arc::clone(&edges);
        let stop = Arc::clone(&sampler_stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let s = p.stats();
                let now = Instant::now();
                let mut e = recorded.lock().unwrap_or_else(|p| p.into_inner());
                e.samples += 1;
                if e.down_at.is_none() {
                    if s.link_down {
                        e.down_at = Some(now);
                        e.reconnects_at_down = s.reconnects;
                    }
                } else if e.up_at.is_none() && !s.link_down {
                    e.up_at = Some(now);
                    e.reconnects_at_up = s.reconnects;
                }
                drop(e);
                std::thread::sleep(SAMPLE_INTERVAL);
            }
        })
    };

    // -- take it away ------------------------------------------------------
    // `--serial` carries the node §8 just resolved. Without it the script falls back to its
    // `/dev/ttyACM1` default — the one name this test is written not to trust, and the one that
    // moved to `/dev/ttyACM2` on the run that produced this slice. It is not a safety hazard
    // either way (`resolve()` refuses anything outside the allowlist), but a test that fails
    // because it handed the script a stale name has failed for the wrong reason, in exactly the
    // scenario it exists to survive. H-B2 passes `--video` for the same reason.
    //
    // Through a `ChildGuard` and a pipe: the guard so an assertion that unwinds past the `wait()`
    // cannot leave a USB-resetting script running unsupervised, and the pipe so the script's own
    // timeline — including the `+N ms  <node> gone` stamp the notice bound is measured from — is
    // read here as well as printed.
    println!(
        "[H-A4] running usb-replug.py serial --method {method} --serial {}",
        before_path.display()
    );
    let spawned = Instant::now();
    let mut replug = child_guard::ChildGuard::spawn(
        "scripts/usb-replug.py",
        Command::new("python3")
            .arg("scripts/usb-replug.py")
            .arg("serial")
            .arg("--method")
            .arg(&method)
            .arg("--serial")
            .arg(&before_path)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stdout(Stdio::piped()),
    )
    .expect("spawning scripts/usb-replug.py");

    let stdout = replug
        .take_stdout()
        .expect("usb-replug.py was spawned with a piped stdout");
    let mut gone_ms: Option<u64> = None;
    for line in BufReader::new(stdout).lines() {
        let line = line.expect("reading usb-replug.py's stdout");
        println!("[H-A4]   replug| {line}");
        if gone_ms.is_none() {
            gone_ms = gone_stamp_ms(&line);
        }
    }
    let status = replug.wait().expect("waiting for scripts/usb-replug.py");
    println!(
        "[H-A4] usb-replug.py exited {status} after {:?}",
        spawned.elapsed()
    );
    assert!(status.success(), "the replug script failed: {status}");

    // -- what the sampler saw ----------------------------------------------
    // The queue is empty and stays empty: nothing below submits anything until after the
    // reconnect, so this window measures the *idle* path — the reader thread's verdict reaching
    // the writer through `Link::is_down` on its bounded idle wait. Both edges are almost
    // certainly already recorded by the time the script returns; the waits are the failsafe.
    let down_at = await_edge(&edges, Duration::from_secs(5), |e| e.down_at).unwrap_or_else(|| {
        let stats = producer.stats();
        panic!(
            "the writer never reported the node gone, with nothing queued and a sampler running \
             across the whole outage. The idle health check is not reaching the writer: {stats:?}"
        )
    });
    let up_at = await_edge(&edges, Duration::from_secs(60), |e| e.up_at)
        .expect("the writer must reconnect once the node is back");
    // The reconnect is credited just after `link_down` clears, so the monotone counter is waited
    // on separately rather than read off the up edge.
    within(Duration::from_secs(5), || producer.stats().reconnects >= 1)
        .expect("the reconnect must be credited once the link is back in service");
    sampler_stop.store(true, Ordering::SeqCst);
    sampler.join().expect("the sampling thread must not panic");
    let seen = *edges.lock().unwrap_or_else(|p| p.into_inner());
    println!(
        "[H-A4] sampler: {} samples; reconnects was {} at the down edge and {} at the up edge",
        seen.samples, seen.reconnects_at_down, seen.reconnects_at_up
    );

    // The two numbers `ReconnectConfig::default` is waiting on. Printed together, and labelled,
    // so they can be read off one run: do not change the defaults from anything else.
    //
    // The notice latency is an **upper bound**. Its zero is the script's own "gone" stamp added
    // to the instant the child was spawned, and the script's `t0` is later than that spawn by
    // however long python took to start and resolve its arguments — so the true latency is
    // smaller than what is printed, never larger.
    let (zero, from) = match gone_ms {
        Some(ms) => (
            spawned + Duration::from_millis(ms),
            format!("the script's own \"+{ms} ms … gone\" stamp, offset from its spawn"),
        ),
        None => (
            spawned,
            "the moment the script was spawned — no \"gone\" line was parsed, so this bound is \
             looser by the script's whole start-up"
                .to_string(),
        ),
    };
    let notice_bound = down_at.saturating_duration_since(zero);
    let down_for = up_at.saturating_duration_since(down_at);
    println!(
        "[H-A4] FOR ReconnectConfig::default — notice latency <= {notice_bound:?} (upper bound, \
         from {from}), down for {down_for:?} (link_down true -> false, i.e. the whole outage as \
         the writer experienced it)"
    );
    assert!(
        notice_bound < Duration::from_secs(5),
        "the writer took more than 5 s to notice an idle link had gone (bound {notice_bound:?} \
         from {from}); the health poll is 100 ms, so this is the bound not working"
    );
    let after_path = reporter
        .resolve()
        .expect("§8 must name the serial node again once the device is back")
        .path;
    println!(
        "[H-A4] §8 resolves the serial node to {} after the replug (was {})",
        after_path.display(),
        before_path.display()
    );
    if after_path != before_path {
        println!(
            "[H-A4] the node was renumbered by the replug. That is the C6 case this test exists \
             to survive, and it survived it."
        );
    }

    let stats = producer.stats();
    assert_eq!(stats.reconnects, 1, "exactly one reconnect: {stats:?}");
    assert!(
        stats.reconnect_attempts >= 2,
        "the initial open plus at least one reconnect attempt: {stats:?}"
    );
    let release = stats.last_release.expect("the sequence records a release");
    assert_eq!(release.reason, ReleaseReason::Reconnected);
    assert_eq!(release.outcome, ReleaseOutcome::Submitted);
    let info = stats.device_info.expect("device info was re-queried");
    println!("[H-A4] device info after the reconnect: {info:?}");
    assert!(stats.down_since.is_none());
    println!("[H-A4] stats after the reconnect: {stats:?}");

    // -- the consequence check --------------------------------------------
    // Anything the device pushed while the link was coming back is stale for this purpose.
    while unsolicited.try_recv().is_ok() {}

    producer.engage().expect("deliberate recapture after §2.8");
    let started_caps = info.caps_lock;
    for (round, want) in [(1, !started_caps), (2, started_caps)] {
        for down in [true, false] {
            producer
                .submit(Event::Key {
                    key: HidKey::Usage(CAPS_LOCK),
                    down,
                })
                .unwrap_or_else(|e| panic!("CapsLock {down} in round {round}: {e}"));
        }
        let at = await_caps_push(&unsolicited, want, Duration::from_secs(2)).unwrap_or_else(|| {
            panic!(
                "round {round}: the target never reported caps={want}; the reconnected writer is \
                 not driving it"
            )
        });
        println!("[H-A4] round {round}: target reported caps={want} after {at:?}");
    }
    println!("[H-A4] CapsLock ended where it began ({started_caps})");

    let stats = producer.stats();
    println!("[H-A4] final stats: {stats:?}");
    assert!(!stats.link_down);
    assert_eq!(stats.reconnects, 1, "no further reconnects happened");
    drop(writer);
}
