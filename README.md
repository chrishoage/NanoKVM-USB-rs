# NanoKVM-USB-rs

Native Linux client for the Sipeed NanoKVM-USB dongle. Drives the device over its
CH9329 serial link and its UVC video node, replacing the Chromium-based vendor client.

- **Design of record:** [`docs/NATIVE_CLIENT_PLAN.md`](docs/NATIVE_CLIENT_PLAN.md) (rev 4).
- **Status:** Stage 0 complete, its 19 amendments folded into rev 4 of the design. Ready for
  Stage 1. Evidence in [`docs/STAGE0_FINDINGS.md`](docs/STAGE0_FINDINGS.md).
- **Target desktop:** niri 26.04 on Wayland. Linux only.

## Layout

| Path | Contents |
| --- | --- |
| `docs/NATIVE_CLIENT_PLAN.md` | The design. Read this first. |
| `docs/STAGE0_FINDINGS.md` | Exit review: what was measured, and the 19 amendments. |
| `fixtures/packets/` | Hand-derived protocol frames. The authority for protocol tests. |
| `scripts/` | Verification you can run instead of trusting the docs. |
| `src/` | The real crate. Does not exist until Stage 1. |

`CLAUDE.md` carries the working notes and hazards for anyone, human or agent, picking this up.

## Branches

`main` carries only what Stage 1 builds on. **The Stage 0 spike code and its raw evidence live
on the `stage-0` branch**, which is `main` plus `spikes/`, `docs/stage0/` and the captured
fixtures.

That material is deliberately not on `main`: it is throwaway by design, it is large, and
everything durable it produced has already been folded into the design and the fixtures. Check
out `stage-0` when you need the measurement behind a specific claim.

## Checks

```
python3 scripts/check-fixtures.py     # revalidates every protocol fixture from scratch
python3 scripts/device-health.py      # devices present, paired, and the link answers
```

Neither takes arguments and both exit non-zero on failure. The first needs no hardware.
