# Architecture

The project is one Rust package with a library and the `nanokvm` binary. The binary
selects devices and either starts the viewer or runs a command. Protocol encoding,
script compilation, and input state can be tested without physical devices.

## Ownership and interfaces

| Module | Responsibility | Boundary |
| --- | --- | --- |
| `proto` | CH9329 frames, replies, HID reports, physical-key mapping | Bytes and values; no I/O. |
| `link` | Transactions and transport health | Shared interface for the viewer writer and CLI. |
| `input` | Admission, motion coalescing, held state, cancellation, reconnect | One writer owns a `Link`; producers submit events. |
| `serial` | Port I/O, incremental reads, reply matching | Implements `Link`; provides fake CH9329 devices. |
| `capture` | V4L2 acquisition, JPEG validation and decode, recovery | `FrameSource`, `SourceOpener`, and one-frame handoffs. |
| `discovery` | Pairing from sysfs and candidate video capabilities | Injectable sysfs/probe access; reopen adapters join it to I/O. |
| `audio` | Capture/playback workers and drift correction | PCM source/sink traits; ALSA confined to `alsa.rs`. |
| `script` | Chord, text, and macro compilation | Fully resolved keyboard reports or complete diagnostics. |
| `cli` | Command arguments, delivery, files, signals, output | Opens the resources needed by each command. |
| `viewer` | Event loop, capture state, input mapping, presentation | UI commands and snapshots coordinate the workers. |

The viewer event loop must remain responsive while serial transactions, decoding,
or compositor presentation block. Its workers have separate responsibilities:

- The input writer serializes reports and owns held-key state. A serial reader
  parses replies and unsolicited notifications.
- Capture copies compressed bytes out of mmap buffers. Decode validates and produces
  RGBA. Each handoff keeps at most one pending frame, replacing older work.
- Render owns GPU presentation. It can block in a compositor call without blocking
  input release on the event loop.
- Audio capture and playback exchange fixed-size periods through a bounded ring.
  Each side can fail or reopen independently.
- Clipboard reads and potentially blocking device opens use helper threads with
  bounded waits from their callers.

## Input ordering and release

Keyboard and button transitions form queue barriers. Adjacent absolute motion keeps
the newest position; relative motion accumulates deltas. Coalescing never moves a
transition across the motion around it. Large deltas split into report-sized pieces;
a run is separately bounded so stalled delivery cannot accumulate unlimited work.

The writer owns both the physical held set and the six-key HID projection. Additional
non-modifier keys are suppressed until released, rather than promoted later into
presses the user did not just make. Modifier bits do not consume the six usage slots.

Cancellation uses an epoch and a separate flag, so it remains schedulable when the
queue is full. A trigger closes admission and advances the requested epoch. Between
reports, the writer latches that epoch, discards its queued work, releases held input,
and advances acknowledgement. A newer trigger keeps cancellation armed for another
pass. New-session events use the acknowledged epoch; they are not stale merely
because their epoch equals it.

Local acknowledgement and release delivery are separate. An unavailable transport
still advances the epoch but records `Unsent`, allowing the UI to report that keys
may remain held. Shutdown closes admission before requesting its final release, so
another producer cannot recapture and send input after that release.

The bridge exposes relative and absolute mice separately. Releasing an absolute
session therefore includes an absolute button release at the last position as well
as the relative mouse release. Subsequent sessions discard that old position.

## Serial recovery

The reader matches replies by command byte: unsolicited lock-state frames can arrive
between a request and its reply. After a timeout, a quiet window and late-reply slot
prevent an old reply from satisfying a newer request of the same command. Those
replies have no transaction IDs, so their ambiguity cannot be resolved from bytes alone.

The writer checks idle transport health without sending keepalive reports. Both an
idle hang-up and a failed transaction enter the same cancellation path. Reconnect:

1. Drops the old descriptor and discards stale input.
2. Opens a replacement, resolving discovered node paths again.
3. Writes the zero preamble needed to finish an incomplete bridge frame.
4. Releases keyboard and mouse state, then queries device information.
5. Accepts the link only if commissioning succeeds; leaves input disengaged.

Retry delays start at 250 ms and double to 4 seconds. Cancellation and shutdown
remain serviceable during waits. Explicit paths stay fixed. Dropping descriptors
before reopening matters because a held descriptor can reserve the old device index.

CLI keyboard commands use the same transport interface but send reports sequentially
without the viewer queue. They fail with partial progress instead of reconnecting:
replaying an unattended sequence could repeat a command. Viewer shortcuts and paste
use the existing producer because opening a second serial writer would interleave frames.

## Capture and presentation

The default stream is MJPEG, 1920 × 1080 at 60 fps. `S_PARM` sets the interval
explicitly. Each JPEG header supplies actual dimensions, bounded to 4096 × 2304
before allocation. Strict decoding rejects incomplete frames. Capture timestamps
survive decoding so age is measured from capture rather than from a later handoff.

The v4l stream requeues its last buffer before dequeueing the next one. A failed
dequeue leaves that index stale; repeatedly calling it can then fail indefinitely.
The adapter polls the descriptor itself and rebuilds the mmap stream after dequeue
errors. It copies only `bytesused` bytes before letting the buffer be reused.

Recovery distinguishes three conditions:

| Condition | Response |
| --- | --- |
| No frame from a present source | Restart streaming after 2 seconds; continue polling. |
| Disconnected source | Drop it and reopen with backoff from 500 ms to 5 seconds. |
| Persistent header-size mismatch | Require both 12 consecutive mismatches and a 500 ms grace period; attempt up to three restarts, one reopen with a fresh restart budget, then accept and report the delivered size. |

The size watchdog has its own counters and backoff. A matching frame resets its
mismatch streak; stall recovery does not consume its format-recovery budget.
Sources without negotiated dimensions disable that watchdog. A UI format request
updates the opener and reuses the reopen path because `S_FMT` cannot run on a streaming
node. The single-source `Pipeline::start` API has no replacement source and cannot
recover a disconnection; the viewer uses `start_with_opener`.

The last decoded frame remains available through capture failures. Video failure
after startup does not stop keyboard control. HDMI signal loss is not inferred from
frame pixels or arrival: the measured dongle streams a placeholder through target reboot.

Render chooses from supported surface presentation modes, preferring Fifo and otherwise the first offered mode. The measured niri surface offered Mailbox and Fifo.
The render thread draws the video before the UI and retains every pending texture
delta even when intermediate geometry is replaced. Losing an atlas update would
leave later geometry referencing missing textures.

Shutdown bounds the render join. GPU objects are intentionally retained until process
exit: a detached thread can finish after winit closes the Wayland connection, and
EGL destructors can then access that closed connection. This lifetime assumption
must be revisited before adding a mode that stops rendering but keeps the process alive.

## Viewer routing and paste

Capture state is a pure reducer. It consumes both edges of the click or Enter used
to engage capture and requests release on explicit escape, focus loss, overflow,
link loss, or shutdown. The title retains failure reasons even when the menu is hidden.

Input routing is decided before passing events to egui. Its `consumed` result is
insufficient because egui consumes keys such as Tab that a captured target needs.
Outstanding presses track who owns their release. Pointer events over an open menu
stay local; dismissing a menu cannot click through to the target.

Paste reads the clipboard on a separate connection, normalizes line endings, validates
all text, and requests fresh device information before typing letters. The refresh
returns its baseline generation atomically with the request; failed attempts advance
the generation while marking the reading stale. This avoids both waiting for an
already-completed refresh and trusting old CapsLock state.

Paste releases tracked host-held input as a prefix without canceling the capture
session it needs. It then submits paced key transitions. Physical keypresses and target button presses are suppressed during paste;
held-input releases, motion, and wheel events remain routable. The release key
still cancels through the normal epoch mechanism. Cancellation and failure preserve partial progress. Logs contain
counts and status, not clipboard content.

## Audio

Capture uses native S16_LE stereo at 48 kHz without resampling. Playback uses ALSA
`default` with conversion permitted. The period ring holds eight periods of 480
frames: 80 ms of configured capacity, not measured audio latency. Playback prefills
six periods because its four-period device buffer initially absorbs writes without
blocking; the remaining two periods provide a scheduling cushion.

Persistent high occupancy drops a period; persistent low occupancy inserts silence.
Push and pop have independent drift streaks. Underruns and overruns reset those streaks
and count separately so a device outage is not reported as clock drift. Muting zeros
playback while capture and counters continue.

Audio pairs only with the capture node's USB device. Its resolver scans sound cards
against that device directory without querying video or serial. It handles card
renumbering on the same port, but moving ports requires restarting the viewer.

Each side reports opening, running, and failure independently. Retry backoff for an
absent device grows to 10 seconds; a busy device uses the base retry delay. Snapshot
readers supervise opens that have not completed, because a worker blocked inside
open cannot report its own delay. Stop waits up to 250 ms and can detach a stuck worker.
