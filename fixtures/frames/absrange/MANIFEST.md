# fixtures/frames/absrange — closed-loop evidence for §11 q3

Captured 2026-09-09 by `spikes/absrange` (`absrange-spike`) against the live rig:
CH9329 serial link `/dev/ttyACM1`, capture node `/dev/video4`, target a Raspberry
Pi 3B running the Raspberry Pi OS desktop at **1920x1080**. Negotiated capture
format `MJPG 1920x1080 @ 30 fps` (`S_FMT` then `S_PARM`, per docs/stage0/capture.md).
Capture is 1:1 with the target framebuffer, so a pixel in these frames is a pixel
on the Pi's screen.

All frames are full resolution and unmodified — deliberately **not** downscaled,
because at the screen edges the cursor is clipped to one or two pixels and any
resampling destroys exactly the evidence these frames exist to carry.

Method: `ref-park-3000-3000.jpg` is the reference. Every other frame is compared
against it with `spikes/absrange/analyse.py`; the desktop is a static wallpaper so
the absolute difference contains only the cursor at its reference position and the
cursor at its new position. Reported position is the bounding-box top-left of the
changed region, which is the arrow cursor's tip (its hotspot) and stays correct
when the glyph is clipped at a screen edge.

**Every mouse frame here uses the corrected payload with its leading mode byte**
(`0x02` for absolute, `0x01` for relative). See docs/stage0/absrange.md and the
`mouse_reports_need_a_leading_mode_byte` disagreement in
`fixtures/packets/ch9329.toml`. Frames sent without it are ACKed and ignored.

## Absolute

| File | Sent | Measured cursor tip | What it shows |
|---|---|---|---|
| `ref-park-3000-3000.jpg` | x=3000 y=3000 | (1406, 790) | The reference frame every diff is taken against. `floor(3000*1920/4096)=1406`, `floor(3000*1080/4096)=791`. |
| `abs-0-0.jpg` | x=0 y=0 | (0, 0) | Value 0 reaches the first pixel on both axes. |
| `abs-2048-2048.jpg` | x=2048 y=2048 | (960, 539) | Half scale lands dead centre. This is the single frame that fixes the full-scale divisor at **4096**, not 32768. |
| `abs-4093-4093.jpg` | x=4093 y=4093 | (1918, 1078) | Still one pixel short of the corner — the mapping is still moving here, it has not saturated. |
| `abs-4095-4095.jpg` | x=4095 y=4095 | (1919, 1079) | **Full scale reaches the true bottom-right pixel.** Only the cursor's tip column is on screen; changed pixels are at x=1919, y=1078..1079. |
| `abs-4096-4096.jpg` | x=4096 y=4096 | (1919, 1079) | One past full scale. Byte-for-byte the same cursor position as 4095 — it **pins at the edge, it does not wrap to the origin**. This refutes hypothesis 2. |
| `abs-8191-8191.jpg` | x=8191 y=8191 | (1919, 1079) | Top of the 13-bit field, still pinned at the far edge. |
| `abs-8192-8192.jpg` | x=8192 y=8192 | (0, 0) | **The wrap is at 8192, not 4096.** One past the 13-bit field and the cursor is back at the origin; the bottom-right 20x20 region is unchanged and the top-left has 550 changed pixels. |
| `abs-20000-20000.jpg` | x=20000 y=20000 | (1695, 952) | `20000 mod 8192 = 3616`; `floor(3616*1920/4096)=1695`, `floor(3616*1080/4096)=953`. A large value landing at a precisely predicted interior position, which is what makes the mod-8192 law more than curve-fitting. |

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

**+dx moves right, +dy moves down.** Magnitudes are smaller than the delta sent
because the target applies pointer acceleration to relative motion; see
docs/stage0/absrange.md.

## Reproducing

```
cd spikes/absrange && cargo build --release
./target/release/absrange-spike /dev/ttyACM1 /dev/video4 <outdir> 400 <script>
python3 analyse.py <outdir>/ref-park-3000-3000.jpg <outdir>/abs-*.jpg
```

The script file used for this set is reproduced in docs/stage0/absrange.md.
