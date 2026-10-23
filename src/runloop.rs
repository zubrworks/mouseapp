//! Running a CoreFoundation run loop on a background thread, or the
//! daemon's event loop on the main thread, until the program stops (see
//! [`crate::shutdown`]); stopping the app's `NSApplication`.

use std::ffi::c_void;
use std::time::Duration;

use core_foundation::base::{TCFType, kCFAllocatorDefault};
use core_foundation::runloop::{
    CFRunLoop, CFRunLoopGetCurrent, CFRunLoopSource, CFRunLoopSourceContext, CFRunLoopSourceCreate,
    CFRunLoopSourceSignal, CFRunLoopStop, CFRunLoopWakeUp, kCFRunLoopCommonModes,
    kCFRunLoopDefaultMode,
};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSEvent, NSEventModifierFlags, NSEventType};
use objc2_foundation::NSPoint;

/// Runs `run_loop` (the current thread's) until a stop request. The loop
/// sleeps until one of its sources fires; a stop request ends it through a
/// [`RunLoopStopper`].
pub fn run_until_stopped(run_loop: &CFRunLoop) {
    match RunLoopStopper::add_to(run_loop) {
        Some(stopper) => {
            crate::shutdown::on_stop(move || stopper.signal());
            while !crate::shutdown::stopped() {
                CFRunLoop::run_current();
            }
        }
        None => {
            log::error!("cannot create the run loop stop source; checking for stop every second");
            while !crate::shutdown::stopped() {
                // SAFETY: CoreFoundation's global constant, valid for the whole process.
                CFRunLoop::run_in_mode(
                    unsafe { kCFRunLoopDefaultMode },
                    Duration::from_secs(1),
                    false,
                );
            }
        }
    }
}

/// Opaque Carbon `EventRef`, `EventTargetRef` and `EventLoopRef`.
type CarbonRef = *mut c_void;

/// `kEventDurationForever`.
const EVENT_DURATION_FOREVER: f64 = -1.0;
/// `noErr`.
const NO_ERR: i32 = 0;
/// `kEventClassAppleEvent`: an Apple event waiting to be processed.
const EVENT_CLASS_APPLE_EVENT: u32 = u32::from_be_bytes(*b"eppc");
/// `kCoreEventClass` / `kAEQuitApplication`: the quit request (logout,
/// shutdown, `osascript -e 'quit app ...'`).
const CORE_EVENT_CLASS: u32 = u32::from_be_bytes(*b"aevt");
const AE_QUIT_APPLICATION: u32 = u32::from_be_bytes(*b"quit");

/// `AEEventHandlerProcPtr`.
type AppleEventHandler =
    extern "C" fn(event: *const c_void, reply: *mut c_void, refcon: *mut c_void) -> i16;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn ReceiveNextEvent(
        num_types: u32,
        list: *const c_void,
        timeout: f64,
        pull: u8,
        out_event: *mut CarbonRef,
    ) -> i32;
    fn SendEventToEventTarget(event: CarbonRef, target: CarbonRef) -> i32;
    fn GetEventDispatcherTarget() -> CarbonRef;
    fn ReleaseEvent(event: CarbonRef);
    fn GetMainEventLoop() -> CarbonRef;
    fn QuitEventLoop(event_loop: CarbonRef) -> i32;
    fn GetEventClass(event: CarbonRef) -> u32;
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn AEProcessEvent(event: CarbonRef) -> i32;
    fn AEInstallEventHandler(
        event_class: u32,
        event_id: u32,
        handler: AppleEventHandler,
        refcon: *mut c_void,
        is_sys_handler: u8,
    ) -> i16;
}

/// Runs the main thread's event loop (HIToolbox, the layer under
/// `NSApplication`) until a stop request: takes every event WindowServer
/// sends to this process off its queue and hands it to the standard
/// handlers (Apple events to the Apple Event Manager, where a quit request
/// stops the program), and runs the main run loop meanwhile (`NSWorkspace`
/// notifications, the main dispatch queue). Unlike `NSApplication::run`
/// it needs no `NSApplication`, whose setup (screen modes, menu bar,
/// appearance) would cost the daemon several MB of memory.
pub fn run_events_until_stopped(_mtm: MainThreadMarker) {
    let run_loop = CFRunLoop::get_current();
    let stopper = RunLoopStopper::with_perform(&run_loop, quit_main_event_loop);
    let timeout = match stopper {
        Some(stopper) => {
            crate::shutdown::on_stop(move || stopper.signal());
            EVENT_DURATION_FOREVER
        }
        None => {
            log::error!("cannot create the run loop stop source; checking for stop every second");
            1.0
        }
    };
    // SAFETY: a valid handler for the whole process; no refcon.
    let installed = unsafe {
        AEInstallEventHandler(
            CORE_EVENT_CLASS,
            AE_QUIT_APPLICATION,
            on_quit_event,
            std::ptr::null_mut(),
            0,
        )
    };
    if installed != 0 {
        log::warn!("cannot handle the quit Apple event: {installed}");
    }
    // SAFETY: plain getter; the target lives as long as the process.
    let dispatcher = unsafe { GetEventDispatcherTarget() };
    while !crate::shutdown::stopped() {
        let mut event: CarbonRef = std::ptr::null_mut();
        // SAFETY: no type list (every event), a valid output pointer; on the
        // main thread (`MainThreadMarker`).
        let got = unsafe { ReceiveNextEvent(0, std::ptr::null(), timeout, 1, &mut event) };
        if got == NO_ERR && !event.is_null() {
            // SAFETY: `event` was pulled off the queue for us (pull = true)
            // and is released once dispatched. An Apple event without a
            // handler gets an error reply, so its sender does not wait.
            unsafe {
                if GetEventClass(event) == EVENT_CLASS_APPLE_EVENT {
                    AEProcessEvent(event);
                } else {
                    SendEventToEventTarget(event, dispatcher);
                }
                ReleaseEvent(event);
            }
        }
    }
}

/// Handler of the quit Apple event: stops the program as SIGTERM does.
extern "C" fn on_quit_event(_event: *const c_void, _reply: *mut c_void, _refcon: *mut c_void) -> i16 {
    log::info!("quit requested by an Apple event");
    crate::shutdown::request_stop();
    0
}

/// Perform callback of the daemon's [`RunLoopStopper`]: runs on the main
/// thread and makes `ReceiveNextEvent` return.
extern "C" fn quit_main_event_loop(_info: *const c_void) {
    // SAFETY: the main event loop exists for the whole process.
    unsafe { QuitEventLoop(GetMainEventLoop()) };
}

/// Makes `NSApplication::run` return. `stop:` takes effect only after the
/// next event, so post an empty one.
pub fn stop_app(app: &NSApplication) {
    app.stop(None);
    let event = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
        NSEventType::ApplicationDefined,
        NSPoint::new(0.0, 0.0),
        NSEventModifierFlags::empty(),
        0.0,
        0,
        None,
        0,
        0,
        0,
    );
    if let Some(event) = event {
        app.postEvent_atStart(&event, true);
    }
}

/// A run loop source that stops the run loop it was added to (or does what
/// its `perform` callback says). It can be signalled from any thread, and a
/// signal sent before the loop runs is not lost (unlike a bare `CFRunLoopStop`).
struct RunLoopStopper {
    source: CFRunLoopSource,
    run_loop: CFRunLoop,
}

// SAFETY: `CFRunLoopSourceSignal`, `CFRunLoopWakeUp` and CoreFoundation
// reference counting are thread-safe.
unsafe impl Send for RunLoopStopper {}

/// Perform callback of [`RunLoopStopper`]: runs on the loop's own thread.
extern "C" fn stop_current_run_loop(_info: *const c_void) {
    // SAFETY: the current thread's run loop is valid while it runs this callback.
    unsafe { CFRunLoopStop(CFRunLoopGetCurrent()) };
}

impl RunLoopStopper {
    fn add_to(run_loop: &CFRunLoop) -> Option<Self> {
        Self::with_perform(run_loop, stop_current_run_loop)
    }

    /// A source that runs `perform` on the loop's thread when signalled.
    fn with_perform(run_loop: &CFRunLoop, perform: extern "C" fn(*const c_void)) -> Option<Self> {
        let mut context = CFRunLoopSourceContext {
            version: 0,
            info: std::ptr::null_mut(),
            retain: None,
            release: None,
            copyDescription: None,
            equal: None,
            hash: None,
            schedule: None,
            cancel: None,
            perform,
        };
        // SAFETY: a valid version 0 context (copied by the call) without info.
        let raw = unsafe { CFRunLoopSourceCreate(kCFAllocatorDefault, 0, &mut context) };
        if raw.is_null() {
            return None;
        }
        // SAFETY: `raw` is a new, owned source (create rule).
        let source = unsafe { CFRunLoopSource::wrap_under_create_rule(raw) };
        // SAFETY: CoreFoundation's global constant, valid for the whole process.
        run_loop.add_source(&source, unsafe { kCFRunLoopCommonModes });
        Some(RunLoopStopper {
            source,
            run_loop: run_loop.clone(),
        })
    }

    /// Makes the run loop stop as soon as it runs.
    fn signal(&self) {
        // SAFETY: both objects are alive (owned by `self`); the calls are thread-safe.
        unsafe {
            CFRunLoopSourceSignal(self.source.as_concrete_TypeRef());
            CFRunLoopWakeUp(self.run_loop.as_concrete_TypeRef());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopper_ends_the_run_loop_signalled_before_or_while_running() {
        use std::sync::mpsc;

        let (to_test, from_loop) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || {
            let run_loop = CFRunLoop::get_current();
            let stopper = RunLoopStopper::add_to(&run_loop).expect("stop source");
            stopper.signal(); // before the loop runs
            CFRunLoop::run_current();
            to_test.send(stopper).unwrap();
            CFRunLoop::run_current(); // until signalled by the test thread
            done.send(()).unwrap();
        });
        let stopper = from_loop
            .recv_timeout(Duration::from_secs(5))
            .expect("early signal kept");
        std::thread::sleep(Duration::from_millis(50));
        stopper.signal();
        finished
            .recv_timeout(Duration::from_secs(5))
            .expect("signal from another thread");
    }
}
