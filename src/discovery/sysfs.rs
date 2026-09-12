//! Read-only sysfs access behind an injectable interface.
//!
//! Recorded trees let tests cover USB 2.0, SuperSpeed, and ambiguous topologies without
//! depending on the host's devices.

use std::fs;
use std::path::{Path, PathBuf};

/// Read-only sysfs operations. Missing attributes and dangling links return `None`
/// because hotplug can remove a device during the walk.
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
    /// `device` link that leaves the recorded subset is the "device vanished" case, and
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
