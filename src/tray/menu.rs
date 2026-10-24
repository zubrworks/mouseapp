//! The menu content (independent of AppKit, so it can be tested), its
//! rendering with `muda` and the decoding of menu clicks.

use anyhow::{Result, anyhow};
use muda::{CheckMenuItem, IconMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use objc2_foundation::{NSObjectProtocol, NSString};
use rust_i18n::t;

use super::icons::battery_icon;
use crate::autostart;
use crate::driver::State;
use crate::hidpp::features::battery::Battery;
use crate::hub::MouseInfo;
use crate::ipc;
use crate::permissions::Permission;
use crate::profiles::{self, Store};

/// DPI values offered in the quick menu (only those the mouse supports).
const DPI_PRESETS: [u16; 8] = [400, 800, 1200, 1600, 2400, 3200, 4000, 6400];

/// One entry of the menu, independent of AppKit so it can be tested.
#[derive(Debug, PartialEq, Eq)]
enum Item {
    Label(String),
    /// Mouse name with a template battery icon (icon omitted without a battery).
    Header {
        id: String,
        title: String,
        battery: Option<Battery>,
    },
    Separator,
    Submenu(String, Vec<Item>),
    Check {
        id: String,
        title: String,
        checked: bool,
    },
    Action {
        id: String,
        title: String,
    },
}

const ID_SETTINGS: &str = "settings";
const ID_SETTINGS_FILE: &str = "settings_file";
const ID_RELOAD: &str = "reload";
const ID_RESTART_DOCK: &str = "restart_dock";
const ID_ABOUT: &str = "about";
const ID_QUIT: &str = "quit";
const ID_AUTOSTART: &str = "autostart";
const ID_PERMISSION: &str = "permission.";
const ID_PROFILE: &str = "profile:";
const ID_MANAGE_PROFILES: &str = "manage_profiles";
const ID_START_DRIVER: &str = "start_driver";

/// Profiles offered in the menu: every name (the built-in one first) and
/// the active one.
struct Profiles<'a> {
    names: &'a [String],
    active: &'a str,
    /// The current settings differ from the active profile.
    modified: bool,
}

/// A decoded menu click.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Click {
    Dpi(u16),
    Settings,
    SettingsFile,
    Reload,
    Autostart,
    RestartDock,
    Permission(Permission),
    Profile(String),
    ManageProfiles,
    StartDriver,
    About,
    Quit,
}

pub(super) fn parse_click(id: &str) -> Option<Click> {
    match id {
        ID_SETTINGS => return Some(Click::Settings),
        ID_SETTINGS_FILE => return Some(Click::SettingsFile),
        ID_RELOAD => return Some(Click::Reload),
        ID_AUTOSTART => return Some(Click::Autostart),
        ID_RESTART_DOCK => return Some(Click::RestartDock),
        ID_ABOUT => return Some(Click::About),
        ID_QUIT => return Some(Click::Quit),
        ID_MANAGE_PROFILES => return Some(Click::ManageProfiles),
        ID_START_DRIVER => return Some(Click::StartDriver),
        _ => {}
    }
    if let Some(name) = id.strip_prefix(ID_PROFILE) {
        return Some(Click::Profile(name.to_string()));
    }
    if let Some(p) = id.strip_prefix(ID_PERMISSION) {
        return Permission::from_id(p).map(Click::Permission);
    }
    let (kind, rest) = id.split_once(':')?;
    match kind {
        "dpi" => rest.parse().ok().map(Click::Dpi),
        _ => None,
    }
}

/// DPI presets the mouse supports, plus its current value if not a preset.
fn dpi_choices(m: &MouseInfo) -> Vec<u16> {
    let mut v: Vec<u16> = match &m.dpi_list {
        Some(l) => DPI_PRESETS
            .iter()
            .copied()
            .filter(|&d| l.contains(d))
            .collect(),
        None => Vec::new(),
    };
    if let Some(cur) = m.dpi
        && !v.contains(&cur)
    {
        v.push(cur);
        v.sort_unstable();
    }
    v
}

/// Builds the menu content for the current state of the daemon.
fn model(driver: &State, launch_at_login: bool, profiles: &Profiles) -> Vec<Item> {
    let mut out = Vec::new();
    match driver {
        State::Running(status) => mouse_items(&mut out, status),
        State::Outdated(status) => {
            out.push(Item::Label(t!("menu.driver_outdated").to_string()));
            out.push(Item::Separator);
            mouse_items(&mut out, status);
        }
        State::Starting => {
            out.push(Item::Label(t!("menu.driver_starting").to_string()));
            out.push(Item::Separator);
        }
        State::Stopped => {
            out.push(Item::Label(t!("menu.driver_not_running").to_string()));
            out.push(Item::Action {
                id: ID_START_DRIVER.into(),
                title: t!("menu.start_driver").to_string(),
            });
            out.push(Item::Separator);
        }
    }
    if !matches!(out.last(), Some(Item::Separator)) {
        out.push(Item::Separator);
    }
    if !profiles.names.is_empty() {
        let mut items: Vec<Item> = profiles
            .names
            .iter()
            .map(|n| Item::Check {
                id: format!("{ID_PROFILE}{n}"),
                title: profiles::display_name(n),
                checked: n == profiles.active,
            })
            .collect();
        items.push(Item::Separator);
        items.push(Item::Action {
            id: ID_MANAGE_PROFILES.into(),
            title: t!("menu.manage_profiles").to_string(),
        });
        let title = t!("menu.profile", name = profiles::marked_name(profiles.active, profiles.modified));
        out.push(Item::Submenu(title.to_string(), items));
    }
    out.push(Item::Action {
        id: ID_SETTINGS.into(),
        title: t!("menu.settings").to_string(),
    });
    out.push(Item::Action {
        id: ID_SETTINGS_FILE.into(),
        title: t!("menu.show_settings_file").to_string(),
    });
    // Reloading is the daemon's job.
    if driver.status().is_some() {
        out.push(Item::Action {
            id: ID_RELOAD.into(),
            title: t!("menu.reload").to_string(),
        });
    }
    out.push(Item::Check {
        id: ID_AUTOSTART.into(),
        title: t!("menu.launch_at_login").to_string(),
        checked: launch_at_login,
    });
    out.push(Item::Action {
        id: ID_RESTART_DOCK.into(),
        title: t!("menu.restart_dock").to_string(),
    });
    out.push(Item::Separator);
    out.push(Item::Action {
        id: ID_ABOUT.into(),
        title: t!("menu.about").to_string(),
    });
    out.push(Item::Action {
        id: ID_QUIT.into(),
        title: t!("menu.quit").to_string(),
    });
    out
}

/// Notices of the running daemon (wheel settings, permissions) and the
/// mice it serves.
fn mouse_items(out: &mut Vec<Item>, status: &ipc::Status) {
    if let Some(reason) = &status.scroll_problem {
        out.push(Item::Label(t!("menu.scroll_problem", reason = reason).to_string()));
        out.push(Item::Separator);
    }
    for p in &status.missing {
        out.push(Item::Label(t!("permissions.missing", name = p.name()).to_string()));
        out.push(Item::Action {
            id: format!("{ID_PERMISSION}{}", p.id()),
            title: t!("menu.open_permission", name = p.name()).to_string(),
        });
        out.push(Item::Separator);
    }
    if status.mice.is_empty() {
        out.push(Item::Label(t!("menu.no_mouse").to_string()));
    }
    for m in &status.mice {
        out.push(Item::Header {
            id: format!("header:{}", m.key),
            title: m.model.clone().unwrap_or_else(|| m.name.clone()),
            battery: m.battery,
        });
        let choices = dpi_choices(m);
        if !choices.is_empty() {
            let items = choices
                .into_iter()
                .map(|d| Item::Check {
                    id: format!("dpi:{d}"),
                    title: t!("menu.dpi_value", value = d).to_string(),
                    checked: m.dpi == Some(d),
                })
                .collect();
            out.push(Item::Submenu(t!("menu.dpi").to_string(), items));
        }
        out.push(Item::Separator);
    }
}

/// Appends `items` to a menu or submenu.
fn render(parent: &dyn Appender, items: &[Item]) -> Result<()> {
    for item in items {
        match item {
            Item::Label(text) => parent.add(&MenuItem::with_id(
                format!("label:{text}"),
                text,
                false,
                None,
            ))?,
            Item::Header {
                id,
                title,
                battery,
            } => match battery.map(battery_icon) {
                Some((rgba, w, h)) => {
                    let item = IconMenuItem::with_id(id.as_str(), title, false, None, None);
                    item.set_icon_templated(muda::Icon::from_rgba(rgba, w, h).ok());
                    parent.add(&item)?;
                }
                None => parent.add(&MenuItem::with_id(id.as_str(), title, false, None))?,
            },
            Item::Separator => parent.add(&PredefinedMenuItem::separator())?,
            Item::Submenu(title, children) => {
                let sub = Submenu::new(title, true);
                render(&sub, children)?;
                parent.add(&sub)?;
            }
            Item::Check { id, title, checked } => parent.add(&CheckMenuItem::with_id(
                id.as_str(),
                title,
                true,
                *checked,
                None,
            ))?,
            Item::Action { id, title } => {
                parent.add(&MenuItem::with_id(id.as_str(), title, true, None))?
            }
        }
    }
    Ok(())
}

/// `Menu` and `Submenu` share no trait for appending, so wrap both.
trait Appender {
    fn add(&self, item: &dyn IsMenuItem) -> Result<()>;
}

impl Appender for Menu {
    fn add(&self, item: &dyn IsMenuItem) -> Result<()> {
        self.append(item).map_err(|e| anyhow!(e))
    }
}

impl Appender for Submenu {
    fn add(&self, item: &dyn IsMenuItem) -> Result<()> {
        self.append(item).map_err(|e| anyhow!(e))
    }
}

/// macOS 27 hides menu item images unless `preferredImageVisibility` is
/// `.visible` (1); muda does not set it, so do it on the built NSMenu.
fn show_item_images(menu: &Menu, battery_labels: &[String]) {
    use muda::ContextMenu;
    use objc2_app_kit::NSAccessibility;
    const VISIBLE: isize = 1;
    let ptr = menu.ns_menu() as *const objc2_app_kit::NSMenu;
    // SAFETY: muda returns its live NSMenu, owned by `menu`, which outlives this call.
    let Some(ns_menu) = (unsafe { ptr.as_ref() }) else {
        return;
    };
    // The only items with an image are the mouse headers with a battery icon,
    // in the order of `battery_labels`.
    let mut labels = battery_labels.iter();
    for i in 0..ns_menu.numberOfItems() {
        let Some(item) = ns_menu.itemAtIndex(i) else {
            continue;
        };
        if item.image().is_none() {
            continue;
        }
        if let Some(label) = labels.next() {
            item.setAccessibilityLabel(Some(&NSString::from_str(label)));
        }
        if !item.respondsToSelector(objc2::sel!(setPreferredImageVisibility:)) {
            continue;
        }
        // SAFETY: the selector exists (checked) and takes an NSInteger enum.
        let _: () = unsafe { objc2::msg_send![&*item, setPreferredImageVisibility: VISIBLE] };
    }
}

/// Spoken description of the battery icon of every mouse that has one.
fn battery_labels(driver: &State) -> Vec<String> {
    let mice = driver.status().map_or(&[][..], |s| &s.mice[..]);
    mice.iter()
        .filter_map(|m| m.battery)
        .map(|b| match b.percent {
            Some(p) => t!("menu.battery_percent", value = p).to_string(),
            None => t!("menu.battery").to_string(),
        })
        .collect()
}

pub(super) fn build_menu(driver: &State, store: &Store) -> Result<Menu> {
    let menu = Menu::new();
    let (names, active) = (store.all(), store.active());
    let profiles = Profiles { names: &names, active: &active, modified: store.is_modified() };
    render(&menu, &model(driver, autostart::is_enabled(), &profiles))?;
    show_item_images(&menu, &battery_labels(driver));
    Ok(menu)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::BUILTIN;
    use crate::hidpp::features::battery::Charge;
    use crate::hidpp::features::dpi::DpiList;

    fn no_profiles() -> Profiles<'static> {
        Profiles { names: &[], active: BUILTIN, modified: false }
    }

    /// The daemon runs and reports these mice, wheel problem and missing permissions.
    fn running(mice: &[MouseInfo], scroll_problem: Option<&str>, missing: &[Permission]) -> State {
        State::Running(ipc::Status {
            mice: mice.to_vec(),
            missing: missing.to_vec(),
            scroll_problem: scroll_problem.map(str::to_string),
            profile: BUILTIN.into(),
            agent: None,
        })
    }

    fn action_ids(m: &[Item]) -> Vec<&str> {
        m.iter()
            .filter_map(|i| match i {
                Item::Action { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect()
    }

    fn mouse() -> MouseInfo {
        MouseInfo {
            key: "unit:aa".into(),
            name: "MX".into(),
            model: None,
            battery: Some(Battery {
                percent: Some(80),
                charge: Charge::Discharging,
            }),
            dpi: Some(1600),
            dpi_list: Some(DpiList {
                values: vec![],
                ranges: vec![(200, 8000, 50)],
            }),
            smartshift: None,
            details: None,
        }
    }

    #[test]
    fn empty_menu_has_fixed_entries() {
        let m = model(&running(&[], None, &[]), false, &no_profiles());
        assert_eq!(m[0], Item::Label(t!("menu.no_mouse").to_string()));
        assert_eq!(action_ids(&m), [ID_SETTINGS, ID_SETTINGS_FILE, ID_RELOAD, ID_RESTART_DOCK, ID_ABOUT, ID_QUIT]);
    }

    #[test]
    fn stopped_driver_offers_start() {
        let m = model(&State::Stopped, false, &no_profiles());
        assert_eq!(m[0], Item::Label(t!("menu.driver_not_running").to_string()));
        assert_eq!(action_ids(&m), [ID_START_DRIVER, ID_SETTINGS, ID_SETTINGS_FILE, ID_RESTART_DOCK, ID_ABOUT, ID_QUIT]);
        assert_eq!(parse_click(ID_START_DRIVER), Some(Click::StartDriver));
        // Starting: a notice only, no mice and nothing to reload.
        let m = model(&State::Starting, false, &no_profiles());
        assert_eq!(m[0], Item::Label(t!("menu.driver_starting").to_string()));
        assert_eq!(action_ids(&m), [ID_SETTINGS, ID_SETTINGS_FILE, ID_RESTART_DOCK, ID_ABOUT, ID_QUIT]);
    }

    #[test]
    fn scroll_problem_is_shown_first() {
        let m = model(&running(&[], Some("no permission"), &[]), false, &no_profiles());
        let Item::Label(text) = &m[0] else { panic!("no notice") };
        assert!(text.contains("no permission"));
        assert_eq!(m[1], Item::Separator);
        assert!(!matches!(&model(&running(&[], None, &[]), false, &no_profiles())[0], Item::Label(t) if t.contains("no permission")));
    }

    #[test]
    fn missing_permission_gets_notice_and_button() {
        let m = model(&running(&[], None, &[Permission::InputMonitoring]), false, &no_profiles());
        assert!(matches!(&m[0], Item::Label(_)));
        assert!(matches!(&m[1], Item::Action { id, .. } if id == "permission.input_monitoring"));
        assert_eq!(m[2], Item::Separator);
        assert_eq!(
            parse_click("permission.accessibility"),
            Some(Click::Permission(Permission::Accessibility))
        );
    }

    #[test]
    fn mouse_menu_marks_current_dpi() {
        let m = model(&running(&[mouse()], None, &[]), false, &no_profiles());
        let Some(Item::Submenu(_, dpi)) = m.iter().find(|i| matches!(i, Item::Submenu(..))) else {
            panic!("no dpi submenu");
        };
        assert_eq!(dpi.len(), DPI_PRESETS.len());
        assert!(dpi.contains(&Item::Check {
            id: "dpi:1600".into(),
            title: "1600 DPI".into(),
            checked: true
        }));
    }

    #[test]
    fn mouse_menu_starts_with_model_header() {
        let mut mi = mouse();
        mi.model = Some("MX Master 3S".into());
        let m = model(&running(&[mi.clone()], None, &[]), false, &no_profiles());
        assert_eq!(
            m[0],
            Item::Header {
                id: "header:unit:aa".into(),
                title: "MX Master 3S".into(),
                battery: mi.battery
            }
        );
        // Unknown model falls back to the name reported by the mouse.
        let m = model(&running(&[mouse()], None, &[]), false, &no_profiles());
        assert!(matches!(&m[0], Item::Header { title, .. } if title == "MX"));
    }

    #[test]
    fn profile_submenu_marks_active_profile() {
        let names = [BUILTIN.to_string(), "Mój".to_string()];
        let m = model(&running(&[], None, &[]), false, &Profiles { names: &names, active: "Mój", modified: false });
        let Some(Item::Submenu(title, items)) = m.iter().find(|i| matches!(i, Item::Submenu(..)))
        else {
            panic!("no profile submenu");
        };
        assert!(title.contains("Mój"), "{title}");
        assert!(!title.contains(&t!("profiles.modified", name = "Mój").to_string()), "{title}");
        assert_eq!(
            items[0],
            Item::Check {
                id: "profile:default".into(),
                title: profiles::display_name(BUILTIN),
                checked: false
            }
        );
        assert!(matches!(&items[1], Item::Check { id, checked: true, .. } if id == "profile:Mój"));
        assert!(matches!(items.last(), Some(Item::Action { id, .. }) if id == ID_MANAGE_PROFILES));
        assert_eq!(parse_click("profile:Mój"), Some(Click::Profile("Mój".into())));
        assert_eq!(parse_click("manage_profiles"), Some(Click::ManageProfiles));
    }

    #[test]
    fn modified_profile_is_marked_in_menu_title() {
        let names = [BUILTIN.to_string(), "Mój".to_string()];
        let m = model(&running(&[], None, &[]), false, &Profiles { names: &names, active: "Mój", modified: true });
        let Some(Item::Submenu(title, _)) = m.iter().find(|i| matches!(i, Item::Submenu(..))) else {
            panic!("no profile submenu");
        };
        assert!(title.contains(&t!("profiles.modified", name = "Mój").to_string()), "{title}");
    }

    #[test]
    fn unusual_current_dpi_is_listed() {
        let mut m = mouse();
        m.dpi = Some(1650);
        assert!(dpi_choices(&m).contains(&1650));
    }

    #[test]
    fn decodes_clicks() {
        assert_eq!(parse_click("dpi:800"), Some(Click::Dpi(800)));
        assert_eq!(parse_click("about"), Some(Click::About));
        assert_eq!(parse_click("quit"), Some(Click::Quit));
        assert_eq!(parse_click("settings_file"), Some(Click::SettingsFile));
        assert_eq!(parse_click("restart_dock"), Some(Click::RestartDock));
        assert_eq!(parse_click("label:x"), None);
        assert_eq!(parse_click("dpi:abc"), None);
    }
}
