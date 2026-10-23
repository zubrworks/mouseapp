//! `debug menu` and `debug menu-click`: the menu bar menu as AppKit has it
//! (titles, enabled, checked, key equivalents) with the item ids, and a
//! click on one item through AppKit, as if the user chose it.

use std::fmt::Write;

use anyhow::{Result, bail};
use muda::{ContextMenu, Menu, MenuItemKind};
use objc2::Message;
use objc2::rc::Retained;
use objc2_app_kit::{NSEventModifierFlags, NSMenu, NSMenuItem};
use rust_i18n::t;

/// One menu item.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Node {
    pub title: String,
    /// The app's id of the item (see `tray::menu`), when known.
    pub id: Option<String>,
    pub enabled: bool,
    /// `Some` for an item with a check mark (checked or not).
    pub checked: Option<bool>,
    pub separator: bool,
    /// Key equivalent, e.g. `cmd+q`; empty if none.
    pub key: String,
    pub children: Vec<Node>,
}

/// The AppKit menu behind a `muda` menu.
fn ns_menu(menu: &Menu) -> Option<&NSMenu> {
    let ptr = menu.ns_menu() as *const NSMenu;
    // SAFETY: muda returns its live NSMenu, owned by `menu`, which outlives
    // the returned reference.
    unsafe { ptr.as_ref() }
}

/// Reads `menu` as AppKit shows it.
pub fn read(menu: &Menu) -> Vec<Node> {
    ns_menu(menu).map_or_else(Vec::new, |ns| read_level(ns, &menu.items()))
}

/// The NSMenu items with the ids of the matching `muda` items (same order).
fn read_level(ns: &NSMenu, kinds: &[MenuItemKind]) -> Vec<Node> {
    let count = usize::try_from(ns.numberOfItems()).unwrap_or(0);
    let kinds = if kinds.len() == count { kinds } else { &[] };
    (0..count)
        .filter_map(|i| {
            let item = ns.itemAtIndex(i as isize)?;
            let kind = kinds.get(i);
            Some(read_item(&item, kind))
        })
        .collect()
}

fn read_item(item: &NSMenuItem, kind: Option<&MenuItemKind>) -> Node {
    if item.isSeparatorItem() {
        return Node { separator: true, ..Node::default() };
    }
    // SAFETY: `state` is an NSInteger property of every NSMenuItem.
    let state: isize = unsafe { objc2::msg_send![item, state] };
    let checked = match kind {
        Some(MenuItemKind::Check(_)) => Some(state != 0),
        _ => (state != 0).then_some(true),
    };
    let sub_kinds = match kind {
        Some(MenuItemKind::Submenu(s)) => s.items(),
        _ => Vec::new(),
    };
    let children = item.submenu().map_or_else(Vec::new, |sub| read_level(&sub, &sub_kinds));
    Node {
        title: item.title().to_string(),
        id: kind.map(|k| k.id().as_ref().to_string()),
        enabled: item.isEnabled(),
        checked,
        separator: false,
        key: key_text(&item.keyEquivalent().to_string(), item.keyEquivalentModifierMask()),
        children,
    }
}

/// `cmd+shift+q` style text of a key equivalent.
fn key_text(key: &str, mods: NSEventModifierFlags) -> String {
    if key.is_empty() {
        return String::new();
    }
    let mut parts = Vec::new();
    for (flag, name) in [
        (NSEventModifierFlags::Control, "ctrl"),
        (NSEventModifierFlags::Option, "alt"),
        (NSEventModifierFlags::Shift, "shift"),
        (NSEventModifierFlags::Command, "cmd"),
    ] {
        if mods.contains(flag) {
            parts.push(name.to_string());
        }
    }
    parts.push(key.to_lowercase());
    parts.join("+")
}

/// The tree as indented lines: `---` separators, `> ` submenus, `[x]` /
/// `[ ]` check marks, then `(disabled)`, `key=` and `id=`.
pub fn format(nodes: &[Node]) -> String {
    let mut out = String::new();
    write_level(&mut out, nodes, 0);
    out
}

fn write_level(out: &mut String, nodes: &[Node], depth: usize) {
    for n in nodes {
        let indent = "  ".repeat(depth);
        if n.separator {
            let _ = writeln!(out, "{indent}---");
            continue;
        }
        let mark = match (n.children.is_empty(), n.checked) {
            (false, _) => "> ",
            (true, Some(true)) => "[x] ",
            (true, Some(false)) => "[ ] ",
            (true, None) => "- ",
        };
        let _ = write!(out, "{indent}{mark}{}", n.title);
        if !n.enabled {
            let _ = write!(out, "  (disabled)");
        }
        if !n.key.is_empty() {
            let _ = write!(out, "  key={}", n.key);
        }
        if let Some(id) = &n.id {
            let _ = write!(out, "  id={id}");
        }
        out.push('\n');
        write_level(out, &n.children, depth + 1);
    }
}

/// Finds an item by its id (anywhere in the tree) or by its path of titles
/// joined with `>` (exact titles first, then ignoring case). Returns the
/// index at every level.
pub fn find(nodes: &[Node], path: &str) -> Option<Vec<usize>> {
    let path = path.trim();
    if let Some(found) = find_id(nodes, path) {
        return Some(found);
    }
    let titles: Vec<&str> = path.split('>').map(str::trim).collect();
    let mut level = nodes;
    let mut out = Vec::new();
    for title in titles {
        let usable = |n: &Node| !n.separator;
        let i = level
            .iter()
            .position(|n| usable(n) && n.title == title)
            .or_else(|| level.iter().position(|n| usable(n) && n.title.eq_ignore_ascii_case(title)))?;
        out.push(i);
        level = &level[i].children;
    }
    Some(out)
}

fn find_id(nodes: &[Node], id: &str) -> Option<Vec<usize>> {
    for (i, n) in nodes.iter().enumerate() {
        if n.id.as_deref() == Some(id) {
            return Some(vec![i]);
        }
        if let Some(mut rest) = find_id(&n.children, id) {
            rest.insert(0, i);
            return Some(rest);
        }
    }
    None
}

/// The node at `indices`.
fn node_at<'a>(nodes: &'a [Node], indices: &[usize]) -> Option<&'a Node> {
    let (&last, parents) = indices.split_last()?;
    let mut level = nodes;
    for &i in parents {
        level = &level.get(i)?.children;
    }
    level.get(last)
}

/// Chooses the item at `path` (see [`find`]) through AppKit, which sends
/// the app the same menu event as a click; the app handles it right after.
pub fn click(menu: &Menu, path: &str) -> Result<String> {
    let nodes = read(menu);
    let not_found = || anyhow::anyhow!(t!("debug.no_menu_item", path = path).to_string());
    let indices = find(&nodes, path).ok_or_else(not_found)?;
    let node = node_at(&nodes, &indices).ok_or_else(not_found)?;
    if !node.enabled {
        bail!(t!("debug.menu_item_disabled", path = path));
    }
    if !node.children.is_empty() {
        bail!(t!("debug.menu_item_submenu", path = path));
    }
    let Some(root) = ns_menu(menu) else {
        bail!(not_found());
    };
    let (&last, parents) = indices.split_last().ok_or_else(not_found)?;
    let mut level: Retained<NSMenu> = root.retain();
    for &i in parents {
        level = level.itemAtIndex(i as isize).and_then(|item| item.submenu()).ok_or_else(not_found)?;
    }
    level.performActionForItemAtIndex(last as isize);
    let id = node.id.as_deref().unwrap_or("?");
    Ok(format!("{}\n", t!("debug.menu_clicked", title = node.title, id = id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, id: &str) -> Node {
        Node { title: title.into(), id: Some(id.into()), enabled: true, ..Node::default() }
    }

    fn sample() -> Vec<Node> {
        let mut label = item("No mouse connected", "label:x");
        label.enabled = false;
        let mut default = item("Default", "profile:default");
        default.checked = Some(false);
        let mut mine = item("Mój", "profile:Mój");
        mine.checked = Some(true);
        let mut profiles = item("Profile: Mój", "7");
        profiles.children = vec![default, mine, Node { separator: true, ..Node::default() }];
        let mut quit = item("Quit", "quit");
        quit.key = "cmd+q".into();
        vec![label, Node { separator: true, ..Node::default() }, profiles, quit]
    }

    #[test]
    fn formats_the_tree() {
        assert_eq!(
            format(&sample()),
            "- No mouse connected  (disabled)  id=label:x\n\
             ---\n\
             > Profile: Mój  id=7\n  [ ] Default  id=profile:default\n  [x] Mój  id=profile:Mój\n  ---\n\
             - Quit  key=cmd+q  id=quit\n"
        );
    }

    #[test]
    fn finds_items_by_id_or_titles() {
        let nodes = sample();
        assert_eq!(find(&nodes, "quit"), Some(vec![3]));
        assert_eq!(find(&nodes, "profile:Mój"), Some(vec![2, 1]));
        assert_eq!(find(&nodes, "Profile: Mój > Default"), Some(vec![2, 0]));
        assert_eq!(find(&nodes, " profile: mój>default "), Some(vec![2, 0]));
        assert_eq!(find(&nodes, "Quit"), Some(vec![3]));
        assert_eq!(find(&nodes, "Profile: Mój > Nothing"), None);
        assert_eq!(find(&nodes, "Settings"), None);
        assert_eq!(node_at(&nodes, &[2, 1]).map(|n| n.title.as_str()), Some("Mój"));
        assert_eq!(node_at(&nodes, &[9]), None);
    }

    #[test]
    fn key_equivalents_read_like_shortcuts() {
        assert_eq!(key_text("", NSEventModifierFlags::Command), "");
        assert_eq!(key_text("q", NSEventModifierFlags::Command), "cmd+q");
        assert_eq!(
            key_text("Q", NSEventModifierFlags::Command | NSEventModifierFlags::Shift),
            "shift+cmd+q"
        );
    }
}
