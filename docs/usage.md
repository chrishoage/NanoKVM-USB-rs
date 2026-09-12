# Usage

Run `nanokvm` without a subcommand to open the viewer. Run `nanokvm --help` for
viewer options and `nanokvm <command> --help` for command-specific options.

## Device selection

```sh
nanokvm devices
nanokvm devices --probe
```

The listing includes video nodes, serial nodes, sound cards, and the evidence used
to pair them. A video device can expose both a capture node and a metadata node;
only the capture node carries images. Listing may open candidate video nodes for
a read-only capability query. It does not open serial unless `--probe` is given.

`--probe` queries only the selected serial node. It reports firmware version,
whether the bridge sees a target, lock states, and USB identification strings.
These identify the bridge and target connection; they do not verify that a target
application acted on a keypress.

Automatic pairing uses a shared USB device or the kernel's port-peer relationship.
On the tested USB 2.0 connection, it uses the dongle's internal hub and three device
identifiers. That last rule is marked `internal hub (degraded)` because matching
commodity identifiers cannot establish unique device identity.

If selection is missing or ambiguous, inspect the listing and supply paths:

```sh
nanokvm --video /dev/video4 --serial /dev/ttyACM1
nanokvm key --serial /dev/ttyACM1 ctrl+c
nanokvm shot --video /dev/video4 screen.jpg
```

For the viewer, supplying both paths bypasses pair selection; supplying one
constrains discovery. A subcommand can use its required explicit node even when
the other device is absent. Discovered video and serial nodes are resolved again
on reconnect. Explicit paths are used unchanged, including after unplugging.
Do not assume a path still belongs to the same device after re-enumeration.

`--serial`, `--video`, `--width`, `--height`, and `--fps` can appear before or after
a subcommand. Size and rate options affect video capture. Viewer-only options,
such as `--pointer`, `--no-audio`, and `--stats-interval`, are rejected with subcommands.

## Viewer controls

| Action | Control |
| --- | --- |
| Capture keyboard and mouse | Click the video or press Enter. |
| Release input | Pause, focus another window, or niri's Mod+Escape. |
| Paste the host clipboard | Shift+Pause, or Paste in the Keyboard menu. |
| Cancel paste | Pause or Shift+Pause; focus loss also cancels it. |
| Change capture size | Video menu; choices come from the device. |
| Send Win+Tab or Ctrl+Alt+Del | Keyboard menu while captured. |
| Change pointer behavior | Mouse menu. |
| Mute output | Audio menu. |
| Move or collapse the menu | Drag it or select Hide. |

Capturing consumes both edges of the activating click or Enter keypress. While
captured, keyboard shortcuts are forwarded subject to the compositor's inhibition
support and reserved bindings. Pause is handled locally. On niri, Mod+Escape can
send an initial Super press to the target before the compositor recognizes the
escape chord; a target that reacts to a lone Super tap may open its application menu.

Absolute pointer mode maps the host pointer through the displayed video rectangle.
Relative mode locks the pointer to the viewer and sends movement deltas; the target
applies its own pointer acceleration. Use `--pointer relative` at startup or change
the mode in the menu. Release capture to regain access to the host pointer and menu
when relative mode is locked.

Changing capture resolution briefly interrupts the picture while the stream reopens.
It does not change the target computer's display settings. The menu shows both the
requested capture mode and the size of arriving frames, which can temporarily differ.
The tested dongle rescales the target's HDMI image into the selected capture size.

## Clipboard paste

Paste types into the target's focused application. It does not transfer clipboard
ownership or files to the target. Newlines become Enter keystrokes, so pasting into
a shell can execute commands.

US QWERTY is the supported target layout. All text is checked before the first
keystroke; unsupported characters are listed with their first positions. CRLF and
CR line endings become LF. Clipboard reads are limited to 16 MiB and have a deadline.
A compositor without a supported data-control protocol disables paste.

Before sending letters, the viewer requests fresh lock state from the bridge.
It refuses the paste if CapsLock is on or the state cannot be read. Punctuation-only
text can still be accepted with CapsLock on. The viewer has no CapsLock override;
`nanokvm type` provides explicit alternatives for a command-line invocation.

Paste submits one key transition every 40 ms. An unshifted character usually needs
two transitions; a shifted character needs four. The progress count is transitions
accepted by the input queue, not characters confirmed by the target. Physical keypresses and mouse button presses are suppressed during paste.
Releases for already-held input, pointer motion, and scrolling still reach the
target. The menu remains usable where pointer capture permits it.

Canceling stops further submission and requests release-all. A partial paste stays
partial; it is not replayed after reconnect. Clipboard text and offending characters
are displayed in the menu as needed, but are not written to logs or settings.

## Settings

The viewer reads `$XDG_CONFIG_HOME/nanokvm/config.toml`, falling back to
`$HOME/.config/nanokvm/config.toml`. A relative `XDG_CONFIG_HOME` is ignored. If
neither location is usable, settings are not persisted.

A missing file uses defaults. A partial file is valid; omitted keys use defaults.
Malformed values and unknown keys produce a startup error with the file location.
This also means an older binary can reject settings written by a newer one.

These are the default values:

```toml
menu_open = true
mouse_mode = "absolute"
cursor_hidden = false
wheel_direction = "natural"
audio_muted = false
pill_x = 16.0
pill_y = 16.0
layout = "us"
```

`mouse_mode` accepts `absolute` or `relative`; `wheel_direction` accepts `natural`
or `inverted`. Menu position is in logical points. The open submenu, capture size,
and frame rate are not saved. A `--pointer` override changes the initial live mode;
menu changes are what update the saved setting. Settings are saved on change through
a temporary file and rename. A failed save is reported and can be retried.

## Screenshots

```sh
nanokvm shot screen.jpg
nanokvm shot screen.png
nanokvm shot - > screen.jpg
nanokvm shot --width 1280 --height 720 --fps 60 screen.png
```

Omitting the path creates a timestamped JPEG in the current directory. `.jpg` and
`.jpeg` retain the frame's original JPEG bytes; `.png` decodes and encodes one frame.
`-` writes JPEG to stdout. File output is replaced atomically on success.

The default `--skip 8` discards startup frames that may have the previous resolution.
Incomplete JPEGs are rejected. By default, a frame must match the negotiated size:
30 consecutive mismatches trigger one stream restart, and 90 total mismatches end
the attempt. Use `--any-size` to accept another size without that recovery. `--timeout`
defaults to 10 seconds and accepts at most 3600. A disconnect fails the screenshot;
it does not start the viewer's reconnect loop.

## Keyboard commands

```sh
nanokvm key ctrl+alt+del
nanokvm key --dry-run A a
nanokvm type --layout us 'hello'
printf 'hello\n' | nanokvm type -
nanokvm type --caps-lock compensate 'Hello'
```

A chord combines modifiers with one key, separated by `+`. Modifiers include
`ctrl`, `alt`, `shift`, `super`, and their right-hand forms `rctrl`, `ralt`, `rshift`,
`rsuper`. A key can be a name such as `enter`, `f10`, `pgdn`, or `kp5`, or a printable
character. Modifier-only chords tap those modifiers. See `nanokvm key --help` for
the full syntax and aliases.

Single-character chord tokens use US QWERTY mapping and preserve case: `A` includes
Shift, while `a` does not. `key` has no layout option. The target's layout and lock
state still determine the resulting character.

`type` accepts a text argument, or `-` to read stdin. `type` and `macro` support
`--layout us`; unsupported layouts and characters are errors. CapsLock behavior is
chosen with `--caps-lock`:

| Value | Effect when CapsLock would change the text |
| --- | --- |
| `refuse` (default) | Abort before sending a keyboard report. |
| `ignore` | Send the compiled reports without adjustment. |
| `compensate` | Invert Shift for letters. |

The initial lock-state check cannot protect against another source changing
CapsLock during delivery. Check the target's output after a long script.

### Macro files

```text
# example.macro
layout us
key ctrl+alt+t
wait 500
type echo hello
key enter
```

```sh
nanokvm macro --dry-run example.macro
nanokvm macro example.macro
```

Each line is `key`, `type`, or `wait`. `wait` takes milliseconds, up to 3,600,000
per step. An optional `layout` directive must precede the first step and agree
with `--layout` if provided. Blank lines and whole-line `#` comments are ignored.
`type` consumes text through the end of its line; an inline `#` is literal text.
There are no mouse steps.

### Delivery and interruption

`key`, `type`, and `macro` compile all input before opening serial and report all
compilation problems together. `--dry-run` performs no discovery, reads no sysfs,
and opens no device. It prints the reports and compares known frames with the
protocol fixtures.

Live delivery waits for each report's acknowledgement and defaults to a 40 ms delay
between reports (`--delay-ms`). Unlike viewer paste, a shifted character can be
represented by one press report and one release report. Acknowledgement establishes
bridge receipt, not application behavior on the target.

A failed sequence reports partial progress and attempts a keyboard release. It does
not reconnect or retry the sequence. SIGINT, SIGTERM, and SIGHUP request cleanup;
SIGKILL and loss of the host cannot be caught. If release fails, inspect the target
before continuing. Close the viewer before using another command that needs its
serial or capture device.

## Troubleshooting

| Symptom | What to check |
| --- | --- |
| No pair, or several pairs | Read `nanokvm devices`; verify the dongle's connections and select explicit paths if needed. |
| Permission denied | Inspect permissions or ACLs on the reported node. Grant the user device access using the host's device policy. Avoid running the viewer as root. |
| Device busy | Close other viewers, serial commands, or recorders using the same device. |
| No window | Run inside a Wayland session with a working GPU driver. The viewer has not been verified on other compositors beyond niri. |
| Serial disconnected | Reconnect the dongle. Discovered paths can follow node renumbering; explicit paths may need updating. Capture again after recovery. |
| Release unsent | The target may still hold input. Restore the link and verify release before resuming. |
| Old image remains | The last decoded frame remains visible during stalls and disconnects. Check the title for the reported condition. |
| Picture present while target is off | This dongle can keep streaming a placeholder image. Frame arrival does not establish HDMI signal presence. |
| Wrong capture size | Allow recovery to finish. The title reports when the device still differs from the negotiated size; for screenshots, consider `--any-size`. |
| Audio unavailable | Check the sound-card pairing, permissions, and ALSA default output. Audio failure does not stop video or input. |
| Audio missing after moving USB ports | Restart the viewer; audio recovery retains the original USB port path. |
| Paste disabled or refused | Capture input, check compositor data-control support, turn off target CapsLock, and review unsupported characters in the Keyboard menu. |
| Settings error | Correct the named key or move the file aside to start with defaults. |

`--stats-interval N` logs capture, rendering, and input measurements every N seconds,
plus audio details when applicable. The default is 5; `0` disables periodic statistics.
Capture-to-submit age ends at GPU submission, and audio buffer size is configured
capacity. Neither is an end-to-end latency measurement.
