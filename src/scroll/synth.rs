//! Synthesized scrolling: phased horizontal pixel events for the thumb
//! wheel, and plain pixel events for the main wheel in trackpad mode.

use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventTapLocation, CGScrollEventUnit, CGScrollPhase,
};

/// Scroll phase of a synthesized thumb wheel event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Began,
    Changed,
    Ended,
}

impl Phase {
    fn value(self) -> i64 {
        let p = match self {
            Phase::Began => CGScrollPhase::Began,
            Phase::Changed => CGScrollPhase::Changed,
            Phase::Ended => CGScrollPhase::Ended,
        };
        i64::from(p.0)
    }
}

/// Builds a pixel scroll event: `vertical` like the wheel's delta,
/// `horizontal` positive when content moves right. It is continuous
/// (precise deltas) and carries no phase.
fn pixel_event(vertical: i32, horizontal: i32) -> Result<impl std::ops::Deref<Target = CGEvent>> {
    CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Pixel, 2, vertical, horizontal, 0)
        .ok_or_else(|| anyhow!("cannot create scroll event"))
}

/// Builds a horizontal pixel scroll event (positive = content moves right)
/// carrying a scroll phase, like a trackpad swipe.
pub(super) fn horizontal_event(
    pixels: i32,
    phase: Phase,
) -> Result<impl std::ops::Deref<Target = CGEvent>> {
    let ev = pixel_event(0, pixels)?;
    // Only the phase is set after creation: writing a delta field would make
    // macOS recompute the others.
    CGEvent::set_integer_value_field(
        Some(&ev),
        CGEventField::ScrollWheelEventScrollPhase,
        phase.value(),
    );
    Ok(ev)
}

/// Posts a vertical pixel event without phase (main wheel in trackpad
/// mode): apps scroll by the pixels but never stretch the page past its
/// edge or start a pull gesture (see `trackpad`).
pub(super) fn post_vertical(pixels: i32) -> Result<()> {
    let ev = pixel_event(pixels, 0)?;
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
    log::debug!("wheel: posted {pixels} px");
    Ok(())
}

fn post_horizontal(pixels: i32, phase: Phase) -> Result<()> {
    let ev = horizontal_event(pixels, phase)?;
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
    log::debug!("thumb wheel: posted {pixels} px, phase {phase:?}");
    Ok(())
}

/// A thumb wheel movement is ended after this long without rotation.
const GESTURE_IDLE: Duration = Duration::from_millis(120);

/// Decides the phase of each thumb wheel event: the first one of a movement
/// begins the gesture, the next ones change it.
#[derive(Debug, Default)]
struct Phaser {
    last: Option<Instant>,
}

impl Phaser {
    fn step(&mut self, now: Instant) -> Phase {
        let p = if self.last.is_some() {
            Phase::Changed
        } else {
            Phase::Began
        };
        self.last = Some(now);
        p
    }

    fn expired(&self, now: Instant) -> bool {
        self.last
            .is_some_and(|t| now.saturating_duration_since(t) >= GESTURE_IDLE)
    }

    /// True when a gesture was open (an Ended event is due).
    fn end(&mut self) -> bool {
        self.last.take().is_some()
    }
}

/// Posts thumb wheel scrolling as phased gestures (Began, Changed..., Ended).
/// AppKit and WebKit views ignore much of horizontal scrolling that carries
/// no phase (web pages, swipe actions in lists), while phased pixel events
/// are handled like a trackpad. The open gesture is ended when dropped.
#[derive(Debug, Default)]
pub struct HorizontalGesture {
    phaser: Phaser,
}

impl HorizontalGesture {
    /// Scrolls by `pixels` (positive = content moves right).
    pub fn scroll(&mut self, pixels: i32, now: Instant) -> Result<()> {
        if pixels == 0 {
            return Ok(());
        }
        post_horizontal(pixels, self.phaser.step(now))
    }

    /// Ends the open gesture, if any.
    pub fn end(&mut self) -> Result<()> {
        if self.phaser.end() {
            post_horizontal(0, Phase::Ended)?;
        }
        Ok(())
    }

    /// Ends the gesture once the wheel has been idle long enough.
    pub fn expire(&mut self, now: Instant) -> Result<()> {
        if self.phaser.expired(now) {
            self.end()?;
        }
        Ok(())
    }

    /// True while a gesture is open (the caller should poll often).
    pub fn active(&self) -> bool {
        self.phaser.last.is_some()
    }
}

impl Drop for HorizontalGesture {
    fn drop(&mut self) {
        let _ = self.end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gesture_phases_begin_change_and_expire() {
        let t0 = Instant::now();
        let mut p = Phaser::default();
        assert!(!p.expired(t0));
        assert_eq!(p.step(t0), Phase::Began);
        assert_eq!(p.step(t0 + Duration::from_millis(50)), Phase::Changed);
        assert!(!p.expired(t0 + Duration::from_millis(100)));
        assert!(p.expired(t0 + Duration::from_millis(50) + GESTURE_IDLE));
        assert!(p.end());
        assert!(!p.end()); // only one Ended per gesture
        assert_eq!(p.step(t0 + Duration::from_secs(1)), Phase::Began);
    }

    #[test]
    fn horizontal_event_is_phased_pixel_scroll() {
        let get = |ev: &CGEvent, f| CGEvent::integer_value_field(Some(ev), f);
        let ev = horizontal_event(-7, Phase::Began).unwrap();
        assert_eq!(get(&ev, CGEventField::ScrollWheelEventScrollPhase), 1);
        assert_eq!(get(&ev, CGEventField::ScrollWheelEventIsContinuous), 1);
        assert_eq!(get(&ev, CGEventField::ScrollWheelEventPointDeltaAxis2), -7);
        assert_eq!(get(&ev, CGEventField::ScrollWheelEventPointDeltaAxis1), 0);
        let end = horizontal_event(0, Phase::Ended).unwrap();
        assert_eq!(get(&end, CGEventField::ScrollWheelEventScrollPhase), 4);
        assert_eq!(get(&end, CGEventField::ScrollWheelEventPointDeltaAxis2), 0);
        let ev = horizontal_event(3, Phase::Changed).unwrap();
        assert_eq!(get(&ev, CGEventField::ScrollWheelEventScrollPhase), 2);
        assert_eq!(get(&ev, CGEventField::ScrollWheelEventPointDeltaAxis2), 3);
    }

    #[test]
    fn vertical_event_is_plain_continuous_pixels() {
        let get = |ev: &CGEvent, f| CGEvent::integer_value_field(Some(ev), f);
        let ev = pixel_event(-12, 0).unwrap();
        assert_eq!(get(&ev, CGEventField::ScrollWheelEventIsContinuous), 1);
        assert_eq!(get(&ev, CGEventField::ScrollWheelEventPointDeltaAxis1), -12);
        assert_eq!(get(&ev, CGEventField::ScrollWheelEventPointDeltaAxis2), 0);
        // No gesture and no momentum: no rubber band, no pull to reload.
        assert_eq!(get(&ev, CGEventField::ScrollWheelEventScrollPhase), 0);
        assert_eq!(get(&ev, CGEventField::ScrollWheelEventMomentumPhase), 0);
    }
}
