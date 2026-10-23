//! The settings window: an `NSWindow` with tabs on a preferences-style
//! toolbar, and the Objective-C object that receives toolbar and control
//! actions. Actions are queued and handled in [`SettingsWindow::poll`], so
//! views are never rebuilt while AppKit is still inside one of them.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBackingStoreType, NSImage, NSToolbar, NSToolbarDelegate, NSToolbarItem,
    NSUserInterfaceItemIdentification, NSView, NSWindow, NSWindowStyleMask, NSWindowToolbarStyle,
};
use objc2_foundation::{NSArray, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

use super::{Context, Effect, MouseView, Tab, dispatch};
use crate::driver::State;
use crate::profiles::Store;

/// Toolbar identifier (also keeps AppKit's toolbar state apart from others).
const TOOLBAR_ID: &str = "MouseSettingsToolbar";

struct TargetIvars {
    ctx: Context,
    /// Effects of control changes not yet handled by `poll`.
    effects: RefCell<Vec<Effect>>,
    /// Tab clicked on the toolbar, not yet shown.
    selected: Cell<Option<Tab>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; `Target` has no Drop.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = TargetIvars]
    #[name = "MouseSettingsTarget"]
    struct Target;

    impl Target {
        /// Action of every control (see `ui::wire`); the control's
        /// identifier says what changed.
        #[unsafe(method(controlChanged:))]
        fn control_changed(&self, sender: &AnyObject) {
            let Some(id) = sender.downcast_ref::<NSView>().and_then(|v| v.identifier()) else {
                return;
            };
            if super::live_update(sender, self.mtm()) {
                return;
            }
            let effects = dispatch(&id.to_string(), sender, &self.ivars().ctx, self.mtm());
            self.ivars().effects.borrow_mut().extend(effects);
        }

        /// Action of the toolbar tab items.
        #[unsafe(method(selectTab:))]
        fn select_tab(&self, sender: &NSToolbarItem) {
            if let Some(tab) = Tab::from_id(&sender.itemIdentifier().to_string()) {
                self.ivars().selected.set(Some(tab));
            }
        }
    }

    // SAFETY: NSObjectProtocol has no safety requirements.
    unsafe impl NSObjectProtocol for Target {}

    // SAFETY: the method signatures match the protocol.
    unsafe impl NSToolbarDelegate for Target {
        #[unsafe(method_id(toolbar:itemForItemIdentifier:willBeInsertedIntoToolbar:))]
        fn toolbar_item(
            &self,
            _toolbar: &NSToolbar,
            id: &NSString,
            _will_insert: bool,
        ) -> Option<Retained<NSToolbarItem>> {
            self.toolbar_item_for(id)
        }

        #[unsafe(method_id(toolbarDefaultItemIdentifiers:))]
        fn default_ids(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSString>> {
            tab_ids()
        }

        #[unsafe(method_id(toolbarAllowedItemIdentifiers:))]
        fn allowed_ids(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSString>> {
            tab_ids()
        }

        #[unsafe(method_id(toolbarSelectableItemIdentifiers:))]
        fn selectable_ids(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSString>> {
            tab_ids()
        }
    }
);

impl Target {
    fn new(ctx: Context, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TargetIvars {
            ctx,
            effects: RefCell::new(Vec::new()),
            selected: Cell::new(None),
        });
        // SAFETY: NSObject's `init` has this signature.
        unsafe { msg_send![super(this), init] }
    }

    /// A new toolbar item for the tab with identifier `id`.
    fn toolbar_item_for(&self, id: &NSString) -> Option<Retained<NSToolbarItem>> {
        let tab = Tab::from_id(&id.to_string())?;
        let item = NSToolbarItem::initWithItemIdentifier(NSToolbarItem::alloc(self.mtm()), id);
        item.setLabel(&NSString::from_str(&tab.title()));
        item.setImage(tab_image(tab).as_deref());
        // SAFETY: `self` implements `selectTab:` and lives as long as
        // the window (owned by `SettingsWindow`).
        unsafe {
            item.setTarget(Some(self));
            item.setAction(Some(sel!(selectTab:)));
        }
        Some(item)
    }
}

/// SF Symbol of a tab, described by the tab's title (for VoiceOver).
fn tab_image(tab: Tab) -> Option<Retained<NSImage>> {
    crate::macos::symbol_image(tab.symbol(), &tab.title())
}

fn tab_ids() -> Retained<NSArray<NSString>> {
    let ids: Vec<Retained<NSString>> = Tab::ALL
        .iter()
        .map(|t| NSString::from_str(t.id()))
        .collect();
    NSArray::from_retained_slice(&ids)
}

/// Owner of the settings window; lives on the main thread with the menu.
pub struct SettingsWindow {
    mtm: MainThreadMarker,
    target: Retained<Target>,
    window: Option<Retained<NSWindow>>,
    tab: Tab,
    /// Content width of the window: the widest of all tabs, so that it does
    /// not change when switching tabs (the height follows the tab shown).
    /// Zero until measured.
    width: f64,
}

/// Content size of the window for a tab of size `tab`: the width fits the
/// widest tab (`widest`), the height is the tab's own.
fn fit_size(widest: f64, tab: NSSize) -> NSSize {
    NSSize::new(widest.max(tab.width), tab.height)
}

impl SettingsWindow {
    /// Prepares the window (created on first [`show`](Self::show)) for the
    /// settings in `store`.
    pub fn new(store: Store, mtm: MainThreadMarker) -> Self {
        SettingsWindow {
            mtm,
            target: Target::new(Context::new(store), mtm),
            window: None,
            tab: Tab::ALL[0],
            width: 0.0,
        }
    }

    /// Tells the window what the daemon reports (nothing when it does not
    /// run): the connected mouse, the missing permissions and whether it
    /// runs. Redraws if any changed.
    pub fn set_driver(&mut self, state: &State) {
        let ctx = &self.target.ivars().ctx;
        if !card_changed(&ctx.driver.borrow(), state) {
            return;
        }
        let status = state.status();
        ctx.driver.replace(state.clone());
        *ctx.mouse.borrow_mut() = MouseView::from_mice(status.map_or(&[], |s| &s.mice[..]));
        *ctx.missing.borrow_mut() = status.map(|s| s.missing.clone()).unwrap_or_default();
        // The mouse changes the content of the tabs: measure them all again.
        self.width = 0.0;
        self.refresh();
    }

    /// Opens the window (or brings it to the front) with fresh values.
    pub fn show(&mut self) {
        let window = match &self.window {
            Some(w) => w.clone(),
            None => {
                let w = self.create_window();
                self.window = Some(w.clone());
                w
            }
        };
        self.show_tab(self.tab);
        if !window.isVisible() {
            window.center();
        }
        crate::macos::activate_app(self.mtm);
        window.orderFrontRegardless();
        window.makeKeyAndOrderFront(None);
    }

    /// Opens the window on `tab`.
    pub fn show_at(&mut self, tab: Tab) {
        self.tab = tab;
        self.show();
    }

    /// Test build: the window, once created (see `debug window`).
    #[cfg(feature = "debug-access")]
    pub fn ns_window(&self) -> Option<&NSWindow> {
        self.window.as_deref()
    }

    /// The tab shown.
    pub fn tab(&self) -> Tab {
        self.tab
    }

    /// Is the window on screen?
    pub fn is_open(&self) -> bool {
        self.window.as_ref().is_some_and(|w| w.isVisible())
    }

    /// Re-reads the settings into the open window (e.g. after a menu change).
    pub fn refresh(&mut self) {
        if self.is_open() {
            self.show_tab(self.tab);
        }
    }

    /// Handles queued toolbar and control actions; returns what the rest of
    /// the app has to do.
    pub fn poll(&mut self) -> Vec<Effect> {
        if let Some(tab) = self.target.ivars().selected.take() {
            self.show_tab(tab);
        }
        let effects = std::mem::take(&mut *self.target.ivars().effects.borrow_mut());
        if effects.contains(&Effect::LanguageChanged) {
            self.relocalize();
        } else if effects.contains(&Effect::Refresh) {
            self.refresh();
        }
        effects
    }

    fn create_window(&self) -> Retained<NSWindow> {
        let mtm = self.mtm;
        // SAFETY: `setReleasedWhenClosed(false)` below; we keep our own reference.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(460.0, 200.0)),
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Miniaturizable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: the window is owned by `self.window`, not by AppKit.
        unsafe { window.setReleasedWhenClosed(false) };
        let toolbar =
            NSToolbar::initWithIdentifier(NSToolbar::alloc(mtm), &NSString::from_str(TOOLBAR_ID));
        toolbar.setDelegate(Some(ProtocolObject::from_ref(&*self.target)));
        toolbar.setAllowsUserCustomization(false);
        window.setToolbarStyle(NSWindowToolbarStyle::Preference);
        window.setToolbar(Some(&toolbar));
        window
    }

    /// Builds `tab` with current values and puts it into the window. The
    /// window keeps the width of the widest tab and takes the height of this
    /// one, animated, with its top edge in place; narrower content is
    /// centered. The first time (and after a language or mouse change) every
    /// tab is built to find the width.
    fn show_tab(&mut self, tab: Tab) {
        self.tab = tab;
        let Some(window) = self.window.clone() else {
            return;
        };
        let target: &AnyObject = &self.target;
        let ctx = &self.target.ivars().ctx;
        if self.width == 0.0 {
            // The tab shown is built last, so the state the tabs keep in
            // `ctx` belongs to it.
            for t in Tab::ALL.into_iter().filter(|t| *t != tab) {
                self.width = self.width.max(t.build(ctx, target, self.mtm).frame().size.width);
            }
        }
        let view = tab.build(ctx, target, self.mtm);
        let size = view.frame().size;
        let content = fit_size(self.width, size);
        self.width = content.width;
        // The new view goes in first, pinned to the top of the window as it
        // is now; the window then changes size around it.
        let current = window.contentRectForFrameRect(window.frame()).size;
        let holder = NSView::initWithFrame(
            NSView::alloc(self.mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), current),
        );
        view.setFrameOrigin(NSPoint::new(
            ((current.width - size.width) / 2.0).round(),
            current.height - size.height,
        ));
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewMinXMargin
                | NSAutoresizingMaskOptions::ViewMaxXMargin
                | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        holder.addSubview(&view);
        window.setContentView(Some(&holder));
        window.setTitle(&NSString::from_str(&tab.title()));
        if let Some(toolbar) = window.toolbar() {
            toolbar.setSelectedItemIdentifier(Some(&NSString::from_str(tab.id())));
        }
        let old = window.frame();
        let new = window.frameRectForContentRect(NSRect::new(NSPoint::new(0.0, 0.0), content));
        let top = old.origin.y + old.size.height;
        window.setFrame_display_animate(
            NSRect::new(NSPoint::new(old.origin.x, top - new.size.height), new.size),
            true,
            window.isVisible(),
        );
    }

    /// Updates every text after a language change.
    fn relocalize(&mut self) {
        // Texts of another length may need another size.
        self.width = 0.0;
        if let Some(toolbar) = self.window.as_ref().and_then(|w| w.toolbar()) {
            for item in toolbar.items() {
                if let Some(tab) = Tab::from_id(&item.itemIdentifier().to_string()) {
                    item.setLabel(&NSString::from_str(&tab.title()));
                    item.setImage(tab_image(tab).as_deref());
                }
            }
        }
        self.show_tab(self.tab);
    }
}

/// Whether the window must be rebuilt for the new daemon state: only when the
/// kind of state (running, starting, stopped), the model or DPI list of the
/// mouse shown, or the missing permissions change. Battery and current DPI
/// are left out on purpose: a rebuild would interrupt a field the user is
/// editing.
fn card_changed(old: &State, new: &State) -> bool {
    if std::mem::discriminant(old) != std::mem::discriminant(new) {
        return true;
    }
    let (Some(old), Some(new)) = (old.status(), new.status()) else {
        return false;
    };
    let (old_view, new_view) = (MouseView::from_mice(&old.mice), MouseView::from_mice(&new.mice));
    old.missing != new.missing
        || old_view.model.name != new_view.model.name
        || old_view.dpi_list != new_view.dpi_list
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hidpp::features::battery::{Battery, Charge};
    use crate::hidpp::features::dpi::DpiList;
    use crate::hub::MouseInfo;
    use crate::ipc::Status;
    use crate::permissions::Permission;

    #[test]
    fn fit_size_takes_widest_width_and_own_height() {
        let fit = fit_size(520.0, NSSize::new(460.0, 300.0));
        assert_eq!((fit.width, fit.height), (520.0, 300.0));
        // A tab wider than all measured ones widens the window.
        let fit = fit_size(520.0, NSSize::new(600.0, 200.0));
        assert_eq!((fit.width, fit.height), (600.0, 200.0));
    }

    fn mouse(model: Option<&str>, dpi: u16, percent: u8, list: &[u16]) -> MouseInfo {
        MouseInfo {
            key: "k".into(),
            name: "MX Master 3S".into(),
            model: model.map(String::from),
            battery: Some(Battery { percent: Some(percent), charge: Charge::Discharging }),
            dpi: Some(dpi),
            dpi_list: Some(DpiList { values: list.to_vec(), ranges: Vec::new() }),
            ..MouseInfo::default()
        }
    }

    fn running(mice: Vec<MouseInfo>, missing: Vec<Permission>) -> State {
        State::Running(Status { mice, missing, ..Status::default() })
    }

    const M: Option<&str> = Some("MX Master 3S");

    #[test]
    fn battery_and_current_dpi_do_not_rebuild() {
        let old = running(vec![mouse(M, 800, 80, &[400, 800])], vec![]);
        assert!(!card_changed(&old, &old.clone()));
        let battery = running(vec![mouse(M, 800, 79, &[400, 800])], vec![]);
        assert!(!card_changed(&old, &battery));
        let dpi = running(vec![mouse(M, 1600, 80, &[400, 800])], vec![]);
        assert!(!card_changed(&old, &dpi));
    }

    #[test]
    fn state_kind_rebuilds() {
        let run = running(vec![], vec![]);
        for (a, b) in [
            (&State::Stopped, &State::Starting),
            (&State::Starting, &run),
            (&run, &State::Stopped),
        ] {
            assert!(card_changed(a, b));
            assert!(card_changed(b, a));
        }
        assert!(!card_changed(&State::Stopped, &State::Stopped));
        assert!(!card_changed(&State::Starting, &State::Starting));
    }

    #[test]
    fn dpi_list_rebuilds() {
        let old = running(vec![mouse(M, 800, 80, &[400, 800])], vec![]);
        let new = running(vec![mouse(M, 800, 80, &[400, 800, 1600])], vec![]);
        assert!(card_changed(&old, &new));
    }

    #[test]
    fn permissions_rebuild() {
        let old = running(vec![], vec![Permission::Accessibility]);
        let both = running(vec![], Permission::ALL.to_vec());
        let none = running(vec![], vec![]);
        assert!(card_changed(&old, &both));
        assert!(card_changed(&old, &none));
        assert!(!card_changed(&old, &old.clone()));
    }

    #[test]
    fn mouse_appearing_with_a_list_rebuilds() {
        let none = running(vec![], vec![]);
        let one = running(vec![mouse(M, 800, 80, &[400, 800])], vec![]);
        assert!(card_changed(&none, &one));
        assert!(card_changed(&one, &none));
        // The same default model and no list reported: nothing to redraw.
        let mut bare = mouse(M, 800, 80, &[]);
        bare.dpi_list = None;
        let mut unknown = mouse(None, 800, 80, &[]);
        unknown.dpi_list = None;
        assert!(!card_changed(&running(vec![bare], vec![]), &running(vec![unknown], vec![])));
    }

    #[test]
    fn only_the_shown_mouse_counts() {
        let first = mouse(M, 800, 80, &[400, 800]);
        let other = mouse(M, 800, 50, &[100, 200]);
        let old = running(vec![first.clone(), other.clone()], vec![]);
        // Another mouse's battery, DPI or list changes: the card shows the first.
        let mut other2 = other.clone();
        other2.battery = None;
        other2.dpi_list = None;
        other2.dpi = Some(400);
        assert!(!card_changed(&old, &running(vec![first.clone(), other2], vec![])));
        // Unrecognized mice before the recognized one are skipped.
        let stranger = mouse(None, 800, 10, &[1]);
        assert!(!card_changed(&old, &running(vec![stranger, first.clone(), other.clone()], vec![])));
        // Reordering changes which mouse is shown, so a different list shows up.
        assert!(card_changed(&old, &running(vec![other, first], vec![])));
    }
}
