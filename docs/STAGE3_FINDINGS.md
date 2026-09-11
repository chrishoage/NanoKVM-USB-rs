# Stage 3 — Findings

Stage 3 is **CLI conveniences** (plan §12): `devices`, `shot`, `key <chord>`, `type` with a
declared layout and an unreachable-character policy (§10.2), and macro files. This document is the
evidence index for what Stage 3 changed, in the same shape as `STAGE2_FINDINGS.md`: the amendments
the plan needs (D1–D10), the module and test inventory, the hardware run, and what is still
outstanding.

**Status at the time of writing:** every Stage 3 item is built, reviewed by **two adversarial
reviewers blind to each other**, fixed, and then **verified on hardware by consequence** (§12's
rule, and A17's). The three consequences observed on the target and the device, not in an ack:

- the **target's lock bit read back** after `nanokvm key capslock` — off → ON → off across three
  `devices --probe` reads, which is the device's own account of its HID state and not the ack the
  command printed;
- the **typed commands on the target's terminal**, photographed by `shot`: the macro's
  `echo stage3 macro ok`, the piped `echo stage3 stdin ok`, and their output;
- the **compensated CapsLock line**: with CapsLock on, `type --caps-lock compensate 'echo Caps
  Compensated'` put exactly `echo Caps Compensated` on the target's command line — right case,
  wrong lock state — while the default `refuse` in the same condition typed nothing at all.

Green at the time of writing: `cargo fmt`, `cargo clippy --all-targets -- -D warnings` with and
without `--features hardware`, `cargo test` **587 passed, 0 failed, 1 ignored across 32 test
targets**, and `check-fixtures.py` (**17 requests, 11 responses, 3 documented anomalies**).

The hardware run found five defects the tests could not (H1–H5, §"Hardware run" below), and the two
reviews found fifteen more. All twenty were fixed in one pass, each with a test that fails without
the fix, and the hardware run was then **repeated against the fixed binaries** (21:08 UTC). Both
runs are in the table below.

## What was built

| Module | Stage 3 change |
| --- | --- |
| `cli/mod.rs` (new) | The subcommand enum, and the two things every command shares: `install_signal_handlers` (SIGINT, SIGTERM **and** SIGHUP — a harness timeout and a closed terminal are exactly the callers these commands exist for) and `sources()`, the one place that decides what discovery may read and open. Its doc comment carries the rule the whole stage rests on: **a subcommand opens only the node it needs**, and the residual cost is discovery's own read-only `QUERYCAP`. |
| `cli/out.rs` (new) | Printing that survives a closed pipe. `println!` panics on `EPIPE`; `head -1` closing the pipe is an ordinary thing to do. The stream is given up once and the command still runs to its own end and releases (D8). Explicitly **not** `SIG_DFL` for `SIGPIPE`, which would kill the process past the release-all guard. |
| `cli/devices.rs` (new) | `nanokvm devices`: Stage 2's `Inventory::listing` unchanged, plus `--probe`, which opens **only the serial node `discover` would select, or the one `--serial` names** (`probe_targets`, a pure function with its own fixture tests), sends one `GET_INFO` and three `GET_USB_STRING`, and nothing else. |
| `cli/shot.rs` (new) | `nanokvm shot`: `pick_frame` (the frame policy as a pure function of a `FrameSource`), `ShotTarget` (`.jpg` = the device's bytes untouched, `.png` = the pipeline's own decoder plus the `png` crate, `-` = the JPEG on stdout), atomic write through a `Temporary` that unlinks itself on panic or signal, and `summary_line`, which prints what the policy threw away. |
| `cli/keys.rs` (new) | `key`, `type`, `macro`: the I/O half — open, gate on `GET_INFO`, decide the CapsLock question, guard, pace — in that order, which is the order the safety argument depends on. `Released` is the `Drop` guard that sends the keyboard release-all on every exit path (§2.6). No mouse report exists in the file. |
| `script/` (new, pure) | `keynames` (the name vocabulary → `winit::KeyCode`, resolved to usages by `proto::keymap` so no usage is retyped), `layout` (the declared **target** layout, §10.2, US QWERTY only), `chord` (the `ctrl+alt+t` grammar; a single-character last token resolves through the *layout*, D7), `compile` (`compile_key`/`compile_type`/`compile_macro`, each returning a fully resolved `Script` or an error naming **every** problem), `dry_run` (the render, which names each frame after `fixtures/packets/ch9329.toml` and prints `DISAGREES` where the encoder and the authority differ), `fixtures` (reading that file). No I/O anywhere in the module. |
| `proto/usb_string.rs` (new) | `UsbStringKind`, `UsbStrings`, `parse_usb_string`: the `[type, len, ascii…]` reply shape, defensive about the device's own `LEN` (§3.2), now pinned to observed bytes (D3). |
| `proto/frame.rs` | `Display for DeviceInfo` and `DeviceInfo::locks()`: one wording for the lock bits everywhere — `caps=on`, never `caps=true` (H2). `input/writer.rs` and `viewer/app.rs` were rewritten onto it, so the viewer's startup line, the reconnect line, `key`/`type` and `devices --probe` now print the same words for the same bit. |
| `serial/` | `SerialLink::get_usb_strings` (three transactions, each logging its raw reply payload at debug — which is how the reply fixtures got their bytes), and `fake::FakeCh9329` answering all three string types plus `ERR_PARAM` for a type it does not have. |
| `discovery/` | `NoProbe`: a probe that opens nothing and says so, for a discovery run against a *recorded* tree. Its refusal is an `Unsupported` error rather than a silent `false`, so the listing reports "not probed" instead of claiming a capture node it never asked about. |
| `main.rs` | The subcommand dispatch, the globals (`--serial`/`--video`/`--width`/`--height`/`--fps` are `global = true` and work on either side of the subcommand), `Needed::One` for a command that opens one node, `short_pairing_line` (H5), the hidden `--sysfs-root`, and the `--dry-run` dispatch that happens **before** any device is resolved (H3). `--list-devices` is gone; it is `nanokvm devices`. |
| `fixtures/packets/ch9329.toml` | Six new entries: the three `GET_USB_STRING` requests flipped to `observed = true`, and the three replies added as `[[response]]`s, all six carrying `observed_on = "2026-09-11, nanokvm devices --probe, USB 2.0 link"`, a field the file's header now explains. |
| `examples/` | `type-keys` and its `script.rs` are **deleted**, as Stage 2 said they would be when the CLI arrived (D10). |

**Tests: 451 (Stage 2 exit) → 587**, all non-hardware, across 32 test targets; the one `#[ignore]`d
non-hardware test is unchanged (`capture::decoder::tests::decode_timing_spot_check`, a timing
measurement run explicitly). The hardware test binaries are untouched by this stage. The new test
binaries are `tests/cli_keys.rs` (18), `tests/cli_shot.rs` (22), `tests/cli_shape.rs` (11) and
`tests/script.rs` (2); `tests/type_keys.rs` (14) was deleted.

## Amendments the plan needs

### D1. §2.9's "same queue, different admission policy" is not what a script should use

§2.9 says the GUI and scripts share one queue and differ only in admission. They do not, and should
not. `key`, `type` and `macro` transact **one report at a time** over a `SerialLink` under the
`Released` guard (`src/cli/keys.rs::run`), and never touch `input`'s queue or its writer thread.

Two reasons, both structural:

- **§2.7's reconnect policy is the wrong policy for a script.** The writer's answer to a link that
  dies mid-sequence is to discard the queue, release everything and resume disengaged (C1). That is
  right for a viewer, where the user is present and can re-capture. A script must do the opposite:
  stop, and say **how far it got** (§2.8 item 3). Every error path in `run` carries
  `{sent} of {total} reports had been delivered, so the sequence is incomplete`, which is a fact the
  queue deliberately does not keep.
- **The queue would coalesce nothing.** Coalescing exists for pointer motion (§5.1, C8), and a
  script has no motion — by construction (D6: there is no mouse path in these commands at all).

What §2.9 should say: the *viewer* has a queue with a non-blocking admission policy; a script is a
blocking, paced sequence of acknowledged transactions over the same `link` seam, and it fails
rather than recovers.

### D2. §10.2's text injection, as shipped — and the CapsLock policy §10.2 did not anticipate

§10.2 asked for an explicit `--layout` with a stated default and a declared policy for unreachable
characters. All three are built, and one thing more that §10.2 could not have predicted from the
desk:

- **`--layout`, one layout.** `Layout::Us` is the only variant; the default is stated in help
  (`[default: us]`, pinned by the help-text test in `tests/cli_shape.rs`) rather than assumed
  silently. The flag is `Option<String>` rather than a clap default, because
  `macro` needs to tell "the user said `us`" from "the user said nothing" — see R3 below.
- **Every unreachable character at once.** `CompileError::Unreachable` carries each offending
  character once with the 1-based index of its first occurrence, and the message ends
  `text injection needs a key for every character (§10.2); nothing was sent.` A long line is fixed
  in one pass, not one character per run.
- **The CapsLock policy.** §10.2 treats the target's keyboard state as unobservable. It is not:
  `GET_INFO` reports the target's lock bits (Appendix), so the client *knows* that the letters it is
  about to send will arrive inverted. That makes it a determinism decision rather than an
  approximation, and the decision is handed to the user: `--caps-lock refuse` (the default; nothing
  is typed), `ignore` (send as compiled), or `compensate` (invert shift on ASCII letters). Both
  compilations are produced **before the port is opened**, because a decision that needed a compile
  after the gate would put a resolution failure on the far side of §2.8 item 3.

Measured, 2026-09-11, with the target's CapsLock on: `type 'echo Refused'` exited 1 and the target's
terminal shows no such line; `type --caps-lock compensate 'echo Caps Compensated'` followed by
`key enter` put `echo Caps Compensated` on the command line and `Caps Compensated` under it — the
right case with CapsLock on (`shot2.png`).

The refusal's wording is itself an amendment: it says **"No keyboard report was sent."**, not
"nothing was sent". By the time it is reached the port is open, the §5.1 preamble is written and
`GET_INFO` has answered, so "nothing" would be a claim the code cannot make (§3.4, R7).

### D3. §8's last paragraph: `GET_USB_STRING` is opt-in, and its reply bytes are now pinned

§8 records the three strings (`Sipeed` / `NanoKVM-USB` / `BA1612624UJPW2RUJ`) as "useful for logs
and for Stage 3's device listing". As built:

- It is read by **`devices --probe` and by nothing else**. The plain listing opens no serial node;
  its only device access is discovery's read-only `QUERYCAP`, exactly as before.
- The reply shape is `[type, len, ascii…]` — the datasheet's, which A16 never confirmed because it
  recorded the *strings* and not the bytes. The 2026-09-11 probe run logged the raw payloads at
  debug, and they are now in `fixtures/packets/ch9329.toml` as three `[[response]]` entries with
  their checksums derived, alongside the three requests flipped to `observed = true`. All six carry
  `observed_on`, a field the file's header now explains: they were seen on a later run than the
  Stage 0 traffic capture, and the file should not pretend otherwise. `parse_usb_string` still
  accepts the one-byte-shorter form, since a second unit has not been seen.
- The serial string remains useless for *pairing* (§8's own point: reading it requires having
  already opened the link) and is printed as identification only.

### D4. §12 named `shot` without a policy; this is the policy

§12's one line for Stage 3 does not say which frame a screenshot is. Every rule below is a Stage 0–2
finding applied to a single frame, and all of them are tested against `SyntheticSource` rather than
the dongle (`tests/cli_shot.rs`):

1. **Skip 8 delivered frames** (`--skip`, default 8). A6: after an idle gap the device emits up to
   eight frames at the *previous* resolution, and a stream that has just been started is exactly
   that case.
2. **Reject a frame with no end-of-image marker.** C12's four truncated JPEGs at a signal transition
   are what this is for; a truncated JPEG written to a file is a corrupt screenshot nothing
   downstream can distinguish from a real one. Trailing zero bytes are tolerated before the check,
   and the whole Stage 0 frame corpus is asserted to pass it.
3. **Dimensions from this frame's SOF header** (A6), never `G_FMT`. A header that will not parse is
   the frame's problem: dropped and counted.
4. **Compare with the negotiated size** (C14, unless `--any-size`). 30 consecutive mismatches buy
   **one** `restart()` — `STREAMON` is what re-commits a lost format — and 90 in total end the
   attempt with an error naming both sizes and pointing at `--any-size`. The pipeline's ladder
   (three restarts, one escalation reopen, then acceptance) is for a session that keeps running; a
   shot has one remedy to spend.
5. **`.jpg` is the device's bytes byte for byte**, so the file is exactly the frame whose header the
   size came from; `.png` is that frame through the pipeline's own decoder (RGBA, strict) and the
   `png` crate, so a PNG is never a second opinion on a JPEG the client could not read. Either way
   the file appears atomically via a `.tmp` and a rename.

Measured, 2026-09-11 (release build): `shot shot1.jpg` produced 1920x1080, 181 832 B, `skipped 8
frames, 0 at another size`, **0.31 s wall** including discovery and `S_FMT`/`S_PARM`; `shot
shot1.png` produced 478 049 B of 1920x1080 RGBA; `shot -` produced a valid JPEG on stdout. Every
shot this session skipped exactly 8 and mismatched 0 — the stale-size case this policy is armed for
did not occur, which is what "armed for" means.

### D5. §8's "never guess" extends to probing: only the pair discovery *selects* is opened

§8's policy is about which pair to *use*. `devices --probe` writes CH9329 frames into a serial node,
which makes "which node do I open to find out what this is" the same question with a sharper edge,
and the first version got it wrong: it probed the serial half of **every candidate pair** in
`Inventory::pairs`, while `discover` refuses to choose when there is more than one.

Two of §8's three rules (`same_device`, `port_peer`) carry no vendor-id filter, so on a SuperSpeed
desk an unrelated CDC-ACM device under a peered port would have been sent CH9329 frames. The only
test used the `usb2-desk` fixture, which has no `peer` links, so nothing caught it.

As fixed, `probe_targets` is a pure function of what `discover` returned: `--serial` wins outright
and is then the *only* node opened; otherwise it is the serial half of the one selected pair;
otherwise **nothing**, and the caller prints discovery's own listing and exits non-zero. Pinned by
`cli::devices::tests::an_ambiguous_desk_yields_nothing_to_probe` against the `two-dongles` fixture,
and by `only_the_serial_node_discovery_selected_is_ever_probed`, which first asserts that the
fixture still carries the unrelated `/dev/ttyACM0` the rule is about.

### D6. The CLI's shape, and what it refuses

- **No subcommand is the viewer, unchanged** — same flags, same behaviour, same startup.
- **`--serial`, `--video`, `--width`, `--height`, `--fps` are global and work in either position.**
  `nanokvm --serial X key a` and `nanokvm key a --serial X` are the same command. clap's
  `args_conflicts_with_subcommands` gives the first of those a usage error, which is why it is not
  used (H4); the check is written out instead.
- **Viewer-only flags are rejected by name**: `nanokvm --pointer relative devices` exits with
  `--pointer has no meaning for 'devices'`. They are `Option` for exactly this reason — a clap
  default would make the mistake invisible and the flag silently ignored.
- **`--list-devices` is gone**, replaced by `nanokvm devices`.
- **A subcommand opens only the node it needs**: `devices` nothing (bar discovery's `QUERYCAP`)
  unless `--probe`, `key`/`type`/`macro` the serial node, `shot` the video node, and none of them
  starts a viewer thread. `Needed::One` makes a discovery failure survivable when the needed node
  was named explicitly, so `key --serial … a` works on a desk whose video node is unplugged.
- **`--dry-run` runs no discovery at all.** It is dispatched before `select_devices`, so it reads no
  `/sys` and opens nothing — not even the read-only `QUERYCAP` (H3). The node argument it is handed
  is the string `<no device: --dry-run resolves none>`, which is not a path, so a future call that
  reached an `open()` would say where it came from.
- **A hidden `--sysfs-root`** points discovery at a recorded tree so that binary-level tests
  enumerate a fixture desk rather than the developer's (R10). It also replaces the probe with
  `NoProbe`, because a recording's `/dev` names belong to whatever is plugged into *this* machine —
  on this desk, the user's webcam.

### D7. `key A` typed `a`

A single-character chord token was resolved through the case-insensitive key-name table before the
layout, so the name `a` matched `A` and the shift was dropped: a silent wrong keystroke on a live
console. The rule is now that a **single-character** last token resolves through the declared layout
first (which knows `A` is shift+`a` and `:` is shift+`;`), and only a longer token goes to the name
table. Found by review, pinned by test, and confirmed on hardware: `key --dry-run A a` renders
report 1 as `57 AB 00 02 08 02 00 04 …`, which is the authority file's `kb_shift_a_press`, and
report 3 with no shift.

### D8. Signals and pipes: three signals caught, and `EPIPE` is not a panic

`key`, `type`, `macro` and `shot` catch **SIGINT, SIGTERM and SIGHUP**. SIGTERM is what a harness
sends when its timeout fires and these commands exist to be driven by such a harness; SIGHUP is the
terminal going away. Taking the default disposition for either kills the process between a press and
its release, or between a `create` and its `rename`. All three set one flag, which the send loop and
the frame loop poll, and everything that actually happens — the abort, the release-all, the unlinked
temporary — happens on the main thread through `Drop` guards.

A closed stdout is **not** a panic, and the fix is emphatically not `SIG_DFL` for SIGPIPE: restoring
the default disposition would kill the process at the write, past the release-all guard, leaving a
key held on a live console. Instead every user-facing line goes through `cli::out`, which treats
`BrokenPipe` as "stop printing and keep going". The one exception is `shot -`, where stdout *is* the
output and a JPEG that did not get through is a failed screenshot.

Measured: before the fix, `nanokvm key capslock | head -1` panicked with `failed printing to
stdout: Broken pipe` — **and the guard still released**, which is why nothing was left held, and
why the defect was cosmetic in effect and structural in kind. After the fix the same pipeline
exits 0, still prints `[release-all] keyboard report sent` on stderr, and the target's CapsLock
was toggled back and read off as `caps=off`.

### D9. Diagnostics are complete, not first-error — and no user value can panic a deadline

Every compiler in `script` reports **all** of what is wrong: `key` lists every invalid chord,
`macro` every bad line with its line number, `type` every unreachable character. Nothing is sent on
any of those paths, so there is no reason to hurry the failure, and the alternative makes fixing a
five-chord line a five-run job with the port opened in between.

Two values that used to panic are now refused before anything is opened:

- a macro `wait` is bounded at **one hour** at compile time — `Instant::now() +
  Duration::from_millis(u64::MAX)` overflows and panics, and it would have done so mid-script with a
  key possibly held;
- `shot --timeout` is bounded at **3600 s** and validated **before the video node is opened**, for
  the same reason with the node already open.

Both bounds are §2.8 item 3 applied to arithmetic: every refusal happens before the device is
touched.

### D10. The `type-keys` instrument is deleted, as it promised

Stage 2 shipped `examples/type-keys` as a scripted keyboard sender and said it existed until the CLI
arrived. It is gone, with `examples/type_keys/script.rs` and its fourteen tests in
`tests/type_keys.rs`. Every property those tests pinned is still pinned, in the place that suits it:

- the **process-level** ones continue as `tests/cli_keys.rs` (18 tests) against the real binary —
  the `GET_INFO` gate, the `Drop` guard, the three signals, the abort that still releases, the
  US QWERTY typing path, and the grep that proves no mouse path exists (now over all of `src/cli/`
  and `src/script/`, not one example file, R5);
- the **compiler** ones moved down into `src/script/`'s in-module tests, by the same names where the
  behaviour is the same (`an_unknown_key_name_is_rejected_and_produces_nothing`,
  `a_chord_is_one_press_with_the_modifier_bits_and_one_release`), with the fixture check as
  `tests/script.rs`.

## Hardware run

All of it on 2026-09-11, this desk (USB 2.0 link, CH9329 v1.8) against the Raspberry Pi 3B target,
release build. Every row is a consequence observed on the target or reported by the device, not an
ack (§3.4, A17).

| Command | Observed consequence |
| --- | --- |
| `nanokvm devices` (19:03) | The Stage 2 `--list-devices` table unchanged, exit 0; nothing opened but `QUERYCAP` |
| `nanokvm devices --probe` | Probed **exactly `/dev/ttyACM1`** and never `/dev/ttyACM0` on bus 5; exit 0. Raw reply payloads read from `RUST_LOG=debug`: `00 06 "Sipeed"`, `01 0B "NanoKVM-USB"`, `02 11 "BA1612624UJPW2RUJ"` — the `[type, len, ascii]` shape, now three fixtures (D3) |
| `nanokvm shot shot1.jpg` (19:18) | 1920x1080, 181 832 B, `skipped 8 frames, 0 at another size`, **0.31 s** wall including discovery and `S_FMT`/`S_PARM`; `file` confirms JPEG |
| `nanokvm shot shot1.png` | 478 049 B, PNG 1920x1080 RGBA; viewed — the Pi desktop with a terminal open |
| `nanokvm shot -` | A valid JPEG on stdout per `file -` |
| `^C` during `shot --skip 100000 --timeout 60` | `Error: interrupted`, exit 1, **no output file and no `.tmp` left** |
| `nanokvm key capslock` ×2 (19:2x) | `devices --probe` read the target's lock bit **off → ON → off** across the two presses. The lock bit, from the device; not the ack |
| `nanokvm macro check.macro` | 44 reports; the target's terminal shows `echo stage3 macro ok` and its output (`shot2.png`) |
| `printf 'echo stage3 stdin ok\n' \| nanokvm type -` | 42 reports; shown on the target — the trailing newline became an Enter, as `type -`'s verbatim read intends |
| CapsLock ON + `type 'echo Refused'` | Exit 1, `the target reports CapsLock ON …`; **the terminal shows no such line** |
| CapsLock ON + `type --caps-lock compensate 'echo Caps Compensated'` + `key enter` | The target shows `echo Caps Compensated` and `Caps Compensated` — correct case with CapsLock on |

**Five defects the test suite could not have found**, all fixed in the same pass as the review's:

| | Defect |
| --- | --- |
| H1 | `key capslock \| head -1` panicked on `EPIPE`. The `Released` guard still fired and the CapsLock state was consistent afterwards, so nothing was left held — but a CLI must not panic because its reader left, and the obvious fix (`SIG_DFL` for SIGPIPE) would have killed the process *past* the guard |
| H2 | `key`/`type` printed `locks: num=false caps=true`; `devices --probe` printed `num=off caps=on`. One bit of one reply, two wordings |
| H3 | `--dry-run` ran discovery — `QUERYCAP` on candidate video nodes — so "opens no device" was not quite true |
| H4 | `nanokvm --serial X key a` was a usage error (clap's `args_conflicts_with_subcommands`): globals were accepted only *after* the subcommand, which is not how most people type them |
| H5 | §8's degraded-rule warning is a six-line paragraph, printed on **every** subcommand run including dry runs — noise that trains a user to stop reading warnings |

**Post-fix rerun, 21:08 UTC, release build, same desk:**

| | Result |
| --- | --- |
| H1 | `key capslock \| head -1` exits 0, no panic, `[release-all] keyboard report sent` still on stderr; CapsLock toggled back and read `caps=off` afterwards |
| H2/H5 | Every subcommand run logs **one** WARN line — `using /dev/ttyACM1, paired with /dev/video4 by the degraded internal-hub rule (nanokvm devices shows the evidence)` — and `key` prints `locks: num=off caps=off scroll=off`, identical to `devices --probe` |
| H4 | `nanokvm --serial /dev/ttyACM1 --video /dev/video4 key --dry-run A` accepted; `nanokvm --pointer relative devices` → `error: --pointer has no meaning for 'devices'` |
| R4 | `key --dry-run A a`: report 1 is `57 AB 00 02 08 02 00 04 …` = fixture `kb_shift_a_press`; report 3 carries no shift. The uppercase single-character chord keeps its shift |
| R1 | `devices --probe` still probes exactly one node, `/dev/ttyACM1`, with the same strings |
| — | `key numlock` twice, 2 reports each, exit 0; the device reports `num=off` afterwards |
| — | 587 passed, 0 failed, 1 ignored; `check-fixtures.py`: 17 requests, 11 responses |

## Review

Stage 3 went to **two adversarial reviewers, blind to each other** — Codex (gpt-5.6-terra,
read-only sandbox) and a Claude reviewer — with a standing requirement to demonstrate each finding
with a failing test. Both returned "needs-attention": five findings from Codex (C1–C5) and ten from the
Claude reviewer (R1–R10), **fifteen in all**. **Every one was accepted and fixed**, each with a test that fails without the fix,
and the five hardware defects above were folded into the same pass.

The three the supervisor rates as material:

- **R1 — `devices --probe` probed every candidate pair** (D5). The only one that could have written
  CH9329 frames into hardware that is not the dongle's, and the only one whose test coverage was
  actively misleading: the fixture it used has no `peer` links, so the rule that would have failed
  was never exercised.
- **R4 — `key A` typed `a`** (D7). A silent wrong keystroke on a live console, from a table lookup
  order.
- **R3 — the macro `layout` directive was unreachable.** `MacroArgs.layout` had a clap default, so
  `compile_macro` always received `Some(layout)` and a file's own directive could never apply; every
  non-`us` directive reported a conflict with a flag the user had not passed. The fix is the
  `Option` D2 describes, and it is why `--layout` is spelt that way in two places.

The rest: C1/C2 (first-error diagnostics for chords and macro lines, D9), C3/C4 (the two
overflowing deadlines, D9), C5 (an unbounded `Command::output()` in `tests/cli_shape.rs` that
would have hung the suite rather than failed it), R2 (`--skip` and `BadFrame` sharing one counter,
so bad frames ate the A6 budget), R5 (the no-mouse grep did not cover `src/cli/shot.rs`,
`devices.rs`, `mod.rs`), R6 (`shot` left its `.tmp` on SIGTERM/SIGHUP and on an unwind), R7
("nothing was sent" over-claiming, D2), R8 and R9 (below), R10 (the hidden `--sysfs-root`, D6).

**R8 deserves a correction, because the reviewer's premise was only partly right.** The finding was
that `pick_frame` checked the deadline before `next_frame` and never after, so "a good frame
arriving just past the deadline was discarded unlooked-at". In the code as it stood, a frame already
*in hand* was never discarded — the deadline was consulted before asking for the next one, so
anything that came back was judged. The one case that genuinely differed is a deadline that had
**already passed on entry** (`--timeout 0`), where the old order returned `Timeout` without asking
the source for a single frame. The reordering was adopted anyway, because the new order is the one
worth stating — ask, judge what came back, and only then let the clock end the attempt — and both
cases are now pinned by test (`a_frame_is_examined_even_when_the_deadline_has_already_passed` and
`a_frame_that_is_no_good_ends_the_attempt_at_the_deadline`). The real bound was, and remains,
`--timeout` plus one dequeue timeout.

R9 is smaller and worth naming for the same reason: `type -` on a terminal read stdin to EOF with no
announcement, which is indistinguishable from a hang. It now says so on stderr.

## Carried forward

- **`key` has no `--layout`, and only `us` exists.** A chord's single-character token resolves
  through `Layout::default()` (D7), so `key :` on a non-US target would be wrong — as would `type`,
  which at least says which layout it assumed. Adding a layout is adding a variant, its table and
  its names; nothing else in the crate knows what a layout is.
- **No mouse step in macros, deliberately.** §12 Stage 4's rule is "justified by demonstrated need",
  and the need here is negative: a blind click on a live desktop can launch or destroy something
  (CLAUDE.md). Neither mouse command byte appears anywhere under `src/cli/` or `src/script/`, and a
  test in `tests/cli_keys.rs` reads every file in both directories to keep it that way.
- **Every command still runs one `inventory`** — the read-only `QUERYCAP` on candidate video nodes
  that `cli/mod.rs` documents as the residual cost — whichever node it actually wants; the one
  exception is `key`/`type`/`macro` under `--dry-run`, which resolve nothing at all. `devices
  --probe` pays for two walks, deliberately, because "everything found" and "the one pair §8 would
  use" are different questions. Stage 2's "discovery runs twice at startup" note still stands for
  the viewer.
- **`tests/viewer_startup.rs` still enumerates the real `/sys`.** It is pre-Stage 3 behaviour;
  `--sysfs-root` was added for the new binary-level tests and was not applied there. Its own paths
  cannot exist, so it opens nothing of the user's, but it is the one binary-level test left that
  reads the developer's desk.
- **`--sysfs-root` is not plumbed into `discovery::reopen`.** It reaches `select_devices` and
  `cli::devices`, so the *first* resolution honours it; the reopeners (`NodeResolver::real`) go
  straight to `/sys`. Nothing in the CLI reopens anything — only the viewer does — so this is a gap
  in the flag's reach rather than a live defect.
- **Under `--sysfs-root`, the recorded desk yields two candidate pairs.** `NoProbe` answers nothing,
  so nothing breaks the tie between the capture node and its metadata sibling (`/dev/video4` and
  `/dev/video5`, both against `/dev/ttyACM1`), and §8 correctly refuses to choose. That is why the
  probe tests in `tests/cli_shape.rs` pass `--serial`: the flag is §8's override and is the honest
  way to name a node on a desk discovery cannot resolve, but it does mean those tests exercise the
  override arm and not the selection arm. The selection arm is covered in-module
  (`cli::devices::tests`) against the fixtures with a `MapProbe`.
- **`capture::handoff`'s `a_consumer_never_sees_an_older_value_after_a_newer_one` flaked once**
  under a parallel full-suite load during this stage, and did not reproduce in five reruns. Recorded
  rather than fixed: one observation is not a diagnosis, and a test that is timing-sensitive under
  load is worth knowing about before it is worth changing.
- **SuperSpeed is still unexercised**, as at Stage 2 exit. D5's reasoning about peered ports is the
  clearest case yet of a rule that matters on a link this desk has not had since Stage 0, and it is
  tested against the `two-dongles` fixture and nothing else.
- The Stage 2 carried-forward list is otherwise unchanged: the bare Super tap (C7), the remaining
  hardware tests that hardcode node names, the escalation reopen's brief wrong wording (C14),
  `max_barriers` (C11), VT-switch keys, and bindgen cross-architecture.
