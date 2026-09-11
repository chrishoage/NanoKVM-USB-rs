//! Fakes for driving [`super::discover`] against a recorded sysfs tree (§9.3).
//!
//! These are compiled into the library, not behind `#[cfg(test)]`, for the same reason
//! [`crate::serial::fake`] and [`crate::input::testing`] are: the tests that matter here are
//! integration tests in `tests/`, and an integration test links the library like any other
//! consumer would.
//!
//! Both fakes are *decorators over real fixture data* rather than hand-built tables. §9.1's rule
//! — the recording is the authority, never a retyped description of it — applies to sysfs
//! snapshots exactly as it applies to packet fixtures. [`OverrideSysfs`] therefore expresses "the
//! desk, but with the internal hub's id changed" as a one-attribute override on the real
//! snapshot, so a test cannot accidentally pass because a hand-typed tree left out the attribute
//! that decides the case.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, Once};

use super::probe::NodeProbe;
use super::sysfs::{RealSysfs, Sysfs};

/// `fixtures/sysfs/`: one committed recording (`usb2-desk.sysfs`), one script, and the trees
/// built from them.
fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/sysfs")
}

/// A recorded sysfs tree from `fixtures/sysfs/`.
///
/// The trees themselves are **not committed** — 1300-odd single-line files is not what git is
/// for. What is committed is `fixtures/sysfs/usb2-desk.sysfs`, the recording serialised to one
/// line-oriented file, plus `fixtures/sysfs/synthesize.py`, which expands it and derives the
/// other three trees from it. A tree that is missing is therefore not a broken checkout: it is a
/// fresh one, and this materialises it on first use. **python3 is a test-time dependency of this
/// crate** for that reason.
///
/// # Panics
///
/// Panics if the tree is still missing after materialisation, or if `python3` is unavailable or
/// the script fails — naming the command to run by hand, because a fixture that cannot be built
/// is one broken thing to report, not one failed assertion per test.
pub fn fixture(name: &str) -> RealSysfs {
    let root = fixtures_dir().join(name);
    if !root.is_dir() {
        materialize(&root);
    }
    assert!(
        root.is_dir(),
        "missing sysfs fixture {name}: {} — run `python3 fixtures/sysfs/synthesize.py`; \
         see fixtures/sysfs/MANIFEST.md",
        root.display()
    );
    RealSysfs::new(root)
}

/// Build the fixture trees, at most once per process and one process at a time.
///
/// The [`Once`] is the cheap half: within a process every later `fixture()` call reuses the one
/// build. The lock file is the half that matters, because `cargo test` runs several test
/// binaries and each is its own process. `synthesize.py` publishes each tree by renaming it into
/// place, so the worst a lost race could do is make a reader traverse a tree that is being
/// replaced by an identical one — but holding the lock and re-checking means the second process
/// normally finds the tree already there and does not rebuild at all.
fn materialize(wanted: &Path) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = fixtures_dir();
        let script = dir.join("synthesize.py");
        let lock_path = dir.join(".synthesize.lock");
        let lock = File::create(&lock_path).unwrap_or_else(|e| {
            panic!("cannot create {}: {e}", lock_path.display());
        });
        // SAFETY: `lock` owns the descriptor and outlives the call; `flock` only blocks.
        let locked = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0;
        if locked && wanted.is_dir() {
            return; // another test binary built it while this one waited.
        }
        let out = Command::new("python3")
            .arg(&script)
            .output()
            .unwrap_or_else(|e| {
                panic!(
                    "sysfs fixtures are missing and python3 could not be run ({e}). \
                     Build them by hand with: python3 {}",
                    script.display()
                )
            });
        assert!(
            out.status.success(),
            "python3 {} failed ({}):\n{}{}",
            script.display(),
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    });
}

/// A [`Sysfs`] that answers from an inner tree except where an attribute has been overridden.
///
/// `Some(value)` replaces the attribute, `None` removes it. Nothing else about the tree changes,
/// so a test that mutates one `idProduct` is testing precisely the id check and nothing else.
pub struct OverrideSysfs<S: Sysfs> {
    inner: S,
    attrs: HashMap<(PathBuf, String), Option<String>>,
}

impl<S: Sysfs> OverrideSysfs<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            attrs: HashMap::new(),
        }
    }

    /// Replace one attribute. `dir` is the sysfs-absolute device directory.
    pub fn set(mut self, dir: impl Into<PathBuf>, name: &str, value: &str) -> Self {
        self.attrs
            .insert((dir.into(), name.to_string()), Some(value.to_string()));
        self
    }

    /// Remove one attribute.
    pub fn remove(mut self, dir: impl Into<PathBuf>, name: &str) -> Self {
        self.attrs.insert((dir.into(), name.to_string()), None);
        self
    }
}

impl<S: Sysfs> Sysfs for OverrideSysfs<S> {
    fn read_attr(&self, dir: &Path, name: &str) -> Option<String> {
        if let Some(v) = self.attrs.get(&(dir.to_path_buf(), name.to_string())) {
            return v.clone();
        }
        self.inner.read_attr(dir, name)
    }

    fn read_link(&self, path: &Path) -> Option<PathBuf> {
        self.inner.read_link(path)
    }

    fn list_dir(&self, dir: &Path) -> Vec<PathBuf> {
        self.inner.list_dir(dir)
    }

    fn exists(&self, path: &Path) -> bool {
        // An override that removes an attribute must also make it stop existing, or the walk up
        // to the USB device would still stop at a device with no readable `idVendor`.
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if let Some(parent) = path.parent() {
                if let Some(v) = self.attrs.get(&(parent.to_path_buf(), name.to_string())) {
                    return v.is_some();
                }
            }
        }
        self.inner.exists(path)
    }
}

/// A [`NodeProbe`] that answers from a table and records every node it was asked about.
///
/// The recording is the point: the probe opens a device node, so "which nodes did discovery
/// open?" is a behaviour worth asserting on, not an implementation detail.
pub struct MapProbe {
    answers: HashMap<PathBuf, Result<bool, i32>>,
    default: Option<Result<bool, i32>>,
    asked: Mutex<Vec<PathBuf>>,
}

impl Default for MapProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl MapProbe {
    pub fn new() -> Self {
        Self {
            answers: HashMap::new(),
            default: None,
            asked: Mutex::new(Vec::new()),
        }
    }

    /// `dev` is a `/dev/video*` path.
    pub fn capture(mut self, dev: impl Into<PathBuf>, is_capture: bool) -> Self {
        self.answers.insert(dev.into(), Ok(is_capture));
        self
    }

    /// Make one node fail with a raw errno, e.g. `libc::EACCES`.
    pub fn failing(mut self, dev: impl Into<PathBuf>, errno: i32) -> Self {
        self.answers.insert(dev.into(), Err(errno));
        self
    }

    /// The answer for any node not named. Without one, an unnamed node is a test bug and panics.
    pub fn default_capture(mut self, is_capture: bool) -> Self {
        self.default = Some(Ok(is_capture));
        self
    }

    /// Every node the probe was asked about, in call order.
    pub fn asked(&self) -> Vec<PathBuf> {
        self.asked.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl NodeProbe for MapProbe {
    fn is_capture_node(&self, dev: &Path) -> Result<bool, io::Error> {
        self.asked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(dev.to_path_buf());
        let answer = self
            .answers
            .get(dev)
            .copied()
            .or(self.default)
            .unwrap_or_else(|| panic!("MapProbe has no answer for {}", dev.display()));
        answer.map_err(io::Error::from_raw_os_error)
    }
}
