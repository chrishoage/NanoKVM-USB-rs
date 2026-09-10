#!/usr/bin/env python3
"""Confirm the NanoKVM-USB is present, correctly paired, and answering.

Run this before blaming your code. It checks, in order: both device nodes exist and are
usable, they belong to the same physical dongle, and the CH9329 answers GET_INFO with a
frame whose checksum recomputes.

Uses only the standard library, and deliberately does not depend on anything under spikes/,
which is throwaway Stage 0 code. Read-only apart from one GET_INFO request. Exits non-zero if
anything is wrong.

Usage:  scripts/device-health.py [video-node] [tty-node]      (defaults: video4 ttyACM1)
"""
import os
import sys
import termios
import time

GET_INFO = bytes([0x57, 0xAB, 0x00, 0x01, 0x00, 0x03])
BAUD = termios.B57600


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


def check_pairing(video, tty, report):
    """NATIVE_CLIENT_PLAN §8. Same USB device is proof; failing that, the kernel's port
    `peer` link proves the two sit on one physical connector."""
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

    peer = os.path.join(port_dir_of(v), "peer")
    if os.path.exists(peer):
        peer_dev = os.path.realpath(os.path.join(os.path.realpath(peer), "device"))
        if os.path.isdir(peer_dev) and (s == peer_dev or s.startswith(peer_dev + os.sep)):
            report(True, "pairing", f"serial sits under the USB2 peer of the video port "
                                    f"({os.path.basename(peer_dev)}) — proof")
            return True

    common = os.path.commonpath([v, s])
    if os.path.exists(os.path.join(common, "idVendor")):
        report(True, "pairing", f"both contained in {os.path.basename(common)} — weaker proof")
        return True

    report(False, "pairing", "no evidence these are one device; use explicit --serial/--video")
    return False


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
        get_info(tty, report)

    print()
    if failures:
        print(f"FAIL: {len(failures)} check(s) failed: {', '.join(failures)}")
        return 1
    print("OK: device present, paired, and answering")
    return 0


if __name__ == "__main__":
    sys.exit(main())
