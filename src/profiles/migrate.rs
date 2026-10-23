//! Moving an old single-file `config.toml` into a profile.

use std::path::{Path, PathBuf};

use rust_i18n::t;
use toml_edit::DocumentMut;

use crate::config::{Config, ConfigError};

use super::names::BUILTIN;
use super::store::{Store, write};

/// Name of the profile that receives the settings of an old single-file
/// `config.toml` (localized).
pub(super) fn migrated() -> String {
    t!("profiles.migrated_name").to_string()
}
/// Keys `config.toml` may have; any other key means an old single-file config.
const MAIN_KEYS: [&str; 2] = ["language", "profile"];

const MAIN_HEADER: &str = "\
# Mouse: UI language and the profile the current settings came from.
# The settings in use are in current.toml next to this file; the app and the
# driver change only that file. Profiles are files in the \"profiles\" folder
# (<name>.toml, keys as in config.example.toml); choosing a profile copies it
# into current.toml. \"default\" is the built-in factory profile and cannot
# be changed.
";

/// `config.toml` text for `language` and `profile`.
fn main_text(language: Option<&str>, profile: &str) -> String {
    let quote = |s: &str| toml::Value::String(s.to_string()).to_string();
    let language = match language {
        Some(l) => format!("language = {}", quote(l)),
        None => "# language = \"en\"".to_string(),
    };
    format!(
        "{MAIN_HEADER}\n# UI language code (en, pl, ...). Missing = follow the system language.\n{language}\n\n# Profile the current settings came from: \"{BUILTIN}\" or a file name from \"profiles\" without .toml.\nprofile = {}\n",
        quote(profile)
    )
}

impl Store {
    /// Creates `config.toml` if it is missing, moves the settings of an old
    /// single-file `config.toml` into the profile [`migrated`] (the old file
    /// is kept as `config.toml.bak`) and creates `current.toml` from the
    /// active profile if it is missing. Returns the new profile's name when
    /// settings were moved.
    pub fn prepare(&self) -> Result<Option<String>, ConfigError> {
        let moved = self.prepare_main()?;
        self.ensure_current()?;
        Ok(moved)
    }

    /// The `config.toml` part of [`prepare`](Self::prepare).
    fn prepare_main(&self) -> Result<Option<String>, ConfigError> {
        let path = self.main_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                write(&path, &main_text(None, BUILTIN))?;
                return Ok(None);
            }
            Err(source) => return Err(ConfigError::Read { path, source }),
        };
        let doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
            ConfigError::Parse { path: path.clone(), message: e.to_string() }
        })?;
        if doc.iter().all(|(k, _)| MAIN_KEYS.contains(&k)) {
            return Ok(None);
        }
        self.migrate(doc).map(Some)
    }

    /// Moves everything but the language from `doc` (an old `config.toml`)
    /// into a new profile, keeping the comments, and makes it active.
    fn migrate(&self, mut doc: DocumentMut) -> Result<String, ConfigError> {
        let path = self.main_path();
        let language = doc.get("language").and_then(|i| i.as_str()).map(str::to_string);
        doc.remove("language");
        doc.remove("profile");
        let profile = doc.to_string();
        // An invalid file is not touched: the error is reported as before.
        Config::parse(&profile, &path)?;
        let backup = free_backup_path(&path);
        std::fs::copy(&path, &backup)
            .map_err(|source| ConfigError::Write { path: backup.clone(), source })?;
        let name = self.migration_name(&profile);
        write(&self.profile_path(&name), &profile)?;
        write(&path, &main_text(language.as_deref(), &name))?;
        log::info!(
            "settings moved from {} to profile \"{name}\" (backup: {})",
            path.display(),
            backup.display()
        );
        Ok(name)
    }

    /// [`migrated`], or "<name> 2", ... if a different profile has that name. A
    /// file with the same text is reused (a migration cut short).
    fn migration_name(&self, text: &str) -> String {
        let base = migrated();
        (1..)
            .map(|n| if n == 1 { base.clone() } else { format!("{base} {n}") })
            .find(|name| match std::fs::read_to_string(self.profile_path(name)) {
                Ok(existing) => existing == text,
                Err(_) => !self.profile_path(name).exists(),
            })
            .unwrap_or(base)
    }
}

/// `<file>.bak`, or `<file>.bak.2`, ... when an older backup exists.
pub(super) fn free_backup_path(path: &Path) -> PathBuf {
    let base = format!("{}.bak", path.display());
    (1..)
        .map(|n| PathBuf::from(if n == 1 { base.clone() } else { format!("{base}.{n}") }))
        .find(|p| !p.exists())
        .unwrap_or_else(|| PathBuf::from(base))
}
