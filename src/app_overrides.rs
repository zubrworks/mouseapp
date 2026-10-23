//! Per-application settings profiles: finds the frontmost application and
//! merges its `[apps."<bundle id>"]` section over the global config.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSRunningApplication, NSWorkspace, NSWorkspaceApplicationKey,
    NSWorkspaceDidActivateApplicationNotification,
};
use objc2_foundation::{NSNotification, NSObjectProtocol};

use crate::config::{Config, GestureSet};

/// The frontmost application is re-read at most this often.
const CACHE_TTL: Duration = Duration::from_millis(250);

static CACHE: Mutex<Option<(Instant, Option<String>)>> = Mutex::new(None);

/// Bundle id of the active application, kept up to date by a
/// [`FrontmostWatch`]; the outer `None` means nothing is watching.
static WATCHED: Mutex<Option<Option<String>>> = Mutex::new(None);

/// Bundle id of the frontmost application (`None` if unknown). Cheap enough
/// to call per event: the daemon reads the value kept by its
/// [`FrontmostWatch`], other commands a short-lived cached lookup.
pub fn frontmost_bundle() -> Option<String> {
    if let Some(id) = crate::util::lock(&WATCHED).as_ref() {
        return id.clone();
    }
    let now = Instant::now();
    let mut cache = crate::util::lock(&CACHE);
    if let Some((at, id)) = cache.as_ref()
        && now.saturating_duration_since(*at) < CACHE_TTL
    {
        return id.clone();
    }
    let id = query_frontmost();
    *cache = Some((now, id.clone()));
    id
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; `ActivationObserver`
    // has no Drop.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "MouseActivationObserver"]
    struct ActivationObserver;

    // SAFETY: NSObjectProtocol has no safety requirements.
    unsafe impl NSObjectProtocol for ActivationObserver {}

    impl ActivationObserver {
        /// `NSWorkspaceDidActivateApplicationNotification`: another
        /// application came to the front.
        #[unsafe(method(activated:))]
        fn activated(&self, note: &NSNotification) {
            // SAFETY: reading an immutable AppKit constant.
            let key: &AnyObject = unsafe { NSWorkspaceApplicationKey };
            let app = note
                .userInfo()
                .and_then(|info| info.objectForKey(key))
                .and_then(|app| app.downcast::<NSRunningApplication>().ok());
            let id = match app {
                Some(app) => bundle_of(&app),
                // No application in the notification: ask the workspace.
                None => NSWorkspace::sharedWorkspace()
                    .frontmostApplication()
                    .and_then(|app| bundle_of(&app)),
            };
            log::debug!("frontmost application: {}", id.as_deref().unwrap_or("?"));
            *crate::util::lock(&WATCHED) = Some(id);
        }
    }
);

fn bundle_of(app: &NSRunningApplication) -> Option<String> {
    app.bundleIdentifier().map(|s| s.to_string())
}

/// Follows the frontmost application through `NSWorkspace` notifications,
/// so [`frontmost_bundle`] needs no lookup (no WindowServer request) per
/// event. The notifications arrive on the main run loop, which must run
/// (the daemon's event loop). Dropping it stops following.
pub struct FrontmostWatch {
    observer: Retained<ActivationObserver>,
}

impl FrontmostWatch {
    pub fn start(mtm: MainThreadMarker) -> FrontmostWatch {
        let observer: Retained<ActivationObserver> =
            // SAFETY: NSObject's `init` has this signature.
            unsafe { msg_send![ActivationObserver::alloc(mtm), init] };
        let workspace = NSWorkspace::sharedWorkspace();
        let id = workspace.frontmostApplication().and_then(|app| bundle_of(&app));
        *crate::util::lock(&WATCHED) = Some(id);
        // SAFETY: `activated:` takes one NSNotification and returns nothing;
        // the observer is removed (Drop) before it is released.
        unsafe {
            workspace.notificationCenter().addObserver_selector_name_object(
                &observer,
                sel!(activated:),
                Some(NSWorkspaceDidActivateApplicationNotification),
                None,
            );
        }
        FrontmostWatch { observer }
    }
}

impl Drop for FrontmostWatch {
    fn drop(&mut self) {
        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        // SAFETY: the observer was added by `start`.
        unsafe { center.removeObserver(&self.observer) };
        *crate::util::lock(&WATCHED) = None;
    }
}

/// Frontmost app = owner of the top-most normal window. Unlike
/// `NSWorkspace::frontmostApplication` this needs no running main run loop,
/// so it works in commands without one. It talks to WindowServer on every
/// call; the daemon uses a [`FrontmostWatch`] instead.
fn query_frontmost() -> Option<String> {
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use core_graphics::window::{
        copy_window_info, kCGNullWindowID, kCGWindowListExcludeDesktopElements,
        kCGWindowListOptionOnScreenOnly,
    };
    let list = copy_window_info(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
        kCGNullWindowID,
    )?;
    let (k_layer, k_pid) = (CFString::new("kCGWindowLayer"), CFString::new("kCGWindowOwnerPID"));
    for item in list.iter() {
        // SAFETY: every element of the window info array is a CFDictionary.
        let dict: CFDictionary<CFString, CFType> =
            unsafe { CFDictionary::wrap_under_get_rule(*item as *const _) };
        let num = |k: &CFString| {
            dict.find(k)
                .and_then(|v| v.downcast::<CFNumber>())
                .and_then(|n| n.to_i64())
        };
        if num(&k_layer) != Some(0) {
            continue;
        }
        // A window that cannot be attributed does not stop the scan.
        let Some(pid) = num(&k_pid) else {
            continue;
        };
        let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid as i32)
        else {
            continue;
        };
        return bundle_of(&app);
    }
    None
}

/// Gesture actions of `over` replacing those of `base`, slot by slot.
fn merge_set(over: &GestureSet, base: &GestureSet) -> GestureSet {
    let mut out = GestureSet { threshold: over.threshold.or(base.threshold), ..GestureSet::default() };
    for slot in GestureSet::SLOTS {
        if let Some(target) = out.slot_mut(slot) {
            *target = over.get(slot).or(base.get(slot)).map(str::to_string);
        }
    }
    out
}

/// The global config with the profile of `bundle` applied: buttons and
/// gesture actions given in the profile replace the global ones, the rest
/// stays (a button's gesture section is merged over the global one, or over
/// the standard set if there is none). Unknown bundle ids give an unchanged
/// copy.
pub fn effective(cfg: &Config, bundle: Option<&str>) -> Config {
    let mut out = cfg.clone();
    out.apps.clear();
    let Some(app) = bundle.and_then(|b| cfg.apps.get(b)) else {
        return out;
    };
    out.buttons
        .extend(app.buttons.iter().map(|(k, v)| (k.clone(), v.clone())));
    let (g, o) = (&cfg.gestures, &app.gestures);
    out.gestures.thumb = merge_set(&o.thumb, &g.thumb);
    for (key, set) in &o.buttons {
        let merged = merge_set(set, &g.for_button(key));
        out.gestures.buttons.insert(key.clone(), merged);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const TEXT: &str = r#"
[buttons]
back = "back"
forward = "forward"
[gestures]
up = "mission_control"
down = "app_expose"
threshold = 60
[apps."com.apple.Safari".buttons]
back = "desktop_left"
[apps."com.apple.Safari".gestures]
up = "app_expose"
[apps."com.apple.Safari".gestures.middle]
down = "copy"
[apps."com.apple.finder".buttons]
back = "mission_control"
"#;

    fn cfg() -> Config {
        Config::parse(TEXT, Path::new("t.toml")).unwrap()
    }

    #[test]
    fn safari_and_finder_differ() {
        let c = cfg();
        let s = effective(&c, Some("com.apple.Safari"));
        let f = effective(&c, Some("com.apple.finder"));
        assert_eq!(s.buttons["back"], "desktop_left");
        assert_eq!(f.buttons["back"], "mission_control");
        assert_eq!(s.gestures.thumb.up.as_deref(), Some("app_expose"));
        assert_eq!(f.gestures.thumb.up.as_deref(), Some("mission_control"));
    }

    #[test]
    fn unspecified_values_come_from_global() {
        let s = effective(&cfg(), Some("com.apple.Safari"));
        assert_eq!(s.buttons["forward"], "forward");
        assert_eq!(s.gestures.thumb.down.as_deref(), Some("app_expose"));
        assert_eq!(s.gestures.thumb.threshold, Some(60));
        assert!(s.apps.is_empty());
        // A button's section is merged over the standard set (no global one).
        let mut middle = GestureSet::standard();
        middle.down = Some("copy".into());
        assert_eq!(s.gestures.for_button("middle"), middle);
    }

    #[test]
    fn unknown_or_missing_app_is_global() {
        let c = cfg();
        let mut global = c.clone();
        global.apps.clear();
        assert_eq!(effective(&c, Some("org.example.none")), global);
        assert_eq!(effective(&c, None), global);
    }

    #[test]
    fn frontmost_lookup_does_not_panic() {
        // Value depends on the environment (may be None without a GUI session).
        let _ = frontmost_bundle();
    }
}
