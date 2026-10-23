//! Default values and allowed ranges of the settings.

use std::ops::RangeInclusive;

/// Speed in percent when the config has none (wheel and thumb wheel).
pub const DEFAULT_SPEED: u16 = 100;
/// Default gesture movement threshold when the config has none.
pub const DEFAULT_THRESHOLD: u16 = 50;
/// Allowed ranges, used by the validation and by the settings sliders.
pub const THUMB_SPEED_RANGE: RangeInclusive<u16> = 1..=1000;
pub const SCROLL_SPEED_RANGE: RangeInclusive<u16> = 10..=1000;
pub const GESTURE_THRESHOLD_RANGE: RangeInclusive<u16> = 1..=10_000;

/// `(min, max)` of a range as slider bounds.
pub fn slider_bounds(r: &RangeInclusive<u16>) -> (f64, f64) {
    (f64::from(*r.start()), f64::from(*r.end()))
}
