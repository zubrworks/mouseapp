//! `debug window`: the controls and texts of the settings window, read
//! from its views (labels, fields, popups, switches, sliders, buttons,
//! tables), in view order.

use std::fmt::Write;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::msg_send;
use objc2_app_kit::{
    NSButton, NSControl, NSPopUpButton, NSSlider, NSSwitch, NSTableView, NSTextField,
    NSUserInterfaceItemIdentification, NSView, NSWindow,
};
use objc2_foundation::NSString;

/// One control or text of the window.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Line {
    /// `label`, `field`, `popup`, `switch`, `slider`, `button`, `table`, `image`.
    pub kind: &'static str,
    pub text: String,
    /// The value, when the control has one besides its text.
    pub value: Option<String>,
    /// The identifier the app gave the control (see `ui::dispatch`).
    pub id: Option<String>,
    pub enabled: bool,
}

/// The window as text: a header line, then one line per control.
pub fn dump(window: Option<&NSWindow>, tab: &str) -> String {
    let Some(window) = window else {
        return "window: none\n".to_string();
    };
    let mut lines = Vec::new();
    if let Some(view) = window.contentView() {
        walk(&view, &mut lines);
    }
    let size = window.frame().size;
    let mut out = format!(
        "window: \"{}\" tab={tab} open={} size={}x{}\n",
        window.title(),
        window.isVisible(),
        size.width.round(),
        size.height.round()
    );
    out.push_str(&format(&lines));
    out
}

/// The lines as text: `kind "text" value=... id=... (disabled)`.
pub fn format(lines: &[Line]) -> String {
    let mut out = String::new();
    for l in lines {
        let _ = write!(out, "{} {:?}", l.kind, l.text);
        if let Some(v) = &l.value {
            let _ = write!(out, " value={v}");
        }
        if let Some(id) = &l.id {
            let _ = write!(out, " id={id}");
        }
        if !l.enabled {
            let _ = write!(out, " (disabled)");
        }
        out.push('\n');
    }
    out
}

/// Adds the lines of `view` and its visible subviews.
fn walk(view: &NSView, out: &mut Vec<Line>) {
    if view.isHidden() {
        return;
    }
    if let Some(line) = describe(view) {
        // The subviews of a control are its parts; a table's are its rows.
        let leaf = line.kind != "table";
        out.push(line);
        if leaf {
            return;
        }
    }
    for sub in view.subviews().iter() {
        walk(&sub, out);
    }
}

fn text(s: Option<Retained<NSString>>) -> String {
    s.map(|s| s.to_string()).unwrap_or_default()
}

/// The line of a view that shows something; `None` for containers.
fn describe(view: &NSView) -> Option<Line> {
    let obj: &AnyObject = view;
    let id = view.identifier().map(|i| i.to_string()).filter(|i| !i.is_empty());
    let enabled = obj.downcast_ref::<NSControl>().is_none_or(|c| c.isEnabled());
    let line = |kind, text: String, value: Option<String>| Line { kind, text, value, id: id.clone(), enabled };
    if let Some(p) = obj.downcast_ref::<NSPopUpButton>() {
        let items: Vec<String> = p.itemTitles().iter().map(|s| s.to_string()).collect();
        return Some(line("popup", text(p.titleOfSelectedItem()), Some(format!("[{}]", items.join("|")))));
    }
    if let Some(s) = obj.downcast_ref::<NSSwitch>() {
        let on = if s.state() != 0 { "on" } else { "off" };
        return Some(line("switch", accessibility_label(view), Some(on.into())));
    }
    if let Some(s) = obj.downcast_ref::<NSSlider>() {
        let range = format!("{} ({}..{})", s.doubleValue(), s.minValue(), s.maxValue());
        return Some(line("slider", accessibility_label(view), Some(range)));
    }
    if let Some(b) = obj.downcast_ref::<NSButton>() {
        // SAFETY: `state` is an NSInteger property of every NSButton.
        let state: isize = unsafe { msg_send![b, state] };
        let title = b.title().to_string();
        let title = if title.is_empty() { accessibility_label(view) } else { title };
        return Some(line("button", title, (state != 0).then(|| "on".to_string())));
    }
    if let Some(f) = obj.downcast_ref::<NSTextField>() {
        let kind = if f.isEditable() { "field" } else { "label" };
        let placeholder = text(f.placeholderString());
        let value = (f.isEditable() && !placeholder.is_empty()).then(|| format!("placeholder:{placeholder:?}"));
        return Some(line(kind, f.stringValue().to_string(), value));
    }
    if let Some(t) = obj.downcast_ref::<NSTableView>() {
        let size = format!("{}x{}", t.numberOfRows(), t.numberOfColumns());
        return Some(line("table", String::new(), Some(size)));
    }
    if view.class().name().to_string_lossy().contains("ImageView") {
        let label = accessibility_label(view);
        return (!label.is_empty()).then(|| line("image", label, None));
    }
    None
}

/// What VoiceOver says for `view` (empty if nothing set).
fn accessibility_label(view: &NSView) -> String {
    // SAFETY: every NSView answers `accessibilityLabel` with an optional NSString.
    let label: Option<Retained<NSString>> = unsafe { msg_send![view, accessibilityLabel] };
    text(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_lines() {
        let lines = [
            Line { kind: "label", text: "Pointer speed".into(), enabled: true, ..Line::default() },
            Line {
                kind: "popup",
                text: "Default".into(),
                value: Some("[Default|Mój]".into()),
                id: Some("profile".into()),
                enabled: false,
            },
        ];
        assert_eq!(
            format(&lines),
            "label \"Pointer speed\"\npopup \"Default\" value=[Default|Mój] id=profile (disabled)\n"
        );
    }
}
