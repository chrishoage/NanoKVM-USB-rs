# Stage 4 — Findings

Stage 4 is **audio, chrome and paste** (plan §12, rev 5's scope): 4a plays the dongle's USB Audio
Class capture stream, 4b puts an egui pill and its popovers over the video, and 4c pastes the
Wayland clipboard onto the target through `script::compile`. RFB, recording and the jiggler stayed
in §10. This document is the evidence index for what Stage 4 changed, in the same shape as
`STAGE3_FINDINGS.md`: the amendments the plan needs (E1–E20), the module and test inventory, the
hardware run, the reviews, and what is still outstanding.

**Status at the time of writing:** every sub-stage is built, **reviewed by two adversarial
reviewers blind to each other** — three rounds, 4a 7+10 findings, 4b 3+14, 4c 4+7 — fixed, merged
into one tree with its seams wired, and then **verified on hardware by consequence** (§12's rule,
and A17's). The consequences observed, none of them an acknowledgement:

- **the target's 1 kHz tone in the PCM this client captured**: Goertzel ratio **689 857** with the
  tone against **0.018** before it and **1.0** after Ctrl+C — 3.8 × 10⁵ to 9.7 × 10⁵ across six
  runs, 1.03 × 10⁶ in the post-merge sweep, against a `TONE_RATIO` threshold of 100. The silent
  negative controls are the other half of the evidence, and they were only silent after E9 was
  measured;
- **the same tone at the host sink's own monitor**: ratio **2 713 257**, rms 0.283, recorded from
  `nanokvm-test`'s monitor after asserting by PipeWire object id that our stream landed on that
  sink and not on the user's speakers;
- **drift: −13.9 ppm**, measured twice over **720 s** of steady state three hours and several code
  changes apart, agreeing to one period, with **zero xruns in 24 minutes** of streaming;
- **the pasted clipboard, by md5 on the target's screen**: every ASCII printable over two lines,
  `781ea9f294035a509fe843fd497ee94a`, 101 bytes — identical to the local hash of the file that was
  copied; and again after the merge, `9ece22c5608b8e1e3f589c37fffaa7c5`, 53 bytes;
- **the refusal that sent nothing**: with three unreachable characters on the clipboard, the
  screenshots either side of the run are **byte-identical** (`md5 c94258f26fbc616726e8e84c38505ab0`
  both times) and the lock bits are unchanged;
- **the cancel with partial progress and nothing held**: the release key mid-paste stopped it at
  **75 of 4080 keys** — 37 complete characters plus one press whose release the §2.6 release-all
  lifted — and the 38 characters on the target's screen, the unchanged lock bits and the correctly
  cased text typed afterwards are what says so;
- **the chrome's per-frame cost**: `chrome draw p50 8–26 µs` on the render thread (22 µs with audio
  and a paste running alongside), against Stage 1's 6.94 ms frame period at 144 Hz — about **0.3 %
  of a frame** — with `chrome build p50 27–117 µs` on the event loop;
- **present-mode enumeration under wgpu 27**: `surface present modes offered: ["Mailbox", "Fifo"]`
  / `present mode chosen: Fifo`, identical to Stage 1 under wgpu 26, so §5.3 carries across the
  bump rather than being assumed to.

Green at the time of writing, in the main tree:

```
cargo fmt --all --check                                          clean
cargo clippy --all-targets -- -D warnings                        clean
cargo clippy --all-targets --features hardware -- -D warnings    clean
cargo test                        830 passed, 0 failed, 5 ignored, 40 test targets
cargo test --features hardware    831 passed, 0 failed, 22 ignored
cargo test --test viewer_chrome_ui -- --ignored      4 passed (the wgpu snapshots)
python3 scripts/check-fixtures.py   OK: 17 requests, 11 responses, 3 documented anomalies
python3 scripts/usb-replug.py --self-test            OK: 21 cases passed
python3 scripts/device-health.py    OK: device present, paired, and answering
```

and the hardware sweeps, one binary at a time with `fuser` checked free before each:

```
cargo test --features hardware --test audio_hardware   -- --ignored … --skip drift_over_ten_minutes
                                                       5 passed, 0 failed, in 51.42s / 51.75s
cargo test --features hardware --test serial_hardware  -- --ignored …   1 passed, 0 failed
cargo test --features hardware --test capture_hardware -- --ignored …   3 passed, 0 failed
```

**Tests: 587 (Stage 3 exit) → 830**, 5 ignored, across 40 test targets. The merged baseline is
823 = 663 (4a) + 747 (4b+4c) − 587, and the seven above it are the tests written for the merge
seams. The 5 `#[ignore]`d non-hardware tests are 4b's four wgpu snapshot tests and Stage 1's
`capture::decoder::tests::decode_timing_spot_check`; the 17 hardware-only ignored tests take the
count to 22 under `--features hardware`.

`ldd target/debug/nanokvm` is still exactly the four §7.2 permits — `libasound.so.2`, `libc`,
`libgcc_s`, `libm` — plus the loader and the vdso. wgpu 27, four egui crates and `wl-clipboard-rs`
added **nothing** to the link surface.

## What was built

| Module | Stage 4 change |
| --- | --- |
| `audio/ring.rs` (new, pure) | `RingConfig` (periods × period_frames, `depth`, `silence`), `PeriodRing` (`push`/`pop` → `PushOutcome`/`PopOutcome`), `DriftPolicy` (`on_push`/`on_pop` → `DriftAction`, E6), `RingCounts` (pushed/popped/overruns/underruns/drift_drops/drift_inserts), and the measured constants `SAMPLE_RATE = 48_000`, `CHANNELS = 2`. No ALSA type appears in the file. `RingConfig::default` is 8 × 480 frames = 80 ms, and its doc now carries the drift measurement that argues for it (E7). |
| `audio/pcm.rs` (new) | The seam: `PcmSource`, `PcmSink`, `PcmSourceOpener`, `PcmSinkOpener`, and the fakes the whole audio suite runs on — `ScriptedSource`, `RecordingSink`, `DeviceBufferedSink`, `PacedSource`, and the `Wedged*` pair (a source, a sink and their openers) that park for ever, which is what E10's invisible failure needed. |
| `audio/alsa.rs` (new) | The only file that names ALSA. `AlsaCapture`, `AlsaPlayback`, `open_capture`, `DefaultSinkOpener` (`default`, or `named()` for a test sink), `classify` (errno → condition), and `configure`/`HwPlan::for_side`: S16_LE / 2 ch / 48 kHz, read back and failed on a mismatch on the **capture** side only, `set_rate_resample(false)` on the capture side only — `default` may be a `plug`. Both PCMs are opened non-blocking and driven through `pcm.wait()` so a stop flag is always seen. |
| `audio/tone.rs` (new, pure) | Goertzel: `power_at`, `find_tone` → `ToneReport { power, reference, ratio, rms }`, `mono`, `channel_of`, `TONE_RATIO = 100.0`. Unit-tested against synthetic sines, including a quiet tone under noise, so a hardware failure is about the audio path and not about the DSP. |
| `audio/mod.rs` (new) | `AudioError` (`NoCard`/`Busy`/`Gone`/`Open`/`Io`, each naming the device and why), `AudioSide`, `AudioCondition`, `AudioStats` (atomic counters, one condition slot **per side**, the `opening` supervision of E10), `AudioSnapshot`, `AudioConfig` (`prefill_periods`, `reopen_backoff_cap`, `stop_deadline`), `reopen_delay`, and `AudioHandle::spawn/stop/snapshot/mute_flag/set_muted` driving the two threads `nanokvm-audio-capture` and `nanokvm-audio-play`. |
| `discovery/mod.rs` | The sound class: `SoundCard { sysfs, name, id, usb }` with `number()` parsed from the `card<N>` directory name **on every call** (C13), `alsa_device()` → `hw:<N>`, `alsa_card_id()` → `hw:CARD=<id>` (printed, never opened); `AudioPairing { Paired, NoCard, Ambiguous, Unknown }`; `audio_for` — §8 evidence 1 and nothing weaker (E8); `collect_sound_cards`; a `sound cards:` section in the listing and an `audio:` line under every pair. |
| `discovery/reopen.rs` | `CardResolver` → `ResolvedCard`, built from the capture node's **USB device directory** with a `Sysfs` and **no probe at all** (E8), and `DiscoveringCardOpener`, whose failures are `AudioError` and never `DiscoveryError` — a discovery failure must reach the audio thread as "no card, retry" and nothing that could stop the client. Plus `DiscoveringOpener::set_format`, which also drops the already-open source so a format change is not silently ignored on the first open. |
| `viewer/chrome/route.rs` (new, pure) | The routing rule: `Sink { Target, Chrome, Dropped }`, `PointerKind`, `RouteInputs`, `Outstanding`, `route_key`, `route_pointer`, `resolve_paste_chord`. It never reads `EventResponse::consumed` (E3) and it is the module the whole safety argument runs through. |
| `viewer/chrome/hit.rs` (new, pure) | `HitAreas`: the pill/popover rectangle in egui points plus `pixels_per_point`, hit-tested against winit's physical pixels; a non-finite or non-positive scale falls back to 1.0, because it is a divisor. Tested at 1.0 and 1.5. |
| `viewer/chrome/frame.rs` (new) | `ChromeFrame` (primitives + `TexturesDelta` + ppp + size), `absorb` (deltas accumulate with `TexturesDelta::append`, pictures replace), and an mpsc `channel()`/`ChromeReceiver::drain()`. Deliberately **not** the drop-oldest `Slot`: a dropped delta loses font-atlas `set` entries for ever. |
| `viewer/chrome/config.rs` (new) | `$XDG_CONFIG_HOME/nanokvm/config.toml` (falling back to `$HOME/.config`), `Store::at(root)` injectable for tests, `Config` with `#[serde(deny_unknown_fields, default)]` (E18), `MouseMode`, `WheelDirection`, `PasteLayout`, `ConfigError`, and the free `persist(store, config, saved)` that owns the write-on-change rule and advances `saved` only on a successful save. `Store::save` is tmp-plus-rename. |
| `viewer/chrome/audio.rs` (new) | The 4a↔4b seam: `ChromeAudio`, `AudioState { On, Muted, Off, Opening { side }, Absent { reason } }`, the counters, and `ChromeAudio::from_snapshot(&AudioSnapshot, RingConfig, Arc<AtomicBool>)` — the one place a chrome audio state is constructed, so it is testable against a real `AudioHandle`. |
| `viewer/chrome/shortcut.rs` (new) | `Builtin { WinTab, CtrlAltDel }` and `transitions`: `script::compile_key`, then a pure report-differencing pass into `input::Event::Key` transitions, releases before presses and modifiers outside usages. Shared with `paste` — one mapper, one rule about what a transition is. |
| `viewer/chrome/ui.rs` (new) | The pill, the grip, the four popovers (Video, Keyboard, Mouse, Audio), `ChromeCommand`, `ChromeFacts`, `ChromeOutput`, `PasteFacts`, `PasteAvailability { Ready, NotCaptured, Unsupported { reason }, Running }`. Every control has a tooltip and every disabled control's tooltip says why. The widgets only *ask*; the event loop's `perform_chrome` is authoritative. |
| `viewer/chrome/clipboard.rs` (new) | `Availability::probe()` (one `get_mime_types` roundtrip at startup — types, not content), `Fetch::spawn()/poll()` on a short-lived `nanokvm-clipboard` thread behind a one-at-a-time `FetchGate`, with a `READ_DEADLINE` and the 16 MiB bound of E16. `MimeType::Text` is `wl-clipboard-rs`'s own preference order and is exactly §12 Stage 4c's. |
| `viewer/chrome/paste.rs` (new, pure) | `normalise` (CRLF/CR → LF and nothing else), `compile` (twice, under `CapsLock::Off` and `Compensate`, refusing only if the two differ — E13), `Refusal` + `offenders()` + `log_line()`, `PasteJob`/`PasteOutcome`/`PasteProgress`/`Ending`/`ending_for`, `PACE`, `PREPARE_TIMEOUT`, `chord()`. |
| `capture/` | `SourceOpener::set_format(w, h, fps) -> bool` (default `false`), `V4l2Opener::set_format`, `enumerate_modes(path)` — `VIDIOC_ENUM_FRAMESIZES` on a second read-only handle — and in `pipeline.rs` a one-slot `format_request`, `PipelineHandle::request_format`, `Capture::apply_format_request` once per capture-loop pass, `PipelineStats::format_changes`, and `format_refused` as a function so the warning reads as a sentence. |
| `input/` | The on-demand `GET_INFO` refresh E13 needed: `Shared::request_info_refresh` / `LinkState::info_generation` / `info_stale`, `Writer::refresh_device_info()` serviced **between frames** (and breaking both the idle park and the reconnect wait), `Stats::device_info_generation` / `device_info_stale`, and `Producer::refresh_device_info() -> u64` (`#[must_use]`), which returns the generation the request was made against. |
| `script/mod.rs` | `pub const REPORT_DELAY_MS: u64 = 40` — the single authority for the pacing, read by `cli/keys.rs`'s three `--delay-ms` defaults and by `chrome::paste::PACE` (E14). |
| `viewer/input_map.rs` | `map_key` takes `ModifiersState`; `KeyAction::Paste` for `Shift+Pause`. Bare `Pause` is still `Release`, both key-ups are still `Swallowed`, and a held chord still does not re-trigger. |
| `viewer/render.rs` | `Gpu` gains an `egui_wgpu::Renderer`; `draw` takes `Option<&mut ChromeFrame>` and draws egui **after** the video quad in the same pass through `forget_lifetime()` (egui sets its own viewport but resets only the scissor, so the other order letterboxes the chrome); `chrome_fits_surface` skips — without touching its deltas — a frame laid out for another surface; `RenderStats` gains `egui_p50_us`/`egui_max_us`; and `release_gpu` is the unconditional `mem::forget` of E11. |
| `viewer/app.rs` | The largest single file change of the stage: the chrome's build/perform/persist cycle on the event loop, `route_inputs`, `give_to_egui`, `audio()`, `PasteState` and its twelve methods, the `Outstanding` bookkeeping, `cursor_should_hide`, `format_change_needed`, `key_also_goes_to_egui`, the `CHROME_MIN_REPAINT` floor (E4), and a first `#[cfg(test)] mod tests` in a file that had none. |
| `viewer/title.rs` | `AudioTitle { Off, On { muted, counts, ring }, Opening(side), Unavailable(reason) }` and `audio_fragment`. The counters appear only once one is nonzero; the buffer depth is stated as *configured*, never as latency. Everything §2.8 and §6.1 require is still in the title, so the title remains the fallback for a chrome that is closed. |
| `main.rs` | `--no-audio`; the hidden dev flags `--chrome-popover <Video\|Keyboard\|Mouse\|Audio>`, `--capture-on-start`, `--paste-on-capture` (implies the former) and `--paste-cancel-after-ms <MS>`; the chrome config load **before** the window and before audio, so a parse error cannot return after two audio threads exist; the device's mode enumeration; `Selection::video_usb` for the card resolver; audio started last and `drop(audio)` on the shutdown path. All five flags are refused alongside a subcommand by `viewer_only_flag()`, like every other viewer-only flag. |
| `fixtures/sysfs/` | The **2026-09-11 sound addendum**: 20 records merged into `usb2-desk.sysfs` additively (24 insertions, 0 deletions, 4 of them the provenance header), covering `card{0,1,2,3,8}` and their `device`/`id`/`number`. `snapshot-sysfs.py` gained the sound class and now captures the `device` link's target directory; `synthesize.py` reconstructs the webcam's card in the bus-5 negative control and gives `two-dongles` both a non-dongle USB card under the dongle's own hub and the second dongle's own renumbered card. `device-health.py` gained `check_audio`, which reports and never fails the run. |
| Tests | New binaries: `tests/audio_path.rs` (15), `tests/audio_ring.rs` (8 proptests), `tests/audio_isolation.rs` (3, structural — the `tests/cli_keys.rs` pattern), `tests/audio_hardware.rs` (6, all `#[ignore]` behind `--features hardware`), `tests/viewer_chrome.rs` (16 proptests), `tests/viewer_chrome_ui.rs` (29 `egui_kittest`, 4 of them the `#[ignore]`d wgpu snapshots against `tests/snapshots/*.png`), `tests/capture_format.rs` (5), `tests/input_device_info.rs` (7). Extended: `tests/discovery.rs`, `tests/viewer_title.rs`, `tests/cli_shape.rs`, `tests/viewer_state.rs`. |

## Amendments the plan needs

### E1. §12 Stage 4b's version arithmetic is right, and `resolver = "3"` is the part it does not say

All three of the plan's clauses check out, one of them understated: `egui-wgpu` 0.32 wants
`wgpu ^25` and 0.33 wants `^27.0.1` — **wgpu 26 was skipped entirely**, so there is no "older egui
on 26" to fall back to and the bump is forced; `egui`/`egui-wgpu`/`egui-winit` 0.33.3 and
`wgpu` 27.0.1 all declare `rust-version: 1.88`; and egui 0.34 and 0.35 need 1.92 while **0.36 needs
1.95**, against this desk's 1.89.

What the plan could not have known is that the bump **breaks the build the moment the lock is
regenerated**. `wgpu-hal` 27.0.4 deliberately loosened its `ordered-float` constraint (*"Remove
fragile dependency constraint on `ordered-float` that prevented semver-compatible changes above
5.0.0"*), so the default resolver walks to `ordered-float 5.5.0`, whose `rust-version` is **1.90**.
The fix is declarative: `resolver = "3"` in `[package]`, which makes cargo read `rust-version` and
print `Locking 417 packages to latest Rust 1.89 compatible versions` / `Adding ordered-float v5.4.0
(available: v5.5.0, requires Rust 1.90)`. The lock carries 5.4.0 today, and the comment in
`Cargo.toml` says why so a future `cargo update` does not quietly undo it. Preferred to a
`--precise` pin nothing explains.

Two feature decisions are load-bearing rather than tidy, and §7.2 should record both:
`egui-winit`'s default `clipboard` feature pulls `arboard` **and** smithay-clipboard → SCTK 0.20 →
calloop 0.14 — a second `wayland-backend` build, which `viewer::wayland`'s own docs say makes
winit's foreign `wl_surface` proxy unrecognisable and silently kills shortcut inhibition (§1.5).
With `default-features = false, features = ["wayland"]` the lock has exactly **one**
`wayland-backend` (0.3.17, verified) and `cargo tree -d` reports no duplicate wayland/calloop/
smithay/x11 crate. And `egui_kittest` turns on `egui/accesskit`, with which `egui-winit` 0.33.3
fails to compile unless its own `accesskit` feature is on (upstream `E0027`); declaring that feature
in **`[dev-dependencies]`** is what keeps `accesskit_winit` and the AT-SPI/zbus tree out of the
shipped binary, which `ldd` proves.

### E2. §5.3 re-verified under wgpu 27, and the error handler that must stay uninstalled

The plan asks for the present-mode findings to be re-checked rather than assumed. Measured, four
separate runs, identical every time, and again in the integrated run after the merge:

```
[INFO  nanokvm::viewer::render] surface present modes offered: ["Mailbox", "Fifo"]
[INFO  nanokvm::viewer::render] present mode chosen: Fifo
```

`Immediate` is still absent on Wayland and niri still offers exactly `[Mailbox, Fifo]`. Nothing in
`render.rs`, `app.rs`, `wayland.rs` or `main.rs` needed a wgpu-27 change: all 587 Stage 3 tests
passed after the bump and before a line of 4b was written, and `serial_hardware` and
`capture_hardware` still pass on the desk, so **the bump did not break the Stage 1/2 paths**.

One new line appears and is expected: `egui_wgpu::renderer` warns that the framebuffer is
`Bgra8UnormSrgb` rather than its preferred `Rgba8Unorm`. `Renderer::new` selects
`fs_main_gamma_framebuffer` from `format.is_srgb()`, so the colours are right, and the surface
format is chosen by `render.rs` for the *video*. **Do not install `on_uncaptured_error` to quiet
it**: §5.3 records that an installed handler turns `Surface::configure`'s validation failure into a
silently unconfigured surface and moves the panic to the next `get_current_texture()`, and wgpu 27
changed that callback to `Arc` + `Sync`, which makes it easier to reach for and no less wrong.

### E3. `egui-winit` consumes Tab unconditionally, so the routing rule lives at the source

§12 Stage 4b says "the chrome never steals a key the target was going to get". The obvious
implementation — route on `EventResponse::consumed` — **cannot** satisfy it:

```rust
// egui-winit-0.33.3/src/lib.rs:415-418
// When pressing the Tab key, egui focuses the first focusable element, hence Tab always consumes.
let consumed = self.egui_ctx.wants_keyboard_input()
    || event.logical_key == winit::keyboard::Key::Named(winit::keyboard::NamedKey::Tab);
```

Tab, on a KVM, to a shell. Two more of the same shape: `wants_keyboard_input()` reflects the
*previous* frame's UI, so it is a lagging predicate; and `CursorMoved` is gated on
`is_using_pointer()` rather than `wants_pointer_input()`, so hovering the pill reports
`consumed: false`.

So the rule is ours, pure, in `chrome/route.rs`, and `EventResponse` is discarded everywhere.
`route_key`, in order: a `Release` goes to the target's release path **from every state**; the up
edge of anything in `Outstanding` goes to the target from every state; `paste_running` drops;
relative-mode capture with a **confirmed** grab makes the chrome unreachable; an open popover or
modal takes keys; otherwise the target, **including Tab**. Three deviations from the design are
recorded in the module's own docs:

- **`paste_running` moved ahead of the relative-capture clause.** The design's order forwards a
  host keystroke into the middle of a relative-mode paste, which §12 Stage 4c forbids, and the two
  states overlap in practice.
- **`pointer_locked` is required for "unreachable"**, fed from `self.grab.is_some()`. On a
  compositor that refuses both `Locked` and `Confined`, the pill stays reachable and clickable
  instead of having its clicks sent to the target as real button presses.
- **`Outstanding` exists at all.** Both reviewers found the same hole from different directions: a
  key whose press was forwarded and whose release arrived after a popover opened was routed to the
  chrome, and the target kept holding it. The set is maintained in `App::perform` — `Forward` sets a
  bit, `ReleaseAll` clears them — and is proptested over all 192 flag states in both directions.

`CursorMoved` and every key **release** are additionally given to egui whatever the routing says,
or egui's hover highlight and `keys_down` go stale; only the *rebuild* is gated, so motion over the
video costs one `on_window_event` and no tessellation.

### E4. The UI is built and tessellated on the event loop, not the render thread — measured

The brief's "egui-wgpu (tessellation + draw, on the render thread)" is not available as written:
tessellation is `egui::Context::tessellate`, which needs the `Context`. The split as built is the
event loop owning `egui::Context`, `egui_winit::State` and the chrome state (input → `ctx.run` →
`tessellate` → `handle_platform_output`), and the render thread owning only
`egui_wgpu::Renderer` (`update_texture` / `update_buffers` / `render` / `free_texture`). Only plain
data crosses.

The reason is the routing predicate: it must be answerable **synchronously, on the event-loop
thread, while deciding where the current key goes**. On the render thread it would lag a frame and
couple input routing to render timing, which §5.4 forbids. The cost is therefore the thing to
measure, and it is: **`chrome build p50 27–117 µs, p95 38–148 µs, max 156 µs`**, two orders of
magnitude below the 6.94 ms frame period and three below the 250 ms tick. §5.4's demand that
handling latency not correlate with rendering stands.

Two mechanics the plan does not have:

- **A chrome frame must set `passes.pending`**, or an egui animation freezes whenever the video
  stops, because a closed capture slot sleeps `FRAME_WAIT`.
- **`CHROME_MIN_REPAINT = 16 ms` floors egui's `repaint_delay`.** Folding `repaint_delay` into
  `about_to_wait`'s `WaitUntil` literally *spins*: egui asks for `Duration::ZERO` while anything
  animates, and the first instrumented run reported **4096 builds in 3 s** — the stats ring's whole
  capacity. After the floor, 12–24 builds per 3 s, which is the 250 ms tick. A chrome frame more
  often than the display refreshes is discarded by the render thread anyway. The constant's doc
  carries the measurement.

### E5. The chrome's per-frame cost, next to the Stage 1 number — and which `present` figure this is

§12 Stage 4b asks for the render thread's per-frame cost with the chrome open, stated next to the
Stage 1 number. `chrome draw` is a sub-timer around exactly `update_texture` + `update_buffers` +
`Renderer::render`:

| run | `chrome draw` p50 / max (render thread) | `chrome build` p50 / p95 (event loop) |
| --- | --- | --- |
| collapsed (`menu_open = false`) | 8–20 / 10–24 µs | 27–97 / 38–118 µs |
| expanded (`menu_open = true`) | 11–20 / 12–26 µs | 57–116 / 69–139 µs |
| Video popover open (`--chrome-popover Video`) | 10–16 / 11–19 µs | 37–96 / 47–156 µs |
| integrated: chrome + audio + a running paste | 22 / 33 µs | 117 / 148 µs |

Stage 1's frame period at 144 Hz is 6.94 ms, so **20 µs is about 0.3 % of a frame**, and capture is
untouched: `60.0 fps`, `dropped pre-decode 0`, `decode errors 0`, `capture errors 0`, chrome open or
closed, which is Stage 1's operating point exactly.

**The `present` number in every Stage 4 run is the locked-session one** — `present p50 999.3–999.6
ms, 3–5 presented` — because the desk was locked (`LockedHint=yes`) for the whole session, so niri
schedules no frame callbacks. That agrees with Stage 1's own recorded locked figure (1000.4 ms), so
the instrument agrees with itself across the wgpu bump, but **Stage 1's unlocked `present p50
0.0 ms` was not re-taken and is not claimed**. The same applies to `capture-to-submit age p50
25–28 ms` against Stage 1's 18.1 ms: that is the locked session, not the chrome — the render thread
takes one frame a second, so every frame it takes has been sitting in the slot.

`event-loop handling p50 0 p99 0–1 max 1 µs` also appears, and it is **not** evidence: the sample
count is 0–4, because nothing may drive the pointer on the user's desktop. Stage 1's real numbers
(p50 0 µs, p99 ≤ 4 µs, max 9 µs over 4096 samples) were taken by the user moving the mouse for
8.5 minutes, and re-taking them needs the same.

### E6. The drift policy needs two counters, and the prefill must be deeper than the device buffer

Two things §12 Stage 4a's one-paragraph drift policy does not contain, both of which cost a failing
test or a hardware run to find:

- **`on_push` and `on_pop`, not one `observe(level)`.** A single entry point swallows corrections:
  a low-side streak can complete inside a `push`, where the `InsertOne` is discarded, and the streak
  resets having done nothing. The two directions now count separately, pinned by a unit test and a
  proptest.
- **`prefill_periods = DEVICE_PERIODS + 2`, derived from the device buffer rather than from the
  ring.** The playback side primes the ring and then starts writing, and **the first
  `device_periods` writes do not block** — the card's buffer is empty, so the prefill is transferred
  straight out of the ring. With `prefill == device_periods == 4` the ring is drained to zero *by
  its own prefill*, sits on the low-water mark, and the policy inserts silence for a clock
  difference that has not happened. Measured against the real card, 20 s per configuration:

| `device_periods` | `prefill_periods` | drift inserts | silent periods | ring level |
| --- | --- | --- | --- | --- |
| 4 | 4 | **2** | **2** | 0–2 |
| 4 | 6 | 0 | 0 | 1–2 |
| 2 | 4 | 0 | 0 | 1–2 |

Two periods of injected silence is inaudible; **polluting the counters §12 asks to be read as a
drift rate is not** — at face value those two inserts are about 10 000 ppm against a real 14.
The invariant is stated as a relation in a unit test, and a `DeviceBufferedSink` fake that behaves
like a sound card reproduces both halves.

A third correction belongs here: **a correction is what happens *instead of* an xrun.** `push` takes
a `DropOne` only when the queue is neither empty nor full, `pop` takes an `InsertOne` only when the
queue is non-empty, and an overrun or underrun calls `DriftPolicy::reset()` — otherwise an outage
shows up as drift, and a stall leaves a streak armed.

### E7. The ring depth is not sized by drift; it is sized by scheduling, and now says so

§12 Stage 4a requires the ring depth to be chosen from the measured drift rate. Measured, and the
answer is that **drift does not size this ring**:

- drift is ≤ 14 ppm, so the time for drift alone to move the level by one period is
  `480 / (48 000 × 14 × 10⁻⁶) = 714 s` — twelve minutes;
- the operating level is `prefill_periods − device_periods = 2` periods, held at 1–2 for the whole
  twelve minutes, leaving five periods of headroom above and one below before a correction;
- so a ring sized for drift alone could be **three** periods deep.

What sizes it is the longest gap either thread can go without being run. **8 × 480 frames (80 ms)
stays, now for a stated reason**: 24 minutes of xrun-free streaming on a desk simultaneously
decoding 1080p60 and playing the user's own audio. Four is probably enough and has no evidence;
sixteen doubles a latency nothing asked to be doubled. `RingConfig::default`'s doc carries the date,
the duration, the counters and the ppm.

The latency claim stays as §5.5 requires: the **configured** depth (periods × period size) with the
word "configured" in the title string itself. No end-to-end figure is claimed anywhere.

### E8. §8 extended: a card pairs by same-device containment, resolved without a probe

§8 rev 3 wished for same-device evidence and the serial node could not give it. The sound card can:
`/sys/class/sound/cardN/device` resolves to an interface of the *same* USB device as the capture
node's, so `audio_for` reuses `same_device()` — the predicate `Evidence::SameDevice` already uses —
and nothing weaker. The containment rule video-and-serial falls back to on this USB 2.0 link would
be actively **wrong** here: anything else on the dongle's internal hub is contained by that hub too,
which is why `fixtures/sysfs/two-dongles` now carries an ordinary USB sound card on a free port of
the second dongle's own hub as the negative control.

The four outcomes are all non-errors (§4.1 rev 5 makes audio a side channel): `Paired`, `NoCard`,
`Ambiguous` (§8's "never silently pick one" — both are named and nothing is selected) and `Unknown`
("not asked", when `--video` named a node discovery never enumerated).

**And the resolver runs no discovery.** `CardResolver` is given the USB device directory the
viewer's pair already resolved to and pairs against *that*: one read of `/sys/class/sound`, one read
of the device's attributes, `audio_for`. The first version re-ran `discover` inside the retry loop,
which meant a `VIDIOC_QUERYCAP` on every `/dev/video*` on the machine **every 500 ms for ever**
whenever no card paired — a background task opening video nodes twice a second, and a periodic
entrant into the replug race. The test that pins it is stronger than a panicking probe: a `Sysfs`
that panics if `/class/video4linux` or `/class/tty` is listed at all.

**The cost, stated in the doc comment rather than left to be discovered: a dongle replugged into a
*different* port gets a different sysfs path, and audio then reports no card until the client is
restarted**, while video and input follow it because they re-run discovery. A replug into the same
port — what `usb-replug.py` and a device reset do — keeps the path and renumbers the card, which is
the C13 case this resolver re-resolves for. `hw:<N>` is parsed out of the `card<N>` directory name
at open time and never remembered; `hw:CARD=<id>` is printed and never opened, because two identical
dongles give two cards called `Video`.

### E9. The dongle hands over ~2 ms of stale audio on every capture open — A6's audio analogue

A capture taken **eight seconds** after the target's tone had been stopped was not silent: all of
the energy sat in the first half-second window, and splitting further put it inside the **first
10 ms period** — about 2 ms of the old tone at full level. It is a fixed quantity rather than a
decaying tail (captures 4 s and 8 s after Ctrl+C returned power 0.003929 and 0.003887, a 1 %
difference), and a 40-second `pw-record` started immediately after Ctrl+C is at the noise floor in
every window, so the *target* goes quiet within about a second and what is left is the card's own
buffer being handed over on the next open.

It nearly invalidated the negative control: one sweep's "nothing is playing" capture came back at
**ratio 85 against a threshold of 100** — a control that was measuring the previous test. The fix is
in the test, not the client: `STALE_PERIODS = 1`, reported in the transcript of every run and
discarded. The negative controls moved from ratio 85–97 to **1.0**.

**Deliberately not fixed in the client.** Two milliseconds on each open is inaudible, and discarding
a period would put a device-specific rule into `src/audio/alsa.rs` for no benefit a listener could
hear. It is recorded so the next person to measure anything through this card knows to throw the
first period away. A7's rule generalises: this device hands over its previous state at the start of
a freshly opened stream, on video *and* on audio.

### E10. §2.8 has a hole: an open that never returns is invisible. The `Opening` state closes it

Observed once on hardware (4a's D2): the playback side **never opened, never failed and never
logged anything**, stuck inside `snd_pcm_open` — before `configure`, which logs the granted sizes,
and before any error path, which would have raised a condition. `stop()` behaved exactly as
designed, waited its 250 ms, warned, detached and returned. Deliberately hunted afterwards and
**not reproduced**: 146 opens across four reproduction attempts, all normal, in about 6 ms.

The defect it exposes is a surfacing one, and it is the plan's: **"has not started yet" and
"working" were the same title** — `playback_opens == 0` the only clue, no condition, and
`audio on (buffer …)` on screen. As fixed after the merge:

- `AudioStats` gained `opening: [Option<Instant>; 2]`, set immediately **before** `opener.open()`
  and cleared on the first period that moves (also on an open error or device error, where the side
  has a condition of its own and a second would be noise);
- `AudioStats::snapshot()` supervises it — **on the reader**, because the only thread that knows an
  open is outstanding is the one blocked inside it — and past `AudioConfig::reopen_backoff_cap`
  raises `AudioError::Open { why: "the open has not returned in 10s. …" }`. The message names the
  **limit**, never the elapsed time, so `report`'s dedup logs it exactly once;
- it renders as `— audio opening (capture)…` in the title and `audio: opening (playback)…` in the
  popover; `is_running()` is false for it, so Mute is disabled and says why.

The fake it needed did not exist — every existing fake either succeeds or returns `Err`, and the
invisible failure is the one that does neither — so `pcm::WedgedSourceOpener`/`WedgedSinkOpener`
were written beside the wedged source and sink. Seen live in the integrated run:

```
title: … — waiting for the first frame — audio opening (capture)…
title: … — audio on (buffer 8x480 frames configured)
```

and it raised **no** spurious condition anywhere in the post-merge hardware sweep.

### E11. The render thread must never destroy its GPU objects, and the obvious fix fails

Found by the 4a hardware agent and fixed in the merge. **It is Stage 3's defect, not audio's.**

On a locked session the compositor does not schedule the surface, `present()` blocks, the render
thread overruns the 500 ms join deadline and `App::teardown` **detaches** it (Stage 1/3 behaviour).
Main then finishes shutdown and closes the winit/Wayland connection; the detached thread later wakes
and drops the wgpu GLES/EGL instance, which marshals Wayland requests on a connection that is gone:

```
TID: … (nanokvm-render)   Signal: 11 (SEGV) si_code: SEGV_MAPERR
#2  wl_proxy_marshal_array_flags (libwayland-client.so.0)   #4 libEGL_mesa.so.0
#8  <wgpu_hal::gles::egl::Inner as Drop>::drop   #11 drop_in_place<wgpu_core::instance::Instance>
#14 nanokvm::viewer::render::render_loop
```

Frequency, measured: on a locked session **four of six** runs took the detach path, and the crash is
a race inside it — one occurrence in seven runs. §2.6 is not violated in the part that matters: the
release-all is submitted *before* the crash, so the target is never left holding a key.

**The first fix was wrong, and the desk said so.** An `abandoned` flag set by main before returning,
read by the render thread after `present` returns, choosing between `drop` and `mem::forget`, was
implemented exactly — and the integrated run segfaulted anyway, with frame #16 in the new function
on the `drop` branch: the flag read as unset. **The deadline does not land before the teardown; it
lands inside it.** The thread left `present()` at ~450 ms, read the flag (still unset) and began
destroying; `finished` is raised only *after* the teardown, so the event loop's 500 ms expired
mid-destructor and closed the connection out from under it. No flag read before the teardown can
close that window, because the window *is* the teardown, and a handshake would be the unbounded join
the detach exists to avoid.

**The shipped rule: `release_gpu` is an unconditional `mem::forget`.** `render_loop` only ever
returns while the process is on its way out — either the event loop set `stop`, or a fatal was
recorded, which `App::tick` turns into a `CloseRequested` — so there is no path on which the render
thread ends and the process carries on needing a GPU. The kernel reclaims the device, the surface
and the textures a moment later as it does every other allocation held at `exit`. Side benefit: the
thread's exit after `present` returns is now instant, which makes the bounded join *less* likely to
expire at all. Pinned by `render::tests::the_render_threads_gpu_objects_are_never_destructed`, a
stand-in that counts its own drops, so a future tidy-up fails there rather than on someone's locked
desk once in seven runs. Verified by **16 runs, all exit 0, `coredumpctl` empty**, 7 of them taking
the detach path, plus two 30-second integrated runs — one of them the exact workload that crashed
under the first fix.

### E12. D1 is amended: the chrome's built-in shortcuts go through the viewer's producer

D1 (Stage 3) established that a *script* transacts one report at a time over its own `SerialLink`
and fails rather than recovers. The chrome's `Win+Tab` and `Ctrl+Alt+Del` do the opposite — ordinary
`Event::Key` transitions through the viewer's existing `input::Producer` and capture state machine —
and D1's reasoning inverts on all three points at the window:

1. **D1's path is not available.** The viewer's writer thread owns the serial link for the life of
   the window. There is no second `Link`, and opening the node twice would interleave two writers'
   frames on a chip with no inter-byte timeout (§5.1) — a guaranteed "a truncated serial write
   corrupts the next command".
2. **D1's first reason inverts.** A script stops and says how far it got *because nobody is
   watching*. At the window the user is present, and §2.7's answer — discard the queue, release
   everything, resume disengaged, say so in the title — is right for a `Ctrl+Alt+Del` whose link
   died halfway, and is what happens to every other key the user presses.
3. **D1's second reason is vacuous.** Coalescing exists for pointer motion (§5.1, C8); these are key
   events, which §2.2 makes barriers.

Nothing is left held: `compile_key`'s `tap` is a press followed by `RELEASE_ALL`, and `transitions`
differences consecutive reports, so the trailing release-all becomes an explicit key-up for every
modifier and usage the press put down, in the order usages-up, modifiers-up, modifiers-down,
usages-down. `every_builtin_ends_with_nothing_held` replays the list through a held-set and asserts
it empties and that nothing is released while not held. The items are **disabled while not
captured**, because `Producer::submit` refuses then (§2.6), and the disabled tooltip says so.

So §2.9 as amended by D1 needs a third sentence: *the viewer's own chrome is on the viewer's side of
that split, not the script's.*

### E13. Paste is refused under CapsLock on a reading taken *now* — a new `input` capability

The chrome takes D2's **`refuse`** policy, `type`'s own default, and offers no override. Three
reasons, in `paste.rs`'s docs:

1. **A paste has no length cap and therefore no bounded duration.** `type` delivers a short argument
   in a second or two, so a lock state read a moment ago is still true at the last character. A
   2 000-character paste runs for **163 s** (measured). `compensate` is one decision applied to
   every letter; anything that toggles CapsLock during those minutes turns the fix into the bug,
   silently, for the remainder.
2. **`compensate` types something other than what was copied** — it sends the opposite shift bit and
   relies on the target to invert it. That is the approximation §10.2 rules out everywhere else,
   accepted for `type` only because a human typed the flag for that one invocation. Nobody types a
   flag for a menu item.
3. **One rule**: the refusal reuses D2's exact wording and remedies.

Two things make "the wrong case can never be typed silently" structural rather than a claim. The
question is asked of the **compiler**: `compile` compiles the text twice, under `CapsLock::Off` and
`CapsLock::Compensate`, and refuses only when CapsLock is on *and the two differ* — so a clipboard
of pure punctuation pastes with CapsLock on, correctly. And **the reading is fresh**:
`Stats::device_info` alone is whatever the link reported when it was *commissioned* — on a stable
link, the value from process start — so deciding whether a page of letters arrives inverted from a
minutes-old reading is A17's mistake in another costume, and it would have made the viewer's paste
strictly *less* safe than `nanokvm type`, which opens its own link and gets a fresh `GET_INFO` every
time.

Hence the one part of Stage 4 that reaches outside the viewer: `Producer::refresh_device_info()`,
serviced by the **writer**, between frames, because the writer is the sole serialization point
(§2.6) and a second thread transacting would interleave frames on the chip. Three properties the
plan should carry:

- it **returns the generation the request was made against**, read inside the same critical section
  that sets the flag, so a refresh serviced before the caller looks still reads as a change;
- the flag breaks both the idle park and the reconnect backoff wait, or an idle or disconnected
  writer would never see it;
- a refresh that could not be served still **advances the generation** — so a caller is never
  wedged — and sets `device_info_stale`, which the viewer maps to *unknown*, and an unknown lock
  state is refused exactly like a known-on one. A failed refresh is deliberately **not** escalated
  to `LinkDown`: it warns once and leaves the session engaged, and a link that is really gone is
  still found by the write path and the idle health poll.

Measured: with three unreachable characters the refusal named `'é' (U+00E9) at position 4`,
`'ï' (U+00EF) at 8`, `'→' (U+2192) at 12` in the popover, the lock bits were unchanged, and the
screenshots either side are byte-identical.

### E14. The pacing is per key *transition*, and a shifted character costs four of them

`script::REPORT_DELAY_MS = 40` is now the single authority, read by `cli/keys.rs`'s three
`--delay-ms` defaults and by `chrome::paste::PACE`, so "the paste runs at the rate `type` runs at"
is a fact the compiler keeps rather than two literals that agree today. Its justification is
unchanged and is the target's, not the chip's: a keyboard ack round trip measured 4.16–4.19 ms
(B-series, A11), so the chip could take reports ten times faster; desktops drop keys delivered
faster than a human types them.

**The unit is the difference, not the character, and the plan should say so.** `compile_type` emits
a `tap` per character — one press report and one `RELEASE_ALL` — and `shortcut::transitions`
differences consecutive reports, each difference being one barrier and therefore one report. So an
unshifted character is 2 transitions (80 ms) and a **shifted one is 4** (Shift down, key down, key
up, Shift up — 160 ms). That is **twice `type`'s cost per shifted character**, because `type` sends
the two reports it compiled regardless of shift. Verified on hardware: the 101-character
exit-criterion text compiled to **296** transitions, the log predicted ~11 s, and it took **12 s**
wall clock; a 2 040-character clipboard compiled to 4 080 transitions and ~163 s.

### E15. The paste's opening release-all is not §2.6's, and `paste_held` narrows E3's clause

§12 Stage 4c asks the job to begin with an explicit release. It **cannot** be
`Producer::request_release_all`: that is a §2.6 *cancellation*, which disengages the producer and
ends the capture session the paste needs — the paste would release the keyboard and then have
nowhere to type. The prelude is instead an explicit key-up (and button-up) for everything in
`Outstanding`: what the target holds because *this viewer* forwarded the press, which is exactly the
Shift of the trigger chord. The last of those ups leaves the target's report zeroed — the state a
release-all would have left it in — with the session still engaged. It is a prefix of the job's own
steps, so it is counted in the progress and paced like the rest, and it costs nothing on the wire
when nothing is held (§2.5).

The cancel goes the other way and reuses §2.6 exactly: `resolve_paste_chord` is a pure function
applied *before* `route_key`, and `Shift+Pause` during a running paste becomes `KeyAction::Release`
— **release wins, it never restarts** — so the cancel is the existing, already-tested path rather
than a second one that would have to be kept equivalent. `Action::ReleaseAll(reason)` carries the
reason through, and `ending_for` maps UserRequested/FocusLost/Shutdown → `Cancelled` and
Overflow/LinkDown/Reconnected/CaptureReleased → `Failed`, with the count in both.

And a real bug caught while wiring: E3's "the up edge of an outstanding key always reaches the
target" is **wrong during a paste**, because `Outstanding` then mixes what the host holds with what
the paste holds. A user who was holding Shift when the paste started and lets go while the paste is
holding Shift for a capital would have that up forwarded into the middle of the character. Hence
`RouteInputs::paste_held`, which excludes the paste's own presses from that clause. Nothing is
stranded: the prelude released what the host held, and every host key during a paste is dropped.

Measured: the release key at 3 000 ms stopped the job at **75 of 4080 keys**, which is 37 complete
characters plus one press whose release never went out; the target's `cat` shows exactly 38
characters and nothing after them — no repeat, no run-on — the lock bits are unchanged either side,
and text typed afterwards arrives in the right case, which a stuck modifier would have destroyed.

### E16. A 16 MiB read bound, and a refusal that logs a count and nothing else

**The bound is a stated deviation from §12 Stage 4c's "no length cap".** It is not a cap on a paste:
it bounds a `read_to_end` from a pipe an *unrelated process* writes, which without a bound is an
unbounded allocation driven by another program. At two transitions a character and 40 ms a
transition, 16 MiB of text is **over two weeks** of uninterrupted typing (asserted by a test, and
the smaller honest figure rather than "years"), so the cap that binds a real paste is still the
plan's own — the rate and the cancel. It is named in the three places anyone meets it: the refusal
(with the limit and the bytes that had arrived), the module doc, and the Paste tooltip. The read
also polls the pipe fd against a deadline and closes it rather than blocking in `read_to_end`, and a
`FetchGate` admits one reader at a time so a second paste is refused by name rather than queued.

**Nothing derived from the clipboard's content is logged, printed or persisted.** The log line is a
byte length, the layout and the lock state. The first version logged one WARN per unreachable
character — which puts the user's clipboard, character by character, into a log file — and the
review found it: `Refusal::log_line()` now emits the sentence plus a **count** and "see the Keyboard
popover", and the popover, which is on the user's own screen, still lists every offender. The
`digest` helper was deleted outright rather than kept as a content-derived value nothing needed. The
user's own clipboard was stashed for the hardware run with `wl-paste > file` and restored with
`wl-copy < file` **without ever being opened**, and it is 7 bytes again, as it was.

### E17. A button press during a paste: the pill stays clickable, and nothing reaches the target

The review asked for button presses to be dropped while a paste runs, which taken literally also
kills the chrome — clicking the pill or a popover item would do nothing for the minutes a long paste
lasts. The lead's ruling, and the shipped rule: `route_pointer`'s chrome clause sits **above** the
paste-drop clause, so a button press while `paste_running` is

- `Sink::Chrome` over the pill or with any popover open, while the chrome is reachable,
- `Sink::Dropped` everywhere else, including under a relative-mode pointer lock,
- and **never `Sink::Target`** — which is the invariant that actually protects the target, and is
  what the proptest asserts.

Nothing is stranded by the reordering: a press routed to the chrome ends at egui and never becomes
`Outstanding`, so no up edge is owed for it. This is the one behaviour in the stage that changed
*after* both 4c reviews, and it is the one that most wants a human at the keyboard (see "Carried
forward").

### E18. `deny_unknown_fields` catches the typo; `default` means a missing key is not an error

§12 Stage 4b says "a malformed file is an error naming the key, not a silent reset". As built,
`Config` is `#[serde(deny_unknown_fields, default)]`, which splits that sentence in two:

- a **typo'd** key is refused by name — `unknown field 'wheel_direciton', expected one of …`, with
  the file position, the offending line and a caret, printed with the path prefixed and a non-zero
  exit; nothing falls back to defaults;
- a **missing** key takes its default. A hand-written one-line config is a legitimate thing to
  write, and nothing is being reset because the key was never there. That is a deliberate reading of
  the plan's sentence and is pinned by a test.

The cost, which the plan should carry: **`deny_unknown_fields` makes the config
forward-incompatible** — an older binary refuses a file a newer one wrote. That is the right trade
here, because the alternative is the silent reset the plan forbids, and the error already names the
key. A layout the
build does not know is refused the same way, by the enum: typing against the wrong layout is the
failure §10.2 exists to prevent.

Two more config facts: `Store::save` is tmp-plus-rename (an interrupted in-place write truncates the
previous settings), and `persist` advances its `saved` marker **only on a successful save**, so a
failed write is retried rather than remembered as written. Verified by consequence on the desk: a
run that changed something wrote the file back exactly once, whole; runs that changed nothing wrote
nothing.

### E19. The viewer persists settings: anything starting it must isolate `XDG_CONFIG_HOME`

`tests/audio_hardware.rs::no_audio_leaves_the_stage_3_viewer_untouched` starts the shipped binary,
and once 4b landed that binary persists chrome settings — so the test wrote
**`~/.config/nanokvm/config.toml` in the user's own home**. Found because the file existed after the
hardware sweep; it has been deleted and the test now points `XDG_CONFIG_HOME` at a scratch directory
under `out_dir()`.

Two details worth carrying, both in comments at the call site. The directory is called
`nk-settings-root` and **not** `audio-hardware-xdg`, because setting `XDG_CONFIG_HOME` also
redirects the Vulkan loader's layer search, which prints the paths it looked in — and a path with
"audio" in it broke that test's own "exactly one line mentions audio" assertion. And every desk run
in this stage used an `XDG_CONFIG_HOME` under the scratchpad for the same reason;
`~/.config/nanokvm` does not exist on this desk, checked after every sweep. The general rule, now in
CLAUDE.md: **a hardware test or desk run that starts the viewer must point `XDG_CONFIG_HOME`
somewhere disposable**, or it is writing the user's settings.

### E20. Smaller deviations the plan should absorb by reference

| | Deviation | Reason |
| --- | --- | --- |
| a | `SourceOpener::set_format` changes what the **next open** negotiates, and the pipeline reopens. | V4L2 refuses `S_FMT` on a streaming node, and the pipeline already owns a reopen path that does `S_FMT`/`S_PARM`/`STREAMON` correctly and counts it (C14's `Step::Reopen`). A second route into the driver would be a second copy of the one negotiation this client does. |
| b | `enumerate_modes` reduces a **stepwise/continuous** frame-size range to its maximum. | `v4l`'s `to_discrete()` expands a stepwise range by its step — hundreds of thousands of entries for a 2×2 step over 4K. The device advertises discrete sizes (§6, measured: `1920x1080, 1280x720, 720x576, 720x480, 640x480`), so the arm exists to be honest, not to be used. |
| c | `render::chrome_fits_surface` **skips** a chrome frame laid out for a different surface, leaving its texture deltas untouched. | A resize races the event loop's rebuild; skipping the draw costs one frame, and freeing the deltas would lose atlas entries for ever. |
| d | The grip reads **"Hide" / "Menu"**, not `≡` / `⋮`. | egui's bundled fonts have neither glyph and the first snapshot rendered a tofu box. `egui_phosphor 0.11` stays the one-line upgrade (carried forward). |
| e | The **modal** clause exists in `route_key`/`RouteInputs`, but `modal_open()` is a constant `false`. | §12 Stage 4b puts the settings modal out of scope; the clause is in the plan, is cheaper to carry than to retrofit, and is tested. |
| f | `KeyAction::Paste` routes to `Sink::Target`. | Exactly as `KeyAction::Release` already does — `Sink::Target` means "the target's side of the loop", and for these two that is the release path and the paste trigger, not a forwarded keystroke. A fourth sink for one viewer-local binding would be a wider change than the thing it describes. |
| g | `MimeType::Text` rather than three explicit `Specific` attempts. | `wl-clipboard-rs` 0.9.3's `Text` **is** §12 Stage 4c's order — `text/plain;charset=utf-8`, then `UTF8_STRING`, then any `text/*`. Three `Specific` calls would be three Wayland connections and the same answer. niri advertises both `ext_data_control_manager_v1` and `zwlr_data_control_manager_v1`, measured with a `wl_registry` roundtrip, so the crate takes the newer one. |
| h | `Config` gained `layout` while 4b's `pill_x`/`pill_y` keep their claim on the plan's word "layout". | §12 Stage 4b read the persisted "layout" as the pill's position and §12 Stage 4c needs the keyboard reading of the same word. Both are present under names that say which is which; the module docs record the ambiguity. |
| i | Three hidden dev flags beyond `--chrome-popover`: `--capture-on-start`, `--paste-on-capture`, `--paste-cancel-after-ms`. | A paste needs a *captured* session and nothing on this desk may drive the pointer or the keyboard for a hardware run; `--paste-cancel-after-ms` feeds `Trigger::ReleaseKey`, the shipping cancel, not a private one. All are `hide = true` and all are refused alongside a subcommand by `viewer_only_flag()`. |
| j | `--no-audio` **spawns nothing**, rather than spawning and muting; mute is at the playback side, writing silence while still popping. | "No card opened" is otherwise unobservable from a test, and it is pinned structurally. Muting at the playback side keeps the ring flowing, so unmuting resumes on live audio rather than on a backlog and an overrun is never an artefact of having been muted. |
| k | The chrome config is loaded **before** audio starts. | Its `?` must not return an error after two audio threads exist; audio is still the last subsystem started, which is 4a's own rule. |

## Hardware run

Three runs on this desk (USB 2.0 link, CH9329 v1.8) against the Raspberry Pi 3B target: 4a's audio
run on 2026-09-11/12, 4c's paste run, and the integrated run after the merge. Timestamps are UTC and
are given where the handoffs record them. Every row is a consequence observed on the target, in the
PCM, at the sink, or reported by the device — never an ack.

| | Command / condition | Observed consequence |
| --- | --- | --- |
| 23:15 | `nanokvm shot` before touching anything | Target at a shell prompt, LXTerminal focused (`evidence/01-before.jpg`) |
| 23:16–23:29 | capability probe on the target | `speaker-test`, `aplay`, `pw-play` present; **no `pactl` on the Pi**. HDMI audio reaches the dongle with no configuration at all (`evidence/02-probe.jpg`) |
| — | every ALSA open, every run | `capture hw:8 granted 480 frames per period, 1920 frames of buffer (48000 Hz, 2 ch, resampling disabled)` and the same for `playback default` with resampling allowed — 480 is `RingConfig::period_frames` and 1920 is `device_periods × 480`. The "granted a period other than the ring's" warning has never fired |
| 01:5x | tone, three captures in order (quiet / tone / quiet) | ratio **0.018** → **689 857** → **1.0**; rms 0.283 throughout the tone, every half-second window ≥ 2 × 10⁶, so the tone is continuous rather than a transient in an average. `TONE_RATIO = 100` sits four orders below the signal and two above the floor — confirmed by measurement, not merely passed (`evidence/capture-*.wav`) |
| 02:00 | playback to a null sink, whole `AudioHandle` | The **sink's own monitor** recorded ratio **2 713 257**, rms 0.283, after `pactl list sink-inputs` confirmed by object id that our stream was on `nanokvm-test` and not the user's speakers (`evidence/audio-hardware-sink-monitor.wav`) |
| 01:44–01:57, 02:10–02:23 | drift, `NANOKVM_DRIFT_SECS=720`, twice | 71 999 periods captured, 72 000 written, **one** drift insert, **zero** overruns and underruns, level unchanged → **−13.9 ppm**, host clock the faster. The 30-second snapshots are exactly 3000 periods apart every time. Run 1 and run 2 — three hours and several code changes apart — agree to one period (`run-drift.log`, `run-drift2.log`) |
| 02:0x | `--no-audio` under `timeout` | **No `nanokvm-audio-*` thread** among the fourteen sampled, the Stage 3 threads all present (asserted, so a viewer that failed to start cannot pass), **exactly one** log line mentions audio and it is the disabled notice, title says `— audio off`, exit 0 (`evidence/viewer-no-audio.log`, 406 lines) |
| 02:0x | `EBUSY`, card held by a PipeWire recorder | Condition in **50 ms**, kind `busy`, naming `hw:8`; **one** logged condition over ~25 retries at 500 ms; `capture_opens == 0` and nothing captured while held; playback kept running with 370 counted silent periods and no condition of its own; recovery 4.5 s (PipeWire's linger, not a slow reopen) and `recoveries_logged == 1`; `stop()` returned in 27 ms (`run-busy.log`) |
| 02:31 | final 4a sweep | 5 passed, 0 failed, in 51.49 s; desk left clean |
| 02:02:02→14 | paste, every ASCII printable, 101 bytes | `296 key transitions … ~11 s at 40 ms a key`; **12 s** wall. On the target: `781ea9f294035a509fe843fd497ee94a` and `101` — **identical to the local md5 and byte count** (`4c/pi-after-md5.png`) |
| — | paste refused, `café naïve → ok` | Three offenders named with their positions; lock bits identical before and after; **`md5sum` of the screenshots either side is the same value** `c94258f26fbc616726e8e84c38505ab0` — not one pixel changed (`4c/pi-after-refusal.png`) |
| — | cancel, 2 040 characters, release key at 3 s | `paste cancelled after 75 of 4080 keys (UserRequested)`; the target's `cat` holds **38 characters**, nothing after them; lock bits unchanged; text typed afterwards arrives in the right case (`4c/pi-after-cancel.png`) |
| — | integrated run: chrome + audio + paste, 30 s | `["Mailbox", "Fifo"]`/`Fifo`; `53 bytes … 106 key transitions`, `paste done, 106 keys`, and on the target `9ece22c5608b8e1e3f589c37fffaa7c5` / `53` — identical to the local hash; audio **0 overruns, 0 underruns, 0 drift corrections, one open per side** over 25 s; `chrome draw p50 22 max 33 µs`, `chrome build p50 117 p95 148 µs`; `audio opening (capture)…` then `audio on` in the title (`merge-evidence/integrated.log`, `02-md5-witness.png`) |
| — | post-merge sweeps | `audio_hardware` 5 passed (51.42 s and 51.75 s, run twice), `serial_hardware` 1 passed, `capture_hardware` 3 passed — so **wgpu 27 did not break the Stage 1/2 paths** |
| — | D8 regression runs | 16 runs of the shipped fix, all exit 0, `coredumpctl … \| grep nanokvm` **empty**; the detach path fired on 7 of the 16 (`merge-evidence/d8b-1..16.log`) |
| — | the desk, left as found | `pactl list short modules \| grep nanokvm` empty; `pgrep` empty; `~/.config/nanokvm` does not exist; the user's clipboard 7 bytes, never read; lock bits `num=off caps=off scroll=off`; **no mouse report of any kind was ever sent** |

Evidence files are **session-local** — they live under this session's scratchpad
(`…/scratchpad/evidence/`, `…/scratchpad/4c/`, `…/scratchpad/merge-evidence/`) and are not in the
repository: the target-screen JPEGs and PNGs (`01-before.jpg`…`05-final.jpg`, `pi-before.png`,
`pi-after-md5.png`, `pi-after-refusal.png`, `pi-after-cancel.png`, `pi-final.png`, `00-before.png`,
`02-md5-witness.png`, `03-clean.png`), the four WAVs, the viewer logs, the two drift logs and the
thirty D8 logs.

**Eight defects the test suite could not have found**, all fixed or precisely located:

| | Defect |
| --- | --- |
| D1 | `prefill_periods == device_periods` is no cushion at all: the card's own buffer absorbs the prefill, the ring sits on its low-water mark and the policy invents two drift inserts a session (E6) |
| D2 | The playback thread stuck inside `snd_pcm_open`, silently — no open, no failure, no condition, and a title saying `audio on`. `stop()`'s detach deadline worked exactly as designed. Not reproduced in 146 further opens; answered by the `Opening` supervision (E10) |
| D3 | `PIPEWIRE_NODE` is **inherited by children** and overrides their `--target`, so the busy test's holder recorded the null sink instead of the card and the test measured nothing while reporting a pass |
| D4 | `PIPEWIRE_NODE` is a *request* PipeWire may ignore, and an unresolvable target is silently connected to the **default** sink — i.e. the user's speakers. The routing is now asserted by object id on every run that plays |
| D5 | ~2 ms of stale audio at the start of every freshly opened capture stream (E9); it took a negative control from ratio 1.0 to 85 against a threshold of 100 |
| D6 | A silent sink and perfect counters look identical to a silent *target*, because a period of silence is still a period. The test now waits 3 s for the Pi's preamble and, on failure, captures from the card while the tone plays so the next occurrence says which side was silent |
| D7 | A cascade of `Device or resource busy` on `/dev/ttyACM1`. Equally consistent with the tests overlapping each other and with another agent's viewer holding the dongle, and the two could not be separated. Both were answered: a process-wide `DESK` mutex, and a panic message that says "nothing above is evidence about the audio path" |
| D8 | The viewer segfaults on shutdown when the compositor is not scheduling — the Stage 3 render detach, not audio (E11). The release-all is submitted before the crash, so nothing is left held |

**Deferred, with reasons:**

- **The card vanishing mid-session.** `tests/audio_hardware.rs::card_vanishing_mid_session` is
  written and **prints a loud SKIP rather than passing quietly**, naming the usbfs node, its
  permissions and the unset `NANOKVM_ALLOW_REPLUG`. `usb-replug.py --dry-run video` resolves to a
  whole-device `USBDEVFS_RESET` on `/dev/bus/usb/003/035`, and those nodes are `crw-rw-r-- root
  root` with no ACL — `O_RDWR` as uid 1000 returns `EACCES`, confirmed, and every USBDEVFS ioctl
  needs write access; `sudo` needs a password this run did not have. The interface-only disconnect
  of `3-2.2.2:1.2` hits the same wall. What is *not* deferred is the resolution policy: the
  renumbered-card case is covered by `discovery::reopen`'s C13 unit tests (`hw:8` → `hw:9`, and
  `hw:8` → `hw:10` across the two-dongle fixture), and the busy test exercises the same reopen loop
  against the real card going away and coming back.
- **A full replug** is refused by policy while the user's dock is unplugged: the dongle would come
  back as `/dev/video0` and `/dev/ttyACM0` **permanently**, and every hardware test here hardcodes
  the current names.
- **The unlocked `present p50`.** The session was locked (`LockedHint=yes`) for the whole of Stage
  4; the locked figure agrees with Stage 1's locked figure, and the unlocked one is not claimed.
  One 20-second run at an unlocked desk re-takes it.
- **No human interaction pass on the pill.** Nothing may drive the pointer on the user's desktop,
  so the chrome's behaviour is kittest evidence plus the hidden dev flags, and the winit→egui
  translation, the drag and the hit test against real physical pixels are unexercised by a person.
- **`drift_over_ten_minutes` was not re-run after the merge** — measured twice already, nothing in
  the merge touches the ring, the drift policy or `RingConfig::default`, and a third run costs
  thirteen minutes of the user's desk for a number that is already measured.

## Review

Each sub-stage went to **two adversarial reviewers blind to each other** — Codex (read-only
sandbox) and a Claude reviewer — with a standing requirement to demonstrate each finding with a
failing test. Three rounds:

| Round | Findings (Codex + Claude) | Accepted and fixed |
| --- | --- | --- |
| 4a — audio | 7 + 10 | **10**, each with a test that fails without the fix, and several with the *mutation* recorded (the fix removed, the test run, and what it printed) |
| 4b — chrome | 3 + 14 | **14**, every one, each verified to fail without its fix |
| 4c — paste | 4 + 7 | **all 11** — two were the same defect found by both reviewers (the generation-baseline race and the leaking fetch threads), so 10 fix items; the 16 MiB read bound was kept as a documented memory bound rather than removed (E16) |

**Thirty-four accepted fixes in all.** The two declined in 4a were declined for scope rather than
for merit and both were then done: *"mute is not wired to anything"* — correct, and it is 4b's, and
4b wired it; and *"the drift rate is unmeasured"* — correct, and it was the hardware phase's, which
measured it twice (E7).

The six the supervisor rates as material:

- **The stuck-release routing hole (4b).** Both reviewers found it from different directions: a key
  whose press was forwarded and whose release arrived after a popover opened was routed to the
  chrome, and the target went on holding it. It is the one finding in the stage that could leave a
  key down on a live console without any error anywhere. The fix is `Outstanding` (E3), proptested
  over all 192 flag states in both directions, and its shrunk counterexamples are kept in
  `tests/viewer_chrome.proptest-regressions` as genuine seeds for the bug that was just fixed.
- **`QUERYCAP` in the retry loop (4a).** `DiscoveringCardOpener::open` ran full discovery — a
  `VIDIOC_QUERYCAP` on every `/dev/video*` on the machine — **every 500 ms, for ever**, whenever no
  card paired. On this desk that is a background task opening the user's webcam twice a second, and
  a periodic entrant into the replug race. The fix is E8's probe-free resolver, plus a pure
  `reopen_delay` that doubles only on a genuine *absence* (500 ms → 10 s), and a test that fails if
  the video or tty classes are so much as listed.
- **One condition slot for two sides (4a).** `AudioStats.condition` was a single `Option`, so a
  capture failure and a playback failure displaced each other and the surfaced condition depended on
  which thread reported last. Now one slot per side, with a deterministic aggregate. Mutation:
  `index()` hardcoded to 0 → `60 conditions, left: 60 right: 2`.
- **The generation-baseline race (4c).** `Producer::refresh_device_info()` returned `()`, and the
  caller sampled `stats()` *afterwards* for its baseline — so a refresh serviced in between read as
  "nothing changed" and the paste would have compiled against the previous reading. It now returns
  the generation the request was made against, read inside the same critical section that sets the
  flag, and is `#[must_use]`; the test does not compile against the old signature.
- **The offenders-in-the-log privacy leak (4c).** A refusal logged one WARN line per unreachable
  character — the user's clipboard, character by character, into a log file. Now a count and a
  pointer at the popover (E16), with `digest` deleted entirely rather than kept.
- **D8's segfault, and the wrong first fix (hardware + merge).** Worth naming not for the crash but
  for the correction: the obvious fix — a flag the render thread reads after `present` returns —
  was implemented exactly as suggested and **failed on the desk**, with the core dump naming the new
  function on the `drop` branch. The deadline lands *inside* the teardown, not before it. The
  shipped rule is that the thread never destructs at all (E11).

Two smaller ones deserve their names for the same reason. `stop()` could join a thread blocked for
ever in `readi`/`writei` (4a): both PCMs are now non-blocking and driven through `pcm.wait()`, the
stop flag reaches the device through a new opener hook, and `stop()` polls to a 250 ms deadline and
then *detaches with a warning* — the mutation disabling that branch made the test **hang**, which is
what the defect was. And `Store::save` wrote in place (4b), so an interrupted save truncated the
previous settings; it is tmp-plus-rename now, and `persist` advances its marker only on success.

## Carried forward

- **The card vanishing mid-session, and the different-port replug.** The first needs a desk with
  writable usbfs (or `NANOKVM_ALLOW_REPLUG` and a password); the test is written and skips loudly.
  The second is E8's stated cost — audio reports no card until restart — and is a design decision,
  not a defect, but nobody has watched it happen.
- **The unlocked `present p50`** was not re-taken, and with it the other half of §5.3.
  `scratchpad/measure.sh <collapsed|open|popover>` does it in 20 seconds at an unlocked desk.
- **A human pass on the chrome.** Ten seconds of someone clicking the pill, dragging it, pressing
  `Shift+Pause` over a captured window and `Pause` again mid-paste would close the last gap between
  the pure rules and the winit events that feed them. E17's ruling — the pill stays clickable during
  a paste — changed after both reviews and is the part most worth confirming by hand. A compositor
  *without* data-control would also exercise the disabled Paste item's real tooltip; niri advertises
  both protocols, so that path has only unit and kittest evidence.
- **The `Opening` supervisor's threshold is `reopen_backoff_cap` (10 s) and has never fired in
  anger.** It was set from the existing constant rather than from a measurement of how long a
  healthy open takes (about 6 ms, measured across 146 opens), so there is a great deal of room in
  it. D2 has never recurred, so the supervisor has no true positive to its name yet.
- **The remaining hardware tests hardcode node names.** `tests/audio_hardware.rs` resolves the card
  through `discover` + `CardResolver::real` — the production path, never `hw:8` — but still asserts
  `/dev/video4` and `/dev/ttyACM1` so a renumbered desk fails with an explanation instead of
  measuring the wrong device. Stage 2's and Stage 3's note stands unchanged.
- **`deny_unknown_fields` is forward-incompatible** (E18): an older binary refuses a config a newer
  one wrote. Accepted deliberately; if a key is ever removed rather than added, this is the sentence
  to re-read.
- **Several pre-existing tests need the gitignored `fixtures/frames/` corpus** and panic without it
  (`capture::jpeg::the_720p_corpus_reports_1280x720`, two in `capture::decoder`, and
  `capture::pipeline`'s `fixture_dir()` users), so a fresh worktree fails `cargo test` until the
  corpus is copied in. 4b's new `tests/capture_format.rs` follows the opposite convention — it
  returns early with a message pointing at `fixtures/frames/MANIFEST.md` — and the inconsistency is
  a lead decision nobody has taken.
- **`egui_phosphor 0.11` is the one-line icon upgrade.** It pins `egui ^0.33` exactly, is
  MIT/Apache-2.0, exposes codepoints as named Rust consts (so a wrong name is a compile error rather
  than a tofu box), and is the answer if the grip's "Hide"/"Menu" words ever want to be glyphs. The
  Lucide facts are recorded so nobody re-researches them: ISC, not on crates.io, 372 763 bytes of
  TTF in `lucide-static`, 2098 PUA codepoints in a sibling CSS file, and a hand-transcribed table
  with nothing in CI to notice when it drifts.
- **The SuperSpeed sound card has never been recorded.** The two SuperSpeed fixture trees carry no
  card, deliberately: `topology.md` recorded the dongle's two device nodes and nothing about its
  audio interface, the dongle has not been on a SuperSpeed port since Stage 0, and inventing one
  would be evidence of nothing. What the tests pin instead is that an absent card changes nothing
  about the pairing and is reported honestly as "no audio". SuperSpeed remains unexercised
  generally, as at Stage 2 and Stage 3 exit.
- The Stage 3 carried-forward list otherwise stands: `key` has no `--layout` and only `us` exists;
  no mouse step in macros, deliberately, and still no mouse path under `src/cli/` or `src/script/`
  by test; every command still runs one `inventory`; `tests/viewer_startup.rs` still enumerates the
  real `/sys`; `--sysfs-root` is not plumbed into `discovery::reopen`; the recorded desk yields two
  candidate pairs under `--sysfs-root`; and `capture::handoff`'s one-off flake. Stage 2's list
  behind it is unchanged.
