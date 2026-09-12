# CH9329 protocol

The NanoKVM-USB uses a CH9329 serial bridge for keyboard and mouse input. The client
opens its CDC-ACM port at 57,600 baud. Video and audio travel through separate USB
interfaces; neither is carried by this protocol.

The encoders and incremental parser are in `src/proto/`. The machine-readable packet
reference is [`fixtures/packets/ch9329.toml`](../fixtures/packets/ch9329.toml).
Tests read those bytes independently of the encoders. Captured traffic documents what
the device emitted; it does not make a malformed frame valid.

## Framing

```text
57 AB ADDR CMD LEN DATA[LEN] SUM
```

The client uses address zero. `SUM` is the low byte of the sum of every preceding
byte, including the entire payload. A complete frame is `6 + LEN` bytes long.
Length checks precede indexing, and incremental parsing supports split headers,
concatenated frames, garbage, and corrupt checksums.

| Operation | Command | Payload |
| --- | --- | --- |
| Device information | `01` | Empty request. |
| Keyboard report | `02` | Eight bytes: modifiers, reserved zero, six usages. |
| Absolute mouse | `04` | Seven bytes: mode `02`, buttons, X low/high, Y low/high, wheel. |
| Relative mouse | `05` | Five bytes: mode `01`, buttons, dx, dy, wheel. |
| USB identification string | `0A` | String selector; see packet fixtures. |

Successful replies set command bit `80`. Error replies use `CMD | C0` with a one-byte
error code; the recorded device reported `E4` for checksum failure and `E5` for an
invalid parameter. Reply matching uses command bytes, not arrival order.

## Device information and strings

`GET_INFO` reports firmware, target connection, and NumLock/CapsLock/ScrollLock state.
The tested bridge reports firmware 1.8. Unsolicited `81` frames follow target lock
changes, so they can appear while another command is outstanding. The serial reader
publishes those changes separately from transaction replies.

USB strings are read only by `nanokvm devices --probe`. The measured response payload
is `[type, len, ascii…]`; the parser also accepts a form without the length byte.
It checks the selector because all three string replies share the same command.
Control characters are escaped for display. The string returned by the bridge is
useful for identification after opening, but cannot establish which port to open.

## Keyboard reports

The modifier byte contains the eight left/right modifier bits. The remaining six
usage slots hold non-modifier keys. The writer tracks physical state separately from
that projection. When all six slots are occupied, another key is suppressed until
its release; freeing a slot does not generate a delayed press for it.

Physical-key forwarding uses the target's keyboard layout. Text injection instead
compiles characters against the declared US QWERTY layout. These paths share HID
encoders but have different semantics: a physical key does not guarantee a character.

The keymap preserves the upstream `LaunchApp2`/`BrowserSearch` usage collision at
`F0`, maps `WakeUp`, and leaves keys without a supported usage unmapped. The detailed
unmapped-key table lives beside `hid_key` in `src/proto/keymap.rs`.

## Mouse reports

The leading mouse mode byte is required. The tested device acknowledges a report
without it while ignoring the intended motion. An acknowledgement therefore cannot
serve as an end-to-end pointer test.

Relative dx, dy, and wheel values are signed. Larger accumulated motion is split
into report-sized deltas. Positive dx moves right and positive dy moves down;
the target's acceleration determines the resulting pixel distance.

Absolute coordinates use the measured law:

```text
effective = min(value & 0x1FFF, 4095)
pixel = floor(effective * extent / 4096)
```

The client clamps to 4095 before sending. Values 4096 through 8191 clamp at the far
edge in the device, while 8192 wraps to zero. For extents up to 4096, the inverse
mapping is `ceil(pixel * 4096 / extent)`. Flooring that inverse can miss the final
row or column. Larger extents cannot address every pixel.

The [absolute-coordinate fixture manifest](../fixtures/frames/absrange/MANIFEST.md)
records cursor positions observed in captured frames. Relative and absolute reports
address separate HID mouse devices, so release must clear both when applicable.

## Malformed and delayed traffic

The bridge can emit `57 AB 00 FE 00`: a five-byte reply with no checksum. Alone, it
remains incomplete until the reader's silence timer expires it. Followed by a valid
frame, its missing checksum slot consumes the next header byte. The parser resumes
scanning one byte after a failed header so it can recover that following frame.

A partial host write has a different consequence: the bridge waits indefinitely for
its remaining bytes. The next command can become the tail of that incomplete frame.
The client completes writes before observing cancellation and sends a 16-byte zero
preamble when commissioning a fresh link, followed by release-all and `GET_INFO`.
This preamble was tested against a torn keyboard report; it is not a general recovery
claim for arbitrary bridge configuration traffic. Baud reconfiguration is unsupported.

There are no transaction sequence numbers. A reply arriving after timeout can be
byte-identical to the reply expected by the next request. The reader uses a quiet
window and late-reply tracking to avoid accepting it as fresh; retrying `GET_INFO`
on the same timed-out link can consequently encounter another timeout.

## Verification

```sh
python3 scripts/check-fixtures.py
cargo test --test proto_fixtures --test proto_report --test proto_proptests
```

Hardware validation must observe a consequence such as a returned lock-state change
or cursor movement in captured video. Historical timings and the limitations of those
measurements are in [Hardware measurements](hardware.md).
