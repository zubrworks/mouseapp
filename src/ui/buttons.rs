//! Buttons tab: the action of every remappable button.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::MainThreadMarker;
use objc2_app_kit::NSView;
use rust_i18n::t;

use super::{
    ActionChoice, Context, Effect, action_choice, action_popup, as_view, form, read_config,
    remappable_buttons, update_config,
};

const PREFIX: &str = "buttons.";

pub fn build(ctx: &Context, target: &AnyObject, mtm: MainThreadMarker) -> Retained<NSView> {
    let cfg = read_config(ctx);
    let unset = t!("settings.buttons.default").to_string();
    let rows = remappable_buttons(ctx)
        .into_iter()
        .map(|b| {
            let id = format!("{PREFIX}{}", b.key);
            let popup = action_popup(
                cfg.buttons.get(b.key).map(String::as_str),
                true,
                &unset,
                &id,
                target,
                mtm,
            );
            (format!("{}:", b.label()), as_view(&popup))
        })
        .collect();
    form(rows, mtm)
}

pub fn changed(id: &str, sender: &AnyObject, ctx: &Context, mtm: MainThreadMarker) -> Vec<Effect> {
    let Some(key) = id.strip_prefix(PREFIX) else {
        return Vec::new();
    };
    if !remappable_buttons(ctx).iter().any(|b| b.key == key) {
        return Vec::new();
    }
    let current = read_config(ctx).buttons.get(key).cloned();
    match action_choice(sender, current.as_deref(), true, mtm) {
        ActionChoice::Set(action) => update_config(ctx, mtm, |c| match action {
            Some(a) => {
                c.buttons.insert(key.to_string(), a);
            }
            None => {
                c.buttons.remove(key);
            }
        }),
        // Cancelled: `dispatch` shows the real value again.
        ActionChoice::Keep => Vec::new(),
    }
}
