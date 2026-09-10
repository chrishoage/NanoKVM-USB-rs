# Capture fixtures — NanoKVM-USB (MACROSILICON 345f:2133, `/dev/video4`)

Produced by Stage 0 spike #3 ("Capture"). See `docs/stage0/capture.md` for the
full findings and `spikes/capture/` for the program that wrote these.

The `.jpg` files themselves are **gitignored** (`.gitignore: /fixtures/frames/*.jpg`).
This manifest is not — it is the committed record of what was captured.

## Provenance

- Device: `/dev/video4`, `uvcvideo`, USB `4-2.2`, `345f:2133 MACROSILICON "USB3 Video"`,
  `iSerial 20210623`, hw revision `0x3100`, USB 3.20 @ 5 Gbps.
- Source: Raspberry Pi 3B over HDMI, showing the Raspberry Pi OS desktop
  (static wallpaper, taskbar clock reading 18:30, idle mouse pointer).
- Host: Linux 7.2.2-arch1-1, `uvcvideo` params `clock=CLOCK_MONOTONIC hwtimestamps=0 nodrop=1`.
- Captured: 2026-09-09.
- Only `MJPG` is offered by this device — there is **no YUYV / uncompressed format**
  (confirmed in both `VIDIOC_ENUM_FMT` and the raw USB VideoStreaming descriptors),
  so there is no raw-frame fixture to capture.

## How they were captured

```
cargo build --release --manifest-path spikes/capture/Cargo.toml

# capture-spike <device> <width> <height> <frames> <outdir> <prefix> <fps>
./spikes/capture/target/release/capture-spike /dev/video4 3840 2160 30 fixtures/frames mjpeg-3840x2160 30
./spikes/capture/target/release/capture-spike /dev/video4 1920 1080 60 fixtures/frames mjpeg-1920x1080 30
./spikes/capture/target/release/capture-spike /dev/video4 1280 720   6 fixtures/frames mjpeg-1280x720  60
./spikes/capture/target/release/capture-spike /dev/video4 2560 1440  6 fixtures/frames mjpeg-2560x1440 60
./spikes/capture/target/release/capture-spike /dev/video4  640  480  6 fixtures/frames mjpeg-640x480   60
./spikes/capture/target/release/capture-spike /dev/video4  720  576  6 fixtures/frames mjpeg-720x576   60
./spikes/capture/target/release/capture-spike /dev/video4  720  480  6 fixtures/frames mjpeg-720x480   60
```

Each is `VIDIOC_S_FMT` (MJPG at the given size) → `VIDIOC_S_PARM` (given fps) →
`VIDIOC_REQBUFS`(4, MMAP) → `STREAMON` → N × `DQBUF`, writing `buf[..bytesused]`
verbatim to disk. The bytes on disk are the exact UVC payload the device produced;
nothing re-encodes them.

Per-run logs (frame-by-frame `bytesused`, `flags`, `timestamp`) are in
`fixtures/traffic/capture-spike-<W>x<H>.txt`.

## Files

| Group | Files | Format | V4L2 negotiated | JPEG SOF dims | Frames | Bytes/frame | Group total | Capture fps |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `mjpeg-3840x2160-NN.jpg` | 01–30 | MJPEG | 3840x2160 MJPG | 3840x2160 | 30 | 665,461 (all identical) | 19,963,830 | 30.00 |
| `mjpeg-2560x1440-NN.jpg` | 01–06 | MJPEG | 2560x1440 MJPG | 2560x1440 | 6 | 280,951 (all identical) | 1,685,706 | 50.00 obs. |
| `mjpeg-1920x1080-NN.jpg` | 01–60 | MJPEG | 1920x1080 MJPG | 1920x1080 | 60 | 171,945 (all identical) | 10,316,700 | 30.00 |
| `mjpeg-1280x720-NN.jpg` | 01–06 | MJPEG | 1280x720 MJPG | 1280x720 | 6 | 74,362 (all identical) | 446,172 | 60.00 |
| `mjpeg-720x576-NN.jpg` | 01–06 | MJPEG | 720x576 MJPG | 720x576 | 6 | 36,380 (all identical) | 218,280 | 27.17 obs. |
| `mjpeg-720x480-NN.jpg` | 01–06 | MJPEG | 720x480 MJPG | 720x480 | 6 | 31,805 (all identical) | 190,830 | 22.16 obs. |
| `mjpeg-640x480-NN.jpg` | 01–06 | MJPEG | 640x480 MJPG | 640x480 | 6 | 28,688 (all identical) | 172,128 | 60.04 |

Total on disk: 120 files, ~32 MB. All baseline (non-progressive) JPEG, 8-bit,
3 components, YCbCr 4:2:0, with restart markers (`FFD0`–`FFD7`) present.

The "obs." fps figures are the rate actually observed from the buffer timestamps
over a 6-frame burst and are dominated by stream start-up; the sustained figures
measured over 200–600 frames are in `fixtures/traffic/sustained-throughput.txt`
and every advertised mode hit its advertised rate there.

## Caveats for the decode-benchmark spike (§11 q5) — read this

1. **Every frame within a group is byte-identical** (`md5sum` → 1 unique hash per
   group). The Pi's desktop is static, so the encoder produces the same bitstream
   each frame. This is fine for isolating decoder throughput but it means the
   corpus exercises exactly one image's entropy per resolution.
2. **This is a low-entropy image** — a smooth gradient wallpaper. A busy console
   with scrolling text will produce substantially larger frames and slower
   decodes. Decode times measured on this corpus are a **lower bound**, not a
   worst case. Do not size the pipeline budget from these numbers alone.
3. **The 4K frames carry no more real detail than the 1080p frames.** Downscaling
   `mjpeg-3840x2160-01.jpg` to 1920x1080 and comparing against
   `mjpeg-1920x1080-01.jpg` gives PSNR y=43.8 dB — the device upscales a ≤1080p
   HDMI source to whatever UVC resolution is negotiated (see `docs/stage0/capture.md` §6).
   The 4K frames are still valid *decode* workloads (real 3840x2160 JPEG,
   665 KB each), they just do not represent a genuine 4K source.
4. **The non-16:9 frames (`720x576`, `720x480`, `640x480`) are anamorphically
   stretched**, not letterboxed or cropped: the device squeezes the full 16:9
   source into the requested aspect ratio. Useful as a distortion fixture,
   useless as a "what a 4:3 target looks like" fixture.
