//! A fake trackpad "dock swipe": the three/four-finger gesture that moves
//! between desktops and follows the fingers. Used by the gesture button with
//! `gestures.trackpad_swipe` (see `events`): Began, Changed with the
//! progress, then Ended or Cancelled go to the Dock.
//!
//! On macOS 27 the Dock no longer reads the gesture data from undocumented
//! `CGEvent` fields (the approach used before by e.g. Mac Mouse Fix); the
//! data has to be an `IOHIDEvent` of type DockSwipe attached to a `CGEvent`
//! of type 30 (dock control). The IOHIDEvent functions and
//! `SLEventSetIOHIDEvent` are private, so they are looked up at run time; a
//! missing one is logged once and the gestures fall back to the keys.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::OnceLock;

use rust_i18n::t;

/// Dock progress of one whole desktop (macOS 13 and later).
const ONE_DESKTOP: f64 = 4.0 / 3.0;

/// Sign of the Dock progress towards the desktop on the right (fingers
/// moving left, i.e. negative, bring the right desktop in).
const RIGHT_SIGN: f64 = -1.0;

/// Dock end velocity per desktop per second of the swipe: the velocity is
/// the per-frame (8 ms) change of the progress × 100, as tested in task 154.
const VELOCITY_PER_SPEED: f64 = 0.008 * 100.0 * ONE_DESKTOP;

/// IOHIDEvent phase bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Began = 1,
    Changed = 2,
    Ended = 4,
    Cancelled = 8,
}

/// The values the Dock gets for a swipe at `desktops` (positive = towards
/// the desktop on the right, 1.0 = one desktop) moving at `speed` desktops
/// per second: `(progress, velocity)`.
fn dock_values(desktops: f64, speed: f64) -> (f64, f64) {
    (
        desktops * ONE_DESKTOP * RIGHT_SIGN,
        speed * VELOCITY_PER_SPEED * RIGHT_SIGN,
    )
}

/// True if the swipe events can be sent: the private functions exist and
/// Accessibility (needed to post events) is granted.
pub fn available() -> bool {
    Api::get().is_some() && super::accessibility()
}

/// Sends one swipe event to the Dock (see [`dock_values`]); `speed` is used
/// by Ended and Cancelled only. Does nothing if the private functions are
/// missing.
pub fn post(phase: Phase, desktops: f64, speed: f64) {
    let Some(api) = Api::get() else {
        return;
    };
    let (progress, velocity) = dock_values(desktops, speed);
    api.post(phase, progress, velocity);
}

// IOHIDEvent constants (IOHIDFamily IOHIDEventTypes.h).
const HID_TYPE_VELOCITY: u32 = 9;
const HID_TYPE_DOCK_SWIPE: u32 = 23;
/// `IOHIDEventFieldBase(type)`.
const fn field(kind: u32, index: u32) -> u32 {
    (kind << 16) | index
}
const FIELD_DOCK_SWIPE_MOTION: u32 = field(HID_TYPE_DOCK_SWIPE, 1);
const FIELD_DOCK_SWIPE_PROGRESS: u32 = field(HID_TYPE_DOCK_SWIPE, 2);
const FIELD_DOCK_SWIPE_FLAVOR: u32 = field(HID_TYPE_DOCK_SWIPE, 5);
const FIELD_VELOCITY_X: u32 = field(HID_TYPE_VELOCITY, 0);
const FIELD_VELOCITY_Y: u32 = field(HID_TYPE_VELOCITY, 1);
const FIELD_VELOCITY_Z: u32 = field(HID_TYPE_VELOCITY, 2);
/// `kIOHIDGestureMotionHorizontalX`.
const MOTION_HORIZONTAL: isize = 1;
/// `kIOHIDGestureFlavorDockPrimary`.
const FLAVOR_DOCK_PRIMARY: isize = 3;
/// `kIOHIDEventEventOptionPhaseShift`.
const PHASE_SHIFT: u32 = 24;
/// `CGEventType` of dock control events (`NSEventTypeDockControl`).
const CG_EVENT_DOCK_CONTROL: u32 = 30;
/// `kCGSessionEventTap`.
const CG_SESSION_EVENT_TAP: u32 = 1;

type Ref = *mut c_void;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventCreate(source: Ref) -> Ref;
    fn CGEventSetType(event: Ref, kind: u32);
    #[cfg(test)]
    fn CGEventGetType(event: Ref) -> u32;
    fn CGEventPost(tap: u32, event: Ref);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: Ref);
}

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> Ref;
    fn dlsym(handle: Ref, name: *const c_char) -> Ref;
}

const RTLD_LAZY: c_int = 1;

/// Looks up a function in a system library; `None` if it is not there.
fn lookup(lib: &CStr, name: &CStr) -> Option<Ref> {
    // SAFETY: both strings are NUL-terminated; system libraries stay loaded.
    let p = unsafe {
        let h = dlopen(lib.as_ptr(), RTLD_LAZY);
        if h.is_null() {
            return None;
        }
        dlsym(h, name.as_ptr())
    };
    (!p.is_null()).then_some(p)
}

const IOKIT: &CStr = c"/System/Library/Frameworks/IOKit.framework/IOKit";
const SKYLIGHT: &CStr = c"/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight";

type CreateFn = unsafe extern "C" fn(alloc: Ref, kind: u32, timestamp: u64, options: u32) -> Ref;
type SetIntegerFn = unsafe extern "C" fn(event: Ref, field: u32, value: isize);
type SetFloatFn = unsafe extern "C" fn(event: Ref, field: u32, value: f64);
type AppendFn = unsafe extern "C" fn(parent: Ref, child: Ref, options: u32);
type AttachFn = unsafe extern "C" fn(cg_event: Ref, hid_event: Ref);

/// Private functions used for the gesture events.
struct Api {
    create: CreateFn,
    set_integer: SetIntegerFn,
    set_float: SetFloatFn,
    append: AppendFn,
    attach: AttachFn,
}

static API: OnceLock<Option<Api>> = OnceLock::new();

impl Api {
    /// The functions; `None` (logged once) if one is missing.
    fn get() -> Option<&'static Api> {
        API.get_or_init(|| {
            Api::load()
                .inspect_err(|name| {
                    log::warn!("{}", t!("action.dock_swipe_unavailable", name = name));
                })
                .ok()
        })
        .as_ref()
    }

    fn load() -> Result<Api, String> {
        let find = |lib: &CStr, name: &CStr| {
            lookup(lib, name).ok_or_else(|| name.to_string_lossy().into_owned())
        };
        // SAFETY: the signatures match the IOKit / SkyLight functions
        // (IOHIDEventType, IOHIDEventField and IOOptionBits are 32-bit,
        // CFIndex is isize, IOHIDFloat is a double on 64-bit).
        unsafe {
            Ok(Api {
                create: std::mem::transmute::<Ref, CreateFn>(find(IOKIT, c"IOHIDEventCreate")?),
                set_integer: std::mem::transmute::<Ref, SetIntegerFn>(find(
                    IOKIT,
                    c"IOHIDEventSetIntegerValue",
                )?),
                set_float: std::mem::transmute::<Ref, SetFloatFn>(find(
                    IOKIT,
                    c"IOHIDEventSetFloatValue",
                )?),
                append: std::mem::transmute::<Ref, AppendFn>(find(
                    IOKIT,
                    c"IOHIDEventAppendEvent",
                )?),
                attach: std::mem::transmute::<Ref, AttachFn>(find(
                    SKYLIGHT,
                    c"SLEventSetIOHIDEvent",
                )?),
            })
        }
    }

    /// The DockSwipe IOHIDEvent (owned by the caller), with a velocity
    /// child for the end.
    fn hid_event(&self, phase: Phase, progress: f64, velocity: f64) -> Ref {
        let options = (phase as u32) << PHASE_SHIFT;
        // SAFETY: the event is valid until released; the child is retained
        // by the parent when appended, so it is released here.
        unsafe {
            let e = (self.create)(std::ptr::null_mut(), HID_TYPE_DOCK_SWIPE, 0, options);
            if e.is_null() {
                return e;
            }
            (self.set_integer)(e, FIELD_DOCK_SWIPE_MOTION, MOTION_HORIZONTAL);
            (self.set_integer)(e, FIELD_DOCK_SWIPE_FLAVOR, FLAVOR_DOCK_PRIMARY);
            (self.set_float)(e, FIELD_DOCK_SWIPE_PROGRESS, progress);
            if matches!(phase, Phase::Ended | Phase::Cancelled) {
                let v = (self.create)(std::ptr::null_mut(), HID_TYPE_VELOCITY, 0, 0);
                if !v.is_null() {
                    (self.set_float)(v, FIELD_VELOCITY_X, velocity);
                    (self.set_float)(v, FIELD_VELOCITY_Y, velocity);
                    (self.set_float)(v, FIELD_VELOCITY_Z, 0.0);
                    (self.append)(e, v, 0);
                    CFRelease(v);
                }
            }
            e
        }
    }

    /// The dock control `CGEvent` carrying the event (owned by the caller).
    fn cg_event(&self, phase: Phase, progress: f64, velocity: f64) -> Option<Ref> {
        let hid = self.hid_event(phase, progress, velocity);
        if hid.is_null() {
            return None;
        }
        // SAFETY: `hid` is valid; the CGEvent keeps its own reference to it.
        unsafe {
            let e = CGEventCreate(std::ptr::null_mut());
            if !e.is_null() {
                CGEventSetType(e, CG_EVENT_DOCK_CONTROL);
                (self.attach)(e, hid);
            }
            CFRelease(hid);
            (!e.is_null()).then_some(e)
        }
    }

    fn post(&self, phase: Phase, progress: f64, velocity: f64) {
        match self.cg_event(phase, progress, velocity) {
            // SAFETY: `e` is a valid CGEvent, released after posting.
            Some(e) => unsafe {
                CGEventPost(CG_SESSION_EVENT_TAP, e);
                CFRelease(e);
            },
            None => {
                log::warn!("dock swipe: cannot create the event {phase:?} progress={progress:.3}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn right_is_negative_progress_and_velocity() {
        let (p, v) = dock_values(1.0, 1.0);
        assert!(close(p, -ONE_DESKTOP));
        // 1.333 per desktop in 1 s = 0.0107 per 8 ms frame, × 100.
        assert!(close(v, -ONE_DESKTOP * 0.8));
        let (p, v) = dock_values(-0.5, -2.0);
        assert!(close(p, 0.5 * ONE_DESKTOP) && v > 0.0);
    }

    /// Builds the events (without posting) and reads them back through the
    /// private getters: checks the field numbers, phase and ABI.
    #[test]
    fn events_carry_the_gesture_data() {
        type GetFloat = unsafe extern "C" fn(Ref, u32) -> f64;
        type GetInt = unsafe extern "C" fn(Ref, u32) -> isize;
        type GetPhase = unsafe extern "C" fn(Ref) -> u16;
        type GetType = unsafe extern "C" fn(Ref) -> u32;
        type GetChildren = unsafe extern "C" fn(Ref) -> Ref;
        type CopyHid = unsafe extern "C" fn(Ref) -> Ref;
        let api = Api::get().expect("private API");
        let sym = |lib, name| lookup(lib, name).expect("symbol");
        const CG: &CStr = c"/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics";
        // SAFETY: signatures match the IOKit / CoreGraphics getters; every
        // created object is released.
        unsafe {
            let get_float: GetFloat =
                std::mem::transmute::<Ref, GetFloat>(sym(IOKIT, c"IOHIDEventGetFloatValue"));
            let get_int: GetInt =
                std::mem::transmute::<Ref, GetInt>(sym(IOKIT, c"IOHIDEventGetIntegerValue"));
            let get_phase: GetPhase =
                std::mem::transmute::<Ref, GetPhase>(sym(IOKIT, c"IOHIDEventGetPhase"));
            let get_type: GetType =
                std::mem::transmute::<Ref, GetType>(sym(IOKIT, c"IOHIDEventGetType"));
            let children: GetChildren =
                std::mem::transmute::<Ref, GetChildren>(sym(IOKIT, c"IOHIDEventGetChildren"));
            let copy_hid: CopyHid =
                std::mem::transmute::<Ref, CopyHid>(sym(CG, c"CGEventCopyIOHIDEvent"));

            let e = api.hid_event(Phase::Changed, -0.75, 0.0);
            assert_eq!(get_type(e), HID_TYPE_DOCK_SWIPE);
            assert_eq!(get_phase(e), Phase::Changed as u16);
            assert_eq!(get_int(e, FIELD_DOCK_SWIPE_MOTION), MOTION_HORIZONTAL);
            assert_eq!(get_int(e, FIELD_DOCK_SWIPE_FLAVOR), FLAVOR_DOCK_PRIMARY);
            assert!(close(get_float(e, FIELD_DOCK_SWIPE_PROGRESS), -0.75));
            assert!(children(e).is_null());
            CFRelease(e);

            let e = api.hid_event(Phase::Ended, -0.75, 3.5);
            assert_eq!(get_phase(e), Phase::Ended as u16);
            let kids = children(e);
            assert!(!kids.is_null());
            assert_eq!(core_foundation::array::CFArrayGetCount(kids.cast()), 1);
            let v = core_foundation::array::CFArrayGetValueAtIndex(kids.cast(), 0) as Ref;
            assert_eq!(get_type(v), HID_TYPE_VELOCITY);
            assert!(close(get_float(v, FIELD_VELOCITY_X), 3.5));
            CFRelease(e);

            let cg = api.cg_event(Phase::Ended, -0.75, 3.5).expect("CGEvent");
            assert_eq!(CGEventGetType(cg), CG_EVENT_DOCK_CONTROL);
            let hid = copy_hid(cg);
            assert!(!hid.is_null());
            assert_eq!(get_type(hid), HID_TYPE_DOCK_SWIPE);
            assert!(close(get_float(hid, FIELD_DOCK_SWIPE_PROGRESS), -0.75));
            CFRelease(hid);
            CFRelease(cg);
        }
    }
}
