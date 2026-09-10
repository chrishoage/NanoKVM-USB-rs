# NanoKVM-USB-rs — working notes for agents

Native Linux client for the Sipeed NanoKVM-USB dongle. Rust, one crate, Linux only, targeting
niri on Wayland.

## Read this first, and not more than this

1. `docs/NATIVE_CLIENT_PLAN.md` — the design of record, currently **rev 4**. Start at §0.2, which
   lists what Stage 0 measurement changed.
2. `docs/STAGE0_FINDINGS.md` — the evidence index and the 19 amendments.
3. `docs/STAGE1_FINDINGS.md` — what building Stage 1 changed: the amendments B1–B7 the plan still
   needs, the module/test inventory, and the hardware numbers (taken on a USB 2.0 link).

Raw per-spike evidence runs to roughly 3500 lines and **lives on the `stage-0` branch**, under
`docs/stage0/`, alongside the throwaway spike crates in `spikes/`. **Do not read it up front.**
Check out that branch and open one file when you need the measurement behind a specific
claim.

The plan says what to build. The findings say how we know. If they ever disagree, the findings
are the evidence and the plan is the bug.

## Hardware on this desk

| | |
| --- | --- |
| Video | `/dev/video4` — "USB3 Video", `345f:2133`. Model is **Pro 4K60**, measured |
| Serial | `/dev/ttyACM1` — CH9329 behind a `1a86:55d3` bridge |
| Target | A Raspberry Pi 3B running the Raspberry Pi OS desktop at 1080p |

- **Do not touch `/dev/video0`–`3` or `/dev/ttyACM0`.** Unrelated hardware belonging to the
  user.
- The two dongle nodes are **separate USB devices on separate buses**. See §8; the pairing rule
  is the kernel's port `peer` link, which `scripts/device-health.py` implements and checks.
- **The captured video is the target's only feedback channel.** It has no network. Never
  unplug the HDMI cable, and never change the target's resolution.
- The target is disposable and you may drive it. **Never send a blind mouse click** — a click
  on a live desktop can launch or destroy something. Motion is safe.
- Always send a release-all before exiting anything that sent input.

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

## Repo conventions

- **No Cargo workspace.** On the `stage-0` branch each spike is a standalone crate so it can
  be deleted without touching anything else. Keep it that way if you add one.
- `spikes/` is **throwaway Stage 0 code** and is not on `main`. Do not build on it, do not tidy
  it, do not test it.
- `src/` is the Stage 1 crate, laid out per plan §4: `proto/` (pure), `link.rs` (the one seam
  between the input writer and a transport), `input/`, `serial/`, `capture/`, `viewer/`. `proto`
  and `input` do no I/O; keep it that way.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test` must all be clean
  before anything is called done. Hardware tests are behind `--features hardware` and `#[ignore]`;
  run them with `cargo test --features hardware --test serial_hardware --test capture_hardware --
  --ignored --nocapture`. They toggle the target's CapsLock twice and stream from the video node;
  they never click.
- `fixtures/packets/ch9329.toml` is the **authority** for protocol tests (§9.1). Tests must
  read it, never retype the bytes — retyping is how a transcription bug silently blesses a
  wrong encoder.
- Captured frames and build artifacts are gitignored. `reference/` is a read-only upstream
  checkout, also ignored.

## Verify rather than trust

```
python3 scripts/check-fixtures.py     # revalidates every packet fixture from scratch
python3 scripts/device-health.py      # devices present, paired, and the link answers
```

Run both after touching the protocol or the fixtures. They exit non-zero on failure and need
no arguments. `device-health.py` needs the hardware; `check-fixtures.py` does not.

## Git

- **Never commit to `main`** unless the user asks in that message, and **never push** unless
  asked. Committing is not permission to push.
- The user commits this repo themselves unless they say otherwise.
