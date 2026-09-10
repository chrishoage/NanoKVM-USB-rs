# Stage 0 — Findings and exit review

Feasibility spikes per docs/NATIVE_CLIENT_PLAN.md §12. Hardware: the unit on this desk, driving a
Raspberry Pi 3B target running the Raspberry Pi OS desktop. Host: Arch Linux, kernel
7.2.2-arch1-1, niri 26.04 (8ed0da4) on Wayland, Rust 1.89.0.

Detail lives in the per-spike documents; this page is the exit review and the list of
places the plan needs amending.

**The per-spike documents below live on the `stage-0` branch**, together with the throwaway
spike crates that produced them. `main` carries this exit review and nothing else from Stage 0,
because everything durable has been folded into the design and the fixtures.

| Spike | Document |
| --- | --- |
| Wayland integration on niri | [`stage0/wayland.md`](stage0/wayland.md), [`stage0/wayland-manual-test.md`](stage0/wayland-manual-test.md) |
| Basic device communication | [`stage0/serial.md`](stage0/serial.md) |
| Capture | [`stage0/capture.md`](stage0/capture.md) |
| Packaging | [`stage0/packaging.md`](stage0/packaging.md) |
| USB topology and pairing | [`stage0/topology.md`](stage0/topology.md) |
| Upstream §3 re-verification | [`stage0/upstream-verification.md`](stage0/upstream-verification.md) |
| Decode benchmark | [`stage0/decode-bench.md`](stage0/decode-bench.md) |

## Exit criteria — met

§12 sets four conditions for leaving Stage 0.

| Condition | Status |
| --- | --- |
| The four feasibility questions answered | **Yes** — all four feasible, one with a changed shape |
| §11 questions 1–11 answered or carried forward | **Yes** — all eleven answered by measurement; none deferred |
| Primary capture path chosen | **Yes** — MJPEG at 1920x1080, 60 fps. There is no second option |
| Fixtures in hand | **Yes** — 135 captured frames, 22 validated packet examples, annotated traffic logs |

Nineteen amendments to the design came out of it, listed below. Four of them
(A1, A3, A17, A18) change decisions the plan had already made; the rest add
constraints or close open questions.

Stage 0 code is throwaway by design and lives in `spikes/`. Nothing in `src/` exists yet,
which is correct — that is Stage 1.

## The four feasibility questions

| Question | Verdict |
| --- | --- |
| **Wayland integration** | **Feasible.** winit stands. Shortcut inhibition, pointer lock and focus loss all work on niri. §1.5's smithay fallback is not needed. |
| **Basic device communication** | **Feasible.** `GET_INFO` alone reaches a working state; no init sequence, no mode set. Version 1.8, target reported connected. |
| **Capture** | **Feasible, with constraints.** MJPEG only, every advertised mode delivers its advertised rate, timestamps are usable. |
| **Packaging** | **Feasible, but not as a static binary.** glibc dynamic is the only workable shape. |

## §11 question status

| # | Question | Status |
| --- | --- | --- |
| 1 | Capture formats and model | **CONFIRMED** — MJPG only; Pro 4K60 unit |
| 2 | USB topology and iSerial | **CONFIRMED** — two devices, two buses; §8 needs rewriting |
| 3 | Absolute coordinate range | **CONFIRMED** by closed-loop test — 12-bit usable in a 13-bit field, divisor 4096 |
| 4 | Checksum behaviour | **CONFIRMED** — datasheet formula accepted; bad checksums get an explicit error frame |
| 5 | Decode benchmark | **CONFIRMED** — zune-jpeg clears 1080p60 with 6.9x margin; 4K60 is marginal |
| 6 | Wayland spike on niri | **CONFIRMED** — inhibit, lock and escape all work |
| 7 | Packaging | **CONFIRMED** — musl is a dead end; ship glibc |
| 8 | Handshake | **CONFIRMED** — nothing beyond `GET_INFO` needed, but the device does push unsolicited frames |
| 9 | Timestamp clock | **CONFIRMED** — monotonic, start-of-exposure, subtractable |
| 10 | Signal indication | **CONFIRMED NEGATIVE** — none exists on this device |
| 11 | Render blocking and thread placement | **CONFIRMED** — premise refuted, conclusion unchanged |

## Amendments the plan needs

Numbered so they can be worked through against the document.

### A1. §6 must drop the YUYV option

> "YUYV passthrough may be viable at lower resolutions and avoids decode entirely"

There is no YUYV. The device advertises exactly one pixel format, `MJPG`, confirmed by
`ENUM_FMT`, by the raw USB descriptors, and by `S_FMT` rejecting YUYV. JPEG decode is
therefore unconditionally on the critical path at every resolution, which promotes §11 q5
from a supporting measurement to a blocking dependency.

### A2. §8's evidence hierarchy is wrong for this hardware

§8's only item labelled proof — "both interfaces belong to the same USB device (identical
`busnum:devnum`)" — does not exist here. The video node and the serial node are separate USB
devices on separate buses, because the dongle contains its own internal USB 2.0 hub and the
video device enumerates at SuperSpeed.

The replacement is stronger than §8's "weak, suggestive only" port-path adjacency: the
kernel publishes SuperSpeed-to-HighSpeed port `peer` symlinks, and two peered ports are the
same physical connector by kernel assertion. Proposed order: same `busnum:devnum` → port
`peer` link → containment under the dongle's internal hub → explicit override.

Also demote `iSerial`. The video device reports `20210623`, a date, almost certainly a
model-wide firmware build date rather than a unit identifier.

### A3. §5.4's premise is refuted; its conclusion survives

§5.4 assumes `get_current_texture()` can block for up to a frame. It does not on this stack:
median 4 µs, worst case 771 µs over 800 frames. **`present()` blocks instead**, for a full
refresh period — about 6.9 ms of every 6.94 ms frame at 144 Hz, roughly 99 % duty.

So §5.4 option (b), "never block on acquire", fixes nothing, and §11 q11 resolves to option
(a): **render on its own thread**. The plan called this "the option that cannot be wrong,
only unnecessary"; it is measurably necessary.

Do not reach for `Mailbox` to dodge the block. It is non-blocking only because it does not
pace the client at all, and it free-runs at about 25 000 fps burning a core.

### A4. §5.3 is correct on every point, with one addition

`Immediate` is absent, forcing it is fatal, and `AutoNoVsync` degrades rather than panicking
— all confirmed. niri offers `[Mailbox, Fifo]` and nothing else.

The addition: forcing an unavailable mode panics at `Surface::configure` **only with wgpu's
default error handler**. An application that installs its own `on_uncaptured_error` gets a
silently unconfigured surface and the panic moves to the next `get_current_texture()`. Fatal
either way. Enumerate; never name a mode blind.

### A5. §6.1's "Signal loss" row is unreachable on this hardware

No control, no DV timings, no UVC extension unit, `v4l2_input.status` permanently zero, and
the UVC error bit never set. The vendor HID interface on the video device returns video
buffer contents, not status.

§6.1 already specifies the right fallback — "if the capture device offers no reliable signal
indication, do not synthesize one" — so the behaviour does not change. What changes is that
the three-condition table collapses to two on this unit, and S2-1 cannot be met as written.
Say so in §6.1 rather than leaving a row that can never fire.

### A6. New Stage 1 requirement — trust the JPEG header, not the driver

After an idle period the device reproducibly emits **up to eight consecutive frames at the
previous resolution**, carrying the new `sizeimage`, with no `ERROR` flag and no reliable
`sequence` tell. Reproduced 3 times out of 3 with an idle gap, 0 out of 10 without.

Frame dimensions must be read from the JPEG start-of-frame header on every frame. Never
trust `G_FMT`. This is cheap to honour from the first commit and expensive to retrofit,
which puts it in the same category as §6.1's S1 items.

### A7. New Stage 1 requirement — set the frame interval explicitly

`S_FMT` alone leaves 1080p and 720p at a **240 fps** default. `S_PARM` must be called
explicitly to select the frame interval, or the pipeline inherits a rate nothing asked for.

### A8. §3.1's runtime impact is broader than stated

The plan frames upstream's short receive checksum as validating "by accident" on a trailing
zero byte. That is the benign half. The other half: **a corrupted final payload byte passes
validation**, because the checksum never covers it. Strengthens §9.1's case for treating
upstream captures as fixtures rather than authority.

### A9. §3.3 is out of scope, not open

It concerns the Electron desktop build's misuse of `node-serialport`, which this port shares
no code with. Recorded as out of scope so nobody schedules a runtime confirmation for it.

### A10. §7.2's static-binary goal is settled: abandon it

A musl build succeeds and links nothing — static-pie, zero `DT_NEEDED`, no `musl-gcc`
required. It is also useless: a static musl binary has no dynamic loader, so every `dlopen`
fails and the program gets zero GPU adapters and cannot open a window. Dynamic musl fails
too, because glibc shared objects will not relocate under it.

§1.4 anticipated that a GPU binary is never fully self-contained. The measurement sharpens
it: musl can only absorb the three libraries every host already has, while making unloadable
the ones that could never be bundled anyway.

**"Single binary" is therefore defined as:** one dynamically-linked glibc executable, no
installer, no bundled runtime, no sidecar; linking only `libc`, `libm` and `libgcc_s`;
dlopening only the host's GPU driver and Wayland libraries; and requiring neither `libv4l2`
nor `libudev`. Recommended release target `x86_64-unknown-linux-gnu` built against an old
glibc, stripped, roughly 8 MB.

`libv4l2` is avoidable and confirmed avoided: the `v4l` crate's default `v4l2` feature is
bindgen over kernel headers with no link directive. Pin `default-features = false,
features = ["v4l2"]` so nothing re-adds it.

### A11. §5.1's serial ceilings measure the wrong thing

The baud setting *is* honoured — 9600, 38400 and 115200 all produce silence, and sustained
throughput measures 5861 B/s against the predicted 5760 — so §5.1's arithmetic is sound. It
is also not what limits anything, because this is CDC-ACM over USB rather than a real UART:
`write()` returns in about 50 µs regardless.

What matters is the acknowledged round trip, which is far worse than the byte arithmetic
predicts for the mouse:

| Report | §5.1 predicted | Measured 1:1 rate | Ack round trip |
| --- | --- | --- | --- |
| Keyboard | 411 /s | **332 /s** | 4.15 ms |
| Mouse relative | 575 /s | **91 /s** | 17.0 ms |
| Mouse absolute | 480 /s | **83 /s** | 17.0 ms |

Mouse reports are roughly six times slower than the plan assumes. **Overload provides no
backpressure — it silently corrupts**, producing bursts of `0xE4` checksum errors.

This changes the status of a §2 requirement. §5.1 presents coalescing and pacing as things
to implement and then measure, with the question left open as to whether serial is a real
bottleneck. At 83 absolute reports per second against a pointer that moves continuously, it
plainly is. **Mouse coalescing is a correctness requirement, not an optimisation**, and it
should not wait for Stage 2 instrumentation to justify it.

### A12. The device pushes unsolicited frames — §11 q8 answered, and it constrains the read path

CONFIRMED, 4 times out of 4: an unrequested `0x81` frame arrives about 15 ms after a lock-key
state change, and it can arrive *after* an unrelated acknowledgement. Forty seconds of idle
listening produced zero bytes, so the device is quiet unless something happens.

Consequence for `serial`: **replies must be matched by command byte, not by arrival order.**
A read path that assumes the next frame answers the last request will mis-attribute a lock
notification. This is a structural requirement on the reader, so it belongs in Stage 1.

Incidentally this also confirms the keyboard path end to end without needing video: lock
bits come from the *target's* HID output report, so the device reporting a lock change proves
the target processed the keystroke.

### A13. §3.2's short-buffer case is real traffic, not a hypothetical

The plan rates §3.2 as a guard bug that "fails safe" in JavaScript and would fault in a
stricter port. It is stronger than that: the device genuinely emits a frame the guard was
wrong about. The reply to an undefined command is

```
57 AB 00 FE 00        five bytes, no checksum byte at all
```

confirmed over a 10-second wait. The frame format requires a checksum byte even at `LEN 0`.
**A Rust port that indexes `frame[5 + len]` panics on a frame this device really sends.**
§9.2 item 5 already wants this fuzzed; it should also be a fixed regression case.

### A14. §3.1's impact, third widening: upstream cannot see errors at all

Success replies end in `0x00`, which is why upstream works, and §3.1's hypothesis is
confirmed byte-for-byte on the real `GET_INFO` response.

But error replies end in `0xE4` or `0xE5`. Upstream's short checksum therefore **rejects
every error frame the device has ever sent it**, leaving it unable to distinguish a reported
device error from silence. That is a much larger functional gap than "validates by accident".

The plan's appendix does not document error frames at all. They exist and have a shape:
`CMD | 0xC0` with a one-byte error code, e.g. `57 AB 00 C1 01 E4 A8`. The link recovers
immediately afterwards. Add them to the appendix.

### A15. The CH9329 receive parser has no inter-byte timeout

Send a header claiming `LEN 8` followed by only two payload bytes, and the chip keeps
waiting — tested out to 10 seconds — then consumes the *next* command as the remainder of the
truncated one. One partial write corrupts the following command.

This makes §2.6's "never abandon a partially written frame" rule load-bearing rather than
merely tidy, and it means recovery after a torn write needs an explicit resynchronisation
strategy rather than a reconnect-and-hope. Relevant to §2.7.

### A16. The device carries a second serial number, readable only over the link

`GET_USB_STRING` returns `Sipeed` / `NanoKVM-USB` / `BA1612624UJPW2RUJ`. That last string is
distinct from either USB `iSerial` seen in A2, and it looks unit-unique.

It does not help pairing, since reading it requires already having opened the serial link,
which is the thing pairing is trying to establish. It is useful for identifying a unit in
logs and for user-facing device listings in Stage 3.

### A17. The appendix's mouse report shapes are wrong — a mode byte is missing

The most consequential correction in Stage 0, because it makes the mouse not work at all.

The appendix documents the mouse payloads as the bare HID reports:

```
Mouse rel     4 B   [buttons, dx, dy, wheel]
Mouse abs     6 B   [buttons, xLo, xHi, yLo, yHi, wheel]
```

The CH9329 requires a **leading mode byte** in front of each:

```
Mouse rel     5 B   [0x01, buttons, dx, dy, wheel]
Mouse abs     7 B   [0x02, buttons, xLo, xHi, yLo, yHi, wheel]
```

Upstream does prepend it, but at the send site rather than in the report builder the
appendix was derived from — verified at `browser/src/components/mouse/relative.tsx:136`
(`sendMouseData([0x01, ...report])`) and `absolute.tsx:166` (`[0x02, ...report]`). Reading
only the builder loses the byte, which is exactly how the appendix came to be wrong.

**The failure mode is nasty: the device acknowledges the short form and does nothing.** A
protocol-level test that checks for an `ACK` therefore passes while the pointer never moves.
That is precisely why the serial spike's acknowledge-only sweep found no signal and had to
defer the range question, and it is a standing warning that on this device an `ACK` is not
evidence of effect.

Knock-on corrections:

- §5.1's wire-byte table undercounts mouse frames by one byte each: relative is 11 wire
  bytes, not 10; absolute is 13, not 12. The measured rates in A11 supersede these anyway.
- Stage 1 should assert payload lengths of 5 and 7 at the encoder, so a regression here
  fails loudly instead of silently doing nothing.

`fixtures/packets/ch9329.toml` has been corrected — 10 packets rewritten, a new
`[[disagreement]]` entry recording it, and all 22 entries re-validated.

### A18. §3.4 resolved — 12 bits usable inside a 13-bit field, divisor 4096

Answered by closed-loop test: send a coordinate, capture a frame, locate the cursor. Ninety
measurements, no outliers, fitting one law:

```
effective = min(v & 0x1FFF, 4095)
pixel     = floor(effective * extent / 4096)
```

- **Full scale is 4095 and the divisor is 4096.** 2048 lands dead centre, and the slope
  matches `1920/4096` and `1080/4096` to six decimal places.
- **4096 clamps to the far edge rather than wrapping.** The wrap is further out: **8192 maps
  to pixel (0, 0)**, because the field is 13 bits wide with only 12 usable.
- The appendix's documented `0 to 32767` is **refuted**. Upstream's `MAX_ABS_COORD = 4096` is
  the correct divisor, and its `1.0 → 4096` off-by-one is real but harmless, saved by the
  clamp.
- The same full scale applies to both axes, each mapped onto its own extent. No aspect
  correction happens in the device.

All three hypotheses in §3.4 were wrong as stated. The third, "the firmware clamps
internally", is closest but misses the 13-bit field and the wrap beyond it.

**For Stage 1:** clamp to 4095 in our own encoder and never rely on the device to do it, since
an overshoot past 8191 silently wraps to the origin — a pointer jump to the top-left corner
of someone's console. Map through the pixel *centre*, `((2*px + 1) * 2048) / extent`; the
naive `px * 4096 / extent` yields 4093 for pixel 1919 and leaves the last column and row
unreachable.

**Relative motion works**, with the standard convention: `+dx` is right and `+dy` is down, no
sign flip needed. Magnitudes do not survive the trip — 30 units produces 15 to 20 pixels,
varying — because the target applies pointer acceleration. **Relative motion can therefore
never be used to reach a specific coordinate**, which is worth stating since it is a tempting
shortcut.

### A19. §1.3 answered — zune-jpeg clears the bar at 1080p, and 4K60 is marginal

Measured on an AMD Ryzen 9 9950X3D, zune-jpeg 0.5.15, single thread, decoding straight into
a reused RGBA buffer. Standard deviation under 1 % of the mean, and two reruns plus a
cross-CCD run agree within 3 %.

| Mode | Median decode | Sustained | Margin at target rate |
| --- | --- | --- | --- |
| 1280x720 | 0.98 ms | 1018 fps | — |
| **1920x1080** | **2.40 ms** | 416 fps | **6.9x at 60 fps** |
| 2560x1440 | 4.16 ms | 242 fps | 1.7x at 144 fps |
| **3840x2160** | **10.01 ms** | 99.8 fps | **1.6x at 60 fps** |

So §1.3's question resolves in zune-jpeg's favour for the recommended default mode, and the
language decision needs no revisiting.

**The content caveat is measured rather than asserted.** The corpus is byte-identical frames
of an idle, low-entropy desktop, so these are a lower bound. Against synthetic frames 2.4x
the byte size, 1080p60 still clears comfortably at 4x, but **4K60 stops clearing** — 16.36 ms
against a 16.67 ms budget. This reinforces the capture spike's recommendation of 1080p60 as
the default from a second, independent direction. Real busy-screen frame sizes are CARRIED
FORWARD; instrument `bytesused` in Stage 1.

Three concrete Stage 1 decisions fall out:

- **Ask for RGBA, do not repack.** zune emits RGBA directly through a dedicated AVX2 kernel
  and it is free. Decoding to RGB and repacking on the CPU costs 34 % — 0.80 ms at 1080p,
  3.18 ms at 4K. A benchmark that measures only decode hides this entirely.
- **Enable strict mode.** A truncated frame otherwise decodes as `Ok` with a partial image.
  `set_strict_mode(true)` turns it into a proper error at no measured cost. Given A6's stale
  frames and A15's torn writes, silently accepting partial data is the wrong default here.
- **Exactly one decode thread**, as §4.1 already specifies. Extra threads raise aggregate
  throughput but never reduce per-frame latency, and §5.2's one-pending-frame rule leaves
  them nothing to decode.

Correctness was verified against libjpeg-turbo rather than assumed: PSNR 66 to 72 dB across
all seven modes, worst channel delta 3, which is IDCT rounding. The device's frames are
self-contained rather than abbreviated, carrying full Huffman and quantisation tables every
frame, so the classic capture-hardware MJPEG hazard does not apply.

§5.2's assumption that the compressed copy is cheap relative to decode is confirmed: 0.002 ms
at 1080p against 2.40 ms of decode.

Alternatives were evaluated and rejected. `jpeg-decoder` is 2.2x slower even with rayon, and
libjpeg-turbo is only 12 to 16 % faster while still marginal at 4K, which does not justify a
C dependency.

## Carried forward into Stage 1 and 2

- **Input latency versus render timing.** §5.4's acceptance test is behavioural and was not
  measured; the spike measured only the block. Stage 1 owns it.
- **Resynchronisation after a torn write.** A15 shows the chip waits indefinitely for the
  remainder of a truncated frame and then eats the next command. There is no strategy yet for
  recovering a link left in that state; §2.7's reconnect does not obviously cover it.
- **Overload behaviour has no backpressure.** A11 shows saturation corrupts silently rather
  than blocking. §2.8's overflow policy assumes a queue that fills; it should also account for
  a transport that accepts writes it cannot honour.
- **An acknowledgement is not evidence of effect** (A17). Worth carrying as a testing
  principle, not just a fact: hardware assertions in Stage 1 need to observe consequences,
  not acknowledgements.
- **HDMI unplug behaviour** (§11 q12) — untested, because the capture path is the only
  feedback channel from the target and could not be interrupted.
- **Mode-change effect** (§11 q13) — leans "no capture rebuild needed"; the device appears to
  rescale internally. Not confirmed.
- **The dongle on a USB 2.0-only port.** The pairing rule's fallback shape is reasoned but
  untested; the unit has only been observed on a SuperSpeed port.
- **VT-switch key combinations** are not inhibitable and will always reach the host.
- **Bindgen cross-architecture correctness** — the `v4l2-sys-mit` build script passes no
  `--target`.
- **Real busy-screen JPEG frame sizes.** A19's margins rest on an idle desktop. Instrument
  `bytesused` in Stage 1 to find out what a working target actually produces.
- **The no-GPU failure message.** A user whose driver fails to load needs a comprehensible
  error rather than a panic.
