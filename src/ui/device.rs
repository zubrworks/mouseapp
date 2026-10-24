//! Device tab: read-only facts about the connected mouse (name, firmware,
//! serial number, battery, connection, Easy-Switch channels).

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSTextField, NSView};
use objc2_foundation::NSString;
use rust_i18n::t;

use super::{Context, as_view, form, hint_label};
use crate::driver::State;
use crate::hidpp::features::battery::Battery;
use crate::hidpp::features::fwinfo::EntityKind;
use crate::hub::MouseInfo;

/// Shown for a value the mouse did not report.
const MISSING: &str = "—";

/// The mouse the tab describes: the first recognized one, else the first.
pub fn shown_mouse(state: &State) -> Option<&MouseInfo> {
    let mice = &state.status()?.mice;
    mice.iter().find(|m| m.model.is_some()).or_else(|| mice.first())
}

/// Whether the tab's content differs between two daemon states.
pub fn changed(old: &State, new: &State) -> bool {
    let key = |s| shown_mouse(s).map(|m| (&m.name, &m.model, &m.battery, &m.details));
    std::mem::discriminant(old) != std::mem::discriminant(new) || key(old) != key(new)
}

/// Text of an optional value: the value, or a dash.
fn or_dash(value: Option<&str>) -> String {
    value.filter(|v| !v.is_empty()).unwrap_or(MISSING).to_string()
}

/// Battery as "40% (discharging)"; a dash when not reported.
fn battery_text(battery: Option<&Battery>) -> String {
    let Some(b) = battery else {
        return MISSING.to_string();
    };
    let pct = b.percent.map_or("?".to_string(), |p| format!("{p}%"));
    format!("{pct} ({})", b.charge.label())
}

/// Texts of the Easy-Switch channels `1..=count`: the host name, "(empty)"
/// for an empty channel, a dash when the name is unknown; the current one is
/// marked.
fn channel_texts(count: u8, current: u8, names: &[String]) -> Vec<String> {
    (0..count)
        .map(|i| {
            let name = match names.get(usize::from(i)) {
                None => MISSING.to_string(),
                Some(n) if n.is_empty() => t!("info.host_empty").to_string(),
                Some(n) => n.clone(),
            };
            if i == current {
                format!("{name}{}", t!("info.host_current"))
            } else {
                name
            }
        })
        .collect()
}

/// `(label, value)` rows describing `mouse`.
fn rows(mouse: &MouseInfo) -> Vec<(String, String)> {
    let details = mouse.details.as_ref();
    let fw = details.and_then(|d| d.firmware.as_ref());
    let version = |kind| or_dash(fw.and_then(|d| d.version(kind)));
    let label = |key: &str| t!(key).to_string();
    let mut rows = vec![
        (label("settings.device.name"), or_dash(Some(&mouse.name))),
        (label("settings.device.model"), or_dash(mouse.model.as_deref())),
        (label("settings.device.serial"), or_dash(fw.and_then(|d| d.serial.as_deref()))),
        (label("settings.device.firmware"), version(EntityKind::Firmware)),
        (label("settings.device.bootloader"), version(EntityKind::Bootloader)),
        (label("settings.device.hardware"), version(EntityKind::Hardware)),
        (
            label("settings.device.model_id"),
            or_dash(fw.and_then(|d| d.model_ids_text()).as_deref()),
        ),
        (label("settings.device.battery"), battery_text(mouse.battery.as_ref())),
        (
            label("settings.device.connection"),
            details.map_or(MISSING.to_string(), |d| d.connection.label()),
        ),
    ];
    if let Some(h) = details.and_then(|d| d.hosts.as_ref()) {
        let names = details.map_or(&[][..], |d| &d.host_names[..]);
        for (i, text) in channel_texts(h.count, h.current, names).into_iter().enumerate() {
            rows.push((t!("settings.device.channel", number = i + 1).to_string(), text));
        }
    }
    rows
}

/// A label whose text can be selected and copied.
fn value_label(text: &str, mtm: MainThreadMarker) -> Retained<NSView> {
    let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    label.setSelectable(true);
    as_view(&label)
}

/// Why there is nothing to show: no mouse, or no driver.
fn empty_message(state: &State) -> String {
    match state {
        State::Stopped => t!("settings.general.driver_stopped"),
        State::Starting => t!("settings.general.driver_starting"),
        State::Running(_) | State::Outdated(_) => t!("menu.no_mouse"),
    }
    .to_string()
}

pub fn build(ctx: &Context, _target: &AnyObject, mtm: MainThreadMarker) -> Retained<NSView> {
    let driver = ctx.driver.borrow();
    let Some(mouse) = shown_mouse(&driver) else {
        let message = hint_label(&NSString::from_str(&empty_message(&driver)), mtm);
        return form(vec![(String::new(), as_view(&message))], mtm);
    };
    let rows = rows(mouse)
        .into_iter()
        .map(|(label, value)| (label, value_label(&value, mtm)))
        .collect();
    form(rows, mtm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::Connection;
    use crate::hidpp::features::battery::Charge;
    use crate::hidpp::features::fwinfo::{DeviceDetails, Entity};
    use crate::hidpp::features::host::Hosts;
    use crate::hub::DeviceInfo;
    use crate::ipc::Status;

    fn state(mouse: Option<MouseInfo>) -> State {
        State::Running(Status { mice: mouse.into_iter().collect(), ..Status::default() })
    }

    fn mouse() -> MouseInfo {
        MouseInfo {
            key: "k".into(),
            name: "Mouse".into(),
            model: Some("Model".into()),
            battery: Some(Battery { percent: Some(40), charge: Charge::Discharging }),
            details: Some(DeviceInfo {
                connection: Connection::Bluetooth,
                firmware: Some(DeviceDetails {
                    model_ids: vec![0xB034],
                    serial: Some("SN1".into()),
                    entities: vec![Entity { kind: EntityKind::Firmware, version: "RBM 1.0".into(), active: true }],
                    ..DeviceDetails::default()
                }),
                hosts: Some(Hosts { count: 3, current: 0 }),
                host_names: vec!["Laptop".into(), String::new()],
            }),
            ..MouseInfo::default()
        }
    }

    #[test]
    fn battery_text_shows_percent_and_state() {
        let b = Battery { percent: Some(40), charge: Charge::Charging };
        assert_eq!(battery_text(Some(&b)), format!("40% ({})", Charge::Charging.label()));
        let b = Battery { percent: None, charge: Charge::Full };
        assert_eq!(battery_text(Some(&b)), format!("? ({})", Charge::Full.label()));
        assert_eq!(battery_text(None), MISSING);
    }

    #[test]
    fn channels_mark_current_and_fill_gaps() {
        let names = vec!["Laptop".to_string(), String::new()];
        let texts = channel_texts(3, 0, &names);
        assert_eq!(texts[0], format!("Laptop{}", t!("info.host_current")));
        assert_eq!(texts[1], t!("info.host_empty"));
        assert_eq!(texts[2], MISSING);
        assert!(channel_texts(0, 0, &names).is_empty());
    }

    #[test]
    fn rows_use_dash_for_missing_values() {
        let r = rows(&mouse());
        let value = |key: &str| r.iter().find(|(l, _)| *l == t!(key)).unwrap().1.clone();
        assert_eq!(value("settings.device.serial"), "SN1");
        assert_eq!(value("settings.device.firmware"), "RBM 1.0");
        assert_eq!(value("settings.device.bootloader"), MISSING);
        assert_eq!(value("settings.device.model_id"), "B034");
        // 9 facts and 3 channels.
        assert_eq!(r.len(), 12);
        let bare = MouseInfo { name: "Mouse".into(), ..MouseInfo::default() };
        let r = rows(&bare);
        assert_eq!(r.len(), 9);
        assert!(r[1..].iter().filter(|(_, v)| v == MISSING).count() >= 7);
    }

    #[test]
    fn shown_mouse_prefers_recognized() {
        let other = MouseInfo { key: "o".into(), ..MouseInfo::default() };
        let s = State::Running(Status { mice: vec![other, mouse()], ..Status::default() });
        assert_eq!(shown_mouse(&s).unwrap().key, "k");
        assert!(shown_mouse(&State::Stopped).is_none());
    }

    #[test]
    fn changed_on_battery_details_and_state() {
        let old = state(Some(mouse()));
        assert!(!changed(&old, &old.clone()));
        let mut m = mouse();
        m.battery = Some(Battery { percent: Some(39), charge: Charge::Discharging });
        assert!(changed(&old, &state(Some(m))));
        let mut m = mouse();
        m.details.as_mut().unwrap().hosts = Some(Hosts { count: 3, current: 1 });
        assert!(changed(&old, &state(Some(m))));
        let mut m = mouse();
        m.details = None;
        assert!(changed(&old, &state(Some(m))));
        // DPI alone does not matter.
        let mut m = mouse();
        m.dpi = Some(1600);
        assert!(!changed(&old, &state(Some(m))));
        assert!(changed(&old, &State::Stopped));
        assert!(changed(&old, &state(None)));
    }
}
