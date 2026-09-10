#!/usr/bin/env python3
"""Revalidate fixtures/packets/ch9329.toml from scratch.

These frames are the AUTHORITY for the protocol tests (NATIVE_CLIENT_PLAN §9.1), so nothing
here trusts the file's own arithmetic. Every checksum is recomputed from the raw frame bytes,
and every declared field is checked against them.

Needs no hardware. Exits non-zero on any failure.
"""
import re
import sys
import tomllib
from pathlib import Path

FIXTURE = Path(__file__).resolve().parent.parent / "fixtures/packets/ch9329.toml"

HEAD = (0x57, 0xAB)
# Command -> (payload length, leading mode byte). The mode byte is the correction from
# STAGE0_FINDINGS A17: the device ACKs a mouse report without it and does nothing.
MOUSE = {0x04: (7, 0x02), 0x05: (5, 0x01)}
KEYBOARD_CMD = 0x02
KEYBOARD_LEN = 8


HEX = re.compile(r"^[0-9A-Fa-f]{2}$")


def parse(frame):
    """Leading two-hex-digit tokens only. Fields may carry a trailing prose comment."""
    out = []
    for tok in frame.split():
        if not HEX.match(tok):
            break
        out.append(int(tok, 16))
    return out


def check_frame(entry, kind, errors):
    name = entry.get("name", "<unnamed>")
    where = f"{kind} {name}"
    by = parse(entry["frame"])

    def bad(msg):
        errors.append(f"{where}: {msg}")

    if len(by) < 6:
        bad(f"frame is {len(by)} bytes, minimum is 6")
        return
    if tuple(by[:2]) != HEAD:
        bad(f"header is {by[0]:#04x} {by[1]:#04x}, expected 0x57 0xAB")

    cmd, declared_len = by[3], by[4]
    payload = by[5:-1]

    if len(by) != 6 + declared_len:
        bad(f"LEN says {declared_len} payload bytes, frame carries {len(by) - 6}")
        return

    recomputed = sum(by[:-1]) & 0xFF
    if recomputed != by[-1]:
        bad(f"checksum is {by[-1]:#04x}, recomputes to {recomputed:#04x}")

    # Declared fields must agree with the bytes, so a hand-edit cannot drift them apart.
    if "cmd" in entry and entry["cmd"] != cmd:
        bad(f"cmd field {entry['cmd']:#04x} != frame CMD {cmd:#04x}")
    if "len" in entry and entry["len"] != declared_len:
        bad(f"len field {entry['len']} != frame LEN {declared_len}")
    if "checksum" in entry and entry["checksum"] != by[-1]:
        bad(f"checksum field {entry['checksum']:#04x} != frame SUM {by[-1]:#04x}")
    if "data" in entry and list(entry["data"]) != payload:
        bad("data field does not match the payload bytes in frame")

    # Requests only: shape rules the device will not report violations of.
    if kind == "packet":
        if cmd in MOUSE:
            want_len, want_mode = MOUSE[cmd]
            if declared_len != want_len:
                bad(f"mouse cmd {cmd:#04x} payload is {declared_len} bytes, must be {want_len}")
            elif payload[0] != want_mode:
                bad(f"mouse cmd {cmd:#04x} mode byte is {payload[0]:#04x}, must be {want_mode:#04x}")
        if cmd == KEYBOARD_CMD and declared_len != KEYBOARD_LEN:
            bad(f"keyboard payload is {declared_len} bytes, must be {KEYBOARD_LEN}")


def check_disagreements(entries, errors):
    """A disagreement records where derivation and observation part company. Check the file is
    internally honest about that, and that any complete frame quoted in one is arithmetically
    right -- a deliberately truncated frame is the anomaly being documented and is exempt."""
    for e in entries:
        name = e.get("name", "<unnamed>")
        if not e.get("note"):
            errors.append(f"disagreement {name}: no note explaining the disagreement")

        derived, observed = e.get("derived_reply"), e.get("observed_reply")
        if derived is not None and observed is not None and derived.strip() == observed.strip():
            errors.append(f"disagreement {name}: derived and observed are identical, "
                          "so nothing disagrees")

        for field in ("trigger", "derived_reply", "observed_reply"):
            text = e.get(field)
            if not text:
                continue
            by = parse(text)
            # Only complete frames are checkable. Short ones are the documented anomaly, and
            # entries using placeholders like <xLo> stop the parse early by design.
            if len(by) >= 6 and len(by) == 6 + by[4]:
                recomputed = sum(by[:-1]) & 0xFF
                if recomputed != by[-1]:
                    errors.append(
                        f"disagreement {name}: {field} quotes a complete frame whose checksum "
                        f"is {by[-1]:#04x} but recomputes to {recomputed:#04x}")


def main():
    if not FIXTURE.exists():
        print(f"FAIL: {FIXTURE} not found", file=sys.stderr)
        return 1
    doc = tomllib.load(FIXTURE.open("rb"))
    errors = []

    packets = doc.get("packet", [])
    responses = doc.get("response", [])
    disagreements = doc.get("disagreement", [])

    for e in packets:
        check_frame(e, "packet", errors)
    for e in responses:
        check_frame(e, "response", errors)
    check_disagreements(disagreements, errors)

    names = [e.get("name") for e in packets + responses]
    for n in set(names):
        if names.count(n) > 1:
            errors.append(f"duplicate name {n!r}")

    total = len(packets) + len(responses)
    if errors:
        print(f"FAIL: {len(errors)} problem(s) across {total} frames\n", file=sys.stderr)
        for e in errors:
            print(f"  {e}", file=sys.stderr)
        return 1

    print(f"OK: {len(packets)} requests, {len(responses)} responses, "
          f"{len(disagreements)} documented anomalies")
    print("     every checksum recomputed, every declared field cross-checked,")
    print("     mouse mode bytes and payload lengths verified")
    return 0


if __name__ == "__main__":
    sys.exit(main())
