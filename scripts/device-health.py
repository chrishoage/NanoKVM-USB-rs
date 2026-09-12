#!/usr/bin/env python3
"""Check device identity, pairing, and CH9329 information replies.

Usage: scripts/device-health.py [video-node] [tty-node]
Defaults: video4 ttyACM1. Verify device identities before using these defaults.

Reads sysfs and device metadata, then sends GET_INFO. Node, pairing, and serial
failures return nonzero. Audio format checks are informational because a missing
sound card does not prevent video or keyboard control.

Requires only Python's standard library. For reset/rebind diagnostics, see
usb-replug.py; this script does not reset devices."""
import os
import sys
import termios
import time

GET_INFO = bytes([0x57, 0xAB, 0x00, 0x01, 0x00, 0x03])
BAUD = termios.B57600

# The three vendor:product ids the containment rule requires. Keep in step with the constants of
# the same names in src/discovery/mod.rs; this script and that module implement one rule, and a
# health check that accepts what the client rejects is worse than no health check.
VIDEO_ID = ("345f", "2133")
SERIAL_ID = ("1a86", "55d3")
INTERNAL_HUB_ID = ("1a40", "0101")


def usb_device_of(sysfs_link):
    d = os.path.realpath(sysfs_link)
    while d != "/" and not os.path.exists(os.path.join(d, "idVendor")):
        d = os.path.dirname(d)
    return d if os.path.exists(os.path.join(d, "idVendor")) else None


def attr(d, name):
    try:
        return open(os.path.join(d, name)).read().strip()
    except OSError:
        return None


def port_dir_of(dev):
    """The sysfs hub-port device this USB device is plugged into."""
    name = os.path.basename(dev)
    parent = os.path.dirname(dev)
    bus, path = name.split("-", 1)
    port = path.split(".")[-1]
    hub = os.path.basename(parent)
    if hub.startswith("usb"):
        return os.path.join(parent, f"{bus}-0:1.0", f"usb{bus}-port{port}")
    return os.path.join(parent, f"{hub}:1.0", f"{hub}-port{port}")


def usb_id(d):
    return (attr(d, "idVendor"), attr(d, "idProduct"))


def check_pairing(video, tty, report):
    """Check same-device identity, port-peer links, then internal-hub containment.

    Containment requires matching video, serial, and hub IDs, but those commodity
    IDs remain weaker evidence than a kernel identity or peer link. Keep verdict
    strings consistent with discovery::Evidence; Rust tests check that agreement."""
    v = usb_device_of(f"/sys/class/video4linux/{video}/device")
    s = usb_device_of(f"/sys/class/tty/{tty}/device")
    if not v or not s:
        report(False, "pairing", "could not resolve a node to its USB device")
        return False

    report(True, "video device", f"{os.path.basename(v)} "
                                 f"{attr(v,'idVendor')}:{attr(v,'idProduct')} ({attr(v,'product')})")
    report(True, "serial device", f"{os.path.basename(s)} "
                                  f"{attr(s,'idVendor')}:{attr(s,'idProduct')} ({attr(s,'product')})")

    if (attr(v, "busnum"), attr(v, "devnum")) == (attr(s, "busnum"), attr(s, "devnum")):
        report(True, "pairing", "same USB device (busnum:devnum) — proof")
        return True

    video_port = port_dir_of(v)
    peer = os.path.join(video_port, "peer")
    if os.path.exists(peer):
        peer_port = os.path.realpath(peer)
        peer_dev = os.path.realpath(os.path.join(peer_port, "device"))
        if os.path.isdir(peer_dev) and (s == peer_dev or s.startswith(peer_dev + os.sep)):
            report(True, "pairing",
                   f"port peer: the serial device sits under {os.path.basename(peer_port)}, "
                   f"which the kernel declares the peer of the video device's port "
                   f"{os.path.basename(video_port)} — proof")
            return True

    hub = os.path.commonpath([v, s])
    while hub != "/" and not os.path.exists(os.path.join(hub, "idVendor")):
        hub = os.path.dirname(hub)
    if (os.path.exists(os.path.join(hub, "idVendor"))
            and os.path.dirname(v) == hub and os.path.dirname(s) == hub
            and usb_id(hub) == INTERNAL_HUB_ID
            and usb_id(v) == VIDEO_ID and usb_id(s) == SERIAL_ID):
        report(True, "pairing",
               f"internal hub (degraded): both are direct children of the dongle's own hub "
               f"{os.path.basename(hub)} {attr(hub,'idVendor')}:{attr(hub,'idProduct')} "
               f"— an internal-hub containment test, contained by that hub and by all three vendor ids, "
               f"which are commodity part numbers rather than a kernel assertion")
        return True

    report(False, "pairing", "no evidence these are one device; use explicit --serial/--video")
    return False


def sound_card_of(usb_device):
    """Return the first (cardN, sysfs path) on the video USB device, or None.

    Hub containment is insufficient: another device under that hub may own audio.
    Read card numbers at runtime because replug can change them."""
    base = "/sys/class/sound"
    if not os.path.isdir(base):
        return None
    for name in sorted(os.listdir(base)):
        if not name.startswith("card") or not name[4:].isdigit():
            continue
        link = os.path.join(base, name, "device")
        if not os.path.exists(link):
            continue
        if usb_device_of(link) == usb_device:
            return name, os.path.realpath(os.path.join(base, name))
    return None


# What /proc/asound/cardN/stream0 must say. Measured on this desk 2026-09-11: the dongle offers
# one capture stream and exactly one format, so anything else means this is not that card.
AUDIO_FORMAT = (("Format: S16_LE", "S16_LE"),
                ("Channels: 2", "2 channels"),
                ("Rates: 48000", "48000 Hz"))


def check_audio(video, report):
    """Report the paired card's advertised capture format without opening PCM.

    Read sysfs and /proc/asound to avoid taking a busy card from its current user.
    Audio findings are informational and do not determine the script's exit code."""
    usb = usb_device_of(f"/sys/class/video4linux/{video}/device")
    if not usb:
        report(False, "audio card", "could not resolve the video node to its USB device")
        return False
    found = sound_card_of(usb)
    if not found:
        report(False, "audio card", f"no sound card belongs to {os.path.basename(usb)}; is "
                                    "snd-usb-audio loaded?")
        return False
    card, _ = found
    number = card[4:]
    report(True, "audio card", f"{card} (hw:{number}) on {os.path.basename(usb)} "
                               f"-- same USB device as the capture node, proof")

    stream = f"/proc/asound/{card}/stream0"
    try:
        text = open(stream).read()
    except OSError as e:
        report(False, "audio format", f"{stream}: {e}")
        return False
    if "Capture:" not in text:
        report(False, "audio format", f"{stream} declares no capture stream")
        return False
    missing = [label for needle, label in AUDIO_FORMAT if needle not in text]
    if missing:
        report(False, "audio format",
               f"{stream} does not declare {', '.join(missing)} -- this client opens hw:{number} "
               "with exactly S16_LE/2ch/48000 and nothing else")
        return False
    report(True, "audio format", "S16_LE, 2 channels, 48000 Hz capture (stream0)")
    return True


def get_info(tty, report):
    path = f"/dev/{tty}"
    try:
        fd = os.open(path, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    except OSError as e:
        report(False, "serial open", f"{path}: {e}")
        return False
    try:
        a = termios.tcgetattr(fd)
        iflag, oflag, cflag, lflag, ispeed, ospeed, cc = a
        cflag = (cflag | termios.CLOCAL | termios.CREAD) & ~termios.CSIZE
        cflag = (cflag | termios.CS8) & ~(termios.PARENB | termios.CSTOPB | termios.CRTSCTS)
        iflag &= ~(termios.IXON | termios.IXOFF | termios.IXANY | termios.INLCR | termios.ICRNL)
        oflag &= ~termios.OPOST
        lflag &= ~(termios.ICANON | termios.ECHO | termios.ECHOE | termios.ISIG)
        cc = list(cc)
        cc[termios.VMIN], cc[termios.VTIME] = 0, 5
        termios.tcsetattr(fd, termios.TCSANOW,
                          [iflag, oflag, cflag, lflag, BAUD, BAUD, cc])
        termios.tcflush(fd, termios.TCIOFLUSH)

        os.write(fd, GET_INFO)
        deadline, buf = time.monotonic() + 1.0, b""
        while time.monotonic() < deadline and len(buf) < 14:
            try:
                chunk = os.read(fd, 64)
                if chunk:
                    buf += chunk
                    continue
            except BlockingIOError:
                pass
            time.sleep(0.01)
    finally:
        os.close(fd)

    hexs = " ".join(f"{b:02X}" for b in buf)
    if len(buf) < 14:
        report(False, "GET_INFO", f"short reply ({len(buf)} bytes): {hexs or '<nothing>'}")
        return False
    if (sum(buf[:-1]) & 0xFF) != buf[-1]:
        report(False, "GET_INFO", f"checksum mismatch: {hexs}")
        return False

    version = 1.0 + (buf[5] - 0x30) / 10
    connected, locks = bool(buf[6]), buf[7]
    report(True, "GET_INFO", f"chip v{version:.1f}, target connected={connected}, "
                             f"lockBits=0x{locks:02X}")
    if locks:
        report(False, "lock state", f"lockBits 0x{locks:02X} — a lock key is active on the "
                                    "target; something may have been left held")
        return False
    return True


def main():
    video = sys.argv[1] if len(sys.argv) > 1 else "video4"
    tty = sys.argv[2] if len(sys.argv) > 2 else "ttyACM1"
    failures = []

    def report(ok, label, detail):
        print(f"  [{'ok' if ok else 'FAIL'}] {label:16} {detail}")
        if not ok:
            failures.append(label)

    def note(ok, label, detail):
        """Report without a verdict. For the side channel: seen, said, not counted."""
        print(f"  [{'ok' if ok else 'note'}] {label:16} {detail}")

    print(f"NanoKVM-USB health check: /dev/{video}, /dev/{tty}\n")

    for node in (f"/dev/{video}", f"/dev/{tty}"):
        if not os.path.exists(node):
            report(False, "node present", f"{node} does not exist")
        elif not os.access(node, os.R_OK | os.W_OK):
            report(False, "node access", f"{node} exists but is not read/write for this user")
        else:
            report(True, "node present", node)

    if not failures:
        check_pairing(video, tty, report)
        # Audio before GET_INFO: it opens nothing and cannot disturb anything, and a missing card
        # is worth reporting even on a desk whose serial link has wedged. It is reported through
        # `note`, so nothing it finds can fail this script -- see `check_audio`.
        check_audio(video, note)
        get_info(tty, report)

    print()
    if failures:
        print(f"FAIL: {len(failures)} check(s) failed: {', '.join(failures)}")
        return 1
    print("OK: device present, paired, and answering")
    return 0


if __name__ == "__main__":
    sys.exit(main())
