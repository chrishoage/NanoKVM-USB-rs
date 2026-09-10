# NanoKVM-USB-rs

Native Linux client for the Sipeed NanoKVM-USB dongle. Drives the device over its
CH9329 serial link and its UVC video node, replacing the Chromium-based vendor client.

- **Design of record:** [`docs/NATIVE_CLIENT_PLAN.md`](docs/NATIVE_CLIENT_PLAN.md) (rev 4).
- **Status:** Stage 1 built — a usable viewer with keyboard and mouse — on the `stage-1`
  branch. Signed off live on 2026-09-10 (§5.4 measurement in the findings); only the Pause key, the
  wheel and one user-observed niri-bind check remain on the checklist. Stage 0 evidence in
  [`docs/STAGE0_FINDINGS.md`](docs/STAGE0_FINDINGS.md); what Stage 1 found in
  [`docs/STAGE1_FINDINGS.md`](docs/STAGE1_FINDINGS.md).
- **Target desktop:** niri 26.04 on Wayland. Linux only.

## Layout

| Path | Contents |
| --- | --- |
| `docs/NATIVE_CLIENT_PLAN.md` | The design. Read this first. |
| `docs/STAGE0_FINDINGS.md` | Stage 0 exit review: what was measured, and the 19 amendments. |
| `docs/STAGE1_FINDINGS.md` | Stage 1: amendments B1–B7, module and test inventory, hardware numbers. |
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
./target/release/nanokvm --serial /dev/ttyACM1 --video /dev/video4
```

Explicit `--serial` and `--video` are required; discovery is Stage 2. Defaults are MJPEG
1920x1080 at 60 fps and absolute pointer mode (`--pointer relative` locks the pointer instead).

- **Click or press Enter in the window to capture.** The capturing click is consumed, never sent
  to the target. While captured, compositor shortcuts are inhibited and every key goes to the
  target, including host key combinations.
- **Release with `Pause`** (never forwarded), with niri's own `Mod+Escape`, or by focusing another
  window. Every release sends a release-all to the target first.
- The window title reports the capture state, whether frames have stopped arriving, and whether
  a release could not be delivered (in which case the target may still be holding keys).
- `--stats-interval N` logs pipeline, input and event-loop timings every N seconds.

## Checks

```
python3 scripts/check-fixtures.py     # revalidates every protocol fixture from scratch
python3 scripts/device-health.py      # devices present, paired, and the link answers
cargo test                            # 287 tests, no hardware needed
cargo test --features hardware --test serial_hardware --test capture_hardware -- --ignored --nocapture
```

The scripts take no arguments and exit non-zero on failure. The hardware tests toggle the
target's CapsLock twice (and put it back) and stream from the video node; they never click.
