#!/usr/bin/env python3
r"""Record the slice of sysfs that device discovery reads, as a relocatable directory tree.

Snapshots keep both SuperSpeed port-peer and USB 2.0 internal-hub topologies
under test without moving the hardware between test runs.

The output is laid out exactly like `/sys`, so `RealSysfs::new(OUTDIR)` reads it with no special
casing:

    OUTDIR/devices/pci0000:00/.../usb3/3-2/3-2.2/3-2.2.2/{idVendor,idProduct,...}
    OUTDIR/bus/usb/devices/3-2.2.2   -> relative symlink into devices/
    OUTDIR/class/video4linux/video4  -> relative symlink into devices/
    OUTDIR/class/tty/ttyACM1         -> relative symlink into devices/
    OUTDIR/class/sound/card8         -> relative symlink into devices/

Every symlink is written relative, so the tree can be committed, moved and checked out anywhere.
Only the attributes discovery reads are copied, so the result is a few hundred small files and no
kernel-internal state.

A tree is what `RealSysfs` reads, but it is a thousand tiny files and nobody wants those in
git. The same tree also serialises to **one line-oriented text file** (`.sysfs`), which is what
is committed; `fixtures/sysfs/synthesize.py` expands it back before the tests run.

    #nanokvm-sysfs 1            header, first line
    # anything else               a comment, ignored on read
    d <path>                    a directory with nothing in it (an unoccupied hub port)
    f <path>\t<value>           an attribute file and its exact contents
    l <path>\t<target>          a symlink and its target, verbatim (always relative)

Comments exist so a file's provenance can live in the file. A recording that is not a single
run -- `usb2-desk.sysfs` is 2026-09-10 plus a 2026-09-11 sound addendum -- has to say so where
the next person opens it, not only in MANIFEST.md. They are **not** preserved by a re-record:
`write_snapshot` writes the header and the records, so re-add the comment by hand afterwards
(MANIFEST.md's "Re-recording this desk" says the same).

Paths are relative to the tree root and sorted bytewise, so the file diffs one device at a
time. Values, targets and paths are escaped to pure ASCII -- `\\`, `\t`, `\n`, `\r` and `\xNN`
for every other byte outside printable ASCII -- so the format is lossless for arbitrary
attribute bytes and the file never depends on the reader's locale.

Standard library only, and read-only with respect to the system. Usage:

    scripts/snapshot-sysfs.py [--root /sys] [--force] OUTDIR         # capture -> tree
    scripts/snapshot-sysfs.py [--root /sys] --file [--exclude GLOB] OUT.sysfs
    scripts/snapshot-sysfs.py --pack DIR OUT.sysfs [--exclude GLOB]  # serialise a tree
    scripts/snapshot-sysfs.py --expand FILE DIR [--force]            # materialise a tree
"""
import argparse
import fnmatch
import os
import shutil
import sys
import tempfile

# What discovery reads, plus enough identification for a human reading the fixture.
# `id` and `number` are the ALSA card's: discovery reads `id` for the `hw:CARD=` spelling, and
# `number` is recorded so a reader can check it against the `cardN` directory name that the card
# number is derived from.
ATTRS = [
    "idVendor", "idProduct", "product", "manufacturer", "serial",
    "busnum", "devnum", "speed", "bDeviceClass",
    "bInterfaceNumber", "bInterfaceClass",
    "name", "dev", "index",
    "id", "number",
]

# Class directories discovery scans, with the name prefix it cares about. `/sys/class/sound`
# holds `controlCN`, `pcmCNDMc`, `seq` and `timer` alongside the cards; only `cardN` is a card,
# and only `cardN` is what `discovery::collect_sound_cards` looks at.
CLASSES = [("video4linux", "video"), ("tty", "ttyACM"), ("sound", "card")]


class Snapshot:
    def __init__(self, root, out):
        self.root = root
        self.out = out
        self.captured = {}          # real dir -> mirrored dir
        self.links = []             # (mirrored link path, real target)
        self.attr_count = 0

    def mirror(self, real):
        """Where a real path lands in the output tree, or None if it is outside --root."""
        if real == self.root:
            return self.out
        if not real.startswith(self.root + os.sep):
            return None
        return os.path.join(self.out, os.path.relpath(real, self.root))

    def capture(self, real):
        """Copy one directory's attributes. Idempotent."""
        dst = self.mirror(real)
        if dst is None or real in self.captured:
            return dst
        os.makedirs(dst, exist_ok=True)
        self.captured[real] = dst
        for attr in ATTRS:
            src = os.path.join(real, attr)
            if os.path.islink(src) or not os.path.isfile(src):
                continue
            try:
                with open(src, "rb") as fh:
                    data = fh.read()
            except OSError:
                continue        # write-only or permission-denied attributes are simply skipped
            with open(os.path.join(dst, attr), "wb") as fh:
                fh.write(data)
            self.attr_count += 1
        return dst

    def link(self, link_path, real_target):
        self.links.append((link_path, real_target))

    def usb_devices(self):
        """Every /sys/bus/usb/devices entry: devices, interfaces, and the ports under them."""
        base = os.path.join(self.root, "bus", "usb", "devices")
        names = sorted(os.listdir(base)) if os.path.isdir(base) else []
        ports = 0
        for name in names:
            real = os.path.realpath(os.path.join(base, name))
            if not os.path.isdir(real):
                continue
            self.capture(real)
            self.link(os.path.join(self.out, "bus", "usb", "devices", name), real)
            # Hub-port directories live under the hub's interface 0. They carry the `peer`
            # symlink used for port-peer pairing, and a `device` symlink naming what is plugged in.
            for child in sorted(os.listdir(real)):
                child_real = os.path.join(real, child)
                if "-port" not in child or not os.path.isdir(child_real):
                    continue
                if os.path.islink(child_real):
                    continue
                self.capture(child_real)
                ports += 1
                for link_name in ("peer", "device"):
                    src = os.path.join(child_real, link_name)
                    if os.path.islink(src):
                        self.link(os.path.join(self.mirror(child_real), link_name),
                                  os.path.realpath(src))
        return names, ports

    def class_nodes(self):
        found = []
        for cls, prefix in CLASSES:
            base = os.path.join(self.root, "class", cls)
            if not os.path.isdir(base):
                continue
            for name in sorted(os.listdir(base)):
                if not name.startswith(prefix):
                    continue
                real = os.path.realpath(os.path.join(base, name))
                if not os.path.isdir(real):
                    continue
                dst = self.capture(real)
                if dst is None:
                    continue
                self.link(os.path.join(self.out, "class", cls, name), real)
                device = os.path.join(real, "device")
                if os.path.islink(device):
                    target = os.path.realpath(device)
                    # Capture the link's target as well, so the link survives `write_links`'s
                    # "only link to things that were actually captured" rule even when the target
                    # is not a USB device. A PCI sound card resolves to a PCI device directory
                    # that `usb_devices()` never walks; without this it would land in the fixture
                    # as a *dangling* `device` link, and discovery would then reject it for the
                    # wrong reason -- a missing link rather than a walk that finds no `idVendor`.
                    self.capture(target)
                    self.link(os.path.join(dst, "device"), target)
                found.append((cls, name, real))
        return found

    def write_links(self):
        """Second pass: only link to things that were actually captured."""
        made, skipped = 0, 0
        for link_path, real_target in self.links:
            target = self.mirror(real_target)
            if target is None or not os.path.exists(target):
                skipped += 1
                continue
            os.makedirs(os.path.dirname(link_path), exist_ok=True)
            if os.path.lexists(link_path):
                os.unlink(link_path)
            os.symlink(os.path.relpath(target, os.path.dirname(link_path)), link_path)
            made += 1
        return made, skipped


# ------------------------------------------------------------------------------------------
# The single-file format.
# ------------------------------------------------------------------------------------------
HEADER = "#nanokvm-sysfs 1"
_ESCAPES = {0x5C: "\\\\", 0x09: "\\t", 0x0A: "\\n", 0x0D: "\\r"}
_UNESCAPES = {"\\": 0x5C, "t": 0x09, "n": 0x0A, "r": 0x0D}


def esc(data):
    """Bytes -> printable ASCII. Lossless: every byte round-trips through `unesc`."""
    out = []
    for b in data:
        if b in _ESCAPES:
            out.append(_ESCAPES[b])
        elif 0x20 <= b <= 0x7E:
            out.append(chr(b))
        else:
            out.append("\\x%02x" % b)
    return "".join(out)


def unesc(text):
    """Printable ASCII -> bytes. The inverse of `esc`."""
    out, i = bytearray(), 0
    while i < len(text):
        c = text[i]
        if c != "\\":
            out.append(ord(c))
            i += 1
        elif text[i + 1] == "x":
            out.append(int(text[i + 2:i + 4], 16))
            i += 4
        else:
            out.append(_UNESCAPES[text[i + 1]])
            i += 2
    return bytes(out)


def pack(treedir, exclude=()):
    """Serialise a directory tree to a sorted list of record lines.

    Symlinks are recorded by their raw target, so a relocatable tree stays relocatable. Only
    *empty* directories get a `d` record; every other directory is implied by what is in it.
    """
    records = []
    for dirpath, dirnames, filenames in os.walk(treedir):
        empty = not dirnames and not filenames
        real_dirs = []
        for name in dirnames:
            full = os.path.join(dirpath, name)
            rel = os.path.relpath(full, treedir)
            if os.path.islink(full):
                records.append("l %s\t%s" % (esc(rel.encode()), esc(os.readlink(full).encode())))
            else:
                real_dirs.append(name)
        dirnames[:] = real_dirs
        for name in filenames:
            full = os.path.join(dirpath, name)
            rel = esc(os.path.relpath(full, treedir).encode())
            if os.path.islink(full):
                records.append("l %s\t%s" % (rel, esc(os.readlink(full).encode())))
            else:
                with open(full, "rb") as fh:
                    records.append("f %s\t%s" % (rel, esc(fh.read())))
        if empty and dirpath != treedir:
            records.append("d %s" % esc(os.path.relpath(dirpath, treedir).encode()))
    if exclude:
        records = [r for r in records if not excluded(r.split(" ", 1)[1].split("\t", 1)[0], exclude)]
    return sorted(records, key=lambda r: (r.split(" ", 1)[1].split("\t", 1)[0], r))


def excluded(path, globs):
    return any(fnmatch.fnmatchcase(path, g) for g in globs)


def write_snapshot(records, path):
    with open(path, "w") as fh:
        fh.write(HEADER + "\n")
        for line in records:
            fh.write(line + "\n")


def expand(snapshot, treedir):
    """Materialise a snapshot file as a directory tree. Creates `treedir` if it is missing.

    Returns the number of records applied -- comments and blank lines are neither."""
    with open(snapshot) as fh:
        lines = fh.read().splitlines()
    if not lines or lines[0] != HEADER:
        raise ValueError("%s: not a %s snapshot" % (snapshot, HEADER))
    os.makedirs(treedir, exist_ok=True)
    applied = 0
    for n, line in enumerate(lines[1:], start=2):
        if not line or line.startswith("#"):
            continue
        applied += 1
        kind, rest = line[0], line[2:]
        path, _, payload = rest.partition("\t")
        dst = os.path.join(treedir, unesc(path).decode())
        if kind == "d":
            os.makedirs(dst, exist_ok=True)
        elif kind == "f":
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            with open(dst, "wb") as out:
                out.write(unesc(payload))
        elif kind == "l":
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            if os.path.lexists(dst):
                os.unlink(dst)
            os.symlink(unesc(payload).decode(), dst)
        else:
            raise ValueError("%s:%d: unknown record type %r" % (snapshot, n, kind))
    return applied


def read(path):
    try:
        with open(path) as fh:
            return fh.read().strip()
    except OSError:
        return None


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--root", default="/sys", help="sysfs mount point to record (default /sys)")
    ap.add_argument("--force", action="store_true",
                    help="write into a non-empty OUTDIR, or overwrite an existing snapshot file")
    ap.add_argument("--file", action="store_true",
                    help="write OUTDIR as a single-file snapshot instead of a directory tree")
    ap.add_argument("--pack", nargs=2, metavar=("DIR", "FILE"),
                    help="serialise an existing tree to a snapshot file; touches no /sys")
    ap.add_argument("--expand", nargs=2, metavar=("FILE", "DIR"),
                    help="materialise a snapshot file as a directory tree")
    ap.add_argument("--exclude", action="append", default=[], metavar="GLOB",
                    help="drop records whose tree-relative path matches GLOB (repeatable); "
                         "only meaningful when writing a snapshot file")
    ap.add_argument("outdir", nargs="?")
    args = ap.parse_args()

    if args.pack and args.expand:
        print("error: --pack and --expand are mutually exclusive", file=sys.stderr)
        return 1

    if args.expand:
        src, dst = args.expand
        if os.path.isdir(dst) and os.listdir(dst) and not args.force:
            print(f"error: {dst} exists and is not empty (use --force)", file=sys.stderr)
            return 1
        n = expand(src, dst)
        print(f"expanded  {n} records from {src} into {dst}")
        return 0

    if args.pack:
        treedir, dst = args.pack
        if not os.path.isdir(treedir):
            print(f"error: {treedir} is not a directory", file=sys.stderr)
            return 1
        if os.path.exists(dst) and not args.force:
            print(f"error: {dst} exists (use --force)", file=sys.stderr)
            return 1
        records = pack(treedir, args.exclude)
        write_snapshot(records, dst)
        print(f"packed    {treedir} -> {dst}: {len(records)} records")
        return 0

    if args.outdir is None:
        ap.error("an output path is required unless --pack or --expand is given")

    root = os.path.realpath(args.root)
    if not os.path.isdir(os.path.join(root, "bus", "usb", "devices")):
        print(f"error: {root} does not look like sysfs (no bus/usb/devices)", file=sys.stderr)
        return 1
    out = os.path.abspath(args.outdir)
    if args.file:
        if os.path.exists(out) and not args.force:
            print(f"error: {out} exists (use --force)", file=sys.stderr)
            return 1
        # Capture into a scratch tree and serialise that, so the two output forms can never
        # drift: the file is always exactly what the tree writer produced.
        scratch = tempfile.mkdtemp(prefix="snapshot-sysfs.")
        snapshot_file, out = out, os.path.join(scratch, "tree")
    else:
        snapshot_file, scratch = None, None
        if os.path.isdir(out) and os.listdir(out) and not args.force:
            print(f"error: {out} exists and is not empty (use --force)", file=sys.stderr)
            return 1

    snap = Snapshot(root, out)
    usb_names, ports = snap.usb_devices()
    class_found = snap.class_nodes()
    made, skipped = snap.write_links()

    if snapshot_file:
        records = pack(out, args.exclude)
        write_snapshot(records, snapshot_file)
        shutil.rmtree(scratch, ignore_errors=True)

    devices = [n for n in usb_names if ":" not in n]
    print(f"root      {root}")
    print(f"output    {snapshot_file or out}"
          + (f" ({len(records)} records)" if snapshot_file else ""))
    print(f"captured  {len(devices)} USB devices, {len(usb_names) - len(devices)} interfaces, "
          f"{ports} hub ports, {snap.attr_count} attributes")
    print(f"symlinks  {made} written, {skipped} skipped (target outside the snapshot)")
    print()
    print("USB devices:")
    for name in devices:
        real = os.path.realpath(os.path.join(root, "bus", "usb", "devices", name))
        vid, pid = read(os.path.join(real, "idVendor")), read(os.path.join(real, "idProduct"))
        product = read(os.path.join(real, "product")) or ""
        speed = read(os.path.join(real, "speed")) or "?"
        print(f"  {name:<16} {vid}:{pid}  {speed:>5}M  {product}")
    print()
    print("class nodes:")
    for cls, name, real in class_found:
        usb = os.path.realpath(os.path.join(real, "device"))
        while usb != "/" and not os.path.exists(os.path.join(usb, "idVendor")):
            usb = os.path.dirname(usb)
        print(f"  /dev/{name:<12} {cls:<12} -> {os.path.basename(usb)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
