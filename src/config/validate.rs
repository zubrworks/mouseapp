//! Value checks of a profile.

use std::collections::BTreeMap;
use std::ops::RangeInclusive;

use crate::devices::{self, MODELS};

use super::limits::{GESTURE_THRESHOLD_RANGE, SCROLL_SPEED_RANGE, THUMB_SPEED_RANGE};
use super::model::{Config, GestureSet, Gestures, ThumbMode};

impl Config {
    /// Checks value ranges against the known models.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(dpi) = self.dpi {
            let min = MODELS.iter().map(|m| m.dpi.min).min().unwrap_or(0);
            let max = MODELS.iter().map(|m| m.dpi.max).max().unwrap_or(u16::MAX);
            if dpi < min || dpi > max {
                return Err(format!("dpi = {dpi} is outside {min}-{max}"));
            }
        }
        if self.smartshift.threshold == Some(0) {
            return Err("smartshift.threshold must be 1-255".into());
        }
        if let Some(v) = self.thumbwheel.speed
            && !THUMB_SPEED_RANGE.contains(&v)
        {
            return Err(range_error("thumbwheel.speed", v, &THUMB_SPEED_RANGE));
        }
        if let Some(m) = &self.thumbwheel.mode
            && ThumbMode::from_name(m).is_none()
        {
            return Err(format!(
                "thumbwheel.mode = \"{m}\" must be one of: {}",
                ThumbMode::names()
            ));
        }
        if let Some(d) = &self.scroll.direction
            && d != "natural"
            && d != "standard"
        {
            return Err(format!("scroll.direction = \"{d}\" must be \"natural\" or \"standard\""));
        }
        if let Some(v) = self.scroll.speed
            && !SCROLL_SPEED_RANGE.contains(&v)
        {
            return Err(range_error("scroll.speed", v, &SCROLL_SPEED_RANGE));
        }
        if let Some(v) = self.scroll.smoothing
            && v > 90
        {
            return Err(format!("scroll.smoothing = {v} is outside 0-90"));
        }
        validate_actions("", &self.buttons, &self.gestures)?;
        for (bundle, app) in &self.apps {
            if bundle.trim().is_empty() {
                return Err("empty bundle id in [apps]".into());
            }
            validate_actions(&format!("apps.\"{bundle}\"."), &app.buttons, &app.gestures)?;
        }
        Ok(())
    }
}

/// Checks button names, gesture thresholds and that no action is empty.
/// `prefix` names the table in messages ("" or `apps."id".`).
fn validate_actions(
    prefix: &str,
    buttons: &BTreeMap<String, String>,
    gestures: &Gestures,
) -> Result<(), String> {
    let known = devices::remappable_keys();
    for (button, action) in buttons {
        if !known.contains(&button.as_str()) {
            return Err(format!(
                "unknown button \"{button}\" in [{prefix}buttons] (known: {})",
                known.join(", ")
            ));
        }
        if action.trim().is_empty() {
            return Err(format!("empty action for button \"{button}\""));
        }
    }
    validate_gesture_set(&format!("{prefix}gestures"), &gestures.thumb)?;
    for (button, set) in &gestures.buttons {
        if !known.contains(&button.as_str()) {
            return Err(format!(
                "unknown button \"{button}\" in [{prefix}gestures.{button}] (the thumb button uses [{prefix}gestures] itself)"
            ));
        }
        validate_gesture_set(&format!("{prefix}gestures.{button}"), set)?;
    }
    Ok(())
}

fn range_error(name: &str, v: u16, r: &RangeInclusive<u16>) -> String {
    format!("{name} = {v} is outside {}-{}", r.start(), r.end())
}

/// Checks the threshold range and that no action is empty; `table` names
/// the section in messages.
fn validate_gesture_set(table: &str, set: &GestureSet) -> Result<(), String> {
    if let Some(t) = set.threshold
        && !GESTURE_THRESHOLD_RANGE.contains(&t)
    {
        return Err(range_error(&format!("{table}.threshold"), t, &GESTURE_THRESHOLD_RANGE));
    }
    for (name, action) in set.slots() {
        if action.as_deref().is_some_and(|a| a.trim().is_empty()) {
            return Err(format!("empty action for {table}.{name}"));
        }
    }
    Ok(())
}
