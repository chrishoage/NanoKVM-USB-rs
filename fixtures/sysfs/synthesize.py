#!/usr/bin/env python3
"""Materialise every sysfs fixture tree from the one file that is committed.

`usb2-desk.sysfs` is the recording — this desk, 2026-09-10, plus the 2026-09-11 sound addendum,
serialised by `scripts/snapshot-sysfs.py` (its own header comment says which is which). It holds
the captured part and nothing else. This script expands it into `usb2-desk/`, writes back the
bus-5 negative control the dock took with it when it unplugged itself mid-session, and builds
the three trees that are not on this desk and cannot be: the SuperSpeed shape needs the dongle
on a USB 3 port during the USB 2.0 recording, and the two-dongle ambiguity needs a second
dongle. Every device it writes is listed in MANIFEST.md with
the source of its attribute values, so the trees stay auditable.

The four trees are build output and are **not committed** (.gitignore). `cargo test`
materialises them on demand — `src/discovery/testing.rs` runs this script when a tree it was
asked for is missing — so python3 is a test-time dependency of this crate.

It is idempotent and writes only under `fixtures/sysfs/`. Each tree is built under a temporary
name and renamed into place, so a concurrent reader sees either the old tree or the new one and
never a half-built one. Run from anywhere:

    python3 fixtures/sysfs/synthesize.py [--force]
"""
import argparse
import importlib.util
import os
import shutil
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
DESK = os.path.join(HERE, "usb2-desk")
SNAPSHOT = os.path.join(HERE, "usb2-desk.sysfs")
REPO = os.path.dirname(os.path.dirname(HERE))   # the crate root


def _snapshot_module():
    """`scripts/snapshot-sysfs.py` owns the snapshot format; import it rather than restate it."""
    path = os.path.join(REPO, "scripts", "snapshot-sysfs.py")
    spec = importlib.util.spec_from_file_location("snapshot_sysfs", path)
    module = importlib.util.module_from_spec(spec)
    sys.dont_write_bytecode = True      # no __pycache__ next to the fixtures
    spec.loader.exec_module(module)
    return module


def publish(build, final):
    """Rename a freshly built tree over the old one, leaving no half-built tree visible."""
    old = f"{final}.old.{os.getpid()}"
    shutil.rmtree(old, ignore_errors=True)
    if os.path.isdir(final):
        os.rename(final, old)
    os.rename(build, final)
    shutil.rmtree(old, ignore_errors=True)


def build_dir(final):
    """A scratch path next to `final`, so the rename that publishes it stays on one filesystem."""
    build = f"{final}.new.{os.getpid()}"
    shutil.rmtree(build, ignore_errors=True)
    return build

# The two xHCI controllers on this desk, as they appear in the captured tree.
C34 = "devices/pci0000:00/0000:00:08.1/0000:0b:00.3"   # buses 3 (HS) and 4 (SS)
C5 = "devices/pci0000:00/0000:00:08.1/0000:0b:00.4"    # bus 5 (the dock)


def write(root, relpath, attrs):
    """Create a sysfs directory with attribute files."""
    d = os.path.join(root, relpath)
    os.makedirs(d, exist_ok=True)
    for k, v in attrs.items():
        with open(os.path.join(d, k), "w") as fh:
            fh.write(v + "\n")
    return d


def link(root, relpath, target_relpath):
    """Create a relative symlink, replacing any existing one."""
    src = os.path.join(root, relpath)
    os.makedirs(os.path.dirname(src), exist_ok=True)
    if os.path.lexists(src):
        os.unlink(src)
    os.symlink(os.path.relpath(os.path.join(root, target_relpath),
                               os.path.dirname(src)), src)


def usb_device(root, path, name, **attrs):
    """A USB device directory plus its /bus/usb/devices alias."""
    write(root, path, attrs)
    link(root, f"bus/usb/devices/{name}", path)


def usb_interface(root, path, name, **attrs):
    write(root, path, attrs)
    link(root, f"bus/usb/devices/{name}", path)


def class_node(root, cls, node_name, iface_path, **attrs):
    """A class node under its interface, plus the /class/<cls>/<name> alias and `device` link."""
    node = f"{iface_path}/{cls}/{node_name}"
    write(root, node, attrs)
    link(root, f"{node}/device", iface_path)
    link(root, f"class/{cls}/{node_name}", node)


def port(root, hub_path, hub_name, n, device_path=None, peer_path=None):
    """A hub-port directory, named the way the kernel names it.

    An ordinary hub `3-2.2` puts its ports under its own interface 0: `3-2.2:1.0/3-2.2-port2`.
    A root hub is spelled `usb<B>` as a device but `<B>-0:1.0` as an interface, so its ports are
    `usb4/4-0:1.0/usb4-port2`. Both `discovery::port_dir_of` and `port_dir_of` in
    `scripts/device-health.py` build these two forms, and a fixture that got them wrong would
    quietly test nothing.
    """
    if hub_name.startswith("usb"):
        iface = f"{hub_name[3:]}-0:1.0"
    else:
        iface = f"{hub_name}:1.0"
    p = f"{hub_path}/{iface}/{hub_name}-port{n}"
    write(root, p, {})
    if device_path:
        link(root, f"{p}/device", device_path)
    if peer_path:
        link(root, f"{p}/peer", peer_path)


# --------------------------------------------------------------------------------------------
# 1. usb2-desk: the bus-5 negative control.
#
# The dock carrying the user's webcam and an unrelated CDC-ACM device disconnected between the
# readout that opened this session and the snapshot run. Its attribute values below are that
# readout verbatim; MANIFEST.md reproduces it. The subtree matters because it is the shape a loose
# loose common-ancestor wording would have paired by accident: a video device and a serial device
# that are direct children of one generic hub.
# --------------------------------------------------------------------------------------------
def bus5_negative_control(root=DESK):
    base = f"{C5}/usb5/5-1"
    assert os.path.isdir(os.path.join(root, base)), "usb2-desk is missing usb5/5-1"

    usb_device(root, f"{base}/5-1.4", "5-1.4",
               idVendor="2109", idProduct="2817", busnum="5", devnum="89", speed="480",
               bDeviceClass="09", product="USB2.0 Hub             ",
               manufacturer="VIA Labs, Inc.         ", serial="000000000")
    usb_interface(root, f"{base}/5-1.4/5-1.4:1.0", "5-1.4:1.0",
                  bInterfaceNumber="00", bInterfaceClass="09")

    usb_device(root, f"{base}/5-1.4/5-1.4.4", "5-1.4.4",
               idVendor="2109", idProduct="2817", busnum="5", devnum="91", speed="480",
               bDeviceClass="09", product="USB2.0 Hub             ",
               manufacturer="VIA Labs, Inc.         ", serial="000000000")
    usb_interface(root, f"{base}/5-1.4/5-1.4.4/5-1.4.4:1.0", "5-1.4.4:1.0",
                  bInterfaceNumber="00", bInterfaceClass="09")

    hub = f"{base}/5-1.4/5-1.4.4/5-1.4.4.4"
    usb_device(root, hub, "5-1.4.4.4",
               idVendor="0bda", idProduct="5411", busnum="5", devnum="94", speed="480",
               bDeviceClass="09", product="USB2.1 Hub", manufacturer="Generic")
    usb_interface(root, f"{hub}/5-1.4.4.4:1.0", "5-1.4.4.4:1.0",
                  bInterfaceNumber="00", bInterfaceClass="09")

    cam = f"{hub}/5-1.4.4.4.2"
    usb_device(root, cam, "5-1.4.4.4.2",
               idVendor="046d", idProduct="086b", busnum="5", devnum="98", speed="480",
               bDeviceClass="ef", product="Logi 4K Stream Edition", serial="476C95B2")
    usb_interface(root, f"{cam}/5-1.4.4.4.2:1.0", "5-1.4.4.4.2:1.0",
                  bInterfaceNumber="00", bInterfaceClass="0e")
    for i in range(4):
        class_node(root, "video4linux", f"video{i}", f"{cam}/5-1.4.4.4.2:1.0",
                   name="Logi 4K Stream Edition", index=str(i), dev=f"81:{i}")
    # The webcam's microphone: a USB Audio Class interface on the *same* USB device as its video
    # interface, which is exactly the shape a loose evidence 1 pairs on. It is here so that the
    # audio rule has a reconstructed negative control: the card pairs with the *webcam*, and must
    # never be offered to the dongle. (The measured version of the same control is `card0` on
    # `5-1.1.1`, which is in the recording itself.) **Reconstructed, and further from measurement
    # than the rest of this subtree** -- see MANIFEST.md. The interface number, the card number
    # and the ALSA `id` string were never read on this desk, and no test asserts on any of those
    # values: `tests/discovery.rs` finds this card by its USB device, not by its name.
    usb_interface(root, f"{cam}/5-1.4.4.4.2:1.2", "5-1.4.4.4.2:1.2",
                  bInterfaceNumber="02", bInterfaceClass="01")
    class_node(root, "sound", "card7", f"{cam}/5-1.4.4.4.2:1.2", id="Edition", number="7")

    acm = f"{hub}/5-1.4.4.4.3"
    usb_device(root, acm, "5-1.4.4.4.3",
               idVendor="043e", idProduct="9a8a", busnum="5", devnum="99", speed="12",
               bDeviceClass="ef", product="LG Monitor Controls",
               manufacturer="LG Electronics Inc.", serial="F2110200Z666")
    usb_interface(root, f"{acm}/5-1.4.4.4.3:1.2", "5-1.4.4.4.3:1.2",
                  bInterfaceNumber="02", bInterfaceClass="02")
    class_node(root, "tty", "ttyACM0", f"{acm}/5-1.4.4.4.3:1.2", dev="166:0")

    # Hub ports for the two devices. No `peer`: the readout of the live tree was taken before the
    # dock vanished and did not include this hub's port directories, so nothing is claimed about
    # them beyond what is plugged in where. A peer here could only point at an empty SuperSpeed
    # port on the companion bus, which cannot pair anything.
    port(root, hub, "5-1.4.4.4", 2, device_path=cam)
    port(root, hub, "5-1.4.4.4", 3, device_path=acm)


# --------------------------------------------------------------------------------------------
# SuperSpeed reconstruction; see MANIFEST.md for historical provenance.
# --------------------------------------------------------------------------------------------
def usb3_stage0():
    final = os.path.join(HERE, "usb3-stage0")
    root = build_dir(final)

    u3, u4 = f"{C34}/usb3", f"{C34}/usb4"
    usb_device(root, u3, "usb3", idVendor="1d6b", idProduct="0002", busnum="3", devnum="1",
               speed="480", bDeviceClass="09", product="xHCI Host Controller",
               manufacturer="Linux 7.2.2-arch1-1 xhci-hcd", serial="0000:0b:00.3")
    usb_interface(root, f"{u3}/3-0:1.0", "3-0:1.0", bInterfaceNumber="00", bInterfaceClass="09")
    usb_device(root, u4, "usb4", idVendor="1d6b", idProduct="0003", busnum="4", devnum="1",
               speed="10000", bDeviceClass="09", product="xHCI Host Controller",
               manufacturer="Linux 7.2.2-arch1-1 xhci-hcd", serial="0000:0b:00.3")
    usb_interface(root, f"{u4}/4-0:1.0", "4-0:1.0", bInterfaceNumber="00", bInterfaceClass="09")

    # The external dock hub: one physical connector, two logical hubs, ports peered.
    hs = f"{u3}/3-2"
    usb_device(root, hs, "3-2", idVendor="0bda", idProduct="5423", busnum="3", devnum="26",
               speed="480", bDeviceClass="09", product="4-Port USB 2.0 Hub",
               manufacturer="Generic")
    usb_interface(root, f"{hs}/3-2:1.0", "3-2:1.0", bInterfaceNumber="00", bInterfaceClass="09")
    ss = f"{u4}/4-2"
    usb_device(root, ss, "4-2", idVendor="0bda", idProduct="0423", busnum="4", devnum="2",
               speed="5000", bDeviceClass="09", product="4-Port USB 3.0 Hub",
               manufacturer="Generic")
    usb_interface(root, f"{ss}/4-2:1.0", "4-2:1.0", bInterfaceNumber="00", bInterfaceClass="09")
    port(root, u3, "usb3", 2, device_path=hs, peer_path=f"{u4}/4-0:1.0/usb4-port2")
    port(root, u4, "usb4", 2, device_path=ss, peer_path=f"{u3}/3-0:1.0/usb3-port2")

    # The dongle's own hub, on the HS side, with the CH9329 bridge on its port 4.
    ihub = f"{hs}/3-2.2"
    usb_device(root, ihub, "3-2.2", idVendor="1a40", idProduct="0101", busnum="3", devnum="31",
               speed="480", bDeviceClass="09", product="USB2.0 HUB")
    usb_interface(root, f"{ihub}/3-2.2:1.0", "3-2.2:1.0",
                  bInterfaceNumber="00", bInterfaceClass="09")
    ser = f"{ihub}/3-2.2.4"
    usb_device(root, ser, "3-2.2.4", idVendor="1a86", idProduct="55d3", busnum="3", devnum="4",
               speed="12", bDeviceClass="02", product="USB Single Serial", serial="5C37176280")
    usb_interface(root, f"{ser}/3-2.2.4:1.0", "3-2.2.4:1.0",
                  bInterfaceNumber="00", bInterfaceClass="02")
    class_node(root, "tty", "ttyACM1", f"{ser}/3-2.2.4:1.0", dev="166:1")
    port(root, ihub, "3-2.2", 4, device_path=ser)

    # The video device, SuperSpeed, on the other bus entirely.
    vid = f"{ss}/4-2.2"
    usb_device(root, vid, "4-2.2", idVendor="345f", idProduct="2133", busnum="4", devnum="5",
               speed="5000", bDeviceClass="ef", product="USB3 Video",
               manufacturer="MACROSILICON", serial="20210623")
    usb_interface(root, f"{vid}/4-2.2:1.0", "4-2.2:1.0",
                  bInterfaceNumber="00", bInterfaceClass="0e")
    class_node(root, "video4linux", "video4", f"{vid}/4-2.2:1.0",
               name="USB3 Video: USB3 Video", index="0", dev="81:4")
    class_node(root, "video4linux", "video5", f"{vid}/4-2.2:1.0",
               name="USB3 Video: USB3 Video", index="1", dev="81:5")

    # The peer pair that is the whole point of this fixture: the video device's port on bus 4 and
    # the port the dongle's internal hub occupies on bus 3 are one connector.
    port(root, ss, "4-2", 2, device_path=vid, peer_path=f"{hs}/3-2:1.0/3-2-port2")
    port(root, hs, "3-2", 2, device_path=ihub, peer_path=f"{ss}/4-2:1.0/4-2-port2")
    publish(root, final)


# --------------------------------------------------------------------------------------------
# 3. usb3-rootport: the same SuperSpeed shape with no external hub in the way.
#
# The dongle plugged straight into a motherboard USB 3 port. It is the arrangement most users
# will have, and it is the only fixture that exercises the *root-hub* spelling of the port
# directory: a device on a root hub is `4-2`, its hub directory is `usb4`, and its port is
# `usb4/4-0:1.0/usb4-port2` rather than `<hub>/<hub>:1.0/<hub>-portN`. Reconstructed, not
# captured -- this desk has no free SuperSpeed port -- from the same topology.md attribute values
# usb3-stage0 uses, with the external dock hub removed and both devices moved up one level.
# --------------------------------------------------------------------------------------------
def usb3_rootport():
    final = os.path.join(HERE, "usb3-rootport")
    root = build_dir(final)

    u3, u4 = f"{C34}/usb3", f"{C34}/usb4"
    usb_device(root, u3, "usb3", idVendor="1d6b", idProduct="0002", busnum="3", devnum="1",
               speed="480", bDeviceClass="09", product="xHCI Host Controller",
               manufacturer="Linux 7.2.2-arch1-1 xhci-hcd", serial="0000:0b:00.3")
    usb_interface(root, f"{u3}/3-0:1.0", "3-0:1.0", bInterfaceNumber="00", bInterfaceClass="09")
    usb_device(root, u4, "usb4", idVendor="1d6b", idProduct="0003", busnum="4", devnum="1",
               speed="10000", bDeviceClass="09", product="xHCI Host Controller",
               manufacturer="Linux 7.2.2-arch1-1 xhci-hcd", serial="0000:0b:00.3")
    usb_interface(root, f"{u4}/4-0:1.0", "4-0:1.0", bInterfaceNumber="00", bInterfaceClass="09")

    # The dongle's own hub on the HS root port, the CH9329 bridge on its port 4.
    ihub = f"{u3}/3-2"
    usb_device(root, ihub, "3-2", idVendor="1a40", idProduct="0101", busnum="3", devnum="31",
               speed="480", bDeviceClass="09", product="USB2.0 HUB")
    usb_interface(root, f"{ihub}/3-2:1.0", "3-2:1.0", bInterfaceNumber="00", bInterfaceClass="09")
    ser = f"{ihub}/3-2.4"
    usb_device(root, ser, "3-2.4", idVendor="1a86", idProduct="55d3", busnum="3", devnum="4",
               speed="12", bDeviceClass="02", product="USB Single Serial", serial="5C37176280")
    usb_interface(root, f"{ser}/3-2.4:1.0", "3-2.4:1.0",
                  bInterfaceNumber="00", bInterfaceClass="02")
    class_node(root, "tty", "ttyACM1", f"{ser}/3-2.4:1.0", dev="166:1")
    port(root, ihub, "3-2", 4, device_path=ser)

    # The video function, SuperSpeed, on the SS root port that is the same physical connector.
    vid = f"{u4}/4-2"
    usb_device(root, vid, "4-2", idVendor="345f", idProduct="2133", busnum="4", devnum="5",
               speed="5000", bDeviceClass="ef", product="USB3 Video",
               manufacturer="MACROSILICON", serial="20210623")
    usb_interface(root, f"{vid}/4-2:1.0", "4-2:1.0", bInterfaceNumber="00", bInterfaceClass="0e")
    class_node(root, "video4linux", "video4", f"{vid}/4-2:1.0",
               name="USB3 Video: USB3 Video", index="0", dev="81:4")
    class_node(root, "video4linux", "video5", f"{vid}/4-2:1.0",
               name="USB3 Video: USB3 Video", index="1", dev="81:5")

    # The root-hub port pair. These are the directories the root-hub branch of `port_dir_of`
    # constructs, and nothing else in the fixture set reaches them.
    port(root, u4, "usb4", 2, device_path=vid, peer_path=f"{u3}/3-0:1.0/usb3-port2")
    port(root, u3, "usb3", 2, device_path=ihub, peer_path=f"{u4}/4-0:1.0/usb4-port2")
    publish(root, final)


# --------------------------------------------------------------------------------------------
# 4. two-dongles: usb2-desk plus a second dongle on port 3 of the same external hub.
# --------------------------------------------------------------------------------------------
def two_dongles():
    final = os.path.join(HERE, "two-dongles")
    root = build_dir(final)
    shutil.copytree(DESK, root, symlinks=True)

    hs = f"{C34}/usb3/3-2"
    ihub = f"{hs}/3-2.3"
    usb_device(root, ihub, "3-2.3", idVendor="1a40", idProduct="0101", busnum="3", devnum="41",
               speed="480", bDeviceClass="09", product="USB2.0 HUB")
    usb_interface(root, f"{ihub}/3-2.3:1.0", "3-2.3:1.0",
                  bInterfaceNumber="00", bInterfaceClass="09")

    vid = f"{ihub}/3-2.3.2"
    usb_device(root, vid, "3-2.3.2", idVendor="345f", idProduct="2133", busnum="3", devnum="44",
               speed="480", bDeviceClass="ef", product="USB2 Video",
               manufacturer="MACROSILICON", serial="20210621")
    usb_interface(root, f"{vid}/3-2.3.2:1.0", "3-2.3.2:1.0",
                  bInterfaceNumber="00", bInterfaceClass="0e")
    class_node(root, "video4linux", "video6", f"{vid}/3-2.3.2:1.0",
               name="USB2 Video: USB2 Video", index="0", dev="81:6")
    class_node(root, "video4linux", "video7", f"{vid}/3-2.3.2:1.0",
               name="USB2 Video: USB2 Video", index="1", dev="81:7")
    # The second unit's own sound card, on its own UAC control interface, exactly as the first
    # unit's card8 sits on 3-2.2.2:1.2 in the recording. It is what makes this tree the fixture
    # for "the card came back under a different number after a replug": one resolver asked across
    # a change of tree sees hw:8 become hw:10 (`discovery::reopen`'s tests).
    usb_interface(root, f"{vid}/3-2.3.2:1.2", "3-2.3.2:1.2",
                  bInterfaceNumber="02", bInterfaceClass="01")
    class_node(root, "sound", "card10", f"{vid}/3-2.3.2:1.2", id="Video_1", number="10")

    ser = f"{ihub}/3-2.3.4"
    usb_device(root, ser, "3-2.3.4", idVendor="1a86", idProduct="55d3", busnum="3", devnum="43",
               speed="12", bDeviceClass="02", product="USB Single Serial", serial="5C37176281")
    usb_interface(root, f"{ser}/3-2.3.4:1.0", "3-2.3.4:1.0",
                  bInterfaceNumber="00", bInterfaceClass="02")
    class_node(root, "tty", "ttyACM2", f"{ser}/3-2.3.4:1.0", dev="166:2")

    # An ordinary USB sound card on port 3 of the *second dongle's own internal hub*. It is the
    # negative control the audio rule needs and nothing else in the fixture set provides: a sound
    # card contained under the dongle's hub, alongside its capture node, on a different USB
    # device. hub containment would pair it; evidence 1 -- identical busnum:devnum,
    # which is the only rule `discovery::audio_for` implements -- must not. The hub has four
    # ports and two of them are free, so nothing about the arrangement is exotic. The ids,
    # product string and ALSA `id` are this desk's own `5-1.1.1` (0d8c:0016, card0 "Device",
    # read 2026-09-11); its position here is synthetic, like the rest of this tree.
    snd = f"{ihub}/3-2.3.3"
    usb_device(root, snd, "3-2.3.3", idVendor="0d8c", idProduct="0016", busnum="3", devnum="42",
               speed="12", bDeviceClass="00", product="USB Audio Device", manufacturer="C-Media")
    usb_interface(root, f"{snd}/3-2.3.3:1.0", "3-2.3.3:1.0",
                  bInterfaceNumber="00", bInterfaceClass="01")
    class_node(root, "sound", "card9", f"{snd}/3-2.3.3:1.0", id="Device", number="9")

    port(root, hs, "3-2", 3, device_path=ihub)
    port(root, ihub, "3-2.3", 2, device_path=vid)
    port(root, ihub, "3-2.3", 3, device_path=snd)
    port(root, ihub, "3-2.3", 4, device_path=ser)
    publish(root, final)


def usb2_desk(force=False):
    """Expand the recording, then write the bus-5 negative control back into it.

    When the tree is already there the control is restored in place, which is the cheap
    idempotent path; only a missing tree (or `--force`) pays for a full expand.
    """
    if os.path.isdir(DESK) and not force:
        bus5_negative_control()
        return "bus-5 negative control restored (webcam + unrelated CDC-ACM)"
    if not os.path.isfile(SNAPSHOT):
        raise SystemExit(f"error: {SNAPSHOT} missing; this is a broken checkout")
    root = build_dir(DESK)
    n = _snapshot_module().expand(SNAPSHOT, root)
    bus5_negative_control(root)
    publish(root, DESK)
    return f"expanded from usb2-desk.sysfs ({n} records) + bus-5 negative control"


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--force", action="store_true",
                    help="re-expand usb2-desk/ from usb2-desk.sysfs even if it is already there")
    args = ap.parse_args()

    print(f"usb2-desk     {usb2_desk(args.force)}")
    usb3_stage0()
    print("usb3-stage0   built from historical SuperSpeed topology (see MANIFEST.md)")
    usb3_rootport()
    print("usb3-rootport built from historical SuperSpeed topology (see MANIFEST.md), dongle straight into a root port")
    two_dongles()
    print("two-dongles   built from usb2-desk + a second dongle on 3-2.3")
    return 0


if __name__ == "__main__":
    sys.exit(main())
