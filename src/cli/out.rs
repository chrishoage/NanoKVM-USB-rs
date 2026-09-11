//! Printing that survives a closed pipe.
//!
//! `nanokvm key capslock | head -1` used to end in `failed printing to stdout: Broken pipe`: the
//! `println!` macros panic on any write error, and `head` closing the pipe is an ordinary thing
//! for a user to do. The panic itself was not the worst of it — a panic unwinds through
//! [`keys::Released`](crate::cli::keys), so the release-all still went out — but a CLI that
//! aborts because its reader left is a CLI that cannot be piped.
//!
//! **The fix is not `SIG_DFL` for `SIGPIPE`.** Restoring the default disposition would kill the
//! process at the write, past the release-all guard, leaving a key held on a live console —
//! exactly what §2.6 and CLAUDE.md forbid. So the signal stays ignored (as Rust's runtime sets it)
//! and the `EPIPE` it turns into is handled here: a broken pipe means *stop printing and keep
//! going*, and the command still runs to its own end, releases, and exits on its own terms.
//!
//! The one place a broken stdout is a real error is `shot -`, where stdout **is** the output: a
//! JPEG that did not get through is a failed screenshot, not a quiet one. That path writes
//! directly and keeps its `Err` (see [`shot::ShotTarget::write`](crate::cli::shot::ShotTarget)).

use std::io::{ErrorKind, Write};
use std::sync::atomic::{AtomicBool, Ordering};

/// Set once a stream has gone away, so the rest of the run stops writing to it rather than
/// retrying a write per line.
static STDOUT_GONE: AtomicBool = AtomicBool::new(false);
static STDERR_GONE: AtomicBool = AtomicBool::new(false);

/// One line on stdout, or nothing at all if the reader has gone.
pub fn line(text: &str) {
    write_to(&STDOUT_GONE, &mut std::io::stdout().lock(), text, true);
}

/// `text` on stdout with no newline added, for a block that brings its own (a listing, the dry-run
/// table).
pub fn block(text: &str) {
    write_to(&STDOUT_GONE, &mut std::io::stdout().lock(), text, false);
}

/// One line on stderr: a note about the run rather than part of what the run produced.
///
/// Stderr gets the same treatment as stdout because the `Drop` guard's release-all report goes
/// there (§2.6.1), and a panic while unwinding out of a `Drop` aborts the process — taking the
/// message the failure was about to print with it.
pub fn note(text: &str) {
    write_to(&STDERR_GONE, &mut std::io::stderr().lock(), text, true);
}

/// Write, and give up on the stream for good if it has gone away.
///
/// Every error is swallowed, not just `BrokenPipe`: there is by definition nowhere to report a
/// failure to write to the only stream this function has. `BrokenPipe` is the one worth
/// remembering, because it is permanent.
fn write_to(gone: &AtomicBool, out: &mut impl Write, text: &str, newline: bool) {
    if gone.load(Ordering::Relaxed) {
        return;
    }
    let result = if newline {
        writeln!(out, "{text}").and_then(|()| out.flush())
    } else {
        write!(out, "{text}").and_then(|()| out.flush())
    };
    if let Err(e) = result {
        if e.kind() == ErrorKind::BrokenPipe {
            gone.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the whole module exists for: a write that fails is not a panic, and the
    /// stream is not written to again.
    #[test]
    fn a_broken_pipe_stops_the_stream_instead_of_panicking() {
        struct Broken(u32);
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                self.0 += 1;
                Err(std::io::Error::new(ErrorKind::BrokenPipe, "closed"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let gone = AtomicBool::new(false);
        let mut sink = Broken(0);
        write_to(&gone, &mut sink, "first", true);
        assert!(gone.load(Ordering::Relaxed), "the stream was not given up");
        write_to(&gone, &mut sink, "second", true);
        assert_eq!(sink.0, 1, "the stream was written to after it had gone");
    }

    /// An ordinary write still writes, newline and all, and leaves the stream usable.
    #[test]
    fn an_ordinary_write_goes_through_and_the_stream_stays_open() {
        let gone = AtomicBool::new(false);
        let mut buf: Vec<u8> = Vec::new();
        write_to(&gone, &mut buf, "one", true);
        write_to(&gone, &mut buf, "two", false);
        assert_eq!(String::from_utf8(buf).expect("utf-8"), "one\ntwo");
        assert!(!gone.load(Ordering::Relaxed));
    }
}
