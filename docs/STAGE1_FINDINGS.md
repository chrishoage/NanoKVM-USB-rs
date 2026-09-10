# Stage 1 — Findings

Stage 1 of `docs/NATIVE_CLIENT_PLAN.md` (rev 4) §12: the minimal usable viewer. This page records
what building it taught us, in the same spirit as `docs/STAGE0_FINDINGS.md`: the plan says what to
build, this page says what we found when we did. Where a finding contradicts the plan, the finding
wins and the plan needs the amendment listed here.

**Environment.** Same desk as Stage 0 — Pro 4K60 unit, Raspberry Pi 3B target at 1080p, Arch
Linux, niri 26.04, Rust 1.89.0 — with one difference that matters: **during Stage 1 the dongle
enumerated on a USB 2.0 link** (`345f:2133` presents as "USB2 Video" at `3-2.2.2`, behind the
dongle's internal hub, alongside the serial bridge at `3-2.2.4`). The SuperSpeed path did not come
back after replugging; the cause is on the host side and was not chased. Consequences:

- The device advertises MJPG at 1920x1080, 1280x720, 720x576, 720x480 and 640x480, each at
  60/50/30/25 fps. **No 4K, no 1440p, and 1080p tops out at 60 fps instead of 240.** The Stage 1
  primary path (1080p60, §6) is unaffected and measured at a sustained 60.00 fps.
- Both nodes sit under the dongle's internal hub with no port `peer` link, which is exactly the
  §8 fallback shape Stage 0 could not test. `scripts/device-health.py` reports it as "both
  contained in 3-2.2 — weaker proof" and passes. Free data point for Stage 2's discovery work.
- Every hardware number below is a USB 2.0 number. Stage 0's were SuperSpeed.

## What was built

One crate, `lib` + `bin`, modules per §4. Line counts are for orientation, not merit.

| Module | Files | What it owns | Tests |
| --- | --- | --- | --- |
| `proto` | `frame.rs`, `report.rs`, `keymap.rs` | Framing, streaming parser with byte-wise resync, HID report builders, `abs_coord`, winit `KeyCode` → HID usage | 26 unit + 29 integration (fixtures, proptests, `abs_coord` law) |
| `link` | `link.rs` | The `Link` trait: one complete frame in, one matched reply out | — |
| `input` | `queue.rs`, `held.rs`, `writer.rs`, `shared.rs`, `stats.rs`, `testing.rs` | §2 in full: coalescing queue, suppression projection, epoch cancellation, overflow failure | 9 unit + 46 integration (ordering, projection, cancellation, overflow, shutdown, writer) |
| `serial` | `port.rs`, `reader.rs`, `link.rs`, `fake.rs` | Port open, reader thread matching by command byte, `Link` impl, fake CH9329 on a pty | 20 pty integration + 1 hardware |
| `capture` | `jpeg.rs`, `decoder.rs`, `handoff.rs`, `v4l2.rs`, `synthetic.rs`, `pipeline.rs` | V4L2 mmap streaming, JPEG-header dimensions, zune-jpeg strict RGBA decode, one-pending-frame slots, capture/decode threads | 74 unit + 13 integration + 3 hardware |
| `viewer` | `app.rs`, `state.rs`, `render.rs`, `wayland.rs`, `input_map.rs` | winit window, render thread, Wayland shortcut inhibit via a second event queue, capture state machine (a pure reducer), input translation | 38 unit + 32 integration (pure reducer + sessions against a real writer) |
| `main.rs` | — | CLI, wiring, SIGINT release, shutdown order | — |

**287 tests run without hardware** (`cargo test`: 147 unit, 140 integration), three consecutive
runs identical. Hardware tests live behind `--features hardware` and are `#[ignore]` (§9.3):

```
cargo test --features hardware --test serial_hardware --test capture_hardware -- --ignored --nocapture
```

## Amendments the plan needs

### B1. §3.4's pixel-centre mapping is not an exact inverse of the device law at 4K

§3.4 says to map through the pixel centre, `((2*px + 1) * 2048) / extent`, so that the last
column is reachable. That formula is exact for every extent measured on this desk, but against the
measured law `pixel = floor(v * extent / 4096)` it lands **one pixel short for 1536 of 3840
columns and 96 of 2160 rows**. The exact right inverse for every extent up to 4096 is

```
v = ceil(px * 4096 / extent)
```

the smallest coordinate whose floor reaches `px`. It sends 2048 for the centre of a 1920-wide
screen (§3.4's "dead centre") and 4094 for column 1919 — 4095 is unreachable at 1920 and that is
correct. `proto::abs_coord` uses the ceiling form; `tests/proto_report.rs` proves the round trip
for every pixel of every advertised extent plus 4096, and keeps the centre formula's failure counts
as a regression guard. **§3.4 should replace "map through the pixel centre" with the ceiling
formula.** The reasoning in §3.4 (the naive floor loses the last column) stands.

### B2. The `v4l` crate's ring is corrupted by any failed dequeue — poll the fd yourself and rebuild on error

`v4l` 0.14's `Stream::next()` requeues its `arena_index` *before* dequeuing, and `dequeue()`
assigns a new `arena_index` only after `VIDIOC_DQBUF` succeeds. So any error out of `next()` —
the crate's own `set_timeout` expiring, or `EIO`, which V4L2 documents for temporary signal
loss — leaves `arena_index` naming a buffer the driver owns; the following `next()` requeues it,
gets `EINVAL`, and every call after that fails. **One stall or one hiccup would kill capture
permanently**, the exact failure §6.1 S1-2 forbids.

`capture::V4l2Source` therefore never lets the crate time out: it polls the file descriptor
itself with `libc::poll`, inspecting `revents` (`POLLHUP`/`POLLNVAL` are a disconnection,
`POLLERR` a stream error), and calls `next()` only when a buffer is ready. Any dequeue error
other than `ENODEV`, and any `POLLERR`, marks the stream broken; the next call drops the
`MmapStream` (whose `Drop` issues the `STREAMOFF` that also clears vb2's sticky queue error),
recreates it, re-primes and restarts, counting `stream_rebuilds`. Verified on hardware:
`a_dropped_stream_is_rebuilt_and_frames_flow_again` and `a_short_timeout_returns_and_the_
stream_recovers`. Consequence for §5.2's "requeue immediately": the crate defers QBUF to the
following `next()`, so one of the four buffers is held between calls; nothing references the
mapping after the copy, so the ownership rule holds.

### B3. §2.6's "clear the flag" must be conditional

Clearing the cancellation flag unconditionally at the end of the sequence loses a trigger that
arrived after the writer latched `E`: `requested_epoch` then sits above `acked_epoch` with no flag
set, and `engage()` can never succeed. The writer clears the flag only when `requested_epoch == E`
and otherwise runs another sequence. Tested (`tests/input_cancellation.rs`, the 50-concurrent-
trigger and cancellation-storm cases). **§2.6 step 2's last bullet should read "clear the flag if
no newer trigger arrived; otherwise repeat".** Related: the step-3 staleness test is strict
(`epoch < acked_epoch`) while the drain is `<= E`, because a fresh session's events legitimately
carry exactly `acked_epoch`.

### B4. The five-byte no-checksum reply needs byte-wise resync, not just tolerance

§3.2 asks the parser not to panic on `57 AB 00 FE 00`. Not panicking is not enough: if the next
frame follows immediately, its `0x57` fills the missing checksum slot, the candidate fails its
checksum, and a parser that then skips the whole candidate loses the next reply. `proto::Parser`
resumes scanning one byte after the failed frame's header, so the sequence `57 AB 00 FE 00` +
`get_info_reply` yields `[BadChecksum, Frame(0x81)]`. Standalone, the five bytes are undecidable
rather than wrong, so the transport's silence timer calls `expire_partial` and gets `Truncated`.
Both cases are fixed regression tests.

### B5. Upstream keymap oddities, carried across deliberately

`reference/browser/src/libs/keyboard/keymap.ts` maps both `LaunchApp2` and `BrowserSearch` to
`0xF0`, and has an entry for `Wake` that is not a W3C `code` (the code is `WakeUp`). The port keeps
the `0xF0` collision (pinned by a test as the only duplicate) and maps winit's `WakeUp`. 170 of
winit 0.30.13's 194 `KeyCode` variants are mapped; the 24 unmapped ones are listed in
`proto::keymap` with a reason each.

### B6. Two adversarial reviews found what the module tests did not

Each module was written test-first against its plan section and came back green. Two
independent adversarial reviews (one on `proto`/`input`/`serial`/`capture`, one on `viewer`/
`main`) then confirmed defects with failing tests, most of them in the seams the plan does not
spell out. Recorded here because each is a class of bug, not a typo, and the regression tests now
carry the plan's intent where the plan was silent.

| # | Where | Defect (confirmed unless marked) | Plan gap |
| --- | --- | --- | --- |
| 1 | `capture::decoder` | A corrupt SOF byte sized the scratch buffer from the header with no ceiling: a 4096x4096 claim decoded `Ok` into 64 MiB; zune's 16384 default admits 1 GiB and a 1.1 s decode. Now bounded by `MAX_WIDTH x MAX_HEIGHT` at the header parse, the decoder options and the allocation. | A6 says "trust the header, never `G_FMT`", and says nothing about bounding it. |
| 2 | `input::shutdown` | `shutdown` triggered the release and only then set stop; a producer in the §2.8 recapture loop could re-engage in between, write input after the final release-all, and be told `Submitted` (21 of 3000 runs; one left key 0x04 held). Now `shutting_down` is set in the same critical section as the trigger, `engage`/`submit` fail with `ShuttingDown`, and `shutdown` returns its own release's outcome. | §2.6 lists shutdown as a trigger without saying recapture must be closed first. |
| 3 | `input::writer` | The cancellation flag was checked between queue entries, not between frames; one entry of accumulated relative motion splits into an unbounded run of reports (788 reports, 8.7 s, in the test). Now checked between every `transact`, and a run is capped at ±4095 per axis with `rel_truncated` counted. | §2.6 says "at a frame boundary"; §2.3 says "split, never clamp"; together they allow an unbounded run. |
| 4 | `serial::reader` | A reply arriving after its transact timed out was matched to the next transact of the same command, which the writer issues back to back. Now a timeout opens a quiet window before the next request is registered. | Inherent ambiguity: a late reply and a fresh one are byte-identical. |
| 5 | `serial::reader` | Dropping the unsolicited receiver made the reader thread exit without marking the link down; every later transact timed out forever. Now a failed hand-off is counted and reading continues. | — |
| 6 | `capture::v4l2` | The crate's `Handle::poll` discards `revents`, so `POLLERR`/`POLLHUP` read as "buffer ready" and the error arm spun at 100 % CPU while reporting `Stalled`. Now `libc::poll` with `revents` inspected and a bounded backoff. | — |
| 7 | `capture::v4l2` | B2's workaround covered the poll timeout only: any failing `DQBUF` (e.g. `EIO`) also leaves the crate's `arena_index` stale and every later `next()` returns `EINVAL`. Now a failed dequeue rebuilds the stream. (Live trigger suspected, code path confirmed from crate source.) | B2 as first written. |
| 8 | `viewer::app` | A consumed capture click's button-up, arriving while still `Engaging`, left a flag set that then ate the release of the next genuine click: left button held on the target until the next release-all. The bookkeeping moved into the pure reducer where the suite can see it. | §12 says the click must not reach the target; the second edge was the gap. |
| 9 | `viewer::app` | Exit joined the render thread, which blocks indefinitely in `present()` when the compositor sends no frame callbacks (occluded, other workspace, locked session). Now occlusion pauses presenting and the join is bounded. (Suspected; consistent with the 999 ms presents measured on the locked session.) | §5.4 describes the block; nothing described exit. |
| 10 | `viewer::app` | Entering `Engaging` did not reschedule the tick, so re-capture had a 0–250 ms dead zone that dropped input and amplified #8. | — |
| 11 | `viewer::render` | The cursor-mapping rectangle was published by the render thread after the next present, so for up to ~110 ms after a resize the pointer mapped through the old rectangle. Now computed on the event loop from the current window size. | — |
| 12 | `viewer::wayland` | The dispatch thread looped in `blocking_dispatch` with no stop and outlived winit's `wl_display`; the spike had ended with `process::exit`. Now a pollable loop with a stop flag, joined on drop. | §1.5 describes the second queue, not its lifetime. |

Explicitly clean under the same scrutiny: the `proto` parser and encoders, the keymap (all 162
usages diffed against upstream), the §2.6 core invariant (no stale event after its release, 200
concurrent rounds), suppression, `Slot` (no loss, duplication or lost wake-up under contention),
the JPEG walker (1.3 M crafted inputs), and the event loop's freedom from any blocking call into
the writer, the pipeline or the renderer (§5.4 by inspection).

### B7. §5.4's "99 % duty inside `present()`" describes the Stage 0 spike, not the client

Measured live (below): `present p50 0.0 ms` with the client rendering one frame per captured
frame at 60/s on a 144 Hz display under Fifo. The swapchain never fills, so `present()` never
waits. The 6.9 ms figure was a spike rendering unthrottled. §5.4 should state the block as
conditional: it happens when the compositor withholds frame callbacks (occluded, locked:
measured 1000 ms) or when the display refresh is at or below the capture rate. The render thread
stays, for exactly those two cases.

## Hardware measurements

### Serial (`/dev/ttyACM1`, CH9329 v1.8)

Verified by consequence, not by acknowledgement (§12): the target's CapsLock bit.

```
GET_INFO in 3.996 ms: version 1.8, target_connected, all locks off
CapsLock tap: press ack 4.17 ms, release ack 15.49 ms   caps false -> true  (0x81 push + re-query agree)
CapsLock tap: press ack 4.19 ms, release ack 4.16 ms    caps true  -> false (target left as found)
stats: frames_rx 9, unsolicited 2, late_replies 0, bad_checksum 0, truncated 0, garbage 0
```

Keyboard ack round trip 4.16–4.19 ms, matching A11's 4.15 ms. The one 15 ms release ack coincides
with the device emitting the lock-state push. A12 reconfirmed: two unsolicited `0x81` frames.

### Capture (`/dev/video4`, MJPG 1920x1080 @ 60, USB 2.0)

```
buffer flags: MAPPED | TIMESTAMP_MONOTONIC | TSTAMP_SRC_SOE
rate from buffer timestamps: 60.00 fps over 1.983 s
capture-to-dequeue age (not latency, §5.5): min 14.75 ms  p50 14.77 ms  max 14.82 ms
bytesused: min 171943  max 171943  mean 171943  (n = 120)
decode (release, zune-jpeg 0.5.15, strict, RGBA): median 2.36 ms  (A19: 2.40 ms)
```

The dequeue age sits on §5.5's predicted one-frame-period floor: it is USB transfer time. **A19's
carry-forward on busy-screen frame sizes is still open** — `bytesused` was byte-constant because
the target was an idle desktop.

### The binary (`nanokvm --serial /dev/ttyACM1 --video /dev/video4`)

Run on the merged tree with the session **locked** (the user was away), which is why presentation
numbers are not yet meaningful: niri sends no frame callbacks to a locked session and `present()`
falls back to Mesa's one-second WSI timeout.

```
CH9329 firmware 1.8, target connected, locks: num=false caps=false scroll=false
S_FMT gave MJPG 1920x1080; frame interval after S_PARM 1/60
gpu adapter: AMD Radeon RX 6800 XT (RADV NAVI21) (Vulkan)
surface present modes offered: ["Mailbox", "Fifo"]   present mode chosen: Fifo
stats: capture 60.0 fps (dropped pre-decode 0 post-decode 348, decode errors 0, capture errors 0)
       | present p50 1000.4 ms (locked session) | event-loop tick and stats cadence held
release-all requested: Shutdown → release-all submitted on shutdown; all threads joined
```

What this run does establish: the whole pipeline runs against the real device, the decoder keeps
up (zero pre-decode drops; post-decode drops are the renderer being throttled by the lock, as
designed by §5.2), the event loop keeps its 250 ms tick while the render thread sits inside a
one-second `present()` — the qualitative half of §5.4 — and shutdown releases the target.

### The binary at an unlocked desk (the §5.4 exit measurement)

Run by the user on the same tree, 2026-09-10, ~8.5 minutes: mouse motion, typing in a text
editor and playing a game on the target. Abs pointer mode, 144 Hz display, USB 2.0 link.

```
surface present modes offered: ["Mailbox", "Fifo"]   present mode chosen: Fifo
stats: capture 60.0 fps (dropped pre-decode 0 post-decode 0, decode errors 0, capture errors 0)
       | capture-to-submit age p50 18.1 ms max 18.5 ms
       | present p50 0.0 ms max 0.0 ms (120 presented, 0 surface recoveries)
       | event-loop handling p50 0 µs p99 0 µs max 2 µs (4096 samples)
       | input queue 1/1 max-in-queue 17.1 ms | reports kb 0 abs 1441 rel 0
       | coalesced abs 13942 rel 0 wheel 0 | cancellations 0 overflows 0 unmapped keys 0
...
stats: ... reports kb 1040 abs 2864 ... coalesced abs 25133 ... unmapped keys 0   (end of run)
release-all requested: FocusLost
^C interrupted; releasing and exiting → release-all requested: Shutdown → release-all submitted
```

What it establishes:

- **§5.4, the side-by-side number.** During sustained pointer motion (4096 samples per 2 s
  interval, the ring's capacity) event-loop handling is p50 0 µs, p99 ≤ 4 µs, max 9 µs across the
  whole run. Nothing on the event loop waits for the renderer, the decoder or the writer.
- **`present()` does not block at the operating point.** `present p50 0.0 ms`, not the 6.9 ms
  the checklist predicted. The 6.9 ms in Stage 0 came from a spike that rendered unthrottled and
  so kept the Fifo swapchain full; the client renders one frame per captured frame, 60/s on a
  144 Hz display, so the queue never fills and `present()` returns at once. The instrument is
  not broken: the same counter read 1000.4 ms on the locked session above. The render thread is
  still justified by that locked case and by any display whose refresh is at or below the capture
  rate, where Fifo would block for a full period per frame. **This is amendment B7** for the plan:
  §5.4's "99 % duty inside `present()`" describes the spike, not the client.
- **Capture-to-submit age** p50 18–19 ms, max ≤ 21 ms, steady over 8 minutes. That is one frame
  period of exposure/readout plus roughly 3 ms of USB 2.0 transfer, 2.4 ms decode and the upload.
  Compare 14.77 ms capture-to-dequeue in the capture test: the extra ~4 ms is decode plus handoff.
- **Input.** Abs reports paced by the ~17 ms ack round trip (`max-in-queue 17.1 ms`, A11), 9:1
  coalescing of pointer motion into abs reports, 1040 keyboard reports with 0 unmapped keys, 0
  overflows, 0 cancellations. The one abs report at capture time is the pointer sync, not a click.
- **Release paths observed:** focus loss (`FocusLost`) and SIGINT (`Shutdown`, then `release-all
  submitted`). Capture engaged once and stayed engaged for the whole session.
- **Consequences observed by the user:** the target's cursor tracks the host pointer, typed text
  appears on the target, a game is playable, and pointer latency is visibly lower than the
  Chromium client's.

### Three short follow-up runs (same desk, 20:51–20:53)

One in abs mode, two with `--pointer relative`, each with capture/release cycles and `Ctrl+C`.

```
abs:  compositor deactivated the shortcut inhibitor; releasing capture → release-all requested: UserRequested
      re-capture → abs 15, coalesced 193 → second release → cancellations 2, rel 2, kb 4
rel:  pointer grab requested: Locked (requested, not yet in force) → rel 600, coalesced rel 5234
      input queue 1/3 max-in-queue 39.0 ms  (rel + button barrier + rel: two ack round trips)
rel:  target lock state: num=false caps=true scroll=false  …  4 s later  caps=false
every run: ^C → release-all requested: Shutdown → release-all submitted on shutdown
```

- **Mod+Escape release** works, five times over three runs: niri deactivates the inhibitor, the
  viewer requests `UserRequested`, one cancellation per release, and the counters show exactly one
  keyboard release-all and one mouse release-all per cycle (`kb`/`rel` each +1). Re-capture after
  release engages immediately (S1-2 observed live).
- **Relative mode** works: the lock is requested and honoured, rel reports flow at the ack pace,
  and deltas coalesce ~9:1. `max-in-queue 39 ms` with queue depth 3 is a button transition
  splitting a motion run into two entries around the barrier, as §2 specifies.
- **The CapsLock consequence check** passes: a captured CapsLock tap flips the target's lock bit,
  seen as the unsolicited `0x81` push logged as `target lock state: … caps=true`, then back.
- **Observation, not a defect:** each Mod+Escape cycle counts two keyboard reports, and one of them
  is the Super press. niri delivers Mod to the focused client before it recognises Mod+Escape, so
  the target sees a bare Super tap on every compositor-initiated release. Every KVM client has
  this; it is only visible if the target binds something to a lone Super tap. Carried forward.

**Not yet exercised across all runs:** the `Pause` release key (every observed release was the
compositor deactivating the inhibitor), the wheel (`wheel 0` in every run), and whether a niri
bind is inhibited while captured (not visible in the log; only the user can say). None blocks
the exit.

## Interactive sign-off checklist

1. `cargo build --release && ./target/release/nanokvm --serial /dev/ttyACM1 --video /dev/video4 --stats-interval 2`
2. ✅ Window shows the Pi desktop letterboxed; `capture-to-submit age p50` in the 15–25 ms range.
   `present p50` reads 0.0 ms at 144 Hz, not 6.9 ms — see B7 above; 6.9 ms is expected only on a
   display at or below 60 Hz.
3. ✅ Move the mouse over the window without clicking: `event-loop handling p50/p99` stay in the
   microseconds. That line is the §5.4 exit measurement; pasted above.
4. ✅ Click once: title becomes `[captured — Pause or Mod+Esc releases]`; the target's pointer must
   not jump or click. Move: the target's cursor tracks absolutely.
5. ◐ Press a niri bind (`Mod+Shift+/`): the compositor must not react ☐ (user-observed only).
   Press CapsLock: the log line `target lock state: … caps=true` is the consequence check ✅.
6. ◐ Release three ways, one per run: `Pause` ☐, `Mod+Escape` ✅, focusing another window ✅. Each
   shows the cursor, restores compositor shortcuts, logs the release reason, leaves nothing held.
7. ✅ Re-capture after each release; it should engage within a frame or two.
8. ✅ `--pointer relative`: cursor hidden and frozen on the host, target cursor moves (magnitudes
   differ — target acceleration, §3.4). ☐ Also roll the wheel once in either mode (`wheel` counter).
9. ✅ `Ctrl+C` in the terminal: `interrupted; releasing and exiting`, then `release-all submitted`.

## Carried forward

- **Busy-screen JPEG frame sizes** (A19). Still unmeasured; `bytesused` is now instrumented in
  `PipelineStats`, so the number falls out of ordinary use.
- **The SuperSpeed link.** Unrelated to this project's code, but until it returns, 4K and 1440p
  cannot be exercised and Stage 2's peer-link pairing test cannot be rerun.
- **Scripts and macros** (§2.9 blocking admission) are not implemented; `Producer::
  wait_until_engageable` is the only blocking primitive. Stage 3.
- **Reconnect** (§2.7) is not implemented. `SerialLink` stays down after a transport failure and
  the input writer records the release as `unsent`. Stage 2, as planned.
- **Bare Super tap on compositor-initiated release.** niri delivers the Mod press before it
  acts on Mod+Escape, so the target sees Super down then release-all. Harmless on the Pi desktop
  as observed; if a target binds a lone Super tap, the fix is a deferred lone-modifier press,
  which trades latency for it. Decide in Stage 2 alongside §2.6.
- **`max_barriers = 256`** is a guess: roughly 0.8 s of keyboard transitions at the measured rate,
  nearer 3 s of clicks. Tune with a real viewer feeding it.
