# Cursor-coordinate evidence

Fifteen full-resolution JPEGs recorded on 2026-09-09 with a NanoKVM-USB and a
Raspberry Pi 3B running Raspberry Pi OS at 1920 × 1080. The recorded paths were
`/dev/ttyACM1` and `/dev/video4`; capture used MJPG, 1920 × 1080, 30 fps.
These files are committed. Keep their original resolution: at the edges, only
one or two cursor pixels may remain visible.

Every measurement compares a frame with `ref-park-3000-3000.jpg`. On the static
desktop, changed regions identify the old and new cursor positions. The bounding
box's top-left is used as the cursor tip, including when the glyph is clipped.
One-pixel differences from the arithmetic are visible in the table.

All mouse reports include the required mode byte: `0x02` for absolute, `0x01`
for relative. Reports without it were acknowledged but had no target effect.
See [protocol](../../../docs/protocol.md) and the
`mouse_reports_need_a_leading_mode_byte` packet-fixture entry.

## Absolute

| File | Sent | Measured cursor tip | What it shows |
|---|---|---|---|
| `ref-park-3000-3000.jpg` | x=3000 y=3000 | (1406, 790) | The reference frame every diff is taken against. `floor(3000*1920/4096)=1406`, `floor(3000*1080/4096)=791`. |
| `abs-0-0.jpg` | x=0 y=0 | (0, 0) | Value 0 reaches the first pixel on both axes. |
| `abs-2048-2048.jpg` | x=2048 y=2048 | (960, 539) | Half scale supports a full-scale divisor of 4096. |
| `abs-4093-4093.jpg` | x=4093 y=4093 | (1918, 1078) | Still one pixel short of the corner — the mapping is still moving here, it has not saturated. |
| `abs-4095-4095.jpg` | x=4095 y=4095 | (1919, 1079) | Full scale reaches the true bottom-right pixel. Only the cursor's tip column is on screen; changed pixels are at x=1919, y=1078..1079. |
| `abs-4096-4096.jpg` | x=4096 y=4096 | (1919, 1079) | One past full scale. Byte-for-byte the same cursor position as 4095 — it pins at the edge, it does not wrap to the origin. |
| `abs-8191-8191.jpg` | x=8191 y=8191 | (1919, 1079) | Top of the 13-bit field, still pinned at the far edge. |
| `abs-8192-8192.jpg` | x=8192 y=8192 | (0, 0) | The wrap is at 8192, not 4096. One past the 13-bit field and the cursor is back at the origin; the bottom-right 20x20 region is unchanged and the top-left has 550 changed pixels. |
| `abs-20000-20000.jpg` | x=20000 y=20000 | (1695, 952) | `20000 mod 8192 = 3616`; `floor(3616*1920/4096)=1695`, `floor(3616*1080/4096)=953`. The measured interior position supports the mod-8192 mapping. |

## Relative (`SEND_MS_REL_DATA`, mode byte `0x01`)

Buttons are zero in every frame. Nothing was ever clicked.

| File | Sent | Measured cursor tip | Delta from previous |
|---|---|---|---|
| `rel-base.jpg` | abs x=2048 y=2048 | (960, 539) | — (starting point) |
| `rel-dx-p30.jpg` | dx=+30 dy=0 | (975, 539) | +15, 0 |
| `rel-dx-p30b.jpg` | dx=+30 dy=0 | (993, 539) | +18, 0 |
| `rel-dy-p30.jpg` | dx=0 dy=+30 | (993, 556) | 0, +17 |
| `rel-dy-p30b.jpg` | dx=0 dy=+30 | (993, 574) | 0, +18 |
| `rel-dxdy-m30.jpg` | dx=-30 dy=-30 | (973, 555) | -20, -19 |

+dx moves right, +dy moves down. Magnitudes are smaller than the delta sent
because the target applies pointer acceleration to relative motion.

## Reproduction

The original tool, analysis script, and input script are on the historical
`stage-0` branch at `spikes/absrange/` and `docs/stage0/absrange.md`. Run them
from a separate checkout after verifying device identity and reading the
[hardware restrictions](../../../docs/development.md#hardware-work).

The analysis requires the original target resolution and unscaled capture.
No mouse buttons were pressed during this recording.
