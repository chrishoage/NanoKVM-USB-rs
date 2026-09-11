//! A child process that cannot outlive the scope that spawned it.
//!
//! `00-common.md`: "Kill every process you start. Leave nothing running when you finish." That is
//! not automatic — `std::process::Child` says in its own documentation that it does **not** kill
//! on drop — and a test asserts its way to the `wait()` at the bottom through a dozen `assert!`s,
//! any one of which unwinds straight past it. For `tests/capture_recovery_hardware.rs`'s H-B2 the
//! child is `scripts/usb-replug.py`, a script that resets USB devices; leaving one of those
//! running unsupervised because an assertion about the *method* failed is exactly the outcome the
//! rule exists to prevent, and it is the most likely failure that test has.
//!
//! So every child in this test suite is spawned through [`ChildGuard`], which kills and reaps in
//! its `Drop` — the same shape `tests/serial_hardware.rs` uses for its release-all guard.
//!
//! This module is included by more than one test binary (`#[path = "support/child_guard.rs"] mod
//! child_guard;`), so each binary uses only part of it; `#![allow(dead_code)]` keeps that from
//! becoming a warning.

#![allow(dead_code)]

use std::process::{Child, Command, ExitStatus};

/// A spawned child process, killed and reaped when this value is dropped.
///
/// Dropping after [`ChildGuard::wait`] does nothing: the child has already been reaped and its
/// pid must not be signalled again.
pub struct ChildGuard {
    child: Option<Child>,
    label: String,
}

impl ChildGuard {
    /// Spawn `cmd`, labelled for the message printed if it has to be killed.
    pub fn spawn(label: &str, cmd: &mut Command) -> std::io::Result<ChildGuard> {
        Ok(ChildGuard {
            child: Some(cmd.spawn()?),
            label: label.to_string(),
        })
    }

    /// The child's pid, or `None` once it has been waited for.
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }

    /// Take the child's piped stdout, for a caller that wants to read the script's timeline as it
    /// happens rather than after the fact.
    ///
    /// Only meaningful if the `Command` was given `Stdio::piped()`. The reader **must** be drained
    /// to EOF (or dropped) before [`ChildGuard::wait`], or a script that fills the pipe buffer
    /// blocks on its own `print` and never exits.
    pub fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.as_mut().and_then(|c| c.stdout.take())
    }

    /// Take the child's piped stdin, for a caller that feeds the process its input.
    ///
    /// Taking it is also how the child is told there is no more: the pipe is closed when the
    /// returned handle is dropped, and a child reading to EOF waits for exactly that.
    pub fn take_stdin(&mut self) -> Option<std::process::ChildStdin> {
        self.child.as_mut().and_then(|c| c.stdin.take())
    }

    /// Take the child's piped stderr, for a caller that wants what the process said about itself.
    ///
    /// The same caveat as [`ChildGuard::take_stdout`]: a child that fills the pipe buffer blocks
    /// on its own `eprintln` until something drains it.
    pub fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.child.as_mut().and_then(|c| c.stderr.take())
    }

    /// Wait up to `limit` for the child to exit on its own, polling rather than blocking.
    ///
    /// `None` means it is still running and the guard still owns it, so a caller that gives up
    /// here still kills it on the way out. This is the bounded form every out-of-process test
    /// needs: [`ChildGuard::wait`] blocks for as long as the child cares to run, which turns a
    /// hang in the binary under test into a hang in the test suite.
    pub fn wait_bounded(
        &mut self,
        limit: std::time::Duration,
    ) -> std::io::Result<Option<ExitStatus>> {
        let deadline = std::time::Instant::now() + limit;
        loop {
            let Some(child) = self.child.as_mut() else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{} was already waited for", self.label),
                ));
            };
            if let Some(status) = child.try_wait()? {
                self.child = None;
                return Ok(Some(status));
            }
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Wait for the child to exit on its own, giving up ownership of it: after this the guard has
    /// nothing left to kill.
    pub fn wait(&mut self) -> std::io::Result<ExitStatus> {
        match self.child.take() {
            Some(mut child) => child.wait(),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{} was already waited for", self.label),
            )),
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let pid = child.id();
        // Both results are deliberately ignored: the child may have exited between the kill and
        // the wait, and a `Drop` that panicked while unwinding would abort the process — taking
        // the failure message the test was about to print with it.
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("child guard: killed and reaped {} (pid {pid})", self.label);
    }
}
