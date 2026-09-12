# Hardware measurements and limitations

These results summarize the repository's recorded hardware work from September
9–12, 2026. They have not been re-measured during the documentation rewrite.
They describe one setup, not guarantees for every dongle, target, or host.

The original reports remain in Git history. The `stage-0` branch contains the
prototype tools and raw capture/serial/topology reports under `docs/stage0/`,
`spikes/`, and `fixtures/traffic/`. Later reports referenced session-local screenshots,
WAV files, and logs that were not committed; their observations are retained here,
but those raw artifacts are not available from a fresh checkout.

## Test setup

| Component | Recorded setup |
| --- | --- |
| Dongle | Sipeed NanoKVM-USB Pro 4K60; CH9329 firmware 1.8. |
| Target | Raspberry Pi 3B, Raspberry Pi OS desktop, normally 1920 × 1080. |
| Host | Arch Linux, kernel 7.2.2-arch1-1; AMD Ryzen 9 9950X3D. |
| Desktop | niri 26.04 (`8ed0da4`), Wayland, 144 Hz display. |
| GPU | AMD Radeon RX 6800 XT, RADV. |
| Rust | 1.89.0. |
| USB | Initial prototypes used SuperSpeed; subsequent viewer and recovery work used USB 2.0. |

On USB 2.0, video `345f:2133` and serial `1a86:55d3` appeared as direct children
of the dongle's `1a40:0101` hub. The audio card belonged to the video device's USB
Audio Class interface. Node and card numbers changed after resets. Product strings
also varied with link speed (`USB2 Video`/`USB3 Video`), as did the video iSerial
(`20210621`/`20210623`). None of those strings establishes unit identity.

## Video

The unit advertised MJPEG only. It rejected YUYV. Initial sustained capture tests
reached the advertised rates; short startup bursts were not reliable rate measurements.
The USB 2.0 device exposed 1920 × 1080, 1280 × 720, 720 × 576, 720 × 480, and
640 × 480 sizes. Higher-resolution evidence comes from the earlier SuperSpeed work.

After an idle period, the device emitted up to eight frames at its previous resolution.
The frames carried no reliable error or sequence indication. `S_FMT` alone left
1080p and 720p at a 240 fps default, so the client explicitly calls `S_PARM`.

| Measurement | Recorded result | Qualification |
| --- | --- | --- |
| 1080p60 capture | 60.00 fps over 1.983 s | USB 2.0; buffer timestamps. |
| Capture-to-dequeue age | p50 14.77 ms | Monotonic start-of-exposure timestamp to dequeue; not display latency. |
| 1080p decode | median 2.36–2.40 ms | Release build, strict zune-jpeg 0.5.15, reused RGBA buffer, idle desktop. |
| 720p decode | median 0.98 ms | Same benchmark method. |
| 1440p decode | median 4.16 ms | Same benchmark method. |
| 4K decode | median 10.01 ms | Upscaled idle desktop; low-entropy content. |
| 4K synthetic busy input | 16.36 ms | Approximately 2.4× the compressed size; almost the full 60 fps budget. |
| Stream restart | 8.7 ms call; first frame 73.5 ms later | Single measured device. |

The 120-frame bulk corpus contains byte-identical frames within each resolution.
It measures decoder throughput for one image at each size. Its 4K frames are upscaled
from a lower-resolution HDMI source. Real busy-screen performance remains unmeasured;
1080p60 has substantially more measured decode headroom than 4K60. See the
[corpus manifest](../fixtures/frames/MANIFEST.md) for sizes and provenance.

During a roughly 46-second target reboot outage, the dongle kept emitting 1080p frames
at about 60 fps. Four of 14,378 JPEGs were truncated around signal transitions; the
pipeline retained the last valid frame. A target change to 720p also left the negotiated
capture and JPEG dimensions at 1080p, with three decode errors around transitions.
The device rescales internally. No reliable HDMI signal-state interface was found.

A USB reset could leave the device streaming 640 × 480 while the driver still
reported 1920 × 1080. In one viewer run, 516.7 ms of mismatches triggered a restart,
and the next frame returned to 1080p. Recovery tests reopened capture and received
30 frames within 1.17 seconds. Both recorded capture-replug runs renumbered the nodes.

## Input and serial

The device acknowledged malformed mouse payloads without producing motion. Mouse
verification therefore used captured cursor positions; keyboard verification used
returned lock-state changes and text visible on the target.

| Measurement | Recorded result |
| --- | --- |
| Keyboard acknowledgement | About 4.16–4.19 ms; one release took 15.49 ms alongside a lock-state notification. |
| Mouse acknowledgement | About 17 ms per relative or absolute report. |
| Serial write call | About 50 µs. |
| Zero-preamble write | 40–67 µs. |
| Torn frame followed by preamble | `GET_INFO` answered in 7.86 ms. |
| Torn frame without preamble | `GET_INFO` timed out at 500.11 ms. |
| Idle link-loss detection | At most 95.8 ms in the recorded reset test. |
| Serial reconnect | 781.6 ms over three attempts for a 414 ms device outage. |

Early throughput runs also reported aggregate rates of 332 keyboard, 91 relative,
and 83 absolute reports per second. Those are separate measurements from the round-trip
figures and should not be treated as their reciprocals. Overdriving the link produced
checksum errors rather than useful backpressure. Live pointer motion coalesced roughly
9:1; raising baud was not investigated because acknowledgement pacing dominated.

Absolute coordinates followed a 4096-divisor law with usable range 0–4095 and a
13-bit mask. The [committed image evidence](../fixtures/frames/absrange/MANIFEST.md)
includes edge clamping and wrap at 8192. Relative +30 reports moved 15–18 pixels in
several samples, demonstrating target acceleration rather than a fixed pixel distance.

## Viewer and UI

An unlocked 8.5-minute viewer run at 1080p60 on a 144 Hz host display measured:

- Capture-to-submit age p50 around 18–19 ms, maximum at most 21 ms.
- Event-handler duration p99 at most 4 µs and maximum 9 µs under pointer activity.
- No decode errors, queue overflows, or unmapped keys in that run.
- Absolute and relative control, focus-loss release, and niri Mod+Escape release.

These measurements predate the floating controls. With those controls, the recorded
render-thread UI draw cost was p50 8–26 µs; the integrated audio/paste run measured
22 µs median draw and 117 µs median UI build. Those later runs were on a locked
session. Their 0–4 input samples do not establish interactive event-handler latency.

Both wgpu versions tested offered Mailbox and Fifo on niri. The client chose Fifo.
On the unlocked 144 Hz display, presenting at 60 fps returned immediately; on a locked
session it could block about one second. Unthrottled prototype rendering also blocked
for a refresh period. These differing conditions explain the differing timings.

A detached render thread once crashed while destroying EGL objects after the Wayland
connection closed. Retaining GPU objects until process exit was followed by 16 runs
without a crash, seven of which exercised detachment. This is evidence for the current
shutdown policy, not proof that every backend or lifetime arrangement is covered.

## Audio and paste

Native capture and playback granted 480 frames per period and 1,920 frames of device
buffer at 48 kHz stereo. Capture disabled resampling; the host default playback allowed it.

A target-generated 1 kHz tone was found in captured PCM with a Goertzel ratio of
689,857 between silent controls of 0.018 and 1.0. The host test sink's own monitor
recorded a ratio of 2,713,257 after routing was checked by object ID. This established
both capture and playback, rather than only successful opens or nonzero counters.

Two 720-second drift runs each recorded 71,999 captured periods, 72,000 written
periods, one inserted period, and zero overruns or underruns: about −13.9 ppm, with
the host clock faster. The eight-period ring provides scheduling headroom; that
clock difference alone takes roughly twelve minutes to move occupancy by one period.
The ring's 80 ms capacity is not an end-to-end latency measurement.

Fresh capture opens returned about 2 ms of stale audio in the first period, even
several seconds after a tone stopped. Hardware measurements discard that first period
to keep their silent controls valid. Playback does not discard it. One playback open
hung; it did not recur in 146 further opens. The UI now displays opening state and
supervises overdue opens instead of claiming audio is running.

Clipboard testing included all printable ASCII in a 101-byte sample. Its target-side
hash matched the host file after 296 key transitions and about 12 seconds. An
unsupported-character refusal left before/after screenshots byte-identical. Canceling
a longer paste at three seconds stopped after 75 of 4,080 transitions; subsequent
text had the expected case, with no stuck modifier observed.

## Remaining validation and design limits

- Audio disappearance during a hardware reset was not exercised because the run lacked
  USB reset permissions. Same-port card renumbering is covered by fake sysfs tests.
- Audio after moving to another physical port requires restart by design; that scenario
  has not been observed live. SuperSpeed audio topology has not been recorded.
- Interactive use of the floating controls and unlocked presentation timings need a
  human pass. A compositor without clipboard data-control has only fake/UI test coverage.
- A bare Super press can reach the target during Mod+Escape release and open its menu.
- The 256-barrier input bound is count-based. With long transaction timeouts it can
  represent substantial backlog; a time-based admission limit remains unimplemented.
- Some hardware tests require the originally recorded node names. Review their identity
  checks before running them on a different setup.
- `--sysfs-root` affects initial selection but does not replace the viewer's reopen
  resolver. The old startup tests still read the host's sysfs.
- A handoff ordering test had one reported failure under parallel load that did not
  recur in five retries. No cause was established.
- VT-switch bindings, cross-architecture bindgen builds, other compositors, and other
  dongle models have not been verified. Baud reconfiguration and rollover reports
  were not investigated because the client does not use them.
