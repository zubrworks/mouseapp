//! Changing settings in TOML text while keeping comments and formatting.

use std::path::Path;

use toml_edit::{DocumentMut, Item, Table};

use super::model::Config;

/// Config text with `edit` applied to its settings; comments and untouched
/// keys stay as they are.
pub fn edit_text(text: &str, path: &Path, edit: impl FnOnce(&mut Config)) -> Result<String, String> {
    let old = Config::parse(text, path).map_err(|e| e.to_string())?;
    let mut new = old.clone();
    edit(&mut new);
    new.validate()?;
    let mut doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| e.to_string())?;
    let as_doc = |c: &Config| -> Result<DocumentMut, String> {
        toml::to_string(c)
            .map_err(|e| e.to_string())?
            .parse::<DocumentMut>()
            .map_err(|e| e.to_string())
    };
    sync_table(doc.as_table_mut(), as_doc(&old)?.as_table(), as_doc(&new)?.as_table());
    Ok(doc.to_string())
}

/// Makes `dst` (the real document) follow the change from `old` to `new`.
fn sync_table(dst: &mut Table, old: &Table, new: &Table) {
    let gone: Vec<String> = old
        .iter()
        .map(|(k, _)| k.to_string())
        .filter(|k| !new.contains_key(k))
        .collect();
    for key in gone {
        dst.remove(&key);
    }
    for (key, item) in new.iter() {
        let old_item = old.get(key);
        let both_tables = item.is_table() && old_item.is_some_and(Item::is_table);
        if both_tables
            && let (Some(dt), Some(ot), Some(nt)) = (
                dst.get_mut(key).and_then(Item::as_table_mut),
                old_item.and_then(Item::as_table),
                item.as_table(),
            )
        {
            sync_table(dt, ot, nt);
            continue;
        }
        if old_item.map(ToString::to_string) == Some(item.to_string()) && dst.contains_key(key) {
            continue;
        }
        let mut item = item.clone();
        match dst.get_mut(key) {
            Some(existing) => {
                // Keep the comment written next to the old value.
                if let (Item::Value(old_v), Item::Value(v)) = (&*existing, &mut item) {
                    *v.decor_mut() = old_v.decor().clone();
                }
                *existing = item;
            }
            None => {
                dst.insert(key, item);
            }
        }
    }
}
