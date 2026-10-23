//! Selects the UI language from the CLI flag, config or system settings.

/// Fallback language, always complete.
const DEFAULT_LANG: &str = "en";

/// Picks the language: `--lang` flag > config > macOS system language > English.
pub fn init(flag: Option<&str>) {
    let lang = flag
        .and_then(normalize)
        .or_else(|| config_language().as_deref().and_then(normalize));
    choose(lang);
}

/// Switches the language at runtime: `lang` (a config value, `None` = follow
/// the system) > macOS system language > English.
pub fn apply(lang: Option<&str>) {
    choose(lang.and_then(normalize));
}

/// Language codes with a translation file, sorted.
pub fn available() -> Vec<String> {
    let mut v: Vec<String> = rust_i18n::available_locales!()
        .iter()
        .map(|l| l.to_string())
        .collect();
    v.sort();
    v
}

/// Base code (`pl`) of `tag` if a translation exists for it.
pub fn supported(tag: &str) -> Option<String> {
    normalize(tag).filter(|l| available().iter().any(|a| a == l))
}

/// Sets `preferred` (or else the system language) if translated, else English.
fn choose(preferred: Option<String>) {
    let available = available();
    let chosen = preferred
        .or_else(system_language)
        .filter(|l| available.iter().any(|a| a == l))
        .unwrap_or_else(|| DEFAULT_LANG.to_string());
    rust_i18n::set_locale(&chosen);
}

/// The `language` setting from `config.toml`, if set.
fn config_language() -> Option<String> {
    crate::profiles::Store::open().ok()?.language()
}

/// Reduces tags like `pl_PL.UTF-8` or `pl-PL` to the base code `pl`.
fn normalize(tag: &str) -> Option<String> {
    let base = tag
        .split(['_', '-', '.', '@'])
        .next()?
        .trim()
        .to_lowercase();
    if base.is_empty() || base == "c" || base == "posix" {
        None
    } else {
        Some(base)
    }
}

/// Reads the preferred language from macOS (falls back to LANG).
fn system_language() -> Option<String> {
    let first = crate::sysprefs::global_strings("AppleLanguages")
        .and_then(|langs| langs.first().and_then(|l| normalize(l)));
    first.or_else(|| std::env::var("LANG").ok().as_deref().and_then(normalize))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_tags() {
        assert_eq!(normalize("pl_PL.UTF-8").as_deref(), Some("pl"));
        assert_eq!(normalize("en-US").as_deref(), Some("en"));
        assert_eq!(normalize("C"), None);
    }
}
