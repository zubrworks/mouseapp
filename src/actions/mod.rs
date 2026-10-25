//! System actions triggered by buttons and gestures.
//!
//! Action names (snake_case, as used in `config.toml`):
//! `back`, `forward`, `mission_control`, `app_expose`, `desktop_left`,
//! `desktop_right`, `launchpad`, `screenshot`, `volume_up`, `volume_down`,
//! `mute`, `play_pause`, `next_track`, `prev_track`, `copy`, `paste`, `undo`,
//! `redo`, `show_desktop`, `switch_app`, `middle_click`, `shift_wheel_mode`
//! (toggles the scroll wheel between ratchet and free-spin; needs the mouse),
//! `gestures` (buttons only: the button works like the gesture button, see
//! [`BUTTON_ONLY`]) and `shortcut:<combo>` (e.g. `shortcut:cmd+shift+a`). Hyphens are accepted instead of underscores.

pub mod dock_swipe;
pub mod keyboard;
pub mod pointer;

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, anyhow, bail};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use rust_i18n::t;

use crate::hidpp::device::Device;
use crate::hidpp::features::smartshift;
use keyboard::{Media, Shortcut};

/// A resolved action ready to be executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Key(Shortcut),
    Media(Media),
    ShowDesktop,
    MiddleClick,
    /// Toggles the wheel between ratchet and free-spin (needs the device).
    ShiftWheelMode,
    /// The button tracks movement like the gesture button and runs the
    /// `[gestures]` actions (handled by the event loop, buttons only).
    Gestures,
    /// Mission Control or App Exposé through a Dock notification, without
    /// keys: what up and down run with `gestures.trackpad_swipe` (no name in
    /// the config, see [`through_dock`]).
    Expose(Expose),
}

/// What [`Action::Expose`] opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expose {
    MissionControl,
    App,
}

impl Expose {
    fn notification(self) -> &'static str {
        match self {
            Expose::MissionControl => "com.apple.expose.awake",
            Expose::App => "com.apple.expose.front.awake",
        }
    }
}

/// Mission Control and App Exposé (the built-in names, or a shortcut with
/// the same keys) as their Dock notification; `None` for any other action.
pub fn through_dock(action: &Action) -> Option<Action> {
    static KEYS: OnceLock<Option<(Action, Action)>> = OnceLock::new();
    let keys = KEYS.get_or_init(|| Some((parse("mission_control").ok()?, parse("app_expose").ok()?)));
    let (mission_control, app_expose) = keys.as_ref()?;
    if action == mission_control {
        Some(Action::Expose(Expose::MissionControl))
    } else if action == app_expose {
        Some(Action::Expose(Expose::App))
    } else {
        None
    }
}

/// Names of all built-in actions (without `shortcut:<combo>`).
pub const NAMES: &[&str] = &[
    "back",
    "forward",
    "mission_control",
    "app_expose",
    "desktop_left",
    "desktop_right",
    "launchpad",
    "screenshot",
    "volume_up",
    "volume_down",
    "mute",
    "play_pause",
    "next_track",
    "prev_track",
    "copy",
    "paste",
    "undo",
    "redo",
    "show_desktop",
    "switch_app",
    "middle_click",
    "shift_wheel_mode",
    "gestures",
];

/// Names that only a button can use, never a gesture slot.
pub const BUTTON_ONLY: &[&str] = &["gestures"];

/// Action names offered for a button (`for_button`) or a gesture slot.
pub fn names(for_button: bool) -> impl Iterator<Item = &'static str> {
    NAMES.iter().copied().filter(move |n| for_button || !BUTTON_ONLY.contains(n))
}

/// Resolves an action name from the config into an `Action`.
pub fn parse(name: &str) -> Result<Action> {
    let norm = name.trim().to_lowercase().replace('-', "_");
    if let Some(combo) = norm.strip_prefix("shortcut:") {
        return Ok(Action::Key(Shortcut::parse(combo)?));
    }
    let key = |s: &str| Shortcut::parse(s).map(Action::Key);
    Ok(match norm.as_str() {
        "back" => key("cmd+[")?,
        "forward" => key("cmd+]")?,
        "mission_control" => key("ctrl+up")?,
        "app_expose" => key("ctrl+down")?,
        "desktop_left" => key("ctrl+left")?,
        "desktop_right" => key("ctrl+right")?,
        "launchpad" => Action::Key(Shortcut::raw(keyboard::KEY_LAUNCHPAD)),
        "screenshot" => key("cmd+shift+4")?,
        "volume_up" => Action::Media(Media::VolumeUp),
        "volume_down" => Action::Media(Media::VolumeDown),
        "mute" => Action::Media(Media::Mute),
        "play_pause" => Action::Media(Media::PlayPause),
        "next_track" => Action::Media(Media::Next),
        "prev_track" => Action::Media(Media::Previous),
        "copy" => key("cmd+c")?,
        "paste" => key("cmd+v")?,
        "undo" => key("cmd+z")?,
        "redo" => key("cmd+shift+z")?,
        "show_desktop" => Action::ShowDesktop,
        "switch_app" => key("cmd+tab")?,
        "middle_click" => Action::MiddleClick,
        "shift_wheel_mode" => Action::ShiftWheelMode,
        "gestures" => Action::Gestures,
        _ => bail!("{}", t!("action.unknown", name = name, list = NAMES.join(", "))),
    })
}

/// Set once Accessibility was seen granted; it is not asked again this session.
static ACCESSIBILITY_OK: AtomicBool = AtomicBool::new(false);

/// True if Accessibility is granted. Only a positive answer is remembered in
/// `known`, so a permission given while the program runs is still noticed.
fn accessibility_ok(known: &AtomicBool, check: impl FnOnce() -> bool) -> bool {
    if known.load(Ordering::Relaxed) {
        return true;
    }
    let granted = check();
    if granted {
        known.store(true, Ordering::Relaxed);
    }
    granted
}

/// True if Accessibility is granted (remembered once it is).
pub(crate) fn accessibility() -> bool {
    accessibility_ok(&ACCESSIBILITY_OK, crate::permissions::accessibility_granted)
}

/// True for Show Desktop and App Exposé, which put the Dock in a mode that
/// draws nothing on a full-screen Space (the next one only closes it). Compared
/// with what the built-in name resolves to, so a custom shortcut with the same
/// keys counts too.
fn needs_regular_space(action: &Action) -> bool {
    static APP_EXPOSE: OnceLock<Option<Action>> = OnceLock::new();
    matches!(action, Action::ShowDesktop | Action::Expose(Expose::App))
        || APP_EXPOSE.get_or_init(|| parse("app_expose").ok()).as_ref() == Some(action)
}

/// True if `action` is not to be sent because the Space under the mouse
/// pointer is a full-screen one. `fullscreen` is asked only for those actions;
/// if it cannot tell (`None`), the action is sent.
fn skipped_in_fullscreen(action: &Action, fullscreen: impl FnOnce() -> Option<bool>) -> bool {
    needs_regular_space(action) && fullscreen() == Some(true)
}

/// Executes an action. Keyboard and mouse actions check the Accessibility
/// permission first; `ShiftWheelMode` needs the device instead. `Gestures`
/// is not a standalone action (the event loop tracks the button instead).
pub fn run(action: &Action, dev: Option<&Device>) -> Result<()> {
    match action {
        Action::ShiftWheelMode => {
            let Some(dev) = dev else {
                bail!("{}", t!("action.needs_device"));
            };
            return toggle_wheel_mode(dev);
        }
        Action::Gestures => bail!("{}", t!("action.button_only", name = "gestures")),
        _ => {}
    }
    if !accessibility() {
        bail!("{}", t!("action.no_accessibility"));
    }
    if skipped_in_fullscreen(action, crate::spaces::cursor_space_is_fullscreen) {
        log::info!("action skipped: fullscreen space");
        return Ok(());
    }
    match action {
        Action::Key(s) => keyboard::press(s),
        Action::Media(m) => keyboard::media(*m),
        Action::ShowDesktop => keyboard::show_desktop(),
        Action::MiddleClick => pointer::middle_click(),
        Action::Expose(e) => keyboard::dock_notification(e.notification()),
        Action::ShiftWheelMode | Action::Gestures => Ok(()),
    }
}

/// The event source for synthetic input events.
fn event_source() -> Result<CGEventSource> {
    CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow!("cannot create event source"))
}

/// Switches the wheel between ratchet and free-spin; the threshold is untouched.
/// Talks to the mouse and may block; the event loop leaves it to the check thread.
pub(crate) fn toggle_wheel_mode(dev: &Device) -> Result<()> {
    let Some(cur) = smartshift::read(dev)? else {
        bail!("{}", t!("smartshift.unsupported"));
    };
    let now = smartshift::write(
        dev,
        smartshift::Change { ratchet: Some(!cur.ratchet), threshold: None },
    )?;
    log::info!("wheel mode: {}", if now.ratchet { "ratchet" } else { "free-spin" });
    Ok(())
}

/// `mouse test-action <name>`
pub fn test_action(name: &str) -> Result<()> {
    let action = parse(name)?;
    if action == Action::ShiftWheelMode {
        let dev = Device::open_default()?;
        run(&action, Some(&dev))?;
    } else {
        run(&action, None)?;
    }
    println!("{}", t!("action.done", name = name));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessibility_is_checked_until_granted_then_once() {
        let known = AtomicBool::new(false);
        assert!(!accessibility_ok(&known, || false));
        assert!(!accessibility_ok(&known, || false));
        assert!(accessibility_ok(&known, || true));
        // Remembered: the check is not called any more.
        assert!(accessibility_ok(&known, || panic!("asked again")));
    }

    #[test]
    fn fullscreen_space_skips_show_desktop_and_app_expose() {
        for name in ["show_desktop", "app_expose", "shortcut:ctrl+down"] {
            let a = parse(name).unwrap();
            assert!(skipped_in_fullscreen(&a, || Some(true)), "{name}");
            assert!(!skipped_in_fullscreen(&a, || Some(false)), "{name}");
            // Type not readable: sent as before.
            assert!(!skipped_in_fullscreen(&a, || None), "{name}");
        }
    }

    #[test]
    fn expose_through_the_dock_keeps_the_fullscreen_check() {
        let app = Action::Expose(Expose::App);
        assert!(skipped_in_fullscreen(&app, || Some(true)));
        assert!(!skipped_in_fullscreen(&app, || Some(false)));
        let mc = Action::Expose(Expose::MissionControl);
        assert!(!skipped_in_fullscreen(&mc, || panic!("asked")));
    }

    #[test]
    fn mission_control_and_app_expose_go_through_the_dock() {
        let dock = |n: &str| through_dock(&parse(n).unwrap());
        assert_eq!(dock("mission_control"), Some(Action::Expose(Expose::MissionControl)));
        assert_eq!(dock("shortcut:ctrl+up"), Some(Action::Expose(Expose::MissionControl)));
        assert_eq!(dock("app_expose"), Some(Action::Expose(Expose::App)));
        for other in ["show_desktop", "desktop_left", "copy", "shortcut:ctrl+shift+up"] {
            assert_eq!(dock(other), None, "{other}");
        }
    }

    #[test]
    fn other_actions_are_sent_in_fullscreen_without_asking() {
        for name in ["mission_control", "desktop_left", "desktop_right", "copy", "volume_up"] {
            let a = parse(name).unwrap();
            assert!(!skipped_in_fullscreen(&a, || panic!("asked for {name}")));
        }
    }

    #[test]
    fn all_names_parse() {
        for n in NAMES {
            parse(n).unwrap();
        }
        assert_eq!(parse("mission-control").unwrap(), parse("mission_control").unwrap());
    }

    #[test]
    fn unknown_fails() {
        assert!(parse("nope").is_err());
        assert!(parse("shortcut:cmd+nokey").is_err());
        assert!(parse("shortcut:zzz").is_err());
    }

    #[test]
    fn gestures_only_for_buttons() {
        assert_eq!(parse("gestures").unwrap(), Action::Gestures);
        assert!(names(true).any(|n| n == "gestures"));
        assert!(!names(false).any(|n| n == "gestures"));
        assert_eq!(names(false).count(), NAMES.len() - BUTTON_ONLY.len());
    }
}
