# NanoKVM-USB-rs

[![CI](https://github.com/chrishoage/NanoKVM-USB-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/chrishoage/NanoKVM-USB-rs/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/chrishoage/NanoKVM-USB-rs)](https://github.com/chrishoage/NanoKVM-USB-rs/releases/latest)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

A native Linux client for the Sipeed NanoKVM-USB. View and control a connected
computer through the dongle, play its HDMI audio, and type clipboard text without
running the vendor's browser client. The target computer needs no network connection.

The client includes a Wayland viewer and commands for device discovery, screenshots,
key chords, text entry, and keyboard macros. It has been tested with a NanoKVM-USB
Pro 4K60 and niri on Arch Linux. Other models and compositors have not been verified.

## Install

Download the binary for the latest release, verify it, and unpack it:

```sh
tag=$(curl -fsSL https://api.github.com/repos/chrishoage/NanoKVM-USB-rs/releases/latest | grep -m1 '"tag_name"' | cut -d'"' -f4)
base=nanokvm-$tag-x86_64-unknown-linux-gnu
curl -fLO "https://github.com/chrishoage/NanoKVM-USB-rs/releases/download/$tag/$base.tar.gz"
curl -fLO "https://github.com/chrishoage/NanoKVM-USB-rs/releases/download/$tag/$base.tar.gz.sha256"
sha256sum -c "$base.tar.gz.sha256"
tar xzf "$base.tar.gz"
```

The binary is built on Ubuntu 22.04, so it needs glibc 2.35 or newer. It links
against alsa-lib, and loads Wayland and a Vulkan driver at runtime, so it needs a
Wayland session and a working GPU driver. Build from source for anything older.

On Arch, build the package from the PKGBUILD in this repository:

```sh
git clone https://github.com/chrishoage/NanoKVM-USB-rs.git
cd NanoKVM-USB-rs/packaging
makepkg -si
```

That tracks the default branch rather than a release, and installs the same
`nanokvm` binary to `/usr/bin`.

## Build and run

Build requirements are Rust 1.89 or newer, a C toolchain, Linux headers, libclang,
`pkg-config`, and ALSA development files. The viewer also needs a Wayland session
and a working GPU driver. Audio plays through the host's ALSA `default` device,
which can route through PipeWire when the host is configured for it.

From the repository directory:

```sh
cargo build --release --locked
./target/release/nanokvm devices
./target/release/nanokvm
```

Connect the target's HDMI output and USB keyboard/mouse connection to the dongle,
and connect the dongle to the Linux host. The host user needs permission to open
its video, serial, and sound devices. If a device cannot be opened, check the path
and permissions reported by the command; see [troubleshooting](docs/usage.md#troubleshooting).

The default capture mode is 1920 × 1080 at 60 fps. Device paths are discovered from
USB topology. If discovery cannot select one pair, use the paths shown by `devices`:

```sh
./target/release/nanokvm --video /dev/video4 --serial /dev/ttyACM1
```

Those paths are examples. They can change when a device is unplugged. Automatically
discovered paths are resolved again on reconnect; explicit paths stay fixed.

## Use the viewer

- Click the video or press **Enter** to send keyboard and mouse input to the target.
  The click or keypress used to start capture is consumed locally.
- Press **Pause** to return control to the host. Changing focus also releases input.
  On niri, **Mod+Escape** releases compositor shortcut inhibition and capture.
- Press **Shift+Pause** to type the host clipboard into the target. Press **Pause**
  again to cancel. Paste supports US QWERTY text and refuses unsupported characters
  before typing. The target's active application determines where the text goes.
- Use the floating menu to change capture resolution, send built-in shortcuts,
  adjust mouse behavior, or mute audio. Drag it to reposition it; **Hide** collapses it.

The window title reports connection problems and release failures. If a release
could not be sent, the target may still hold a key or mouse button. After a serial
reconnect, capture input again to resume control.

Audio starts automatically when the dongle's sound card can be paired. Use
`--no-audio` to disable it. Video and keyboard control continue if audio is unavailable.

## Command line

The examples below assume `nanokvm` is on `PATH`; otherwise use
`./target/release/nanokvm`.

| Command | Purpose |
| --- | --- |
| `nanokvm devices` | List devices and pairing information. |
| `nanokvm devices --probe` | Also query the selected bridge's firmware, connection, and lock state. |
| `nanokvm shot screen.png` | Save one frame as PNG. Use `.jpg` for the device's JPEG bytes. |
| `nanokvm key ctrl+alt+del` | Send a key chord and release it. |
| `nanokvm type 'hello'` | Type text using the target's US QWERTY layout. |
| `nanokvm macro commands.macro` | Run a file of keyboard and wait steps. |

Use `--dry-run` with `key`, `type`, or `macro` to inspect reports without opening
any devices. Run `nanokvm --help` or `nanokvm <command> --help` for options.

## Documentation

- [Usage and troubleshooting](docs/usage.md): device selection, controls, settings,
  screenshots, and scripting.
- [Architecture](docs/architecture.md): subsystem ownership, input guarantees, and recovery.
- [Protocol](docs/protocol.md): CH9329 framing, report formats, and device quirks.
- [Hardware measurements](docs/hardware.md): recorded results, their limits, and open validation work.
- [Development and testing](docs/development.md): build checks, fixtures, and hardware-test precautions.

The viewer is Linux/Wayland only. Text injection currently supports US QWERTY;
macros cannot send mouse input. Remote access and session recording are not implemented.

## License

Dual-licensed under either the [MIT license](LICENSE-MIT) or the
[Apache License, Version 2.0](LICENSE-APACHE), at your option.
