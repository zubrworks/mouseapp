//! Reading and writing profile files.

use std::path::Path;

use super::edit::edit_text;
use super::model::{Config, ConfigError};

/// Commented example of a profile file; also the template of new profiles.
pub const EXAMPLE: &str = include_str!("../../config.example.toml");

impl Config {
    /// Parses and validates TOML text. `path` is used only in messages.
    pub fn parse(text: &str, path: &Path) -> Result<Config, ConfigError> {
        let cfg: Config = toml::from_str(text).map_err(|e| ConfigError::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        cfg.validate().map_err(|message| ConfigError::Invalid {
            path: path.to_path_buf(),
            message,
        })?;
        Ok(cfg)
    }

    /// Reads the file (the example if missing), applies `edit`, validates and
    /// writes it back keeping comments, key order and formatting: only keys
    /// that changed are touched.
    pub fn update_file(path: &Path, edit: impl FnOnce(&mut Config)) -> Result<(), String> {
        let text = crate::fsutil::read_or_template(path).map_err(|e| e.to_string())?;
        let out = edit_text(&text, path, edit)?;
        crate::fsutil::write_atomic(path, out).map_err(|e| e.to_string())
    }

    /// Reads the file without creating it; `None` if missing or invalid.
    #[cfg(test)]
    pub fn read_existing(path: &Path) -> Option<Config> {
        let text = std::fs::read_to_string(path).ok()?;
        Config::parse(&text, path).ok()
    }
}
