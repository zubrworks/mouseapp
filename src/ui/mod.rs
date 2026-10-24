//! Settings window (AppKit): tabs on the toolbar, one file per tab. Every
//! change is written to the current settings at once and the daemon
//! re-applies them.

pub mod apps;
pub mod device;
pub mod buttons;
pub mod general;
pub mod gestures;
pub mod pointer;
pub mod profiles;
pub mod scrolling;
pub mod window;

mod actions;
mod context;
mod controls;
mod dialogs;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::MainThreadMarker;
use objc2_app_kit::NSView;
use rust_i18n::t;

pub use actions::*;
pub use context::*;
pub use controls::*;
pub use dialogs::*;
pub use window::SettingsWindow;

/// What the rest of the app has to do after a change made in the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    /// The config file was written: reload and apply it.
    ConfigSaved,
    /// Save this DPI in the current settings (the daemon writes the file).
    Dpi(u16),
    /// The UI language changed: rebuild all texts.
    LanguageChanged,
    /// Something shown in the menu bar menu changed (e.g. launch at login).
    MenuChanged,
    /// Settings changed outside the control that was used: re-read them.
    Refresh,
    /// Start the daemon.
    StartDriver,
    /// Stop the daemon.
    StopDriver,
}

/// Toolbar tabs, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    General,
    Pointer,
    Buttons,
    Gestures,
    Scrolling,
    Apps,
    Device,
}

impl Tab {
    pub const ALL: [Tab; 7] = [
        Tab::General,
        Tab::Pointer,
        Tab::Buttons,
        Tab::Gestures,
        Tab::Scrolling,
        Tab::Apps,
        Tab::Device,
    ];

    /// Toolbar item identifier, also the prefix of control ids in the tab.
    pub fn id(self) -> &'static str {
        match self {
            Tab::General => "general",
            Tab::Pointer => "pointer",
            Tab::Buttons => "buttons",
            Tab::Gestures => "gestures",
            Tab::Scrolling => "scrolling",
            Tab::Apps => "apps",
            Tab::Device => "device",
        }
    }

    pub fn from_id(id: &str) -> Option<Tab> {
        Tab::ALL.into_iter().find(|t| t.id() == id)
    }

    pub fn title(self) -> String {
        match self {
            Tab::General => t!("settings.tab.general"),
            Tab::Pointer => t!("settings.tab.pointer"),
            Tab::Buttons => t!("settings.tab.buttons"),
            Tab::Gestures => t!("settings.tab.gestures"),
            Tab::Scrolling => t!("settings.tab.scrolling"),
            Tab::Apps => t!("settings.tab.apps"),
            Tab::Device => t!("settings.tab.device"),
        }
        .to_string()
    }

    /// SF Symbol shown on the toolbar.
    pub fn symbol(self) -> &'static str {
        match self {
            Tab::General => "gearshape",
            Tab::Pointer => "cursorarrow",
            Tab::Buttons => "computermouse",
            Tab::Gestures => "hand.draw",
            Tab::Scrolling => "arrow.up.and.down",
            Tab::Apps => "square.grid.2x2",
            Tab::Device => "info.circle",
        }
    }

    /// Builds the tab content with the current settings. Controls send
    /// their changes to `target` (see [`wire`]).
    fn build(self, ctx: &Context, target: &AnyObject, mtm: MainThreadMarker) -> Retained<NSView> {
        match self {
            Tab::General => general::build(ctx, target, mtm),
            Tab::Pointer => pointer::build(ctx, target, mtm),
            Tab::Buttons => buttons::build(ctx, target, mtm),
            Tab::Gestures => gestures::build(ctx, target, mtm),
            Tab::Scrolling => scrolling::build(ctx, target, mtm),
            Tab::Apps => apps::build(ctx, target, mtm),
            Tab::Device => device::build(ctx, target, mtm),
        }
    }

    /// Handles a change of the control `id` (`<tab>.<name>`).
    fn changed(
        self,
        id: &str,
        sender: &AnyObject,
        ctx: &Context,
        mtm: MainThreadMarker,
    ) -> Vec<Effect> {
        match self {
            Tab::General => general::changed(id, sender, ctx, mtm),
            Tab::Pointer => pointer::changed(id, sender, ctx, mtm),
            Tab::Buttons => buttons::changed(id, sender, ctx, mtm),
            Tab::Gestures => gestures::changed(id, sender, ctx, mtm),
            Tab::Scrolling => scrolling::changed(id, sender, ctx, mtm),
            Tab::Apps => apps::changed(id, sender, ctx, mtm),
            Tab::Device => Vec::new(),
        }
    }
}
