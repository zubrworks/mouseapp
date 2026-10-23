//! Reads macOS global preferences (the `-g` domain of `defaults`) through
//! CoreFoundation instead of running the `defaults` tool.

use core_foundation::array::CFArray;
use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFPreferencesAnyApplication: CFStringRef;
    fn CFPreferencesCopyAppValue(key: CFStringRef, application: CFStringRef) -> CFTypeRef;
}

/// The value of `key` in the global domain, if set.
fn global_value(key: &str) -> Option<CFType> {
    let key = CFString::new(key);
    // SAFETY: both arguments are valid CFStrings; a non-null result follows
    // the "Copy" rule (we own it) and is released by `CFType`.
    unsafe {
        let value = CFPreferencesCopyAppValue(key.as_concrete_TypeRef(), kCFPreferencesAnyApplication);
        (!value.is_null()).then(|| CFType::wrap_under_create_rule(value))
    }
}

/// A global boolean setting (stored as a boolean or a 0/1 number).
pub fn global_bool(key: &str) -> Option<bool> {
    let value = global_value(key)?;
    if let Some(b) = value.downcast::<CFBoolean>() {
        return Some(b.into());
    }
    value.downcast::<CFNumber>()?.to_i64().map(|n| n != 0)
}

/// A global array of strings (other elements are skipped).
pub fn global_strings(key: &str) -> Option<Vec<String>> {
    let array = global_value(key)?.downcast_into::<CFArray>()?;
    Some(
        array
            .iter()
            // SAFETY: every element of a CFArray is a valid CFType.
            .filter_map(|v| unsafe { CFType::wrap_under_get_rule(*v as CFTypeRef) }.downcast::<CFString>())
            .map(|s| s.to_string())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_key_is_none() {
        assert_eq!(global_bool("mouse.test.no.such.key"), None);
        assert_eq!(global_strings("mouse.test.no.such.key"), None);
    }

    #[test]
    fn wrong_type_is_none() {
        // AppleLanguages is an array, not a boolean.
        assert_eq!(global_bool("AppleLanguages"), None);
    }
}
