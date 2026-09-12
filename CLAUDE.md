# NanoKVM-USB-rs — working notes for agents

Native Linux client for the Sipeed NanoKVM-USB dongle. Rust, one crate, Linux only, targeting
niri on Wayland.

## Read this first, and not more than this

1. `docs/NATIVE_CLIENT_PLAN.md` — the design of record, currently **rev 5**. Start at §0.2, which
   lists what Stage 0 measurement changed, and §0.3, which lists what rev 5 changed about Stage 4
   (audio, the egui chrome, clipboard paste; RFB and recording dropped).
2. `docs/STAGE0_FINDINGS.md` — the evidence index and the 19 amendments.
3. `docs/STAGE1_FINDINGS.md` — what building Stage 1 changed: the amendments B1–B7 the plan still
   needs, the module/test inventory, and the hardware numbers (taken on a USB 2.0 link).
4. `docs/STAGE2_FINDINGS.md` — what Stage 2 changed: amendments C1–C14, reconnect/recovery/
   discovery as built, and **all three hardware measurements taken** — target reboot, resolution
   change, and the kernel-side replug (which found two real bugs, C13 and C14).
5. `docs/STAGE3_FINDINGS.md` — what Stage 3 changed: amendments D1–D10, the CLI as built
   (`devices`, `shot`, `key`, `type`, `macro`), the two blind adversarial reviews, and the
   hardware verification — by consequence, as always: a lock bit read back, the typed line on the
   target's screen.
6. `docs/STAGE4_FINDINGS.md` — what Stage 4 changed: amendments E1–E20, audio/chrome/paste as
   built, the three rounds of blind review, and the hardware verification — the 1 kHz tone in the
   captured PCM, the drift in ppm, and the pasted clipboard's md5 on the target's screen.

Raw per-spike evidence runs to roughly 3500 lines and **lives on the `stage-0` branch**, under
`docs/stage0/`, alongside the throwaway spike crates in `spikes/`. **Do not read it up front.**
Check out that branch and open one file when you need the measurement behind a specific
claim.

The plan says what to build. The findings say how we know. If they ever disagree, the findings
are the evidence and the plan is the bug.

## Hardware on this desk

**Identify hardware by USB identity, never by node name.** Node names are whatever the kernel had
free at enumeration: after a replug the dongle can come back as `/dev/video0` and `/dev/ttyACM0`
(it will, if the dock is unplugged at the time), and today's names can belong to something else
tomorrow. `nanokvm devices` prints what discovery currently sees.

| | Identity (stable) | Today's names |
| --- | --- | --- |
| The dongle | a `1a40:0101` internal hub with `345f:2133` video and `1a86:55d3` serial as its direct children | `/dev/video4` + `/dev/video5`, `/dev/ttyACM1` |
| The dongle's sound card | the USB Audio Class interface `1.2` of `345f:2133` — the **same USB device** as the capture node, which is why §8's strongest rule pairs it | `card8`, `hw:8` |
| The user's hardware | **everything on USB bus 5** — the dock, carrying a Logitech webcam and an unrelated CDC-ACM device | `/dev/video0`–`3`, `/dev/ttyACM0` |
| Target | A Raspberry Pi 3B running the Raspberry Pi OS desktop at 1080p | — |

- **Never open anything on bus 5.** It is the user's, not ours. Check with
  `readlink -f /sys/class/video4linux/videoN/device` or `nanokvm devices` rather than assuming
  a number.
- Of the dongle's two video nodes, one is the capture node and the other is a metadata sibling.
  Which is which is decided by `VIDIOC_QUERYCAP`, never by the number (`discovery::probe`).
- **Identify the sound card by sysfs, never by number.** Card numbers renumber on replug exactly
  as `/dev` names do, and the desk's other four cards (three PCI codecs and a bus-5 USB card) are
  in the same class directory. `hw:<N>` is parsed out of `/sys/class/sound/card<N>` at open time
  and never remembered; `nanokvm devices` prints the pairing and its evidence.
- **Never match on the product string.** The video interface calls itself "USB2 Video" on a USB
  2.0 link and "USB3 Video" on SuperSpeed — the same unit, the same `345f:2133`, a different
  name depending on which port it is in. The model is **Pro 4K60**, measured.
- The two dongle nodes are **separate USB devices on separate buses** (§8). Which pairing rule
  applies depends on the port: on SuperSpeed it is the kernel's port `peer` link, and on a USB
  2.0 port — which is where it is today — there is no `peer`, so the rule is the degraded one,
  containment under the dongle's own internal hub with all three vendor ids checked. That is not
  proof, and both `discovery` and `scripts/device-health.py` say so where they use it.
- **The captured video is the target's only feedback channel.** It has no network. Never
  unplug the HDMI cable, and never change the target's resolution.
- The target is disposable and you may drive it. **Never send a blind mouse click** — a click
  on a live desktop can launch or destroy something. Motion is safe. The CLI has **no mouse path
  at all, by test**: nothing under `src/cli/` or `src/script/` can build a mouse report, and
  `tests/cli_keys.rs` reads every file in both to keep it so.
- Always send a release-all before exiting anything that sent input.
- A hardware test or example that hardcodes a node name (`tests/*_hardware.rs`,
  `examples/capture-probe.rs`) must **say so in a comment**, naming the identity it means, so a
  run on a renumbered desk fails with an explanation instead of touching the wrong device.
- The replug hardware tests shell out to `scripts/usb-replug.py`. It unbinds and rebinds a USB
  device; run it only when a task says to, and never against bus 5.

## Things that will waste your time if you forget them

- **An acknowledgement is not evidence of effect.** The device ACKs malformed mouse reports and
  does nothing. Assert on a consequence — a pixel moved, a lock bit changed — never on an ACK.
  This one cost a whole spike.
- **Mouse payloads carry a leading mode byte**: rel is `5 B [0x01, …]`, abs is `7 B [0x02, …]`.
  Rev 3 of the plan had this wrong.
- **Frame dimensions come from the JPEG header, never `G_FMT`.** The device emits up to eight
  frames at the previous resolution after an idle period.
- **Call `S_PARM` explicitly** or 1080p silently runs at 240 fps.
- **Match serial replies by command byte, not arrival order.** The device pushes unsolicited
  frames.
- **A truncated serial write corrupts the next command.** The chip has no inter-byte timeout.
- **An open fd keeps a tty or video index, so the node renumbers on replug.** The kernel hands
  back the lowest *free* name. Hold `/dev/ttyACM1` across a reset and the dongle returns as
  `/dev/ttyACM2`, permanently; the capture node renumbered `video4`→`video5` on both H-B2 runs.
  Every same-name replug this project recorded was a race it happened to win (C13, C6).
- **After a USB reset the device may stream 640x480 with the format commit lost** — `G_FMT` still
  says 1920x1080, because that is the driver's opinion, not the device's. The stream is stuck, not
  the device. The pipeline's format watchdog restarts the stream and fixes it (C14); do not
  "correct" a size by trusting `G_FMT`.

## Repo conventions

- **No Cargo workspace.** On the `stage-0` branch each spike is a standalone crate so it can
  be deleted without touching anything else. Keep it that way if you add one.
- `spikes/` is **throwaway Stage 0 code** and is not on `main`. Do not build on it, do not tidy
  it, do not test it.
- `src/` is the crate, laid out per plan §4: `proto/` (pure), `link.rs` (the one seam
  between the input writer and a transport), `input/`, `serial/`, `capture/`, `viewer/`,
  `discovery/` (§8: pairing the two nodes from sysfs alone), `script/` (pure: the chord and macro
  compiler behind `key`/`type`/`macro`) and `cli/` (the subcommands and their I/O). `proto`,
  `input` and `script` do no I/O, and `discovery` reads `/sys` plus one `QUERYCAP`; keep it that
  way. `discovery::reopen` is the one module that sits above the rest — it is what `main.rs`
  reopens devices through.
- **`--dry-run` on `key`/`type`/`macro` opens no device and does not even read `/sys`**, and
  `nanokvm devices` without `--probe` opens no serial node — its only device access is
  discovery's read-only `QUERYCAP`. Use them to check a script, or the desk, before touching
  hardware.
- **The viewer has hidden dev flags** for the things nothing on this desk may drive: besides
  `--exit-after` and `--sysfs-root`, there are `--chrome-popover <Video|Keyboard|Mouse|Audio>`
  (opens a popover so its cost can be measured), `--capture-on-start`, `--paste-on-capture`
  (implies the former) and `--paste-cancel-after-ms <MS>` (feeds the real release key, not a
  private cancel). All are viewer-only and are refused alongside a subcommand like every other
  viewer flag. Use them rather than inventing a second code path for a measurement.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo clippy --all-targets
  --features hardware -- -D warnings` and `cargo test` must all be clean before anything is
  called done. The second clippy run matters because the hardware tests only compile under that
  feature, so nothing else ever type-checks them.
- `cargo test` runs everything that needs no hardware, including the keyboard-command tests in
  `tests/cli_keys.rs` (they drive the built binary against a pty fake, not the dongle).
  `cargo test --test viewer_chrome_ui -- --ignored` runs the four wgpu snapshot tests against
  `tests/snapshots/*.png`; they are `#[ignore]`d because they need a real adapter (a GPU or
  lavapipe), and the baseline PNGs are committed while the `.new`/`.diff`/`.old` ones are not.
  Hardware tests are behind `--features hardware` and `#[ignore]`; run them one binary at a time
  with `--test-threads=1`, e.g. `cargo test --features hardware --test serial_hardware --test
  capture_hardware --test audio_hardware -- --ignored --nocapture --test-threads=1` (add
  `--skip drift_over_ten_minutes`, which takes twelve minutes and is measured already). They
  toggle the target's CapsLock twice, stream from the video node and open the sound card; they
  never click.
- **The viewer persists settings to `$XDG_CONFIG_HOME/nanokvm/config.toml`** (falling back to
  `$HOME/.config`). Any hardware test or desk run that starts the shipped binary must point
  `XDG_CONFIG_HOME` at a scratch directory, or it writes the user's own settings — one already
  did. Avoid the word "audio" in that path: `XDG_CONFIG_HOME` also redirects the Vulkan loader's
  layer search, which prints the paths it looked in.
- `fixtures/packets/ch9329.toml` is the **authority** for protocol tests (§9.1). Tests must
  read it, never retype the bytes — retyping is how a transcription bug silently blesses a
  wrong encoder.
- Captured frames and build artifacts are gitignored. `reference/` is a read-only upstream
  checkout, also ignored. So are the four sysfs trees under `fixtures/sysfs/`: they are built
  from `fixtures/sysfs/usb2-desk.sysfs` by `synthesize.py` (see "Verify rather than trust").

## Verify rather than trust

```
python3 scripts/check-fixtures.py     # revalidates every packet fixture from scratch
python3 scripts/device-health.py      # devices present, paired, and the link answers
python3 scripts/usb-replug.py --self-test   # the replug hardware fixture, against fakes only
```

Run those three after touching the protocol or the fixtures. They exit non-zero on failure and
need no arguments. `device-health.py` needs the hardware; the other two do not.

**`cargo test` needs `python3` on PATH.** The four sysfs trees under `fixtures/sysfs/` are build
output and are gitignored; what is committed is `fixtures/sysfs/usb2-desk.sysfs` (the 2026-09-10
recording of this desk, serialised to one line-oriented file) and `fixtures/sysfs/synthesize.py`,
which expands it and reconstructs the other three trees from it.
`nanokvm::discovery::testing::fixture` runs that script automatically the first time a tree it
was asked for is missing, so a fresh checkout just works. Build them by hand, or after editing
`synthesize.py`, with:

```
python3 fixtures/sysfs/synthesize.py          # missing trees, plus the bus-5 negative control
python3 fixtures/sysfs/synthesize.py --force  # re-expand usb2-desk/ from the recording too
```

Never hand-edit a tree: it is overwritten. Edit the recording or the script, and see
`fixtures/sysfs/MANIFEST.md` for the format and for how to re-record this desk.

`python3 scripts/usb-replug.py <video|serial|dongle|both>` **touches the hardware**: it makes
the kernel unplug and replug the dongle, so both /dev nodes vanish and return. Only Stage 2
recovery tests should run it; use `--dry-run` if you just want to see what it resolves. It
identifies the dongle by sysfs port path, so it still works when a /dev node is missing
(`--port 3-2.2`), and it prints a warning if the nodes come back under new names — every
hardware test here hardcodes `/dev/video4` and `/dev/ttyACM1`.

## Git

- **Never commit to `main`** unless the user asks in that message, and **never push** unless
  asked. Committing is not permission to push.
- The user commits this repo themselves unless they say otherwise.
