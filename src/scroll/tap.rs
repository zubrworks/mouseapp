//! The scroll wheel event tap and the thread that keeps it running. In
//! trackpad mode the tap also replaces wheel events by a glide, animated by
//! a run loop timer on the same thread (the timer sleeps when idle).

use std::cell::RefCell;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use core_foundation::base::TCFType;
use core_foundation::date::CFAbsoluteTimeGetCurrent;
use core_foundation::runloop::{
    CFRunLoopTimer, CFRunLoopTimerInvalidate, CFRunLoopTimerRef, CFRunLoopTimerSetNextFireDate,
};
use rust_i18n::t;

use super::synth::post_vertical;
use super::trackpad::{FRAME, Glide, SpinGuard};
use super::transform::{Axis, trackpad_input, transform};
use super::{lock, set_problem, update, wait_until_needed, wake_waiting};
use crate::config::Scroll;

/// How long to wait before trying to create the tap again (the permission
/// may be granted while the app runs).
const RETRY: Duration = Duration::from_secs(3);

/// Starts the tap on its own thread (daemon). The tap is only created while
/// some setting needs it (the thread sleeps until [`update`] says so) and is
/// retried until it works.
pub fn spawn(c: &Scroll) {
    update(c);
    let res = std::thread::Builder::new()
        .name("scroll".into())
        .spawn(move || {
            crate::shutdown::on_stop(wake_waiting);
            let mut last_error = String::new();
            while wait_until_needed() {
                match create_tap() {
                    Ok(tap) => {
                        set_problem(None);
                        println!("{}", t!("scroll.tap_active"));
                        run_loop(&tap);
                        return;
                    }
                    Err(e) => {
                        let msg = format!("{e:#}");
                        if msg != last_error {
                            log::error!("{}", t!("scroll.tap_failed", error = msg.clone()));
                            last_error = msg.clone();
                        }
                        set_problem(Some(msg));
                        std::thread::sleep(RETRY);
                    }
                }
            }
        });
    if let Err(e) = res {
        log::error!("cannot start the scroll thread: {e}");
    }
}

/// `mouse scroll`: runs the tap in the foreground until Ctrl-C.
pub fn run() -> Result<()> {
    let (_, cfg) = crate::profiles::Store::open()?.load_current()?;
    update(&cfg.scroll);
    if lock().is_none() {
        println!("{}", t!("scroll.nothing"));
        return Ok(());
    }
    let tap = create_tap()?;
    println!("{}", t!("scroll.running"));
    run_loop(&tap);
    Ok(())
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// Enables or disables an event tap (`tap` is its mach port).
    fn CGEventTapEnable(tap: *mut std::ffi::c_void, enable: bool);
}

/// True if the system reports that it switched the tap off (the callback
/// stalled, or user input was being disrupted).
fn is_tap_disabled(etype: core_graphics::event::CGEventType) -> bool {
    use core_graphics::event::CGEventType;
    matches!(
        etype,
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
    )
}

/// Creates the scroll wheel event tap (needs Accessibility permission).
/// The tap turns itself back on as soon as the system disables it.
fn create_tap() -> Result<core_graphics::event::CGEventTap<'static>> {
    use core_foundation::base::TCFType;
    use core_graphics::event::{
        CGEventTap, CGEventTapLocation as Loc, CGEventTapOptions, CGEventTapPlacement, CGEventType,
        CallbackResult,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicPtr, Ordering};

    let state = Mutex::new([Axis::default(), Axis::default()]);
    let spin = Mutex::new(SpinGuard::default());
    // The callback needs the tap's mach port, which exists only after the
    // tap is created. The port lives as long as the tap, and so does the
    // callback (the tap invalidates the port when dropped).
    let port = Arc::new(AtomicPtr::<std::ffi::c_void>::new(std::ptr::null_mut()));
    let cb_port = Arc::clone(&port);
    let tap = CGEventTap::new(
        Loc::Session,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        vec![CGEventType::ScrollWheel],
        move |_proxy, etype, ev| {
            if is_tap_disabled(etype) {
                log::warn!("scroll tap disabled by the system ({etype:?}); enabling it again");
                let p = cb_port.load(Ordering::Acquire);
                if !p.is_null() {
                    // SAFETY: `p` is the mach port of the tap that owns this
                    // callback, so it is still valid while the callback runs.
                    unsafe { CGEventTapEnable(p, true) };
                }
                return CallbackResult::Keep;
            }
            if !matches!(etype, CGEventType::ScrollWheel) {
                return CallbackResult::Keep;
            }
            let Some(s) = *lock() else {
                return CallbackResult::Keep;
            };
            if let Some(input) = trackpad_input(ev, &s) {
                let drop_spin = crate::util::lock(&spin).event(input.wheel_modifier, Instant::now());
                if drop_spin {
                    log::debug!("scroll: Cmd/Ctrl/Opt during a spin, wheel event dropped");
                    glide_stop();
                    return CallbackResult::Drop;
                }
                if input.plain() && glide_input(input.pixels) {
                    return CallbackResult::Drop;
                }
            }
            let mut axes = crate::util::lock(&state);
            transform(ev, &mut axes, &s, Instant::now());
            CallbackResult::Keep
        },
    )
    .map_err(|()| {
        if crate::permissions::accessibility_granted() {
            anyhow!("{}", t!("scroll.cannot_create_tap"))
        } else {
            anyhow!("{}", t!("scroll.no_accessibility"))
        }
    })?;
    port.store(
        tap.mach_port().as_concrete_TypeRef().cast(),
        Ordering::Release,
    );
    Ok(tap)
}

/// Runs the tap on the current thread until the program stops. The run
/// loop sleeps until an event arrives. A tap the system disables is turned
/// back on by its own callback.
fn run_loop(tap: &core_graphics::event::CGEventTap<'static>) {
    use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};

    let Ok(source) = tap.mach_port().create_runloop_source(0) else {
        log::error!("cannot create the run loop source");
        return;
    };
    let run_loop = CFRunLoop::get_current();
    // SAFETY: CoreFoundation's global constant, valid for the whole process.
    run_loop.add_source(&source, unsafe { kCFRunLoopCommonModes });
    let timer = glide_timer();
    // SAFETY: as above.
    run_loop.add_timer(&timer, unsafe { kCFRunLoopCommonModes });
    GLIDER.with_borrow_mut(|g| {
        *g = Some(Glider {
            glide: Glide::default(),
            timer,
        })
    });
    tap.enable();
    crate::runloop::run_until_stopped(&run_loop);
    if let Some(g) = GLIDER.with_borrow_mut(Option::take) {
        // SAFETY: the timer is valid while `g` holds it.
        unsafe { CFRunLoopTimerInvalidate(g.timer.as_concrete_TypeRef()) };
    }
}

/// The glide of trackpad mode and the timer that animates it. Lives on the
/// tap's thread: the tap callback and the timer both run there.
struct Glider {
    glide: Glide,
    timer: CFRunLoopTimer,
}

thread_local! {
    static GLIDER: RefCell<Option<Glider>> = const { RefCell::new(None) };
}

/// Far enough in the future to mean "never" (about 30 years).
const NEVER: f64 = 1.0e9;

/// A repeating frame timer that waits until [`schedule`] starts it.
fn glide_timer() -> CFRunLoopTimer {
    // SAFETY: plain CoreFoundation clock read.
    let never = unsafe { CFAbsoluteTimeGetCurrent() } + NEVER;
    CFRunLoopTimer::new(
        never,
        FRAME.as_secs_f64(),
        0,
        0,
        on_frame,
        std::ptr::null_mut(),
    )
}

/// Runs the timer every frame while the glide needs it; otherwise it sleeps.
fn schedule(timer: &CFRunLoopTimer, running: bool, was_running: bool) {
    if running == was_running {
        return;
    }
    let delay = if running { FRAME.as_secs_f64() } else { NEVER };
    // SAFETY: the timer is valid; setting its next fire date is allowed
    // from its own callout too.
    unsafe {
        CFRunLoopTimerSetNextFireDate(
            timer.as_concrete_TypeRef(),
            CFAbsoluteTimeGetCurrent() + delay,
        );
    }
}

/// Posts pixels of the glide (outside the borrow of [`GLIDER`]).
fn post(pixels: i32) {
    if pixels == 0 {
        return;
    }
    if let Err(e) = post_vertical(pixels) {
        log::warn!("trackpad scrolling: {e:#}");
    }
}

/// Stops the glide at once (a key went down during it).
fn glide_stop() {
    GLIDER.with_borrow_mut(|g| {
        if let Some(g) = g.as_mut() {
            let was = g.glide.active();
            g.glide.stop();
            schedule(&g.timer, false, was);
        }
    });
}

/// Feeds a wheel movement to the glide; false if there is none (the event
/// then keeps the standard handling).
fn glide_input(pixels: f64) -> bool {
    let posted = GLIDER.with_borrow_mut(|g| {
        let g = g.as_mut()?;
        let was = g.glide.active();
        let pixels = g.glide.input(pixels, Instant::now());
        schedule(&g.timer, g.glide.active(), was);
        Some(pixels)
    });
    match posted {
        Some(pixels) => {
            post(pixels);
            true
        }
        None => false,
    }
}

/// True if the glide goes on: trackpad mode is on and no Cmd, Ctrl or Opt
/// is down (apps would turn the frames into zoom or similar).
fn glide_allowed(trackpad: bool, wheel_modifier: bool) -> bool {
    trackpad && !wheel_modifier
}

/// One frame of the glide; a glide left over after trackpad mode was
/// switched off, or with Cmd, Ctrl or Opt held, is stopped at once.
extern "C" fn on_frame(_timer: CFRunLoopTimerRef, _info: *mut std::ffi::c_void) {
    let on = glide_allowed(
        lock().is_some_and(|s| s.trackpad()),
        crate::actions::keyboard::wheel_modifier_held(),
    );
    let posted = GLIDER.with_borrow_mut(|g| {
        let g = g.as_mut()?;
        let was = g.glide.active();
        let pixels = if on {
            g.glide.frame(Instant::now())
        } else {
            g.glide.stop();
            0
        };
        schedule(&g.timer, g.glide.active(), was);
        Some(pixels)
    });
    post(posted.unwrap_or(0));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_disabled_tap() {
        use core_graphics::event::CGEventType;
        assert!(is_tap_disabled(CGEventType::TapDisabledByTimeout));
        assert!(is_tap_disabled(CGEventType::TapDisabledByUserInput));
        assert!(!is_tap_disabled(CGEventType::ScrollWheel));
    }

    #[test]
    fn glide_stops_with_modifier() {
        assert!(glide_allowed(true, false));
        assert!(!glide_allowed(true, true));
        assert!(!glide_allowed(false, false));
    }
}
