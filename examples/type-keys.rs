//! `type-keys` — a scripted keyboard sender, for driving the target from a shell.
//!
//! A Stage 2 measurement instrument. The §6.1 S2-2 run needs a terminal opened on the target and a
//! command typed into it, and until Stage 3 ships the real `type` subcommand the only way to do
//! that is the interactive viewer. This is the throwaway that fills the gap; it is held to the
//! same safety rules as everything else that touches the target, and it will be deleted when
//! Stage 3 lands.
//!
//! ```text
//! cargo run --example type-keys -- --serial /dev/ttyACM1 \
//!     chord:ctrl+alt+t 'text:sudo reboot' key:enter
//! ```
//!
//! # What it is, and is not
//!
//! **Key forwarding against a declared layout, not text injection** (§10.2). `text:` resolves each
//! character to a physical key plus a shift bit through a hard-coded **US QWERTY** table, and the
//! HID usage then comes from [`nanokvm::proto::keymap`] — the same table the viewer forwards
//! through, so there is one authority for usages and this file retypes none of them. A target on
//! another layout receives different characters. That is the documented limit of the instrument,
//! not something to paper over here: `--layout` and the unreachable-character policy belong to
//! Stage 3's CLI, per §10.2.
//!
//! **There is no mouse in this binary.** Not a click, not the idle report, not a mouse half of the
//! release. A click on a live desktop can launch or destroy something, so the code that would
//! build one does not exist in this file. Neither mouse command byte is named anywhere in it or
//! in `type_keys/script.rs`, and a test in `tests/type_keys.rs` greps both to keep it that way.
//!
//! # The safety properties, and where each comes from
//!
//! - **Compile the whole script, then send** (§2.8 item 3). An unknown key name or a character the
//!   layout cannot produce is an error before the port is opened, so a script is either fully
//!   typeable or not attempted. A half-typed `sudo reboot` on a live console is exactly the
//!   partially delivered sequence §2.8 forbids.
//! - **Release-all on every exit path**, including panic and any of SIGINT, SIGTERM and SIGHUP,
//!   with the `Drop` guard of `tests/serial_hardware.rs` (§2.6). A harness timeout sends SIGTERM
//!   and a closed terminal sends SIGHUP, so treating only SIGINT specially would have left a key
//!   held for the caller this instrument was written for. Its outcome is printed rather than
//!   swallowed: an unsent release means the target may still be holding a key (§2.6.1).
//! - **Blocking and paced** (§2.9). Every report is an acknowledged `transact`; a device error or
//!   timeout aborts the script and names how far it got.
//! - **An acknowledgement is not evidence of effect** (§3.4, A17). What this prints is acks. The
//!   consequence is on the target's screen, and checking it is the operator's job.
//!
//! The 40 ms default pacing is for the *target*, not the chip: a keyboard ack round trip measured
//! 4.16–4.19 ms (STAGE1_FINDINGS §"hardware numbers", A11), so the chip could take reports ten
//! times faster. Desktops drop keys delivered faster than a human types them.
//!
//! # Where the code lives, and why it is split in two
//!
//! An example's own `#[cfg(test)]` tests are built and run only by `cargo test --examples`, never
//! by the plain `cargo test` that `CLAUDE.md` names as the gate — so tests written inside this
//! file would silently stop being run by the thing that is supposed to run them. Everything below
//! `main` therefore lives in `examples/type_keys/script.rs`, which is not a Cargo target of its
//! own but is `#[path]`-included by two crates:
//!
//! - this file, as `mod script`, which is what makes `cargo run --example type-keys` work;
//! - `tests/type_keys.rs`, an ordinary integration test, which is what makes `cargo test` with no
//!   flags run all fourteen tests for this instrument.
//!
//! That test also drives the *built binary* against `serial::fake::FakeCh9329` on a pty, which is
//! how the `GET_INFO` gate, the `Drop` guard and the signal path are covered end to end without
//! hardware. `cargo test` builds examples, so the binary those tests exec is there by the time
//! they run; if you run `cargo test --test type_keys` on its own, `cargo build --examples` first.
//!
//! What stays here is only `main`: parse, compile, and then the open/gate/guard/run sequence,
//! which is the one ordering the safety argument depends on and is worth reading in one piece.

#[path = "type_keys/script.rs"]
mod script;

use anyhow::{bail, Context, Result};
use clap::Parser as ClapParser;
use std::time::Duration;

use nanokvm::serial::SerialLink;

use script::{compile, dry_run, install_signal_handlers, run, Args, Released, TIMEOUT};

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    // Before anything is opened, let alone sent.
    let steps = compile(&args.script)?;

    if args.dry_run {
        return dry_run(&steps);
    }

    // SIGINT, SIGTERM and SIGHUP all become the same flag, and the flag becomes an abort plus the
    // guard's release-all. Installed before the port is opened, so no signal can arrive at a
    // moment when a key is held and the handler is not yet in place.
    install_signal_handlers();

    let mut link = SerialLink::open(&args.serial).with_context(|| {
        format!(
            "opening the CH9329 serial link on {}. Check the path and that you can write to it.",
            args.serial.display()
        )
    })?;

    // GET_INFO first: it is the one thing that proves a CH9329 is there and answering (Appendix),
    // and `target_connected` is the difference between typing into a console and typing into
    // nothing. Checked before the guard exists, so refusing to run sends no frame at all.
    let info = link
        .get_info(TIMEOUT)
        .with_context(|| format!("GET_INFO on {} did not answer", args.serial.display()))?;
    println!(
        "CH9329 firmware {:.1}, target {}, locks: num={} caps={} scroll={}",
        info.version,
        if info.target_connected {
            "connected"
        } else {
            "NOT connected"
        },
        info.num_lock,
        info.caps_lock,
        info.scroll_lock
    );
    if !info.target_connected {
        bail!(
            "the device reports no target on its HID side: the keystrokes would go nowhere. \
             Refusing to run."
        );
    }

    let mut link = Released(link);
    let delay = Duration::from_millis(args.delay_ms);
    let sent = run(&mut link, &steps, delay)?;
    println!("delivered {sent} reports; the effect is on the target's screen, not in this ack");
    Ok(())
}
