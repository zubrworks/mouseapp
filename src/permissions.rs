//! macOS privacy permissions the app depends on: Accessibility (synthetic
//! input, scroll tap) and Input Monitoring (HID access to the mouse).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::string::{CFString, CFStringRef};
use objc2::MainThreadMarker;
use rust_i18n::t;
use serde::{Deserialize, Serialize};

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOHIDCheckAccess(request: u32) -> u32;
    fn IOHIDRequestAccess(request: u32) -> bool;
}

/// `kIOHIDRequestTypeListenEvent`.
const REQUEST_LISTEN_EVENT: u32 = 1;
/// `kIOHIDAccessTypeGranted`.
const ACCESS_GRANTED: u32 = 0;

/// A permission that can be missing (serialized as its [`Permission::id`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Accessibility,
    InputMonitoring,
}

impl Permission {
    /// Every permission, in display order.
    pub const ALL: [Permission; 2] = [Permission::Accessibility, Permission::InputMonitoring];

    /// Stable id used for menu items and window controls.
    pub fn id(self) -> &'static str {
        match self {
            Permission::Accessibility => "accessibility",
            Permission::InputMonitoring => "input_monitoring",
        }
    }

    pub fn from_id(id: &str) -> Option<Permission> {
        Permission::ALL.into_iter().find(|p| p.id() == id)
    }

    /// Localized name of the permission.
    pub fn name(self) -> String {
        match self {
            Permission::Accessibility => t!("permissions.accessibility").to_string(),
            Permission::InputMonitoring => t!("permissions.input_monitoring").to_string(),
        }
    }

    fn settings_url(self) -> &'static str {
        match self {
            Permission::Accessibility => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
            }
            Permission::InputMonitoring => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent"
            }
        }
    }

    /// Is the permission granted right now?
    pub fn granted(self) -> bool {
        match self {
            Permission::Accessibility => accessibility_granted(),
            // SAFETY: plain C call with an enum value; no pointers.
            Permission::InputMonitoring => unsafe {
                IOHIDCheckAccess(REQUEST_LISTEN_EVENT) == ACCESS_GRANTED
            },
        }
    }

    /// Opens the matching System Settings pane.
    pub fn open_settings(self, _mtm: MainThreadMarker) {
        if !crate::macos::open_url(self.settings_url()) {
            log::warn!("cannot open System Settings");
        }
    }
}

/// True when this process may post synthetic input events.
pub fn accessibility_granted() -> bool {
    // SAFETY: plain C call without arguments.
    unsafe { AXIsProcessTrusted() }
}

/// How long [`missing`] reuses its last answer.
const MISSING_TTL: Duration = Duration::from_secs(2);

/// Permissions that are not granted, in display order. The answer is kept
/// for [`MISSING_TTL`] because the status request asks for it often.
pub fn missing() -> Vec<Permission> {
    static CACHE: Mutex<Option<(Instant, Vec<Permission>)>> = Mutex::new(None);
    let mut cache = crate::util::lock(&CACHE);
    if let Some((at, list)) = cache.as_ref()
        && at.elapsed() < MISSING_TTL
    {
        return list.clone();
    }
    let list = missing_now();
    *cache = Some((Instant::now(), list.clone()));
    list
}

/// Permissions that are not granted right now (no cache).
fn missing_now() -> Vec<Permission> {
    Permission::ALL
        .into_iter()
        .filter(|p| !p.granted())
        .collect()
}

/// Asks macOS for the missing permissions: shows the system prompts and adds
/// this app to both lists in System Settings, so the user only flips the switch.
pub fn request_missing() {
    for p in missing_now() {
        match p {
            Permission::Accessibility => {
                // SAFETY: the key is a constant CFString from the framework.
                let key = unsafe { CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt) };
                let options = CFDictionary::from_CFType_pairs(&[(key, CFBoolean::true_value())]);
                // SAFETY: valid dictionary that outlives the call.
                unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) };
            }
            // SAFETY: plain C call with an enum value; no pointers.
            Permission::InputMonitoring => unsafe {
                IOHIDRequestAccess(REQUEST_LISTEN_EVENT);
            },
        }
        log::info!("asked macOS for permission: {}", p.id());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        for p in Permission::ALL {
            assert_eq!(Permission::from_id(p.id()), Some(p));
        }
        assert_eq!(Permission::from_id("x"), None);
    }

    #[test]
    fn panes_differ() {
        assert_ne!(
            Permission::Accessibility.settings_url(),
            Permission::InputMonitoring.settings_url()
        );
    }
}
