# Native NanoKVM-USB Client — Design (rev 4)

Replace the Chromium-based client with a native Linux binary that drives the KVM over
its CH9329 serial link and its UVC video node.

- **Scope:** Linux only. Target desktop is **niri 26.04 on Wayland**.
- **Language:** Rust. Settled; see §1.
- **Shape:** one crate, `lib` + `bin`, modules. No workspace. See §7.
- **Reference implementation:** `sipeed/NanoKVM-USB` @ `1d1dd5e`.
- **Status:** **Stage 0 complete.** Its findings are folded into this revision. Ready for
  Stage 1 (§12).

**Priority: prove the complete local KVM experience early, then expand the
architecture based on demonstrated need.**

## Evidence

Rev 4 is the first revision containing measurements rather than only reasoning. Claims
marked **measured** were established on hardware during Stage 0 and are sourced in
`docs/STAGE0_FINDINGS.md`, which indexes the per-spike evidence under `docs/stage0/`.

The unit of record is a **NanoKVM-USB Pro (4K60)** driving a Raspberry Pi 3B target, on an
Arch Linux host running niri 26.04 on Wayland. Where a measurement could plausibly differ on
another unit, another target or another compositor, this document says so rather than
generalising silently.

---

## 0. Corrections to rev 1

Recorded explicitly because rev 1 is wrong on these points and the errors were
load-bearing.

| Rev 1 claim | Status | Correction |
| --- | --- | --- |
| Ebitengine exposes no usable physical key | **Wrong** | Ebitengine's `Key` constants *are* physical positions: "KeyQ represents Q key on US keyboards and ' (quote) key on Dvorak keyboards." The physical-key argument does not distinguish the languages. |
| Go's JPEG decoder caps frame rate below 60 fps | **Unsupported** | No benchmark was run on this hardware. Removed as a decision input; see §1.3. |
| Compositor shortcuts cannot be forwarded on Wayland | **Wrong** | `zwp_keyboard_shortcuts_inhibit_v1` exists for exactly this. **niri 26.04 implements it** (`src/protocols/shortcuts_inhibit.rs`). Client-side support is the open question, not the protocol. |
| Never use `PresentMode::Fifo` | **Wrong and hazardous** | `Immediate` is *unsupported on Wayland*. Since wgpu removed automatic fallback, naming a non-`Auto` mode **panics** when unavailable. `Mailbox` still presents at vblank. See §5.3. |
| Renegotiate baud at startup | **Withdrawn** | Do not rewrite chip settings automatically. Measure first. See §5.1. |
| An RFB frontend is "mostly free" | **Wrong** | It carries input translation, session ownership, encoding, auth and disconnect cleanup. Deferred and optional. See §10. |

### 0.1 Corrections to rev 2

| Rev 2 claim | Status | Correction |
| --- | --- | --- |
| glfw owns the Wayland connection and does not hand out the surface or display | **Wrong** | `glfwGetWaylandDisplay()` and `glfwGetWaylandWindow()` return the `wl_display` and `wl_surface`. Neither glfw nor winit exposes `wl_seat`, so both require a self-bound registry. This was never a differentiator. §1.1. |
| Queue overflow converges via state resync; worst case a key is held marginally too long | **Wrong** | Resending state cannot recover a lost press/release pair or its ordering. Replaced with explicit overflow failure. §2.8. |
| Release-all jumps the queue | **Insufficient** | Jumping ahead leaves queued key-downs to run *after* the release and re-hold the key. Requires cancellation, not priority. §2.6. |
| "No signal" state on video loss | **Underspecified** | A legitimate target display can be black. Distinguish disconnection, capture error, and positively reported signal loss. §6.1. |
| Report `present_time − capture_timestamp` | **Mislabeled** | `present()` returning is not proof the image reached the screen. Start with capture-to-submit age. §5.5. |
| 1-slot latest-value cell | **Unsafe as stated** | Holding an mmap buffer past requeue is a use-after-free. The requirement is one pending frame, owned. §5.2. |

### 0.2 Corrections to rev 3

Rev 3 was reasoned but unmeasured. Stage 0 measured it. These are the claims that did not
survive contact with the hardware, in descending order of how much damage they would have
done. Full evidence in `docs/STAGE0_FINDINGS.md`, whose amendment numbers are cited as A*n*.

| Rev 3 claim | Status | Correction |
| --- | --- | --- |
| Mouse payloads are `4 B [buttons, dx, dy, wheel]` and `6 B [buttons, xLo, xHi, yLo, yHi, wheel]` | **Wrong** | The CH9329 requires a leading **mode byte**: rel is `5 B [0x01, …]`, abs is `7 B [0x02, …]`. **The device ACKs the short form and does nothing.** A17. |
| "YUYV passthrough may be viable at lower resolutions and avoids decode entirely" | **Wrong** | The device advertises exactly one pixel format, `MJPG`. Decode is unconditionally on the hot path. A1. |
| "On a vblank-locked surface `get_current_texture()` can block for up to a frame" | **Wrong** | Acquire does not block on this stack. **`present()` blocks**, for ~99 % of every frame. The §5.4 conclusion survives; its premise does not. A3. |
| Absolute range is `0 to 32767`, or `MAX_ABS_COORD = 4096`, unresolved | **Resolved** | 12 bits usable inside a 13-bit field. Full scale 4095, divisor 4096, wrap at 8192. A18. |
| §5.1's per-report wire ceilings bound the input rate | **Misleading** | Baud is honoured but irrelevant: this is CDC-ACM, and `write()` returns in ~50 µs. The real limit is the ack round trip, ~6× worse than predicted for the mouse. A11. |
| §3.2's short-buffer case would only matter in "a stricter port" | **Understated** | The device really emits `57 AB 00 FE 00` — five bytes, no checksum. A Rust port indexing `frame[5+len]` panics on real traffic. A13. |
| §3.1's impact is that responses "validate by accident" | **Understated twice** | It also silently accepts a corrupted trailing payload byte, and it rejects **every error frame the device sends**, so upstream cannot tell a device error from silence. A8, A14. |
| §6.1's three capture conditions are all distinguishable | **Wrong on this hardware** | No signal-presence indication exists in any form. The "Signal loss" row can never fire. A5. |
| §8's proof is "both interfaces belong to the same USB device" | **Unavailable** | They are two USB devices on two buses. Replaced by the kernel's USB port `peer` link, which is stronger than the "weak, suggestive" adjacency rev 3 expected to fall back on. A2. |
| §7.2 treats a static musl binary as an open possibility | **Closed, negative** | It builds and links nothing, then gets zero GPU adapters, because static musl has no dynamic loader. A10. |
| §1.3 leaves decoder throughput an open measurement | **Answered** | zune-jpeg clears 1080p60 with 6.9× margin. 4K60 clears at 1.6× and stops clearing under busier content. A19. |
| §11 q8 "does the device emit unsolicited frames?" | **Answered: yes** | Lock-key changes push an unrequested frame, which can arrive after an unrelated ack. Replies must be matched by CMD, not arrival order. A12. |

Two findings are new rather than corrections, and both are Stage 1 requirements because they
are cheap to honour early and expensive to retrofit:

- **Frame dimensions come from the JPEG header, never from `G_FMT`.** After an idle period the
  device emits up to eight frames at the *previous* resolution, with no error flag and no
  reliable sequence tell. A6.
- **`S_PARM` must be called explicitly.** `S_FMT` alone leaves 1080p and 720p at a 240 fps
  default. A7.

And one testing principle, learned the expensive way (A17):

> **On this device an acknowledgement is not evidence of effect.** Hardware assertions must
> observe a consequence — a pixel moved, a lock bit changed — not an `ACK`.

Building the later stages corrected this document again, and those amendments are cited the same
way, from their own findings documents rather than folded in here: B*n* in
`docs/STAGE1_FINDINGS.md`, C*n* in `docs/STAGE2_FINDINGS.md`, and **D1–D10 in
`docs/STAGE3_FINDINGS.md`** — what the CLI stage changed about §2.9, §8, §10.2 and §12.

---

## 1. Language: Rust

Rust is a reasonable choice. **This does not require showing Go is unsuitable, and no
further effort goes into the comparison.** The language decision is settled; what
follows is the rationale of record, not an open question.

### 1.1 Wayland access is symmetric, not a differentiator

Shortcut inhibition requires binding `zwp_keyboard_shortcuts_inhibit_manager_v1` and
creating an inhibitor against the window's `wl_surface` for a `wl_seat`. No
general-purpose windowing library exposes this as an API, so in either language you
bind the global yourself alongside whatever owns the window.

Both toolkits are in the same position:

- winit exposes `wl_display` and `wl_surface` through `raw-window-handle`.
- glfw exposes them through `glfwGetWaylandDisplay()` and `glfwGetWaylandWindow()`.
- **Neither exposes `wl_seat`,** so either way you bind your own registry to get it.

Rev 2 claimed glfw withholds these. That was wrong. Whether Ebitengine offers a usable
route to glfw's native handles is a separate question, and one this project does not
need to answer.

What remains true is that `wayland-client` is first-class in Rust and the technique of
driving extra protocols alongside winit's surface is well-trodden. That is a
convenience argument, not a capability one.

### 1.2 Supporting: nobody else owns the main loop

Ebitengine is a game engine. `Update` runs on a fixed tick, `Draw` per frame, and the
engine owns the loop. A KVM wants to present when a frame arrives from V4L2 and to
process input without waiting on a tick. `SetTPS` can be pushed around but you are
working against the framework's ownership of scheduling — which is precisely where the
frame-freshness and input-responsiveness requirements in §2 and §5 live.

winit hands you the event loop and lets a capture thread drive redraws via
`request_redraw()`.

### 1.3 Decode throughput — measured, and it clears

MJPEG decode is on the hot path, and rev 4 knows it is *unconditionally* on the hot path,
because the device offers no uncompressed format at all (§6). Rev 1 asserted a frame-rate
ceiling with no measurement; Stage 0 measured it.

**zune-jpeg 0.5, single thread, decoding into a reused RGBA buffer** (measured, AMD Ryzen 9
9950X3D):

| Mode | Median decode | Sustained | Margin at target rate |
| --- | --- | --- | --- |
| 1920x1080 | 2.40 ms | 416 fps | **6.9x at 60 fps** |
| 2560x1440 | 4.16 ms | 242 fps | 1.7x at 144 fps |
| 3840x2160 | 10.01 ms | 99.8 fps | 1.6x at 60 fps |

The decoder clears the bar at the chosen mode with room to spare, so the language decision
needs no revisiting.

**The margin at 4K is thinner than it looks.** The corpus was an idle, low-entropy desktop.
Against synthetic frames 2.4x the byte size, 1080p60 still clears at 4x but **4K60 stops
clearing** — 16.36 ms against a 16.67 ms budget. This is an independent second reason to
default to 1080p60 (§6). Real busy-screen frame sizes are still unmeasured; instrument
`bytesused` in Stage 1.

Three implementation consequences, all measured rather than assumed:

- **Request RGBA from the decoder.** zune emits it directly through a dedicated AVX2 kernel
  at no cost. Decoding to RGB and repacking on the CPU costs **34 %**. A benchmark that times
  only decode hides this.
- **Enable strict mode.** A truncated frame otherwise decodes as `Ok` with a partial image.
  `set_strict_mode(true)` makes it an error at no measured cost, which matters given §6's
  stale frames and §5.1's torn writes.
- **One decode thread**, as §4.1 says. More threads raise aggregate throughput but never
  reduce per-frame latency, and §5.2's one-pending-frame rule leaves them nothing to do.

Alternatives were measured and rejected: `jpeg-decoder` is 2.2x slower even with rayon, and
libjpeg-turbo is 12–16 % faster while still marginal at 4K, which does not buy a C dependency.

### 1.4 Where Go would have been easier

Static linking is genuinely easier in Go: `CGO_ENABLED=0` produces a truly static
binary. Rust needs the musl target and the `v4l` crate's pure-v4l2 backend to avoid
linking `libv4l2`.

**But the goal is partly unreachable in either language.** A GPU-rendering binary
dlopens the Vulkan or GL driver at runtime, so it is never fully self-contained.
"Single binary" realistically means "one executable with no bundled runtime and no
install step." §7.2 makes that an early experiment rather than an assumption.

### 1.5 Decision

Rust. Settled. **The Stage 0 Wayland spike succeeded, so the `smithay-client-toolkit`
fallback is not needed and winit stands.** Recorded because rev 3 left it conditional.

The technique that works, since it is not obvious: take winit's `wl_display` and `wl_surface`
from `raw-window-handle`, wrap the display with `Backend::from_foreign_display` and
`Connection::from_backend`, then create a **second `wl_event_queue`** on it. libwayland
dispatches per queue, so winit's loop and our dispatch thread coexist without contention —
sharing winit's queue is what fails. Self-bind `wl_seat` and the inhibit manager on our own
registry, and re-wrap winit's surface through `ObjectId::from_ptr`. No protocol error, and
`active` arrives in about 0.2 ms.

---

## 2. Input correctness

The hardest correctness surface in the program. Specified before implementation
because it is easy to get subtly wrong and hard to notice.

### 2.1 One queue, one order

A single writer thread consumes a single queue. **Do not split keyboard and mouse into
separate queues** — that loses ordering between a modifier press and a click, and
shift-click breaks.

### 2.2 Event classes

| Class | Examples | Coalescing rule |
| --- | --- | --- |
| Absolute state | pointer position | may be **replaced** by a newer value |
| Accumulating delta | relative motion, wheel | must be **summed**, never replaced |
| Transition | button press/release, any key state change | **barrier**. Never dropped, never merged, never reordered |

Rev 1's "keep the newest, drop the rest" was wrong: applied to deltas it discards
movement, and applied across transitions it destroys ordering.

### 2.3 Barrier semantics

A transition flushes the newest pending position and all accumulated deltas ahead of
it, then enqueues itself, then starts a fresh accumulation.

This makes the required sequence intact by construction:

```
move to A → press → drag to B → release

  positions before press coalesce to A
  press is a barrier      → flush A, then press
  positions during drag coalesce to B
  release is a barrier    → flush B, then release
```

Accumulated relative motion and wheel deltas saturate at the report range
(`-127..127`) and **split across multiple reports** when the accumulation exceeds it,
rather than clamping and losing the remainder.

**Keyboard reports are never coalesced.** The 8-byte report is full state, so
consecutive reports look mergeable — but merging drops a fast press-release pair
entirely and the target never sees the keystroke. Every keyboard state change is a
barrier.

### 2.4 Known, accepted degradation

Under sustained overload a drag becomes a straight line from press point to release
point, because intermediate positions coalesce away. This is correct for pointing and
wrong for freehand drawing on the target. Document it; do not treat it as a bug. Report
it through the drop counters in §5.4 so it is visible rather than mysterious.

### 2.5 Held-state tracking

Two distinct pieces of state, and conflating them is a bug:

- **Physical held set** — every non-modifier key currently held down, unbounded, plus
  the modifier bitmask and the mouse button mask. This is the authoritative record of
  what the user is physically holding.
- **The six report slots** — a *projection* of that set into the 8-byte HID report,
  which carries at most six non-modifier keys.

**Projection rule: suppression, not promotion.** Rev 3's first wording contradicted
itself — "re-project the first six of the physical set" promotes a seventh key when a
slot frees, while the same sentence said it must not appear. Resolved in favour of
**suppression**, for the reason below.

Every key in the physical held set carries a status decided **at press time**, which
**never changes while that key remains held**:

| Status | Assigned when | Behaviour |
| --- | --- | --- |
| `Reported` | pressed while fewer than six keys are `Reported` | occupies a slot |
| `Suppressed` | pressed while six keys are already `Reported` | tracked, never enters any report for the duration of this hold |

- **On key down:** count current `Reported` keys. Fewer than six, admit as `Reported`
  and rebuild the report. Six already, admit as `Suppressed` and **emit no report** —
  nothing observable changed.
- **On key up:** remove the key from the set. If it was `Reported`, its slot frees and
  the report is rebuilt from the remaining `Reported` keys. **No `Suppressed` key is
  ever promoted into the freed slot.**
- **A `Suppressed` key becomes eligible again only through a fresh press**, which
  requires the user to release it first. Its next press is evaluated against the
  then-current `Reported` count like any other.

**Consequence, stated deliberately:** the report can carry fewer than six keys while
more than six are physically held. Holding seven and releasing one yields a five-key
report, not six. That is intended.

**Why suppression over promotion.** Promotion is what real 6KRO hardware does, since a
keyboard reports whatever it currently scans. But on a KVM, promotion delivers a
keystroke to someone's console at a moment the user did not choose — they pressed the
seventh key, saw nothing, and it arrives later when an unrelated key is released. The
failure mode of suppression is that a key does not register and the user presses it
again, which is obvious and self-correcting. The failure mode of promotion is a delayed
phantom keystroke on a live console. Beyond six non-modifier keys is an anomaly on a KVM
rather than intent, so the safer behaviour wins.

Two open decisions, both hardware-dependent and unresolved:

- Whether to send the HID rollover-error report (`0x01` in all six slots) when more than
  six are held, which is what real 6KRO hardware does, instead of suppressing. Deferred:
  **suppression is the specified behaviour until measurement changes it.** Adopting
  rollover reporting would replace the table above, so it is a design change, not a
  tweak. Whether the CH9329 forwards a rollover report faithfully is unknown. §11.
- Whether the chip's own behaviour differs from what a report implies. §11.

**Ignore host key repeat.** winit's `KeyEvent` carries a `repeat` flag; discard those
events when forwarding physical held state. The target's own operating system generates
repetition from the held HID state, and forwarding host repeats doubles it.

### 2.6 Release-all cancels; it does not merely jump ahead

Emit a full release — zeroed keyboard report, all buttons up — on **every** one of:

- window focus loss
- pointer or keyboard capture release
- an explicit user "release all" binding
- serial reconnect
- overflow failure (§2.8)
- clean shutdown

Priority alone is **not** sufficient. A release-all that jumps to the head of the queue
still leaves queued key-downs behind it, which then run and re-hold the key. The
release must *invalidate* prior work, not outrank it.

**A generation counter alone is not sufficient.** It leaves a race: the writer checks an
event's generation, passes it, and is then preempted. Cancellation happens. The writer
resumes and writes the now-stale event. If the release was scheduled ahead of it — which
it must be, since a release has to be schedulable when the queue is full — the device
sees release-then-keydown and the key is held. That is the exact bug the generation
counter was introduced to prevent.

**The fix is to make the writer the sole serialization point.** The generation check is
not a filter that producers or a monitor apply concurrently; it is applied by the one
thread that writes, in the same loop iteration as the write, and the cancellation
sequence runs to completion in that thread. There is no window because there is no
concurrency at the boundary.

**State:**

- `requested_epoch` — incremented by any cancellation trigger. Shared, atomic.
- `acked_epoch` — advanced only by the writer. Shared, atomic.
- A **1-slot cancellation flag**, separate from the event queue. Setting an already-set
  flag is a no-op, so repeated or concurrent triggers coalesce into one sequence and one
  release. **This is how a release stays schedulable when the event queue is full:** the
  release is never enqueued at all, and the flag always has room.
- Every input event carries the epoch it was produced under.

**The writer's loop, in this order every iteration:**

1. **Check the cancellation flag before dequeuing or writing anything.**
2. If set, run the cancellation sequence to completion, without interleaving any other
   write:
   - latch `requested_epoch` as `E`;
   - **drain and discard** every queued event whose epoch is `≤ E`, writing none of
     them;
   - synthesize and write the release-all reports — zeroed keyboard, all buttons up —
     directly, not through the queue;
   - clear the tracked state: physical held set with its per-key statuses (§2.5),
     modifier mask, button mask;
   - set `acked_epoch = E` with an outcome of `submitted` or `unsent`;
   - clear the flag.
3. If not set, dequeue one event, re-check its epoch against `acked_epoch`, discard it
   if stale, otherwise write it.

**Cancellation happens at a frame boundary, never mid-frame.** The writer's atomic unit
is one complete protocol frame. It checks the flag *between* frames, and completes any
frame already in progress before entering the sequence. Frames are 10 to 14 bytes
(§5.1), so the delay is bounded and small. Abandoning a partially written frame would
desynchronize the chip's parser, which is worse than the delay.

**Gating the next capture session.** A new session must not emit input until it observes
`acked_epoch ≥` its own epoch. This pairs with the deliberate-recapture requirement in
§2.8: the user's re-grab is what triggers the check, and input is not accepted until the
acknowledgment lands.

### 2.6.1 Acknowledgment is not delivery

These are different facts and the design must not conflate them.

| | Means | Does not mean |
| --- | --- | --- |
| `acked_epoch` advanced | The writer discarded all stale work and will write none of it, and the release was either submitted to the transport or recorded as unsent | That the device received it, that the chip processed it, or that the target's HID state changed |

Consequences:

- **A failed transport must still advance the ack**, with outcome `unsent`. Otherwise a
  dead cable wedges input permanently. The release is re-attempted on reconnect (§2.7),
  because the target's HID state after an unplug is unknown.
- **Bytes already handed to the kernel cannot be recalled.** The guarantee is that no
  further stale events are *submitted*, not that nothing stale reaches the device.
- **No software guarantee survives the cable.** If the link is gone, nothing here
  ensures the target released the key. The ack is a local barrier, not a receipt.

Report the outcome where the user can see it. An `unsent` release means the target may
still be holding keys, and that is worth saying rather than hiding behind a cleared local
state.

### 2.7 Reconnect

On serial reconnect:

1. **Discard the whole pending queue.** Replaying stale input is worse than losing it —
   a click queued three seconds ago may land somewhere destructive.
2. Send release-all to resynchronize the target's HID state.
3. Re-query device info.
4. Resume.

Never replay.

### 2.8 Bounded-queue overflow: fail explicitly

Rev 2 claimed state resync repairs overflow. **It does not.** Resending current HID
state can restore a held modifier, but it cannot recover a lost press/release pair — a
dropped keystroke or click simply never happened from the target's point of view — and
it cannot restore ordering relative to other input. There is no silent recovery here.

Rev 2 also inferred an inhuman input rate from saturation. Wrong: **a stalled writer
fills the queue at ordinary rates.** A blocked port write, a device that stopped
draining, or a serial link wedged mid-frame all saturate a queue while the user types
normally. Saturation indicates the transport is not keeping up, which says nothing
about the producer.

**Initial policy — explicit failure, no silent degradation:**

1. **Reclaim by coalescing** under §2.2 rules first. This is the normal path and
   handles the ordinary case, where the queue is dominated by redundant absolute
   positions.
2. If coalescing cannot make room, the input session has **failed**. Do not drop
   transitions and continue.
   - **Trigger cancellation** (§2.6) — increment `requested_epoch` and set the
     cancellation flag. The writer discards the stale queue and writes the release
     itself; nothing is enqueued, which is why a full queue cannot block this path.
   - **Surface it.** The viewer shows that input was interrupted — not a silent
     counter. Report the cancellation outcome (§2.6.1): an `unsent` release means the
     target may still be holding keys.
   - **Require deliberate recapture.** Do not silently resume. Input stays disengaged
     until the user re-grabs, and the new session waits on `acked_epoch` (§2.6) before
     accepting input.
3. **Scripts get an error.** If a `type` or macro sequence cannot be preserved intact,
   it fails and says so, naming how far it got. Never a partially delivered sequence
   reported as success.

This trades availability for correctness, which is the right trade for a tool that
drives someone else's console. Revisit only with measurement showing overflow happens
in normal use for a reason worth accommodating.

Instrument queue depth, time-in-queue, coalescing rate and overflow events regardless
— the goal is that overflow is observable and rare, not merely handled.

### 2.9 Admission policy differs by caller

The GUI must never block on the writer. A script must never lose a keystroke. Same
queue, different admission policy:

- **Viewer:** non-blocking submission. On failure, §2.8 applies.
- **Scripts and macros:** blocking, paced sends, with an error rather than a drop when
  the sequence cannot be preserved.

---

## 3. Upstream findings, now confirmed against hardware

Rechecked against the pinned commit `1d1dd5e`, which is still upstream's HEAD, so there is no
divergence to reconcile. Rev 1 stated these too confidently; rev 3 hedged them appropriately;
rev 4 has run them on the device. Every one held, and two proved worse than described.

### 3.1 CONFIRMED (inspection **and hardware**) — receive checksum is one byte short

`desktop/src/main/device/proto.ts`, mirrored in `browser/src/libs/device/proto.ts`.

The checksum byte is read at the correct offset:

```ts
sum = data[headerIndex + 5 + dataLen]
```

The comparison value is computed over the wrong range:

```ts
for (let i = headerIndex; i < headerIndex + 4 + dataLen; i++) { s += data[i] }
```

That covers `4 + dataLen` bytes. Correct coverage is `5 + dataLen`
(`HEAD1, HEAD2, ADDR, CMD, LEN` plus the whole payload), matching what `save()` does on
transmit. The final payload byte is excluded. The bound should be
`headerIndex + 5 + dataLen`.

Arithmetic verified twice, and the runtime impact is now **confirmed on hardware, in three
parts**. Rev 3 anticipated only the first.

1. **The accidental pass is real.** The `GET_INFO` reply is
   `57 AB 00 81 08 38 01 00 00 00 00 00 00 C4`, it does end in `0x00`, and upstream's
   `decode()` validates it byte-for-byte. Hypothesis confirmed.
2. **A corrupted trailing payload byte is silently accepted**, because the checksum never
   covers it. For `GET_INFO` that byte is not load-bearing, which is why the bug survived.
   It is still a data-integrity hole rather than merely a lucky pass.
3. **Every error frame is rejected.** Error replies end in `0xE4` or `0xE5`, never `0x00`, so
   upstream's short checksum discards all of them. **Upstream cannot distinguish a reported
   device error from silence.** This is the largest functional consequence and rev 3 missed
   it entirely.

Error frames are undocumented upstream and real: see the Appendix.

### 3.2 CONFIRMED (inspection **and hardware**) — length guard is two bytes short

Same function, and new in this revision:

```ts
if (data.length < headerIndex + 3 + dataLen + 1) { return -1 }   // needs >= head+4+dataLen
sum = data[headerIndex + 5 + dataLen]                            // indexes head+5+dataLen
```

The guard admits buffers two bytes shorter than the code then indexes. In JavaScript that
yields `undefined`, the comparison always fails, and `decode` returns `-1` — so it **fails
safe**.

**But this is not hypothetical, and rev 3 was wrong to frame it as something only "a stricter
port would" hit.** The device really sends a frame the guard is wrong about. Its reply to an
undefined command is

```
57 AB 00 FE 00        five bytes: header, addr, CMD 0x7E|0x80, LEN 0x00, and nothing
```

confirmed over a ten-second wait. The frame format requires a checksum byte even at `LEN 0`.
**A Rust port that indexes `frame[5 + len]` panics on real traffic from this device.**

The dead `try`/`catch` around the checksum read is worth noting too: indexing past the end of
a JS array does not throw, so that branch is unreachable. The author expected a bounds fault
the language does not produce — which is exactly the fault a Rust port *does* produce.

This is now a fixed regression case, not only a fuzzing target. §9.2 item 6.

### 3.3 OUT OF SCOPE — desktop read path

`desktop/src/main/device/serial-port.ts`:

```ts
const { value, done } = await this.port.read()
```

That is the Web Streams shape, used correctly in the browser build which holds a real
`ReadableStreamDefaultReader`. `node-serialport` extends Node's `stream.Readable`, whose
`read()` returns a `Buffer` or `null` synchronously with no `done`. Destructuring throws
or yields `undefined`.

Compounding it, `init()` registers `this.port.on('data', () => {})`, which puts the
stream in flowing mode and drains it — so `read()` would return `null` regardless of the
destructuring. Two independent reasons the read path cannot work as written.

**Deliberately not confirmed at runtime, and closed rather than carried forward.** It concerns
the Electron build's use of `node-serialport`, which this port shares no code with. It
explains why the desktop client misbehaves and changes nothing about the Rust design.
Recorded so nobody schedules work against it.

### 3.4 RESOLVED — absolute coordinate range is 12 bits inside a 13-bit field

`browser/src/libs/mouse/index.ts` documents `0 to 32767` and implements
`MAX_ABS_COORD = 4096`, where a normalized `1.0` yields exactly `4096` — one past a 12-bit
maximum. Rev 3 called this unresolvable without hardware. It was resolved with hardware, by
sending a coordinate and finding the cursor in the captured frame. Ninety measurements, no
outliers, one law:

```
effective = min(v & 0x1FFF, 4095)
pixel     = floor(effective * extent / 4096)
```

- **Full scale is 4095; the divisor is 4096.** 2048 lands dead centre, and the slope matches
  `1920/4096` and `1080/4096` to six decimals.
- **`4096` clamps to the far edge rather than wrapping.** The wrap is further out: **8192 maps
  to pixel (0, 0)**, a 13-bit field with 12 usable bits.
- The documented `0 to 32767` is **refuted**. `MAX_ABS_COORD = 4096` is the correct divisor,
  and upstream's `1.0 → 4096` off-by-one is real but harmless, saved by the clamp.
- The same full scale applies to both axes, each onto its own extent. The device applies no
  aspect correction.

**For the encoder (§2):** clamp to 4095 ourselves and never rely on the device, because an
overshoot past 8191 silently jumps the pointer to the top-left corner of a live console. Map
through the pixel *centre*, `((2*px + 1) * 2048) / extent`; the naive `px * 4096 / extent`
sends 4093 for pixel 1919 and leaves the last column and row unreachable.

**Relative motion cannot substitute.** It works, with the standard convention (`+dx` right,
`+dy` down, no sign flip), but magnitudes do not survive: 30 units yields 15–20 pixels,
varying, because the target applies pointer acceleration. **Relative motion can never be used
to reach a specific coordinate.**

---

## 4. Architecture

One crate, `lib` + `bin`. No workspace, no `no_std`, no separate versioning, no generic
frontend abstraction. Introduce a trait only where a test needs to substitute a fake.

```
src/
  main.rs        argument parsing, wiring
  lib.rs
  proto/         CH9329 framing, HID report builders, keymap.   Pure. No I/O.
  input/         event classes, coalescing, held-state, release-all  (§2)
  serial/        port open, reconnect, framing, writer thread + queue
  capture/       V4L2 streaming, format negotiation, MJPEG decode
  discovery/     sysfs topology, serial↔video pairing            (§8)
  viewer/        winit window, wgpu render, input capture, shortcut inhibit
```

**Two traits, both justified by a specific test:**

- `Link` — byte sink/source, so `serial` can be driven by a pty fake and `input` can be
  tested against an in-memory recorder.
- `FrameSource` — so `viewer` can run against a synthetic pattern generator with no
  hardware.

Nothing else gets an interface until something concrete demands it.

`proto` stays I/O-free because that is where the correctness risk concentrates and it
makes property tests and fuzzing cheap. That is a module boundary, not a crate
boundary.

### 4.1 Threads

| Thread | Owns | Handoff out |
| --- | --- | --- |
| Capture | V4L2 dequeue, copy out, requeue | one pending owned frame |
| Decode | MJPEG → RGBA, one thread (§1.3) | one pending decoded frame |
| **Render** | **wgpu acquire, draw, present** | — |
| Serial writer | port writes, pacing | — |
| Event loop | winit events, input | bounded input queue |

Rev 3 had the event loop submitting renders. **Rev 4 splits rendering onto its own thread**,
because `present()` blocks for about 99 % of every frame and would starve input (§5.4). The
event loop no longer touches wgpu.

The decode row also loses its YUYV alternative: there isn't one (§6).

Each video handoff carries **at most one pending frame, and the frame owns its bytes**.
See §5.2 — that is the requirement; the data structure is not.

---

## 5. Latency and freshness

### 5.1 Serial pacing — measured, and the bottleneck is real

Rev 3 computed a wire-byte ceiling and correctly declined to conclude anything from it. The
measurement is now in, and it says the arithmetic was sound but was measuring the wrong thing.

**The baud rate is honoured** — 9600, 38400 and 115200 all produce silence, and sustained
throughput measures 5861 B/s against 5760 predicted. **It is also not the limit.** This is
CDC-ACM over USB rather than a real UART, and `write()` returns in about **50 µs** regardless
of report size. What actually bounds the input rate is the acknowledged round trip:

| Report | Wire bytes | Rev 3 predicted | **Measured 1:1 rate** | Ack round trip |
| --- | --- | --- | --- | --- |
| Mouse relative (5 B payload) | 11 | 575 /s | **91 /s** | 17.0 ms |
| Mouse absolute (7 B payload) | 13 | 480 /s | **83 /s** | 17.0 ms |
| Keyboard (8 B payload) | 14 | 411 /s | **332 /s** | 4.15 ms |

Note the wire-byte column also changed: rev 3 undercounted both mouse frames by one byte,
because it was missing the mode byte (§3.4, Appendix).

**Mouse reports are roughly six times slower than rev 3 assumed**, and **overload provides no
backpressure — it silently corrupts**, producing bursts of `0xE4` checksum errors rather than
blocking or reporting.

This settles a question rev 3 left open. At 83 absolute reports per second against a pointer
that moves continuously, serial *is* a real bottleneck. **Mouse coalescing (§2.2) is therefore
a correctness requirement, not an optimisation**, and it does not wait for Stage 2
instrumentation to justify it. Instrument queue depth, time-in-queue and drop counts anyway,
so overload is observable rather than mysterious.

**The receive parser has no inter-byte timeout.** Send a header claiming `LEN 8` followed by
two payload bytes and the chip waits — tested to ten seconds — then consumes the *next*
command as the remainder of the truncated one. One partial write corrupts the following
command. This makes §2.6's "never abandon a partially written frame" load-bearing rather than
tidy, and it means recovery after a torn write needs an explicit resynchronisation strategy
rather than a reconnect-and-hope (§2.7).

Baud reconfiguration remains an **explicit opt-in operation**, never at startup, and not
before verifying:

- whether the setting persists across replug,
- whether the stock browser/Electron client still works afterward,
- how to recover a device left at a rate nothing expects.

Rev 1's "renegotiate at startup" would silently break the vendor client. Still withdrawn — and
now clearly pointless, since baud is not what limits the link.

### 5.2 Frame freshness

Staleness, not throughput, is what makes a KVM feel wrong.

- **At most one pending frame** at each handoff. Anything that can hold two accumulates
  staleness by design. Whether that is a mutex-guarded slot, a rendezvous channel, or a
  capacity-1 channel with replace-on-full is an implementation choice, not a
  requirement.
- **Drop before decoding.** When a new frame arrives and the previous one is still
  undecoded, discard the old one *pre-decode*. Decoding an already-obsolete frame
  spends the most expensive step in the pipeline on output nobody will see.
- **Separate capture from decode** so V4L2 dequeue never stalls behind a decode.
- **Render takes whatever is pending.** No catch-up, no queue drain.

**Frame bytes must be owned.** A pending frame must not reference an mmap buffer that
has been requeued to V4L2 — that is a use-after-free, and rev 2's wording invited it.

Start simple: **copy the compressed frame out, requeue the capture buffer immediately,
and hand on the owned copy.** At MJPEG sizes the copy is cheap relative to decode, and
it makes buffer lifetime trivially correct — capture never waits on a consumer, and no
consumer can outlive a buffer.

Only consider a scheme that keeps buffers checked out across the handoff — buffer pools,
refcounted mappings, a larger queue depth — if measurement shows the copy actually
costs something. It probably does not, and the lifetime complexity is real.

### 5.3 Presentation mode

Rev 1 was wrong twice here.

- `Immediate` is **not supported on Wayland**.
- wgpu removed automatic fallback, so naming an unavailable non-`Auto` mode **panics**.
- `Mailbox` is tear-free but still presents at vblank; it is not an escape from vsync.

All three points are **confirmed measured** on niri, which offers exactly `[Mailbox, Fifo]`.
`Immediate` and `FifoRelaxed` are both absent, forcing `Immediate` is fatal, and `AutoNoVsync`
degrades cleanly.

One addition rev 3 did not anticipate: **the panic only happens with wgpu's default error
handler.** An application that installs its own `on_uncaptured_error` gets a silently
unconfigured surface instead, and the panic moves to the next `get_current_texture()` as
"Surface is not configured for presentation". Fatal either way, but it moves, so the failure
can appear unrelated to the mode choice.

Correct approach: **enumerate** `surface.get_capabilities().present_modes`, choose from what
is actually offered, and always retain a working fallback. Expect `Fifo` on Wayland and treat
that as the baseline rather than a failure. Never name a mode blind.

Because vblank-locked presentation is the likely reality, §5.2 carries the latency
budget. Presenting the *newest* frame at vblank is what matters.

### 5.4 Rendering must not block input handling — RESOLVED: render on its own thread

A dedicated serial thread does not protect responsiveness if the event loop blocks. Rev 3
assumed the block was in `get_current_texture()`. **It is not.**

Measured on niri, 400 frames per pass, in microseconds:

| Present mode | acquire median | acquire p95 | acquire max | **present median** | frame period |
| --- | --- | --- | --- | --- | --- |
| Mailbox | 12 | 43 | 363 | **3** | 31 |
| Fifo | 4 | 81 | 668 | **6915** | 6945 |

**`get_current_texture()` does not block** — median 4 µs under `Fifo`, worst case 771 µs over
800 measured frames, three orders of magnitude below a frame. **`present()` blocks instead**,
for a full refresh period: about 6.9 ms of every 6.94 ms frame at 144 Hz, roughly **99 % duty**,
and unconditionally rather than occasionally.

So of rev 3's two options:

- ~~Keep rendering on the event loop but never block on acquire~~ — **fixes nothing.** Acquire
  is not where the block is.
- **Render on its own thread, leaving the event loop free to service input.** ← adopted.

Rev 3 called this "the option that cannot be wrong, only unnecessary". It is measurably
necessary: a renderer on the event loop would starve input about 99 % of the time.

**Do not reach for `Mailbox` to dodge the block.** It is non-blocking only because it does not
pace the client at all — the loop free-runs at roughly 25 000 fps, burning a core and the GPU.
§5.2's requirement is presenting the *newest* frame at vblank, which `Fifo` already does. Keep
`Fifo`, pace the render thread off the capture, and let it block.

The choice is also robust to the measurement moving: putting presentation on its own thread is
correct whether the block sits in acquire, in present, or relocates in a future Mesa or wgpu
release.

**The acceptance test is unchanged and still owed:** input latency must not correlate with
render timing. The spike measured the block, not the resulting input latency. Stage 1 owns
this and must measure it rather than assume the structure achieved it.

### 5.5 Label latency measurements for what they actually are

`present()` returning is **not** proof the image reached the screen. It means the
frame was submitted. Claiming presentation latency from it overstates what was
measured.

**Start with capture-to-submit age:** V4L2 buffer timestamp to the moment of submit.
That is honestly measurable and it covers the pipeline this design controls.

Two caveats to check rather than assume:

- **The timestamp clock is verified and usable.** Every buffer reports
  `TIMESTAMP_MONOTONIC | TSTAMP_SRC_SOE` — monotonic, start-of-exposure, never `_COPY` or
  `_UNKNOWN`, with the error flag never set. It is directly subtractable from a local
  `CLOCK_MONOTONIC` read, so **capture-to-submit age is measurable.**

  Two bounds on what the number may be claimed to mean. It carries a **floor of about one
  frame period** (14.9 ms at 1080p60) which is USB transfer time, not pipeline latency. And
  because `uvcvideo` runs with `hwtimestamps=0`, the stamp is **host-side**, so it excludes
  every microsecond spent inside the dongle. Report it as capture-to-submit and nothing
  more.
- **Claim presentation latency only with presentation feedback.** Wayland can report
  actual presentation times, and only that justifies an end-to-end number. Until then
  the metric is named capture-to-submit and nothing more.

Track separately, and name each honestly:

- capture-to-submit age
- frames dropped pre-decode
- frames dropped post-decode
- input queue depth, time-in-queue, coalescing rate, overflow events

FPS alone hides exactly the failures §5.2 and §5.4 exist to prevent.

---

## 6. Video decisions come from the device — and the device has decided

Rev 3 said not to design the pipeline around an assumed format, and to enumerate first. That
was right, and enumeration produced a narrower answer than expected.

**The unit on this desk is a Pro 4K60, measured rather than inferred**, and it advertises
**exactly one pixel format: `MJPG`.** No YUYV, no uncompressed anything — confirmed by
`ENUM_FMT`, by the raw USB descriptors, and by `S_FMT` rejecting YUYV.

**Rev 3's "YUYV passthrough may be viable and avoids decode entirely" is therefore dead.**
There is no decode-free path at any resolution, which is why §1.3's decoder benchmark stopped
being a supporting measurement and became a blocking dependency.

Every advertised mode delivered its advertised rate (measured):

| Resolution | Measured sustained | Bytes/frame | Wire rate |
| --- | --- | --- | --- |
| 3840x2160 | 59.4–59.7 fps | 665,401 | 318 Mbit/s |
| 2560x1440 | 144.0 fps | 280,937 | 322 Mbit/s |
| 1920x1080 | 240.1 fps | 171,921 | 328 Mbit/s |
| 1280x720 | 240.1 fps | 74,364 | 142 Mbit/s |
| 720x576, 720x480, 640x480 | 60.0 fps | 28–36 K | 6–14 Mbit/s |

**Primary path: MJPEG at 1920x1080, 60 fps.** The target outputs 1080p, so 4K costs about 3.9x
the decode and bandwidth for an upscale — confirmed an upscale by PSNR, not assumed. §1.3
independently reaches the same default from the decode side.

Two device behaviours that must be honoured from the first commit, because they are cheap now
and expensive later:

- **Take frame dimensions from the JPEG start-of-frame header, never from `G_FMT`.** After an
  idle period the device reproducibly emits **up to eight consecutive frames at the previous
  resolution**, carrying the new `sizeimage`, with no `ERROR` flag and no reliable `sequence`
  tell. Reproduced 3/3 with an idle gap, 0/10 without.
- **Call `S_PARM` explicitly.** `S_FMT` alone leaves 1080p and 720p at a **240 fps** default, so
  the pipeline silently inherits a rate nothing asked for.

### 6.1 Capture failure criteria, and which stage owns them

Rev 3 titled this "first usable version" while Stage 2 owned all five criteria and
Stage 1's exit was "a usable KVM" — ambiguous, and now resolved by splitting on a
principle: **Stage 1 owns the properties that are expensive to retrofit; Stage 2 owns
recovery behaviour.**

Subsystem independence and not-crashing are architectural. If capture failure can tear
down the input path, that coupling is baked into the structure and unpicking it later
means rework. Automatic recovery from each specific failure shape is additive and can
land later without disturbing anything.

**Report only what is actually known.** Rev 2 treated black or garbage frames as
evidence of signal loss. They are not — **a legitimate target display can be black**, and
painting "NO SIGNAL" over a genuinely blank console is a worse failure than saying
nothing. Never infer signal state from pixel content.

Three conditions, distinguishable and to be distinguished:

| Condition | Evidence | Response |
| --- | --- | --- |
| **Device disconnected** | node gone, `ENODEV` | say so; attempt rediscovery |
| **Capture error or stall** | `EIO`, dequeue timeout, stream stopped | say the capture stalled, not that the signal is gone; attempt restart |
| ~~**Signal loss**~~ | ~~only a positive indication from the device~~ | **unreachable on this hardware — see below** |

**Signal loss cannot be reported on this device. Measured, and the answer is a flat no.** There
is no control, no DV timings support (all four ioctls return `ENOTTY`), no UVC extension unit,
`v4l2_input.status` is permanently zero with `capabilities = 0`, and the UVC error bit is never
set. The vendor HID interface on the video device was investigated as a candidate and rejected:
its descriptor carries feature reports only, with no INPUT item, so it cannot push a
notification at all, and reading it returns video buffer contents rather than status.

**So the three-condition table collapses to two.** Rev 3 already specified the correct fallback
— "if the capture device offers no reliable signal indication, do not synthesize one" — and that
is now the permanent behaviour rather than a contingency: preserve the last image, report that
frames stopped arriving, and leave interpretation to the user.

This is stated as a limitation rather than deleted, because a different unit or a firmware
revision might expose one, and because **S2-1 cannot be met as written** and should not sit in
the plan looking achievable.

#### Stage 1 — architectural, cannot be retrofitted cheaply

- **S1-1. Serial stays live through video loss.** Sending a chord to a target with no
  video is a primary use case, so a dead capture path must not tear down input. This is
  the independence property; it belongs in the structure from the first commit.
- **S1-2. Frames stopping does not crash or hang.** The window stays alive and
  responsive, the last image is preserved, and the tool says frames stopped arriving.
  No automatic recovery required yet — reporting and surviving is enough.

#### Stage 2 — recovery behaviour, additive

- **S2-1. Both reachable conditions distinguished** — disconnection and capture stall — each
  reported as itself rather than collapsed into one message. **Signal loss is not one of them**
  on this hardware; see above. Amended from "all three".
- **S2-2. Target reboot** — frames stop and later resume, possibly at a different
  target resolution, with no restart and no leaked buffers.
- **S2-3. Target resolution change** — **first establish whether it changes anything
  locally** (§11 q13). The device may rescale internally and keep the negotiated UVC
  format fixed, in which case a mode change needs no capture rebuild at all and
  rebuilding is a self-inflicted glitch. Measure before writing renegotiation logic. If
  the negotiated format *does* change, then stop streaming, re-query, reallocate,
  restart, without crashing, hanging or leaking mmap buffers.
- **S2-4. Device unplug and replug** — both nodes, in either order.

The "never infer signal state from pixels" rule above applies from Stage 1 onward. It
costs nothing to honour early and is a behavioural commitment, not a feature.

---

## 7. Packaging and scope

### 7.1 Single package

One crate with the module layout in §4. Deferred until a concrete requirement appears:
separate crates, independent versioning, `no_std`, and any generic frontend framework.

### 7.2 Packaging — settled, and the static binary is abandoned

Rev 1 assumed a static binary. Rev 3 made it an early experiment. Stage 0 ran the experiment
and the answer is negative in an instructive way.

**The musl build succeeds and links nothing.** The full dependency set — `v4l`, `serialport`,
`winit`, `wgpu`, `zune-jpeg` — builds for `x86_64-unknown-linux-musl` in 13 seconds, needs no
`musl-gcc` because rustup's self-contained sysroot suffices, and produces a static-pie binary
with **zero `DT_NEEDED` entries**. It runs, enumerates V4L2 nodes and decodes JPEG.

**It is also useless.** A static musl binary has no dynamic loader, so *every* `dlopen` returns
"Dynamic loading not supported": Vulkan, GL, EGL, `libwayland-client`, `libxkbcommon`. It gets
zero GPU adapters and cannot open a window. Dynamic musl fails too — tested properly in Alpine
and run against this host's libraries, where glibc shared objects refuse to relocate.

§1.4 anticipated that a GPU-rendering binary is never fully self-contained. The measurement
sharpens it into a rule: **musl can only absorb the three libraries every host already has,
while making unloadable the ones that could never be bundled anyway.**

Measured link surface: **3 libraries at build time** (`libc`, `libm`, `libgcc_s`) against **45
at runtime, all dlopened** — `libvulkan`, `libEGL`, `libwayland-egl`, `libwayland-client` and
`libxkbcommon` directly, the rest being Mesa's stack. Note that Wayland and xkbcommon are
dlopened, never linked.

**`libv4l2` is avoidable, and avoided.** The `v4l` crate's default `v4l2` feature is bindgen
over `<linux/videodev2.h>` with no link directive; a negative control with `features =
["libv4l"]` does pull `libv4l2.so.0`. Pin `default-features = false, features = ["v4l2"]` so
nothing re-adds it.

**"Single binary" is defined as:** one dynamically-linked glibc executable, no installer, no
bundled runtime, no sidecar; linking only `libc`, `libm` and `libgcc_s`; dlopening only the
host's GPU driver and Wayland libraries; requiring neither `libv4l2` nor `libudev`.

**Release target: `x86_64-unknown-linux-gnu`, built against an old glibc** (an oldstable
container, or `cargo-zigbuild` targeting `gnu.2.28`), stripped, roughly 8 MB.

---

## 8. Device pairing must not guess

Rev 1's "shared USB ancestry" is not proof. Two dongles behind one hub share an ancestor.

**The topology is now measured, and rev 3's evidence hierarchy does not survive it.** The two
nodes are **not** two interfaces of one USB device. They are two separate USB devices on two
separate buses:

```
external hub port 2
  ├─ SuperSpeed (bus 4)  4-2.2   345f:2133  MACROSILICON USB3 Video   ← /dev/video4
  └─ High Speed (bus 3)  3-2.2   1a40:0101  USB2.0 HUB (inside dongle)
                           └─ port 4  3-2.2.4  1a86:55d3              ← /dev/ttyACM1
```

The dongle contains its own unbranded 4-port USB 2.0 hub with the CH9329 bridge on port 4,
while the video device, being SuperSpeed, enumerates on a bus the kernel presents as entirely
separate. So rev 3's item 1 — the only one it called proof — **is unavailable on this
hardware**.

The replacement is stronger than the fallback rev 3 expected to be left with. The kernel
publishes SuperSpeed-to-HighSpeed port `peer` symlinks, and **two peered ports are the same
physical connector by kernel assertion**, not by heuristic.

**Evidence, strongest first (revised):**

1. Both interfaces belong to the **same USB device** (identical `busnum:devnum`). Proof when
   available. **Not available on this unit.**
2. **USB port `peer` link.** Resolve the video node to its USB device, find the hub port it
   occupies, follow that port's `peer` symlink, and check whether the serial device is the
   peer port's device or a descendant of it. Proof, and it is what this unit needs.
3. **Containment under the dongle's internal hub.** On a USB 2.0-only port the video device
   cannot reach SuperSpeed and enumerates behind that internal hub alongside the serial
   device, with no `peer` link involved. The check degrades to a common-ancestor test, which
   is sound *because the shared ancestor is inside the dongle*. **Untested** — the unit has
   only been observed on a SuperSpeed port.
4. ~~A **unique `iSerial`**~~ — **demoted, it cannot pair anything here.** Both devices have
   one and they are unrelated. The video device reports `20210623`, a date, almost certainly a
   model-wide firmware build date rather than a unit identifier. The serial bridge reports
   `5C37176280`, which is plausibly per-chip but identifies only the bridge. `iSerial` is
   useful for pinning *one* node across replug and useless for pairing two.

A working probe, including a negative control that correctly rejects an unrelated webcam on a
different dock, is in `spikes/topology/pair.py`. It needs only sysfs; no libusb.

The device also reports a third serial over the link itself — `GET_USB_STRING` returns
`Sipeed` / `NanoKVM-USB` / `BA1612624UJPW2RUJ`. It is unit-unique but useless for pairing,
since reading it requires having already opened the serial link. Useful for logs and for
Stage 3's device listing.

**Policy:**

- Resolve `/sys/class/video4linux/videoN/device` and `/sys/class/tty/ttyACMN/device` up
  to the USB device, not to an arbitrary shared ancestor.
- Exactly one candidate pair → use it.
- Ambiguous → **fail with a message listing the candidates.** Never silently pick one.
- Always honour explicit `--serial` and `--video` overrides, which also make the tool
  usable when discovery is wrong.

---

## 9. Tests

### 9.1 Captures are fixtures, not authority

Rev 1 treated recorded traffic from the reference client as ground truth. Given §3 that
is unsound: a capture from a buggy client records the bug.

Use **both**, and reconcile them:

- **Independently specified packet examples** derived from the CH9329 datasheet, written
  by hand with expected bytes. These are the authority.
- **Round-trip properties** — `decode(encode(x)) == x`, checksum invariance over
  arbitrary payloads.
- **Captured traffic** as corroboration and as a regression corpus. Where a capture
  disagrees with the datasheet, that disagreement is a finding to investigate, not a
  test to make pass.

### 9.2 Priorities

In order:

1. **Input ordering** — the §2.3 sequence, transitions never reordered or lost, deltas
   accumulated not replaced, split reports on saturation.
2. **Mouse encoding** — the mode byte is present, payload lengths are exactly 5 and 7, and
   absolute coordinates are clamped to 4095 and mapped through the pixel centre (§3.4). Cheap
   tests guarding a failure the device will not report: it acknowledges a malformed mouse
   report and does nothing.
3. **Held-key projection: suppression** (§2.5). Status is fixed at press time and never
   changes while held, so test that invariant directly:
   - Seven keys pressed in order — the report carries the first six; the seventh appears
     in no report.
   - Release one of the six — the report carries the remaining **five**, not six. The
     suppressed key is **not** promoted.
   - Release the suppressed key, then press it again while five are `Reported` — it now
     enters a slot.
   - Pressing a key while six are `Reported` emits **no report at all**, since nothing
     observable changed.
   - Release all — zeroed report, and the physical set with its statuses is empty.
   - Host repeat events are discarded and never forwarded.
4. **Cancellation synchronization** (§2.6). The race is the point; test the writer's
   observable byte order through the fake link, not the internal counters:
   - A key-down that passed its epoch check before cancellation is **never written after
     the release** for that epoch.
   - **Cancellation with a completely full event queue still produces a release**, since
     the release is synthesized by the writer and never enqueued.
   - **No frame is ever split by cancellation** — every byte sequence on the link is a
     complete, well-formed frame.
   - Repeated and concurrent triggers coalesce into **one** sequence and one release.
   - A new session does not emit input until `acked_epoch` reaches its epoch.
   - **Transport down:** the ack still advances with outcome `unsent`, input is not
     wedged, the outcome is surfaced, and the release is re-attempted on reconnect
     (§2.7).
   - Acknowledgment is asserted as a **local barrier only** — no test may assert that
     the device received anything (§2.6.1).
5. **Overflow failure** (§2.8) — saturation invalidates pending input, releases, and
   requires recapture rather than degrading; a script whose sequence cannot be
   preserved receives an error naming how far it got. Include a **stalled writer** as
   the saturation cause, not just a fast producer.
6. **Parser fragmentation and resynchronization** — frames split across arbitrary read
   boundaries, garbage between frames, truncated frames, the §3.2 short-buffer case. Fuzz
   this; it parses bytes off hardware. Three cases are now **known real traffic** and belong
   in the fixed regression corpus rather than only the fuzzer:
   - the five-byte reply with **no checksum byte** (§3.2) — must not panic;
   - **error frames** `CMD | 0xC0` with a one-byte code (Appendix) — must be surfaced, not
     silently dropped the way upstream drops them (§3.1);
   - an **unsolicited frame arriving between a request and its reply** (Appendix) — the
     reply must still be matched correctly, by command byte.
7. **Reconnect** — queue discarded, release-all sent, no replay, state re-queried.
8. **Frame buffer lifetime** (§5.2) — no pending frame references a requeued mmap
   buffer. Worth an explicit test since the failure is silent corruption, not a crash.

### 9.3 Mechanics

- `proto` is I/O-free, so it takes property tests and a fuzzer directly.
- A **fake CH9329 over `openpty`** exercises the real serial module unmodified: open,
  reconnect, framing, pacing, coalescing, end to end, no hardware.
- A **synthetic `FrameSource`** covers decode and render and makes latency and
  drop-behaviour assertions deterministic.
- Hardware tests sit behind a feature flag, `#[ignore]` by default, never gating CI.

---

## 10. Deferred, and why

### 10.1 Remote access is not nearly free

Rev 1 called an RFB frontend mostly free. It is not: input translation from RFB keysyms
to physical HID codes, session ownership when two clients connect, encoding choice,
authentication, and cleanup on abrupt disconnect. Each is real work with real failure
modes, and RFB is designed for desktop deltas rather than continuous video.

Optional, and last.

### 10.2 Key forwarding and text injection are different operations

Not to be conflated:

- **Physical key forwarding** maps a `KeyCode` to a HID usage code. Layout-independent.
  Core.
- **Text injection** (`nanokvm type "..."`) requires a **declared target keyboard
  layout**, because producing a character means choosing key-plus-modifier combinations
  valid on the *target's* layout, which the host cannot observe.

Text injection therefore needs an explicit `--layout` (default US QWERTY, stated in
help output, not assumed silently) and a declared policy for characters unreachable on
that layout: fail loudly with the offending characters named, rather than sending
approximations. Ship it in the CLI stage, not the core.

---

## 11. Open hardware questions

Measured questions, not assumptions. Grouped by what they block.

### Answered in Stage 0 — closed

All eleven Stage 0 questions were answered by measurement. **None were carried forward as
unmeasured constraints.** Evidence in `docs/STAGE0_FINDINGS.md`; the section named in each row
now states the answer.

| # | Question | Answer | Where |
| --- | --- | --- | --- |
| 1 | Formats and model | **Pro 4K60. `MJPG` only** — no uncompressed format exists | §6 |
| 2 | USB topology, `iSerial` | **Two devices, two buses.** Pairing proof is the port `peer` link; `iSerial` cannot pair | §8 |
| 3 | Absolute range | **12 bits usable in a 13-bit field.** Full scale 4095, divisor 4096, wrap at 8192 | §3.4 |
| 4 | Checksum | **Datasheet formula accepted.** Bad checksums get an explicit error frame. `GET_INFO` replies do end in `0x00` | §3.1, Appendix |
| 5 | Decode benchmark | **Clears.** 1080p60 at 6.9x margin; 4K60 marginal and fails under busier content | §1.3 |
| 6 | Wayland on niri | **Works.** Shortcut inhibit, pointer lock, escape and focus loss all verified. winit stands | §1.5, §5.3 |
| 7 | Packaging | **musl is a dead end.** Ship glibc; `libv4l2` avoidable | §7.2 |
| 8 | Handshake | **Nothing beyond `GET_INFO`** — but the device *does* push unsolicited frames | Appendix |
| 9 | Timestamp clock | **Monotonic, start-of-exposure.** Capture-to-submit age is measurable | §5.5 |
| 10 | Signal indication | **None exists, in any form.** §6.1's signal-loss row can never fire | §6.1 |
| 11 | Render blocking | **Acquire does not block; `present()` does.** Resolved to a dedicated render thread | §5.4 |

Two caveats attach to q9 and are worth carrying: the capture-to-submit delta has a floor of
about one frame period that is USB transfer time, and the timestamp is host-side, so it
excludes all in-dongle latency. Neither invalidates the metric; both bound what it may be
claimed to mean (§5.5).

### Still open, not blocking Stage 1

- **Input latency versus render timing** (§5.4). The acceptance test is behavioural and was
  never run. Stage 1 owns it.
- **Real busy-screen JPEG frame sizes** (§1.3). Every margin above rests on an idle desktop.
  Instrument `bytesused`.
- **Resynchronisation after a torn write** (§5.1). The chip waits indefinitely then eats the
  next command. There is no strategy yet, and §2.7's reconnect does not obviously cover it.
- **The dongle on a USB 2.0-only port** (§8). The fallback pairing shape is reasoned, untested.
- **VT-switch key combinations** are not inhibitable and will always reach the host.
- **Bindgen cross-architecture correctness** — `v4l2-sys-mit`'s build script passes no
  `--target`.
- **The no-GPU failure message.** A user whose driver fails to load needs an explanation, not a
  panic.

### Blocks Stage 2 exit

12. **Capture failure shape.** Which of the three §6.1 conditions does this unit
    actually produce on HDMI unplug, and on target reboot?
13. **Mode-change effect.** Does a target resolution change alter the *negotiated UVC
    format*, or does the device rescale internally and keep it fixed? Decides whether
    capture rebuild logic is needed at all. (§6.1 item S2-3)
14. **Rollover.** **Unresolved.** Does the CH9329 forward a HID rollover-error report
    faithfully, and does its own behaviour differ from what a report implies? Does the
    chip impose its own key limit independent of the report? §2.5 specifies suppression
    and does not depend on the answer; a decision to adopt rollover reporting instead
    would replace that projection rule, so this stays open rather than assumed. (§2.5)
15. **Baud.** Only if §5.1 measurement shows serial is a real bottleneck: does
    `SET_PARA_CFG` persist across replug, does the stock client still work afterward,
    and how is a mis-set device recovered?

### Non-technical

16. A native client is a maintenance commitment the browser version does not impose, and
    upstream will keep changing. Keeping `proto` small and I/O-free is the hedge, since
    it is the only place upstream drift can hurt. (Renumbered in rev 4; rev 3 had two items
    numbered 15.)

---

## 12. Delivery

Reordered so the complete local experience is proven before the architecture grows.

### Stage 0 — Feasibility spikes — **COMPLETE**

**Purpose was to prove four things feasible:** Wayland integration, basic device
communication, capture, packaging. All four are feasible, one with a changed shape.

| Condition | Result |
| --- | --- |
| Four feasibility questions answered | **Yes** |
| §11 questions 1–11 answered or carried forward | **Yes — all eleven answered by measurement, none deferred** |
| Primary capture path chosen | **MJPEG at 1920x1080, 60 fps.** There was no second option (§6) |
| Fixtures in hand | **Yes** — 135 captured frames, 22 validated packet examples, annotated traffic logs |

The spike code was throwaway by design and lives in `spikes/`, one standalone crate per spike.
Evidence and the full amendment list are in `docs/STAGE0_FINDINGS.md`; §0.2 above summarises
what changed in this document as a result.

**Discovery was not a Stage 0 deliverable** and was not built. Explicit `--serial` and
`--video` selection is sufficient throughout and remains supported permanently (§8). The
topology work in §8 was cheap and non-blocking, and is recorded so Stage 2 need not re-derive
it.

### Stage 1 — Minimal usable viewer

The whole point. A window on niri showing the target, with working keyboard and mouse.
Everything below is now specified rather than contingent, because Stage 0 answered the
questions Stage 1 would otherwise have had to guess at.

- `proto` with datasheet tests and round-trip properties, reading its frame examples from
  `fixtures/packets/ch9329.toml` rather than retyping them. **Assert mouse payload lengths of
  5 and 7** (§3.4, Appendix).
- `serial` with the pty fake. **No handshake is required beyond `GET_INFO`**, but the reader
  must **match replies by command byte, not arrival order**, because the device pushes
  unsolicited frames (Appendix). It must also survive a five-byte reply with no checksum byte
  (§3.2).
- `capture` on **MJPEG 1920x1080 at 60 fps**, with owned frame bytes (§5.2), **dimensions read
  from the JPEG header rather than `G_FMT`**, and **`S_PARM` called explicitly** (§6).
- Decode with **zune-jpeg, RGBA output, strict mode, one thread** (§1.3).
- Explicit `--serial`/`--video` selection. Discovery heuristics can come later.
- `viewer`: winit + wgpu, **enumerated** present mode (expect `[Mailbox, Fifo]`), shortcut
  inhibit — which works, via a second `wl_event_queue` on winit's display and a self-bound
  registry — pointer lock, release-all on focus loss. **niri does not deactivate the inhibitor
  on focus loss, so release-all must be client-driven** (§2.6). The compositor's
  shortcut-inhibit toggle flips the inhibitor to `inactive`, and that event is the user's way
  out; act on it.
- **Render on its own thread** (§5.4). Settled by measurement, not a default.
- Input path with the §2 rules in place from the start — held-key suppression, release-all
  cancellation and coalescing correctness are not things to retrofit. **Mouse coalescing is a
  correctness requirement here, not a Stage 2 optimisation** (§5.1). Clamp absolute
  coordinates to 4095 in our own encoder and map through the pixel centre (§3.4).
- **§6.1 S1-1 and S1-2** — serial independent of capture, and frames stopping neither crashes
  nor hangs. Architectural; the rest of §6.1 is Stage 2.

**Exit:** the tool is a usable KVM on this desktop, **input latency does not correlate with
render timing — measured, not assumed** (§5.4), and video loss neither crashes it nor kills
input. Not robust yet.

**Verify by consequence, not by acknowledgement.** Every hardware assertion must observe an
effect — a pixel moved, a lock bit changed — because this device acknowledges requests it does
not act on (§3.4).

### Stage 2 — Hardening

- Input: full §2.6 release-all triggers and cancellation, §2.8 overflow failure and
  recapture, ordering and projection tests.
- Reconnect on both nodes, in either order, per §2.7.
- **§6.1 S2-1 through S2-4** — the recovery behaviours, including the mode-change
  measurement before any renegotiation logic.
- `discovery` with the §8 evidence rules, now that explicit selection has been carrying
  the tool.
- Latency instrumentation: capture-to-submit age, drop counters, queue depth. Then
  measure §5.1 and decide whether serial is actually a bottleneck.
- Answer §11 questions 12–15, and close the items §11 lists as still open — notably the
  torn-write resynchronisation strategy (§5.1) and the USB 2.0-only pairing shape (§8).

**Exit:** survives target reboots, replugs, resolution changes and overload without
stuck keys or restarts.

### Stage 3 — CLI conveniences — **COMPLETE**

Five subcommands on the one binary, with `nanokvm` alone still the viewer: `devices` (the §8
listing, plus an opt-in `--probe` that opens only the serial node discovery would select),
`shot` (one frame to `.jpg`, `.png` or stdout, under a stated frame policy), `key <chord>`,
`type` with the declared layout and unreachable-character policy §10.2 asked for, and `macro`
files. Verified on hardware by consequence — the target's lock bit, the typed commands on its
screen — on 2026-09-11.

Evidence, the ten amendments this document needs as a result (D1–D10) and the measurements are
in `docs/STAGE3_FINDINGS.md`.

### Stage 4 — Optional extensions

RFB server (§10.1), clipboard paste, session recording, mouse jiggler. Each justified by
demonstrated need, not by the architecture having room for it.

---

## Appendix: protocol reference from the pinned source

**Corrected in rev 4 against hardware.** Where this appendix and upstream's source disagree,
the hardware wins; where the hardware was not exercised, it says so. Machine-readable,
individually validated frame examples live in `fixtures/packets/ch9329.toml`, which the §9
tests should read directly rather than retyping — retyping is where a transcription bug enters
and silently blesses a wrong encoder.

Commands (`desktop/src/main/device/proto.ts`):

```
GET_INFO              0x01
SEND_KB_GENERAL_DATA  0x02
SEND_KB_MEDIA_DATA    0x03
SEND_MS_ABS_DATA      0x04
SEND_MS_REL_DATA      0x05
SEND_MY_HID_DATA      0x06
READ_MY_HID_DATA      0x87
GET_PARA_CFG          0x08
SET_PARA_CFG          0x09
GET_USB_STRING        0x0a
SET_USB_STRING        0x0b
SET_DEFAULT_CFG       0x0c
RESET                 0x0f
```

Frame layout:

```
HEAD1(0x57) HEAD2(0xAB) ADDR CMD LEN DATA[LEN] SUM
SUM = (HEAD1 + HEAD2 + ADDR + CMD + LEN + sum(DATA)) & 0xFF
```

Note: this is the **transmit** formula from `save()`, which matches the datasheet.
`decode()` disagrees with it — see §3.1. Treat the datasheet as authoritative.

Payload shapes. **The mouse payloads are not bare HID reports — each carries a leading mode
byte.** Rev 3 omitted it and would not have moved the pointer at all:

```
Keyboard      8 B   [modifier, 0x00, key0..key5]                    6-key rollover
Mouse rel     5 B   [0x01, buttons, dx, dy, wheel]                  dx/dy/wheel -127..127
Mouse abs     7 B   [0x02, buttons, xLo, xHi, yLo, yHi, wheel]      little-endian, 0..4095 (§3.4)
```

Upstream does send the mode byte, but prepends it at the call site
(`browser/src/components/mouse/relative.tsx:136`, `absolute.tsx:166`) rather than in the report
builder this appendix was originally derived from. Reading only the builder loses it.

> **The failure mode is silent.** The device **acknowledges the short form and does nothing**.
> A protocol test asserting on `ACK` passes while the pointer never moves. Assert payload
> lengths of 5 and 7 at the encoder so a regression fails loudly.

**Error frames**, undocumented upstream and confirmed on hardware. The device answers a bad
frame with `CMD | 0xC0` and a one-byte error code:

```
57 AB 00 C1 01 E4 A8      checksum error in response to GET_INFO
```

The link recovers immediately afterwards. Note that upstream's `decode()` rejects every one of
these (§3.1), which is why they are absent from its documentation.

**Malformed reply, confirmed on hardware.** An undefined command yields a five-byte frame with
**no checksum byte at all** — see §3.2. A parser must tolerate it without panicking.

**Unsolicited frames, confirmed on hardware.** The device pushes an unrequested `0x81` frame
about 15 ms after a lock-key state change, 4 times out of 4, and it can arrive *after* an
unrelated acknowledgement. Forty seconds of idle listening produced zero bytes, so the device
is otherwise quiet. **Replies must be matched by command byte, not by arrival order.**

**The receive parser has no inter-byte timeout** — see §5.1. A truncated write consumes the
following command.

Modifier bits: `LCtrl 1<<0, LShift 1<<1, LAlt 1<<2, LMeta 1<<3, RCtrl 1<<4, RShift 1<<5,
RAlt 1<<6, RMeta 1<<7`

Mouse button bits: `Left 1<<0, Right 1<<1, Middle 1<<2, Back 1<<3, Forward 1<<4`

Serial defaults: 57600 baud, 8N1, 500 ms read timeout. Confirmed: the baud setting is honoured
(other rates produce silence), and no handshake or initialisation beyond `GET_INFO` is required
to reach a working state.

`GET_INFO` response payload: `[versionChar, connectedFlag, lockBits, ...]` where
`version = 1.0 + (data[0] - 0x30) / 10` and `lockBits` carries NumLock (bit 0), CapsLock
(bit 1), ScrollLock (bit 2). Observed on this unit:

```
TX  57 AB 00 01 00 03
RX  57 AB 00 81 08 38 01 00 00 00 00 00 00 C4     version 1.8, connected, lockBits 0x00
```

Reply arrives at +3.98 ms, consistently. Note the lock bits originate in the *target's* HID
output report, so a lock-bit change is proof the target processed a keystroke — a cheap
end-to-end check that needs no video.

Keymap source to port: `browser/src/libs/keyboard/keymap.ts` is keyed on W3C UI Events
`code` values, which `winit`'s `KeyCode` also follows — a mechanical port. Character-to-key
tables for text injection are in `browser/src/libs/keyboard/charCodes.ts`, and belong to
§10.2 rather than the core.
