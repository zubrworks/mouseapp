//! Routing of control changes to the tabs, and the action popups shared by
//! the Buttons, Gestures and Applications tabs.

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::NSPopUpButton;
use rust_i18n::t;

use super::{Context, Effect, Tab, ask_shortcut, popup, popup_index};
use crate::actions;

/// Routes a control change to the tab named by the id prefix. The window is
/// re-read after every change (so a declined or failed save shows the real
/// values again), except after a DPI change: the daemon writes that file
/// later, so a redraw now would show the old value.
pub(super) fn dispatch(
    id: &str,
    sender: &AnyObject,
    ctx: &Context,
    mtm: MainThreadMarker,
) -> Vec<Effect> {
    let tab = id.split_once('.').and_then(|(t, _)| Tab::from_id(t));
    let Some(tab) = tab else {
        log::warn!("settings: unknown control {id}");
        return Vec::new();
    };
    let mut effects = tab.changed(id, sender, ctx, mtm);
    if !effects.iter().any(|e| matches!(e, Effect::Dpi(_))) {
        effects.push(Effect::Refresh);
    }
    effects
}

/// One entry of an action popup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionItem {
    /// No action set (the label depends on the tab).
    Unset,
    /// A built-in action or `shortcut:<combo>` (value as in the config).
    Value(String),
    /// Ask for a shortcut.
    Prompt,
}

/// Entries of an action popup for the configured value `current`; button-only
/// actions (e.g. `gestures`) are offered only `for_button`.
pub fn action_items(current: Option<&str>, for_button: bool) -> Vec<ActionItem> {
    let mut items = vec![ActionItem::Unset];
    items.extend(actions::names(for_button).map(|n| ActionItem::Value(n.to_string())));
    if let Some(c) = current
        && !actions::names(for_button).any(|n| n == c)
    {
        items.push(ActionItem::Value(c.to_string()));
    }
    items.push(ActionItem::Prompt);
    items
}

/// Whether `/System/Applications/Launchpad.app` exists (checked once).
fn launchpad_app_exists() -> bool {
    static EXISTS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *EXISTS.get_or_init(|| std::path::Path::new("/System/Applications/Launchpad.app").exists())
}

/// Locale key suffix of an action name: the Launchpad key opens "Apps" when
/// the system has no Launchpad app.
fn locale_key(name: &str, launchpad_app: bool) -> &str {
    if name == "launchpad" && !launchpad_app { "launchpad_apps" } else { name }
}

fn action_title(item: &ActionItem, unset: &str) -> String {
    match item {
        ActionItem::Unset => unset.to_string(),
        ActionItem::Value(v) => match v.strip_prefix("shortcut:") {
            Some(combo) => t!("settings.action.shortcut", combo = combo).to_string(),
            None if actions::NAMES.contains(&v.as_str()) => {
                t!(format!("settings.action.{}", locale_key(v, launchpad_app_exists())))
                    .to_string()
            }
            None => v.clone(),
        },
        ActionItem::Prompt => t!("settings.action.custom").to_string(),
    }
}

/// Popup choosing an action of a button (`for_button`) or a gesture slot;
/// `unset` is the title of "no action".
pub fn action_popup(
    current: Option<&str>,
    for_button: bool,
    unset: &str,
    id: &str,
    target: &AnyObject,
    mtm: MainThreadMarker,
) -> Retained<NSPopUpButton> {
    let items = action_items(current, for_button);
    let titles: Vec<String> = items.iter().map(|i| action_title(i, unset)).collect();
    let selected = items
        .iter()
        .position(|i| match (i, current) {
            (ActionItem::Unset, None) => true,
            (ActionItem::Value(v), Some(c)) => v == c,
            _ => false,
        })
        .unwrap_or(0);
    popup(&titles, selected, id, target, mtm)
}

/// Result of a change in an action popup.
pub enum ActionChoice {
    /// Set (`Some`) or clear (`None`) the action.
    Set(Option<String>),
    /// Nothing to change (cancelled or invalid shortcut).
    Keep,
}

/// What the user chose in an action popup built for `current` and
/// `for_button`. The "custom shortcut" entry asks for the key combination in
/// a dialog.
pub fn action_choice(
    sender: &AnyObject,
    current: Option<&str>,
    for_button: bool,
    mtm: MainThreadMarker,
) -> ActionChoice {
    let Some(index) = popup_index(sender) else {
        return ActionChoice::Keep;
    };
    match action_items(current, for_button).get(index) {
        Some(ActionItem::Unset) => ActionChoice::Set(None),
        Some(ActionItem::Value(v)) => ActionChoice::Set(Some(v.clone())),
        Some(ActionItem::Prompt) => match ask_shortcut(mtm) {
            Some(value) => ActionChoice::Set(Some(value)),
            None => ActionChoice::Keep,
        },
        None => ActionChoice::Keep,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launchpad_is_called_apps_without_the_app() {
        assert_eq!(locale_key("launchpad", true), "launchpad");
        assert_eq!(locale_key("launchpad", false), "launchpad_apps");
        assert_eq!(locale_key("back", false), "back");
    }

    #[test]
    fn action_popup_keeps_custom_shortcut() {
        let items = action_items(Some("shortcut:cmd+k"), true);
        assert_eq!(items.first(), Some(&ActionItem::Unset));
        assert_eq!(items.last(), Some(&ActionItem::Prompt));
        assert!(items.contains(&ActionItem::Value("shortcut:cmd+k".into())));
        assert_eq!(action_items(None, true).len(), actions::NAMES.len() + 2);
    }

    #[test]
    fn gestures_offered_only_for_buttons() {
        let gestures = ActionItem::Value("gestures".into());
        assert!(action_items(None, true).contains(&gestures));
        assert!(!action_items(None, false).contains(&gestures));
    }

    #[test]
    fn every_action_has_a_title_in_every_language() {
        for lang in crate::i18n::available() {
            for name in actions::NAMES {
                for launchpad_app in [true, false] {
                    let key = format!("settings.action.{}", locale_key(name, launchpad_app));
                    let text = t!(key.as_str(), locale = lang.as_str()).to_string();
                    assert_ne!(text, key, "{lang}: {key}");
                }
            }
        }
    }
}
