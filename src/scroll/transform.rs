//! Scroll value transforms: thumb wheel rotation to pixels, direction, speed
//! and smoothing of mouse wheel events, and wheel events to pixels in
//! trackpad mode.

#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

use crate::config::{DEFAULT_SPEED, Scroll};

/// Converts thumb wheel rotation increments to pixels, carrying the
/// remainder over. `speed` is a percentage (100 = [`PIXELS_PER_TURN`] per
/// full revolution of the wheel).
#[derive(Debug)]
pub struct Scaler {
    rest: f32,
    per_unit: f32,
}

/// Pixels per full revolution of the thumb wheel at 100% speed.
const PIXELS_PER_TURN: f32 = 360.0;

impl Default for Scaler {
    fn default() -> Self {
        Scaler::new(crate::hidpp::features::thumbwheel::DEFAULT_DIVERTED_RES)
    }
}

impl Scaler {
    /// `diverted_res`: rotation increments per revolution (getThumbwheelInfo);
    /// 0 falls back to the MX Master value.
    pub fn new(diverted_res: u16) -> Self {
        let res = crate::hidpp::features::thumbwheel::diverted_res_or_default(diverted_res);
        Scaler {
            rest: 0.0,
            per_unit: PIXELS_PER_TURN / f32::from(res),
        }
    }

    pub fn convert(&mut self, rotation: i16, speed: u16, invert: bool) -> i32 {
        let sign = if invert { -1.0 } else { 1.0 };
        let v = self.rest + sign * f32::from(rotation) * self.per_unit * f32::from(speed) / 100.0;
        let whole = v.trunc();
        self.rest = v - whole;
        whole as i32
    }
}

/// Events further apart than this are not smoothed together.
const SMOOTH_WINDOW_MS: u128 = 120;

/// Active scroll settings, derived from the config.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Settings {
    /// Flip the sign of mouse wheel deltas.
    invert: bool,
    /// Speed factor (1.0 = unchanged).
    factor: f64,
    /// 0.0 = off .. 0.9: share of the previous delta mixed into the next.
    smoothing: f64,
    /// The main wheel scrolls like a trackpad (see `trackpad`).
    trackpad: bool,
}

impl Settings {
    /// `system_natural`: whether macOS "natural scrolling" is on. The system
    /// applies it to mice too, so a mouse wanting the opposite is inverted.
    pub fn from_config(c: &Scroll, system_natural: bool) -> Option<Settings> {
        let invert = match c.direction.as_deref() {
            Some("natural") => !system_natural,
            Some("standard") => system_natural,
            _ => false,
        };
        let speed = c.speed.unwrap_or(DEFAULT_SPEED);
        let trackpad = c.trackpad.unwrap_or(false);
        // The glide of trackpad mode replaces smoothing.
        let smoothing = if trackpad {
            0
        } else {
            c.smoothing.unwrap_or(0)
        };
        if !invert && speed == DEFAULT_SPEED && smoothing == 0 && !trackpad {
            return None;
        }
        Some(Settings {
            invert,
            factor: f64::from(speed) / 100.0,
            smoothing: f64::from(smoothing) / 100.0,
            trackpad,
        })
    }

    pub(super) fn trackpad(&self) -> bool {
        self.trackpad
    }

    /// Pixels of a vertical wheel movement in trackpad mode, with speed and
    /// direction applied. The fixed-point delta (fractional lines, with the
    /// macOS scroll acceleration) leads; without it the line delta, and the
    /// pixel delta last.
    pub(super) fn trackpad_pixels(&self, d: Deltas) -> f64 {
        let lines = if d.fixed != 0.0 {
            d.fixed
        } else if d.lines != 0 {
            d.lines as f64
        } else {
            d.points as f64 / SYSTEM_LINE_PIXELS
        };
        let sign = if self.invert { -1.0 } else { 1.0 };
        lines * TRACKPAD_LINE_PIXELS * self.factor * sign
    }
}

/// Pixels per line in the pixel delta macOS gives a wheel event.
const SYSTEM_LINE_PIXELS: f64 = 10.0;
/// Pixels per wheel line (one slow click) in trackpad mode at 100% speed.
const TRACKPAD_LINE_PIXELS: f64 = 24.0;

/// One axis of a scroll event: line, pixel and fixed-point (fractional) deltas.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(super) struct Deltas {
    lines: i64,
    points: i64,
    fixed: f64,
}

/// Per-axis state: remainders of rounding and the previous output.
#[derive(Debug, Default)]
pub(super) struct Axis {
    rest: f64,
    point_rest: f64,
    prev: f64,
    at: Option<Instant>,
}

/// Rounds `value` to a whole number, carrying the remainder in `rest`.
/// Nothing is lost: a movement below one unit stays in `rest` and comes out
/// with the following events (a total of at least half a unit rounds up).
fn round_carry(rest: &mut f64, value: f64) -> i64 {
    let total = *rest + value;
    let whole = total.round();
    *rest = total - whole;
    whole as i64
}

impl Axis {
    /// Applies invert, speed and smoothing to one delta; the result is fractional.
    fn scale(&mut self, delta: f64, s: &Settings, now: Instant) -> f64 {
        let mut v = delta * s.factor * if s.invert { -1.0 } else { 1.0 };
        let recent = self
            .at
            .is_some_and(|t| now.saturating_duration_since(t).as_millis() < SMOOTH_WINDOW_MS);
        if s.smoothing > 0.0 && recent && self.prev.signum() == v.signum() {
            v = v * (1.0 - s.smoothing) + self.prev * s.smoothing;
        }
        self.prev = v;
        self.at = Some(now);
        v
    }

    /// Transforms all deltas of one event axis with a single common ratio.
    /// High-resolution events carry 0 lines and only fixed-point/pixel deltas,
    /// so the fixed-point delta (fractional) is the leading value when present.
    /// Returns `None` when the axis has no movement.
    pub(super) fn process(&mut self, d: Deltas, s: &Settings, now: Instant) -> Option<Deltas> {
        let lead = if d.fixed != 0.0 {
            d.fixed
        } else if d.lines != 0 {
            d.lines as f64
        } else if d.points != 0 {
            d.points as f64
        } else {
            return None;
        };
        let ratio = self.scale(lead, s, now) / lead;
        let lines = if d.lines != 0 {
            round_carry(&mut self.rest, d.lines as f64 * ratio)
        } else {
            0
        };
        let points = if d.points != 0 {
            round_carry(&mut self.point_rest, d.points as f64 * ratio)
        } else {
            0
        };
        Some(Deltas {
            lines,
            points,
            fixed: d.fixed * ratio,
        })
    }
}

/// Applies the settings to a scroll wheel event in place. Trackpads and
/// Magic Mouse (continuous events) are left alone.
pub(super) fn transform(
    ev: &core_graphics::event::CGEvent,
    axes: &mut [Axis; 2],
    s: &Settings,
    now: Instant,
) {
    use core_graphics::event::EventField;

    if ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS) != 0 {
        log::debug!("scroll: continuous event left alone");
        return;
    }
    let fields = [
        (
            EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1,
            EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1,
            EventField::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_1,
        ),
        (
            EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2,
            EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2,
            EventField::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_2,
        ),
    ];
    for (axis, (lines, points, fixed)) in axes.iter_mut().zip(fields) {
        let d = Deltas {
            lines: ev.get_integer_value_field(lines),
            points: ev.get_integer_value_field(points),
            fixed: ev.get_double_value_field(fixed),
        };
        let Some(out) = axis.process(d, s, now) else {
            continue;
        };
        log::debug!("scroll: {d:?} -> {out:?} (factor {})", s.factor);
        // Setting the line delta recomputes the point and fixed-point
        // deltas, so it must come first.
        ev.set_integer_value_field(lines, out.lines);
        ev.set_integer_value_field(points, out.points);
        ev.set_double_value_field(fixed, out.fixed);
    }
}

/// A vertical mouse wheel event in trackpad mode.
#[derive(Debug, PartialEq)]
pub(super) struct TrackpadInput {
    /// Scaled and signed pixels.
    pub pixels: f64,
    /// Cmd, Ctrl or Opt is down: the wheel means zoom or the like.
    pub wheel_modifier: bool,
    /// Shift is down: the wheel scrolls sideways.
    pub shift: bool,
}

impl TrackpadInput {
    /// No modifier key: the event belongs to the glide.
    pub fn plain(&self) -> bool {
        !self.wheel_modifier && !self.shift
    }
}

/// In trackpad mode: a vertical mouse wheel event. `None` leaves the event
/// to [`transform`]: trackpads and synthesized events (continuous) and
/// horizontal wheels. What to do with modifier keys is up to the caller.
pub(super) fn trackpad_input(
    ev: &core_graphics::event::CGEvent,
    s: &Settings,
) -> Option<TrackpadInput> {
    use core_graphics::event::{CGEventFlags as Fl, EventField as F};

    if !s.trackpad || ev.get_integer_value_field(F::SCROLL_WHEEL_EVENT_IS_CONTINUOUS) != 0 {
        return None;
    }
    let horizontal = ev.get_integer_value_field(F::SCROLL_WHEEL_EVENT_DELTA_AXIS_2) != 0
        || ev.get_integer_value_field(F::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2) != 0
        || ev.get_double_value_field(F::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_2) != 0.0;
    if horizontal {
        return None;
    }
    let flags = ev.get_flags();
    let wheel_modifier = flags.intersects(
        Fl::CGEventFlagCommand | Fl::CGEventFlagAlternate | Fl::CGEventFlagControl,
    );
    let shift = flags.intersects(Fl::CGEventFlagShift);
    let d = Deltas {
        lines: ev.get_integer_value_field(F::SCROLL_WHEEL_EVENT_DELTA_AXIS_1),
        points: ev.get_integer_value_field(F::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1),
        fixed: ev.get_double_value_field(F::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_1),
    };
    if d == Deltas::default() {
        return None;
    }
    let px = s.trackpad_pixels(d);
    log::debug!("scroll: {d:?} -> trackpad {px:.1} px");
    Some(TrackpadInput {
        pixels: px,
        wheel_modifier,
        shift,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_and_keeps_remainder() {
        let mut s = Scaler::default();
        assert_eq!(s.convert(2, 100, false), 6);
        assert_eq!(s.convert(1, 50, false), 1); // 1.5 -> 1, rest .5
        assert_eq!(s.convert(1, 50, false), 2); // 1.5 + .5
        assert_eq!(s.convert(3, 100, true), -9);
    }

    #[test]
    fn scaler_uses_wheel_resolution() {
        // One full turn gives the same distance whatever the resolution.
        let turn = |res: u16, speed: u16| {
            let mut s = Scaler::new(res);
            (0..res).map(|_| s.convert(1, speed, false)).sum::<i32>()
        };
        assert_eq!(turn(120, 100), 360);
        assert_eq!(turn(24, 100), 360);
        assert_eq!(turn(120, 300), 1080);
        assert_eq!(turn(120, 50), 180);
        let mut s = Scaler::new(0); // unknown resolution: MX Master default
        assert_eq!(s.convert(1, 100, false), 3);
    }

    /// Runs a line-only event through the axis and returns the output lines.
    fn apply(a: &mut Axis, lines: i64, s: &Settings, now: Instant) -> i64 {
        a.process(
            Deltas {
                lines,
                points: 0,
                fixed: 0.0,
            },
            s,
            now,
        )
        .map_or(0, |d| d.lines)
    }

    fn cfg(direction: Option<&str>, speed: Option<u16>, smoothing: Option<u8>) -> Scroll {
        Scroll {
            direction: direction.map(String::from),
            speed,
            smoothing,
            trackpad: None,
        }
    }

    fn trackpad_cfg(direction: Option<&str>, speed: Option<u16>) -> Settings {
        let c = Scroll {
            trackpad: Some(true),
            smoothing: Some(50),
            ..cfg(direction, speed, None)
        };
        Settings::from_config(&c, true).unwrap()
    }

    #[test]
    fn trackpad_mode_needs_the_tap_and_drops_smoothing() {
        let s = trackpad_cfg(None, None);
        assert!(s.trackpad());
        assert_eq!(s.smoothing, 0.0);
        assert!(!s.invert);
        let off = Scroll {
            trackpad: Some(false),
            ..cfg(None, None, None)
        };
        assert!(Settings::from_config(&off, true).is_none());
        let s = Settings::from_config(&cfg(None, None, Some(40)), true).unwrap();
        assert!(!s.trackpad());
        assert!((s.smoothing - 0.4).abs() < 1e-9);
    }

    #[test]
    fn wheel_clicks_become_pixels() {
        let d = |lines, points, fixed| Deltas {
            lines,
            points,
            fixed,
        };
        let s = trackpad_cfg(None, None);
        // One click: one line.
        assert_eq!(s.trackpad_pixels(d(1, 10, 1.0)), 24.0);
        // Accelerated by macOS: the fractional delta leads.
        assert_eq!(s.trackpad_pixels(d(2, 25, 2.5)), 60.0);
        // High-resolution wheel: fractions of a line.
        assert_eq!(s.trackpad_pixels(d(0, 1, 0.125)), 3.0);
        assert_eq!(s.trackpad_pixels(d(-3, 0, 0.0)), -72.0);
        assert_eq!(s.trackpad_pixels(d(0, 5, 0.0)), 12.0);
        // Speed and direction apply.
        assert_eq!(
            trackpad_cfg(None, Some(200)).trackpad_pixels(d(1, 10, 1.0)),
            48.0
        );
        assert_eq!(
            trackpad_cfg(Some("standard"), None).trackpad_pixels(d(1, 10, 1.0)),
            -24.0
        );
        assert_eq!(
            trackpad_cfg(Some("natural"), None).trackpad_pixels(d(-1, -10, -1.0)),
            -24.0
        );
    }

    #[test]
    fn trackpad_takes_only_vertical_wheel_events() {
        use core_graphics::event::EventField as F;
        let s = trackpad_cfg(None, None);
        let px = |ev: &core_graphics::event::CGEvent| trackpad_input(ev, &s).map(|i| i.pixels);
        assert_eq!(px(&wheel_event(1)), Some(24.0));
        assert!(trackpad_input(&wheel_event(1), &s).unwrap().plain());
        let off = event_settings(300);
        assert_eq!(trackpad_input(&wheel_event(1), &off), None);
        let ev = wheel_event(1);
        ev.set_integer_value_field(F::SCROLL_WHEEL_EVENT_IS_CONTINUOUS, 1);
        assert_eq!(px(&ev), None);
        let ev = wheel_event(0);
        assert_eq!(px(&ev), None);
        let ev = wheel_event(1);
        ev.set_integer_value_field(F::SCROLL_WHEEL_EVENT_DELTA_AXIS_2, 1);
        assert_eq!(px(&ev), None);
    }

    #[test]
    fn trackpad_input_reports_modifiers() {
        use core_graphics::event::CGEventFlags as Fl;
        let s = trackpad_cfg(None, None);
        for flag in [Fl::CGEventFlagCommand, Fl::CGEventFlagControl, Fl::CGEventFlagAlternate] {
            let ev = wheel_event(1);
            ev.set_flags(flag);
            let i = trackpad_input(&ev, &s).unwrap();
            assert!(i.wheel_modifier && !i.shift && !i.plain());
        }
        let ev = wheel_event(1);
        ev.set_flags(Fl::CGEventFlagShift);
        let i = trackpad_input(&ev, &s).unwrap();
        assert!(!i.wheel_modifier && i.shift && !i.plain());
    }

    #[test]
    fn direction_depends_on_system_setting() {
        let st = |d, nat| Settings::from_config(&cfg(Some(d), None, None), nat);
        assert!(st("standard", true).unwrap().invert);
        assert!(st("standard", false).is_none());
        assert!(st("natural", false).unwrap().invert);
        assert!(st("natural", true).is_none());
        assert!(Settings::from_config(&cfg(None, None, None), true).is_none());
    }

    #[test]
    fn inverts_and_scales_with_remainder() {
        let s = Settings::from_config(&cfg(Some("standard"), Some(150), None), true).unwrap();
        let mut a = Axis::default();
        let now = Instant::now();
        assert_eq!(apply(&mut a, 2, &s, now), -3);
        assert_eq!(apply(&mut a, 1, &s, now), -2); // -1.5 rounds away, rest .5
        assert_eq!(apply(&mut a, 0, &s, now), 0);
    }

    #[test]
    fn slow_speed_never_drops_a_tick_forever() {
        let s = Settings::from_config(&cfg(None, Some(50), None), true).unwrap();
        let mut a = Axis::default();
        let now = Instant::now();
        let sum: i64 = (0..4).map(|_| apply(&mut a, 1, &s, now)).sum();
        assert_eq!(sum, 2);
    }

    #[test]
    fn smoothing_blends_close_events_only() {
        let s = Settings::from_config(&cfg(None, None, Some(50)), true).unwrap();
        let mut a = Axis::default();
        let t0 = Instant::now();
        assert_eq!(apply(&mut a, 4, &s, t0), 4);
        assert_eq!(apply(&mut a, 2, &s, t0), 3); // 2*.5 + 4*.5
        let later = t0 + Duration::from_millis(500);
        assert_eq!(apply(&mut a, 2, &s, later), 2);
    }

    fn event_settings(speed: u16) -> Settings {
        Settings::from_config(&cfg(None, Some(speed), None), true).unwrap()
    }

    #[test]
    fn hires_event_without_lines_is_scaled() {
        let now = Instant::now();
        let d = Deltas {
            lines: 0,
            points: 4,
            fixed: 0.25,
        };
        let mut a = Axis::default();
        let out = a.process(d, &event_settings(300), now).unwrap();
        assert_eq!(out.lines, 0);
        assert_eq!(out.points, 12);
        assert!((out.fixed - 0.75).abs() < 1e-9);
        let mut a = Axis::default();
        let out = a.process(d, &event_settings(50), now).unwrap();
        assert_eq!(out.lines, 0);
        assert_eq!(out.points, 2);
        assert!((out.fixed - 0.125).abs() < 1e-9);
    }

    #[test]
    fn hires_remainder_carries_between_events() {
        let s = event_settings(50);
        let now = Instant::now();
        let mut a = Axis::default();
        let d = Deltas {
            lines: 0,
            points: 3,
            fixed: 0.1875,
        };
        // 1.5 px per event: 2, then 1 (rest .5 carried), then 2, ...
        let sum: i64 = (0..4).map(|_| a.process(d, &s, now).unwrap().points).sum();
        assert_eq!(sum, 6);
        let out = a.process(d, &s, now).unwrap();
        assert!((out.fixed - 0.09375).abs() < 1e-9); // fractional, no rounding
    }

    #[test]
    fn hires_small_movements_are_carried_not_lost() {
        let s = event_settings(50);
        let mut a = Axis::default();
        let d = Deltas {
            lines: 0,
            points: 1,
            fixed: 0.1,
        };
        let sum: i64 = (0..4)
            .map(|_| a.process(d, &s, Instant::now()).unwrap().points)
            .sum();
        assert_eq!(sum, 2);
    }

    #[test]
    fn line_events_scale_all_deltas_together() {
        let now = Instant::now();
        let d = Deltas {
            lines: 1,
            points: 10,
            fixed: 1.0,
        };
        let out = Axis::default()
            .process(d, &event_settings(300), now)
            .unwrap();
        assert_eq!(
            out,
            Deltas {
                lines: 3,
                points: 30,
                fixed: 3.0
            }
        );
        let out = Axis::default()
            .process(d, &event_settings(50), now)
            .unwrap();
        assert_eq!(out.lines, 1); // 0.5 rounds up: a tick stays
        assert_eq!(out.points, 5);
        assert!((out.fixed - 0.5).abs() < 1e-9);
    }

    #[test]
    fn empty_axis_is_left_alone() {
        let mut a = Axis::default();
        assert_eq!(
            a.process(Deltas::default(), &event_settings(300), Instant::now()),
            None
        );
    }

    #[test]
    fn hires_inverts_sign() {
        let s = Settings::from_config(&cfg(Some("standard"), Some(200), None), true).unwrap();
        let d = Deltas {
            lines: 0,
            points: 2,
            fixed: 0.5,
        };
        let out = Axis::default().process(d, &s, Instant::now()).unwrap();
        assert_eq!(out.points, -4);
        assert!((out.fixed + 1.0).abs() < 1e-9);
    }

    /// A real scroll event: `lines` wheel clicks, optionally made hi-res
    /// (no line delta, only point and fixed-point deltas).
    fn wheel_event(lines: i32) -> core_graphics::event::CGEvent {
        use core_graphics::event::{CGEvent, ScrollEventUnit};
        use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
        let src = CGEventSource::new(CGEventSourceStateID::HIDSystemState).unwrap();
        CGEvent::new_scroll_event(src, ScrollEventUnit::LINE, 1, lines, 0, 0).unwrap()
    }

    fn read(ev: &core_graphics::event::CGEvent) -> (i64, i64, f64) {
        use core_graphics::event::EventField as F;
        (
            ev.get_integer_value_field(F::SCROLL_WHEEL_EVENT_DELTA_AXIS_1),
            ev.get_integer_value_field(F::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1),
            ev.get_double_value_field(F::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_1),
        )
    }

    #[test]
    fn real_line_event_is_scaled() {
        let ev = wheel_event(2);
        let (lines, points, fixed) = read(&ev);
        let mut axes = [Axis::default(), Axis::default()];
        transform(&ev, &mut axes, &event_settings(300), Instant::now());
        let (l, p, f) = read(&ev);
        assert_eq!(l, lines * 3);
        assert_eq!(p, points * 3);
        assert!((f - fixed * 3.0).abs() < 1e-6);
        let ev = wheel_event(2);
        let mut axes = [Axis::default(), Axis::default()];
        transform(&ev, &mut axes, &event_settings(50), Instant::now());
        assert_eq!(read(&ev).0, 1);
    }

    #[test]
    fn real_hires_event_is_scaled() {
        use core_graphics::event::EventField as F;
        for (speed, ratio) in [(300u16, 3.0), (50, 0.5)] {
            let ev = wheel_event(1);
            ev.set_integer_value_field(F::SCROLL_WHEEL_EVENT_DELTA_AXIS_1, 0);
            ev.set_integer_value_field(F::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1, 8);
            ev.set_double_value_field(F::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_1, 0.5);
            let mut axes = [Axis::default(), Axis::default()];
            transform(&ev, &mut axes, &event_settings(speed), Instant::now());
            let (l, p, f) = read(&ev);
            assert_eq!(l, 0);
            assert_eq!(p, (8.0 * ratio) as i64);
            assert!((f - 0.5 * ratio).abs() < 1e-6, "fixed {f} at {speed}%");
        }
    }

    #[test]
    fn continuous_events_are_left_alone() {
        use core_graphics::event::EventField as F;
        let ev = wheel_event(2);
        ev.set_integer_value_field(F::SCROLL_WHEEL_EVENT_IS_CONTINUOUS, 1);
        let before = read(&ev);
        let mut axes = [Axis::default(), Axis::default()];
        transform(&ev, &mut axes, &event_settings(300), Instant::now());
        assert_eq!(read(&ev), before);
    }
}
