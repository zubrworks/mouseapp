//! State the tabs share: the settings store, the connected mouse, the
//! daemon state, and reading and writing the current settings.

use std::cell::RefCell;

use objc2::MainThreadMarker;
use rust_i18n::t;

use super::{Effect, apps, dialogs, gestures};
use crate::config::Config;
use crate::devices::{self, Button, Model};
use crate::driver::State;
use crate::hidpp::features::dpi::DpiList;
use crate::hidpp::features::smartshift::SmartShift;
use crate::hub::MouseInfo;
use crate::permissions::Permission;
use crate::profiles::Store;

/// What tabs need to read and write settings.
pub struct Context {
    /// Settings folder: language, current settings and the profiles.
    pub store: Store,
    /// The connected mouse the window describes.
    pub mouse: RefCell<MouseView>,
    /// Privacy permissions the daemon lacks (from its status).
    pub missing: RefCell<Vec<Permission>>,
    /// What the app knows about the daemon.
    pub driver: RefCell<State>,
    /// State of the Applications tab.
    pub apps: apps::AppsState,
    /// State of the Gestures tab.
    pub gestures: gestures::GesturesState,
}

impl Context {
    pub fn new(store: Store) -> Self {
        Context {
            store,
            mouse: RefCell::default(),
            missing: RefCell::default(),
            driver: RefCell::new(State::Stopped),
            apps: apps::AppsState::default(),
            gestures: gestures::GesturesState::default(),
        }
    }
}

/// What the tabs know about the connected mouse: its registry model (the
/// first model when no mouse is connected) and the DPI list it reported.
#[derive(Clone, Debug)]
pub struct MouseView {
    pub model: &'static Model,
    pub dpi_list: Option<DpiList>,
    /// SmartShift state the mouse reported (its default threshold).
    pub smartshift: Option<SmartShift>,
}

impl Default for MouseView {
    fn default() -> Self {
        MouseView {
            model: devices::default_model(),
            dpi_list: None,
            smartshift: None,
        }
    }
}

impl MouseView {
    /// Describes the first connected recognized mouse, or the first model.
    pub fn from_mice(mice: &[MouseInfo]) -> Self {
        mice.iter()
            .find_map(|m| {
                let model = m.model.as_deref().and_then(devices::by_name)?;
                Some(MouseView {
                    model,
                    dpi_list: m.dpi_list.clone(),
                    smartshift: m.smartshift,
                })
            })
            .unwrap_or_default()
    }

    /// Default SmartShift threshold: the mouse's own, else the model's.
    pub fn smartshift_default(&self) -> u8 {
        self.smartshift
            .and_then(|s| s.usable_default())
            .unwrap_or(self.model.smartshift.default)
    }

    /// DPI slider range `(min, max, step)`: the mouse's own list if it
    /// reported one, otherwise the model's range.
    pub fn dpi_range(&self) -> (u16, u16, u16) {
        if let Some(l) = &self.dpi_list
            && l.min() > 0
            && l.max() >= l.min()
        {
            let step = match l.ranges.first() {
                Some(&(_, _, s)) if l.values.is_empty() && s > 0 => s,
                _ if l.values.len() > 1 => {
                    let mut v = l.values.clone();
                    v.sort_unstable();
                    v.windows(2)
                        .map(|w| w[1] - w[0])
                        .min()
                        .filter(|&d| d > 0)
                        .unwrap_or(1)
                }
                _ => self.model.dpi.step,
            };
            return (l.min(), l.max(), step.max(1));
        }
        let d = &self.model.dpi;
        (d.min, d.max, d.step.max(1))
    }
}

/// Applies `edit` to the current settings and writes them back keeping
/// comments, key order and formatting: only keys that changed are touched.
/// Returns the effects of the change (a failed save has none; `dispatch`
/// makes the window re-read the real values).
pub fn update_config(
    ctx: &Context,
    mtm: MainThreadMarker,
    edit: impl FnOnce(&mut Config),
) -> Vec<Effect> {
    match ctx.store.update_current(edit) {
        Ok(()) => vec![Effect::ConfigSaved],
        Err(e) => {
            dialogs::report_error(mtm, &t!("settings.save_failed", error = e));
            Vec::new()
        }
    }
}

/// Asks the daemon to save `dpi` (it is the one that writes the file, so
/// the two never write it at once). The window is not rebuilt: the file is
/// written after this returns and would show the old value. Without a
/// running daemon nothing is saved, so the slider goes back.
pub fn set_dpi(ctx: &Context, dpi: u16) -> Vec<Effect> {
    if ctx.driver.borrow().status().is_some() {
        vec![Effect::Dpi(dpi)]
    } else {
        Vec::new()
    }
}

/// The current settings, or defaults when the file is invalid.
pub fn read_config(ctx: &Context) -> Config {
    ctx.store.load_current().map(|(_, c)| c).unwrap_or_default()
}

/// Reprogrammable buttons the window offers: the remappable buttons of the
/// connected model.
pub fn remappable_buttons(ctx: &Context) -> Vec<&'static Button> {
    let model = ctx.mouse.borrow().model;
    model.buttons.iter().filter(|b| b.remappable).collect()
}
