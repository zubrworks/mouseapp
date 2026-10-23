//! Profile names: the built-in name, display names and name checks.

use rust_i18n::t;

/// Name of the built-in profile in `config.toml` (`profile = "default"`).
pub const BUILTIN: &str = "default";
/// Longest profile name, in characters.
pub(super) const MAX_NAME: usize = 64;

/// Name for the title of the active profile: with a mark when the current
/// settings differ from it.
pub fn marked_name(name: &str, modified: bool) -> String {
    if modified {
        t!("profiles.modified", name = display_name(name)).to_string()
    } else {
        display_name(name)
    }
}

/// Name shown to the user: the built-in profile is translated.
pub fn display_name(name: &str) -> String {
    if name == BUILTIN {
        t!("profiles.builtin").to_string()
    } else {
        name.to_string()
    }
}

/// Checks a profile name typed by the user; returns it trimmed, or a
/// message saying what is wrong.
pub fn validate_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(t!("profiles.name_empty").to_string());
    }
    if name.chars().count() > MAX_NAME {
        return Err(t!("profiles.name_long", max = MAX_NAME).to_string());
    }
    if name.starts_with('.') || name.chars().any(|c| c.is_control() || "/\\:".contains(c)) {
        return Err(t!("profiles.name_chars").to_string());
    }
    // The built-in profile in any language, so the menu never shows two.
    let lower = name.to_lowercase();
    let reserved = std::iter::once(BUILTIN.to_string()).chain(
        crate::i18n::available()
            .into_iter()
            .map(|l| t!("profiles.builtin", locale = l.as_str()).to_lowercase()),
    );
    if reserved.into_iter().any(|r| r == lower) {
        return Err(t!("profiles.name_reserved", name = name).to_string());
    }
    Ok(name.to_string())
}
