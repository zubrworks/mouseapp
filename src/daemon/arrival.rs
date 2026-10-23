//! Logitech HID devices appearing and disappearing, as IOKit reports them.
//! A mouse switched on is looked for at once (and again a few times, since
//! a Bluetooth mouse may answer only a moment after it appears); a mouse
//! switched off is let go at once instead of after unanswered pings.

use std::collections::HashMap;
use std::ffi::{CStr, c_void};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use core_foundation::base::{CFAllocatorRef, CFRelease, TCFType, kCFAllocatorDefault};
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::runloop::{CFRunLoop, CFRunLoopRef, kCFRunLoopDefaultMode};
use core_foundation::string::{CFString, CFStringRef};

use super::wake::{Event, Waker};

/// USB vendor ID of Logitech.
const LOGITECH: i32 = 0x046D;
/// The first scan waits this long, so the several interfaces of one device
/// that appear together cause one scan.
const ARRIVAL_SETTLE: Duration = Duration::from_millis(100);
/// Waits between the scans that follow the first one after a device appeared.
const ARRIVAL_WAITS: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
];

/// Scans after a device appeared: shortly after, then after each of
/// [`ARRIVAL_WAITS`] in turn.
#[derive(Default)]
pub(super) struct ArrivalScans {
    next: Option<Instant>,
    /// Index into [`ARRIVAL_WAITS`] of the wait after the next scan.
    step: usize,
}

impl ArrivalScans {
    /// A device appeared at `now`: starts the series over, unless its first
    /// scan is still pending.
    pub(super) fn arrived(&mut self, now: Instant) {
        if self.step == 0 && self.next.is_some() {
            return;
        }
        self.next = Some(now + ARRIVAL_SETTLE);
        self.step = 0;
    }

    /// When the next scan of the series is due.
    pub(super) fn due(&self) -> Option<Instant> {
        self.next
    }

    /// A scan ran at `now`; moves on if it was the one due.
    pub(super) fn scanned(&mut self, now: Instant) {
        if self.next.is_none_or(|t| t > now) {
            return;
        }
        match ARRIVAL_WAITS.get(self.step) {
            Some(wait) => {
                self.next = Some(now + *wait);
                self.step += 1;
            }
            None => *self = ArrivalScans::default(),
        }
    }
}

/// True if `path` (as hidapi names devices on macOS) is the IOKit registry
/// entry `id`.
pub(super) fn is_entry(path: &CStr, id: u64) -> bool {
    path.to_bytes() == format!("DevSrvsID:{id}").as_bytes()
}

type IOHIDManagerRef = *mut c_void;
type IOHIDDeviceRef = *mut c_void;
type IOReturn = i32;
type IOHIDDeviceCallback = extern "C" fn(
    context: *mut c_void,
    result: IOReturn,
    sender: *mut c_void,
    device: IOHIDDeviceRef,
);

/// `kIOHIDManagerOptionNone`.
const OPTIONS_NONE: u32 = 0;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOHIDManagerCreate(allocator: CFAllocatorRef, options: u32) -> IOHIDManagerRef;
    fn IOHIDManagerSetDeviceMatching(manager: IOHIDManagerRef, matching: CFDictionaryRef);
    fn IOHIDManagerRegisterDeviceMatchingCallback(
        manager: IOHIDManagerRef,
        callback: Option<IOHIDDeviceCallback>,
        context: *mut c_void,
    );
    fn IOHIDManagerRegisterDeviceRemovalCallback(
        manager: IOHIDManagerRef,
        callback: Option<IOHIDDeviceCallback>,
        context: *mut c_void,
    );
    fn IOHIDManagerScheduleWithRunLoop(
        manager: IOHIDManagerRef,
        run_loop: CFRunLoopRef,
        mode: CFStringRef,
    );
    fn IOHIDManagerUnscheduleFromRunLoop(
        manager: IOHIDManagerRef,
        run_loop: CFRunLoopRef,
        mode: CFStringRef,
    );
    fn IOHIDDeviceGetService(device: IOHIDDeviceRef) -> u32;
    fn IORegistryEntryGetRegistryEntryID(entry: u32, id: *mut u64) -> i32;
}

/// What the callbacks need; used only on the watching thread.
struct Context {
    wake: Waker,
    /// Registry entry of each device seen appearing: a removed device may
    /// no longer tell it.
    entries: HashMap<usize, u64>,
}

/// The registry entry ID of `device`, which hidapi uses as its path.
fn entry_id(device: IOHIDDeviceRef) -> Option<u64> {
    // SAFETY: `device` is the valid device IOKit passed to the callback;
    // the service is not retained (get rule) and only read.
    let service = unsafe { IOHIDDeviceGetService(device) };
    if service == 0 {
        return None;
    }
    let mut id = 0;
    // SAFETY: valid service and output pointer.
    (unsafe { IORegistryEntryGetRegistryEntryID(service, &mut id) } == 0).then_some(id)
}

extern "C" fn on_matched(
    context: *mut c_void,
    _: IOReturn,
    _: *mut c_void,
    device: IOHIDDeviceRef,
) {
    // SAFETY: `context` is the `Context` that `watch` keeps alive while the
    // callbacks are registered; they all run on its thread, one at a time.
    let ctx = unsafe { &mut *context.cast::<Context>() };
    let id = entry_id(device);
    log::debug!("hid device appeared: entry {id:?}");
    if let Some(id) = id {
        ctx.entries.insert(device as usize, id);
    }
    ctx.wake.send(Event::Arrived);
}

extern "C" fn on_removed(
    context: *mut c_void,
    _: IOReturn,
    _: *mut c_void,
    device: IOHIDDeviceRef,
) {
    // SAFETY: as in `on_matched`.
    let ctx = unsafe { &mut *context.cast::<Context>() };
    let id = ctx
        .entries
        .remove(&(device as usize))
        .or_else(|| entry_id(device));
    log::debug!("hid device removed: entry {id:?}");
    if let Some(id) = id {
        ctx.wake.send(Event::Removed(id));
    }
}

/// Starts the thread that sends [`Event::Arrived`] and [`Event::Removed`]
/// until a stop request.
pub(super) fn spawn(wake: Waker) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("hid arrivals".into())
        .spawn(move || {
            if let Err(e) = watch(wake) {
                log::warn!("cannot watch for mice coming and going: {e:#}");
            }
        })
}

/// Owned `IOHIDManagerRef`.
struct Manager(IOHIDManagerRef);

impl Drop for Manager {
    fn drop(&mut self) {
        // SAFETY: an owned reference from `IOHIDManagerCreate` (create rule).
        unsafe { CFRelease(self.0.cast_const()) };
    }
}

fn watch(wake: Waker) -> Result<()> {
    // SAFETY: plain constructor; null on failure.
    let raw = unsafe { IOHIDManagerCreate(kCFAllocatorDefault, OPTIONS_NONE) };
    if raw.is_null() {
        bail!("IOHIDManagerCreate failed");
    }
    let manager = Manager(raw);
    let matching = CFDictionary::from_CFType_pairs(&[(
        CFString::from_static_string("VendorID").as_CFType(),
        CFNumber::from(LOGITECH).as_CFType(),
    )]);
    let context = Box::into_raw(Box::new(Context {
        wake,
        entries: HashMap::new(),
    }));
    let run_loop = CFRunLoop::get_current();
    // The manager is never opened: matching and removal callbacks come
    // without it, and opening would make IOKit open every Logitech
    // interface in this process (element trees, report buffers) for nothing.
    // SAFETY: valid manager, dictionary (copied) and run loop; `context`
    // lives until the callbacks are unregistered below.
    unsafe {
        IOHIDManagerSetDeviceMatching(manager.0, matching.as_concrete_TypeRef());
        IOHIDManagerRegisterDeviceMatchingCallback(manager.0, Some(on_matched), context.cast());
        IOHIDManagerRegisterDeviceRemovalCallback(manager.0, Some(on_removed), context.cast());
        IOHIDManagerScheduleWithRunLoop(
            manager.0,
            run_loop.as_concrete_TypeRef(),
            kCFRunLoopDefaultMode,
        );
    }
    crate::runloop::run_until_stopped(&run_loop);
    // SAFETY: same objects as above, on the same thread; after this no
    // callback can run, so `context` can be freed.
    unsafe {
        IOHIDManagerRegisterDeviceMatchingCallback(manager.0, None, std::ptr::null_mut());
        IOHIDManagerRegisterDeviceRemovalCallback(manager.0, None, std::ptr::null_mut());
        IOHIDManagerUnscheduleFromRunLoop(
            manager.0,
            run_loop.as_concrete_TypeRef(),
            kCFRunLoopDefaultMode,
        );
        drop(Box::from_raw(context));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn scans_follow_an_arrival_at_growing_gaps() {
        let t0 = Instant::now();
        let mut s = ArrivalScans::default();
        assert_eq!(s.due(), None);
        s.arrived(t0);
        let mut at = s.due().unwrap();
        assert_eq!(at, t0 + ARRIVAL_SETTLE);
        let mut gaps = Vec::new();
        while let Some(next) = {
            s.scanned(at);
            s.due()
        } {
            gaps.push((next - at).as_secs());
            at = next;
        }
        assert_eq!(gaps, [1, 2, 4, 8]);
    }

    #[test]
    fn interfaces_appearing_together_share_the_first_scan() {
        let t0 = Instant::now();
        let mut s = ArrivalScans::default();
        s.arrived(t0);
        s.arrived(t0 + Duration::from_millis(30));
        assert_eq!(s.due(), Some(t0 + ARRIVAL_SETTLE));
    }

    #[test]
    fn a_new_arrival_starts_the_series_over() {
        let t0 = Instant::now();
        let mut s = ArrivalScans::default();
        s.arrived(t0);
        s.scanned(t0 + ARRIVAL_SETTLE);
        s.scanned(t0 + Duration::from_secs(2));
        let t1 = t0 + Duration::from_secs(3);
        s.arrived(t1);
        assert_eq!(s.due(), Some(t1 + ARRIVAL_SETTLE));
        s.scanned(t1 + ARRIVAL_SETTLE);
        assert_eq!(s.due(), Some(t1 + ARRIVAL_SETTLE + ARRIVAL_WAITS[0]));
    }

    #[test]
    fn an_early_scan_does_not_advance_the_series() {
        let t0 = Instant::now();
        let mut s = ArrivalScans::default();
        s.arrived(t0);
        s.scanned(t0); // a regular scan before the first one was due
        assert_eq!(s.due(), Some(t0 + ARRIVAL_SETTLE));
    }

    #[test]
    fn hidapi_paths_name_registry_entries() {
        let path = CString::new("DevSrvsID:4294971234").unwrap();
        assert!(is_entry(&path, 4_294_971_234));
        assert!(!is_entry(&path, 429_497_123));
        assert!(!is_entry(&CString::new("IOService:/x").unwrap(), 1));
    }
}
