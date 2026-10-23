//! The scroll wheel event tap and the thread that keeps it running.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use rust_i18n::t;

use super::transform::{Axis, transform};
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
    tap.enable();
    crate::runloop::run_until_stopped(&run_loop);
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

}
