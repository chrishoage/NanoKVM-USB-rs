# MJPEG decode corpus

Recorded on 2026-09-09 from a NanoKVM-USB capture interface (`345f:2133`).
The 120 JPEGs, about 32 MB, are local files excluded from Git. Capture tests
that read this corpus need those files; a fresh clone contains this manifest only.

## Recording conditions

- Capture: `/dev/video4`, USB `4-2.2`, product “USB3 Video”, serial `20210623`,
  hardware revision `0x3100`, USB 3.20 at 5 Gbps.
- Target: Raspberry Pi 3B, Raspberry Pi OS desktop, static wallpaper and idle pointer.
- Host: Linux 7.2.2-arch1-1, uvcvideo parameters
  `clock=CLOCK_MONOTONIC hwtimestamps=0 nodrop=1`.
- Format: MJPEG only; enumeration and USB descriptors offered no uncompressed format.

The capture tool negotiated MJPG and frame rate, allocated four MMAP buffers,
then wrote each dequeued buffer through `bytesused` without re-encoding.
The original tool and detailed recording notes are on the historical `stage-0`
branch at `spikes/capture/` and `docs/stage0/capture.md`.

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

All files are baseline, 8-bit, three-component YCbCr 4:2:0 JPEGs with restart
markers. Every file within a resolution group is byte-identical.

The “obs.” rates cover only six startup frames and are not sustained throughput.
The original notes report advertised rates over longer 200–600 frame runs;
see [hardware measurements](../../docs/hardware.md) for scope and limits.

## Interpretation

This corpus measures decoding of one low-entropy image per resolution. It does
not cover busy consoles, motion, or worst-case frame size. Do not size a pipeline
from these timings alone.

The 4K frames contain an upscaled source at or below 1080p. Downscaling a 4K frame
to 1080p yielded luma PSNR 43.8 dB against the recorded 1080p frame. They remain
valid 3840 × 2160 decode workloads, but do not demonstrate native 4K source detail.

The 720 × 576, 720 × 480, and 640 × 480 frames stretch the full 16:9 source to the
requested aspect ratio. They test that distortion, not a native 4:3 target.

## Reproduction

Use the capture tool from the historical branch in a separate checkout. Its
arguments are device, width, height, frame count, output directory, prefix, fps:

```sh
cargo build --release --manifest-path spikes/capture/Cargo.toml
./spikes/capture/target/release/capture-spike /dev/video4 1920 1080 60 fixtures/frames mjpeg-1920x1080 30
```

Review the device identity and target restrictions in
[development](../../docs/development.md#hardware-work) before recording. Repeat
for the groups above; a new desktop image will produce a different corpus.
