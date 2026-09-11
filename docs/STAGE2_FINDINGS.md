# Stage 2 — Findings

Stage 2 is **Hardening** (plan §12): reconnect on both nodes, the §6.1 recovery behaviours,
`discovery` per §8, the §11 questions 12–15, and the open items §11 lists. This document is the
evidence index for what Stage 2 changed, in the same shape as `STAGE1_FINDINGS.md`: the
amendments the plan needs (C1–C14), the module and test inventory, the hardware numbers, and what
is still outstanding.

**Status at the time of writing:** every Stage 2 code item is built, reviewed and green
(`cargo fmt`, `cargo clippy --all-targets -- -D warnings` with and without `--features hardware`,
`cargo test` **451 passed, 1 ignored, across 29 test binaries**, `check-fixtures.py`,
`usb-replug.py --self-test`).

**All three measurements are now taken**, on 2026-09-11, against the Pi and the dongle on this
desk. The target reboot (§11 q12, S2-2) and the target resolution change (§11 q13, S2-3) produced
the same surprise — **the dongle never stops emitting frames** — written up as C12.

The kernel-side replug (S2-4 live, H-A4, H-B2) was taken last, under `sudo` in the user's own
terminal, and it did not confirm the mechanism — it found two real defects. Loss detection was
**write-driven**, so an idle writer never noticed a hang-up and the fd it kept renamed the device
(**C13**); and after a USB reset the dongle can come up in its default 640x480 mode with the
reopen's format commit lost, leaving the *stream* stuck while `G_FMT` still says 1920x1080
(**C14**). Both were fixed, adversarially reviewed and re-measured on hardware. §6 item 1 is the
run log and the numbers.

The Stage 2 exit criterion — "survives target reboots, replugs, resolution changes and overload
without stuck keys or restarts" — is therefore **claimed**.
Reboots and resolution changes are measured (0 restarts, 0 reopens, 0 disconnects, 0 capture
errors, the negotiated format never renegotiated, no gap over 1 s). Replugs are now measured on
**both nodes in both orders**, by the script alone, by H-A4 and H-B2, and by two live viewer runs:
the client noticed both losses — the serial one with nothing queued and no write attempted —
reopened both nodes **by identity** through §8 discovery across a renumbering, re-established the
negotiated video format, delivered its release-all, and still drove the target's CapsLock
afterwards. Nothing restarted and no key was left held.

> **Final confirmation run (18:16 UTC, 2026-09-11).** A last live viewer replug and reruns of
> H-A4 and H-B2 against the final binaries all passed. Live run 3: reopen came up at 640x480 for
> the third time in three, the watchdog restarted after 516.6 ms (32 frames) and the next frame
> was 1920x1080, serial reconnected in 3 attempts, release-all submitted on shutdown. H-A4:
> notice ≤ 95.6 ms, outage 781.3 ms for a 419 ms gone, 1 reconnect / 3 attempts, same node name
> back, CapsLock 20.2 / 23.0 ms. H-B2: node renumbered `video4`→`video5`, reopened by identity
> straight to 1920x1080 (0 format restarts, 0 escalation reopens), 4 mappings on the new node and
> 0 on the old. The exit claim above stands.

## What was built

| Module | Stage 2 change |
| --- | --- |
| `link.rs` | `LinkSource` (a source of transports, §2.7), `Link::resync` (the torn-write preamble, §5.1), and `Link::is_down` — a **non-writing** health query, default `false`, so a transport that can notice its own hang-up can say so without putting a byte on a live console's keyboard (C13). |
| `serial/` | `SerialLink::resync` (16 zero bytes in one `write_all`), `serial/source.rs`: `SerialLinkSource` whose unsolicited-frame receiver survives every reopen — and whose fixed-path justification is rewritten, because H-A4 falsified it (C13). `SerialLink::is_down` answers from the reader thread's own verdict: `down.is_some() \|\| reader_finished`, with `reader_finished` set from a **`Drop` guard** so a panicked reader reads as down rather than as a healthy link. `reader.rs`'s `Ok(0)` arm is a hang-up, not "no data this round". |
| `input/` | `spawn_with_source`, `ReconnectConfig`, the writer's reconnect run *as a §2.6 cancellation*: preamble → drain → release-all → `GET_INFO`, accepted as a whole or not at all. `ReleaseReason::Reconnected`, `Producer::wait_for_link`, `Stats` link fields as one consistent snapshot. Bounded shutdown while an open is in flight (helper-thread open, 5 ms poll). The idle wait in `next_entry` is **bounded at `LINK_HEALTH_POLL` (100 ms)** and asks `Link::is_down` on every wake; `note_link_lost` routes the answer through the same §2.6 cancellation run as a write failure, so the stale fd is dropped in `acquire` before any backoff (C13). |
| `capture/` | `SourceOpener`, `FrameSource::restart`, `V4l2Opener`, `Pipeline::start_with_opener`: disconnect → drop under `catch_unwind` → reopen with doubling backoff; stall → `restart()` every `restart_after`; `PipelineState::Reconnecting`; per-frame resolution-change accounting; fresh `G_FMT`/`G_PARM` queries. `stop()` bounded to one in-flight source call + 2 ms. **The negotiated-format watchdog** (C14): `FrameSource::negotiated_dimensions` (read once per device open), `format_mismatch_grace`/`format_mismatch_frames`/`format_mismatch_restart_limit`, the bounded restart ladder, the one escalation reopen, acceptance, and the `format_mismatch_restarts`/`_reopens`/`_accepted` counters in `PipelineStats`. |
| `discovery/` (new) | §8 pairing over `Sysfs`/`NodeProbe` traits: same-device, port-`peer`, and the degraded internal-hub rule (all three vendor ids required). Inventory listing, constraints, `reopen.rs`: `DiscoveringLinkSource`/`DiscoveringOpener` that re-run discovery on every reopen unless a path was given explicitly. |
| `viewer/` | `title.rs` (pure title composition), §2.8 notices for overflow and link loss, `Reconnecting` and "serial DOWN, reconnecting (Ns, M attempts)" wording, two-line stats (now carrying `format restarts N (accepted M)`). The C5 fragment `video WxH, negotiated WxH not established` while a mismatch is accepted **and** still mismatched. `app.rs` logs every title transition at info (`title: …`) — added so a live replug run leaves a transcript of what the user saw rather than a claim about it. |
| `main.rs` | `--serial`/`--video` optional, `--list-devices`, `spawn_with_source` + `wait_for_link(2 s)`, `start_with_opener` with fail-fast first open. |
| `examples/` | `capture-probe` (the q12/q13 instrument), `type-keys` (scripted keyboard sender with release-all on every exit path including SIGTERM/SIGHUP; tests under `tests/type_keys.rs`). |
| `scripts/` | `usb-replug.py` (kernel-side unplug/replug of the dongle by `USBDEVFS_RESET` or driver unbind/rebind, 21-case self-test, refuses anything that is not the dongle by descriptor), `snapshot-sysfs.py`, `device-health.py` aligned with the Rust evidence wording. |
| `fixtures/sysfs/` | Four sysfs trees: `usb2-desk` (captured), `usb3-stage0` (reconstructed from the Stage 0 topology evidence), `usb3-rootport`, `two-dongles` (synthesized, `synthesize.py` is the audit trail). **The trees are build output and gitignored.** What is committed is two files: `usb2-desk.sysfs`, the 2026-09-10 recording of this desk serialised to one line-oriented text file, and `synthesize.py`, which expands it, writes the reconstructed bus-5 negative control into it and builds the other three trees from it. `nanokvm::discovery::testing::fixture` runs the script on demand the first time a tree is missing (once per process, under a lock file), so **`python3` is a test-time dependency of this crate**. Format and re-recording: `fixtures/sysfs/MANIFEST.md`. |
| `tests/` | The two replug hardware tests reopen **by identity**, not by name: H-A4 builds a `DiscoveringLinkSource` and H-B2 a `DiscoveringOpener` over `NodeResolver::real`, both re-running §8 on every attempt, and both print the path either side so a rename is recorded rather than fatal (C6, C13). H-A4 watches the outage with a 2 ms sampler thread started *before* the script and observes the transition through a monotone counter, because `link_down` is an edge a level poll started afterwards cannot see. H-B2 asserts the frames settle at the negotiated size within a window **derived from the watchdog's own config** (≈11 s at the shipped defaults) and prints the mismatched-frame counts. `tests/input_idle_loss_review.rs` keeps the slice-F review's nine tests as regression spec. |

Tests: 287 (Stage 1) → **451**, all non-hardware, plus 12 `#[ignore]` hardware tests
(`serial_reconnect_hardware` ×5, `capture_recovery_hardware` ×2, `discovery_hardware` ×1, and the
Stage 1 four). Every slice went through an adversarial review that had to demonstrate each finding
with a failing test, then a fix pass; each review demonstrated its findings with a failing test.

## Amendments the plan needs

### C1. §2.7 as written wedges input if the device never comes back

A cancellation trigger that lands while the writer is waiting out the reconnect backoff must be
serviced *there*. Otherwise `requested_epoch > acked_epoch` for as long as the device is absent,
the flag stays set, and `engage()` never succeeds again — §2.6.1's wedge by another route. The
writer's backoff wait now checks the flag every wake and runs the cancellation sequence (release
`Unsent`, ack advanced) before going back to waiting. Pinned by `tests/input_reconnect.rs`.

**Corrected by C13 in one place:** C1 as first written assumed the outage always begins with a
failed write. It does not — a link can die with nothing queued — so the idle-discovered loss is
routed into *this same* run (`note_link_lost` sets `link_down` before it raises the `LinkDown`
trigger, so the cancellation is serviced before `acquire` is reached). One idle outage therefore
costs exactly the same bookkeeping as one write-driven outage: one `Unsent` release, one advanced
ack, one reconnect. Pinned by `tests/input_idle_loss_review.rs`.

§2.7 should also say what "resume" means: the producer stays **disengaged** after a reconnect.
§2.8's deliberate-recapture rule applies — the user's next grab is what re-engages, and it waits on
the reconnect's acked epoch like any other.

### C2. §5.1's torn-write resynchronisation is a 16-byte zero preamble, and it is measured

Before the release-all on any freshly opened link, `SerialLink::resync` writes 16 zero bytes in one
`write_all`. Reasoning: a frame is `57 AB ADDR CMD LEN DATA[≤8] SUM`, so at most 10 bytes complete
a frame torn after `CMD`; `0x00` can never be a header byte, so the surplus is discarded by a
parser hunting for `57 AB`; and a frame completed with zeros carries the wrong checksum in 255
cases of 256 (rejected with `0xE4`), while the 256th applies an all-released keyboard or a
no-motion, no-button mouse report — both made moot by the release-all that follows.

Measured on the real chip (`tests/serial_reconnect_hardware.rs`, output in
`a-serial-report.md` §3):

| | Result |
| --- | --- |
| H-A1 preamble on a healthy link | 40–67 µs to write; `GET_INFO` before and after identical; no unsolicited frame, no error frame, `bad_checksum 0` |
| H-A2 torn frame (`57 AB 00 02 08 00 00`) + preamble | `GET_INFO` answered in **7.86 ms**; the chip pushed exactly the predicted `0xC2`/`0xE4` |
| H-A3 negative control: torn frame, **no** preamble | `GET_INFO` **timed out at 500.11 ms** with nothing pushed — §5.1 confirmed; the preamble then freed it (54 ms) |

The reconnect sequence treats the `GET_INFO` reply as the proof the parser is realigned; a new
link whose `GET_INFO` fails is dropped and the next attempt made. This closes §11's "no strategy
yet" item.

### C3. §8's evidence item 3 is not proof, and the ids around it are all commodity parts

The desk's dock proves the point: a Logitech webcam (`046d:086b`) and an unrelated CDC-ACM
device (`043e:9a8a`) sit as direct children of one generic hub (`0bda:5411`), and the literal
wording of item 3 ("containment under a shared hub") would have paired them. The implemented rule
is narrower: the common ancestor must be a `1a40:0101` hub **and** the video device `345f:2133`
**and** the serial device `1a86:55d3`, all as direct children. It is reported everywhere as
`internal hub (degraded)`, never as proof, and the client logs a warning naming
`--video`/`--serial` when it pairs on it. A different unit with a different hub id degrades to
`NoPair` plus a listing, and still auto-pairs on SuperSpeed, where the kernel's `peer` link needs
no ids at all.

Two more facts §8 should carry:

- **The video device's `iSerial` is not stable on one unit.** Stage 0 read `20210623` on
  SuperSpeed; today it reads `20210621` on High Speed. §8's residual use for it ("pin one node
  across replug") does not hold for the video device.
- **The product string changes with the link speed** — `USB3 Video` on SuperSpeed, `USB2 Video`
  on USB 2.0, same unit, same `345f:2133`. Nothing may match on it.

`/dev/video5` is the UVC metadata node and is indistinguishable in sysfs; only the per-node
`device_caps` separates it (`0x04200001` capture vs `0x04a00000` metadata — the per-device
`capabilities` field is `0x84a00001` on both). Discovery probes with one read-only `QUERYCAP` and
only on nodes that already have a topological pairing candidate, so the user's webcam is never
opened.

### C4. §6.1 recovery: what the two conditions do now, and the edges the review found

- **Disconnect** (`ENODEV`, `POLLHUP`, or a panic in the `v4l` destructors): state
  `Disconnected` → the source is dropped under `catch_unwind` (the mmap arena is released; checked
  against `/proc/self/maps` in H-B2) → `Reconnecting` with a doubling backoff (500 ms to 5 s) →
  `Running` on the first successful open. The decode thread and the output slot are never
  touched, so the renderer keeps the last image (S1-2).
- **Stall** (no frame for `restart_after`, default 2 s): `FrameSource::restart` — `STREAMOFF`,
  re-prime, `STREAMON` — then keep polling, again every `restart_after`. Measured cost on the
  device: `restart()` **8.7 ms**, return to first new frame **73.5 ms**, about five frames at
  60 fps. Whether 2 s is the right interval for a target that is off for a minute was to be
  decided by the reboot measurement; the reboot **never stalled** (C12), so the interval is still
  the argued 2 s and this unit has not produced the event it exists for (§6 item 2).
- A `restart()` that **panics** is a failed restart, not a lost device (the review found it was
  being recorded as a disconnect, which on the `Pipeline::start` path the binary used would have
  made every no-signal period terminal).
- `last_error` means "the last thing that went wrong, until the device was reopened"; it is
  cleared by a successful reopen and deliberately not by a good frame, because the device can
  deliver frames and refusals interleaved (the oversized-header test).
- `stop()` is bounded to one in-flight source call plus 2 ms; a blocking `open()` runs on a
  helper thread and its result is dropped if the pipeline is stopping. Residual: that open
  completes after `stop()` returns, so the node is released slightly late; nothing waits on it
  and no frame from it can reach a consumer.
- **A third condition joined these two after the replug measurement**: a device that is present
  and streaming, and streaming the *wrong size*, with the reopen's format commit lost. It has its
  own evidence (the SOF header against `S_FMT`), its own bounded remedy and its own counters —
  C14. It is not a disconnect and not a stall, and it is deliberately not worded like either.
- Resolution changes need **no capture rebuild** for the decoder or renderer: the decoder's
  scratch buffer resizes both ways (`decoder.rs`), the renderer recreates its texture on a size
  change (`render.rs`), and the letterbox and absolute-pointer mapping follow. Whether the
  *negotiated* UVC format changes (q13) is now **measured: it does not** — the device rescales
  internally, and neither `G_FMT` nor the SOF dimensions move (§6 item 3). Nothing needs writing;
  the per-frame accounting stays as a safety net for hardware that behaves differently, and if
  such a unit turns up the renegotiation belongs in `restart()` or a reopen, not a third path.

### C5. §2.8 "surface it" needs words in the title, not a counter

Stage 1 handled an overflow by releasing the session and logging a warning, after which the
title said "[click or Enter to capture]" — a silent counter by another name. The reducer now
raises a one-shot notice, `input interrupted: queue overflowed — click to re-capture`, and
`input stopped: serial link down` for link loss, both cleared on the next successful engage. The
`release UNSENT — target may still hold keys` notice is cleared by the reconnect's `Submitted`
release, pinned by a test on the pure `ReleaseNotice`.

### C6. Node names are not identities (and CLAUDE.md now says so)

During Stage 2 the user's dock was unplugged and replugged: `/dev/video0`–`3` and `/dev/ttyACM0`
vanished, bus 5 renumbered from `5-1.4.4.4` to `5-1.1.1`, and they came back later. Had the
dongle been replugged in that window it would have taken `/dev/video0` and `/dev/ttyACM0`. Every
rule that said "never touch `/dev/video0`–`3`" was therefore keyed on the wrong thing.
`CLAUDE.md` now identifies the dongle by `1a40:0101` → `{345f:2133, 1a86:55d3}` and the user's
hardware by *bus 5*; `usb-replug.py` identifies its target by sysfs port path and verifies the
device descriptor on the usbfs node it is about to reset; hardware tests that hardcode
`/dev/video4`/`/dev/ttyACM1` are required to say so in a comment.

**Amended by C13, twice.** First, the two *replug* tests no longer hardcode anything: H-A4 and
H-B2 reopen by identity through `DiscoveringLinkSource`/`DiscoveringOpener` and use the constant
only as the expectation they print. (H-A1..H-A3, `capture_hardware` and `capture-probe` still take
a name, which is honest — they neither take the device away nor expect it back — and they stay on
"Carried forward".) Second, and the sharper half: C6 was written from a dock replug and read as
"names *can* move". A node name comes back only if it is **free**, and it is free only if nothing
still holds the fd. The idle writer held `/dev/ttyACM1` across the reset, so `acm_port_destruct`
never ran, the tty index was never released, and the device returned as `/dev/ttyACM2` and stayed
there; the capture node renumbered `video4`→`video5` on **both** H-B2 runs even though the
pipeline drops its fd within milliseconds of the disconnect. Every same-name replug this project
has recorded was a race the client happened to win, not a property of the desk.

### C7. The bare Super tap has a consequence on this target

Stage 1 carried forward that a compositor-initiated release (niri handling `Mod+Escape`) reaches
the target as Super-down then release-all, and noted it "harmless on the Pi desktop as observed".
It is not harmless: a frame captured from the target during Stage 2 shows the Raspberry Pi OS
**application menu open**, which is what a lone Super tap does there. Slice A's analysis
(`a-serial-report.md` §5) argues the fix belongs in the **viewer**, not the writer: a deferred
lone-modifier press in `input` would violate §2.2's barrier rule for every chord and add its hold
window to every modifier's latency. The viewer can instead treat a modifier press as pending until
either a non-modifier key follows (send both, in order) or a release/focus-loss arrives (send
neither). Not implemented in Stage 2; recommended for the next input pass, with a
Mousepad-and-menu frame captured this session as the evidence.

### C8. §5.1 decision: serial is ack-bound, not baud-bound — q15 is closed as unnecessary

Stage 1's live runs settled the question §12 Stage 2 asked: at the measured 83 absolute reports
per second the raw pointer rate exceeds the link, but coalescing absorbs it (≈9:1 during motion,
queue depth 3, 39 ms max-in-queue, zero overflows over nine minutes of use). The limit is the
acknowledged round trip (17 ms per mouse report), not wire time (`write()` returns in ~50 µs at
57600 baud). Raising the baud rate would not move the bottleneck. **`SET_PARA_CFG` persistence,
stock-client compatibility and recovery of a mis-set device (q15) are therefore not investigated**
and baud reconfiguration stays out of the client entirely. The two-line stats in the viewer carry
`max-in-queue`, `coalesced`, `overflows` and the reconnect counters so a future regression is
visible rather than mysterious.

### C9. q14 rollover is closed by decision, not measurement

§2.5's suppression rule does not depend on how the CH9329 handles a rollover report, and no part
of Stage 2 adopted rollover reporting. The question is closed as "not needed unless §2.5 changes";
nothing was sent to the chip to answer it.

### C10. A `GET_INFO` timeout arms the reader's late-reply slot

Found while writing H-A3: after a `GET_INFO` times out, the reader diverts the *next* `0x81`
frame as a late reply, so the following `GET_INFO` on the same link also times out. It is a
Stage 1 reader property, correct for its purpose (a late reply must not be mis-attributed), and it
does not affect the reconnect path (new link, fresh matcher). Callers that retry `GET_INFO` on a
link that just timed out should expect one more timeout.

### C11. `max_barriers = 256` is the wrong kind of bound

256 barriers at a 100 ms `transact_timeout` is 25.6 s of backlog before §2.8 fails the session.
Input that stale should not be delivered; the useful bound is on *time in queue*, which the
counters already measure. Unchanged in Stage 2 (no overflow has ever been observed live);
recommendation recorded in `a-serial-report.md` §5.

### C12. §6.1 S2-2 has the failure shape wrong: frames do not stop on a target reboot

S2-2 says "frames stop and later resume, possibly at a different target resolution". Measured
(`probe-reboot.log`), neither half happens on this unit. Across a full `systemctl reboot` —
HDMI signal gone for ≈46 s — the dongle emitted **60 fps without interruption**, every frame
1920x1080, `G_FMT` constant `MJPG 1920x1080`, `gaps over 1s: none`. With no signal it emits a
fixed full-frame picture (`bytesused` exactly 97811 both before and after the boot console, so
it is one stored image, not noise); the Pi's boot console is not natively 1080p and arrived
rescaled to 1920x1080 like everything else.

So the real failure shape is: **four truncated JPEGs** (`jpeg decode failed: Exhausted data in
the image`) at the signal transitions, and a one-second dip to ≈56–58 fps at each. The stall path
is never entered, nothing is restarted, nothing is reopened, no `capture error` is raised, and no
resolution change is counted. §6.1 should say that a signal-loss event on this hardware is
indistinguishable from a picture change except for those torn frames — which is A5's
"never infer signal state from pixels" arriving from the other direction.

What the torn frames cost the user: the decode thread counts the error, records it in
`last_error`, logs it at debug and **does not write the output slot** (`pipeline.rs`, the `Err`
arm of `decoder.decode`). So the renderer keeps the previous decoded image for that frame period —
one stale frame of ~17 ms at 60 fps, 4 of them in 14 378. That is the right behaviour and no
change is proposed; it is recorded so that "decode errors: 4" is never read as lost seconds.

The same holds for a mode change (S2-3, C4): see §6 item 3.

### C13. §2.7 loss detection was write-driven, and an idle link never noticed a hang-up

**H-A4 failed on its first run, and it could not have passed.** The writer had submitted nothing
since startup, so when `usb-replug.py` reset the serial device the writer sat in `next_entry`'s
**unbounded** `wait` for the whole 30 s of the test with `link_down` still clear. `Stats::link_down`
had exactly one live writer — a *failed write* (`Shared::mark_link_down`, reached only from the
first-open failure and from the `Down`/`Io` arm of `write_frame`) — and the `Link` trait
(`src/link.rs`) carried no health query at all, only `transact` and `resync`, both writes. A link
that died with nothing queued was undetectable by construction, and the sibling non-hardware test
had recorded the defect as though it were the design: "the writer only learns the port is gone
when it next writes to it".

**The transport knew within one poll.** `serialport` polls before it reads and converts `POLLHUP`
into `BrokenPipe`, so the reader thread marked the `SerialLink` down in milliseconds and exited.
That verdict stopped at the `SerialLink`: nothing carried it across to the input writer's `Shared`,
and `SerialLink::is_down` was invisible behind `Box<dyn Link>`.

**And the fd the idle writer kept renamed the device.** `Writer::link` is cleared only in
`acquire`, which runs only once `link_down` is set. While the writer held the hung-up
`/dev/ttyACM1`, `acm_port_destruct` never ran, the tty minor was never released, and the dongle
re-enumerated as **`/dev/ttyACM2` — permanently**. Two things follow:

- `SerialLinkSource`'s own justification for reopening a fixed path — "the kernel gives the CH9329
  bridge the same node back across a replug on this desk" — is **falsified**. The kernel hands back
  the lowest *free* name, and it is free only if nothing still holds it. The doc comment at
  `src/serial/source.rs` now says exactly that, and says the identity-based reopen is
  `DiscoveringLinkSource`. `SerialLinkSource` is the explicit-`--serial` case and only that.
- The test that reopened `/dev/ttyACM1` by name could never have recovered, whatever the writer
  noticed. See the amendment to C6.

**The fix (slice F; the review's tests are in `tests/input_idle_loss_review.rs`):**

- `Link::is_down` — a health query on the trait, default `false` ("as far as I know, fine"), which
  **must not write, must not block and must not wait on the device**. It is emphatically not a
  keepalive: the device acknowledges anything (§3.4, A17), so traffic would prove nothing the
  transport's own read side does not already know, and on a live console it would put bytes on the
  target's keyboard to find out.
- `SerialLink::is_down` answers `down.is_some() || reader_finished`. `reader_finished` is the
  strictly stronger signal and is set from a **`Drop` guard**, not from the bottom of `run`, so a
  reader thread that *panicked* reads as down. Without that, `down` stays `None`, the link reports
  healthy for ever, and every later `transact` burns its full timeout and is classified `Timeout` →
  degraded rather than `Down` — input dead while the client says the link is up.
- The reader's `Ok(0)` arm is a hang-up (`mark_down`, then end the thread), not "no data this
  round". It is unreachable through `serialport` today, which returns `BrokenPipe` first; as
  written it was an unthrottled spin on a core.
- `next_entry`'s wait is bounded at `LINK_HEALTH_POLL` = **100 ms** and asks `is_down` on every
  wake. 100 ms is a tenth of the second §2.8 wants the fact visible within, and costs an idle
  thread ten mutex acquisitions a second; a submission or a trigger still wakes it immediately, so
  this is a floor on *discovery*, never on latency.
- The loss is routed through the **same §2.6 cancellation run as a write failure**:
  `note_link_lost` sets `link_down` *before* it raises the `LinkDown` trigger, so `run` services
  the cancellation (release `Unsent`, ack advanced — C1) and `acquire` clears `self.link` as its
  first statement, before any backoff. The stale fd therefore goes while the kernel is still
  re-enumerating, which is what lets the node come back under its own name at all.
- A Stage 1 (`spawn`, `reconnect == false`) writer reports the loss **once** and then stays quiet —
  no epoch spin at 10 Hz — and deliberately does *not* drop the fd: freeing the tty index is a §2.7
  concern, and a `spawn`ed writer has no source to reopen from. One more reason `main.rs` builds
  its writer with `spawn_with_source`.
- Both replug hardware tests reopen by identity, and H-A4 observes the outage with a 2 ms sampler
  thread started *before* the script runs. The pattern it replaced — a level poll of `link_down`
  after `usb-replug.py` returns — could not see an edge the repair had already cleared, and the
  "notice latency" it printed was measured from after the outage, so it read ≈0 whether the
  mechanism worked or not.
- `FakeLink::hang_up` now **records the calls it refuses**. It used to swallow them before pushing
  to `calls`, so "the writer discovered the loss without writing" was an assertion no test could
  actually make: silence and a swallowed write looked identical.

**H-A4 after the fix — passed** (user's terminal, under `sudo`, 2026-09-11):

| | |
| --- | --- |
| Serial node gone | 414 ms, **same name back** (`/dev/ttyACM1`) — the fd was dropped in time |
| Notice latency, idle writer, nothing queued, no write attempted | **≤ 95.8 ms** (upper bound: the clock starts at the script's "+5 ms gone" stamp) |
| `link_down` true → false | **781.6 ms** |
| Reconnects / attempts / cancellations | 1 / 3 / 3, `queue_discarded` 0, last release `Reconnected`/`Submitted` |
| CapsLock on the target afterwards | lock bit true after **19.5 ms**, false after **23.2 ms** |

781.6 ms for a 414 ms outage is the backoff ladder behaving exactly as designed, not slack — the
arithmetic is in §6 item 1, under `ReconnectConfig::default`.

### C14. §6.1 S2-4: after a USB reset the device can come up in its default 640x480 mode with the reopen's commit lost — the stream is stuck, not the device

**The reopen worked and the video was still wrong.** In the first live viewer run through a `both`
replug (2026-09-11, 17:28 UTC) the pipeline noticed the disconnect at once, discovery found the
node under its new name (`/dev/video5` → `/dev/video4`), and the reopen was a *full* open —
`open()`, `S_FMT` MJPG 1920x1080, `S_PARM` 60 fps, `REQBUFS`, four `QBUF`s, `STREAMON` — logged as
"reopened after 2 attempt(s)". The first buffer out of it carried the V4L2 `ERROR` flag, the SOF
said 640x480 at sequence 1, and **the stream stayed 640x480 at 60 fps for the remaining two minutes
of the run** while `G_FMT` went on reporting 1920x1080.

**It is the stream, not the device.** A fresh `v4l2-ctl` open of the same node immediately
afterwards got 1920x1080 for 30 frames out of 30. The dongle powers up in its default 640x480 mode;
the reopen's UVC probe/commit was lost on a device that had re-enumerated about half a second
earlier, and `G_FMT` kept answering 1920x1080 because that is the *driver's* record of what was
asked for, not the device's account of what it is sending. A6 already makes the JPEG start-of-frame
header the only authority on a frame's size; this is the failure that makes the difference matter
rather than a technicality.

**Intermittent — and CLAUDE.md's benign transient is the same event.** The live viewer hit it 2/2;
H-B2 hit it 0/2 (the first H-B2 run saw a few 640x480 frames and then 1080p, the second was already
1080p on the first post-reopen frame). A6's "the device emits up to eight frames at the previous
resolution after an idle period" is this same lost-and-late commit in its benign form. C14 is that
transient never ending, and nothing tells the two apart at the first frame — which is why the
response has to be bounded in both directions.

**The fix (slice G): a negotiated-format watchdog in `src/capture/pipeline.rs`.** After any stream
start the pipeline reads `FrameSource::negotiated_dimensions` once (what `S_FMT` committed to — not
a source of frame dimensions, the other half of a comparison) and compares every frame's SOF
against it. The watchdog fires only when **both** gates are passed:

| Gate | Default | Why |
| --- | --- | --- |
| `format_mismatch_frames` | **12** consecutive mismatched frames | A6 bounds the benign transient at *eight frames* — a bound in frames, which converts to a time only at the rate it was converted at. A time-only 500 ms grace sits three times over the transient at 60 fps and *under* it at 10 fps, where eight benign frames take 800 ms and buy a restart at the worst possible moment. A count above eight makes the transient un-triggerable at any frame rate, which is what "a stream that is merely slow is not restarted for being slow" actually requires. |
| `format_mismatch_grace` | **500 ms** since the first of them | Patience with a device that is settling: the same fresh open that showed 640x480 at sequence 1 showed 1920x1080 by sequence 4. |

The remedy ladder, bounded at every step:

1. `restart()` — `STREAMOFF`, re-prime, `STREAMON` on the same fd, which re-commits the format — at
   most `format_mismatch_restart_limit` (**3**) times, spaced by a doubling wait (`grace`,
   `2·grace`, `4·grace`).
2. Then **one** escalation reopen through the `SourceOpener` — a full `S_FMT`/`S_PARM`/`STREAMON`
   on a fresh fd, counted both as a reopen and in `format_mismatch_reopens` — with one fresh
   restart budget.
3. Then **acceptance**: the SOF dimensions are the truth (A6), logged once at warn and counted in
   `format_mismatch_accepted`. A unit that genuinely does not rescale internally would legitimately
   deliver a different size for ever, and it is never restarted in a loop.

A frame at the negotiated size clears the clock, the frame count and the acceptance and refills the
budget; a reopen and that one escalation refill it; **a restart never refills its own budget**, or
the bound would be no bound. A *stall* restart (`restart_after`) deliberately leaves the watchdog's
evidence alone, so an intermittent stream stuck in the wrong mode cannot have its case wiped out
over and over. While a mismatch is accepted **and** still mismatched, the title carries
`video 640x480, negotiated 1920x1080 not established` — C5's rule, words rather than a counter —
and both halves are required, so the counter cannot keep saying so after the device comes right and
the benign transient cannot flag it on every reopen. `format restarts N (accepted M)` is in the
viewer's stats line and the probe summary.

**Why the weaker remedy goes first.** A `restart()` is strictly less than the reopen that had just
failed — no `S_FMT`, no `S_PARM`, the same fd — and the adversarial review's first finding was
exactly that: escalate, because a restart cannot plausibly fix what a full open did not. The live
evidence refuted it. In the second live run (17:45 UTC) the reopen came up at 640x480 again, the
watchdog restarted the stream after **516.7 ms** of wrong-size frames, and the **very next frame
was 1920x1080**. One restart, fixed. The mechanism is most likely timing rather than strength: the
reopen lands about half a second after re-enumeration, while the device is still settling, and a
`STREAMON` half a second later re-commits. That is also the argument for the escalation being the
*same* reopen, later — so it was adopted anyway, behind the restart, where it costs nothing unless
three restarts have already failed.

**This closes the last capture-side gap in §6.1.** Disconnect (S2-4), stall (S2-1), and now a
device that is present, streaming, and streaming the wrong thing: each has its own evidence, its
own bounded remedy, its own counter and its own words in the title, and none of the three is
unbounded or worded like either of the others.

## Hardware numbers (this desk, USB 2.0 link, CH9329 v1.8)

| Measurement | Value | Where |
| --- | --- | --- |
| Zero preamble write | 40–67 µs | H-A1/H-A2 |
| `GET_INFO` after preamble | 6.7–7.9 ms | H-A1/H-A2 |
| Torn frame without preamble | `GET_INFO` timeout at 500.11 ms | H-A3 |
| Initial serial commissioning (open + preamble + release-all + `GET_INFO`) | 25.4 ms, attempt 1 | live run |
| Stream `restart()` | 8.7 ms; first frame 73.5 ms after return | H-B1 |
| Capture, 1080p60, idle desktop | 59.8–60.0 fps, `bytesused` 199 656 constant, `G_FMT` stable across 10 queries | probe |
| Capture-to-submit age | p50 20.7 ms | live run |
| Discovery on this desk | `/dev/video4` + `/dev/ttyACM1`, `internal hub (degraded)` | `--list-devices`, `discovery_hardware` |
| Target reboot, desktop outage | **≈46 s** (desktop lost t≈11 s, desktop back t≈57 s of a 240 s run) | `probe-reboot.log` |
| Target reboot, frames | never stopped: 60 fps throughout, 14 378 frames, `gaps over 1s: none`, SOF 1920x1080 and `G_FMT MJPG 1920x1080` constant | `probe-reboot.log` |
| Target reboot, pipeline events | 0 stream restarts, 0 disconnects, 0 reopens, 0 capture errors, 0 resolution changes | `probe-reboot.log` |
| Target reboot, damage | **4** decode errors, all `Exhausted data in the image`; one-second fps dips to 56.5–57.8 at t≈13, 16, 25, 26 s | `probe-reboot.log` |
| 720p mode change (18 s window) | `G_FMT` and SOF both constant `MJPG 1920x1080`; 0 resolution changes, 0 restarts, 0 reopens, 0 capture errors; **3** decode errors; dips to 55.8–56.9 fps at each transition | `probe-resolution.log` |
| Steady-state control, 240 s | 59.7–60.8 fps, one `bytesused` plateau (176 845 → 178 945), 0 decode errors, 0 events of any kind | `probe-reboot-control.log` |
| Replug, script alone: video device reset | node gone +60 ms, **back 525 ms**, devnum kept, same names | `usb-replug.py video --method reset` |
| Replug, script alone: serial device reset | gone +5 ms, **back 404 ms**, devnum kept | `usb-replug.py serial` |
| Replug, script alone: hub (`dongle`) reset | all three gone +5 ms; video **back 940 ms**, serial **back 1416 ms**, both with new devnums | `usb-replug.py dongle` |
| Replug, script alone: `both` | video gone +5 ms back 445 ms; then serial gone +5 ms back 424 ms | `usb-replug.py both` |
| Replug, script alone: `both-reversed` | serial gone +5 ms back 409 ms; then video gone +61 ms back 496 ms | `usb-replug.py both-reversed` |
| H-A4 notice latency, idle writer, nothing queued | **≤ 95.8 ms** (upper bound from the script's "+5 ms gone" stamp) | H-A4 |
| H-A4 serial outage | node gone 414 ms → `link_down` true→false **781.6 ms**; 1 reconnect, 3 attempts, 3 cancellations | H-A4 |
| H-A4 CapsLock after the reconnect | target lock bit true **19.5 ms**, false **23.2 ms** after the press | H-A4 |
| H-B2 capture reopen | disconnect → 30 frames again **1.17 s**; 1 reopen in 1 attempt, 0 restarts; 4 mmaps on the new node, 0 on the old | H-B2 ×2 |
| H-B2 node renumbering | `video4`→`video5` (capture), `video5`→`video6` (metadata), **both runs**, despite the pipeline dropping its fd within ms | H-B2 ×2 |
| Live replug, serial | down **769 ms**, 3 attempts — attempt 1 correctly failed (§8 found no pair while the node was gone) | live run 1 |
| Live replug, capture | reopened in **2 attempts**, node `video5`→`video4`, rediscovered by §8 | live run 1 |
| Format watchdog (C14) | wrong-size frames for **516.7 ms** → 1 restart → the **next** frame 1920x1080 | live run 2 |

Reading those logs:

- **`bytesused` plateaus are screen content**, and that is the only thing they are (A5: nothing is
  inferred from pixels). The desktop plateaus were matched against frames grabbed either side
  of each event. The 97811 B plateau was **not** seen as a frame — the probe does not save
  frames — it is read as a no-signal still because the identical size recurs before and after
  the boot console; the boot-console reading is likewise inferred from its timing and its slow
  rise. In the reboot run: ~191–205 KB desktop, **97811 B exactly** the dongle's
  no-signal still image, 122 487–125 466 B the Pi's boot console rising as text scrolls,
  165 353 → 174 962 B the desktop back. In the resolution run: 199 016 B is the 720p desktop
  rescaled to 1080p, 201 258 B the 1080p desktop after the revert.
- **Post-decode drops climbing at ~59/s mean the window is not being presented.** Observed in
  both live runs: whenever the viewer is occluded or unfocused on niri the compositor stops asking
  for frames, the renderer stops consuming the decoded slot, and `dropped pre/post 0/N` grows at
  the frame rate while `present p50` goes to ~1000 ms. Capture is unaffected and no counter in the
  capture half moves. It is the intended behaviour of a slot that keeps only the newest frame
  (§5.2); it is recorded here so that a future reader does not open a bug for it.
- **Probe timestamps lag the keystroke that caused the event.** `type-keys` paces at 40 ms per
  report and sends two reports per character, so the ~135-character `wlr-randr` line took ≈13 s to
  type: the slow `bytesused` rise from t≈35 s to t≈48 s in `probe-resolution.log` is that text
  appearing in the terminal, and Enter landed at t≈48.7 s. In the reboot run Enter landed at
  t≈9–11 s. Never read an event time off the probe without subtracting the typing.

## The three measurements — all now taken

1. **S2-4 replug, both nodes, either order (also H-A4 and H-B2) — measured, 2026-09-11.**

   **How it was run.** The blocker the previous session recorded was privilege, not the agent
   harness: the dongle's usbfs nodes are `root:root 0664` and `/sys/bus/usb/drivers/*/unbind` is
   root-only, so both `--method reset` and `--method rebind` need root, and passwordless sudo is
   off on this host. Neither a udev rule nor a persistent grant was taken. **The user ran every
   replug under `sudo` in their own terminal and pasted the output back**, and the orchestrating
   session never opened a usbfs node. That is the procedure to repeat; it needs no host change and
   leaves no standing grant behind.

   ```
   sudo python3 scripts/usb-replug.py video --method reset     # then serial, dongle, both, both-reversed
   sudo -E env NANOKVM_REPLUG_METHOD=reset cargo test --features hardware \
       --test serial_reconnect_hardware -- --ignored --nocapture --test-threads=1 h_a4
   sudo -E env NANOKVM_REPLUG_METHOD=reset cargo test --features hardware \
       --test capture_recovery_hardware -- --ignored --nocapture --test-threads=1 h_b2
   ```

   **The script alone, `--method reset`, five targets.** Every node went away every time — no
   driver survived a reset — and every node came back:

   | Target | Result |
   | --- | --- |
   | `video` | gone +60 ms, back **525 ms**, devnum kept |
   | `serial` | gone +5 ms, back **404 ms**, devnum kept |
   | `dongle` (the internal hub) | all three gone +5 ms; video back **940 ms**, serial back **1416 ms**, both with new devnums |
   | `both` | video gone +5 ms back 445 ms; then, after a 3 s gap, serial gone +5 ms back 424 ms |
   | `both-reversed` | serial gone +5 ms back 409 ms; then video gone +61 ms back 496 ms |

   Node names were unchanged throughout and the script raised no new-name warning — **because
   nothing had the nodes open**. That qualification turned out to be the whole of the matter.

   **Two defects, found by the measurement and fixed before it was believed.**

   - **H-A4 failed first time.** The writer held `/dev/ttyACM1` open across the reset, so the tty
     minor was never freed and the device came back as `/dev/ttyACM2`; the idle writer did not
     notice the loss at all in 30 s, and the fixed-path `SerialLinkSource` could never have
     reopened it even if it had. Diagnosed as write-driven loss detection plus a reopen by name;
     fixed in slice F and written up as **C13**.
   - **The first live viewer run streamed 640x480 for two minutes** after a correct reopen, with
     `G_FMT` still saying 1920x1080. The stream was stuck in the device's power-on mode with the
     reopen's UVC commit lost; fixed in slice G by the negotiated-format watchdog and written up
     as **C14**.

   **After the fixes — the numbers.**

   | Run | Result |
   | --- | --- |
   | H-A4 | node gone 414 ms, same name back; notice **≤ 95.8 ms** with nothing queued and no write attempted; `link_down` true→false **781.6 ms**; 1 reconnect, 3 attempts; release `Reconnected`/`Submitted`; CapsLock toggled the target 19.5 / 23.2 ms after the presses; release-all submitted on shutdown |
   | H-B2 ×2 | node **renumbered both times** (`video4`→`video5` capture, `video5`→`video6` metadata) and was reopened by identity; disconnect → 30 frames again **1.17 s**; 1 reopen in 1 attempt, 0 restarts; 4 mmaps on the new node, 0 on the old. Second run, with the watchdog: the first post-reopen frame was already 1920x1080, 0 mismatched frames, 0 format restarts |
   | Live run 1 (17:28 UTC, `both`) | capture loss noticed at once, node moved `video5`→`video4` and was rediscovered, reopened in 2 attempts; serial loss noticed **idle, with no write**, attempt 1 correctly failed (§8 can find no pair while the serial node is gone), reconnected after **769 ms** and 3 attempts; every title transition logged; one release recorded `UNSENT` and the reconnect's release cleared the notice; CapsLock toggled the target afterwards. **Then the C14 bug**: 640x480 for the rest of the run |
   | Live run 2 (17:45 UTC, `both`, with the watchdog) | reopen came up at 640x480 again; after **516.7 ms** of wrong-size frames the watchdog restarted the stream and the very next frame was 1920x1080; serial reconnected in 3 attempts; release-all submitted on shutdown |

   | Live run 3 (18:16 UTC, `both`, final binaries) | reopen at 640x480 a third time; watchdog restarted after 516.6 ms (32 frames), next frame 1920x1080; serial reconnected in 3 attempts; release-all submitted on shutdown |
   | H-A4 rerun | notice ≤ 95.6 ms; outage 781.3 ms for a 419 ms gone; 1 reconnect / 3 attempts; **same** node name back; CapsLock 20.2 / 23.0 ms |
   | H-B2 rerun | node renumbered `video4`→`video5`; reopened by identity straight to 1920x1080 (0 format restarts, 0 escalation reopens); 4 mappings new / 0 old; settle window derived 10.99 s |

   All three passed; the exit criterion is claimed.

   **Decision on `ReconnectConfig::default`: keep 250 ms initial / 4 s cap. Measured-consistent.**
   The measured serial outage is **404–430 ms** for a device reset and **1416 ms** for a hub reset.
   The writer attempts immediately on noticing the loss and then waits `250 ms`, `500 ms`, `1 s`,
   `2 s`, `4 s`, `4 s`, …, so attempts land at roughly **t ≈ 0, 0.25, 0.75, 1.75, 3.75, 7.75 s**
   after the loss:

   | Outage | First attempt after the node is back | Recovery |
   | --- | --- | --- |
   | 404–430 ms (device reset) | attempt 3, t ≈ 0.75 s | H-A4 measured **781.6 ms** for a 414 ms outage, 3 attempts — the ladder, exactly |
   | 1416 ms (hub reset) | attempt 4, t ≈ 1.75 s | ≈ 330 ms after the node returns |

   So every outage this hardware produces is caught by the third or fourth attempt, within about a
   third of a second of the device being back, and **the 4 s cap is never reached** — it would
   first bind on an outage longer than 3.75 s, where it bounds the worst wait between the device
   returning and the next attempt at 4 s. A smaller initial backoff buys nothing (attempt 1 already
   fires immediately and fails, because the node is not there yet); a larger one would miss the
   0.75 s slot and turn a 414 ms outage into a 1.75 s one. The justification on
   `ReconnectConfig::default` in `src/input/mod.rs` is updated from the `TODO` to this arithmetic.

2. **q12 / S2-2 target reboot — measured, 2026-09-11** (240 s probe run, with a steady-state
   baseline).

   How it was done, and how to repeat it: `sudo reboot` is useless here, because the `pi` user has
   no passwordless sudo and the password prompt swallows the command — that is what the control
   log is, 240 s of steady state. **`systemctl reboot` works without a password**, because logind
   lets the active seat reboot. With the probe already running:
   ```
   cargo run --release --example capture-probe -- --video /dev/video4 --seconds 240 > probe-reboot.log &
   ./target/release/examples/type-keys --serial /dev/ttyACM1 key:esc wait:500 chord:ctrl+alt+t wait:2500 'text:systemctl reboot' key:enter
   ```

   **Answer to q12: none of the three §6.1 conditions.** The dongle never stopped emitting. 60 fps
   for the whole run, SOF 1920x1080 and `G_FMT MJPG 1920x1080` constant, `gaps over 1s: none`,
   0 restarts, 0 disconnects, 0 reopens, 0 capture errors, 0 resolution changes. The desktop was
   gone for ≈46 s (t≈11 s to t≈57 s) and during it the dongle sent its no-signal still image
   (97811 B) and then the rescaled boot console. The only damage was **4 truncated JPEGs** at the
   signal transitions. Recorded as C12, because S2-2 predicts the opposite.

   **Decision on `restart_after`: nothing to decide from this.** The stall path was never entered,
   so a reboot on this unit does not exercise it and this measurement says nothing about the right
   interval. `restart_after` **stays 2 s as an argued value** (the argument is in the doc comment
   on `PipelineConfig::restart_after`) for a stall this unit has not yet produced. No number is
   being claimed for it.

3. **q13 / S2-3 resolution change — measured, 2026-09-11** (75 s probe run). The standing "never change the target's
   resolution" rule was lifted by the user for this one self-reverting command; it reverts even if
   the client dies, which is what makes it safe when the captured video is the target's only
   feedback channel. The Pi runs Wayland, so `xrandr` is not it — the output is `HDMI-A-1` and
   `wlr-randr` is installed (1280x720@60 is among its 27 modes). Typed to the target with the probe
   running:
   ```
   wlr-randr --output HDMI-A-1 --mode 1280x720@60Hz; sleep 20; wlr-randr --output HDMI-A-1 --preferred; sleep 2; wlr-randr --output HDMI-A-1 --mode 1920x1080
   ```
   Afterwards `wlr-randr` reported `1920x1080 px, 60.000000 Hz (preferred, current)`.

   **Answer to q13: the device rescales internally and the negotiated UVC format does not change.**
   Through the whole 720p window (t≈50–68 s) `G_FMT` stayed `MJPG 1920x1080` **and so did the SOF
   dimensions** — the pipeline counted 0 resolution changes, and 0 restarts, 0 reopens, 0 capture
   errors; 3 truncated JPEGs, one-second dips to 55.8–56.9 fps at each transition, no gap over 1 s.
   In the captured frame the Pi desktop in 1280x720 mode arrived with visibly larger UI,
   delivered as a 1920x1080 JPEG.

   **Decision, by §6.1 S2-3's own rule ("measure before writing renegotiation logic"): nothing more
   to build.** No renegotiation path, no capture rebuild. The per-frame resolution-change
   accounting in `pipeline.rs` stays as the safety net it is — a different unit, or this one on
   SuperSpeed, may not behave the same way — and it is now known to be a net this unit never
   touches.

## Carried forward

- **Nothing measurement-shaped.** All three Stage 2 measurements are taken (§6), and what is
  carried forward is code and coverage, not evidence.
- **Bare Super tap** (C7): viewer-side deferred lone-modifier press, not yet implemented.
- **The remaining hardware tests hardcode node names** (`tests/serial_hardware.rs`,
  `tests/capture_hardware.rs`, H-A1..H-A3, and `examples/capture-probe.rs`'s defaults). The two
  *replug* tests were converted to reopen by identity (C6, C13); the rest neither take the device
  away nor expect it back, so a name cannot move under them — but they should still resolve the
  dongle through `discovery`. Listed in `e-integration-report.md` §4.
- **The escalation reopen shows the wrong words briefly** (C14 step 2): it goes through the
  ordinary disconnect path, so the title says `capture device gone, reconnecting` for the second
  or so it takes, when the device is in fact present and merely streaming the wrong size. Harmless
  and rare — it needs three failed restarts first — but it is the one place the three §6.1
  conditions are not worded apart.
- **Discovery runs twice at startup** (once in `select_devices`, once in the reopeners' first
  resolution): two extra read-only `QUERYCAP`s, harmless, untidy.
- **`usb-replug.py` on SuperSpeed**: it requires the video device under the internal hub, which
  is the USB 2.0 shape only; on SuperSpeed it will refuse. Extend when the SuperSpeed link is back.
- **Every replug number here is a USB 2.0 number, and the SuperSpeed shapes are untested live.**
  The port-`peer` pairing rule (§8), the `usb3-stage0`/`usb3-rootport` trees and the C14 timing all
  rest on fixtures or on a link this desk has not had since Stage 0. Nothing suggests they are
  wrong; nothing has exercised them either.
- **`max_barriers`** (C11), **VT-switch keys** (uninhibitable, documented), **bindgen
  cross-architecture** (`v4l2-sys-mit` passes no `--target`; not investigated), unchanged.
- The **no-GPU failure message** §11 asked for already exists (`render.rs` names the likely
  cause on both `request_adapter` and `request_device` failure); §11 can drop it.
- `SubmitError::Disengaged` on the writer-initiated link-loss path carries no §2.8 notice of its
  own; the "serial DOWN" title covers it.
