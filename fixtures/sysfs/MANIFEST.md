# Discovery fixtures

Discovery tests use relocatable sysfs trees with relative symlinks. The fixtures
cover USB 2.0 hub containment, SuperSpeed port peers, unrelated hardware, and
ambiguous selection without opening devices on the test host.

## Sources and generation

| Source | Contents |
| --- | --- |
| `usb2-desk.sysfs` | Sysfs recording from 2026-09-10, with 20 sound records added from 2026-09-11. |
| `synthesize.py` | Expands the recording, adds reconstructed controls, and generates three other topologies. |

Expanded directories are ignored build output. Tests generate them through
`discovery::testing::fixture`, so Python 3 is required. To regenerate manually:

```sh
python3 fixtures/sysfs/synthesize.py
python3 fixtures/sysfs/synthesize.py --force
```

The recording and reconstructions have different evidentiary value. Attributes
invented for a test case are identified below; they are not hardware measurements.

## Snapshot format

One record per line, sorted by path:

```text
#nanokvm-sysfs 1
d <path>
f <path>\t<value>
l <path>\t<target>
```

`d` preserves empty directories, `f` stores attribute bytes, and `l` stores a
relative symlink target. Paths are relative to the tree root. Backslash, tab,
newline, and carriage return use `\\`, `\t`, `\n`, and `\r`; other nonprintable
bytes use `\xNN`. Additional `#` lines are provenance comments.

`scripts/snapshot-sysfs.py --pack DIR FILE` and `--expand FILE DIR` convert
between the text and tree forms. Capture copies discovery attributes, device
and peer links, and hub-port directories. It also follows non-USB class-device
links so PCI sound cards resolve correctly instead of becoming dangling links.

## USB 2.0 recording: `usb2-desk/`

Recorded on Arch Linux, kernel 7.2.2-arch1-1. The original capture included
26 USB devices, 40 interfaces, 65 hub ports, and 342 attributes. Its 475 records
precede the sound addendum. The generator adds the reconstructed dock below.

```text
usb3 ── 3-2      0bda:5423  external hub
        └ 3-2.2  1a40:0101  dongle internal hub
          ├ 3-2.2.2  345f:2133  video4 (capture), video5 (metadata)
          └ 3-2.2.4  1a86:55d3  ttyACM1
```

There is no relevant peer link. Pairing therefore uses the weaker internal-hub
heuristic: both nodes are direct children, and all three vendor/product IDs match.
These are commodity IDs, not unique device identity.

The capture device exposes UVC control/streaming on interfaces `:1.0`/`:1.1`,
UAC control/streaming on `:1.2`/`:1.3`, and HID on `:1.4`. Sound `card8`, ID
`Video`, resolves through `:1.2` to the same USB device as the video node.
The recorded `/proc/asound/card8/stream0` advertised S16_LE, two channels,
48000 Hz, endpoint `0x82`, asynchronous capture, 1000 µs intervals, and no
playback stream. Those PCM descriptors are not part of this sysfs fixture.

The sound addendum contains class aliases, device links, IDs, and numbers for
cards 0, 1, 2, 3, and 8. It leaves the older USB attributes intact; this is a
merged recording, not one simultaneous snapshot. Cards 1–3 are PCI codecs and
resolve to no USB device. Card 0 (`0d8c:0016`, ID `Device`, port `5-1.1.1`) is
a recorded unrelated USB sound card. It is read from sysfs only, never opened.

Capture and metadata nodes share their USB identity. Recorded QUERYCAP values
provide the fake capability probe used by tests:

| Node | `capabilities` | `device_caps` |
| --- | --- | --- |
| `/dev/video4` | `0x84a00001` | `0x04200001` (capture, streaming, extended pixel format) |
| `/dev/video5` | `0x84a00001` | `0x04a00000` (metadata, streaming, extended pixel format) |

Only per-node `device_caps` distinguishes them. Never probe these recorded
paths against the current host to obtain fixture answers.

### Reconstructed dock control

The dock disappeared before the snapshot. The generator reconstructs it from
this earlier readout:

```
5-1.4          2109:2817 speed=480  bus=5 dev=89 class=09 prod='USB2.0 Hub             '
                                                          manu='VIA Labs, Inc.         ' ser=000000000
5-1.4.4        2109:2817 speed=480  bus=5 dev=91 class=09  (same strings)
5-1.4.4.4      0bda:5411 speed=480  bus=5 dev=94 class=09 prod='USB2.1 Hub' manu=Generic
5-1.4.4.4.2    046d:086b speed=480  bus=5 dev=98 class=ef prod='Logi 4K Stream Edition' ser=476C95B2
5-1.4.4.4.3    043e:9a8a speed=12   bus=5 dev=99 class=ef prod='LG Monitor Controls'
                                                          manu='LG Electronics Inc.' ser=F2110200Z666

/sys/class/video4linux/video0..3 -> .../5-1.4.4.4.2/5-1.4.4.4.2:1.0   index 0..3, dev 81:0..81:3,
                                                                      name "Logi 4K Stream Edition"
/sys/class/tty/ttyACM0           -> .../5-1.4.4.4.3/5-1.4.4.4.3:1.2   dev 166:0
```


The webcam and serial device must not pair. Their IDs reject the ordinary case;
a separate test replaces both IDs with the dongle's IDs to isolate rejection
by the generic hub's ID.

Three limitations apply:

- Peer links were not recorded and are omitted.
- Video0–3 were never opened, so capture versus metadata is unknown. Fake probes
  accept all four as capture nodes to make the negative control conservative.
- The webcam's sound card is synthetic. Interface `:1.2`, card number 7, and ID
  `Edition` are invented. Tests depend on its owning USB device, not those values.

## SuperSpeed reconstruction: `usb3-stage0/`

The fixture name is historical. Source attributes come from
`git show stage-0:docs/stage0/topology.md`; the tree itself was generated, not
captured. The video device is `4-2.2`, `345f:2133`, serial `20210623`, at 5000 Mbps.
The bridge is `3-2.2.4`, `1a86:55d3`, serial `5C37176280`, at 12 Mbps under the
internal `1a40:0101` hub `3-2.2`.

The external hub's `4-2-port2` peer points to `3-2-port2`, whose device is the
internal hub. This kernel port relationship supplies pairing evidence across
the two buses. External hub IDs (`0bda:0423`/`0bda:5423`), root-hub IDs, and the
PCI path were filled from the host's live tree; the historical topology notes
named the hub but did not print its IDs. Video4 and video5 share interface `:1.0`.
No sound cards were recorded for this topology; their absence tests optional audio.

## Root-port reconstruction: `usb3-rootport/`

Uses the same device attributes with the external hub removed:

```text
usb4 ── 4-2      345f:2133  video4, video5
        └ usb4-port2 --peer--> usb3-port2
usb3 ── 3-2      1a40:0101  internal hub
        └ 3-2.4  1a86:55d3  ttyACM1
```

This synthetic arrangement exercises root-hub port spelling (`usb4/4-0:1.0/usb4-port2`)
which differs from external-hub paths. Expected evidence is `PortPeer`.

## Ambiguity reconstruction: `two-dongles/`

Adds a synthetic second dongle to the USB 2.0 recording:

```text
3-2.3      1a40:0101  internal hub, devnum 41
  ├ 3-2.3.2  345f:2133  video6, video7; card10 “Video_1” on :1.2
  ├ 3-2.3.3  0d8c:0016  unrelated sound card9
  └ 3-2.3.4  1a86:55d3  ttyACM2, serial 5C37176281
```

Without an override, discovery must list both pairs as ambiguous. Either an
explicit video or serial path selects one. The bridge serial distinguishes
bridges but does not establish which video device belongs to one.

Card9 tests that sharing the dongle's hub is insufficient for audio pairing.
Its IDs and product strings come from the recorded card0, but its placement
is synthetic. Only card10 shares the video USB device. Moving a resolver between
trees also tests card renumbering from `hw:8` to `hw:10`.

## Updating the recording

The generator owns the reconstructed dock subtree. Exclude it from a new live
snapshot so it cannot silently overwrite measured data:

```sh
python3 scripts/snapshot-sysfs.py --root /sys --file --force \
    fixtures/sysfs/usb2-desk.sysfs \
    --exclude 'bus/usb/devices/5-1.4*' \
    --exclude 'devices/*/usb5/5-1/5-1.4*' \
    --exclude 'class/video4linux/video[0-3]' \
    --exclude 'class/tty/ttyACM0' \
    --exclude 'devices/*/usb5/5-1/5-1.4*/sound/*'
python3 fixtures/sysfs/synthesize.py --force
```

These paths describe the recorded setup. Review current topology first. If the
dock is connected, also exclude its current `class/sound/cardN` aliases. Keep
unrelated recorded cards outside that subtree as negative controls; if a live
card collides with synthetic card7, renumber the synthetic card.

The writer does not preserve provenance comments. Restore a dated comment block
identifying any merged records, and update this manifest. Remove exclusions only
when replacing the corresponding reconstruction with measured data.
