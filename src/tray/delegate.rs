//! The application delegate: tells the UI loop when the app is opened again.

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::NSApplication;
use objc2_app_kit::NSApplicationDelegate;
use objc2_foundation::NSObjectProtocol;

pub(super) struct ReopenIvars {
    /// Set when the app is opened again; taken by the UI loop.
    asked: Cell<bool>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; `AppDelegate` has no Drop.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = ReopenIvars]
    #[name = "MouseAppDelegate"]
    pub(super) struct AppDelegate;

    // SAFETY: NSObjectProtocol has no safety requirements.
    unsafe impl NSObjectProtocol for AppDelegate {}

    // SAFETY: the method signature matches the protocol.
    unsafe impl NSApplicationDelegate for AppDelegate {
        /// The running app was opened again (Finder, `open`): show the window.
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn should_handle_reopen(&self, _app: &NSApplication, _visible: bool) -> bool {
            self.ivars().asked.set(true);
            // Handled here; AppKit has nothing more to do.
            false
        }
    }
);

impl AppDelegate {
    pub(super) fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ReopenIvars {
            asked: Cell::new(false),
        });
        // SAFETY: NSObject's `init` has this signature.
        unsafe { msg_send![super(this), init] }
    }

    /// True once after every reopen.
    pub(super) fn take_reopen(&self) -> bool {
        self.ivars().asked.replace(false)
    }
}
