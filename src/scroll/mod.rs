//! Scroll handling: system scroll events synthesized from the diverted thumb
//! wheel, and an event tap that sets mouse wheel direction, speed and
//! smoothing independently of the trackpad.

mod synth;
mod tap;
mod transform;

use std::sync::{Condvar, Mutex, PoisonError};

use crate::config::Scroll;
pub use synth::HorizontalGesture;
pub use tap::{run, spawn};
pub use transform::Scaler;
use transform::Settings;

static SETTINGS: Mutex<Option<Settings>> = Mutex::new(None);
/// Notified when the settings change or a stop is requested.
static CHANGED: Condvar = Condvar::new();

fn lock() -> std::sync::MutexGuard<'static, Option<Settings>> {
    crate::util::lock(&SETTINGS)
}

/// macOS "natural scrolling" (default: on when the key is missing).
pub fn system_natural() -> bool {
    crate::sysprefs::global_bool("com.apple.swipescrolldirection").unwrap_or(true)
}

/// Applies new settings to a running tap (call after a config reload).
pub fn update(c: &Scroll) {
    let settings = Settings::from_config(c, system_natural());
    log::info!("scroll settings: {settings:?}");
    *lock() = settings;
    CHANGED.notify_all();
}

/// Waits until some setting needs the tap; false once a stop is requested.
fn wait_until_needed() -> bool {
    let mut s = lock();
    loop {
        if crate::shutdown::stopped() {
            return false;
        }
        if s.is_some() {
            return true;
        }
        // Nothing needs the tap: an old problem is stale.
        set_problem(None);
        s = CHANGED.wait(s).unwrap_or_else(PoisonError::into_inner);
    }
}

/// Wakes [`wait_until_needed`] to see a stop request.
fn wake_waiting() {
    let _s = lock();
    CHANGED.notify_all();
}

/// Why the tap is not running although the settings need it (shown in the
/// app's menu, because the log of a menu bar app is not visible).
static PROBLEM: Mutex<Option<String>> = Mutex::new(None);

/// The reason the mouse wheel settings are not in effect, if any.
pub fn problem() -> Option<String> {
    crate::util::lock(&PROBLEM).clone()
}

fn set_problem(p: Option<String>) {
    *crate::util::lock(&PROBLEM) = p;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn problem_is_set_and_cleared() {
        set_problem(Some("x".into()));
        assert_eq!(problem().as_deref(), Some("x"));
        set_problem(None);
        assert_eq!(problem(), None);
    }
}
