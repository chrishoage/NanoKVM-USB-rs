#!/usr/bin/env python3
"""Make the kernel unplug and replug the NanoKVM-USB dongle, so recovery is testable.

NATIVE_CLIENT_PLAN §6.1 S2-4 and §12 require the client to survive "device unplug and replug
— both nodes, in either order". Nobody can pull a cable from inside an automated test, and a
recovery test that cannot be repeated is not a test. The user owns the dongle's usbfs nodes,
so the kernel can do the replug for us: usbfs can reset a device (USBDEVFS_RESET) or unbind
and rebind its drivers (USBDEVFS_DISCONNECT/CONNECT). The Stage 2 Rust hardware tests shell
out to this script; it is a test fixture, not part of the client.

Why two methods: a reset is the closer analogue of a cable pull, but a driver that implements
`pre_reset`/`post_reset` survives one in place and its /dev node never disappears — which is a
measurement, not a failure. A rebind always tears the driver down. Run both and use whichever
actually produces a disconnect for the node under test.

Identity, and what "back" means (review R1, R4, R5):

  * A device's identity is its **sysfs port path** (`3-2.2.2`), never its /dev name. v4l2 and
    cdc_acm hand out the lowest free minor at registration, so the very replug this script
    performs can bring the dongle back as /dev/video0 + /dev/ttyACM0. "Back" therefore means
    *the USB device re-enumerated at the same port path and its class nodes exist*, whatever
    they are now called; the new names are printed, loudly, because every hardware test in this
    repo hardcodes /dev/video4 and /dev/ttyACM1.
  * Nothing is refused by /dev name. The rule is topological: every device acted on must hang
    off the dongle's own 1a40:0101 hub, and a `--video`/`--serial` argument that resolves to a
    USB device outside the allowlist is refused. That covers the user's unrelated hardware
    however the kernel happens to have numbered it today.
  * Immediately before any ioctl, the **device descriptor is read back from the usbfs node
    itself** (its first 18 bytes) and must equal the vid:pid `resolve()` approved. sysfs says
    which node to open; the descriptor proves what answered. That closes both the
    `--sysfs-root` hole and the resolve→open devnum race.

Other safety rules (CLAUDE.md, and the contract for this script):

  * Everything is resolved through sysfs at run time; usbfs device numbers change on every
    re-enumeration, so a path captured earlier is a path to somebody else's hardware.
  * A rebind reconnects on **every** exit path, including Ctrl-C in the gap — the same rule as
    the release-all guards on the input tests. SIGINT/SIGTERM are blocked while the interfaces
    are unbound, and the CONNECT loop is in a `finally`.
  * An ioctl that failed is a failed operation: the errors reach the timeline and the exit code.
  * --dry-run resolves, prints, and exits without opening a usbfs node at all.
  * --sysfs-root is for --self-test. Acting on hardware with a root that is not /sys is refused
    before anything is opened; the descriptor check makes even that belt and braces.

Exit codes: 0 every affected node present at the end, 1 a node still missing after --wait or an
ioctl failed, 2 a refusal.

Usage:
  scripts/usb-replug.py <video|serial|dongle|both|both-reversed> [--method reset|rebind]
                        [--gap SECS] [--wait SECS] [--dry-run]
                        [--video /dev/videoN] [--serial /dev/ttyACMN] [--port 3-2.2.2]
  scripts/usb-replug.py --self-test
"""
import argparse
import errno
import fcntl
import importlib.util
import os
import signal
import struct
import sys
import threading
import time

# usbfs ioctls, x86_64 Linux. Verified against /usr/include/linux/usbdevice_fs.h; the
# arithmetic is _IOC(dir, type, nr, size) = (dir << 30) | (size << 16) | (type << 8) | nr with
# type 'U' == 0x55, dir 0 for _IO and 3 for _IOWR.
USBDEVFS_RESET = 0x5514  # _IO('U', 20), no argument
USBDEVFS_IOCTL = 0xC0105512  # _IOWR('U', 18, struct usbdevfs_ioctl), 16-byte struct
USBDEVFS_DISCONNECT = 0x5516  # _IO('U', 22), passed as ioctl_code inside USBDEVFS_IOCTL
USBDEVFS_CONNECT = 0x5517  # _IO('U', 23), likewise

# struct usbdevfs_ioctl { int ifno; int ioctl_code; void *data; } — 16 bytes with the pointer
# aligned to 8, which is exactly what native-mode 'iiP' packs.
USBDEVFS_IOCTL_STRUCT = "iiP"

VIDEO_ID = "345f:2133"  # UVC video side of the dongle   -> /dev/video4 (+ its metadata node)
SERIAL_ID = "1a86:55d3"  # CH9329 behind its CH34x bridge -> /dev/ttyACM1
DONGLE_HUB_ID = "1a40:0101"  # the hub inside the dongle, parent of the other two
ALLOWED_IDS = {VIDEO_ID, SERIAL_ID, DONGLE_HUB_ID}

REAL_SYSFS = "/sys"
USBFS_PREFIX = "/dev/bus/usb/"

POLL_S = 0.005  # 5 ms; the resolution of every "gone"/"back" timestamp below
SETTLE_S = 2.0  # how long a disconnect gets to *begin* after the ioctl returns (review R3)
DESCRIPTOR_LEN = 18  # sizeof(struct usb_device_descriptor)

TARGETS = ("video", "serial", "dongle", "both", "both-reversed")

_INTERRUPTS = (signal.SIGINT, signal.SIGTERM)


class Refused(Exception):
    """A safety rule said no. Always exit 2; never fall back to acting anyway."""


# ------------------------------------------------------------------------------------ the seam


class Kernel:
    """Everything this script does to the machine, in one injectable object (review R9).

    The real implementation is these eight one-liners. `--self-test` substitutes a fake that
    records ioctls, owns a virtual set of /dev nodes and can drive its own clock, which is how
    `run()`, `do_operation()` and `Watcher` get tested without hardware. `real` is what tells
    the acting path that a `--sysfs-root` other than /sys must be refused.
    """

    real = True

    def open(self, path):
        return os.open(path, os.O_RDWR)

    def read(self, fd, count):
        return os.read(fd, count)

    def close(self, fd):
        try:
            os.close(fd)
        except OSError:
            pass

    def ioctl(self, fd, request, arg=0):
        return fcntl.ioctl(fd, request, arg)

    def exists(self, path):
        return os.path.exists(path)

    def monotonic(self):
        return time.monotonic()

    def sleep(self, seconds):
        time.sleep(seconds)


# --------------------------------------------------------------------------- sysfs resolution


def _attr(dev, name):
    try:
        with open(os.path.join(dev, name)) as fh:
            return fh.read().strip()
    except OSError:
        return None


def _listdir(path):
    try:
        return sorted(os.listdir(path))
    except OSError:
        return []


def _class_link(sysfs_root, node):
    """The sysfs `device` link for a /dev node, without opening the node.

    Takes any spelling of the name — `/dev/video4`, `/dev/./video4`, `video4` — because the
    only thing that matters here is the class; identity is decided later, from the topology.
    """
    name = os.path.basename(node.rstrip("/"))
    if name.startswith("video"):
        sub = "video4linux"
    elif name.startswith("tty"):
        sub = "tty"
    else:
        raise Refused(f"{node}: not a video4linux or tty node")
    return os.path.join(sysfs_root, "class", sub, name, "device")


def _usb_device_of(sysfs_root, link):
    """Walk up from a class device to the first ancestor that is a USB device.

    Same rule device-health.py uses (§8): the first directory carrying an `idVendor` attr.
    """
    root = os.path.realpath(sysfs_root)
    dev = os.path.realpath(link)
    while dev != "/" and dev.startswith(root):
        if os.path.exists(os.path.join(dev, "idVendor")):
            return dev
        dev = os.path.dirname(dev)
    raise Refused(f"{link}: no USB device ancestor under {sysfs_root}")


def _device_by_port(sysfs_root, port):
    """Find a USB device by its sysfs port path (`3-2.2.2`) — the identity that survives a replug.

    This is what `--port` names, and it is the only way in when both /dev nodes are missing,
    which is exactly the state this script exists to get out of (review R7).
    """
    name = os.path.basename(port.rstrip("/"))
    direct = os.path.join(sysfs_root, "bus", "usb", "devices", name)
    if os.path.exists(os.path.join(direct, "idVendor")):
        return os.path.realpath(direct)
    for dirpath, _dirnames, _files in os.walk(os.path.join(sysfs_root, "devices")):
        if (os.path.basename(dirpath) == name
                and os.path.exists(os.path.join(dirpath, "idVendor"))):
            return os.path.realpath(dirpath)
    raise Refused(f"--port {name}: no USB device by that name under {sysfs_root}")


def _interfaces_of(dev):
    """Interface numbers of a USB device, read from the `<dev>:C.N` children.

    The kernel writes bInterfaceNumber as %02x, so this parses base 16 — and the fake trees in
    the self-test write it the same way, or the self-test would bless either base (review R8).
    """
    name = os.path.basename(dev)
    out = []
    for entry in _listdir(dev):
        if not entry.startswith(name + ":"):
            continue
        ifno = _attr(os.path.join(dev, entry), "bInterfaceNumber")
        if ifno is not None:
            out.append(int(ifno, 16))
    return sorted(set(out))


def _nodes_of(dev):
    """The /dev nodes a USB device owns, found through its interfaces' class directories.

    Looks in each interface directory and one level below it: uvcvideo and cdc_acm both hang
    their class dir straight off the interface, but usb-serial style drivers add a level.
    """
    name = os.path.basename(dev)
    found = []
    for entry in _listdir(dev):
        if not entry.startswith(name + ":"):
            continue
        iface = os.path.join(dev, entry)
        for d in [iface] + [os.path.join(iface, c) for c in _listdir(iface)]:
            for cls in ("video4linux", "tty"):
                cd = os.path.join(d, cls)
                if os.path.isdir(cd):
                    found += ["/dev/" + n for n in _listdir(cd)]
    # Order matters only for readability; dedupe keeps a doubled class link from lying.
    return sorted(set(found))


def _child_devices(dev):
    """Child USB devices of a device: `3-2.2` -> [`3-2.2.2`, `3-2.2.4`]."""
    name = os.path.basename(dev)
    out = []
    for entry in _listdir(dev):
        child = os.path.join(dev, entry)
        if entry.startswith(name + ".") and os.path.exists(os.path.join(child, "idVendor")):
            out.append(child)
    return out


def _nodes_under(dev):
    """Every /dev node this device and its children own, right now.

    The `dongle` target acts on the hub, which owns no class node of its own; its children's
    nodes are the ones that have to come back.
    """
    out = list(_nodes_of(dev))
    for child in _child_devices(dev):
        out += _nodes_under(child)
    return sorted(set(out))


class Device:
    """One resolved USB device: what it is, where usbfs exposes it, what nodes it owns."""

    def __init__(self, sysfs_root, dev):
        self.sysfs = dev
        self.name = os.path.basename(dev)
        self.vid = _attr(dev, "idVendor")
        self.pid = _attr(dev, "idProduct")
        self.product = _attr(dev, "product") or "?"
        self.busnum = _attr(dev, "busnum")
        self.devnum = _attr(dev, "devnum")
        self.nodes = _nodes_under(dev)
        self.sysfs_root = sysfs_root

    @property
    def ids(self):
        return f"{self.vid}:{self.pid}"

    @property
    def usbfs(self):
        if self.busnum is None or self.devnum is None:
            return None
        return f"{USBFS_PREFIX}{int(self.busnum):03d}/{int(self.devnum):03d}"

    def describe(self, label):
        nodes = " ".join(self.nodes) if self.nodes else "(none)"
        quoted = '"' + self.product + '"'
        return (f"resolved {label:6} {self.name:9} {self.ids} "
                f"{quoted:22} {self.usbfs}  nodes: {nodes}")


def resolve(sysfs_root, video_node, serial_node, port=None):
    """Resolve the dongle's two devices and its hub, applying every refusal rule.

    Returns (video, serial, hub); video or serial may be None when that half has not
    enumerated — `dongle` can still put it back, which is the point of naming devices by port
    path rather than by /dev node (review R7). Raises Refused. Opens nothing.

    The refusals, in order:
      1. an explicit --video/--serial that resolves to a USB device outside the allowlist —
         this is what stands between the script and the user's unrelated hardware, and it is a
         statement about the *device*, not about a name the kernel may reassign (review R5);
      2. anything not hanging off the dongle's own 1a40:0101 hub;
      3. two nodes that do not share that one hub — they are not one dongle.
    """
    seeds = {}
    for label, node in (("video", video_node), ("serial", serial_node)):
        link = _class_link(sysfs_root, node)
        if not os.path.exists(link):
            continue  # a missing node is the case we are here to fix, not a refusal
        dev = Device(sysfs_root, _usb_device_of(sysfs_root, link))
        if dev.ids not in ALLOWED_IDS:
            raise Refused(f"{node} belongs to {dev.name} {dev.ids} '{dev.product}', which is "
                          f"not part of the dongle {sorted(ALLOWED_IDS)} — never touch it")
        seeds[label] = dev

    if port:
        seed = Device(sysfs_root, _device_by_port(sysfs_root, port))
        if seed.ids not in ALLOWED_IDS:
            raise Refused(f"--port {seed.name} is {seed.ids} '{seed.product}', which is not "
                          f"part of the dongle {sorted(ALLOWED_IDS)} — never touch it")
    elif seeds:
        seed = seeds.get("serial") or seeds["video"]
    else:
        raise Refused(f"neither {video_node} nor {serial_node} exists under {sysfs_root}/class "
                      f"— name the device by its sysfs port path instead, e.g. --port 3-2.2")

    hub = seed if seed.ids == DONGLE_HUB_ID else Device(sysfs_root, os.path.dirname(seed.sysfs))
    if hub.ids != DONGLE_HUB_ID:
        raise Refused(f"{seed.name} hangs off {hub.name} {hub.ids} '{hub.product}', not the "
                      f"dongle's internal {DONGLE_HUB_ID} hub — these are not one dongle")

    found = {}
    for child in _child_devices(hub.sysfs):
        dev = Device(sysfs_root, child)
        if dev.ids == VIDEO_ID:
            found["video"] = dev
        elif dev.ids == SERIAL_ID:
            found["serial"] = dev

    for label, dev in seeds.items():
        if os.path.dirname(dev.sysfs) != hub.sysfs:
            raise Refused(f"{dev.name} hangs off "
                          f"{os.path.basename(os.path.dirname(dev.sysfs))}, not {hub.name} — "
                          f"the two nodes do not share one dongle")
        found[label] = dev  # the node-resolved device wins; it is the same device either way

    if not found:
        raise Refused(f"hub {hub.name} has no {VIDEO_ID} or {SERIAL_ID} child — "
                      f"this is not the dongle")
    return found.get("video"), found.get("serial"), hub


# --------------------------------------------------------------------------- usbfs operations


_print_lock = threading.Lock()


def _say(line):
    with _print_lock:
        print(line, flush=True)


def _stamp(kernel, t0, text):
    _say(f"+{int((kernel.monotonic() - t0) * 1000)} ms".ljust(11) + text)


def _usbfs_ioctl(kernel, fd, code, ifno):
    kernel.ioctl(fd, USBDEVFS_IOCTL, struct.pack(USBDEVFS_IOCTL_STRUCT, ifno, code, 0))


def _hold_signals():
    """Block SIGINT/SIGTERM for this thread; returns the previous mask, or None if unsupported.

    A rebind leaves the dongle's drivers unbound between DISCONNECT and CONNECT. A signal
    delivered in that window must not be able to end the process there, or the nodes stay gone
    until somebody physically replugs the dongle (review R2). The `finally` around the gap is
    the other half: this stops the signal arriving, that copes if it arrives anyway.
    """
    try:
        return signal.pthread_sigmask(signal.SIG_BLOCK, _INTERRUPTS)
    except (AttributeError, OSError, ValueError):
        return None


def _release_signals(previous):
    if previous is None:
        return
    try:
        signal.pthread_sigmask(signal.SIG_SETMASK, previous)
    except (OSError, ValueError):
        pass


class Watcher(threading.Thread):
    """Poll a set of /dev node paths and timestamp every appearance and disappearance.

    A thread, because USBDEVFS_RESET blocks for the whole reset: polling from the main thread
    would miss the disconnect entirely and report "never went away", which is the one thing
    this script must not get wrong. Existence only — the nodes are never opened. Names
    discovered after the operation are added with `add()`, so a device that comes back as
    /dev/video0 still gets a "back" line with a timestamp.
    """

    daemon = True

    def __init__(self, nodes, sysfs_root, t0, kernel):
        super().__init__()
        self.kernel = kernel
        self.nodes = list(nodes)
        self.sysfs_root = sysfs_root
        self.t0 = t0
        self.gone = {}
        self.back = {}
        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._present = {n: kernel.exists(n) for n in self.nodes}

    def add(self, nodes):
        """Start watching nodes that turned up during the operation, under new names."""
        with self._lock:
            for node in nodes:
                if node not in self._present:
                    self.nodes.append(node)
                    self._present[node] = False  # so its arrival is stamped like any other

    def run(self):
        # Signals are the main thread's business; this one must never take the process down
        # while a rebind has the interfaces unbound.
        _hold_signals()
        while not self._stop.is_set():
            with self._lock:
                nodes = list(self.nodes)
            for node in nodes:
                now = self.kernel.exists(node)
                with self._lock:
                    if now == self._present.get(node):
                        continue
                    self._present[node] = now
                elapsed = int((self.kernel.monotonic() - self.t0) * 1000)
                if now:
                    self.back.setdefault(node, elapsed)
                    _stamp(self.kernel, self.t0, f"{node} back{self._reidentify(node)}")
                else:
                    self.gone.setdefault(node, elapsed)
                    _stamp(self.kernel, self.t0, f"{node} gone")
            self.kernel.sleep(POLL_S)

    def _reidentify(self, node):
        """Re-resolve a returning node: a reset can keep the name and change the devnum."""
        try:
            dev = _usb_device_of(self.sysfs_root, _class_link(self.sysfs_root, node))
            return f" ({os.path.basename(dev)}, devnum {_attr(dev, 'devnum')})"
        except (Refused, OSError):
            return ""

    def stop(self):
        self._stop.set()
        self.join(timeout=2.0)

    def any_gone(self):
        with self._lock:
            return bool(self.gone)


def _verify_descriptor(kernel, fd, dev, usbfs, t0):
    """Prove the fd we are about to ioctl is the device resolve() approved (review R1).

    The first 18 bytes of a usbfs node are the USB device descriptor: idVendor at bytes 8-9,
    idProduct at 10-11, both little-endian. sysfs told us which node to open; only this says
    what actually answered. It closes two holes at once — a `--sysfs-root` naming a bus and
    devnum that belong to someone else, and the resolve→open race on devnum, which moves on
    every re-enumeration. Raises Refused; returns the ids it read.

    A path outside /dev/bus/usb cannot be a usbfs node and cannot be reached from the CLI
    (`Device.usbfs` always builds one): that is the self-test's stand-in file, and the skip is
    stated on the timeline rather than made silently.
    """
    if not usbfs.startswith(USBFS_PREFIX):
        _stamp(kernel, t0, f"note: {usbfs} is not a usbfs node — descriptor check skipped "
                           f"(only a test double reaches this)")
        return None
    expected = getattr(dev, "ids", None)
    blob = kernel.read(fd, DESCRIPTOR_LEN)
    if len(blob) < DESCRIPTOR_LEN or blob[0] != DESCRIPTOR_LEN or blob[1] != 0x01:
        raise Refused(f"{usbfs} did not return a USB device descriptor ({len(blob)} bytes) — "
                      f"refusing to ioctl a node I cannot identify")
    vid, pid = struct.unpack_from("<HH", blob, 8)
    got = f"{vid:04x}:{pid:04x}"
    if expected is None:
        raise Refused(f"{usbfs} is {got} but the caller supplied no expected vid:pid")
    if got != expected:
        raise Refused(f"{usbfs} is {got}, not the {expected} that {dev.name} resolved to — "
                      f"the devnum moved, or --sysfs-root is lying. Refusing.")
    if got not in ALLOWED_IDS:
        raise Refused(f"{usbfs} is {got}, which is not part of the dongle — never touch it")
    _stamp(kernel, t0, f"descriptor of {usbfs} reads {got}, the device {dev.name} resolved to")
    return got


def _current_nodes(dev):
    """The /dev nodes this device owns *now*, whatever they are called (review R4/R5).

    The port path is the authority. v4l2 and cdc_acm hand out the lowest free minor at
    registration, so the names captured before the operation are a guess about the future, not
    an identity: the device is back when its sysfs directory is there again — same port, same
    physical socket — and carries class nodes. Empty means "not back yet", including the window
    where the device has re-enumerated but its driver has not registered a node.
    """
    sysfs = dev.sysfs
    return _nodes_under(sysfs) if os.path.isdir(sysfs) else []


def do_operation(label, dev, nodes, method, gap, wait, sysfs_root, kernel=None):
    """Reset or rebind one device and report what happened to its nodes.

    True when the device's nodes — under whatever names they now have — are all present at the
    end and no ioctl failed. False otherwise; a safety violation raises Refused instead.
    """
    kernel = kernel or Kernel()
    nodes = list(nodes)
    t0 = kernel.monotonic()
    watcher = Watcher(nodes, sysfs_root, t0, kernel)
    watcher.start()
    errors = []
    current = []
    saw_gone = False
    try:
        usbfs = dev.usbfs
        if usbfs is None:
            _stamp(kernel, t0, f"{dev.name} has no usbfs node (busnum/devnum unreadable)")
            return False
        try:
            fd = kernel.open(usbfs)
        except OSError as e:
            _stamp(kernel, t0, f"cannot open {usbfs}: {e}")
            return False
        try:
            _verify_descriptor(kernel, fd, dev, usbfs, t0)
            if method == "reset":
                errors += _reset(kernel, fd, usbfs, t0)
            else:
                errors += _rebind(kernel, fd, dev, usbfs, gap, t0)
        finally:
            kernel.close(fd)

        # The disconnect may only begin after the ioctl returns, so give it a bounded window
        # before concluding the driver survived in place (review R3).
        settle = min(SETTLE_S, wait)
        settle_deadline = kernel.monotonic() + settle
        while kernel.monotonic() < settle_deadline and not watcher.any_gone():
            kernel.sleep(POLL_S)
        saw_gone = watcher.any_gone()
        if not saw_gone:
            _stamp(kernel, t0, f"no node disappeared within {int(settle * 1000)} ms of the "
                               f"ioctl returning")

        deadline = kernel.monotonic() + wait
        while True:
            current = _current_nodes(dev)
            watcher.add(current)
            if current and all(kernel.exists(n) for n in current):
                break
            if kernel.monotonic() >= deadline:
                break
            kernel.sleep(POLL_S)
        # One more poll interval so the watcher can timestamp the last arrival it just saw.
        kernel.sleep(POLL_S * 4)
    finally:
        watcher.stop()

    return _report(kernel, label, method, nodes, current, wait, errors, saw_gone, watcher)


def _reset(kernel, fd, usbfs, t0):
    _stamp(kernel, t0, f"USBDEVFS_RESET {usbfs}")
    try:
        kernel.ioctl(fd, USBDEVFS_RESET, 0)
    except OSError as e:
        # ENODEV here means the device re-enumerated out from under the fd, which is a
        # successful unplug, not an error. Anything else is a real failure and has to reach
        # the exit code (review R6).
        if e.errno == errno.ENODEV:
            _stamp(kernel, t0, "USBDEVFS_RESET returned ENODEV (device re-enumerated)")
            return []
        _stamp(kernel, t0, f"USBDEVFS_RESET failed: {e}")
        return [f"USBDEVFS_RESET: {e}"]
    return []


def _rebind(kernel, fd, dev, usbfs, gap, t0):
    """DISCONNECT every interface, wait --gap, CONNECT every interface.

    The CONNECT loop is in a `finally` and signals are blocked around the whole window: an
    interrupt between the two halves would otherwise leave the dongle's drivers unbound, which
    is the one failure this fixture must not be able to cause (review R2).
    """
    errors = []
    ifaces = _interfaces_of(dev.sysfs)
    if not ifaces:
        _stamp(kernel, t0, f"{dev.name} has no interfaces to rebind")
        return [f"{dev.name}: no interfaces to rebind"]
    mask = _hold_signals()
    try:
        for ifno in ifaces:
            _stamp(kernel, t0, f"USBDEVFS_DISCONNECT ifno {ifno} {usbfs}")
            try:
                _usbfs_ioctl(kernel, fd, USBDEVFS_DISCONNECT, ifno)
            except OSError as e:
                _stamp(kernel, t0, f"  disconnect ifno {ifno} failed: {e}")
                errors.append(f"DISCONNECT ifno {ifno}: {e}")
        try:
            deadline = kernel.monotonic() + gap
            while kernel.monotonic() < deadline:
                kernel.sleep(POLL_S)
        finally:
            for ifno in ifaces:
                _stamp(kernel, t0, f"USBDEVFS_CONNECT ifno {ifno} {usbfs}")
                try:
                    _usbfs_ioctl(kernel, fd, USBDEVFS_CONNECT, ifno)
                except OSError as e:
                    _stamp(kernel, t0, f"  connect ifno {ifno} failed: {e}")
                    errors.append(f"CONNECT ifno {ifno}: {e}")
    finally:
        _release_signals(mask)
    return errors


def _report(kernel, label, method, nodes, current, wait, errors, saw_gone, watcher):
    """Print the verdict and decide the exit status of one operation."""
    for problem in errors:
        _say(f"error: {label} — {problem}")

    if current and sorted(current) != sorted(nodes):
        _say(f"WARNING: {label} came back under different names: "
             f"{' '.join(nodes)} -> {' '.join(current)}")
        _say("WARNING: every hardware test in this repo hardcodes /dev/video4 and "
             "/dev/ttyACM1 — they need the new names, or another replug to get the old ones.")

    missing = [n for n in (current or nodes) if not kernel.exists(n)]
    if errors:
        _say(f"FAIL: {label} — {len(errors)} ioctl(s) failed; the replug did not happen")
        return False
    if not current:
        _say(f"FAIL: {label} — no node came back within {int(wait * 1000)} ms "
             f"(was: {' '.join(nodes)})")
        return False
    if missing:
        _say(f"FAIL: {label} — still missing after {int(wait * 1000)} ms: {' '.join(missing)}")
        return False
    if not saw_gone:
        _say(f"note: {label} never went away — the driver survived the {method} in place. "
             f"That is a measurement, not a failure: use the other method for a real "
             f"disconnect.")
        _say(f"done: {label} — present throughout, nothing to come back")
        return True
    came_back = [ms for node, ms in watcher.back.items() if node in current]
    _say(f"done: {label} back after {max(came_back) if came_back else 0} ms "
         f"({' '.join(current)})")
    return True


# ------------------------------------------------------------------------------------- driver


def plan_operations(target, video, serial, hub):
    """(label, device, nodes) tuples, in the order they should be performed."""

    def need(label, dev):
        if dev is None:
            raise Refused(f"target {target} needs the {label} device, which has not enumerated "
                          f"under hub {hub.name} — try `dongle` to bring the whole thing back")
        return dev

    if target == "video":
        return [("video", need("video", video), need("video", video).nodes)]
    if target == "serial":
        return [("serial", need("serial", serial), need("serial", serial).nodes)]
    if target == "dongle":
        return [("dongle", hub, sorted({n for d in (video, serial) if d for n in d.nodes}))]
    ops = [("video", need("video", video), need("video", video).nodes),
           ("serial", need("serial", serial), need("serial", serial).nodes)]
    return ops if target == "both" else list(reversed(ops))


def _describe_all(video, serial, hub):
    for label, dev in (("video", video), ("serial", serial), ("hub", hub)):
        if dev is None:
            _say(f"resolved {label:6} (not enumerated under {hub.name} — nothing to act on)")
        else:
            _say(dev.describe(label))


def run(args, kernel=None):
    kernel = kernel or Kernel()
    port = getattr(args, "port", None)
    video, serial, hub = resolve(args.sysfs_root, args.video, args.serial, port)
    _describe_all(video, serial, hub)

    ops = plan_operations(args.target, video, serial, hub)
    watched = sorted({n for _, _, nodes in ops for n in nodes})
    _say(f"target: {args.target}  method: {args.method}  gap: {args.gap}s  "
         f"wait: {args.wait}s  watching: {' '.join(watched)}")

    if args.dry_run:
        _say("dry-run: resolved only, no usbfs node opened")
        return 0

    # --sysfs-root is a test parameter. Acting on the machine with a tree that is not /sys
    # would let it name any bus and devnum on the box, so it is refused before anything is
    # opened; the descriptor check in do_operation is the second line of defence (review R1).
    if kernel.real and os.path.realpath(args.sysfs_root) != REAL_SYSFS:
        raise Refused(f"--sysfs-root {args.sysfs_root} is not {REAL_SYSFS}: a fake tree may "
                      f"only be used with --dry-run or --self-test, never to aim an ioctl")

    ok = True
    for i, (label, dev, nodes) in enumerate(ops):
        if i:
            _say("")
            kernel.sleep(args.gap)
            # Re-resolve by port path: the first operation changed devnums, and it may have
            # changed the /dev names too, so the node arguments are no longer trustworthy.
            video, serial, hub = resolve(args.sysfs_root, args.video, args.serial, hub.name)
            label, dev, nodes = plan_operations(args.target, video, serial, hub)[i]
            _say(dev.describe(label))
        ok = do_operation(label, dev, nodes, args.method, args.gap, args.wait,
                          args.sysfs_root, kernel) and ok
    return 0 if ok else 1


# ---------------------------------------------------------------------------------- self-test


def self_test():
    """Run tests/usb_replug_selftest.py against this module. No hardware, no root.

    The cases live next door because they are as large as the thing they test, and a fixture
    that opens usbfs nodes should be readable end to end. They cover resolution and every
    refusal rule and — since the adversarial review — `run()`, `do_operation()` and `Watcher`
    through a fake Kernel.
    """
    here = os.path.dirname(os.path.abspath(__file__))
    path = os.path.join(here, os.pardir, "tests", "usb_replug_selftest.py")
    if not os.path.exists(path):
        print(f"self-test source missing: {path}", file=sys.stderr)
        return 1
    spec = importlib.util.spec_from_file_location("usb_replug_selftest", path)
    module = importlib.util.module_from_spec(spec)
    written = sys.dont_write_bytecode
    sys.dont_write_bytecode = True  # a test fixture must not leave __pycache__ in the repo
    try:
        spec.loader.exec_module(module)
    finally:
        sys.dont_write_bytecode = written
    return module.run(sys.modules[__name__])


def main():
    p = argparse.ArgumentParser(
        description="Kernel-side unplug/replug of the NanoKVM-USB dongle (hardware fixture).")
    p.add_argument("target", nargs="?", choices=TARGETS,
                   help="which device to take down; 'both' is video then serial, --gap apart")
    p.add_argument("--method", choices=("reset", "rebind"), default="reset",
                   help="reset: USBDEVFS_RESET. rebind: DISCONNECT, --gap, CONNECT per interface")
    p.add_argument("--gap", type=float, default=3.0,
                   help="seconds between disconnect and connect, and between 'both' operations")
    p.add_argument("--wait", type=float, default=20.0, help="seconds to wait for nodes to return")
    p.add_argument("--dry-run", action="store_true", help="resolve and print, open nothing")
    p.add_argument("--video", default="/dev/video4")
    p.add_argument("--serial", default="/dev/ttyACM1")
    p.add_argument("--port", default=None,
                   help="name the dongle by sysfs port path (3-2.2, 3-2.2.2) instead of by a "
                        "/dev node — use when a node is missing, which is the case this script "
                        "exists for")
    p.add_argument("--sysfs-root", default=REAL_SYSFS,
                   help="for --self-test; refused for anything but --dry-run")
    p.add_argument("--self-test", action="store_true",
                   help="run the resolution, refusal and operation tests against fakes")
    args = p.parse_args()

    if args.self_test:
        return self_test()
    if args.target is None:
        p.error("a target is required (or --self-test)")
    try:
        return run(args)
    except Refused as e:
        print(f"refused: {e}", file=sys.stderr)
        return 2
    except OSError as e:
        # A device vanishing mid-walk is an error, not a refusal, and must not look like one.
        print(f"error: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
