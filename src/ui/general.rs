//! General tab: settings profile (choose, save, revert, rename, delete), UI language
//! and launch at login.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSButton, NSView};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use rust_i18n::t;

use super::{
    COLUMN_GAP, Context, Effect, as_view, button, fix_size, form, is_on, popup, popup_index, profiles, switch
};
use crate::autostart;
use crate::driver::State;
use crate::i18n;
use crate::profiles::{BUILTIN, marked_name, switch_to};
use crate::permissions::Permission;

const ID_LANGUAGE: &str = "general.language";
const ID_DRIVER_START: &str = "general.driver_start";
const ID_DRIVER_STOP: &str = "general.driver_stop";
const ID_LAUNCH: &str = "general.launch_at_login";
const ID_PERMISSION: &str = "general.permission.";
const ID_SHOW_FILE: &str = "general.show_settings_file";
const ID_PROFILE: &str = "general.profile";
const ID_PROFILE_SAVE: &str = "general.profile_save";
const ID_PROFILE_SAVE_NEW: &str = "general.profile_save_new";
const ID_PROFILE_REVERT: &str = "general.profile_revert";
const ID_PROFILE_RENAME: &str = "general.profile_rename";
const ID_PROFILE_DELETE: &str = "general.profile_delete";

/// What the app has to do after the profile or the list of profiles changed.
const PROFILE_CHANGED: [Effect; 3] = [Effect::ConfigSaved, Effect::MenuChanged, Effect::Refresh];

/// A row of buttons; `enabled` tells which of them can be used.
fn button_row(buttons: &[(Retained<NSButton>, bool)], mtm: MainThreadMarker) -> Retained<NSView> {
    let mut x = 0.0;
    let mut height: f64 = 0.0;
    let row = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
    for (b, enabled) in buttons {
        b.setEnabled(*enabled);
        let size = b.fittingSize();
        b.setFrame(NSRect::new(NSPoint::new(x, 0.0), size));
        row.addSubview(b);
        x += size.width + COLUMN_GAP;
        height = height.max(size.height);
    }
    let width = (x - COLUMN_GAP).max(0.0);
    row.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height)));
    // A plain view has no intrinsic size (see `ui::slider`).
    fix_size(&row, width, height);
    row
}

/// Three rows (kept short to keep the window narrow): Save to Profile /
/// Revert, Save as New, and Rename / Delete.
fn profile_buttons(
    builtin: bool,
    modified: bool,
    target: &AnyObject,
    mtm: MainThreadMarker,
) -> [Retained<NSView>; 3] {
    let s = profiles::button_states(builtin, modified);
    let b = |key: &str, id: &str| button(&t!(key), id, target, mtm);
    [
        button_row(
            &[
                (b("settings.general.profile_save", ID_PROFILE_SAVE), s.save),
                (b("settings.general.profile_revert", ID_PROFILE_REVERT), s.revert),
            ],
            mtm,
        ),
        button_row(
            &[(b("settings.general.profile_save_new", ID_PROFILE_SAVE_NEW), s.save_new)],
            mtm,
        ),
        button_row(
            &[
                (b("settings.general.profile_rename", ID_PROFILE_RENAME), s.rename),
                (b("settings.general.profile_delete", ID_PROFILE_DELETE), s.delete),
            ],
            mtm,
        ),
    ]
}

/// Popup entries: `None` = follow the system, then every translated language.
fn language_choices() -> Vec<Option<String>> {
    std::iter::once(None)
        .chain(i18n::available().into_iter().map(Some))
        .collect()
}

/// Popup title of a choice; each language is named in itself ("Polski").
fn language_title(choice: &Option<String>) -> String {
    match choice {
        None => t!("settings.general.language_system").to_string(),
        Some(code) => t!("language.name", locale = code.as_str()).to_string(),
    }
}

/// Index of the configured language in [`language_choices`] (0 = system).
fn selected_language(configured: Option<&str>, choices: &[Option<String>]) -> usize {
    configured
        .and_then(i18n::supported)
        .and_then(|code| {
            choices
                .iter()
                .position(|c| c.as_deref() == Some(code.as_str()))
        })
        .unwrap_or(0)
}

pub fn build(ctx: &Context, target: &AnyObject, mtm: MainThreadMarker) -> Retained<NSView> {
    let configured = ctx.store.language();
    let choices = language_choices();
    let titles: Vec<String> = choices.iter().map(language_title).collect();
    let selected = selected_language(configured.as_deref(), &choices);
    let language = popup(&titles, selected, ID_LANGUAGE, target, mtm);
    let launch = switch(autostart::is_enabled(), ID_LAUNCH, target, mtm);

    // A notice with a button for every permission the daemon lacks goes first.
    let missing = ctx.missing.borrow().clone();
    let mut rows: Vec<(String, Retained<NSView>)> = missing
        .into_iter()
        .map(|p| {
            let open = button(
                &t!("permissions.open_settings"),
                &format!("{ID_PERMISSION}{}", p.id()),
                target,
                mtm,
            );
            (
                t!("permissions.missing", name = p.name()).to_string(),
                as_view(&open),
            )
        })
        .collect();
    let names = ctx.store.all();
    let active = ctx.store.active();
    let modified = ctx.store.is_modified();
    let titles: Vec<String> = names.iter().map(|n| marked_name(n, modified && *n == active)).collect();
    let selected = names.iter().position(|n| *n == active).unwrap_or(0);
    let profile = popup(&titles, selected, ID_PROFILE, target, mtm);
    let (state_text, driver_button) = match &*ctx.driver.borrow() {
        State::Running(_) => (
            t!("settings.general.driver_running"),
            button(&t!("settings.general.driver_stop"), ID_DRIVER_STOP, target, mtm),
        ),
        State::Outdated(_) => (
            t!("settings.general.driver_outdated"),
            button(&t!("settings.general.driver_stop"), ID_DRIVER_STOP, target, mtm),
        ),
        State::Starting => {
            let b = button(&t!("settings.general.driver_start"), ID_DRIVER_START, target, mtm);
            b.setEnabled(false);
            (t!("settings.general.driver_starting"), b)
        }
        State::Stopped => (
            t!("settings.general.driver_stopped"),
            button(&t!("settings.general.driver_start"), ID_DRIVER_START, target, mtm),
        ),
    };
    rows.push((state_text.to_string(), as_view(&driver_button)));
    rows.push((t!("settings.general.profile").to_string(), as_view(&profile)));
    for row in profile_buttons(active == BUILTIN, modified, target, mtm) {
        rows.push((String::new(), row));
    }
    rows.push((t!("settings.general.language").to_string(), as_view(&language)));
    rows.push((
        t!("settings.general.launch_at_login").to_string(),
        as_view(&launch),
    ));

    let show_file = button(
        &t!("settings.general.show_settings_file"),
        ID_SHOW_FILE,
        target,
        mtm,
    );
    rows.push((
        t!("settings.general.settings_file").to_string(),
        as_view(&show_file),
    ));
    form(rows, mtm)
}

pub fn changed(id: &str, sender: &AnyObject, ctx: &Context, mtm: MainThreadMarker) -> Vec<Effect> {
    if let Some(p) = id.strip_prefix(ID_PERMISSION).and_then(Permission::from_id) {
        p.open_settings(mtm);
        return Vec::new();
    }
    match id {
        ID_DRIVER_START => vec![Effect::StartDriver],
        ID_DRIVER_STOP => vec![Effect::StopDriver],
        ID_SHOW_FILE => {
            crate::macos::reveal_in_finder(&ctx.store.settings_file());
            Vec::new()
        }
        ID_PROFILE => {
            let Some(name) = popup_index(sender).and_then(|i| ctx.store.all().get(i).cloned())
            else {
                return Vec::new();
            };
            // Cancel (or a failure) puts the popup back on the active profile.
            if name == ctx.store.active() || !profiles::confirm_switch(&ctx.store, mtm) {
                return vec![Effect::Refresh];
            }
            match switch_to(&ctx.store, &name) {
                Ok(true) => PROFILE_CHANGED.to_vec(),
                Ok(false) => Vec::new(),
                Err(e) => {
                    super::report_error(mtm, &t!("settings.save_failed", error = e));
                    vec![Effect::Refresh]
                }
            }
        }
        ID_PROFILE_SAVE => {
            profiles::save_active(&ctx.store, mtm);
            vec![Effect::MenuChanged, Effect::Refresh]
        }
        ID_PROFILE_SAVE_NEW => match profiles::new_profile(&ctx.store, mtm) {
            Some(_) => PROFILE_CHANGED.to_vec(),
            None => Vec::new(),
        },
        ID_PROFILE_REVERT => match ctx.store.revert() {
            Ok(()) => PROFILE_CHANGED.to_vec(),
            Err(e) => {
                super::report_error(mtm, &t!("settings.save_failed", error = e));
                Vec::new()
            }
        },
        ID_PROFILE_RENAME => match profiles::rename_active(&ctx.store, mtm) {
            Some(_) => PROFILE_CHANGED.to_vec(),
            None => Vec::new(),
        },
        ID_PROFILE_DELETE => {
            if profiles::delete_active(&ctx.store, mtm) {
                PROFILE_CHANGED.to_vec()
            } else {
                Vec::new()
            }
        }
        ID_LANGUAGE => {
            let choices = language_choices();
            let Some(choice) = popup_index(sender).and_then(|i| choices.get(i)) else {
                return Vec::new();
            };
            if let Err(e) = ctx.store.set_language(choice.as_deref()) {
                super::report_error(mtm, &t!("settings.save_failed", error = e));
                return Vec::new();
            }
            i18n::apply(choice.as_deref());
            vec![Effect::ConfigSaved, Effect::LanguageChanged]
        }
        ID_LAUNCH => {
            let Some(on) = is_on(sender) else {
                return Vec::new();
            };
            autostart::toggle(on);
            // The menu shows the same switch; the window shows the real state.
            vec![Effect::MenuChanged]
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_start_with_system_and_include_english() {
        let c = language_choices();
        assert_eq!(c[0], None);
        assert!(c.contains(&Some("en".to_string())));
        assert!(c.contains(&Some("pl".to_string())));
    }

    #[test]
    fn selects_configured_language() {
        let c = language_choices();
        let pl = c.iter().position(|x| x.as_deref() == Some("pl")).unwrap();
        assert_eq!(selected_language(Some("pl_PL"), &c), pl);
        assert_eq!(selected_language(Some("xx"), &c), 0);
        assert_eq!(selected_language(None, &c), 0);
    }

    #[test]
    fn languages_are_named_in_themselves() {
        assert_eq!(language_title(&Some("pl".into())), "Polski");
        assert_eq!(language_title(&Some("en".into())), "English");
    }
}
