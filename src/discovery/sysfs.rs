//! The sysfs seam (§8, §7.2).
//!
//! §7.2 fixes the packaging rule that decides this module's shape: **no `libudev`**. Everything
//! §8 needs — vendor and product ids, the device tree, and the kernel's port `peer` assertion —
//! is readable out of plain files and symlinks, so the only dependency here is `std::fs`.
//!
//! [`Sysfs`] exists so the pairing rules in [`super`] can be run against a *recorded* sysfs tree.
//! That is not a testing convenience bolted on afterwards: §8's evidence item 2 (the SuperSpeed
//! `peer` link) and item 3 (containment under the dongle's internal hub) are **two different
//! physical shapes of the same dongle**, and no desk has both at once. Stage 0 measured the
//! first, Stage 1 measured the second (STAGE1_FINDINGS, "Environment"). Recording each into a
//! directory tree that this trait can read unchanged is the only way both rules stay tested.
//! `scripts/snapshot-sysfs.py` records those trees. What is committed is the recording
//! serialised to a single line-oriented file, `fixtures/sysfs/usb2-desk.sysfs`;
//! `fixtures/sysfs/synthesize.py` expands it into `fixtures/sysfs/usb2-desk/` and derives the
//! other three trees from it, and [`super::testing::fixture`] runs that script on demand when a
//! tree is missing.
//!
//! ## Paths are sysfs-absolute, not host-absolute
//!
//! Every path crossing this trait is rooted at the sysfs mount point rather than at `/`:
//! `/class/video4linux/video4`, `/devices/pci0000:00/.../3-2.2.2`, `/bus/usb/devices/3-2`.
//! [`RealSysfs`] prepends its `root` on the way in and strips it on the way out, so the same
//! path values name the same nodes whether `root` is `/sys` or a fixture directory. Callers
//! never see the fixture's location, and a recorded tree stays relocatable.

use std::fs;
use std::path::{Path, PathBuf};

/// Read-only access to the four sysfs operations §8's rules need.
///
/// Implementations return `None` rather than an error for a missing attribute or a dangling
/// link: sysfs races with hotplug, and a device that vanished mid-walk is a normal outcome that
/// must degrade to "not a candidate", never to a failed discovery.
pub trait Sysfs {
    /// One attribute file, with trailing whitespace trimmed. `None` if it does not exist or
    /// cannot be read.
    fn read_attr(&self, dir: &Path, name: &str) -> Option<String>;

    /// Fully resolve `path` (which is normally a symlink) to a sysfs-absolute path. `None` if it
    /// does not exist or points outside the tree.
    fn read_link(&self, path: &Path) -> Option<PathBuf>;

    /// The entries of `dir` as sysfs-absolute paths, sorted by file name. Empty if `dir` is
    /// missing or unreadable. Sorting is what makes discovery deterministic: `readdir` order is
    /// not stable across boots, and an unstable order would make `Ambiguous` listings differ
    /// between runs.
    fn list_dir(&self, dir: &Path) -> Vec<PathBuf>;

    /// Whether `path` exists (following symlinks).
    fn exists(&self, path: &Path) -> bool;
}

/// [`Sysfs`] over a real directory tree: `/sys` in production, a `fixtures/sysfs/*` snapshot in
/// tests.
#[derive(Clone, Debug)]
pub struct RealSysfs {
    root: PathBuf,
}

impl Default for RealSysfs {
    /// The mount point every Linux system has.
    fn default() -> Self {
        Self::new("/sys")
    }
}

impl RealSysfs {
    /// `root` is canonicalised once here so that [`RealSysfs::read_link`] can strip it back off
    /// the canonical form of any path underneath it. Without that, a fixture reached through a
    /// symlinked directory would resolve to a path this type could not recognise as its own.
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        Self {
            root: fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()),
        }
    }

    /// The sysfs mount point (or fixture directory) this instance reads.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// sysfs-absolute -> host path.
    fn host(&self, path: &Path) -> PathBuf {
        self.root.join(path.strip_prefix("/").unwrap_or(path))
    }

    /// host path -> sysfs-absolute, or `None` when the path escaped the tree. A `peer` or
    /// `device` link that leaves the recorded subset is exactly the "device vanished" case, and
    /// dropping it is what makes a partial snapshot safe to read.
    fn sysfs_path(&self, host: &Path) -> Option<PathBuf> {
        host.strip_prefix(&self.root)
            .ok()
            .map(|rest| Path::new("/").join(rest))
    }
}

impl Sysfs for RealSysfs {
    fn read_attr(&self, dir: &Path, name: &str) -> Option<String> {
        let text = fs::read_to_string(self.host(dir).join(name)).ok()?;
        Some(text.trim().to_string())
    }

    fn read_link(&self, path: &Path) -> Option<PathBuf> {
        let target = fs::canonicalize(self.host(path)).ok()?;
        self.sysfs_path(&target)
    }

    fn list_dir(&self, dir: &Path) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(self.host(dir)) else {
            return Vec::new();
        };
        let mut names: Vec<_> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        names.sort();
        names.into_iter().map(|n| dir.join(n)).collect()
    }

    fn exists(&self, path: &Path) -> bool {
        self.host(path).exists()
    }
}
