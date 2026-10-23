//! Per-application profiles tab: a table of applications (bundle ids) and,
//! for the selected one, button and gesture actions that replace the global
//! ones.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAccessibility, NSControlTextEditingDelegate, NSImageView, NSScrollView, NSTableColumn,
    NSTableView, NSTableViewDataSource, NSTableViewDelegate, NSTextField, NSView,
};
use objc2_foundation::{NSIndexSet, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use rust_i18n::t;

use super::{
    ActionChoice, COLUMN_GAP, Context, Effect, MARGIN, MIN_WIDTH, ROW_GAP, action_choice, action_popup, as_view, button, form, popup,
    popup_index, read_config, remappable_buttons, tr_ns, update_config, wire,
};
use crate::config::{AppOverride, GestureSet};
use crate::macos::{app_icon, app_name, choose_app, running_apps};

const ID_TABLE: &str = "apps.table";
const ID_RUNNING: &str = "apps.running";
const ID_CHOOSE: &str = "apps.choose";
const ID_REMOVE: &str = "apps.remove";
const PREFIX_BUTTON: &str = "apps.b.";
const PREFIX_GESTURE: &str = "apps.g.";

/// Width of the tab without an application selected.
const WIDTH: f64 = MIN_WIDTH;
/// Side of the application icon in the table.
const ICON_SIZE: f64 = 18.0;
/// Height of a row of the table.
const ROW_HEIGHT: f64 = 24.0;
/// Height of the table of applications.
const TABLE_HEIGHT: f64 = 120.0;

/// State of the Applications tab, kept in the window [`Context`].
#[derive(Default)]
pub struct AppsState {
    /// Bundle id selected in the table.
    selected: RefCell<Option<String>>,
    /// Bundle ids offered in the running-applications popup (after the
    /// placeholder item).
    running: RefCell<Vec<String>>,
    /// The current table with its data source.
    table: RefCell<Option<AppsTable>>,
}

/// A table and its data source. The table only points to the source, so the
/// source is kept here and the table is detached from it before it is freed.
struct AppsTable {
    table: Retained<NSTableView>,
    /// Only held, so that it lives as long as the table.
    _source: Retained<AppsSource>,
}

impl Drop for AppsTable {
    fn drop(&mut self) {
        // Runs before the fields are released: the table never points to a
        // freed source, even if something still holds the table.
        // SAFETY: setting no data source or delegate is always valid.
        unsafe {
            self.table.setDataSource(None);
            self.table.setDelegate(None);
        }
    }
}

struct SourceIvars {
    rows: Vec<String>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; `AppsSource` has no Drop.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = SourceIvars]
    #[name = "MouseAppsSource"]
    struct AppsSource;

    // SAFETY: NSObjectProtocol has no safety requirements.
    unsafe impl NSObjectProtocol for AppsSource {}

    // SAFETY: the method signatures match the protocol.
    unsafe impl NSTableViewDataSource for AppsSource {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, _table: &NSTableView) -> isize {
            self.ivars().rows.len() as isize
        }
    }

    // SAFETY: the protocol has only optional methods, none implemented here.
    unsafe impl NSControlTextEditingDelegate for AppsSource {}

    // SAFETY: the method signature matches the protocol.
    unsafe impl NSTableViewDelegate for AppsSource {
        #[unsafe(method_id(tableView:viewForTableColumn:row:))]
        fn view_for_row(
            &self,
            _table: &NSTableView,
            _column: Option<&NSTableColumn>,
            row: isize,
        ) -> Option<Retained<NSView>> {
            let mtm = self.mtm();
            let index = usize::try_from(row).ok();
            index
                .and_then(|i| self.ivars().rows.get(i))
                .map(|id| row_view(id, mtm))
        }
    }
);

/// Table row: icon and name of the application `id`.
fn row_view(id: &str, mtm: MainThreadMarker) -> Retained<NSView> {
    let icon = app_icon(id);
    icon.setSize(NSSize::new(ICON_SIZE, ICON_SIZE));
    let image = NSImageView::imageViewWithImage(&icon, mtm);
    image.setFrame(NSRect::new(
        NSPoint::new(2.0, (ROW_HEIGHT - ICON_SIZE) / 2.0),
        NSSize::new(ICON_SIZE, ICON_SIZE),
    ));
    let label = NSTextField::labelWithString(&NSString::from_str(&app_name(id)), mtm);
    let label_size = label.fittingSize();
    label.setFrame(NSRect::new(
        NSPoint::new(ICON_SIZE + COLUMN_GAP, ((ROW_HEIGHT - label_size.height) / 2.0).round()),
        NSSize::new(WIDTH, label_size.height),
    ));
    let cell = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::ZERO, NSSize::new(WIDTH, ROW_HEIGHT)),
    );
    cell.addSubview(&image);
    cell.addSubview(&label);
    cell
}

impl AppsSource {
    fn new(rows: Vec<String>, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(SourceIvars { rows });
        // SAFETY: NSObject's `init` has this signature.
        unsafe { msg_send![super(this), init] }
    }
}

/// Adds `bundle` to the configuration and selects it.
fn add_app(ctx: &Context, mtm: MainThreadMarker, bundle: String) -> Vec<Effect> {
    select(ctx, Some(bundle.clone()));
    update_config(ctx, mtm, |c| {
        c.apps.entry(bundle).or_default();
    })
}

/// Bundle id to show as selected: the remembered one if it still exists,
/// else the first application.
fn selection(ctx: &Context, apps: &[String]) -> Option<String> {
    let current = ctx.apps.selected.borrow().clone();
    current
        .filter(|c| apps.contains(c))
        .or_else(|| apps.first().cloned())
}

fn select(ctx: &Context, id: Option<String>) {
    *ctx.apps.selected.borrow_mut() = id;
}

pub fn build(ctx: &Context, target: &AnyObject, mtm: MainThreadMarker) -> Retained<NSView> {
    let cfg = read_config(ctx);
    let apps: Vec<String> = cfg.apps.keys().cloned().collect();
    let selected = selection(ctx, &apps);
    select(ctx, selected.clone());

    // Table of applications.
    let source = AppsSource::new(apps.clone(), mtm);
    let table = NSTableView::initWithFrame(NSTableView::alloc(mtm), NSRect::ZERO);
    let column =
        NSTableColumn::initWithIdentifier(NSTableColumn::alloc(mtm), &NSString::from_str("app"));
    column.setWidth(WIDTH - 2.0 * MARGIN - 20.0);
    column.setEditable(false);
    table.addTableColumn(&column);
    table.setHeaderView(None);
    // SAFETY: the data source is kept alive in `ctx.apps.table` below, for as
    // long as the table (see `AppsTable`).
    unsafe {
        table.setDataSource(Some(ProtocolObject::from_ref(&*source)));
        table.setDelegate(Some(ProtocolObject::from_ref(&*source)));
    }
    table.setRowHeight(ROW_HEIGHT);
    wire(&table, ID_TABLE, target);
    if let Some(index) = selected
        .as_ref()
        .and_then(|s| apps.iter().position(|a| a == s))
    {
        table.selectRowIndexes_byExtendingSelection(&NSIndexSet::indexSetWithIndex(index), false);
    }
    // Replacing the old pair detaches the old table from its source first.
    *ctx.apps.table.borrow_mut() = Some(AppsTable { table: table.clone(), _source: source });
    let scroll = NSScrollView::initWithFrame(
        NSScrollView::alloc(mtm),
        NSRect::new(NSPoint::ZERO, NSSize::new(WIDTH - 2.0 * MARGIN, TABLE_HEIGHT)),
    );
    scroll.setHasVerticalScroller(true);
    scroll.setDocumentView(Some(&table));
    table.setAccessibilityLabel(Some(&tr_ns("settings.apps.table")));

    // Popup of running applications, the file chooser and the remove button.
    let running = running_apps();
    let mut titles = vec![t!("settings.apps.add_running").to_string()];
    titles.extend(running.iter().map(|(_, name)| name.clone()));
    *ctx.apps.running.borrow_mut() = running.into_iter().map(|(id, _)| id).collect();
    let running_popup = popup(&titles, 0, ID_RUNNING, target, mtm);
    running_popup.setAccessibilityLabel(Some(&tr_ns("settings.apps.add_running")));
    let choose = button(&t!("settings.apps.choose"), ID_CHOOSE, target, mtm);
    let remove = button(&t!("settings.apps.remove"), ID_REMOVE, target, mtm);
    remove.setEnabled(selected.is_some());

    // Overrides of the selected application.
    let overrides = selected.as_ref().and_then(|bundle| {
        let profile = cfg.apps.get(bundle)?;
        Some(overrides_form(ctx, profile, target, mtm))
    });

    let width = WIDTH.max(overrides.as_ref().map_or(0.0, |o| o.frame().size.width));
    // The description wraps within the width of the tab.
    let hint = NSTextField::wrappingLabelWithString(&tr_ns("settings.apps.hint"), mtm);
    hint.setPreferredMaxLayoutWidth(width - 2.0 * MARGIN);
    let overrides_height = overrides.as_ref().map_or(0.0, |o| o.frame().size.height);
    let hint_size = NSSize::new(width - 2.0 * MARGIN, hint.fittingSize().height);
    let row_height = 28.0;
    let gap = ROW_GAP;
    let height = overrides_height
        + 4.0 * MARGIN / 2.0
        + row_height
        + gap
        + TABLE_HEIGHT
        + gap
        + hint_size.height
        + MARGIN;

    let content = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::ZERO, NSSize::new(width, height)),
    );
    let mut y = 0.0;
    if let Some(o) = &overrides {
        o.setFrame(NSRect::new(
            NSPoint::new(((width - o.frame().size.width) / 2.0).round(), y),
            o.frame().size,
        ));
        content.addSubview(o);
        y += overrides_height;
    }
    y += MARGIN / 2.0;
    let popup_width = 220.0;
    let button_width = 90.0;
    running_popup.setFrame(NSRect::new(
        NSPoint::new(MARGIN, y),
        NSSize::new(popup_width, row_height),
    ));
    choose.setFrame(NSRect::new(
        NSPoint::new(MARGIN + popup_width + COLUMN_GAP, y),
        NSSize::new(button_width, row_height),
    ));
    remove.setFrame(NSRect::new(
        NSPoint::new(width - MARGIN - button_width, y),
        NSSize::new(button_width, row_height),
    ));
    content.addSubview(&running_popup);
    content.addSubview(&choose);
    content.addSubview(&remove);
    y += row_height + gap;
    scroll.setFrame(NSRect::new(
        NSPoint::new(MARGIN, y),
        NSSize::new(width - 2.0 * MARGIN, TABLE_HEIGHT),
    ));
    content.addSubview(&scroll);
    y += TABLE_HEIGHT + gap;
    hint.setFrame(NSRect::new(NSPoint::new(MARGIN, y), hint_size));
    content.addSubview(&hint);
    content
}

/// Form with an action popup for every button and gesture; "same as
/// global" means the application has no override.
fn overrides_form(
    ctx: &Context,
    profile: &AppOverride,
    target: &AnyObject,
    mtm: MainThreadMarker,
) -> Retained<NSView> {
    let unset = t!("settings.apps.same_as_global").to_string();
    let mut rows: Vec<(String, Retained<NSView>)> = Vec::new();
    for b in remappable_buttons(ctx) {
        let popup = action_popup(
            profile.buttons.get(b.key).map(String::as_str),
            true,
            &unset,
            &format!("{PREFIX_BUTTON}{}", b.key),
            target,
            mtm,
        );
        rows.push((format!("{}:", b.label()), as_view(&popup)));
    }
    for slot in GestureSet::SLOTS {
        let popup = action_popup(
            profile.gestures.thumb.get(slot),
            false,
            &unset,
            &format!("{PREFIX_GESTURE}{slot}"),
            target,
            mtm,
        );
        rows.push((
            t!(format!("settings.gestures.{slot}")).to_string(),
            as_view(&popup),
        ));
    }
    form(rows, mtm)
}

pub fn changed(id: &str, sender: &AnyObject, ctx: &Context, mtm: MainThreadMarker) -> Vec<Effect> {
    match id {
        ID_TABLE => {
            let Some(table) = sender.downcast_ref::<NSTableView>() else {
                return Vec::new();
            };
            let apps: Vec<String> = read_config(ctx).apps.keys().cloned().collect();
            let row = usize::try_from(table.selectedRow()).ok();
            select(ctx, row.and_then(|r| apps.get(r).cloned()));
            Vec::new()
        }
        ID_RUNNING => {
            let Some(index) = popup_index(sender).and_then(|i| i.checked_sub(1)) else {
                return Vec::new();
            };
            let bundle = ctx.apps.running.borrow().get(index).cloned();
            match bundle {
                Some(bundle) => add_app(ctx, mtm, bundle),
                None => Vec::new(),
            }
        }
        ID_CHOOSE => {
            match choose_app(mtm) {
                Some(bundle) => add_app(ctx, mtm, bundle),
                None => Vec::new(),
            }
        }
        ID_REMOVE => {
            let Some(bundle) = ctx.apps.selected.borrow().clone() else {
                return Vec::new();
            };
            select(ctx, None);
            update_config(ctx, mtm, |c| {
                c.apps.remove(&bundle);
            })
        }
        _ => override_changed(id, sender, ctx, mtm),
    }
}

/// A change of a button or gesture popup of the selected application.
fn override_changed(
    id: &str,
    sender: &AnyObject,
    ctx: &Context,
    mtm: MainThreadMarker,
) -> Vec<Effect> {
    let Some(bundle) = ctx.apps.selected.borrow().clone() else {
        return Vec::new();
    };
    let cfg = read_config(ctx);
    let Some(profile) = cfg.apps.get(&bundle) else {
        return Vec::new();
    };
    let button = id.strip_prefix(PREFIX_BUTTON).filter(|k| {
        remappable_buttons(ctx).iter().any(|b| b.key == *k)
    });
    let slot = id
        .strip_prefix(PREFIX_GESTURE)
        .filter(|s| GestureSet::SLOTS.contains(s));
    let current = match (button, slot) {
        (Some(k), _) => profile.buttons.get(k).cloned(),
        (_, Some(s)) => profile.gestures.thumb.get(s).map(str::to_string),
        _ => return Vec::new(),
    };
    match action_choice(sender, current.as_deref(), button.is_some(), mtm) {
        ActionChoice::Set(action) => update_config(ctx, mtm, |c| {
            let Some(p) = c.apps.get_mut(&bundle) else {
                return;
            };
            match (button, slot, action) {
                (Some(k), _, Some(a)) => {
                    p.buttons.insert(k.to_string(), a);
                }
                (Some(k), _, None) => {
                    p.buttons.remove(k);
                }
                (_, Some(s), a) => {
                    if let Some(target) = p.gestures.thumb.slot_mut(s) {
                        *target = a;
                    }
                }
                _ => {}
            }
        }),
        ActionChoice::Keep => Vec::new(),
    }
}
