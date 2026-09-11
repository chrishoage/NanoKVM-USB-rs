//! A scripted CH9329 on a pty, so the real [`SerialLink`](super::SerialLink) can be exercised
//! unmodified with no hardware (§9.3: "a fake CH9329 over `openpty` exercises the real serial
//! module unmodified").
//!
//! It lives in the library rather than behind `cfg(test)` because the integration tests in
//! `tests/` need it, and a `cfg(test)` module is not visible to them. It is `#[doc(hidden)]` and
//! is not part of the crate's supported surface.
//!
//! # What it reproduces
//!
//! The default behaviour is the device as measured, not the device as documented — including the
//! parts of it that are wrong:
//!
//! - `GET_INFO` → the fixture `get_info_reply` payload (Appendix);
//! - keyboard and mouse commands → the one-byte `0x00` acknowledgement with `cmd | 0x80`
//!   (fixtures `kb_ack`, `mouse_abs_ack`, `mouse_rel_ack`). **It acknowledges anything of the
//!   right shape**, which is exactly the trap in §3.4/A17: the ack proves parsing, not effect;
//! - a frame whose checksum does not verify → the error frame `cmd | 0xC0` with `0xE4`
//!   (fixture `err_checksum_on_get_info`, §3.1/A14);
//! - `GET_USB_STRING` with an empty payload → `0xCA` with `0xE5` (fixture
//!   `err_param_on_get_usb_string`);
//! - **any other command → the five-byte reply `57 AB 00 (cmd|0x80) 00` with no checksum byte**
//!   (§3.2, A13, fixture disagreement `unknown_command_reply_has_no_checksum_byte`). This is the
//!   one that hangs a naive host reader for ever, so the fake must be able to produce it.
//!
//! # What it does not reproduce
//!
//! Pacing. The device's acknowledged round trip is 4.15 ms for a keyboard report and 17.0 ms for
//! a mouse report (A11), and overload corrupts silently rather than blocking (§5.1). The fake
//! answers immediately unless told to delay, so it cannot be used to justify a rate.
//!
//! The fake never panics, on any input. A panic in its thread would surface in an unrelated test
//! as a mysterious timeout.

use std::io;
use std::os::unix::io::RawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::proto::cmd;
use crate::proto::frame::{encode, Event, Frame, Parser, ADDR, HEAD};

/// The `get_info_reply` payload (Appendix, fixture `get_info_reply`): version 1.8, target
/// connected, all lock bits off. Tests that assert on byte-level correctness should load the
/// fixture file instead of relying on this constant; it exists so `Behaviour::default` has an
/// answer without doing I/O.
pub const DEFAULT_GET_INFO_PAYLOAD: [u8; 8] = [0x38, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];

/// Checksum-error code, as the device sends it (fixture `err_checksum_on_get_info`).
pub const ERR_CHECKSUM: u8 = 0xE4;
/// Parameter-error code (fixture `err_param_on_get_usb_string`).
pub const ERR_PARAM: u8 = 0xE5;

/// How long the fake's thread blocks in `poll` before re-checking its knobs and flags. It is the
/// latency of `inject_unsolicited`, `hang_up` and `stop`; replies themselves are not delayed by
/// it, because `poll` returns as soon as a request arrives.
const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// Gap between bytes when `fragment_replies` is on. Long enough that the host's reader really does
/// see the reply arrive in pieces across several `read` calls.
const FRAGMENT_GAP: Duration = Duration::from_millis(1);

/// The fake's initial script. Every field is also settable while it runs; see the setters on
/// [`FakeCh9329`].
#[derive(Debug, Clone)]
pub struct Behaviour {
    /// Payload of the `GET_INFO` reply (Appendix).
    pub get_info_payload: Vec<u8>,
    /// Commands answered with an error frame `cmd | 0xC0` carrying this code, instead of the
    /// normal reply (§3.1).
    pub reject: Vec<(u8, u8)>,
    /// Write replies one byte at a time with a gap, so the host must reassemble them (§9.2 item 6).
    pub fragment_replies: bool,
    /// Wait this long after receiving a request before answering it.
    pub delay_reply: Duration,
    /// Bytes emitted immediately before every reply, which the host must discard as garbage
    /// (§9.2 item 6).
    pub prefix_garbage: Vec<u8>,
}

impl Default for Behaviour {
    fn default() -> Behaviour {
        Behaviour {
            get_info_payload: DEFAULT_GET_INFO_PAYLOAD.to_vec(),
            reject: Vec::new(),
            fragment_replies: false,
            delay_reply: Duration::ZERO,
            prefix_garbage: Vec::new(),
        }
    }
}

/// The knobs, as the thread reads them. Latched when a request arrives, so a test may change them
/// again the moment it observes the request without racing the reply.
#[derive(Debug)]
struct Knobs {
    script: Behaviour,
    /// One-shot: bytes written between receiving the next request and answering it. This is the
    /// A12 case — an unsolicited `0x81` landing inside a request/reply pair.
    inject_before_reply: Option<Vec<u8>>,
    /// Bytes to write at the next opportunity, unprompted.
    inject_now: Vec<u8>,
    /// One-shot: swallow the next reply entirely.
    drop_next_reply: bool,
}

/// What the fake received, for tests to assert on.
#[derive(Debug, Default)]
struct Record {
    frames: Vec<Frame>,
    raw: Vec<u8>,
    chunks: Vec<Vec<u8>>,
}

#[derive(Debug)]
struct FakeState {
    knobs: Mutex<Knobs>,
    record: Mutex<Record>,
    stop: AtomicBool,
    hang_up: AtomicBool,
}

impl FakeState {
    /// Lock policy: same as the rest of the module — recover a poisoned guard rather than
    /// propagate the panic. Here it also matters that the fake never panics *itself*, so a poisoned
    /// lock can only come from a test thread that already failed.
    fn knobs(&self) -> MutexGuard<'_, Knobs> {
        self.knobs.lock().unwrap_or_else(|p| p.into_inner())
    }
    fn record(&self) -> MutexGuard<'_, Record> {
        self.record.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// A fake CH9329 listening on a pty. Drop stops it.
#[derive(Debug)]
pub struct FakeCh9329 {
    state: Arc<FakeState>,
    slave_path: PathBuf,
    /// Our own handle on the slave end, held open for the life of the fake.
    ///
    /// Without it the master would see a hang-up the instant the host closes `/dev/pts/N` — or,
    /// worse, in the window before the host has opened it at all. Holding it means the only
    /// hang-up the host ever sees is the one [`FakeCh9329::hang_up`] asks for.
    slave_fd: RawFd,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl FakeCh9329 {
    /// Open a pty pair, put the slave in raw mode and start the fake on the master side.
    pub fn spawn(script: Behaviour) -> io::Result<FakeCh9329> {
        let (master, slave) = open_pty()?;
        if let Err(e) = make_raw(slave) {
            close(master);
            close(slave);
            return Err(e);
        }
        let slave_path = match ptsname(master) {
            Ok(p) => p,
            Err(e) => {
                close(master);
                close(slave);
                return Err(e);
            }
        };
        if let Err(e) = set_nonblocking(master) {
            close(master);
            close(slave);
            return Err(e);
        }

        let state = Arc::new(FakeState {
            knobs: Mutex::new(Knobs {
                script,
                inject_before_reply: None,
                inject_now: Vec::new(),
                drop_next_reply: false,
            }),
            record: Mutex::new(Record::default()),
            stop: AtomicBool::new(false),
            hang_up: AtomicBool::new(false),
        });

        let thread_state = Arc::clone(&state);
        let thread = std::thread::Builder::new()
            .name("nanokvm-fake-ch9329".to_string())
            .spawn(move || run(thread_state, master))
            .inspect_err(|_| close(slave))?;

        Ok(FakeCh9329 {
            state,
            slave_path,
            slave_fd: slave,
            thread: Mutex::new(Some(thread)),
        })
    }

    /// The `/dev/pts/N` path to hand to [`SerialLink::open`](super::SerialLink::open).
    pub fn slave_path(&self) -> PathBuf {
        self.slave_path.clone()
    }

    /// Write each reply byte one at a time with a gap, forcing the host to reassemble.
    pub fn fragment_replies(&self, on: bool) {
        self.state.knobs().script.fragment_replies = on;
    }

    /// Wait this long between receiving a request and answering it.
    ///
    /// Latched when the request is parsed, which happens **before** the request appears in
    /// [`FakeCh9329::received`]. So a test that has observed a request may change this knob
    /// immediately without racing that request's reply — it will apply to the next one.
    pub fn delay_reply(&self, delay: Duration) {
        self.state.knobs().script.delay_reply = delay;
    }

    /// Answer subsequent `GET_INFO` requests with this payload (Appendix).
    ///
    /// Latched when a request is parsed, like every other knob, so a test that has observed one
    /// request may change the answer to the next without racing this one. Two `GET_INFO` replies
    /// that differ in their payload are the only way a test can tell a late reply from a fresh
    /// one: on the wire the two frames are otherwise identical, which is the whole difficulty the
    /// resynchronisation window exists to handle (§3.1).
    pub fn set_get_info_payload(&self, payload: &[u8]) {
        self.state.knobs().script.get_info_payload = payload.to_vec();
    }

    /// Emit these bytes immediately before every reply. They are not a frame; the host must
    /// discard them and still find the reply (§9.2 item 6).
    pub fn prefix_garbage(&self, bytes: &[u8]) {
        self.state.knobs().script.prefix_garbage = bytes.to_vec();
    }

    /// Answer `cmd` with the error frame `cmd | 0xC0` carrying `code`, until told otherwise.
    pub fn reject_cmd(&self, cmd: u8, code: u8) {
        let mut knobs = self.state.knobs();
        knobs.script.reject.retain(|(c, _)| *c != cmd);
        knobs.script.reject.push((cmd, code));
    }

    /// Stop rejecting `cmd`.
    pub fn accept_cmd(&self, cmd: u8) {
        self.state.knobs().script.reject.retain(|(c, _)| *c != cmd);
    }

    /// Push raw bytes at the host with nothing having asked for them. Written within
    /// [`POLL_INTERVAL`].
    pub fn inject_unsolicited(&self, bytes: &[u8]) {
        self.state.knobs().inject_now.extend_from_slice(bytes);
    }

    /// Push raw bytes at the host **between receiving the next request and answering it**.
    ///
    /// This is the A12 ordering the reader has to survive: a `0x81` lock-state frame arriving
    /// inside a request/reply pair. Unlike [`FakeCh9329::inject_unsolicited`] it is exact rather
    /// than merely soon, so a test built on it cannot flake.
    pub fn inject_before_reply(&self, bytes: &[u8]) {
        self.state.knobs().inject_before_reply = Some(bytes.to_vec());
    }

    /// Receive the next request, record it, and say nothing at all.
    pub fn drop_next_reply(&self) {
        self.state.knobs().drop_next_reply = true;
    }

    /// Close the master side, so the host's slave fd sees a hang-up — `POLLHUP`, and then a
    /// `read` that returns **0**, not `EIO`. Closing a pty master vhangups the slave exactly as
    /// `acm_disconnect` vhangups the dongle's tty, and a hung-up tty answers `read` with EOF
    /// (`hung_up_tty_read`); measured on this kernel, `poll` returns `POLLIN|POLLERR|POLLHUP` and
    /// the following `read(2)` returns 0. So this is a faithful unplug and not an approximation of
    /// one — which matters, because it is what lets the idle-loss case be reproduced without
    /// hardware. (`serialport` checks `revents` before it reads, so what the *host* sees through
    /// it is `BrokenPipe` either way.)
    ///
    /// Takes effect within [`POLL_INTERVAL`]; the fake's thread does the closing, so no fd is
    /// closed under a blocked read.
    pub fn hang_up(&self) {
        self.state.hang_up.store(true, Ordering::SeqCst);
    }

    /// Every well-formed frame received, in order.
    pub fn received(&self) -> Vec<Frame> {
        self.state.record().frames.clone()
    }

    /// Every byte received, concatenated in arrival order.
    ///
    /// A test asserts "the request arrived as one contiguous well-formed frame" against this: the
    /// stream must equal the expected frames end to end, with nothing between them and nothing
    /// left over. That is the property §5.1/A15 actually requires — the chip has no inter-byte
    /// timeout, so a stray or missing byte anywhere in the stream corrupts the next command —
    /// and unlike "arrived in one `read`" it does not depend on scheduling.
    pub fn raw_rx(&self) -> Vec<u8> {
        self.state.record().raw.clone()
    }

    /// The same bytes, split by `read` call. Diagnostic only: how a write lands in the pty buffer
    /// is not a property of the host, so no test should assert on it.
    pub fn read_chunks(&self) -> Vec<Vec<u8>> {
        self.state.record().chunks.clone()
    }

    /// Block until at least `n` frames have been received, or `timeout` elapses. Returns whether
    /// the count was reached. The synchronisation primitive for tests that must act *after* a
    /// request lands — polling this beats sleeping and guessing.
    pub fn wait_for_received(&self, n: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.state.record().frames.len() >= n {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Stop the fake's thread and close the master. Idempotent; [`Drop`] calls it.
    pub fn stop(&self) {
        self.state.stop.store(true, Ordering::SeqCst);
        let handle = self.thread.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

impl Drop for FakeCh9329 {
    fn drop(&mut self) {
        self.stop();
        close(self.slave_fd);
    }
}

// -- the thread -------------------------------------------------------------

fn run(state: Arc<FakeState>, master: RawFd) {
    let mut parser = Parser::new();
    let mut buf = [0u8; 512];

    while !state.stop.load(Ordering::SeqCst) {
        if state.hang_up.load(Ordering::SeqCst) {
            break;
        }

        let queued = std::mem::take(&mut state.knobs().inject_now);
        if !queued.is_empty() {
            write_all(master, &queued);
        }

        match poll_readable(master, POLL_INTERVAL) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(_) => break,
        }
        let n = match read(master, &mut buf) {
            Ok(0) => continue,
            Ok(n) => n,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(_) => break,
        };

        {
            let mut record = state.record();
            record.raw.extend_from_slice(&buf[..n]);
            record.chunks.push(buf[..n].to_vec());
        }
        parser.push(&buf[..n]);
        while let Some(event) = parser.next_event() {
            answer(&state, master, event);
        }
    }

    // The fake owns the master for its whole life, so this is the only close: no fd is ever
    // closed while another thread might be blocked reading it.
    close(master);
}

/// What the fake will say in response to one request, decided in a single pass under the knobs
/// lock so that no knob can change halfway through a reply.
#[derive(Debug)]
struct Plan {
    /// Bytes to write *before* the reply — the A12 case, an unsolicited frame inside a
    /// request/reply pair.
    inject: Option<Vec<u8>>,
    /// The reply itself, or `None` for `drop_next_reply`.
    bytes: Option<Vec<u8>>,
    delay: Duration,
    fragment: bool,
    prefix: Vec<u8>,
}

/// Act on one parsed event exactly as the device does.
fn answer(state: &FakeState, master: RawFd, event: Event) {
    let request = match event {
        Event::Frame(frame) => frame,
        // A candidate whose checksum did not verify. The device answers `cmd | 0xC0` with 0xE4 and
        // then carries on normally (§3.1, Appendix). `raw[3]` is the command byte it blames.
        Event::BadChecksum { ref raw } => {
            if let Some(&cmd) = raw.get(3) {
                write_all(master, &error_frame(cmd, ERR_CHECKSUM));
            }
            return;
        }
        // The host tore a write, or sent noise. The real chip would silently keep counting (A15),
        // so the fake says nothing either.
        Event::Truncated { .. } | Event::Garbage { .. } => return,
    };

    // Latch the whole plan *before* the frame becomes visible to `received`. That ordering is what
    // makes `wait_for_received` a real synchronisation point: once a test can see the request, the
    // knobs that answered it are already read, so changing one cannot race this reply.
    let plan = script_reply(state, &request);
    state.record().frames.push(request);
    write_reply(master, plan);
}

/// Decide the reply for one request and latch every knob it depends on.
fn script_reply(state: &FakeState, request: &Frame) -> Plan {
    let mut knobs = state.knobs();
    let inject = knobs.inject_before_reply.take();
    let delay = knobs.script.delay_reply;
    let fragment = knobs.script.fragment_replies;
    let prefix = knobs.script.prefix_garbage.clone();
    let plan = |bytes| Plan {
        inject: inject.clone(),
        bytes,
        delay,
        fragment,
        prefix: prefix.clone(),
    };

    if std::mem::take(&mut knobs.drop_next_reply) {
        return plan(None);
    }
    let rejected = knobs
        .script
        .reject
        .iter()
        .find(|(c, _)| *c == request.cmd)
        .map(|(_, code)| *code);
    if let Some(code) = rejected {
        return plan(Some(error_frame(request.cmd, code)));
    }
    let bytes = match request.cmd {
        cmd::GET_INFO => frame_bytes(cmd::GET_INFO | 0x80, &knobs.script.get_info_payload),
        cmd::SEND_KB_GENERAL_DATA
        | cmd::SEND_KB_MEDIA_DATA
        | cmd::SEND_MS_ABS_DATA
        | cmd::SEND_MS_REL_DATA
        | cmd::SEND_MY_HID_DATA => frame_bytes(request.cmd | 0x80, &[0x00]),
        // Observed: an empty payload is a parameter error, not a string (fixture
        // `err_param_on_get_usb_string`). The non-empty form returns strings the fake does not
        // model (A16), so it acknowledges instead.
        cmd::GET_USB_STRING if request.data.is_empty() => error_frame(request.cmd, ERR_PARAM),
        cmd::GET_USB_STRING => frame_bytes(request.cmd | 0x80, &[0x00]),
        // §3.2, A13: five bytes, no checksum byte, for ever.
        other => vec![HEAD[0], HEAD[1], ADDR, other | 0x80, 0x00],
    };
    plan(Some(bytes))
}

/// Emit the injected bytes, then wait out `delay_reply`, then the reply itself.
fn write_reply(master: RawFd, plan: Plan) {
    let Plan {
        inject,
        bytes,
        delay,
        fragment,
        prefix,
    } = plan;
    if let Some(inject) = inject {
        write_all(master, &inject);
    }
    if delay > Duration::ZERO {
        std::thread::sleep(delay);
    }
    let Some(bytes) = bytes else { return };
    let mut out = prefix;
    out.extend_from_slice(&bytes);
    if fragment {
        for byte in out {
            write_all(master, &[byte]);
            std::thread::sleep(FRAGMENT_GAP);
        }
    } else {
        write_all(master, &out);
    }
}

/// A well-formed frame, or an empty vector if the payload could not be framed. Never panics:
/// `encode` only rejects payloads over 255 bytes, which no reply here reaches, and a fake that
/// panicked would surface in an unrelated test as a mysterious timeout.
fn frame_bytes(cmd: u8, payload: &[u8]) -> Vec<u8> {
    encode(cmd, payload).unwrap_or_default()
}

/// `cmd | 0xC0` with a one-byte code (Appendix, §3.1).
fn error_frame(cmd: u8, code: u8) -> Vec<u8> {
    frame_bytes(cmd | 0xC0, &[code])
}

// -- libc glue --------------------------------------------------------------
//
// `nix` is a dev-dependency, and this module is compiled into the library, so the pty calls go
// through `libc` directly. Each block below is a single syscall with its arguments checked
// immediately above it.

/// `openpty`, returning `(master, slave)`.
fn open_pty() -> io::Result<(RawFd, RawFd)> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    // SAFETY: `openpty` writes one `c_int` through each of the first two pointers, and both point
    // at live locals. The remaining three arguments are documented as optional and null means
    // "kernel defaults"; passing null also avoids `openpty`'s unbounded `name` buffer, which is
    // why the slave path is fetched with `ptsname_r` afterwards instead.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((master, slave))
}

/// Put `fd` in raw mode: no echo, no CR/LF translation, no signal characters.
///
/// Both directions of a pty pass through the *slave's* line discipline, so this one call is what
/// keeps a `0x0A` inside a frame from becoming `0x0D 0x0A` and a `0x03` from raising SIGINT.
fn make_raw(fd: RawFd) -> io::Result<()> {
    // SAFETY: `termios` is a plain C struct of integers and arrays with no padding invariants, so
    // an all-zero value is a valid one to hand to `tcgetattr`, which overwrites it wholesale.
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is an open terminal (it came from `openpty`) and `termios` is a live local of
    // exactly the type the call expects.
    if unsafe { libc::tcgetattr(fd, &mut termios) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `cfmakeraw` only clears and sets flag bits within the struct it is given.
    unsafe { libc::cfmakeraw(&mut termios) };
    // SAFETY: same fd, and `termios` was initialised by `tcgetattr` above.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &termios) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The `/dev/pts/N` path of the slave belonging to `master`.
fn ptsname(master: RawFd) -> io::Result<PathBuf> {
    let mut buf = [0 as libc::c_char; 128];
    // SAFETY: `ptsname_r` writes at most `buf.len()` bytes, including the terminator, into `buf`,
    // which is a live local of exactly that length. The `_r` form is used precisely because
    // `ptsname` returns a shared static buffer that is not safe across threads.
    let rc = unsafe { libc::ptsname_r(master, buf.as_mut_ptr(), buf.len()) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    // SAFETY: `ptsname_r` returned success, so `buf` holds a NUL-terminated string within bounds.
    let name = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
    Ok(PathBuf::from(name.to_str().map_err(|e| {
        io::Error::new(io::ErrorKind::InvalidData, e)
    })?))
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: `F_GETFL` takes no third argument and only reads the descriptor's flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `F_SETFL` takes an `int` third argument, which is what is passed.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Wait for `fd` to become readable. `Ok(false)` is a timeout.
fn poll_readable(fd: RawFd, timeout: Duration) -> io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    // SAFETY: `&mut pfd` is a live array of exactly the one `pollfd` the count claims.
    let rc = unsafe { libc::poll(&mut pfd, 1, millis) };
    if rc < 0 {
        let e = io::Error::last_os_error();
        // A signal is not a failure; the caller loops.
        if e.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(e);
    }
    if rc == 0 {
        return Ok(false);
    }
    if pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
        && pfd.revents & libc::POLLIN == 0
    {
        return Err(io::Error::from(io::ErrorKind::BrokenPipe));
    }
    Ok(true)
}

fn read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: the pointer and length come from the same live slice, so the kernel writes only
    // within it.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

/// Write every byte, or give up quietly. Nothing here can usefully handle a write failure: the
/// host is about to time out and say so, which is a better diagnostic than anything the fake could
/// produce.
fn write_all(fd: RawFd, bytes: &[u8]) {
    let mut written = 0;
    let deadline = Instant::now() + Duration::from_secs(1);
    while written < bytes.len() {
        // SAFETY: the pointer is inside `bytes` and the length is the remainder of the same live
        // slice, so the kernel reads only within it.
        let n = unsafe { libc::write(fd, bytes[written..].as_ptr().cast(), bytes.len() - written) };
        if n > 0 {
            written += n as usize;
            continue;
        }
        let e = io::Error::last_os_error();
        match e.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_micros(200));
            }
            _ => return,
        }
    }
}

fn close(fd: RawFd) {
    // SAFETY: every fd closed here was opened by `open_pty` in this module and is closed exactly
    // once — the master by the fake's own thread as it returns, the slave by `Drop`.
    unsafe { libc::close(fd) };
}
