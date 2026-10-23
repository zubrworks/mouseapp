//! Resolves the config's buttons and gestures to actions, globally and per
//! application profile.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Result, bail};
use rust_i18n::t;

use crate::actions::{self, Action};
use crate::config::{Config, DEFAULT_THRESHOLD, GestureSet, Gestures, THUMB_GESTURE_BUTTON};
use crate::devices::Model;
use crate::app_overrides;

/// Actions of one gesture button, resolved from its [`GestureSet`].
#[derive(Debug)]
pub(crate) struct GestureBinding {
    /// Slot ("click", "up", ...) -> action.
    pub(crate) actions: HashMap<&'static str, Action>,
    pub(crate) threshold: u16,
}

/// Actions resolved from the config.
pub(crate) struct Bindings {
    /// CID -> action for plain diverted buttons.
    pub(crate) buttons: HashMap<u16, Action>,
    /// CID -> own gesture actions of every button that tracks movement: the
    /// thumb gesture button if any gesture is configured, and every button
    /// set to the `gestures` action.
    pub(crate) gestures: HashMap<u16, GestureBinding>,
}

/// Global bindings plus one resolved set per `[apps."..."]` profile.
pub(crate) struct BindingSet {
    pub(crate) base: Bindings,
    pub(crate) apps: HashMap<String, Bindings>,
}

impl BindingSet {
    /// Bindings for the frontmost application (global ones when it has no
    /// profile). The application is looked up only if profiles exist.
    pub(crate) fn current(&self) -> &Bindings {
        if self.apps.is_empty() {
            return &self.base;
        }
        self.for_app(app_overrides::frontmost_bundle().as_deref())
    }

    pub(crate) fn for_app(&self, bundle: Option<&str>) -> &Bindings {
        bundle.and_then(|id| self.apps.get(id)).unwrap_or(&self.base)
    }

    fn all(&self) -> impl Iterator<Item = &Bindings> {
        std::iter::once(&self.base).chain(self.apps.values())
    }

    /// Plain buttons to divert: those used by the global config or any
    /// profile, except gesture buttons (they are diverted with movement).
    pub(crate) fn button_cids(&self) -> Vec<u16> {
        let gesture = self.gesture_cids();
        let mut cids: Vec<u16> = self
            .all()
            .flat_map(|b| b.buttons.keys().copied())
            .filter(|c| !gesture.contains(c))
            .collect();
        cids.sort_unstable();
        cids.dedup();
        cids
    }

    /// Buttons to divert with movement reports: gesture buttons of the
    /// global config or any profile.
    pub(crate) fn gesture_cids(&self) -> Vec<u16> {
        let mut cids: Vec<u16> = self.all().flat_map(|b| b.gestures.keys().copied()).collect();
        cids.sort_unstable();
        cids.dedup();
        cids
    }
}

pub(crate) fn binding_set(model: &Model, cfg: &Config) -> Result<BindingSet> {
    let mut apps = HashMap::new();
    for id in cfg.apps.keys() {
        let eff = app_overrides::effective(cfg, Some(id));
        apps.insert(id.clone(), bindings(model, &eff)?);
    }
    Ok(BindingSet { base: bindings(model, &app_overrides::effective(cfg, None))?, apps })
}

/// `cfg` without the button and gesture entries whose action cannot be
/// resolved (e.g. written by a newer version), and those entries as
/// `buttons.back = "name"` for messages. Everything else is kept.
pub(crate) fn without_unknown_actions(cfg: &Config) -> (Config, Vec<String>) {
    let mut out = cfg.clone();
    let mut skipped = Vec::new();
    skip_unknown(&mut out.buttons, &mut out.gestures, "", &mut skipped);
    for (id, app) in &mut out.apps {
        let prefix = format!("apps.\"{id}\".");
        skip_unknown(&mut app.buttons, &mut app.gestures, &prefix, &mut skipped);
    }
    (out, skipped)
}

/// [`without_unknown_actions`] for one `[buttons]` / `[gestures]` pair;
/// `prefix` names the table in messages ("" or `apps."id".`).
fn skip_unknown(
    buttons: &mut BTreeMap<String, String>,
    gestures: &mut Gestures,
    prefix: &str,
    skipped: &mut Vec<String>,
) {
    buttons.retain(|key, name| {
        let ok = actions::parse(name).is_ok();
        if !ok {
            skipped.push(format!("{prefix}buttons.{key} = \"{name}\""));
        }
        ok
    });
    skip_unknown_slots(&mut gestures.thumb, &format!("{prefix}gestures"), skipped);
    for (key, set) in &mut gestures.buttons {
        skip_unknown_slots(set, &format!("{prefix}gestures.{key}"), skipped);
    }
}

/// Clears the slots of `set` whose action is unknown or `gestures`.
fn skip_unknown_slots(set: &mut GestureSet, table: &str, skipped: &mut Vec<String>) {
    for slot in GestureSet::SLOTS {
        let Some(value) = set.slot_mut(slot) else {
            continue;
        };
        if let Some(name) = value.as_deref()
            && !actions::parse(name).is_ok_and(|a| a != Action::Gestures)
        {
            skipped.push(format!("{table}.{slot} = \"{name}\""));
            *value = None;
        }
    }
}

/// Resolves the actions of one gesture button; `gestures` is refused in a slot.
fn gesture_binding(set: &GestureSet) -> Result<GestureBinding> {
    let mut actions = HashMap::new();
    for (slot, name) in set.slots() {
        if let Some(name) = name {
            let action = actions::parse(name)?;
            if action == Action::Gestures {
                bail!("{}", t!("action.button_only", name = name));
            }
            actions.insert(slot, action);
        }
    }
    Ok(GestureBinding { actions, threshold: set.threshold.unwrap_or(DEFAULT_THRESHOLD) })
}

fn bindings(model: &Model, cfg: &Config) -> Result<Bindings> {
    let cid_of = |key: &str| model.buttons.iter().find(|b| b.key == key).map(|b| b.cid);
    // Every section is checked, also of buttons not set to `gestures` now.
    for set in cfg.gestures.buttons.values() {
        gesture_binding(set)?;
    }
    let mut buttons = HashMap::new();
    let mut gestures = HashMap::new();
    for (key, name) in &cfg.buttons {
        let Some(cid) = cid_of(key) else {
            continue;
        };
        let action = actions::parse(name)?;
        if action == Action::Gestures {
            gestures.insert(cid, gesture_binding(&cfg.gestures.for_button(key))?);
        } else {
            buttons.insert(cid, action);
        }
    }
    let thumb = gesture_binding(&cfg.gestures.thumb)?;
    if !thumb.actions.is_empty()
        && let Some(cid) = cid_of(THUMB_GESTURE_BUTTON)
    {
        gestures.insert(cid, thumb);
    }
    Ok(Bindings { buttons, gestures })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices;

    const THUMB: u16 = 0x00C3;
    const MIDDLE: u16 = 0x0052;

    fn gesture_cids(b: &Bindings) -> Vec<u16> {
        let mut cids: Vec<u16> = b.gestures.keys().copied().collect();
        cids.sort_unstable();
        cids
    }

    fn parse(text: &str) -> Config {
        Config::parse(text, std::path::Path::new("t.toml")).unwrap()
    }

    #[test]
    fn binds_buttons_and_gestures() {
        let cfg = Config::parse(crate::config::EXAMPLE, std::path::Path::new("t.toml")).unwrap();
        let b = bindings(&devices::mx_master_3s::MODEL, &cfg).unwrap();
        assert!(b.buttons.contains_key(&0x0053)); // back
        assert_eq!(gesture_cids(&b), vec![THUMB]);
        assert_eq!(b.gestures[&THUMB].actions.len(), 5);
        assert_eq!(b.gestures[&THUMB].threshold, 50);
    }

    #[test]
    fn profiles_override_per_app() {
        let text = "[buttons]\nback = \"back\"\n[apps.\"com.apple.Safari\".buttons]\nback = \"desktop_left\"\n[apps.\"com.apple.Safari\".gestures]\nup = \"app_expose\"\n";
        let cfg = Config::parse(text, std::path::Path::new("t.toml")).unwrap();
        let set = binding_set(&devices::mx_master_3s::MODEL, &cfg).unwrap();
        let back = 0x0053;
        let safari = set.for_app(Some("com.apple.Safari"));
        let finder = set.for_app(Some("com.apple.finder"));
        assert_ne!(format!("{:?}", safari.buttons[&back]), format!("{:?}", finder.buttons[&back]));
        assert_eq!(set.for_app(None).buttons[&back], finder.buttons[&back]);
        assert!(safari.gestures[&THUMB].actions.contains_key("up"));
        assert!(!finder.gestures.contains_key(&THUMB));
        assert_eq!(set.gesture_cids(), vec![THUMB]);
        assert_eq!(set.button_cids(), vec![back]);
    }

    #[test]
    fn rejects_unknown_action() {
        let cfg = Config::parse("[buttons]\nback = \"nope\"", std::path::Path::new("t.toml")).unwrap();
        assert!(bindings(&devices::mx_master_3s::MODEL, &cfg).is_err());
    }

    #[test]
    fn unknown_actions_are_skipped_and_the_rest_kept() {
        let text = "dpi = 2400\n[buttons]\nback = \"nope\"\nforward = \"copy\"\n[gestures]\nup = \"future\"\ndown = \"gestures\"\nleft = \"mute\"\n[gestures.middle]\nclick = \"zzz\"\n[apps.\"x\".buttons]\nback = \"later\"\nforward = \"paste\"\n";
        let cfg = parse(text);
        assert!(binding_set(&devices::mx_master_3s::MODEL, &cfg).is_err());
        let (usable, skipped) = without_unknown_actions(&cfg);
        assert_eq!(
            skipped,
            [
                "buttons.back = \"nope\"",
                "gestures.up = \"future\"",
                "gestures.down = \"gestures\"",
                "gestures.middle.click = \"zzz\"",
                "apps.\"x\".buttons.back = \"later\"",
            ]
        );
        binding_set(&devices::mx_master_3s::MODEL, &usable).unwrap();
        assert_eq!(usable.dpi, Some(2400));
        assert_eq!(usable.buttons.keys().collect::<Vec<_>>(), ["forward"]);
        let g = &usable.gestures.thumb;
        assert_eq!((g.up.as_deref(), g.down.as_deref(), g.left.as_deref()), (None, None, Some("mute")));
        assert!(usable.gestures.buttons["middle"].click.is_none());
        assert_eq!(usable.apps["x"].buttons.keys().collect::<Vec<_>>(), ["forward"]);
        // Nothing to skip: unchanged.
        assert_eq!(without_unknown_actions(&usable), (usable.clone(), Vec::new()));
    }

    #[test]
    fn no_gestures_means_no_gesture_diversion() {
        let cfg = Config::parse("", std::path::Path::new("t.toml")).unwrap();
        let b = bindings(&devices::mx_master_3s::MODEL, &cfg).unwrap();
        assert!(b.gestures.is_empty());
        assert!(b.buttons.is_empty());
    }

    #[test]
    fn any_button_can_be_a_gesture_button() {
        let b = bindings(&devices::mx_master_3s::MODEL, &parse("[buttons]\nmiddle = \"gestures\"")).unwrap();
        assert_eq!(gesture_cids(&b), vec![MIDDLE]);
        assert!(b.buttons.is_empty());
        // No section of its own: the standard set.
        let standard = gesture_binding(&GestureSet::standard()).unwrap();
        assert_eq!(b.gestures[&MIDDLE].actions, standard.actions);
        // Together with the thumb button once a gesture is configured.
        let text = "[buttons]\nmiddle = \"gestures\"\n[gestures]\nup = \"mission_control\"";
        let b = bindings(&devices::mx_master_3s::MODEL, &parse(text)).unwrap();
        assert_eq!(gesture_cids(&b), vec![MIDDLE, THUMB]);
    }

    #[test]
    fn every_gesture_button_has_its_own_set() {
        let text = "[buttons]\nmiddle = \"gestures\"\n[gestures]\nup = \"mission_control\"\nthreshold = 40\n[gestures.middle]\nup = \"volume_up\"\nthreshold = 90\n";
        let b = bindings(&devices::mx_master_3s::MODEL, &parse(text)).unwrap();
        let (thumb, middle) = (&b.gestures[&THUMB], &b.gestures[&MIDDLE]);
        assert_eq!(thumb.actions["up"], actions::parse("mission_control").unwrap());
        assert_eq!(middle.actions["up"], actions::parse("volume_up").unwrap());
        assert_eq!((thumb.threshold, middle.threshold), (40, 90));
        // Only what the section sets: no click, no other directions.
        assert_eq!(middle.actions.len(), 1);
        // A per-app section is merged over the global one.
        let text = format!("{text}[apps.\"com.apple.Safari\".gestures.middle]\ndown = \"mute\"\n");
        let set = binding_set(&devices::mx_master_3s::MODEL, &parse(&text)).unwrap();
        let safari = &set.for_app(Some("com.apple.Safari")).gestures[&MIDDLE];
        assert_eq!(safari.actions.len(), 2);
        assert_eq!(safari.threshold, 90);
        assert_eq!(set.for_app(None).gestures[&MIDDLE].actions.len(), 1);
    }

    #[test]
    fn gesture_button_in_one_profile_is_diverted_with_movement() {
        let text = "[buttons]\nmiddle = \"back\"\n[apps.\"com.apple.Safari\".buttons]\nmiddle = \"gestures\"\n";
        let cfg = Config::parse(text, std::path::Path::new("t.toml")).unwrap();
        let set = binding_set(&devices::mx_master_3s::MODEL, &cfg).unwrap();
        assert_eq!(set.gesture_cids(), vec![0x0052]);
        assert!(set.button_cids().is_empty());
        assert!(set.for_app(None).buttons.contains_key(&0x0052));
    }

    #[test]
    fn gestures_action_is_rejected_in_a_gesture_slot() {
        for text in ["[gestures]\nup = \"gestures\"", "[gestures.back]\nclick = \"gestures\""] {
            assert!(bindings(&devices::mx_master_3s::MODEL, &parse(text)).is_err(), "{text}");
        }
    }
}
