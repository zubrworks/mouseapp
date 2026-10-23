//! The diverted thumb wheel: horizontal scrolling or zoom / volume / tab steps.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use rust_i18n::t;

use crate::actions::keyboard;
use crate::actions::{self, Action};
use crate::config::{Config, DEFAULT_SPEED, ThumbMode};
use crate::events::Restore;
use crate::hidpp::device::{Device, is_link_failure};
use crate::hidpp::features::thumbwheel;
use crate::hidpp::message::Message;
use crate::scroll;
use crate::util::Throttle;

/// A repeating failure (e.g. scroll events refused) is logged this often at most.
const LOG_EVERY: Duration = Duration::from_secs(10);

/// The change that gives the thumb wheel back to the mouse.
pub(crate) fn thumb_restore(native_invert: Option<bool>) -> thumbwheel::Change {
    thumbwheel::Change { diverted: Some(false), invert: native_invert }
}

/// Reads the native `invert` of the thumb wheel before it is diverted
/// (diverting resets it). `None` if the mouse does not answer; a lost link
/// is an error.
pub(crate) fn read_native_invert(dev: &Device, idx: u8) -> Result<Option<bool>> {
    match thumbwheel::read_status(dev, idx) {
        Ok(s) => Ok(Some(s.invert)),
        Err(e) if is_link_failure(&e) => Err(e),
        Err(e) => {
            log::warn!("cannot read the thumb wheel state: {e}");
            Ok(None)
        }
    }
}

/// Thumb wheel steps per revolution in the step modes (zoom, volume, tabs).
const THUMB_STEPS_PER_TURN: u16 = 12;

/// Collects thumb wheel rotation and turns it into whole steps, one per
/// `per_step` increments; the rest is kept for the next notification.
#[derive(Debug)]
struct Stepper {
    per_step: i32,
    rest: i32,
}

impl Stepper {
    /// `diverted_res`: rotation increments per revolution (0 = MX Master value).
    fn new(diverted_res: u16) -> Self {
        let res = thumbwheel::diverted_res_or_default(diverted_res);
        Stepper { per_step: i32::from((res / THUMB_STEPS_PER_TURN).max(1)), rest: 0 }
    }

    /// Adds a rotation; returns the signed number of whole steps reached
    /// (`invert` flips the sign).
    fn add(&mut self, delta: i16, invert: bool) -> i32 {
        let d = i32::from(delta);
        self.rest += if invert { -d } else { d };
        let steps = self.rest / self.per_step;
        self.rest -= steps * self.per_step;
        steps
    }

    /// Drops a partial step (the rotation ended).
    fn reset(&mut self) {
        self.rest = 0;
    }
}

/// Actions of a step mode: (backward, forward) for the right/front-positive
/// rotation; `None` for "scroll" (or no mode set).
fn mode_actions(mode: Option<&str>) -> Result<Option<(Action, Action)>> {
    let key = |s: &str| keyboard::Shortcut::parse(s).map(Action::Key);
    let name = mode.unwrap_or(ThumbMode::Scroll.name());
    let Some(mode) = ThumbMode::from_name(name) else {
        bail!("{}", t!("action.unknown_thumb_mode", mode = name));
    };
    Ok(match mode {
        ThumbMode::Scroll => None,
        ThumbMode::Zoom => Some((key("cmd+-")?, key("cmd+=")?)),
        ThumbMode::Volume => Some((
            Action::Media(keyboard::Media::VolumeDown),
            Action::Media(keyboard::Media::VolumeUp),
        )),
        ThumbMode::Tabs => Some((key("ctrl+shift+tab")?, key("ctrl+tab")?)),
    })
}

/// A step mode in use: rotation runs `back` / `forward` once per step.
struct StepMode {
    stepper: Stepper,
    back: Action,
    forward: Action,
    /// Flip the sign of the (right-positive) rotation before counting.
    invert: bool,
}

/// The diverted thumb wheel: turns its notifications into horizontal
/// scrolling (or the actions of a step mode). [`ThumbCheck`] keeps the
/// diversion in place.
pub(crate) struct Thumb {
    idx: u8,
    scaler: scroll::Scaler,
    gesture: scroll::HorizontalGesture,
    speed: u16,
    /// Flip the sign of the (right-positive) rotation before posting.
    invert: bool,
    /// Zoom, volume or tabs instead of scrolling.
    steps: Option<StepMode>,
    /// Rotation notifications received in this session.
    events: u64,
    scroll_log: Throttle,
    action_log: Throttle,
}

impl Thumb {
    /// Diverts the thumb wheel; `None` if the mouse has none or refuses.
    /// The device-side invert stays off; inversion is applied in software.
    pub(crate) fn start(dev: &Device, cfg: &Config, guard: &mut Restore) -> Result<Option<Thumb>> {
        let Some(idx) = dev.feature(thumbwheel::ID)? else {
            log::info!("thumb wheel: not available on {}", dev.name);
            return Ok(None);
        };
        let info = match thumbwheel::read_info(dev, idx) {
            Ok(i) => {
                log::info!("thumb wheel info (feature index {idx}): {i:?}");
                Some(i)
            }
            Err(e) if is_link_failure(&e) => return Err(e),
            Err(e) => {
                log::warn!("cannot read thumb wheel info: {e}");
                None
            }
        };
        let actions = mode_actions(cfg.thumbwheel.mode.as_deref())?;
        guard.thumb = Some(read_native_invert(dev, idx)?);
        if let Err(e) = divert_thumb(dev) {
            if is_link_failure(&e) {
                return Err(e);
            }
            log::warn!("cannot divert the thumb wheel: {e}");
            guard.thumb = None;
            return Ok(None);
        }
        let natural = scroll::system_natural();
        let positive_right = info.is_none_or(|i| i.positive_right);
        let invert = thumb_invert(positive_right, natural, cfg.thumbwheel.invert.unwrap_or(false));
        let res = info.map_or(thumbwheel::DEFAULT_DIVERTED_RES, |i| i.diverted_res);
        let speed = cfg.thumbwheel.speed.unwrap_or(DEFAULT_SPEED);
        let mode = cfg.thumbwheel.mode.as_deref().unwrap_or(ThumbMode::Scroll.name());
        log::info!(
            "thumb wheel: mode {mode}, {res} increments per turn, speed {speed}%, natural scrolling {natural}, invert {invert}"
        );
        // Steps ignore natural scrolling: forward is always zoom in, louder, next tab.
        let steps = actions.map(|(back, forward)| StepMode {
            stepper: Stepper::new(res),
            back,
            forward,
            invert: !positive_right ^ cfg.thumbwheel.invert.unwrap_or(false),
        });
        Ok(Some(Thumb {
            idx,
            scaler: scroll::Scaler::new(res),
            gesture: scroll::HorizontalGesture::default(),
            speed,
            invert,
            steps,
            events: 0,
            scroll_log: Throttle::new(LOG_EVERY),
            action_log: Throttle::new(LOG_EVERY),
        }))
    }

    /// True while a scroll gesture is open: the loop must poll often to end it.
    pub(crate) fn busy(&self) -> bool {
        self.gesture.active()
    }

    /// Handles a thumb wheel notification; false if `msg` is something else.
    pub(crate) fn handle(&mut self, msg: &Message, now: Instant) -> bool {
        let Some(rot) = thumbwheel::parse_rotation(self.idx, msg) else {
            return false;
        };
        self.events += 1;
        if self.events == 1 {
            log::info!("thumb wheel: first notification received ({rot:?})");
        }
        if let Some(m) = self.steps.as_mut() {
            let steps = m.stepper.add(rot.delta, m.invert);
            if rot.status == thumbwheel::RotationStatus::Stop {
                m.stepper.reset();
            }
            log::debug!("thumb wheel {rot:?} -> {steps} steps");
            let action = if steps > 0 { &m.forward } else { &m.back };
            for _ in 0..steps.unsigned_abs() {
                if let Err(e) = actions::run(action, None) {
                    if self.action_log.allow(now) {
                        log::warn!("{}", t!("events.action_failed", error = e));
                    }
                    break;
                }
            }
            return true;
        }
        let pixels = self.scaler.convert(rot.delta, self.speed, self.invert);
        log::debug!("thumb wheel {rot:?} -> scroll {pixels} px");
        let mut r = self.gesture.scroll(pixels, now);
        if rot.status == thumbwheel::RotationStatus::Stop {
            r = r.and(self.gesture.end());
        }
        if let Err(e) = r
            && self.scroll_log.allow(now)
        {
            log::warn!("scroll failed: {e}");
        }
        true
    }

    /// Ends an idle scroll gesture.
    pub(crate) fn expire(&mut self, now: Instant) {
        if let Err(e) = self.gesture.expire(now)
            && self.scroll_log.allow(now)
        {
            log::warn!("scroll failed: {e}");
        }
    }

    /// The check of the diversion, for the check thread.
    pub(crate) fn checker(&self) -> ThumbCheck {
        ThumbCheck { idx: self.idx, lost_log: Throttle::new(LOG_EVERY) }
    }
}

/// Checks that the wheel is still diverted (another program, e.g. Logi
/// Options+, may switch it back to native mode) and diverts it again if not.
/// Runs on the check thread; the caller decides when (see `checks`).
pub(crate) struct ThumbCheck {
    idx: u8,
    lost_log: Throttle,
}

impl ThumbCheck {
    pub(crate) fn check(&mut self, dev: &Device, now: Instant) -> Result<()> {
        let state = match thumbwheel::read_status(dev, self.idx) {
            Ok(s) => s,
            Err(e) if is_link_failure(&e) => return Err(e),
            Err(e) => {
                log::debug!("cannot read thumb wheel status: {e}");
                return Ok(());
            }
        };
        log::debug!("thumb wheel status: {state:?}");
        if state.diverted {
            return Ok(());
        }
        if self.lost_log.allow(now) {
            log::warn!("thumb wheel diversion was reset (another app, e.g. Logi Options+?); diverting again");
        }
        match divert_thumb(dev) {
            Err(e) if is_link_failure(&e) => Err(e),
            Err(e) => {
                log::warn!("cannot divert the thumb wheel: {e}");
                Ok(())
            }
            Ok(()) => Ok(()),
        }
    }
}

impl Drop for Thumb {
    fn drop(&mut self) {
        log::info!("thumb wheel: {} notifications in this session", self.events);
    }
}

/// Diverts the thumb wheel (device-side invert off) and checks the result.
fn divert_thumb(dev: &Device) -> Result<()> {
    let c = thumbwheel::Change { diverted: Some(true), invert: Some(false) };
    let state = thumbwheel::write(dev, c)?;
    log::info!("thumb wheel state after divert: {state:?}");
    if !state.diverted {
        bail!("the mouse kept the thumb wheel in native mode");
    }
    Ok(())
}

/// Whether to flip the thumb wheel rotation before posting it. The rotation
/// is first made right/front-positive (`positive_right` from the mouse); like
/// a native horizontal wheel it then follows macOS natural scrolling, and the
/// config's `invert` flips it once more.
fn thumb_invert(positive_right: bool, natural: bool, invert: bool) -> bool {
    // Posted pixels are positive when content moves right. Right rotation
    // moves content left (view goes right) without natural scrolling.
    let flip = !positive_right ^ !natural;
    flip ^ invert
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumb_direction_follows_mouse_system_and_config() {
        // Right-positive mouse with natural scrolling: posted as is.
        assert!(!thumb_invert(true, true, false));
        assert!(thumb_invert(true, true, true));
        // Standard scrolling flips, as for a native horizontal wheel.
        assert!(thumb_invert(true, false, false));
        // A left-positive mouse is normalized first.
        assert!(thumb_invert(false, true, false));
        assert!(!thumb_invert(false, false, false));
        assert!(thumb_invert(false, false, true));
    }

    #[test]
    fn thumb_restore_uses_native_invert() {
        let c = thumb_restore(Some(true));
        assert_eq!(c.diverted, Some(false));
        assert_eq!(c.invert, Some(true));
        assert_eq!(thumb_restore(Some(false)).invert, Some(false));
        // Unknown native state: the invert is not written.
        assert_eq!(thumb_restore(None).invert, None);
    }

    #[test]
    fn thumb_stepper_counts_steps() {
        // 120 increments per turn: one step per 10.
        let mut s = Stepper::new(120);
        assert_eq!(s.per_step, 10);
        // Accumulates below the threshold.
        assert_eq!(s.add(4, false), 0);
        assert_eq!(s.add(5, false), 0);
        // Reaches it; the rest (9 + 3 - 10 = 2) is kept.
        assert_eq!(s.add(3, false), 1);
        assert_eq!(s.rest, 2);
        assert_eq!(s.add(8, false), 1);
        assert_eq!(s.rest, 0);
        // Several steps at once, and backwards.
        assert_eq!(s.add(25, false), 2);
        assert_eq!(s.add(-25, false), -2);
        assert_eq!(s.rest, 0);
        // Invert flips the direction.
        assert_eq!(s.add(10, true), -1);
        assert_eq!(s.add(-30, true), 3);
        // The rest is dropped when the rotation ends.
        assert_eq!(s.add(7, false), 0);
        s.reset();
        assert_eq!(s.add(5, false), 0);
        // Unknown resolution falls back to the default; tiny ones step on every increment.
        assert_eq!(Stepper::new(0).per_step, 10);
        assert_eq!(Stepper::new(5).per_step, 1);
    }

    #[test]
    fn thumb_modes_have_actions() {
        assert_eq!(mode_actions(None).unwrap(), None);
        assert_eq!(mode_actions(Some("scroll")).unwrap(), None);
        for m in ThumbMode::ALL.map(ThumbMode::name).iter().filter(|m| **m != "scroll") {
            assert!(mode_actions(Some(m)).unwrap().is_some(), "{m}");
        }
        let (back, forward) = mode_actions(Some("volume")).unwrap().unwrap();
        assert_eq!(back, actions::parse("volume_down").unwrap());
        assert_eq!(forward, actions::parse("volume_up").unwrap());
        let (back, forward) = mode_actions(Some("tabs")).unwrap().unwrap();
        assert_eq!(back, actions::parse("shortcut:ctrl+shift+tab").unwrap());
        assert_eq!(forward, actions::parse("shortcut:ctrl+tab").unwrap());
        assert!(mode_actions(Some("pan")).is_err());
    }
}
