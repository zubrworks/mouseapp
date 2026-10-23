//! The settings folder: `config.toml`, `current.toml` and the `profiles` folder.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use rust_i18n::t;
use toml_edit::DocumentMut;

use crate::config::{Config, ConfigError};

use super::builtin::builtin;
use super::current::CURRENT_FILE;
use super::names::{BUILTIN, validate_name};

const MAIN_FILE: &str = "config.toml";
const PROFILES_DIR: &str = "profiles";
const EXTENSION: &str = "toml";

/// The settings folder: `config.toml`, `current.toml` and the `profiles` folder.
#[derive(Clone, Debug)]
pub struct Store {
    dir: PathBuf,
    /// Missing active profile already reported in the log (shared by clones),
    /// so the warning is not repeated until the state changes.
    warned: Arc<Mutex<Option<String>>>,
}

/// Modification times of the files that decide the profile state:
/// `config.toml`, `current.toml` and the active profile.
pub type Stamp = [Option<SystemTime>; 3];

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Store {
        Store { dir: dir.into(), warned: Arc::default() }
    }

    /// `~/Library/Application Support/Mouse`.
    pub fn open() -> Result<Store, ConfigError> {
        let dir = crate::paths::support_dir().ok_or(ConfigError::NoHome)?;
        Ok(Store::new(dir))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `config.toml`: language and the profile the current settings came from.
    pub fn main_path(&self) -> PathBuf {
        self.dir.join(MAIN_FILE)
    }

    pub(super) fn profiles_dir(&self) -> PathBuf {
        self.dir.join(PROFILES_DIR)
    }

    /// File of the user profile `name`.
    pub fn profile_path(&self, name: &str) -> PathBuf {
        self.profiles_dir().join(format!("{name}.{EXTENSION}"))
    }

    /// True for `config.toml` and `current.toml` (what the daemon has to
    /// reload after; profiles are only templates).
    /// Only names are compared: the watcher may report the folder by
    /// another path (symbolic links).
    pub fn is_settings_file(&self, path: &Path) -> bool {
        path.file_name().is_some_and(|n| n == MAIN_FILE || n == CURRENT_FILE)
            && path.parent().and_then(Path::file_name) == self.dir.file_name()
    }

    /// The file with the settings in use, shown by "Show settings file".
    pub fn settings_file(&self) -> PathBuf {
        self.current_path()
    }

    /// The profile named in `config.toml`, even if its file is missing
    /// (then [`active`](Self::active) falls back to the built-in one).
    pub fn configured_profile(&self) -> String {
        let main = self.read_main();
        main.get("profile").and_then(|v| v.as_str()).unwrap_or(BUILTIN).to_string()
    }

    /// Modification times of the files read by `active` and `is_modified`,
    /// with the profile file named in `config.toml` (`configured`, see
    /// [`configured_profile`](Self::configured_profile)), so a missing file
    /// that comes back is noticed too; a different value means the files
    /// have to be read again. The built-in profile has no file.
    pub fn stamp(&self, configured: &str) -> Stamp {
        let time = |p: PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        [
            time(self.main_path()),
            time(self.current_path()),
            (configured != BUILTIN).then(|| time(self.profile_path(configured))).flatten(),
        ]
    }

    /// `config.toml` as a TOML table (empty if missing or unreadable).
    fn read_main(&self) -> toml::Table {
        std::fs::read_to_string(self.main_path())
            .ok()
            .and_then(|t| t.parse::<toml::Table>().ok())
            .unwrap_or_default()
    }

    /// The `language` setting, if set.
    pub fn language(&self) -> Option<String> {
        self.read_main()
            .get("language")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|l| !l.is_empty() && l.len() <= 16)
            .map(str::to_string)
    }

    /// Sets the UI language (`None` = follow the system), keeping comments.
    pub fn set_language(&self, lang: Option<&str>) -> Result<(), ConfigError> {
        self.set_main_key("language", lang)
    }

    /// Name of the active profile (the one the current settings came from):
    /// [`BUILTIN`] when none is set or its file is missing.
    pub fn active(&self) -> String {
        let main = self.read_main();
        let Some(name) = main.get("profile").and_then(|v| v.as_str()) else {
            return BUILTIN.to_string();
        };
        if name == BUILTIN {
            return name.to_string();
        }
        let mut warned = self.warned.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if validate_name(name).is_ok() && self.profile_path(name).is_file() {
            *warned = None;
            return name.to_string();
        }
        if warned.as_deref() != Some(name) {
            log::warn!("profile \"{name}\" not found; using the built-in profile");
            *warned = Some(name.to_string());
        }
        BUILTIN.to_string()
    }

    /// Records `name` as the active profile (without copying it into the
    /// current settings, see [`select`](Self::select)).
    pub(super) fn set_active(&self, name: &str) -> Result<(), String> {
        if name != BUILTIN && !self.profile_path(name).is_file() {
            return Err(t!("profiles.not_found", name = name).to_string());
        }
        self.set_main_key("profile", Some(name)).map_err(|e| e.to_string())
    }

    /// Replaces or removes a top-level string key of `config.toml`, keeping
    /// its comments and the rest of the text.
    pub(super) fn set_main_key(&self, key: &str, value: Option<&str>) -> Result<(), ConfigError> {
        self.prepare()?;
        let path = self.main_path();
        let text = std::fs::read_to_string(&path)
            .map_err(|source| ConfigError::Read { path: path.clone(), source })?;
        let mut doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
            ConfigError::Parse { path: path.clone(), message: e.to_string() }
        })?;
        match value {
            Some(v) => {
                let mut new = toml_edit::value(v);
                // Keep the comment after the old value.
                if let (Some(old), Some(item)) = (doc.get(key).and_then(|i| i.as_value()), new.as_value_mut()) {
                    *item.decor_mut() = old.decor().clone();
                }
                doc[key] = new;
            }
            None => remove_key_keeping_comments(&mut doc, key),
        }
        write(&path, &doc.to_string())
    }

    /// User profiles, sorted by name (the built-in one is not included).
    pub fn list(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.profiles_dir()) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == EXTENSION))
            .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(str::to_string))
            .filter(|n| validate_name(n).as_deref() == Ok(n.as_str()))
            .collect();
        names.sort_by_key(|n| n.to_lowercase());
        names
    }

    /// Every profile for a chooser: the built-in one first.
    pub fn all(&self) -> Vec<String> {
        std::iter::once(BUILTIN.to_string()).chain(self.list()).collect()
    }

    /// Settings of the profile `name`.
    pub fn load(&self, name: &str) -> Result<Config, ConfigError> {
        if name == BUILTIN {
            return Ok(builtin());
        }
        let path = self.profile_path(name);
        let text = std::fs::read_to_string(&path)
            .map_err(|source| ConfigError::Read { path: path.clone(), source })?;
        Config::parse(&text, &path)
    }

    /// Is `name` taken by a profile (case-insensitive, like the file system)?
    pub(super) fn exists(&self, name: &str) -> bool {
        let lower = name.to_lowercase();
        self.list().iter().any(|n| n.to_lowercase() == lower) || self.profile_path(name).exists()
    }

    /// `base`, or `base 2`, `base 3`, ... - the first free valid name.
    pub fn unique_name(&self, base: &str) -> String {
        let base = base.trim();
        (1..)
            .map(|n| if n == 1 { base.to_string() } else { format!("{base} {n}") })
            .find(|n| validate_name(n).is_ok() && !self.exists(n))
            .unwrap_or_else(|| base.to_string())
    }

    /// Renames a user profile (also in `config.toml` if it is active).
    pub fn rename(&self, old: &str, new: &str) -> Result<String, String> {
        if old == BUILTIN {
            return Err(t!("profiles.read_only").to_string());
        }
        let new = validate_name(new)?;
        // A change of letter case only is allowed (same file).
        if new.to_lowercase() != old.to_lowercase() && self.exists(&new) {
            return Err(t!("profiles.name_taken", name = new).to_string());
        }
        let was_active = self.active() == old;
        std::fs::rename(self.profile_path(old), self.profile_path(&new))
            .map_err(|e| e.to_string())?;
        if was_active {
            self.set_active(&new)?;
        }
        Ok(new)
    }

    /// Deletes a user profile; if it was active, the built-in one is chosen
    /// (and copied into the current settings).
    pub fn delete(&self, name: &str) -> Result<(), String> {
        if name == BUILTIN {
            return Err(t!("profiles.read_only").to_string());
        }
        let was_active = self.active() == name;
        std::fs::remove_file(self.profile_path(name)).map_err(|e| e.to_string())?;
        if was_active {
            self.select(BUILTIN)?;
        }
        Ok(())
    }
}

/// Removes a top-level key; the comment lines above it move to the next
/// entry, so a header comment of the file is not lost.
pub(super) fn remove_key_keeping_comments(doc: &mut DocumentMut, key: &str) {
    let prefix = doc
        .as_table()
        .key(key)
        .and_then(|k| k.leaf_decor().prefix().cloned())
        .and_then(|p| p.as_str().map(str::to_string))
        .unwrap_or_default();
    doc.remove(key);
    if prefix.trim().is_empty() {
        return;
    }
    let Some(next) = doc.as_table().iter().next().map(|(k, _)| k.to_string()) else {
        return;
    };
    let table = doc.as_table_mut();
    if table.get(&next).is_some_and(toml_edit::Item::is_table) {
        let decor = table.get_mut(&next).and_then(toml_edit::Item::as_table_mut).map(|t| t.decor_mut());
        if let Some(decor) = decor {
            let rest = decor.prefix().and_then(|p| p.as_str()).unwrap_or_default().to_string();
            decor.set_prefix(prefix + &rest);
        }
    } else if let Some(mut k) = table.key_mut(&next) {
        let rest = k.leaf_decor().prefix().and_then(|p| p.as_str()).unwrap_or_default().to_string();
        k.leaf_decor_mut().set_prefix(prefix + &rest);
    }
}

/// Writes `text` to `path`, creating its folder.
pub(super) fn write(path: &Path, text: &str) -> Result<(), ConfigError> {
    crate::fsutil::write_atomic(path, text)
        .map_err(|source| ConfigError::Write { path: path.to_path_buf(), source })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("mouse-store-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(PROFILES_DIR)).unwrap();
        Store::new(dir)
    }

    #[test]
    fn missing_active_profile_is_reported_once() {
        let s = temp("warned");
        std::fs::write(s.main_path(), "profile = \"gone\"\n").unwrap();
        assert_eq!(s.warned.lock().unwrap().as_deref(), None);
        assert_eq!(s.active(), BUILTIN);
        assert_eq!(s.warned.lock().unwrap().as_deref(), Some("gone"));
        // Again: still the same state, nothing new to report.
        assert_eq!(s.active(), BUILTIN);
        assert_eq!(s.warned.lock().unwrap().as_deref(), Some("gone"));
        // The profile appears: the state is cleared, so a later loss is reported.
        std::fs::write(s.profile_path("gone"), "").unwrap();
        assert_eq!(s.active(), "gone");
        assert_eq!(s.warned.lock().unwrap().as_deref(), None);
        std::fs::remove_file(s.profile_path("gone")).unwrap();
        assert_eq!(s.active(), BUILTIN);
        assert_eq!(s.warned.lock().unwrap().as_deref(), Some("gone"));
        let _ = std::fs::remove_dir_all(s.dir());
    }

    #[test]
    fn stamp_changes_with_each_watched_file() {
        let s = temp("stamp");
        std::fs::write(s.main_path(), "").unwrap();
        std::fs::write(s.profile_path("a"), "").unwrap();
        let first = s.stamp("a");
        assert!(first[0].is_some() && first[1].is_none() && first[2].is_some());
        assert_eq!(s.stamp("a"), first);
        // A profile file that is missing and comes back changes the stamp.
        let missing = s.stamp("b");
        assert_eq!(missing[2], None);
        std::fs::write(s.profile_path("b"), "").unwrap();
        assert_ne!(s.stamp("b"), missing);
        std::fs::write(s.main_path(), "profile = \"b\"\n").unwrap();
        assert_eq!(s.configured_profile(), "b");
        std::fs::remove_file(s.main_path()).unwrap();
        assert_eq!(s.configured_profile(), BUILTIN);
        std::fs::write(s.main_path(), "").unwrap();
        let first = s.stamp("a");
        // The built-in profile has no file to watch.
        assert_eq!(s.stamp(BUILTIN)[2], None);
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(s.current_path(), "").unwrap();
        assert_ne!(s.stamp("a"), first);
        let second = s.stamp("a");
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(s.profile_path("a"), "x").unwrap();
        assert_ne!(s.stamp("a"), second);
        let _ = std::fs::remove_dir_all(s.dir());
    }
}
