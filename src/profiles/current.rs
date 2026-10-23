//! The current settings: `current.toml` next to `config.toml`. The daemon,
//! the settings app and the CLI use only this file, and every change is
//! written only to it. A profile is a template: choosing one copies it into
//! `current.toml`, and `config.toml` remembers which one that was.

use std::path::PathBuf;

use rust_i18n::t;

use crate::config::{Config, ConfigError};

use super::builtin::{builtin, builtin_text};
use super::migrate::free_backup_path;
use super::names::{BUILTIN, validate_name};
use super::store::{Store, write};

/// File of the current settings, next to `config.toml`.
pub(super) const CURRENT_FILE: &str = "current.toml";

/// Makes `name` the active profile and copies it into the current settings.
/// `Ok(true)` if it changed, `Ok(false)` if it already was active (nothing is
/// copied, so changes not saved in the profile stay).
pub fn switch_to(store: &Store, name: &str) -> Result<bool, String> {
    if name == store.active() {
        return Ok(false);
    }
    store.select(name)?;
    Ok(true)
}

impl Store {
    /// `current.toml`: the settings in use.
    pub fn current_path(&self) -> PathBuf {
        self.dir().join(CURRENT_FILE)
    }

    /// File text of the profile `name` (the built-in one with the comments
    /// of the example file). Checked to be valid settings.
    fn profile_text(&self, name: &str) -> Result<String, String> {
        if name == BUILTIN {
            return Ok(builtin_text());
        }
        let path = self.profile_path(name);
        let text = std::fs::read_to_string(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => t!("profiles.not_found", name = name).to_string(),
            _ => ConfigError::Read { path: path.clone(), source: e }.to_string(),
        })?;
        Config::parse(&text, &path).map_err(|e| e.to_string())?;
        Ok(text)
    }

    /// Text of the current settings, checked to be valid.
    fn current_text(&self) -> Result<String, String> {
        self.prepare().map_err(|e| e.to_string())?;
        let path = self.current_path();
        let text = std::fs::read_to_string(&path)
            .map_err(|source| ConfigError::Read { path: path.clone(), source }.to_string())?;
        Config::parse(&text, &path).map_err(|e| e.to_string())?;
        Ok(text)
    }

    /// Creates `current.toml` from the active profile if it is missing (the
    /// first start after an update, or a fresh install). The text is copied
    /// as it is: an invalid profile gives the same error as before.
    pub(super) fn ensure_current(&self) -> Result<(), ConfigError> {
        let path = self.current_path();
        if path.exists() {
            return Ok(());
        }
        let name = self.active();
        let text = if name == BUILTIN {
            builtin_text()
        } else {
            let from = self.profile_path(&name);
            std::fs::read_to_string(&from).map_err(|source| ConfigError::Read { path: from, source })?
        };
        write(&path, &text)?;
        log::info!("current settings created from profile \"{name}\"");
        Ok(())
    }

    /// Name of the profile the current settings came from, and the current
    /// settings.
    pub fn load_current(&self) -> Result<(String, Config), ConfigError> {
        self.prepare()?;
        let name = self.active();
        let path = self.current_path();
        let text = std::fs::read_to_string(&path)
            .map_err(|source| ConfigError::Read { path: path.clone(), source })?;
        Ok((name, Config::parse(&text, &path)?))
    }

    /// Applies `edit` to the current settings, keeping comments, key order
    /// and formatting (only keys that changed are touched).
    pub fn update_current(&self, edit: impl FnOnce(&mut Config)) -> Result<(), String> {
        self.prepare().map_err(|e| e.to_string())?;
        Config::update_file(&self.current_path(), edit)
    }

    /// True if the current settings differ from the active profile (or
    /// cannot be read, so they cannot be shown to match).
    pub fn is_modified(&self) -> bool {
        let Ok((name, current)) = self.load_current() else {
            return true;
        };
        let profile = if name == BUILTIN { Ok(builtin()) } else { self.load(&name) };
        !profile.is_ok_and(|p| p == current)
    }

    /// Makes `name` the active profile and copies it into the current
    /// settings. An invalid profile changes nothing, and neither does a
    /// failed write: `config.toml` is written first and set back if the
    /// copy cannot be written.
    pub fn select(&self, name: &str) -> Result<(), String> {
        let text = self.profile_text(name)?;
        // Creates the files first, so the copy below is not overwritten.
        self.prepare().map_err(|e| e.to_string())?;
        let old = self.recorded_profile();
        self.set_active(name)?;
        if let Err(e) = write(&self.current_path(), &text) {
            if let Err(undo) = self.set_main_key("profile", old.as_deref()) {
                log::error!("profile name in config.toml not set back after a failed copy: {undo}");
            }
            return Err(e.to_string());
        }
        Ok(())
    }

    /// The `profile` value written in `config.toml` (unchecked, unlike
    /// [`active`](Self::active)), so it can be set back exactly.
    fn recorded_profile(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.main_path()).ok()?;
        let main: toml::Table = text.parse().ok()?;
        main.get("profile")?.as_str().map(str::to_string)
    }

    /// Copies the active profile into the current settings again, dropping
    /// the changes not saved in it.
    pub fn revert(&self) -> Result<(), String> {
        self.select(&self.active())
    }

    /// Saves the current settings into the active profile (not the built-in
    /// one, which is read-only).
    pub fn save_to_profile(&self) -> Result<(), String> {
        let name = self.active();
        if name == BUILTIN {
            return Err(t!("profiles.read_only").to_string());
        }
        let text = self.current_text()?;
        write(&self.profile_path(&name), &text).map_err(|e| e.to_string())
    }

    /// Saves the current settings as the new profile `name`, which becomes
    /// active. Returns the cleaned-up name.
    pub fn save_as_new(&self, name: &str) -> Result<String, String> {
        let name = validate_name(name)?;
        if self.exists(&name) {
            return Err(t!("profiles.name_taken", name = name).to_string());
        }
        let text = self.current_text()?;
        write(&self.profile_path(&name), &text).map_err(|e| e.to_string())?;
        self.set_active(&name)?;
        Ok(name)
    }

    /// For current settings that cannot be used: keeps a copy of the file
    /// (`current.toml.bak`, `.bak.2`, ...) and copies the active profile
    /// into it. Returns the copy's path. An unreadable `config.toml` or an
    /// invalid profile changes nothing.
    pub fn recover_current(&self) -> Result<PathBuf, String> {
        self.prepare().map_err(|e| e.to_string())?;
        let text = self.profile_text(&self.active())?;
        let path = self.current_path();
        let backup = free_backup_path(&path);
        std::fs::copy(&path, &backup).map_err(|e| e.to_string())?;
        write(&path, &text).map_err(|e| e.to_string())?;
        Ok(backup)
    }
}
