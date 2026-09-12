//! The per-user chrome settings, in `$XDG_CONFIG_HOME/nanokvm/config.toml` (plan §12 Stage 4b).
//!
//! §12 Stage 4b: *"Persist the reference's per-user choices (menu open, mouse mode, cursor, wheel
//! direction, mute, layout)… Missing file means the defaults; a malformed file is an error naming
//! the key, not a silent reset."* Both halves are enforced here:
//!
//! - **Missing file → defaults.** [`Store::load`] answers `Ok(Config::default())` for `ENOENT`
//!   and for nothing else. A file that is there but unreadable is an error, because "we could not
//!   read your settings" and "you have no settings" are different facts.
//! - **Malformed → an error naming the key.** `toml` 0.9's own `Display` already prints the file
//!   position, the offending source line, a caret and the key name; [`ConfigError`] prefixes the
//!   path and nothing else, because rewording it would lose the caret.
//!
//! # Two deliberate choices about strictness
//!
//! `#[serde(deny_unknown_fields)]` — a typo'd key is refused by name (`unknown field
//! 'wheel_direciton', expected one of …`) rather than silently ignored, which is the failure the
//! plan's sentence is about. The price is that the config is **not forward compatible**: a config
//! written by a newer build is refused by an older one, naming the key it does not know. That is
//! the right trade here — the alternative is the silent reset the plan forbids — but it is a real
//! constraint on adding keys, and it is why every key below has a default.
//!
//! `#[serde(default)]` — a *missing* key takes its default rather than being an error. A
//! hand-written file containing one line is a legitimate thing to write, and nothing is being
//! reset: the key was never there. Only `deny_unknown_fields` is load-bearing for the plan's
//! sentence, and the two are independent.
//!
//! # Nothing here reads the real config directory in a test
//!
//! [`Store::at`] takes the configuration root explicitly and every test uses it against a
//! temporary directory. [`Store::discover`] is the only thing that consults the environment, it is
//! called exactly once from `main`, and it has no test of its own that could write to a real
//! `~/.config`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file, under the configuration root.
const RELATIVE_PATH: &str = "nanokvm/config.toml";

/// How the pointer is delivered, as the config file spells it.
///
/// A separate enum from [`crate::viewer::PointerMode`] on purpose: this one is a file format and
/// has to keep its spelling across versions, while the other is free to be refactored. The
/// conversions below are the only place the two meet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseMode {
    /// [`crate::viewer::PointerMode::Absolute`], the default (§3.4).
    #[default]
    Absolute,
    /// [`crate::viewer::PointerMode::Relative`].
    Relative,
}

impl From<MouseMode> for crate::viewer::PointerMode {
    fn from(m: MouseMode) -> Self {
        match m {
            MouseMode::Absolute => crate::viewer::PointerMode::Absolute,
            MouseMode::Relative => crate::viewer::PointerMode::Relative,
        }
    }
}

impl From<crate::viewer::PointerMode> for MouseMode {
    fn from(m: crate::viewer::PointerMode) -> Self {
        match m {
            crate::viewer::PointerMode::Absolute => MouseMode::Absolute,
            crate::viewer::PointerMode::Relative => MouseMode::Relative,
        }
    }
}

/// Which way a wheel detent is sent to the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WheelDirection {
    /// Pass the host's sign through. winit's positive means "content moves down", and the HID
    /// wheel field uses the same convention, so this is a no-op (`input_map`'s `WheelAccumulator`
    /// has the verification).
    #[default]
    Natural,
    /// Negate it, for a target whose own scrolling is configured the other way.
    Inverted,
}

impl WheelDirection {
    /// The multiplier to apply to a wheel delta.
    pub fn sign(self) -> i32 {
        match self {
            WheelDirection::Natural => 1,
            WheelDirection::Inverted => -1,
        }
    }
}

/// The declared **target** keyboard layout, as the config file spells it (§10.2, §12 Stage 4c).
///
/// A separate enum from [`crate::script::Layout`] for the same reason [`MouseMode`] is separate
/// from [`crate::viewer::PointerMode`]: this one is a file format and has to keep its spelling
/// across versions, while the other is free to be refactored. The conversion below is the only
/// place the two meet, and it is total — adding a layout to `script` is a compile error here until
/// this file is told about it, which is the point.
///
/// Only US QWERTY exists, in `script` and therefore here. It is a *setting* rather than a
/// constant because §10.2's rule is that the layout is **declared and stated, never assumed**:
/// the tooltip on the Paste item names it, the refusal names it, and the file is where a user
/// would look to change it once there is a second one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PasteLayout {
    /// [`crate::script::Layout::Us`], the default stated in the tooltip rather than assumed.
    #[default]
    Us,
}

impl From<PasteLayout> for crate::script::Layout {
    fn from(l: PasteLayout) -> Self {
        match l {
            PasteLayout::Us => crate::script::Layout::Us,
        }
    }
}

/// The per-user choices the chrome remembers.
///
/// Every field has a default, so a partial file is legal and a fresh install needs no file at all.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// The pill is expanded rather than collapsed to its grip.
    pub menu_open: bool,
    /// Absolute or relative pointer.
    pub mouse_mode: MouseMode,
    /// Hide the host cursor over the video, so only the target's own cursor is visible.
    pub cursor_hidden: bool,
    /// Wheel direction.
    pub wheel_direction: WheelDirection,
    /// Audio output muted (4a's `mute` flag; the chrome owns the toggle).
    pub audio_muted: bool,
    /// The pill's top-left corner in egui points — the "layout" the plan's list asks for. Stored
    /// in points rather than pixels so the pill lands in the same visual place on a differently
    /// scaled output.
    pub pill_x: f32,
    /// See [`Config::pill_x`].
    pub pill_y: f32,
    /// The declared target keyboard layout a clipboard paste is compiled against (§10.2, §12
    /// Stage 4c).
    ///
    /// §12 Stage 4b read the plan's *"persist … (menu open, mouse mode, cursor, wheel direction,
    /// mute, layout)"* as the pill's position, which is what the two `pill_*` fields above are.
    /// §12 Stage 4c needs the other reading of the same word — the keyboard layout — so both are
    /// here under names that say which is which.
    pub layout: PasteLayout,
}

impl Default for Config {
    /// The reference's defaults where it has one, and this client's where it does not: the pill
    /// open (the reference shows its bar), absolute pointer (§3.4 — relative cannot reach a
    /// specific pixel), cursor shown, natural wheel, audio on (4a: "default on, like the
    /// reference"), and the pill in the top-left inset by a comfortable margin.
    fn default() -> Self {
        Config {
            menu_open: true,
            mouse_mode: MouseMode::Absolute,
            cursor_hidden: false,
            wheel_direction: WheelDirection::Natural,
            audio_muted: false,
            pill_x: 16.0,
            pill_y: 16.0,
            layout: PasteLayout::Us,
        }
    }
}

/// Why the settings could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file is there but does not parse. The inner `Display` carries the line, the column,
    /// the offending source line and the key — see the module docs.
    #[error("{path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },
    /// The file is there but could not be read, or the directory could not be created, or the
    /// write failed.
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The settings could not be rendered as TOML. Structurally impossible for [`Config`] as it
    /// stands — every field is a scalar — but serialising is fallible and swallowing the error
    /// would be a silent failure to save.
    #[error("rendering the settings as TOML: {source}")]
    Serialize {
        #[source]
        source: toml::ser::Error,
    },
}

/// Reads and writes one settings file.
#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    /// A store under an explicit configuration **root** — the directory `$XDG_CONFIG_HOME` names,
    /// not the file. The file is `<root>/nanokvm/config.toml`.
    ///
    /// This is the constructor every test uses, against a temporary directory.
    pub fn at(root: &Path) -> Store {
        Store {
            path: root.join(RELATIVE_PATH),
        }
    }

    /// The store the XDG basedir rules name: `$XDG_CONFIG_HOME` if it is set and absolute,
    /// otherwise `$HOME/.config`.
    ///
    /// `None` when neither is usable — a process with no `HOME` and no `XDG_CONFIG_HOME` has
    /// nowhere to put a settings file, and inventing a path under the working directory would
    /// scatter dotfiles wherever the user happened to be. The chrome runs with defaults and
    /// remembers nothing, which is the honest degradation.
    ///
    /// The basedir spec says a relative `$XDG_CONFIG_HOME` must be ignored, and it is: a relative
    /// one would resolve against the working directory.
    pub fn discover() -> Option<Store> {
        let root = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(v) if !v.is_empty() && Path::new(&v).is_absolute() => PathBuf::from(v),
            _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
        };
        if !root.is_absolute() {
            return None;
        }
        Some(Store::at(&root))
    }

    /// The file this store reads and writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the settings.
    ///
    /// A file that is not there is [`Config::default`] — that is the plan's "missing file means
    /// the defaults" and it is the only error kind treated that way.
    pub fn load(&self) -> Result<Config, ConfigError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(source) => {
                return Err(ConfigError::Io {
                    path: self.path.clone(),
                    source,
                })
            }
        };
        toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: self.path.clone(),
            source: Box::new(source),
        })
    }

    /// The temporary file [`Store::save`] writes before renaming it over [`Store::path`].
    fn tmp_path(&self) -> PathBuf {
        let mut name = self.path.as_os_str().to_os_string();
        name.push(".tmp");
        PathBuf::from(name)
    }

    /// Write the settings, creating `<root>/nanokvm/` if it is not there.
    ///
    /// Written whole rather than merged: [`Config`] is the entire file, so a write cannot lose a
    /// key it did not know about — `deny_unknown_fields` has already refused any such file at
    /// load time rather than letting it reach here.
    ///
    /// **Written to `config.toml.tmp` and renamed over the target**, never truncated in place. A
    /// `write` that is interrupted — the process is killed, the filesystem is full — leaves a
    /// half-written file, and a half-written file is precisely the *malformed* file this module is
    /// required to refuse loudly on the next run (§12 Stage 4b). A `rename` within one directory
    /// is atomic, so the file a reader sees is always one whole config or the previous one. The
    /// temporary is removed on a failed write rather than left beside the settings.
    pub fn save(&self, config: &Config) -> Result<(), ConfigError> {
        let text =
            toml::to_string_pretty(config).map_err(|source| ConfigError::Serialize { source })?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| ConfigError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        let tmp = self.tmp_path();
        if let Err(source) = std::fs::write(&tmp, text) {
            return Err(ConfigError::Io { path: tmp, source });
        }
        if let Err(source) = std::fs::rename(&tmp, &self.path) {
            // The old file is still whole; the temporary is not worth keeping.
            let _ = std::fs::remove_file(&tmp);
            return Err(ConfigError::Io {
                path: self.path.clone(),
                source,
            });
        }
        Ok(())
    }
}

/// Write `config` when it differs from `saved`, and advance `saved` **only if the write worked**
/// (§12 Stage 4b: "write on change, not on every frame"; review item 3).
///
/// Advancing first made a transient failure permanent: the settings were marked as written, the
/// write had failed, and nothing retried until the user changed something else. Now a failure
/// leaves `saved` where it was, so the very next build tries again.
///
/// `store` is `None` when there is nowhere to write — a process with no `HOME` and no
/// `XDG_CONFIG_HOME`. Then `saved` advances anyway: nothing is being remembered, and leaving it
/// behind would only log a warning on every frame about a file that cannot exist.
///
/// A failure is logged, never returned: losing the memory of which popover was open is not a
/// reason to take a window away from someone.
pub fn persist(store: Option<&Store>, config: &Config, saved: &mut Config) {
    if config == saved {
        return;
    }
    let Some(store) = store else {
        *saved = *config;
        return;
    };
    match store.save(config) {
        Ok(()) => *saved = *config,
        Err(e) => log::warn!("could not save the chrome settings: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A unique temporary directory, removed by [`TempRoot`]'s `Drop`.
    ///
    /// Hand-rolled rather than a `tempfile` dependency: three lines against a crate in the tree
    /// forever, for tests that would otherwise be the only user of it.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(tag: &str) -> TempRoot {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "nanokvm-chrome-config-{}-{tag}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&dir).expect("temp dir");
            TempRoot(dir)
        }

        fn store(&self) -> Store {
            Store::at(&self.0)
        }

        fn write(&self, text: &str) {
            let store = self.store();
            std::fs::create_dir_all(store.path().parent().expect("parent")).expect("mkdir");
            std::fs::write(store.path(), text).expect("write");
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_missing_file_is_the_defaults() {
        let root = TempRoot::new("missing");
        let store = root.store();
        assert!(!store.path().exists());
        assert_eq!(store.load().expect("defaults"), Config::default());
    }

    #[test]
    fn settings_round_trip() {
        let root = TempRoot::new("roundtrip");
        let store = root.store();
        let config = Config {
            menu_open: false,
            mouse_mode: MouseMode::Relative,
            cursor_hidden: true,
            wheel_direction: WheelDirection::Inverted,
            audio_muted: true,
            pill_x: 640.5,
            pill_y: 12.25,
            layout: PasteLayout::Us,
        };
        store.save(&config).expect("save");
        assert_eq!(store.load().expect("load"), config);
        // And the file is the one the plan names, under the root it was given.
        assert!(store.path().ends_with("nanokvm/config.toml"));
    }

    /// The plan's sentence: a malformed file is an error naming the key, not a silent reset.
    #[test]
    fn a_typod_key_is_refused_by_name() {
        let root = TempRoot::new("typo");
        root.write("menu_open = true\nwheel_direciton = \"inverted\"\n");
        let err = root.store().load().expect_err("a typo must not be ignored");
        let text = err.to_string();
        assert!(text.contains("wheel_direciton"), "{text}");
        assert!(text.contains("unknown field"), "{text}");
        // And it names the file, so the user knows which one to fix.
        assert!(text.contains("config.toml"), "{text}");
    }

    /// A wrong type names the key and the position. `toml`'s own `Display` does the work; the test
    /// pins that we do not throw it away.
    #[test]
    fn a_wrong_type_is_an_error_with_a_line_and_column() {
        let root = TempRoot::new("wrongtype");
        root.write("menu_open = \"yes\"\n");
        let err = root.store().load().expect_err("a string is not a bool");
        let text = err.to_string();
        assert!(text.contains("line 1"), "{text}");
        assert!(text.contains("column"), "{text}");
        assert!(text.contains("menu_open"), "{text}");
    }

    /// A key this build does not know is named. This is also the forward-compatibility cost the
    /// module docs record: it is a test so the trade cannot be forgotten.
    #[test]
    fn a_key_from_a_newer_build_is_named_rather_than_ignored() {
        let root = TempRoot::new("newer");
        root.write("menu_open = true\nosk_layout = \"de\"\n");
        let text = root.store().load().expect_err("unknown key").to_string();
        assert!(text.contains("osk_layout"), "{text}");
    }

    /// The §12 Stage 4c key: the declared target layout, spelt as the file spells it, and
    /// defaulting to the one §10.2 states rather than assumes.
    #[test]
    fn the_paste_layout_is_a_setting_with_a_stated_default() {
        assert_eq!(Config::default().layout, PasteLayout::Us);
        assert_eq!(
            crate::script::Layout::from(PasteLayout::Us),
            crate::script::Layout::Us,
            "the file's spelling and the compiler's layout must be the same layout"
        );
        let root = TempRoot::new("layout");
        root.write("layout = \"us\"\n");
        assert_eq!(
            root.store().load().expect("us is a layout").layout,
            PasteLayout::Us
        );
        // A layout this build does not have is refused by name rather than silently defaulted —
        // typing text against the wrong layout is the failure §10.2 exists to prevent.
        let root = TempRoot::new("layout-unknown");
        root.write("layout = \"de\"\n");
        let text = root
            .store()
            .load()
            .expect_err("de is not a layout")
            .to_string();
        assert!(text.contains("layout"), "{text}");
        assert!(text.contains("de"), "{text}");
    }

    /// A partial file is legal: the keys that are there are honoured and the rest default. This is
    /// `serde(default)`, and it is independent of `deny_unknown_fields` above.
    #[test]
    fn a_partial_file_takes_defaults_for_what_it_omits() {
        let root = TempRoot::new("partial");
        root.write("mouse_mode = \"relative\"\n");
        let config = root.store().load().expect("a one-line file is legal");
        assert_eq!(config.mouse_mode, MouseMode::Relative);
        assert_eq!(config.menu_open, Config::default().menu_open);
        assert_eq!(config.pill_x, Config::default().pill_x);
    }

    /// Saving creates the `nanokvm/` directory under a root that has none.
    #[test]
    fn saving_creates_the_directory() {
        let root = TempRoot::new("mkdir");
        let store = root.store();
        assert!(!store.path().parent().expect("parent").exists());
        store.save(&Config::default()).expect("save");
        assert!(store.path().exists());
    }

    /// The file format's spellings are pinned: they are what a user types by hand and what an
    /// older build has to keep reading.
    #[test]
    fn the_file_format_spells_the_enums_in_lower_case() {
        let text = toml::to_string_pretty(&Config {
            mouse_mode: MouseMode::Relative,
            wheel_direction: WheelDirection::Inverted,
            ..Config::default()
        })
        .expect("serialize");
        assert!(text.contains("mouse_mode = \"relative\""), "{text}");
        assert!(text.contains("wheel_direction = \"inverted\""), "{text}");
    }

    #[test]
    fn the_wheel_direction_is_a_sign() {
        assert_eq!(WheelDirection::Natural.sign(), 1);
        assert_eq!(WheelDirection::Inverted.sign(), -1);
    }

    /// **Review item 11.** The write goes through a temporary and a rename: after a save the
    /// temporary is gone and the file parses. A reader never sees a partial file, because the
    /// file it reads was never partially written — it was renamed over, whole.
    #[test]
    fn a_save_leaves_no_temporary_behind_and_the_file_parses() {
        let root = TempRoot::new("atomic");
        let store = root.store();
        let config = Config {
            mouse_mode: MouseMode::Relative,
            pill_x: 123.5,
            ..Config::default()
        };
        store.save(&config).expect("save");
        assert!(
            !store.tmp_path().exists(),
            "{} was left behind",
            store.tmp_path().display()
        );
        assert_eq!(store.load().expect("load"), config);
    }

    /// The half of item 11 that is the point of it: a save that cannot complete leaves the
    /// **previous** settings whole. The interruption is simulated by making the temporary path a
    /// directory, so creating the temporary file fails; an in-place `write` would have truncated
    /// the real file before discovering the same failure.
    #[test]
    fn an_interrupted_save_leaves_the_previous_file_intact() {
        let root = TempRoot::new("interrupted");
        let store = root.store();
        let first = Config {
            pill_x: 11.0,
            ..Config::default()
        };
        store.save(&first).expect("the first save");

        std::fs::create_dir(store.tmp_path()).expect("block the temporary path");
        let err = store
            .save(&Config {
                pill_x: 22.0,
                ..Config::default()
            })
            .expect_err("the write cannot complete");
        assert!(err.to_string().contains("config.toml"), "{err}");
        assert_eq!(
            store.load().expect("load"),
            first,
            "the settings that were already there must survive a failed save"
        );
    }

    /// **Review item 3.** A failed write must be retried, so `saved` advances only on success.
    /// The store is unusable to begin with — the configuration root is a *file* — and starts
    /// working once that is fixed; the second `persist` call, with the same config, must write.
    #[test]
    fn a_failed_save_is_retried_rather_than_remembered_as_written() {
        let root = TempRoot::new("retry");
        let store = root.store();
        // `<root>/nanokvm` is a file, so `create_dir_all` for the parent fails.
        std::fs::write(store.path().parent().expect("parent"), b"in the way").expect("write");

        let wanted = Config {
            menu_open: false,
            pill_x: 42.0,
            ..Config::default()
        };
        let mut saved = Config::default();
        persist(Some(&store), &wanted, &mut saved);
        assert_eq!(
            saved,
            Config::default(),
            "a failed write must not be remembered as written"
        );

        std::fs::remove_file(store.path().parent().expect("parent")).expect("unblock");
        persist(Some(&store), &wanted, &mut saved);
        assert_eq!(saved, wanted, "the retry must happen");
        assert_eq!(store.load().expect("load"), wanted);
    }

    /// With nowhere to write, `persist` is a no-op that does not warn on every frame.
    #[test]
    fn persist_without_a_store_advances_quietly() {
        let wanted = Config {
            pill_y: 99.0,
            ..Config::default()
        };
        let mut saved = Config::default();
        persist(None, &wanted, &mut saved);
        assert_eq!(saved, wanted);
    }

    /// The two pointer enums convert both ways without losing a variant.
    #[test]
    fn the_pointer_modes_convert_both_ways() {
        for m in [MouseMode::Absolute, MouseMode::Relative] {
            assert_eq!(MouseMode::from(crate::viewer::PointerMode::from(m)), m);
        }
    }
}
