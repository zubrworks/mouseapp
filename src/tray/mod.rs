//! The settings app: menu bar icon and menu (`tray-icon` + `muda`, native
//! NSMenu) and the settings window, on the main thread. It is a client of
//! the daemon: status and requests go through the control socket
//! ([`Driver`]); Quit ends only the app, the daemon keeps running.

mod delegate;
mod icons;
mod menu;

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use core_foundation::base::{TCFType, kCFAllocatorDefault};
use core_foundation::date::CFAbsoluteTimeGetCurrent;
use core_foundation::runloop::{
    CFRunLoop, CFRunLoopActivity, CFRunLoopObserver, CFRunLoopObserverContext,
    CFRunLoopObserverCreate, CFRunLoopObserverInvalidate, CFRunLoopObserverRef, CFRunLoopTimer,
    CFRunLoopTimerInvalidate, CFRunLoopTimerRef, kCFRunLoopBeforeWaiting, kCFRunLoopDefaultMode,
};
use muda::{MenuEvent, MenuEventReceiver};
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use rust_i18n::t;
use tray_icon::{TrayIcon, TrayIconBuilder};

use crate::autostart;
use crate::driver::{Driver, State};
use crate::ipc::{self, Request};
use crate::profiles::{self, Store};
use crate::ui::{self, SettingsWindow, Tab};
use delegate::AppDelegate;
use icons::{fallback_icon, symbol_icon};
use menu::{Click, build_menu, parse_click};

/// How often (seconds) the app wakes up to look at the daemon's status.
/// Clicks and window actions are handled at once, not on this beat.
const STATUS_STEP: f64 = 1.0;
/// Shortest time between two checks of the profile files.
const PROFILE_CHECK: Duration = Duration::from_millis(900);

/// The settings app: the menu bar icon and the settings window (opened at
/// once and whenever the app is opened again) on the main thread until Quit.
/// Starts the daemon when it is not running. Returns an error if the icon
/// cannot be created (e.g. no GUI session).
pub fn run() -> Result<()> {
    let mtm = MainThreadMarker::new().ok_or_else(|| anyhow!("menu must run on the main thread"))?;
    let store = Store::open()?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    // NSApplication keeps only a weak reference: `delegate` lives until the end.
    let delegate = AppDelegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    // Launched before the icon and the window appear; `run` does not repeat it.
    app.finishLaunching();

    let driver = Driver::connect(ipc::socket_path(store.dir()), true)?;
    let icon = symbol_icon().map_or_else(fallback_icon, Ok)?;
    let (seen, state) = driver.snapshot();
    let menu = build_menu(&state, &store)?;
    #[cfg(feature = "debug-access")]
    let debug = Debug::start(store.dir(), &menu);
    let tray: TrayIcon = TrayIconBuilder::new()
        .with_icon_templated(icon)
        .with_tooltip(t!("menu.tooltip"))
        .with_menu(Box::new(menu))
        .build()?;
    let active = store.active();
    let profile_configured = store.configured_profile();
    let profile_stamp = store.stamp(&profile_configured);
    let profile_seen = (active, store.is_modified());
    let mut settings = SettingsWindow::new(store.clone(), mtm);
    settings.set_driver(&state);
    settings.show();

    let ui = RefCell::new(Ui {
        mtm,
        store,
        driver,
        tray,
        settings,
        delegate,
        receiver: MenuEvent::receiver(),
        seen,
        state,
        quit: false,
        profile_seen,
        profile_stamp,
        profile_configured,
        profile_read: Instant::now(),
        #[cfg(feature = "debug-access")]
        debug,
    });
    let wakers = Wakers::install(&ui);
    // Returns after Quit (see `Ui::stop`).
    app.run();
    drop(wakers);
    Ok(())
}

/// Everything the app keeps up to date on the main thread.
struct Ui {
    mtm: MainThreadMarker,
    store: Store,
    driver: Driver,
    tray: TrayIcon,
    settings: SettingsWindow,
    delegate: Retained<AppDelegate>,
    receiver: &'static MenuEventReceiver,
    /// Version of `state` (see [`Driver::snapshot`]).
    seen: u64,
    state: State,
    /// Quit was chosen; nothing more to do.
    quit: bool,
    /// Active profile and whether it has unsaved changes, as last shown,
    /// and when they were last read.
    profile_seen: (String, bool),
    profile_stamp: profiles::Stamp,
    /// The profile named in `config.toml` when the stamp was taken.
    profile_configured: String,
    profile_read: Instant,
    #[cfg(feature = "debug-access")]
    debug: Debug,
}

/// Test build: `app.sock` and the menu now shown, for `mouse debug`.
#[cfg(feature = "debug-access")]
struct Debug {
    /// `None` if the socket could not be set up (logged).
    server: Option<crate::debug::server::AppServer>,
    menu: muda::Menu,
}

#[cfg(feature = "debug-access")]
impl Debug {
    fn start(dir: &std::path::Path, menu: &muda::Menu) -> Debug {
        let server = crate::debug::server::AppServer::start(dir)
            .inspect_err(|e| log::warn!("cannot open the debug socket: {e:#}"))
            .ok();
        Debug { server, menu: menu.clone() }
    }
}

impl Ui {
    /// Handles what happened since the last call: reopening the app, menu
    /// clicks, window actions and a new daemon status.
    fn step(&mut self) {
        if self.quit {
            return;
        }
        if self.delegate.take_reopen() {
            self.settings.show();
        }
        // Before the menu events: a debug click is handled in this pass.
        #[cfg(feature = "debug-access")]
        self.debug_requests();
        while let Ok(ev) = self.receiver.try_recv() {
            self.click(ev.id().as_ref());
            if self.quit {
                return;
            }
        }
        let effects = self.settings.poll();
        self.driver.set_watching(self.settings.is_open());
        let profile_changed = effects.iter().any(|e| {
            matches!(e, ui::Effect::ConfigSaved | ui::Effect::MenuChanged | ui::Effect::Refresh)
        }) && self.sync_profile();
        if effects.contains(&ui::Effect::StartDriver) {
            self.driver.start();
        }
        if effects.contains(&ui::Effect::StopDriver) {
            self.driver.send(Request::Quit);
        }
        for effect in &effects {
            if let ui::Effect::Dpi(v) = effect {
                self.driver.send(Request::Dpi(*v));
            }
        }
        if effects.contains(&ui::Effect::ConfigSaved) {
            self.driver.send(Request::Reload);
        }
        if effects.contains(&ui::Effect::LanguageChanged) {
            if let Err(e) = self.tray.set_tooltip(Some(t!("menu.tooltip"))) {
                log::warn!("cannot update the tooltip: {e}");
            }
            self.rebuild();
        } else if profile_changed || effects.contains(&ui::Effect::MenuChanged) {
            self.rebuild();
        }
        self.check_profile();
        let (version, now) = self.driver.snapshot();
        if version != self.seen {
            (self.seen, self.state) = (version, now);
            self.settings.set_driver(&self.state);
            self.rebuild();
        }
    }

    /// Remembers the profile state as it is now, so that a change made by the
    /// window itself does not cause a second refresh. True if it differs from
    /// what was remembered (the menu then has to be rebuilt).
    fn sync_profile(&mut self) -> bool {
        // The stamp first: a write in between is noticed on the next check.
        let configured = self.store.configured_profile();
        self.profile_stamp = self.store.stamp(&configured);
        self.profile_configured = configured;
        let now = (self.store.active(), self.store.is_modified());
        std::mem::replace(&mut self.profile_seen, now.clone()) != now
    }

    /// Notices a change of the active profile or of its unsaved-changes mark
    /// made elsewhere (command line, daemon, another window) and updates the
    /// menu and the General tab. Reads the files at most once a second; other
    /// tabs are left alone, so a control being dragged is not rebuilt.
    fn check_profile(&mut self) {
        if self.profile_read.elapsed() < PROFILE_CHECK {
            return;
        }
        self.profile_read = Instant::now();
        // The files are read only when one of them changed.
        if self.store.stamp(&self.profile_configured) == self.profile_stamp {
            return;
        }
        if !self.sync_profile() {
            return;
        }
        self.rebuild();
        if self.settings.tab() == ui::Tab::General {
            self.settings.refresh();
        }
    }

    /// Handles a click on the menu item with this id.
    fn click(&mut self, id: &str) {
        match parse_click(id) {
            Some(Click::Dpi(v)) => {
                self.driver.send(Request::Dpi(v));
                self.rebuild();
                self.settings.refresh();
            }
            Some(Click::Profile(name)) => {
                if name == self.store.active() || ui::profiles::confirm_switch(&self.store, self.mtm) {
                    match profiles::switch_to(&self.store, &name) {
                        Ok(true) => self.driver.send(Request::Reload),
                        Ok(false) => {}
                        Err(e) => log::warn!("cannot switch profile: {e}"),
                    }
                }
                self.rebuild();
                self.settings.refresh();
            }
            Some(Click::Reload) => self.driver.send(Request::Reload),
            Some(Click::Autostart) => {
                autostart::toggle(!autostart::is_enabled());
                self.rebuild();
                self.settings.refresh();
            }
            Some(Click::RestartDock) => {
                if let Err(e) = crate::dock::restart() {
                    log::warn!("cannot restart the Dock: {e}");
                }
            }
            Some(Click::Settings) => self.settings.show(),
            Some(Click::ManageProfiles) => self.settings.show_at(Tab::General),
            Some(Click::SettingsFile) => {
                crate::macos::reveal_in_finder(&self.store.settings_file())
            }
            Some(Click::Permission(p)) => p.open_settings(self.mtm),
            Some(Click::StartDriver) => self.driver.start(),
            Some(Click::About) => {
                crate::macos::show_about_panel(self.mtm, &t!("menu.about_license"))
            }
            Some(Click::Quit) => self.stop(),
            None => {}
        }
    }

    fn rebuild(&mut self) {
        match build_menu(&self.state, &self.store) {
            Ok(menu) => {
                #[cfg(feature = "debug-access")]
                {
                    self.debug.menu = menu.clone();
                }
                self.tray.set_menu(Some(Box::new(menu)));
            }
            Err(e) => log::warn!("cannot rebuild the menu: {e}"),
        }
    }

    /// Test build: answers the requests waiting on `app.sock`.
    #[cfg(feature = "debug-access")]
    fn debug_requests(&mut self) {
        use crate::debug::proto::AppRequest;
        use crate::debug::{menu, window};
        while let Some(p) = self.debug.server.as_ref().and_then(|s| s.next()) {
            let result = match &p.request {
                AppRequest::Menu => Ok(menu::format(&menu::read(&self.debug.menu))),
                AppRequest::MenuClick(path) => menu::click(&self.debug.menu, path),
                AppRequest::Window => {
                    Ok(window::dump(self.settings.ns_window(), self.settings.tab().id()))
                }
            };
            p.answer(result);
        }
    }

    /// Makes `NSApplication::run` return.
    fn stop(&mut self) {
        self.quit = true;
        crate::runloop::stop_app(&NSApplication::sharedApplication(self.mtm));
    }
}

/// Runs [`Ui::step`] on the main run loop: an observer calls it whenever
/// the loop is about to sleep (so right after every event: a click, a key,
/// a reopen), and a timer wakes the loop every [`STATUS_STEP`] for the
/// daemon's status. Both live in the default mode only, like the old event
/// loop: not while a menu is open or a modal dialog runs. Dropping removes
/// both.
struct Wakers {
    observer: CFRunLoopObserver,
    timer: CFRunLoopTimer,
}

impl Wakers {
    /// `ui` must outlive the returned value.
    fn install(ui: &RefCell<Ui>) -> Wakers {
        let mut context = CFRunLoopObserverContext {
            version: 0,
            info: ptr::from_ref(ui).cast_mut().cast::<c_void>(),
            retain: None,
            release: None,
            copyDescription: None,
        };
        // SAFETY: valid arguments; CF copies the context. `info` stays valid
        // because the observer is invalidated (Drop) before `ui` goes away.
        let observer = unsafe {
            CFRunLoopObserver::wrap_under_create_rule(CFRunLoopObserverCreate(
                kCFAllocatorDefault,
                kCFRunLoopBeforeWaiting,
                1,
                0,
                on_before_waiting,
                &mut context,
            ))
        };
        // SAFETY: CFAbsoluteTimeGetCurrent has no preconditions.
        let first = unsafe { CFAbsoluteTimeGetCurrent() } + STATUS_STEP;
        let timer = CFRunLoopTimer::new(first, STATUS_STEP, 0, 0, on_timer, ptr::null_mut());
        let run_loop = CFRunLoop::get_current();
        // SAFETY: reading an immutable CF constant.
        let mode = unsafe { kCFRunLoopDefaultMode };
        run_loop.add_observer(&observer, mode);
        run_loop.add_timer(&timer, mode);
        Wakers { observer, timer }
    }
}

impl Drop for Wakers {
    fn drop(&mut self) {
        // SAFETY: valid objects owned by `self`; invalidating removes them
        // from the run loop, so the observer never sees a dead `ui`.
        unsafe {
            CFRunLoopObserverInvalidate(self.observer.as_concrete_TypeRef());
            CFRunLoopTimerInvalidate(self.timer.as_concrete_TypeRef());
        }
    }
}

extern "C" fn on_before_waiting(
    _observer: CFRunLoopObserverRef,
    _activity: CFRunLoopActivity,
    info: *mut c_void,
) {
    // SAFETY: `info` is the `RefCell<Ui>` given to `Wakers::install`, alive
    // while the observer is installed; only the main thread runs it.
    let ui = unsafe { &*info.cast_const().cast::<RefCell<Ui>>() };
    // A step that is already running (e.g. it started a nested run loop)
    // finishes first; whatever is new is handled on the next pass.
    if let Ok(mut ui) = ui.try_borrow_mut() {
        ui.step();
    }
}

/// Does nothing: firing wakes the run loop, which then calls the observer.
extern "C" fn on_timer(_timer: CFRunLoopTimerRef, _info: *mut c_void) {}
