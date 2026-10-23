//! Profile dialogs: name a new or renamed profile and confirm a deletion
//! (used by the General tab), and simple alerts and text prompts.

use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSAccessibility, NSAlert, NSAlertFirstButtonReturn, NSTextField, NSView};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use rust_i18n::t;

use crate::profiles::{BUILTIN, Store, display_name};

/// Which profile buttons can be used.
#[derive(Debug, PartialEq, Eq)]
pub struct ButtonStates {
    pub save: bool,
    pub save_new: bool,
    pub revert: bool,
    pub rename: bool,
    pub delete: bool,
}

/// States of the profile buttons: the built-in profile cannot be saved to,
/// renamed or deleted; saving and reverting need unsaved changes.
pub fn button_states(builtin: bool, modified: bool) -> ButtonStates {
    ButtonStates {
        save: !builtin && modified,
        save_new: true,
        revert: modified,
        rename: !builtin,
        delete: !builtin,
    }
}

/// Answer to the question about unsaved changes.
#[derive(Debug, PartialEq, Eq)]
enum Unsaved {
    Save,
    Discard,
    Cancel,
}

/// Maps the pressed alert button (0 = first) to the answer.
fn unsaved_answer(index: usize) -> Unsaved {
    match index {
        0 => Unsaved::Save,
        1 => Unsaved::Discard,
        _ => Unsaved::Cancel,
    }
}

/// Before another profile replaces the current settings: if they have
/// unsaved changes, asks Save / Don't Save / Cancel. For the built-in
/// profile (which cannot be saved to) "Save" is "Save as New...". True if
/// the switch can go on.
pub fn confirm_switch(store: &Store, mtm: MainThreadMarker) -> bool {
    confirm_unsaved(store, mtm, store.active() == BUILTIN)
}

/// The question of [`confirm_switch`]; with `only_new` "Save" is always
/// "Save as New..." (for a profile that is about to be deleted).
fn confirm_unsaved(store: &Store, mtm: MainThreadMarker, only_new: bool) -> bool {
    if !store.is_modified() {
        return true;
    }
    let active = store.active();
    let builtin = only_new;
    let first = if builtin { t!("profiles.save_new") } else { t!("profiles.save") };
    let buttons = [first.to_string(), t!("profiles.discard").to_string(), t!("common.cancel").to_string()];
    let a = alert(
        mtm,
        &t!("profiles.unsaved_title", name = display_name(&active)),
        &t!("profiles.unsaved_hint"),
        &buttons,
    );
    let index = (a.runModal() - NSAlertFirstButtonReturn) as usize;
    match unsaved_answer(index) {
        Unsaved::Cancel => false,
        Unsaved::Discard => true,
        Unsaved::Save if builtin => new_profile(store, mtm).is_some(),
        Unsaved::Save => save_active(store, mtm),
    }
}

/// "Save to Profile": writes the current settings into the active profile.
/// True if saved (an error is shown).
pub fn save_active(store: &Store, mtm: MainThreadMarker) -> bool {
    match store.save_to_profile() {
        Ok(()) => true,
        Err(e) => {
            show_error(mtm, &t!("profiles.error_title"), &e);
            false
        }
    }
}

/// An alert with a message, a hint and buttons (the first is the default).
fn alert(mtm: MainThreadMarker, title: &str, hint: &str, buttons: &[String]) -> objc2::rc::Retained<NSAlert> {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(hint));
    for b in buttons {
        alert.addButtonWithTitle(&NSString::from_str(b));
    }
    alert
}

/// Shows `message` in an alert titled `title`.
pub fn show_error(mtm: MainThreadMarker, title: &str, message: &str) {
    alert(mtm, title, message, &[t!("common.ok").to_string()]).runModal();
}

/// Asks for a line of text in an alert with `title` and `hint`; `buttons` are
/// the titles of OK and Cancel. Returns the text, or `None` if cancelled.
pub fn prompt_text(
    mtm: MainThreadMarker,
    title: &str,
    hint: &str,
    buttons: (&str, &str),
    initial: &str,
    placeholder: Option<&str>,
) -> Option<String> {
    let a = alert(mtm, title, hint, &[buttons.0.to_string(), buttons.1.to_string()]);
    let field = NSTextField::initWithFrame(
        NSTextField::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(260.0, 24.0)),
    );
    field.setStringValue(&NSString::from_str(initial));
    field.setPlaceholderString(placeholder.map(NSString::from_str).as_deref());
    field.setAccessibilityLabel(Some(&NSString::from_str(title)));
    a.setAccessoryView(Some(&field));
    let view: &NSView = &field;
    a.window().setInitialFirstResponder(Some(view));
    if a.runModal() != NSAlertFirstButtonReturn {
        return None;
    }
    Some(field.stringValue().to_string())
}

/// Asks for a profile name (prefilled with `initial`) and passes it to
/// `apply`; a rejected name shows the reason and asks again. Returns what
/// `apply` returned, or `None` if cancelled.
fn ask_name(
    mtm: MainThreadMarker,
    title: &str,
    hint: &str,
    ok: &str,
    initial: &str,
    apply: impl Fn(&str) -> Result<String, String>,
) -> Option<String> {
    let mut text = initial.to_string();
    loop {
        text = prompt_text(mtm, title, hint, (ok, &t!("common.cancel")), &text, None)?;
        match apply(&text) {
            Ok(name) => return Some(name),
            Err(e) => show_error(mtm, &t!("profiles.error_title"), &e),
        }
    }
}

/// "Save as New...": saves the current settings as a new profile under a name the
/// user gives; it becomes active. Returns its name.
pub fn new_profile(store: &Store, mtm: MainThreadMarker) -> Option<String> {
    let from = display_name(&store.active());
    let suggested = store.unique_name(&t!("profiles.copy_name", name = from));
    ask_name(
        mtm,
        &t!("profiles.new_title"),
        &t!("profiles.new_hint"),
        &t!("profiles.create"),
        &suggested,
        |name| store.save_as_new(name),
    )
}

/// "Rename...": renames the active user profile.
pub fn rename_active(store: &Store, mtm: MainThreadMarker) -> Option<String> {
    let old = store.active();
    if old == BUILTIN {
        return None;
    }
    ask_name(
        mtm,
        &t!("profiles.rename_title", name = old),
        "",
        &t!("profiles.rename"),
        &old,
        |name| store.rename(&old, name),
    )
}

/// "Delete...": deletes the active user profile after a confirmation; the
/// built-in profile becomes active. True if deleted.
pub fn delete_active(store: &Store, mtm: MainThreadMarker) -> bool {
    let name = store.active();
    if name == BUILTIN {
        return false;
    }
    let buttons = [t!("profiles.delete").to_string(), t!("common.cancel").to_string()];
    let hint = if store.is_modified() { t!("profiles.delete_hint_changes") } else { t!("profiles.delete_hint") };
    let a = alert(mtm, &t!("profiles.delete_title", name = name), &hint, &buttons);
    if a.runModal() != NSAlertFirstButtonReturn {
        return false;
    }
    // The unsaved changes go with the profile: offer to keep them as a new one.
    if !confirm_unsaved(store, mtm, true) {
        return false;
    }
    match store.delete(&name) {
        Ok(()) => true,
        Err(e) => {
            show_error(mtm, &t!("profiles.error_title"), &e);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_needs_a_user_profile_with_changes() {
        let s = button_states(false, true);
        assert!(s.save && s.revert && s.save_new && s.rename && s.delete);
        let s = button_states(false, false);
        assert!(!s.save && !s.revert && s.save_new);
        // The built-in profile: nothing to save to, rename or delete.
        let s = button_states(true, true);
        assert!(!s.save && !s.rename && !s.delete && s.revert && s.save_new);
        let s = button_states(true, false);
        assert!(!s.save && !s.revert && s.save_new);
    }

    #[test]
    fn alert_buttons_map_to_answers() {
        assert_eq!(unsaved_answer(0), Unsaved::Save);
        assert_eq!(unsaved_answer(1), Unsaved::Discard);
        assert_eq!(unsaved_answer(2), Unsaved::Cancel);
    }
}
