# sysfs fixtures — what each tree is and where it came from

These are the authority for `tests/discovery.rs` (§9.1: the recording is the authority, never a
retyped description of it). Each is a plain directory tree laid out exactly like `/sys`, so
`RealSysfs::new(<dir>)` reads it with no special casing. All symlinks inside are **relative**, so
the trees are relocatable.

They exist because plan §8's two pairing shapes are two different physical arrangements of the
same dongle and **no desk shows both at once**. Stage 0 measured the SuperSpeed shape; Stage 1
lost the SuperSpeed link and measured the USB 2.0 fallback (`docs/STAGE1_FINDINGS.md`,
"Environment"). Without recordings, whichever shape the cable is not in today is untested.

## What is committed, and what is built

Only two files in this directory are source:

| file | what it is |
| --- | --- |
| `usb2-desk.sysfs` | **the recording.** This desk, 2026-09-10, serialised to one line-oriented text file. The captured part and nothing else — the reconstructed bus-5 control below is not in it. |
| `synthesize.py` | expands that file into `usb2-desk/`, writes the bus-5 control back into it, and builds the other three trees from it. |

The four directories are build output and are **gitignored**. Expanded they are about 1300
one-line files; committed they were a diff nobody could read. Nothing is lost: the tree and the
file are the same information, and `snapshot-sysfs.py --pack`/`--expand` round-trip between them.

**Materialisation is automatic.** `nanokvm::discovery::testing::fixture` (the opener behind
`tests/discovery.rs` and the lib tests) runs `python3 fixtures/sysfs/synthesize.py` the first
time a tree it was asked for is missing — once per process, under
`fixtures/sysfs/.synthesize.lock` so concurrent test binaries do not race, and each tree is
renamed into place rather than built in place. **python3 is therefore a test-time dependency of
this crate.** It cannot be avoided by expanding the file in Rust: the `usb2-desk` tree the tests
read is the recording *plus* the reconstructed bus-5 negative control, and the other three trees
are reconstructions — all of which live in `synthesize.py`.

To build them by hand, or after editing `synthesize.py`:

```
python3 fixtures/sysfs/synthesize.py          # missing trees only, plus the bus-5 control
python3 fixtures/sysfs/synthesize.py --force  # re-expand usb2-desk/ from the recording too
```

## The snapshot format

One record per line, sorted bytewise by path, so a diff reads one device at a time:

```
#nanokvm-sysfs 1            header, first line
d <path>                    a directory with nothing in it (an unoccupied hub port)
f <path>\t<value>           an attribute file and its exact contents
l <path>\t<target>          a symlink and its target, verbatim (always relative)
```

Paths are relative to the tree root. Paths, values and targets are escaped to pure ASCII —
`\\`, `\t`, `\n`, `\r`, and `\xNN` for every other byte outside printable ASCII — so the format is
lossless for arbitrary attribute bytes and never depends on the reader's locale. `d` records
exist because a hub port with nothing plugged into it is a real, empty directory and discovery
walks it.

## Re-recording this desk

```
python3 scripts/snapshot-sysfs.py --root /sys --file --force \
    fixtures/sysfs/usb2-desk.sysfs \
    --exclude 'bus/usb/devices/5-1.4*' \
    --exclude 'devices/*/usb5/5-1/5-1.4*' \
    --exclude 'class/video4linux/video[0-3]' \
    --exclude 'class/tty/ttyACM0'
python3 fixtures/sysfs/synthesize.py --force
```

The four `--exclude` globs are exactly the 88 paths `synthesize.py`'s `bus5_negative_control()`
writes. They are there so the recording stays *only* what was captured: if the dock happens to be
plugged in during a re-record, the live bus-5 subtree would land in the file and then be
overwritten by the reconstruction anyway, and nobody could tell afterwards which of the two the
file held. Drop the globs only if you have also deleted `bus5_negative_control()` because the
subtree was genuinely captured — and then say so here.

`--file` writes the snapshot; without it the same command writes a directory tree
(`scripts/snapshot-sysfs.py fixtures/sysfs/usb2-desk --force`), which is still supported and is
what `--pack`/`--expand` convert between. Either way `snapshot-sysfs.py` copies only the
attributes discovery reads — `idVendor idProduct product manufacturer serial busnum devnum speed
bDeviceClass bInterfaceNumber bInterfaceClass name dev index` — plus the `device` and `peer`
symlinks and the hub-port directories. Nothing else about the machine is recorded.

---

## 1. `usb2-desk/` — captured

**Source:** this desk, 2026-09-10, Arch Linux, kernel 7.2.2-arch1-1, `snapshot-sysfs.py` against
the live `/sys`. 26 USB devices, 40 interfaces, 65 hub ports, 334 attributes. Held as
`usb2-desk.sysfs` (459 records: 334 attribute files + 125 symlinks); the `usb2-desk/` tree is
expanded from it, and the bus-5 subtree below is added afterwards by `synthesize.py`.

This is §8 **evidence 3**, the USB 2.0-only shape the plan lists as untested and Stage 1 hit for
real. The dongle enumerates entirely behind its own internal hub, with no `peer` link anywhere
near it:

```
usb3 ── 3-2      0bda:5423  external 4-port hub, NOT the dongle
        └ 3-2.2  1a40:0101  "USB2.0 HUB"        the hub inside the dongle
          ├ 3-2.2.2  345f:2133  "USB2 Video"    /dev/video4 (capture), /dev/video5 (metadata)
          └ 3-2.2.4  1a86:55d3  "USB Single Serial"  /dev/ttyACM1
```

`/dev/video4` and `/dev/video5` are one USB device and are **indistinguishable from sysfs**. The
capture-versus-metadata split comes from `VIDIOC_QUERYCAP`, measured here with `v4l2-ctl --info`:

| node | `capabilities` | `device_caps` | meaning |
| --- | --- | --- | --- |
| `/dev/video4` | `0x84a00001` | `0x04200001` | VIDEO_CAPTURE \| STREAMING \| EXT_PIX_FORMAT |
| `/dev/video5` | `0x84a00001` | `0x04a00000` | META_CAPTURE \| STREAMING \| EXT_PIX_FORMAT |

Note that the per-device `capabilities` field is identical on both nodes — it ORs every node's
caps together — so only the per-node `device_caps` separates them. That is the field
`v4l::Capabilities::capabilities` actually carries, and what `discovery::RealProbe` reads.

### The bus-5 negative control (reconstructed, see below)

The tree also contains the user's dock, which is the reason `discovery`'s containment rule checks
all three vendor ids:

```
usb5 ── 5-1 ── 5-1.4 ── 5-1.4.4 ── 5-1.4.4.4  0bda:5411  "USB2.1 Hub"  a generic hub
                                    ├ 5-1.4.4.4.2  046d:086b  Logi 4K Stream Edition
                                    │                          /dev/video0..3
                                    └ 5-1.4.4.4.3  043e:9a8a  LG Monitor Controls
                                                               /dev/ttyACM0
```

A video device and a serial device, **direct children of one hub** — exactly the shape §8's
looser wording would have paired. It must not pair, and `tests/discovery.rs` asserts that.

Be precise about *which* check rejects it: `internal_hub` tests the video and serial ids before it
reads the hub, so this subtree is rejected on `046d:086b` not being `345f:2133`. It pins the id
checks, **not** the containment check. The containment check has its own negative control,
`a_generic_hub_with_the_dongles_two_ids_under_it_still_does_not_pair`, which relabels these two
devices with the dongle's ids and leaves the hub as `0bda:5411` so the hub id is the only thing
left that can reject the pair.

**This subtree is reconstructed, not captured.** The dock disconnected between the readout that
opened the session and the snapshot run, so `snapshot-sysfs.py` could not see it.
`fixtures/sysfs/synthesize.py` writes it back from that readout, which was:

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

Two things about it are **not measured** and are recorded here so nobody mistakes them for
evidence:

- **`5-1.4.4.4`'s hub-port directories carry no `peer` links.** The readout was taken before the
  dock vanished and did not include them. Nothing depends on it: both devices are on the USB 2.0
  side, so any real peer would point at an empty SuperSpeed port with no device behind it, which
  cannot pair anything.
- **Whether `/dev/video0`–`3` are capture or metadata nodes is unknown.** `/dev/video0`–`3` are
  unrelated hardware and are never opened (CLAUDE.md), so no `QUERYCAP` was taken. The tests use
  `true` for all four, which is the conservative choice: it makes every one of them an eligible
  capture node, so a pairing rule that were too loose would pair one and the negative-control
  test would catch it.

## 2. `usb3-stage0/` — reconstructed by hand

**Source:** `git show stage-0:docs/stage0/topology.md`, written by
`fixtures/sysfs/synthesize.py`. **Nothing in it was captured from a live machine**, because the
dongle has not been on a SuperSpeed port since Stage 0.

This is §8 **evidence 2**: two devices on two buses, joined only by the kernel's port `peer`
assertion.

| what | from `topology.md` |
| --- | --- |
| `4-2.2` `345f:2133` "USB3 Video", `iSerial 20210623`, 5000M, `busnum:devnum 4:5` | "The two nodes" table |
| `3-2.2.4` `1a86:55d3` "USB Single Serial", `iSerial 5C37176280`, 12M, `busnum:devnum 3:4` | "The two nodes" table |
| `3-2.2` `1a40:0101` "USB2.0 HUB", the dongle's internal hub, CH9329 on its port 4 | "q2 answer", the ASCII topology |
| `4-2` / `3-2`, the dock hub's SuperSpeed and High Speed halves | "q2 answer", the ASCII topology |
| `4-2:1.0/4-2-port2` → `peer` → `3-2:1.0/3-2-port2`, whose `device` is `3-2.2` | "Verified traversal on this unit" |

The dock hub's vendor ids (`0bda:0423` / `0bda:5423`), the root-hub ids and the PCI path are
taken from this desk's live tree, where the same dock is still present — `topology.md` names the
hub ("Realtek 4-port dock hub") but does not print its ids. `/dev/video4` and `/dev/video5` are
both attached to `4-2.2:1.0`, per `topology.md`'s "`/dev/video4` (+ `/dev/video5`)".

## 3. `usb3-rootport/` — reconstructed by hand

**Source:** the same `topology.md` attribute values as `usb3-stage0/`, with the external dock hub
removed and both halves of the dongle moved up one level, written by
`fixtures/sysfs/synthesize.py`. **Nothing in it was captured**; this desk has no free SuperSpeed
port.

It is the arrangement most users will actually have — the dongle plugged straight into a
motherboard USB 3 port — and it is the only tree that exercises the **root-hub spelling of a port
directory**. A device on an external hub is `3-2.2.2` and its port is
`3-2.2/3-2.2:1.0/3-2.2-port2`; a device on a root hub is `4-2` and its port is
`usb4/4-0:1.0/usb4-port2`, a different string built by a different branch of
`discovery::port_dir_of` (and of `port_dir_of` in `scripts/device-health.py`). Before this tree
that branch was covered only by a string-level unit test.

```
usb4 ── 4-2      345f:2133  "USB3 Video"   /dev/video4 (capture), /dev/video5 (metadata)
        └ port usb4-port2 --peer--> usb3-port2
usb3 ── 3-2      1a40:0101  "USB2.0 HUB"   the hub inside the dongle
        └ 3-2.4  1a86:55d3  "USB Single Serial"  /dev/ttyACM1
```

Pairing is §8 **evidence 2**: `PortPeer { video_port: "usb4-port2", peer_port: "usb3-port2" }`.

## 4. `two-dongles/` — synthesized

**Source:** `usb2-desk/` plus a second dongle, written by `fixtures/sysfs/synthesize.py`. There
is only one dongle on this desk; this is the §8 ambiguity case, which the policy says must fail
rather than pick.

The second unit is a copy of the first, on port 3 of the same external hub:

```
3-2.3      1a40:0101  internal hub   (devnum 41)
  ├ 3-2.3.2  345f:2133  USB2 Video   /dev/video6 (capture), /dev/video7 (metadata)
  └ 3-2.3.4  1a86:55d3  Single Serial /dev/ttyACM2   iSerial 5C37176281
```

The serial `iSerial` differs by one digit from the real unit's, which is the only thing that
would distinguish two units — and §8 already demoted `iSerial`, since it identifies the CH9329
bridge and nothing links it to a video device. Discovery must return `Ambiguous` listing both
pairs, and must resolve to one when either `--video` or `--serial` names a node.
