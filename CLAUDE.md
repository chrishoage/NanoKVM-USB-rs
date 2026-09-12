# Working in this repository

Native Linux/Wayland client for the Sipeed NanoKVM-USB. One Rust package; the
`nanokvm` binary starts the viewer or a CLI command.

## References

- For user-visible behavior and command examples, read [usage](docs/usage.md).
- Before changing input ordering, ownership, or recovery, read [architecture](docs/architecture.md).
- Before changing CH9329 bytes, read [protocol](docs/protocol.md) and
  `fixtures/packets/ch9329.toml`. Tests read those fixtures; do not retype expected frames.
- For build checks, fixture generation, or live testing, read [development](docs/development.md).
- For measured device quirks and unverified cases, read [hardware measurements](docs/hardware.md).

## Local hardware restrictions

Identify devices from sysfs. The recorded dongle has video `345f:2133` and serial
`1a86:55d3` under hub `1a40:0101`; node and card numbers change after replug.
Everything on USB bus 5 is the user's unrelated hardware: never open or reset it.

The target's only feedback is captured video. Do not unplug HDMI, change the target's
resolution, or send blind mouse clicks. Always attempt release-all before exiting
anything that sent input. USB reset/replug work requires explicit task authorization.

Run hardware tests one binary at a time. Any test using a fixed node path must document
the intended USB identity. Isolate `XDG_CONFIG_HOME` for viewer runs. Audio tests must
verify the actual sink before playback; a requested route can fall back to speakers.

## Completion checks

Run formatting, Clippy with and without `hardware`, and the test suite as documented
in `docs/development.md`. Some capture tests need an untracked JPEG corpus; report
missing fixtures separately from failures. Check rustdoc after API-comment changes.

Keep `reference/` read-only. The prototype branch is historical evidence; changes to
the maintained client belong in `src/`. Generated sysfs trees are disposable: edit
the recording or generator instead.

## Git

Commit to `main` or push only when the user requests it. Otherwise leave changes for
the user to review and commit. User authorization for the current task takes precedence.
