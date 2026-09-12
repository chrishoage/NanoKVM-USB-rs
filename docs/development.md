# Development and testing

Build with `cargo build --locked`. The package declares Rust 1.89 as its minimum
version. Keep dependency changes compatible with that version and the committed lockfile.
The host needs Linux headers, a C toolchain, libclang for V4L2 bindings, `pkg-config`,
and ALSA development files. Tests also need Python 3 for sysfs fixture generation.

The executable uses the host's dynamic libraries and GPU/Wayland stack. A static musl
prototype could build but could not load the GPU and window-system libraries it needed.
The recorded Linux binary linked libc, libm, libgcc_s, and libasound. Do not assume
a binary built against a newer glibc will run on an older distribution.

## Checks

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --features hardware -- -D warnings
cargo test
cargo doc --no-deps
python3 scripts/check-fixtures.py
python3 scripts/usb-replug.py --self-test
```

The hardware-feature Clippy run checks code that the default build excludes. Neither
Clippy invocation runs hardware tests. The Python self-test uses fake sysfs and reset
backends. `scripts/device-health.py` is different: it queries real hardware.

Most tests use pure state, synthetic sources, recorded sysfs, or pseudo-terminals.
Some capture tests require the untracked bulk JPEG corpus in `fixtures/frames/` and
fail without it. The [corpus manifest](../fixtures/frames/MANIFEST.md) describes the
recording and reproduction tools; a fresh clone does not contain those 120 images.
The smaller `fixtures/frames/absrange/` corpus is committed. Do not interpret a missing
bulk corpus as a codec failure or silently replace real measurements with synthetic data.

UI behavior tests run without a window. Four ignored snapshot tests need a GPU or
software adapter and compare against committed PNGs:

```sh
cargo test --test viewer_chrome_ui -- --ignored
```

Review changes to visible labels and layout against both behavior tests and snapshots.
Accept a new snapshot only after inspecting its image. Decoder timing tests are also
ignored because machine load makes their duration unsuitable as a pass/fail threshold.

## Fixture maintenance

`fixtures/packets/ch9329.toml` holds independent protocol examples. Tests should read
it instead of copying encoder output into expected values. After changing packet
fixtures, run `scripts/check-fixtures.py` to recompute framing and checksums.

Sysfs source consists of `fixtures/sysfs/usb2-desk.sysfs` and `synthesize.py`. Expanded
trees are ignored build output and are generated on demand by discovery tests:

```sh
python3 fixtures/sysfs/synthesize.py
python3 fixtures/sysfs/synthesize.py --force
```

Edit the recording or generator, not generated directories. See the
[sysfs manifest](../fixtures/sysfs/MANIFEST.md) for format, provenance, and synthetic
negative controls. A recorded device path must never be opened on the test host;
use fake capability probes with recorded trees.

## Hardware work

Hardware tests are feature-gated and ignored. Run only the test required by the task,
one binary at a time with `--test-threads=1`. Tests may type, play audio, or reset USB;
read the selected test before starting it. Concurrent viewers and tests can contend
for the same node and invalidate measurements.

For example, after checking the test's expected device identity:

```sh
cargo test --features hardware --test serial_hardware -- --ignored --nocapture --test-threads=1
```

The local setup has additional restrictions:

- Identify the dongle through USB identity, never a remembered `/dev` or card number.
  The recorded identifiers are video `345f:2133`, serial `1a86:55d3`, hub `1a40:0101`.
- Everything on USB bus 5 belongs to the user's unrelated dock. Never open or reset it.
- The target has no network; captured video is its feedback channel. Do not unplug
  HDMI or change the target's resolution. Never send blind mouse clicks.
- Always attempt release-all before exiting a process that sent input. Verify a
  consequence on the target rather than treating an acknowledgement as success.
- Point `XDG_CONFIG_HOME` at a disposable directory for every test that starts the
  viewer, so it cannot alter the user's saved settings.
- Audio tests must verify routing to their test sink by object ID before playback.
  An invalid `PIPEWIRE_NODE` can fall back to the user's default speakers; children
  inherit that variable too. Do not treat a requested route as a verified one.
- USB reset tests require explicit task authorization. `scripts/usb-replug.py` can
  remove and recreate nodes; use `--dry-run` to inspect selection without resetting.

Some tests still expect `/dev/video4` and `/dev/ttyACM1`. Their comments must identify
the hardware intended by those paths. A renumbered setup requires review, not blind
execution. Audio measurements discard the first capture period because the device
can return stale samples from an earlier open. Drift tests take about twelve minutes.

## Development flags

The viewer has hidden flags for repeatable manual measurements:

| Flag | Purpose |
| --- | --- |
| `--exit-after SECS` | Request clean exit after a bounded run. |
| `--chrome-popover NAME` | Open Video, Keyboard, Mouse, or Audio at startup. |
| `--capture-on-start` | Engage input through the ordinary capture path. |
| `--paste-on-capture` | Paste after capture; implies capture on startup. |
| `--paste-cancel-after-ms MS` | Inject the ordinary release action during paste. |
| `--sysfs-root PATH` | Use recorded topology for initial selection with no real capability probe. |

Capture and paste flags send input to the target. The sysfs override does not extend
to the viewer's reconnect resolvers. Use command dry runs for checks that must not
read sysfs or open devices.

## Change guidelines

Keep transport and device I/O outside the protocol and script compilers. Preserve
one serial writer, bounded frame handoffs, cancellation at report boundaries, and
separate release acknowledgement from delivery. Consult [architecture](architecture.md)
before changing those interfaces and [protocol](protocol.md) before changing report bytes.

Comments should explain a constraint, invariant, or failure mode that the code does
not make apparent. API docs should describe behavior, bounds, errors, and ownership.
Keep implementation history in Git; update topic documentation when behavior changes.
Historical hardware observations belong in [hardware measurements](hardware.md), with
conditions and limits that distinguish them from measurements of the current build.
