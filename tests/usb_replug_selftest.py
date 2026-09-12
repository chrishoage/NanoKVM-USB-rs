#!/usr/bin/env python3
"""Cases behind `scripts/usb-replug.py --self-test`. No hardware, no root, nothing opened.

Two halves:

  * resolution and refusal, against fake sysfs trees in a temp dir — the four cases the slice
    contract requires, plus the ones the adversarial review added: identity is the sysfs port
    path, so a dongle that comes back as /dev/video0 is still the dongle, a /dev node that
    belongs to another device is refused whatever it is called, and a missing node resolves
    through the other node's hub or through --port.
  * the acting half — `run()`, `do_operation()` and `Watcher` — against a fake Kernel that
    records ioctls and owns a virtual set of /dev nodes. Before the review this half had no
    coverage at all (R9), which is how R1-R6 got in.

Every case here is failure-asserting: it fails if the refusal stops happening, or if the
operation reports the opposite of what the fake kernel did. Nothing in this file opens a
/dev/bus/usb, /dev/video* or /dev/tty* node — the fake Kernel hands out integers.

Run it through the script (`python3 scripts/usb-replug.py --self-test`) or directly.
"""
import errno
import os
import signal
import struct
import sys
import tempfile
import threading
import time
import types

# ------------------------------------------------------------------------ fake sysfs trees


def _write_file(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as fh:
        fh.write(text + "\n")


def _fake_usb_device(parent, name, ids, product, devnum, ifaces=(), busnum=3):
    """One USB device directory in a fake sysfs tree. ifaces: (ifno, class_dir, [node names]).

    bInterfaceNumber is written the way the kernel writes it, `%02x` — interface 16 reads back
    as "10". The old fakes wrote decimal, which agrees with hex only up to 9, so the self-test
    could not tell a correct base from a wrong one.
    """
    dev = os.path.join(parent, name)
    os.makedirs(dev, exist_ok=True)
    vid, pid = ids.split(":")
    _write_file(os.path.join(dev, "idVendor"), vid)
    _write_file(os.path.join(dev, "idProduct"), pid)
    _write_file(os.path.join(dev, "product"), product)
    _write_file(os.path.join(dev, "busnum"), str(busnum))
    _write_file(os.path.join(dev, "devnum"), str(devnum))
    for ifno, cls, nodes in ifaces:
        iface = os.path.join(dev, f"{name}:1.{ifno:x}")
        os.makedirs(iface, exist_ok=True)
        _write_file(os.path.join(iface, "bInterfaceNumber"), f"{ifno:02x}")
        if cls:
            for node in nodes:
                os.makedirs(os.path.join(iface, cls, node), exist_ok=True)
    return dev


def _fake_class_link(root, cls, node_name, iface_dir):
    link_dir = os.path.join(root, "class", cls, node_name)
    os.makedirs(link_dir, exist_ok=True)
    os.symlink(iface_dir, os.path.join(link_dir, "device"))


def _bus_link(root, dev):
    """The /sys/bus/usb/devices/<port> link the kernel keeps; what --port looks up first."""
    devices = os.path.join(root, "bus", "usb", "devices")
    os.makedirs(devices, exist_ok=True)
    link = os.path.join(devices, os.path.basename(dev))
    if not os.path.lexists(link):
        os.symlink(dev, link)


def _build_tree(root, hub_ids="1a40:0101", video_ids="345f:2133", split_parents=False,
                foreign_video=False):
    """A dongle-shaped fake sysfs tree, with knobs for the refusal cases.

    Mirrors the measured topology: 3-2 external hub -> 3-2.2 the dongle's own hub -> 3-2.2.2
    video and 3-2.2.4 serial. `foreign_video` adds an unrelated camera on another bus owning
    /dev/video0, which is how the "not ours" refusal gets tested on a device rather than on a
    name.
    """
    usb = os.path.join(root, "devices", "pci0000:00", "usb3")
    os.makedirs(usb, exist_ok=True)
    ext = _fake_usb_device(usb, "3-2", "0bda:5423", "4-Port USB 2.0 Hub", 26, [(0, None, [])])
    hub = _fake_usb_device(ext, "3-2.2", hub_ids, "USB2.0 HUB", 31, [(0, None, [])])
    video_parent = hub
    if split_parents:
        video_parent = _fake_usb_device(ext, "3-2.3", "1a40:0101", "USB2.0 HUB", 32,
                                        [(0, None, [])])
    vname = "3-2.3.2" if split_parents else "3-2.2.2"
    vdev = _fake_usb_device(video_parent, vname, video_ids, "USB2 Video", 34,
                            [(0, "video4linux", ["video4", "video5"]),
                             (1, None, []), (2, None, [])])
    sdev = _fake_usb_device(hub, "3-2.2.4", "1a86:55d3", "USB Single Serial", 33,
                            [(0, "tty", ["ttyACM1"]), (1, None, [])])
    _fake_class_link(root, "video4linux", "video4", f"{vdev}/{vname}:1.0")
    _fake_class_link(root, "video4linux", "video5", f"{vdev}/{vname}:1.0")
    _fake_class_link(root, "tty", "ttyACM1", f"{sdev}/3-2.2.4:1.0")
    for dev in (ext, hub, video_parent, vdev, sdev):
        _bus_link(root, dev)
    if foreign_video:
        other = os.path.join(root, "devices", "pci0000:00", "usb5")
        os.makedirs(other, exist_ok=True)
        fdev = _fake_usb_device(other, "5-1", "1d6b:0002", "Some Other Camera", 2,
                                [(0, "video4linux", ["video0"])], busnum=5)
        _fake_class_link(root, "video4linux", "video0", f"{fdev}/5-1:1.0")
        _bus_link(root, fdev)
    return root


def _rename_class_nodes(root, iface_dir, cls, mapping):
    """Re-registration under new minor numbers: /dev/video4 comes back as /dev/video0."""
    for old, new in mapping.items():
        os.rename(os.path.join(iface_dir, cls, old), os.path.join(iface_dir, cls, new))
        os.rename(os.path.join(root, "class", cls, old), os.path.join(root, "class", cls, new))


def _drop_class_node(root, cls, node):
    """The /dev node is gone but the USB device is still enumerated — a driver that died."""
    path = os.path.join(root, "class", cls, node)
    os.remove(os.path.join(path, "device"))
    os.rmdir(path)


# ---------------------------------------------------------------------------- the fake kernel


def _descriptor(ids):
    """The first 18 bytes of a usbfs node: a USB device descriptor with this vid:pid."""
    vid, pid = (int(x, 16) for x in ids.split(":"))
    blob = bytearray(18)
    blob[0] = 18  # bLength
    blob[1] = 0x01  # bDescriptorType = DEVICE
    struct.pack_into("<HH", blob, 8, vid, pid)
    return bytes(blob)


class FakeKernel:
    """A usbfs that lives in a dict: no fd is ever a real file descriptor.

    Models just enough kernel to be worth testing against — a descriptor per usbfs path, a
    virtual /dev whose nodes DISCONNECT removes and CONNECT restores, an optional delay before
    a reset's nodes come back, and an optional virtual clock so a 2 s settle window does not
    cost 2 s of wall time.
    """

    real = False

    def __init__(self, ur, devices, nodes=(), reset_delay=None, virtual_clock=False):
        self.ur = ur
        self.devices = dict(devices)  # usbfs path -> {"ids": str, "nodes": [str]}
        self.nodes = set(nodes)
        self.opened = []
        self.ioctls = []  # (usbfs path, request, inner code, ifno)
        self.on_ioctl = None
        self.interrupt_in_gap = False
        self.reset_delay = reset_delay
        self.sigmask_during = []
        self._fds = {}
        self._next_fd = 100
        self._lock = threading.Lock()
        self._virtual = virtual_clock
        self._now = 0.0
        self._timers = []

    # --- the seam --------------------------------------------------------------------------

    def open(self, path):
        self.opened.append(path)
        if path not in self.devices:
            raise OSError(errno.ENOENT, "no such fake device", path)
        self._next_fd += 1
        self._fds[self._next_fd] = path
        return self._next_fd

    def read(self, fd, count):
        return _descriptor(self.devices[self._fds[fd]]["ids"])[:count]

    def close(self, fd):
        self._fds.pop(fd, None)

    def ioctl(self, fd, request, arg=0):
        path = self._fds[fd]
        code = ifno = None
        if request == self.ur.USBDEVFS_IOCTL:
            ifno, code, _ptr = struct.unpack(self.ur.USBDEVFS_IOCTL_STRUCT, arg)
        self.ioctls.append((path, request, code, ifno))
        self.sigmask_during.append(signal.pthread_sigmask(signal.SIG_BLOCK, []))
        if self.on_ioctl is not None:
            return self.on_ioctl(self, path, request, code, ifno)
        if request == self.ur.USBDEVFS_RESET:
            self.unplug(path)
            self.after(self.reset_delay or 0.05, lambda: self.plug(path))
        elif code == self.ur.USBDEVFS_DISCONNECT:
            self.unplug(path)
        elif code == self.ur.USBDEVFS_CONNECT:
            self.plug(path)
        return 0

    def exists(self, path):
        with self._lock:
            return path in self.nodes

    def monotonic(self):
        if not self._virtual:
            return time.monotonic()
        with self._lock:
            return self._now

    def sleep(self, seconds):
        if not self._virtual:
            time.sleep(seconds)
        else:
            with self._lock:
                self._now += max(seconds, 0.001)
            time.sleep(0)  # yield, so the watcher thread still gets to poll
        if self.interrupt_in_gap and threading.current_thread() is threading.main_thread():
            self.interrupt_in_gap = False
            raise KeyboardInterrupt("operator pressed Ctrl-C during the gap")

    # --- the model -------------------------------------------------------------------------

    def unplug(self, path):
        with self._lock:
            self.nodes -= set(self.devices[path]["nodes"])

    def plug(self, path):
        with self._lock:
            self.nodes |= set(self.devices[path]["nodes"])

    def after(self, delay, fn):
        timer = threading.Timer(delay, fn)
        timer.daemon = True
        timer.start()
        self._timers.append(timer)

    def settle(self):
        for timer in self._timers:
            timer.join(timeout=3.0)

    def codes(self):
        return [code for _p, _r, code, _i in self.ioctls if code is not None]

    def resets(self):
        return [r for _p, r, _c, _i in self.ioctls if r == self.ur.USBDEVFS_RESET]


class Capture:
    """Collect everything the script prints, so a case can assert on the timeline."""

    def __enter__(self):
        self._real = sys.stdout
        self._buf = []

        class Sink:
            write = self._buf.append

            def flush(self_inner):
                pass

        sys.stdout = Sink()
        return self

    def __exit__(self, *exc):
        sys.stdout = self._real
        self.text = "".join(self._buf)
        return False


def _args(root, target="video", method="reset", gap=0.02, wait=1.0, dry_run=False, **kw):
    ns = types.SimpleNamespace(sysfs_root=root, video="/dev/video4", serial="/dev/ttyACM1",
                               target=target, method=method, gap=gap, wait=wait,
                               dry_run=dry_run, port=None)
    for key, value in kw.items():
        setattr(ns, key, value)
    return ns


# ------------------------------------------------------------------------------- the cases


def run(ur):
    """Run every case against the usb-replug module `ur`. Returns a process exit code."""
    failures = []

    def case(name, fn):
        # Any exception is a failure, not a crash: a refusal that starts firing where it
        # should not is exactly the regression these cases exist to catch, and it must be
        # reported as one FAIL line rather than a traceback that hides the other 20 cases.
        try:
            print(f"  [ok]   {name}: {fn()}")
        except Exception as e:  # noqa: BLE001 - a test harness catches everything
            failures.append(name)
            print(f"  [FAIL] {name}: {type(e).__name__}: {e}")

    def expect_refusal(fn, what):
        try:
            fn()
        except ur.Refused as e:
            return f"refused — {e}"
        raise AssertionError(f"{what} was NOT refused")

    # -- resolution and refusal ------------------------------------------------------------

    def good():
        with tempfile.TemporaryDirectory() as td:
            root = _build_tree(td)
            v, s, h = ur.resolve(root, "/dev/video4", "/dev/ttyACM1")
            assert (v.name, v.ids) == ("3-2.2.2", "345f:2133"), f"video is {v.name} {v.ids}"
            assert (s.name, s.ids) == ("3-2.2.4", "1a86:55d3"), f"serial is {s.name} {s.ids}"
            assert (h.name, h.ids) == ("3-2.2", ur.DONGLE_HUB_ID), f"hub is {h.name} {h.ids}"
            assert v.nodes == ["/dev/video4", "/dev/video5"], f"video nodes {v.nodes}"
            assert s.nodes == ["/dev/ttyACM1"], f"serial nodes {s.nodes}"
            assert h.nodes == ["/dev/ttyACM1", "/dev/video4", "/dev/video5"], f"hub {h.nodes}"
            assert v.usbfs == "/dev/bus/usb/003/034", f"usbfs path {v.usbfs}"
            assert ur._interfaces_of(v.sysfs) == [0, 1, 2], "video interfaces"
            for target, expect in (("video", ["/dev/video4", "/dev/video5"]),
                                   ("serial", ["/dev/ttyACM1"]),
                                   ("dongle", ["/dev/ttyACM1", "/dev/video4", "/dev/video5"])):
                ops = ur.plan_operations(target, v, s, h)
                assert len(ops) == 1 and ops[0][2] == expect, f"{target} plan {ops}"
            assert ur.plan_operations("dongle", v, s, h)[0][1].name == "3-2.2", "dongle = hub"
            assert [o[0] for o in ur.plan_operations("both", v, s, h)] == ["video", "serial"]
            assert [o[0] for o in ur.plan_operations("both-reversed", v, s, h)] == \
                ["serial", "video"]
            return "video, serial and dongle resolve; nodes, usbfs paths and plans correct"

    def wrong_hub():
        with tempfile.TemporaryDirectory() as td:
            root = _build_tree(td, hub_ids="0bda:5423")
            return expect_refusal(lambda: ur.resolve(root, "/dev/video4", "/dev/ttyACM1"),
                                  "a dongle-shaped tree under a foreign hub")

    def wrong_video():
        with tempfile.TemporaryDirectory() as td:
            root = _build_tree(td, video_ids="1d6b:0002")
            return expect_refusal(lambda: ur.resolve(root, "/dev/video4", "/dev/ttyACM1"),
                                  "a foreign video device")

    def split_parents():
        with tempfile.TemporaryDirectory() as td:
            root = _build_tree(td, split_parents=True)
            return expect_refusal(lambda: ur.resolve(root, "/dev/video4", "/dev/ttyACM1"),
                                  "two nodes on two different hubs")

    def foreign_node_by_device():
        """The user's other hardware is refused because of what it *is*, not what it is called."""
        with tempfile.TemporaryDirectory() as td:
            root = _build_tree(td, foreign_video=True)
            detail = expect_refusal(lambda: ur.resolve(root, "/dev/video0", "/dev/ttyACM1"),
                                    "a /dev/video0 belonging to another device")
            assert "1d6b:0002" in detail, f"refusal does not name the device: {detail}"
            return detail

    def dongle_owning_video0():
        """the replug can hand the dongle /dev/video0, and it is still the dongle."""
        with tempfile.TemporaryDirectory() as td:
            root = _build_tree(td)
            iface = os.path.join(root, "devices", "pci0000:00", "usb3", "3-2", "3-2.2",
                                 "3-2.2.2", "3-2.2.2:1.0")
            _rename_class_nodes(root, iface, "video4linux",
                                {"video4": "video0", "video5": "video1"})
            for spelling in ("/dev/video0", "/dev/./video0", "video0"):
                v, _s, _h = ur.resolve(root, spelling, "/dev/ttyACM1")
                assert v.name == "3-2.2.2", f"{spelling} resolved to {v.name}"
                assert v.nodes == ["/dev/video0", "/dev/video1"], f"{spelling}: {v.nodes}"
            return "every spelling of /dev/video0 resolves to 3-2.2.2 and is acted on"

    def missing_node_via_hub():
        """a missing node is the thing to fix, not a reason to refuse."""
        with tempfile.TemporaryDirectory() as td:
            root = _build_tree(td)
            _drop_class_node(root, "video4linux", "video4")
            _drop_class_node(root, "video4linux", "video5")
            v, s, h = ur.resolve(root, "/dev/video4", "/dev/ttyACM1")
            assert v is not None and v.name == "3-2.2.2", f"video {v}"
            assert (s.name, h.name) == ("3-2.2.4", "3-2.2"), "serial/hub"
            return "video resolved through the serial node's hub with no /dev/video4 in sight"

    def port_when_both_nodes_missing():
        with tempfile.TemporaryDirectory() as td:
            root = _build_tree(td)
            for node in ("video4", "video5"):
                _drop_class_node(root, "video4linux", node)
            _drop_class_node(root, "tty", "ttyACM1")
            expect_refusal(lambda: ur.resolve(root, "/dev/video4", "/dev/ttyACM1"),
                           "resolution with no node and no --port")
            v, s, h = ur.resolve(root, "/dev/video4", "/dev/ttyACM1", port="3-2.2")
            assert (v.name, s.name, h.name) == ("3-2.2.2", "3-2.2.4", "3-2.2"), "by port"
            return "--port 3-2.2 resolves all three when neither /dev node exists"

    def port_of_foreign_device():
        with tempfile.TemporaryDirectory() as td:
            root = _build_tree(td, foreign_video=True)
            return expect_refusal(
                lambda: ur.resolve(root, "/dev/video4", "/dev/ttyACM1", port="5-1"),
                "--port naming another device")

    def hex_interface_numbers():
        """the kernel writes bInterfaceNumber as %02x; so must the fakes."""
        with tempfile.TemporaryDirectory() as td:
            dev = _fake_usb_device(td, "3-2.2.2", "345f:2133", "USB2 Video", 34,
                                   [(16, None, [])])
            raw = open(os.path.join(dev, "3-2.2.2:1.10", "bInterfaceNumber")).read().strip()
            got = ur._interfaces_of(dev)
            assert raw == "10", f"fake wrote bInterfaceNumber {raw!r}, kernel writes '10'"
            assert got == [16], f"interface 16 read back as {got}"
            return "interface 16 is written '10' and read back as 16"

    # -- the acting half -------------------------------------------------------------------

    def dongle_devices(root):
        """(video Device, FakeKernel arguments) for the standard tree."""
        v, s, h = ur.resolve(root, "/dev/video4", "/dev/ttyACM1")
        devices = {v.usbfs: {"ids": v.ids, "nodes": v.nodes},
                   s.usbfs: {"ids": s.ids, "nodes": s.nodes},
                   h.usbfs: {"ids": h.ids, "nodes": h.nodes}}
        return v, s, h, devices

    def sysfs_root_cannot_aim_an_ioctl():
        """a fake tree must not be able to name a bus and devnum to reset."""
        with tempfile.TemporaryDirectory() as td:
            root = _build_tree(td)
            base = os.path.join(root, "devices", "pci0000:00", "usb3", "3-2")
            for path, bus, num in ((os.path.join(base, "3-2.2"), "5", "4"),
                                   (os.path.join(base, "3-2.2", "3-2.2.2"), "5", "2"),
                                   (os.path.join(base, "3-2.2", "3-2.2.4"), "5", "3")):
                _write_file(os.path.join(path, "busnum"), bus)
                _write_file(os.path.join(path, "devnum"), num)

            opened = []

            class RecordingRealKernel(ur.Kernel):
                def open(self_inner, path):
                    opened.append(path)
                    raise AssertionError("a usbfs node was opened: " + path)

            with Capture():
                detail = expect_refusal(lambda: ur.run(_args(root), RecordingRealKernel()),
                                        "an ioctl aimed by a fake --sysfs-root")
            assert not opened, f"opened {opened} — bus 5 is the user's unrelated hardware"
            return detail

    def descriptor_must_match():
        """sysfs says which node to open; the descriptor says what answered."""
        with tempfile.TemporaryDirectory() as td:
            root = _build_tree(td)
            v, _s, _h, devices = dongle_devices(root)
            devices[v.usbfs]["ids"] = "1d6b:0002"  # something else answered on that devnum
            kernel = FakeKernel(ur, devices, nodes=v.nodes)
            with Capture():
                detail = expect_refusal(
                    lambda: ur.do_operation("video", v, v.nodes, "reset", 0.02, 0.2, root,
                                            kernel),
                    "an ioctl on a usbfs node whose descriptor is another device")
            assert not kernel.ioctls, f"ioctls were issued anyway: {kernel.ioctls}"
            assert kernel.opened == [v.usbfs], f"opened {kernel.opened}"
            return detail + " (no ioctl issued)"

    def descriptor_match_proceeds():
        with tempfile.TemporaryDirectory() as td:
            root = _build_tree(td)
            v, _s, _h, devices = dongle_devices(root)
            kernel = FakeKernel(ur, devices, nodes=v.nodes)
            with Capture() as cap:
                ok = ur.do_operation("video", v, v.nodes, "reset", 0.02, 1.0, root, kernel)
            kernel.settle()
            assert ok, "a clean reset reported failure:\n" + cap.text
            assert kernel.resets() == [ur.USBDEVFS_RESET], f"ioctls {kernel.ioctls}"
            assert "345f:2133" in cap.text, "the descriptor check is not on the timeline"
            assert "/dev/video4 gone" in cap.text and "/dev/video4 back" in cap.text, cap.text
            return "descriptor matches, reset issued, both nodes seen gone and back"

    def sigint_in_the_gap_still_reconnects():
        """Ctrl-C between DISCONNECT and CONNECT must not leave the drivers unbound."""
        with tempfile.TemporaryDirectory() as td:
            root = _build_tree(td)
            v, _s, _h, devices = dongle_devices(root)
            kernel = FakeKernel(ur, devices, nodes=v.nodes)
            kernel.interrupt_in_gap = True
            interrupted = False
            with Capture() as cap:
                try:
                    ur.do_operation("video", v, v.nodes, "rebind", 0.2, 0.5, root, kernel)
                except KeyboardInterrupt:
                    interrupted = True
            codes = kernel.codes()
            disconnects = codes.count(ur.USBDEVFS_DISCONNECT)
            connects = codes.count(ur.USBDEVFS_CONNECT)
            assert interrupted, "the KeyboardInterrupt was swallowed: " + cap.text
            assert disconnects == 3, f"{disconnects} DISCONNECTs for 3 interfaces"
            assert connects == disconnects, (
                f"{disconnects} DISCONNECT but {connects} CONNECT after a Ctrl-C in the gap — "
                "the interfaces stay unbound and the nodes never come back")
            assert kernel.nodes == set(v.nodes), f"/dev is {kernel.nodes} after the interrupt"
            blocked = kernel.sigmask_during[0]
            assert signal.SIGINT in blocked and signal.SIGTERM in blocked, (
                "SIGINT/SIGTERM were not blocked while the interfaces were unbound")
            assert signal.SIGINT not in signal.pthread_sigmask(signal.SIG_BLOCK, []), (
                "the signal mask was not restored afterwards")
            return "3 DISCONNECT, 3 CONNECT, nodes back, signals blocked then restored"

    def failed_ioctls_are_a_failure():
        """a rebind whose ioctls all failed must not report success."""
        with tempfile.TemporaryDirectory() as td:
            root = _build_tree(td)
            v, _s, _h, devices = dongle_devices(root)
            kernel = FakeKernel(ur, devices, nodes=v.nodes)

            def refuse(_k, _path, _request, _code, _ifno):
                raise OSError(errno.EPERM, "Operation not permitted")

            kernel.on_ioctl = refuse
            with Capture() as cap:
                ok = ur.do_operation("video", v, v.nodes, "rebind", 0.02, 0.1, root, kernel)
            assert ok is False, "EPERM on every ioctl reported success:\n" + cap.text
            assert cap.text.count("EPERM") >= 6 or "Operation not permitted" in cap.text, cap.text
            assert "FAIL" in cap.text, cap.text
            return "6 EPERM ioctls -> FAIL and a non-zero exit"

    def late_disconnect_is_seen():
        """the disconnect may only begin after the ioctl returns."""
        with tempfile.TemporaryDirectory() as td:
            root = _build_tree(td)
            v, _s, _h, devices = dongle_devices(root)
            kernel = FakeKernel(ur, devices, nodes=v.nodes)

            def late(k, path, request, _code, _ifno):
                if request == ur.USBDEVFS_RESET:
                    k.after(0.06, lambda: k.unplug(path))
                    k.after(0.18, lambda: k.plug(path))
                return 0

            kernel.on_ioctl = late
            with Capture() as cap:
                ok = ur.do_operation("video", v, v.nodes, "reset", 0.02, 2.0, root, kernel)
            kernel.settle()
            assert ok, "a device that went away and came back reported failure:\n" + cap.text
            assert "never went away" not in cap.text, (
                "the node disconnected 60 ms after the ioctl returned and the script said it "
                "never went away:\n" + cap.text)
            assert "gone" in cap.text and "back after" in cap.text, cap.text
            return "a disconnect 60 ms after the ioctl returned is still seen and timed"

    def survived_in_place_is_stated():
        """A reset that preserves its nodes must not consume the full return timeout."""
        with tempfile.TemporaryDirectory() as td:
            root = _build_tree(td)
            v, _s, _h, devices = dongle_devices(root)
            kernel = FakeKernel(ur, devices, nodes=v.nodes, virtual_clock=True)
            kernel.on_ioctl = lambda *a: 0  # a driver with pre_reset/post_reset: nothing moves
            started = time.monotonic()
            with Capture() as cap:
                ok = ur.do_operation("video", v, v.nodes, "reset", 3.0, 20.0, root, kernel)
            wall = time.monotonic() - started
            assert ok, "a device that never went away is not a failure:\n" + cap.text
            assert "never went away" in cap.text and "survived the reset in place" in cap.text, \
                cap.text
            assert wall < 5.0, f"a 20 s --wait took {wall:.1f} s of wall time"
            return (f"reported as survived-in-place, and 2 s of settle + 20 s of wait took "
                    f"{wall * 1000:.0f} ms on the injected clock")

    def new_names_are_a_success():
        """the dongle can come back as /dev/video0, and that is not a failure."""
        with tempfile.TemporaryDirectory() as td:
            root = _build_tree(td)
            v, _s, _h, devices = dongle_devices(root)
            iface = os.path.join(v.sysfs, "3-2.2.2:1.0")
            kernel = FakeKernel(ur, devices, nodes=v.nodes)

            def renumber(k, path, request, _code, _ifno):
                if request != ur.USBDEVFS_RESET:
                    return 0
                k.unplug(path)

                def back():
                    _rename_class_nodes(root, iface, "video4linux",
                                        {"video4": "video0", "video5": "video1"})
                    k.devices[path]["nodes"] = ["/dev/video0", "/dev/video1"]
                    k.plug(path)

                k.after(0.05, back)
                return 0

            kernel.on_ioctl = renumber
            with Capture() as cap:
                ok = ur.do_operation("video", v, v.nodes, "reset", 0.02, 2.0, root, kernel)
            kernel.settle()
            assert ok, ("the device came back as /dev/video0 + /dev/video1 and the script "
                        "called it a failure:\n" + cap.text)
            assert "WARNING" in cap.text and "/dev/video0" in cap.text, (
                "the rename was not announced, and every hardware test hardcodes "
                "/dev/video4:\n" + cap.text)
            assert "/dev/video0 back" in cap.text, cap.text
            return "back as /dev/video0 + /dev/video1 -> success, with a loud rename warning"

    def run_end_to_end():
        """run() itself, both operations, through the fakes."""
        with tempfile.TemporaryDirectory() as td:
            root = _build_tree(td)
            v, s, _h, devices = dongle_devices(root)
            kernel = FakeKernel(ur, devices, nodes=v.nodes + s.nodes)
            with Capture() as cap:
                code = ur.run(_args(root, target="both", method="rebind", gap=0.05, wait=1.0),
                              kernel)
            kernel.settle()
            assert code == 0, f"run() returned {code}:\n{cap.text}"
            assert kernel.opened == [v.usbfs, s.usbfs], f"opened {kernel.opened}"
            codes = kernel.codes()
            assert codes.count(ur.USBDEVFS_DISCONNECT) == 5, f"ioctls {codes}"
            assert codes.count(ur.USBDEVFS_CONNECT) == 5, f"ioctls {codes}"
            for node in ("/dev/video4", "/dev/ttyACM1"):
                assert f"{node} gone" in cap.text and f"{node} back" in cap.text, cap.text
            assert kernel.nodes == set(v.nodes + s.nodes), f"/dev is {kernel.nodes}"
            return "both: video then serial, 5 DISCONNECT + 5 CONNECT, every node back, exit 0"

    def dry_run_opens_nothing():
        with tempfile.TemporaryDirectory() as td:
            root = _build_tree(td)
            _v, _s, _h, devices = dongle_devices(root)
            kernel = FakeKernel(ur, devices)
            with Capture() as cap:
                code = ur.run(_args(root, target="dongle", dry_run=True), kernel)
            assert code == 0 and not kernel.opened and not kernel.ioctls, cap.text
            assert "dry-run" in cap.text, cap.text
            return "dry-run resolved three devices and opened nothing"

    def a_node_that_never_returns_fails():
        with tempfile.TemporaryDirectory() as td:
            root = _build_tree(td)
            v, _s, _h, devices = dongle_devices(root)
            kernel = FakeKernel(ur, devices, nodes=v.nodes)

            def unplug_forever(k, path, _request, _code, _ifno):
                k.unplug(path)
                for node in ("video4", "video5"):
                    _drop_class_node(root, "video4linux", node)
                os.rename(os.path.join(v.sysfs, "3-2.2.2:1.0", "video4linux"),
                          os.path.join(v.sysfs, "3-2.2.2:1.0", "gone"))
                return 0

            kernel.on_ioctl = unplug_forever
            with Capture() as cap:
                ok = ur.do_operation("video", v, v.nodes, "reset", 0.02, 0.2, root, kernel)
            assert ok is False, "a device that never came back reported success:\n" + cap.text
            assert "FAIL" in cap.text, cap.text
            return "no node back within --wait -> FAIL and a non-zero exit"

    print("usb-replug self-test (fake sysfs and a fake kernel; no hardware, nothing opened):")
    case("dongle-shaped tree resolves", good)
    case("parent hub is not 1a40:0101", wrong_hub)
    case("video vid:pid not in allowlist", wrong_video)
    case("dongle with two different parents", split_parents)
    case("a /dev node owned by another device is refused", foreign_node_by_device)
    case("the dongle owning /dev/video0 is still the dongle", dongle_owning_video0)
    case("a missing /dev node resolves through the hub", missing_node_via_hub)
    case("--port resolves when neither node exists", port_when_both_nodes_missing)
    case("--port naming another device is refused", port_of_foreign_device)
    case("bInterfaceNumber is hex, in the fakes too", hex_interface_numbers)
    case("--sysfs-root cannot aim an ioctl", sysfs_root_cannot_aim_an_ioctl)
    case("the usbfs descriptor must be the approved device", descriptor_must_match)
    case("a matching descriptor proceeds to the reset", descriptor_match_proceeds)
    case("Ctrl-C in the rebind gap still reconnects", sigint_in_the_gap_still_reconnects)
    case("every ioctl failing is a failed operation", failed_ioctls_are_a_failure)
    case("a disconnect after the ioctl returns is seen", late_disconnect_is_seen)
    case("a driver that survives the reset is stated", survived_in_place_is_stated)
    case("nodes back under new names are a success", new_names_are_a_success)
    case("a node that never returns is a failure", a_node_that_never_returns_fails)
    case("run() end to end: both, rebind", run_end_to_end)
    case("--dry-run opens nothing", dry_run_opens_nothing)

    print()
    if failures:
        print(f"FAIL: {len(failures)} case(s): {', '.join(failures)}")
        return 1
    print("OK: 21 cases passed")
    return 0


def _load_script():
    import importlib.util

    here = os.path.dirname(os.path.abspath(__file__))
    path = os.path.join(here, os.pardir, "scripts", "usb-replug.py")
    spec = importlib.util.spec_from_file_location("usb_replug", path)
    module = importlib.util.module_from_spec(spec)
    sys.dont_write_bytecode = True  # leave no __pycache__ behind in the repo
    spec.loader.exec_module(module)
    return module


if __name__ == "__main__":
    sys.exit(run(_load_script()))
