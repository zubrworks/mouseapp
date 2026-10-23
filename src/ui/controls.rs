//! Helpers that build and read AppKit controls: buttons, switches, popups,
//! sliders and the two-column form.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly, Message, sel};
use objc2_app_kit::NSUserInterfaceItemIdentification;
use objc2_app_kit::{
    NSAccessibility, NSButton, NSControl, NSControlStateValueOff, NSControlStateValueOn,
    NSGridCellPlacement, NSGridRowAlignment, NSGridView, NSLayoutAttribute, NSLayoutConstraint,
    NSLayoutRelation, NSPopUpButton, NSSlider, NSSwitch, NSTextField, NSView,
};
use objc2_foundation::{NSArray, NSPoint, NSRect, NSSize, NSString};
use rust_i18n::t;

/// A localized text as an `NSString`.
pub fn tr_ns(key: &str) -> Retained<NSString> {
    NSString::from_str(&t!(key))
}

/// A push button reporting to `target` as `id`.
pub fn button(
    title: &str,
    id: &str,
    target: &AnyObject,
    mtm: MainThreadMarker,
) -> Retained<NSButton> {
    // SAFETY: no target or action yet; `wire` sets them below.
    let b = unsafe {
        NSButton::buttonWithTitle_target_action(&NSString::from_str(title), None, None, mtm)
    };
    wire(&b, id, target);
    b
}

/// Gives a control a spoken description (for VoiceOver). A container such as
/// a slider with its value label passes it on to the first control inside.
pub fn describe(view: &NSView, label: &str) {
    if label.is_empty() {
        return;
    }
    if view.downcast_ref::<NSControl>().is_some() {
        view.setAccessibilityLabel(Some(&NSString::from_str(label)));
        return;
    }
    for sub in view.subviews().iter() {
        if sub.downcast_ref::<NSSlider>().is_some() {
            sub.setAccessibilityLabel(Some(&NSString::from_str(label)));
        }
    }
}

/// Makes `control` report changes as `id` to `target`.
pub fn wire(control: &NSControl, id: &str, target: &AnyObject) {
    // SAFETY: `target` implements `controlChanged:` (see window.rs) and
    // outlives the control (the window owner keeps it).
    unsafe {
        control.setTarget(Some(target));
        control.setAction(Some(sel!(controlChanged:)));
    }
    control.setIdentifier(Some(&NSString::from_str(id)));
}

/// Any view (control, label...) as a plain retained `NSView`.
pub fn as_view(view: &NSView) -> Retained<NSView> {
    view.retain()
}

/// Margin around the content of every tab.
pub const MARGIN: f64 = 20.0;
/// Vertical space between rows and blocks of a tab.
pub const ROW_GAP: f64 = 12.0;
/// Horizontal space between neighbouring controls.
pub const COLUMN_GAP: f64 = 8.0;
/// Width at which a description inside a form wraps.
pub const HINT_WIDTH: f64 = 300.0;
/// Narrowest content of a tab.
pub const MIN_WIDTH: f64 = 460.0;

/// A description that wraps at [`HINT_WIDTH`].
pub fn hint_label(text: &NSString, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let label = NSTextField::wrappingLabelWithString(text, mtm);
    label.setPreferredMaxLayoutWidth(HINT_WIDTH);
    label
}

/// Two-column form (label, control) in the macOS settings style: labels
/// right-aligned, rows on a common baseline, [`MARGIN`] around.
pub fn form(rows: Vec<(String, Retained<NSView>)>, mtm: MainThreadMarker) -> Retained<NSView> {
    let rows: Vec<Retained<NSArray<NSView>>> = rows
        .into_iter()
        .map(|(label, control)| {
            describe(&control, label.trim_end_matches(':'));
            let label = NSTextField::labelWithString(&NSString::from_str(&label), mtm);
            NSArray::from_retained_slice(&[as_view(&label), control])
        })
        .collect();
    let grid = NSGridView::gridViewWithViews(&NSArray::from_retained_slice(&rows), mtm);
    grid.setRowAlignment(NSGridRowAlignment::FirstBaseline);
    grid.setRowSpacing(ROW_GAP);
    grid.setColumnSpacing(COLUMN_GAP);
    if grid.numberOfColumns() > 0 {
        grid.columnAtIndex(0)
            .setXPlacement(NSGridCellPlacement::Trailing);
    }
    let size = grid.fittingSize();
    let width = (size.width + 2.0 * MARGIN).max(MIN_WIDTH);
    let height = size.height + 2.0 * MARGIN;
    // Center the form horizontally like System Settings panes.
    grid.setFrame(NSRect::new(
        NSPoint::new(((width - size.width) / 2.0).round(), MARGIN),
        size,
    ));
    let content = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height)),
    );
    content.addSubview(&grid);
    content
}

/// A switch showing `on`.
pub fn switch(on: bool, id: &str, target: &AnyObject, mtm: MainThreadMarker) -> Retained<NSSwitch> {
    let switch = NSSwitch::initWithFrame(NSSwitch::alloc(mtm), NSRect::ZERO);
    switch.setState(if on {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    wire(&switch, id, target);
    switch
}

/// Is a switch (the `sender` of a change) turned on?
pub fn is_on(sender: &AnyObject) -> Option<bool> {
    sender
        .downcast_ref::<NSSwitch>()
        .map(|s| s.state() == NSControlStateValueOn)
}

/// A popup with `titles`, `selected` chosen.
pub fn popup(
    titles: &[String],
    selected: usize,
    id: &str,
    target: &AnyObject,
    mtm: MainThreadMarker,
) -> Retained<NSPopUpButton> {
    let popup =
        NSPopUpButton::initWithFrame_pullsDown(NSPopUpButton::alloc(mtm), NSRect::ZERO, false);
    for title in titles {
        popup.addItemWithTitle(&NSString::from_str(title));
    }
    popup.selectItemAtIndex(selected as isize);
    wire(&popup, id, target);
    popup
}

/// Index chosen in a popup (the `sender` of a change).
pub fn popup_index(sender: &AnyObject) -> Option<usize> {
    let popup = sender.downcast_ref::<NSPopUpButton>()?;
    usize::try_from(popup.indexOfSelectedItem()).ok()
}

/// Slider with a value label next to it. The action is sent while the knob
/// moves (the label follows, see `live_update`) and when it is released
/// (the value is saved). `text` is the label.
pub fn slider(
    range: (f64, f64),
    value: f64,
    text: &str,
    id: &str,
    target: &AnyObject,
    mtm: MainThreadMarker,
) -> Retained<NSView> {
    const SLIDER_WIDTH: f64 = 160.0;
    const LABEL_WIDTH: f64 = 90.0;
    const HEIGHT: f64 = 22.0;
    let slider = NSSlider::initWithFrame(
        NSSlider::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(SLIDER_WIDTH, HEIGHT)),
    );
    slider.setMinValue(range.0);
    slider.setMaxValue(range.1);
    slider.setDoubleValue(value.clamp(range.0, range.1));
    // Continuous: the action also fires while dragging (see `live_update`).
    slider.setContinuous(true);
    wire(&slider, id, target);
    let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    label.setFrame(NSRect::new(
        NSPoint::new(SLIDER_WIDTH + COLUMN_GAP, 2.0),
        NSSize::new(LABEL_WIDTH, 18.0),
    ));
    let view = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(SLIDER_WIDTH + COLUMN_GAP + LABEL_WIDTH, HEIGHT),
        ),
    );
    view.addSubview(&slider);
    view.addSubview(&label);
    // A plain view has no intrinsic size, so the grid would shrink it to
    // zero: the slider would still be drawn but get no mouse events.
    fix_size(&view, SLIDER_WIDTH + COLUMN_GAP + LABEL_WIDTH, HEIGHT);
    view
}

/// Pins the Auto Layout size of a container view to `width` x `height`.
pub(super) fn fix_size(view: &NSView, width: f64, height: f64) {
    for (attr, value) in [
        (NSLayoutAttribute::Width, width),
        (NSLayoutAttribute::Height, height),
    ] {
        // SAFETY: `view` is an NSView and there is no second item.
        let c = unsafe {
            NSLayoutConstraint::constraintWithItem_attribute_relatedBy_toItem_attribute_multiplier_constant(
                view,
                attr,
                NSLayoutRelation::Equal,
                None,
                NSLayoutAttribute::NotAnAttribute,
                1.0,
                value,
            )
        };
        c.setActive(true);
    }
}

/// While a slider knob is dragged: shows the current value in the label
/// next to it (the first number of the label text is replaced) and returns
/// true. Returns false when the change is final (mouse released, keyboard).
pub fn live_update(sender: &AnyObject, mtm: MainThreadMarker) -> bool {
    use objc2_app_kit::{NSApplication, NSEventType};
    let Some(slider) = sender.downcast_ref::<NSSlider>() else {
        return false;
    };
    let dragging = NSApplication::sharedApplication(mtm)
        .currentEvent()
        .is_some_and(|e| {
            matches!(
                e.r#type(),
                NSEventType::LeftMouseDown | NSEventType::LeftMouseDragged
            )
        });
    if !dragging {
        return false;
    }
    show_slider_value(slider, slider.doubleValue().round() as i64);
    true
}

/// Replaces the first run of digits in `text` with `value`.
fn replace_number(text: &str, value: i64) -> String {
    let Some(start) = text.find(|c: char| c.is_ascii_digit()) else {
        return value.to_string();
    };
    let end = text[start..]
        .find(|c: char| !c.is_ascii_digit())
        .map_or(text.len(), |n| start + n);
    format!("{}{value}{}", &text[..start], &text[end..])
}

/// Moves the slider to `value` and writes it into the label next to it.
pub fn show_slider_value(slider: &NSSlider, value: i64) {
    slider.setDoubleValue(value as f64);
    // SAFETY: the slider is used on the main thread, no other references.
    let Some(parent) = (unsafe { slider.superview() }) else {
        return;
    };
    for sub in parent.subviews().iter() {
        if let Some(label) = sub.downcast_ref::<NSTextField>() {
            let old = label.stringValue().to_string();
            label.setStringValue(&NSString::from_str(&replace_number(&old, value)));
        }
    }
}

/// Slider value (the `sender` of a change) rounded to a multiple of `step`.
pub fn slider_value(sender: &AnyObject, step: f64) -> Option<f64> {
    let slider = sender.downcast_ref::<NSSlider>()?;
    Some((slider.doubleValue() / step).round() * step)
}

#[cfg(test)]
mod live_tests {
    use super::replace_number;

    #[test]
    fn replaces_first_number() {
        assert_eq!(replace_number("100%", 245), "245%");
        assert_eq!(replace_number("1600 DPI", 800), "800 DPI");
        assert_eq!(replace_number("83", 5), "5");
        assert_eq!(replace_number("none", 7), "7");
    }
}
