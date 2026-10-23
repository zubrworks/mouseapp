//! Dialogs: error alerts and the shortcut prompt.

use objc2::MainThreadMarker;
use rust_i18n::t;

use super::profiles;
use crate::actions;

/// Shows an error to the user in a dialog.
pub fn report_error(mtm: MainThreadMarker, message: &str) {
    profiles::show_error(mtm, &t!("settings.error_title"), message);
}

/// Dialog asking for a shortcut such as `cmd+shift+4`; returns the config
/// value (`shortcut:<combo>`) or `None` if cancelled or invalid.
pub(super) fn ask_shortcut(mtm: MainThreadMarker) -> Option<String> {
    let text = profiles::prompt_text(
        mtm,
        &t!("settings.shortcut.title"),
        &t!("settings.shortcut.hint"),
        (&t!("common.ok"), &t!("common.cancel")),
        "",
        Some("cmd+shift+4"),
    )?;
    let value = format!("shortcut:{}", text.trim().to_lowercase());
    match actions::parse(&value) {
        Ok(_) => Some(value),
        Err(e) => {
            report_error(mtm, &format!("{e:#}"));
            None
        }
    }
}
