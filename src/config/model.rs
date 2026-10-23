//! Types of one settings profile (the shape of the TOML file).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors with the file path and (for syntax errors) line information.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("cannot write {path}: {source}")]
    Write { path: PathBuf, source: std::io::Error },
    #[error("invalid config {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("invalid value in {path}: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error("cannot determine home directory (HOME is not set)")]
    NoHome,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmartShift {
    /// 1-255.
    pub threshold: Option<u8>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Wheel {
    pub hires: Option<bool>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Thumbwheel {
    /// One of [`ThumbMode`]; missing = "scroll".
    pub mode: Option<String>,
    pub invert: Option<bool>,
    /// Horizontal scroll speed in percent, 1-1000 (default [`DEFAULT_SPEED`](super::DEFAULT_SPEED)).
    pub speed: Option<u16>,
}

/// Thumb wheel modes: horizontal scrolling (the default), or zoom, volume
/// and tab switching in steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThumbMode {
    Scroll,
    Zoom,
    Volume,
    Tabs,
}

impl ThumbMode {
    /// In display order.
    pub const ALL: [ThumbMode; 4] =
        [ThumbMode::Scroll, ThumbMode::Zoom, ThumbMode::Volume, ThumbMode::Tabs];

    /// The name used in the config file and in locale keys.
    pub fn name(self) -> &'static str {
        match self {
            ThumbMode::Scroll => "scroll",
            ThumbMode::Zoom => "zoom",
            ThumbMode::Volume => "volume",
            ThumbMode::Tabs => "tabs",
        }
    }

    pub fn from_name(name: &str) -> Option<ThumbMode> {
        ThumbMode::ALL.into_iter().find(|m| m.name() == name)
    }

    /// Comma-separated names, for messages.
    pub fn names() -> String {
        ThumbMode::ALL.map(ThumbMode::name).join(", ")
    }
}


/// System-wide scroll tweaks for mouse wheels (trackpads are never touched).
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scroll {
    /// "natural" or "standard": the mouse wheel direction, independent of the trackpad.
    pub direction: Option<String>,
    /// Wheel speed in percent, 10-1000 (default 100).
    pub speed: Option<u16>,
    /// Smoothing in percent, 0-90 (default 0 = off).
    pub smoothing: Option<u8>,
}

/// Actions of one gesture button: a click without movement or a move in one
/// of four directions.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GestureSet {
    /// Minimal movement counted as a gesture, 1-10000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub click: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub up: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub down: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right: Option<String>,
}

impl GestureSet {
    /// The factory actions: windows and desktops (as Logi Options+). Also
    /// used by a button set to `gestures` that has no section of its own.
    pub fn standard() -> GestureSet {
        let action = |name: &str| Some(name.to_string());
        GestureSet {
            threshold: None,
            click: action("mission_control"),
            up: action("mission_control"),
            down: action("app_expose"),
            left: action("desktop_left"),
            right: action("desktop_right"),
        }
    }

    /// Slot names in display order (also the config keys and locale suffixes).
    pub const SLOTS: [&'static str; 5] = ["click", "up", "down", "left", "right"];

    fn slot(&self, slot: &str) -> Option<&Option<String>> {
        Some(match slot {
            "click" => &self.click,
            "up" => &self.up,
            "down" => &self.down,
            "left" => &self.left,
            "right" => &self.right,
            _ => return None,
        })
    }

    /// The action of `slot`; `None` if unset or not a slot.
    pub fn get(&self, slot: &str) -> Option<&str> {
        self.slot(slot)?.as_deref()
    }

    /// Mutable access to `slot`; `None` if not a slot.
    pub fn slot_mut(&mut self, slot: &str) -> Option<&mut Option<String>> {
        Some(match slot {
            "click" => &mut self.click,
            "up" => &mut self.up,
            "down" => &mut self.down,
            "left" => &mut self.left,
            "right" => &mut self.right,
            _ => return None,
        })
    }

    /// `(slot, action)` pairs in display order.
    pub fn slots(&self) -> impl Iterator<Item = (&'static str, &Option<String>)> {
        GestureSet::SLOTS.into_iter().filter_map(|s| Some((s, self.slot(s)?)))
    }
}

/// `[gestures]`: the thumb gesture button's actions, plus one
/// `[gestures.<button>]` section for every other button set to `gestures`.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gestures {
    /// The thumb gesture button (keys directly in `[gestures]`). Must stay
    /// before `buttons`: it takes its keys first, the rest are sections.
    #[serde(flatten)]
    pub thumb: GestureSet,
    /// Button key (see `devices`) -> its own set (`[gestures.middle]`).
    #[serde(flatten)]
    pub buttons: BTreeMap<String, GestureSet>,
}

/// Key of the thumb gesture button (its set is `[gestures]` itself).
pub const THUMB_GESTURE_BUTTON: &str = "gesture";

impl Gestures {
    /// Actions of the gesture button `key`: the thumb button uses
    /// `[gestures]`, any other button its section or [`GestureSet::standard`].
    pub fn for_button(&self, key: &str) -> GestureSet {
        if key == THUMB_GESTURE_BUTTON {
            return self.thumb.clone();
        }
        self.buttons.get(key).cloned().unwrap_or_else(GestureSet::standard)
    }

    /// Editable set of `key`; a missing section starts from the standard set.
    pub fn for_button_mut(&mut self, key: &str) -> &mut GestureSet {
        if key == THUMB_GESTURE_BUTTON {
            return &mut self.thumb;
        }
        self.buttons.entry(key.to_string()).or_insert_with(GestureSet::standard)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub dpi: Option<u16>,
    #[serde(default)]
    pub smartshift: SmartShift,
    #[serde(default)]
    pub wheel: Wheel,
    #[serde(default)]
    pub thumbwheel: Thumbwheel,
    #[serde(default)]
    pub scroll: Scroll,
    /// Button key (see `devices`) -> action name.
    #[serde(default)]
    pub buttons: BTreeMap<String, String>,
    #[serde(default)]
    pub gestures: Gestures,
    /// Per-application overrides, keyed by bundle id (`[apps."com.apple.Safari"]`).
    #[serde(default)]
    pub apps: BTreeMap<String, AppOverride>,
}

/// Overrides for one application: only what is given replaces the global
/// `[buttons]` / `[gestures]` entries.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppOverride {
    #[serde(default)]
    pub buttons: BTreeMap<String, String>,
    #[serde(default)]
    pub gestures: Gestures,
}
