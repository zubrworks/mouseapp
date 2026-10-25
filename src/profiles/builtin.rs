//! The built-in read-only "default" profile.

use std::path::Path;

use crate::config::{self, Config, GestureSet, Gestures, Thumbwheel};
use crate::devices;

/// The built-in profile: DPI of the model, SmartShift threshold as the mouse
/// has it, back / forward / middle / top button native (not diverted), the
/// gesture button navigates windows and desktops (with the keys, not like a
/// trackpad), the thumb wheel scrolls horizontally and the wheel scrolling is
/// left to macOS.
pub fn builtin() -> Config {
    let action = |name: &str| Some(name.to_string());
    Config {
        dpi: Some(devices::default_model().default_dpi),
        thumbwheel: Thumbwheel { mode: action("scroll"), ..Thumbwheel::default() },
        gestures: Gestures {
            trackpad_swipe: Some(false),
            thumb: GestureSet::standard(),
            ..Gestures::default()
        },
        ..Config::default()
    }
}

/// TOML text of the built-in profile with the comments of the example file
/// (used when the built-in profile is copied).
pub fn builtin_text() -> String {
    config::edit_text(config::EXAMPLE, Path::new("config.example.toml"), |c| *c = builtin())
        .unwrap_or_else(|_| toml::to_string(&builtin()).unwrap_or_default())
}
