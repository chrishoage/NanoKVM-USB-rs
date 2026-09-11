# NanoKVM-USB-rs

Native Linux client for the Sipeed NanoKVM-USB dongle. Drives the device over its
CH9329 serial link and its UVC video node, replacing the Chromium-based vendor client.

- **Design of record:** [`docs/NATIVE_CLIENT_PLAN.md`](docs/NATIVE_CLIENT_PLAN.md) (rev 4).
- **Status:** **Stage 3 complete** — the viewer of Stages 1 and 2 plus five subcommands
  (`devices`, `shot`, `key`, `type`, `macro`), reviewed and verified on hardware by consequence on
  2026-09-11. What each stage found, and how we know: Stage 0 in
  [`docs/STAGE0_FINDINGS.md`](docs/STAGE0_FINDINGS.md), Stage 1 in
  [`docs/STAGE1_FINDINGS.md`](docs/STAGE1_FINDINGS.md), Stage 2 in
  [`docs/STAGE2_FINDINGS.md`](docs/STAGE2_FINDINGS.md), Stage 3 in
  [`docs/STAGE3_FINDINGS.md`](docs/STAGE3_FINDINGS.md).
- **Target desktop:** niri 26.04 on Wayland. Linux only.

## Layout

| Path | Contents |
| --- | --- |
| `docs/NATIVE_CLIENT_PLAN.md` | The design. Read this first. |
| `docs/STAGE0_FINDINGS.md` | Stage 0 exit review: what was measured, and the 19 amendments. |
| `docs/STAGE1_FINDINGS.md` | Stage 1: amendments B1–B7, module and test inventory, hardware numbers. |
| `docs/STAGE2_FINDINGS.md` | Stage 2: amendments C1–C14, what is built, and all three hardware measurements taken — target reboot, resolution change, and the kernel-side replug. |
| `docs/STAGE3_FINDINGS.md` | Stage 3: amendments D1–D10, the CLI as built, the two adversarial reviews, and the hardware run. |
| `fixtures/packets/` | Hand-derived protocol frames. The authority for protocol tests. |
| `scripts/` | Verification you can run instead of trusting the docs. |
| `src/` | The crate: `proto`, `link`, `input`, `serial`, `capture`, `viewer`, `discovery`, `script`, `cli`, and the `nanokvm` binary. |
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
./target/release/nanokvm                                        # the viewer; discovers both nodes (§8)
./target/release/nanokvm devices                                # what discovery can see, then exit
./target/release/nanokvm --serial /dev/ttyACM1 --video /dev/video4   # or say which
```

`--serial` and `--video` are **optional**: the two nodes are paired from USB topology, by the
kernel's own port `peer` link where the dongle is on a SuperSpeed port and by containment under
the dongle's internal hub where it is not. Discovery never guesses — more than one candidate
pair, or none, prints everything it found and exits non-zero, and the flags are how you answer
it. Giving both uses them exactly as typed and skips discovery entirely; giving one narrows the
search to pairs containing it. `nanokvm devices` prints the same table with the nodes your flags
name marked, and opens nothing but a `VIDIOC_QUERYCAP` on a video node it might select.

### Subcommands

`nanokvm` with no subcommand is the viewer, exactly as before. Each subcommand opens only the
node it needs and starts none of the viewer's threads; the global `--serial`/`--video` and
`--width`/`--height`/`--fps` work on **either side** of the subcommand, so `nanokvm --serial X key
a` and `nanokvm key a --serial X` are the same command. The viewer-only flags (`--pointer`,
`--stats-interval`) are a usage error with a subcommand, named rather than ignored.

| Command | What it does |
| --- | --- |
| `nanokvm devices [--probe]` | The discovery table. `--probe` additionally opens the serial node discovery would select (or the one `--serial` names) and prints its firmware, target-connected flag, lock bits and USB strings. Where discovery cannot name one pair it probes nothing and exits non-zero: it never opens a node the client itself would refuse to use. |
| `nanokvm shot [PATH]` | One frame to `.jpg`, `.png` or stdout, then exit. Opens the video node only. |
| `nanokvm key <CHORD>…` | Send key chords, each pressed and then fully released. Opens the serial node only. |
| `nanokvm type <TEXT>` | Type text against a declared target layout (§10.2). `-` reads the text from stdin. |
| `nanokvm macro <FILE>` | Run a file of `key`, `type` and `wait` steps. |

#### `devices`

```
nanokvm devices                 # the table: every node found, and what pairs with what
nanokvm devices --probe         # ...and ask the paired serial node what it is
```

`--probe` prints the firmware version, whether a target is attached to the HID side, the target's
lock bits and the dongle's USB strings (its unit-unique serial among them). It opens **only** the
node discovery selected, or the one `--serial` names — never every candidate, because a candidate
that is not the dongle is somebody else's device.

#### `shot`

```
nanokvm shot desk.png           # decoded and re-encoded
nanokvm shot desk.jpg           # the device's own bytes, byte for byte
nanokvm shot - > desk.jpg       # the JPEG on stdout
```

The first eight frames after the stream starts are discarded by default (`--skip`): after an idle
period the device emits up to eight frames at the *previous* resolution. A truncated frame is
dropped, the size comes from the frame's own JPEG header — never from the driver — and a stream
stuck at the wrong size is restarted once and then reported rather than silently written to a file.
The file appears atomically, so nothing ever reads half a screenshot.

#### `key`

```
nanokvm key ctrl+alt+t          # a terminal on the target
nanokvm key capslock            # toggle a lock bit; `devices --probe` reads it back
nanokvm key --dry-run ctrl+c    # print the frames, open nothing
```

Chords are `ctrl alt shift super` (right-hand `rctrl ralt rshift rsuper`) plus one key: a name
(`enter`, `f10`, `pgdn`, `kp5`, …) or any single printable character. A chord of only modifiers taps
them. `nanokvm key --help` prints the whole grammar.

#### `type`

```
nanokvm type 'echo hello'
printf 'echo hello\n' | nanokvm type -          # the trailing newline is an Enter
nanokvm type --caps-lock compensate 'echo hello'
```

#### `macro`

```
nanokvm macro login.macro
```

```
# login.macro
key ctrl+alt+t
type echo hello
key enter
wait 500
```

`#` comments out a whole line, never the tail of one: `type` takes its text verbatim to the end of
the line. A file may declare its own layout with a first `layout us` line, which must agree with
`--layout` if you also pass one.

### What the senders guarantee, and what they do not

**The layout is the target's, and this host cannot see it.** `type` and `macro` take `--layout`
(only `us` today, and it is the stated default, not a silent assumption); a character that layout
cannot reach is refused by name, never approximated. The target's CapsLock is visible through the
device, so it is a decision rather than a guess: `--caps-lock refuse` (the default — nothing is
typed), `ignore`, or `compensate`, which inverts shift on letters so the text arrives in the case
you asked for.

**An acknowledgement is not evidence of effect.** The device acknowledges reports it does not act
on, so what these commands print is acks and nothing more; the effect is on the target's screen, and
checking it is yours to do.

The three safety rules the senders enforce, all of them structural rather than promised:

- **The whole script is compiled before the port is opened.** An unknown chord, a bad macro line or
  an unreachable character is an error with nothing sent — and *every* such problem is listed at
  once, so one run fixes the line. `--dry-run` is the same compiler with the port left shut, and
  runs no discovery either.
- **Release-all on every exit path** — clean exit, error, panic, or SIGINT/SIGTERM/SIGHUP — and the
  outcome is printed, because a release that could not be sent means the target may still be
  holding a key.
- **No mouse, at all.** These commands cannot build a mouse report: a blind click on a live desktop
  can launch or destroy something. A test reads every file under `src/cli/` and `src/script/` to
  keep it that way.

### The viewer

Defaults are MJPEG 1920x1080 at 60 fps and absolute pointer mode (`--pointer relative` locks the
pointer instead). `--width`/`--height`/`--fps` are shared with `shot`; `--pointer` and
`--stats-interval` are the viewer's alone.

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
cargo test --test cli_keys --test cli_shot --test cli_shape   # the Stage 3 binary-level tests
cargo test --features hardware --test serial_hardware --test capture_hardware -- --ignored --nocapture
```

The Stage 3 tests exec the built `nanokvm` against a pty fake and a recorded sysfs tree, so they
check what the process does — exit status, the words of an error, the frames that reached the
device, the release-all after a signal — without touching a real device.

The scripts take no arguments and exit non-zero on failure. The hardware tests toggle the
target's CapsLock twice (and put it back) and stream from the video node; they never click.
