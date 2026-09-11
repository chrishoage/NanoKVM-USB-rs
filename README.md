# NanoKVM-USB-rs

Native Linux client for the Sipeed NanoKVM-USB dongle. Drives the device over its
CH9329 serial link and its UVC video node, replacing the Chromium-based vendor client.

- **Design of record:** [`docs/NATIVE_CLIENT_PLAN.md`](docs/NATIVE_CLIENT_PLAN.md) (rev 4).
- **Status:** Stage 2 in progress — device discovery (§8), serial reconnect (§2.7) and capture
  recovery (§6.1) on top of the Stage 1 viewer. Stage 1 was signed off live on 2026-09-10 (§5.4
  measurement in the findings). Stage 0 evidence in
  [`docs/STAGE0_FINDINGS.md`](docs/STAGE0_FINDINGS.md); what Stage 1 found in
  [`docs/STAGE1_FINDINGS.md`](docs/STAGE1_FINDINGS.md).
- **Target desktop:** niri 26.04 on Wayland. Linux only.

## Layout

| Path | Contents |
| --- | --- |
| `docs/NATIVE_CLIENT_PLAN.md` | The design. Read this first. |
| `docs/STAGE0_FINDINGS.md` | Stage 0 exit review: what was measured, and the 19 amendments. |
| `docs/STAGE1_FINDINGS.md` | Stage 1: amendments B1–B7, module and test inventory, hardware numbers. |
| `docs/STAGE2_FINDINGS.md` | Stage 2: amendments C1–C14, what is built, and all three hardware measurements taken — target reboot, resolution change, and the kernel-side replug. |
| `fixtures/packets/` | Hand-derived protocol frames. The authority for protocol tests. |
| `scripts/` | Verification you can run instead of trusting the docs. |
| `src/` | The crate: `proto`, `link`, `input`, `serial`, `capture`, `viewer`, and the `nanokvm` binary. |
| `tests/` | Integration tests per module; hardware tests behind `--features hardware`. |

`CLAUDE.md` carries the working notes and hazards for anyone, human or agent, picking this up.

## Branches

`main` carries only what Stage 1 builds on. **The Stage 0 spike code and its raw evidence live
on the `stage-0` branch**, which is `main` plus `spikes/`, `docs/stage0/` and the captured
fixtures.

That material is deliberately not on `main`: it is throwaway by design, it is large, and
everything durable it produced has already been folded into the design and the fixtures. Check
out `stage-0` when you need the measurement behind a specific claim.

## Running

```
cargo build --release
./target/release/nanokvm                                        # discovers both nodes (§8)
./target/release/nanokvm --list-devices                         # what discovery can see, then exit
./target/release/nanokvm --serial /dev/ttyACM1 --video /dev/video4   # or say which
```

`--serial` and `--video` are **optional**: the two nodes are paired from USB topology, by the
kernel's own port `peer` link where the dongle is on a SuperSpeed port and by containment under
the dongle's internal hub where it is not. Discovery never guesses — more than one candidate
pair, or none, prints everything it found and exits non-zero, and the flags are how you answer
it. Giving both uses them exactly as typed and skips discovery entirely; giving one narrows the
search to pairs containing it. `--list-devices` prints the same table with the nodes your flags
name marked, and opens nothing but a `VIDIOC_QUERYCAP` on a video node it might select.

Defaults are MJPEG 1920x1080 at 60 fps and absolute pointer mode (`--pointer relative` locks the
pointer instead).

- **Click or press Enter in the window to capture.** The capturing click is consumed, never sent
  to the target. While captured, compositor shortcuts are inhibited and every key goes to the
  target, including host key combinations.
- **Release with `Pause`** (never forwarded), with niri's own `Mod+Escape`, or by focusing another
  window. Every release sends a release-all to the target first.
- The window title reports the capture state, whether frames have stopped arriving, and whether
  a release could not be delivered (in which case the target may still be holding keys).
- `--stats-interval N` logs two lines every N seconds: one for capture and display, one for
  input. Both carry the recovery counters (reopens, stream restarts, serial reconnects).

### Unplug, replug, and stalls

None of these ends the session any more, and they are deliberately never worded the same:

- **The serial link goes away.** Input is refused rather than queued into nothing, the title says
  `serial DOWN, reconnecting (N s, M attempts)`, and the writer reopens on a backoff. When the
  link is back it resynchronises the chip's frame parser, sends a full release-all so no key can
  be left held, re-queries the device info and logs it. Capture is untouched throughout (§6.1
  S1-1). **Input stays released until you capture again** — that is deliberate (§2.8): a
  session that resumed by itself would start typing into whatever the target is showing now.
- **The video node goes away.** The window keeps the last frame, input keeps working, and the
  pipeline reopens the node on a backoff; the title says `capture device gone, reconnecting (N
  s)`. If the node comes back under a *different* name (`/dev/video6`), discovery finds it —
  unless you named it with `--video`, which is honoured permanently and never second-guessed.
- **Frames stop arriving from a device that is still there.** That is a stall, not a
  disconnection: the title says `frames stopped N s ago` and the pipeline restarts the stream in
  place. The client never claims the signal is gone; this hardware cannot report that, and the
  pixels are not evidence of it.
- **Frames arrive, at the wrong size.** After a USB reset the dongle can come back streaming its
  power-on 640x480 with the format it just agreed to lost, while the driver still reports
  1920x1080. The pipeline notices (the frame's own header is the authority on its size), restarts
  the stream to re-commit the format, and escalates once to a full reopen if that does not take.
  If the device still will not deliver the negotiated size, that is accepted rather than fought
  over, and the title says so: `video 640x480, negotiated 1920x1080 not established`.

## Checks

```
python3 scripts/check-fixtures.py     # revalidates every protocol fixture from scratch
python3 scripts/device-health.py      # devices present, paired, and the link answers
cargo test                            # no hardware needed
cargo test --features hardware --test serial_hardware --test capture_hardware -- --ignored --nocapture
```

The scripts take no arguments and exit non-zero on failure. The hardware tests toggle the
target's CapsLock twice (and put it back) and stream from the video node; they never click.
