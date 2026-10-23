//! Gestures tab: actions and threshold of a gesture button. The thumb
//! gesture button and every button set to the `gestures` action have their
//! own set; a popup chooses which one is edited.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::MainThreadMarker;
use objc2_app_kit::NSView;
use rust_i18n::t;

use super::{
    ActionChoice, Context, Effect, action_choice, action_popup, as_view, form, hint_label, popup,
    popup_index, read_config, remappable_buttons, slider, slider_value, tr_ns, update_config,
};
use crate::config::{
    Config, DEFAULT_THRESHOLD, GESTURE_THRESHOLD_RANGE, GestureSet, THUMB_GESTURE_BUTTON, slider_bounds,
};

const PREFIX: &str = "gestures.";
const ID_THRESHOLD: &str = "gestures.threshold";
const ID_BUTTON: &str = "gestures.button";

/// State of the Gestures tab, kept in the window [`Context`].
pub struct GesturesState {
    /// Key of the gesture button being edited.
    selected: RefCell<String>,
}

impl Default for GesturesState {
    fn default() -> Self {
        GesturesState { selected: RefCell::new(THUMB_GESTURE_BUTTON.to_string()) }
    }
}

/// Keys of the buttons with gestures: the thumb button, then every
/// remappable button whose action is `gestures`.
fn gesture_buttons(ctx: &Context, cfg: &Config) -> Vec<&'static str> {
    let mut keys = vec![THUMB_GESTURE_BUTTON];
    keys.extend(
        remappable_buttons(ctx)
            .into_iter()
            .filter(|b| cfg.buttons.get(b.key).is_some_and(|a| a == "gestures"))
            .map(|b| b.key),
    );
    keys
}

/// The edited button: the selection if it still has gestures, else the thumb button.
fn selected(ctx: &Context, keys: &[&'static str]) -> &'static str {
    let current = ctx.gestures.selected.borrow().clone();
    keys.iter().copied().find(|k| *k == current).unwrap_or(THUMB_GESTURE_BUTTON)
}

pub fn build(ctx: &Context, target: &AnyObject, mtm: MainThreadMarker) -> Retained<NSView> {
    let cfg = read_config(ctx);
    let keys = gesture_buttons(ctx, &cfg);
    let key = selected(ctx, &keys);
    let titles: Vec<String> = keys.iter().map(|k| t!(format!("button.{k}")).to_string()).collect();
    let index = keys.iter().position(|k| *k == key).unwrap_or(0);
    let button = popup(&titles, index, ID_BUTTON, target, mtm);
    let hint = hint_label(&tr_ns("settings.gestures.hint"), mtm);
    let mut rows: Vec<(String, Retained<NSView>)> = vec![
        (t!("settings.gestures.button").to_string(), as_view(&button)),
        (String::new(), as_view(&hint)),
    ];
    let g = cfg.gestures.for_button(key);
    let unset = t!("settings.gestures.none").to_string();
    rows.extend(GestureSet::SLOTS.iter().map(|slot| {
        let popup = action_popup(g.get(slot), false, &unset, &format!("{PREFIX}{slot}"), target, mtm);
        (
            t!(format!("settings.gestures.{slot}")).to_string(),
            as_view(&popup),
        )
    }));
    let threshold = g.threshold.unwrap_or(DEFAULT_THRESHOLD);
    rows.push((
        t!("settings.gestures.threshold").to_string(),
        slider(slider_bounds(&GESTURE_THRESHOLD_RANGE), f64::from(threshold), &threshold.to_string(), ID_THRESHOLD, target, mtm),
    ));
    form(rows, mtm)
}

pub fn changed(id: &str, sender: &AnyObject, ctx: &Context, mtm: MainThreadMarker) -> Vec<Effect> {
    let cfg = read_config(ctx);
    let keys = gesture_buttons(ctx, &cfg);
    let key = selected(ctx, &keys);
    match id {
        ID_BUTTON => {
            if let Some(k) = popup_index(sender).and_then(|i| keys.get(i)) {
                *ctx.gestures.selected.borrow_mut() = k.to_string();
            }
            Vec::new()
        }
        ID_THRESHOLD => {
            let Some(v) = slider_value(sender, 1.0) else {
                return Vec::new();
            };
            let (min, max) = slider_bounds(&GESTURE_THRESHOLD_RANGE);
            let v = v.clamp(min, max) as u16;
            update_config(ctx, mtm, |c| c.gestures.for_button_mut(key).threshold = Some(v))
        }
        _ => {
            let Some(slot) = id.strip_prefix(PREFIX).filter(|s| GestureSet::SLOTS.contains(s)) else {
                return Vec::new();
            };
            let current = cfg.gestures.for_button(key).get(slot).map(str::to_string);
            match action_choice(sender, current.as_deref(), false, mtm) {
                ActionChoice::Set(action) => {
                    update_config(ctx, mtm, |c| {
                        if let Some(s) = c.gestures.for_button_mut(key).slot_mut(slot) {
                            *s = action;
                        }
                    })
                }
                ActionChoice::Keep => Vec::new(),
            }
        }
    }
}
