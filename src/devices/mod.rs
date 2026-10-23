//! Device model registry.
//!
//! A model is a static [`Model`] description. Adding a model means one new
//! file in this directory plus one line in [`MODELS`].

pub mod mx_master_3s;

use rust_i18n::t;

#[cfg(test)]
use crate::hidpp::features::dpi::DpiList;

/// A physical button that can be reprogrammed (HID++ control ID).
#[derive(Debug, PartialEq, Eq)]
pub struct Button {
    /// Control ID (CID) used by the ReprogControls feature.
    pub cid: u16,
    /// Locale key suffix: the label is `t!("button.<key>")`.
    pub key: &'static str,
    /// Whether the user may assign an action to it. `false` for the main
    /// buttons (left, right): remapping them could lock the user out.
    pub remappable: bool,
}

impl Button {
    /// Localized label.
    pub fn label(&self) -> String {
        t!(format!("button.{}", self.key)).to_string()
    }
}

/// Supported DPI range of the sensor.
#[derive(Debug, PartialEq, Eq)]
pub struct DpiRange {
    pub min: u16,
    pub max: u16,
    pub step: u16,
}

impl DpiRange {
    /// The same range as a device-style DPI list.
    #[cfg(test)]
    pub fn to_list(&self) -> DpiList {
        DpiList {
            values: Vec::new(),
            ranges: vec![(self.min, self.max, self.step)],
        }
    }
}

/// SmartShift auto-disengage threshold range of a model.
#[derive(Debug, PartialEq, Eq)]
pub struct ThresholdRange {
    pub min: u8,
    pub max: u8,
    pub default: u8,
}

/// Static description of one mouse model.
#[derive(Debug)]
pub struct Model {
    /// Marketing name, also used to match the name reported by the device.
    pub name: &'static str,
    /// USB/Bluetooth product IDs of the mouse itself (not of receivers).
    pub pids: &'static [u16],
    /// Reprogrammable buttons.
    pub buttons: &'static [Button],
    /// Sensor DPI range.
    pub dpi: DpiRange,
    /// DPI the mouse starts with / the UI shows when nothing is configured.
    pub default_dpi: u16,
    /// SmartShift threshold range and default.
    pub smartshift: ThresholdRange,
    /// HID++ feature IDs the model is expected to support.
    pub features: &'static [u16],
}

impl Model {
    /// True if the model supports the HID++ feature `id`.
    pub fn has(&self, id: u16) -> bool {
        self.features.contains(&id)
    }
}

/// All known models. Add one line per new model file.
pub static MODELS: &[&Model] = &[&mx_master_3s::MODEL];

/// Keys of the buttons that can be remapped on any known model, sorted.
pub fn remappable_keys() -> Vec<&'static str> {
    let mut keys: Vec<&'static str> = MODELS
        .iter()
        .flat_map(|m| m.buttons.iter().filter(|b| b.remappable).map(|b| b.key))
        .collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// The model the UI describes when no mouse is connected.
pub fn default_model() -> &'static Model {
    MODELS[0]
}

/// Finds a model by the product ID of a directly connected mouse.
pub fn by_pid(pid: u16) -> Option<&'static Model> {
    MODELS.iter().copied().find(|m| m.pids.contains(&pid))
}

/// Finds a model by device name (case-insensitive, ignores surrounding spaces).
/// Needed for receivers, where the PID belongs to the receiver.
pub fn by_name(name: &str) -> Option<&'static Model> {
    let name = name.trim();
    MODELS
        .iter()
        .copied()
        .find(|m| m.name.eq_ignore_ascii_case(name))
}

/// Identifies a model from the PID first, then from the name.
pub fn identify(pid: u16, name: &str) -> Option<&'static Model> {
    by_pid(pid).or_else(|| by_name(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Direct child keys of top-level `section:` in YAML `text`. Blank lines and
    /// `#` comments are skipped; the section ends at the next unindented key.
    /// Nested keys are not returned, but their parent is.
    fn keys_in(text: &str, section: &str) -> Vec<String> {
        text.lines()
            .skip_while(|l| l.trim_end() != format!("{section}:"))
            .skip(1)
            .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
            .take_while(|l| l.starts_with(' '))
            .filter_map(|l| l.strip_prefix("  "))
            .filter(|r| !r.starts_with(' '))
            .filter_map(|r| r.split_once(':').map(|(k, _)| k.trim_matches('"').to_string()))
            .collect()
    }

    /// Direct child keys of `section:` in `locales/<lang>.yml` (`t!` cannot tell
    /// a missing key: it falls back to English).
    fn section_keys(lang: &str, section: &str) -> Vec<String> {
        let path = format!("{}/locales/{lang}.yml", env!("CARGO_MANIFEST_DIR"));
        keys_in(&std::fs::read_to_string(path).unwrap(), section)
    }

    #[test]
    fn keys_in_reads_direct_keys_only() {
        let text = "\
first:
  a: 1

# comment at column 0
  # indented comment
  \"on\": On
  charge:
    low: x
    high: y
  last: z
second:
  b: 2
tail:
  c: 3
  d: 4";
        assert_eq!(keys_in(text, "first"), ["a", "on", "charge", "last"]);
        assert_eq!(keys_in(text, "second"), ["b"]);
        assert_eq!(keys_in(text, "tail"), ["c", "d"]);
        assert!(keys_in(text, "missing").is_empty());
    }

    #[test]
    fn labels_exist_in_every_language() {
        let mut keys: Vec<(&str, String)> = ["ok", "cancel", "on", "off"]
            .iter()
            .map(|k| ("common", k.to_string()))
            .collect();
        for model in MODELS {
            keys.extend(model.buttons.iter().map(|b| ("button", b.key.to_string())));
        }
        let menu_keys = section_keys("en", "menu");
        assert!(!menu_keys.is_empty(), "no menu.* keys found in locales/en.yml");
        keys.extend(menu_keys.into_iter().map(|k| ("menu", k)));
        for lang in crate::i18n::available() {
            for (section, name) in &keys {
                assert!(section_keys(&lang, section).contains(name), "missing {section}.{name} in {lang}");
            }
        }
    }

    #[test]
    fn finds_by_pid_and_name() {
        assert_eq!(by_pid(0xB034).unwrap().name, "MX Master 3S");
        assert!(by_pid(0xC548).is_none());
        assert_eq!(by_name(" mx master 3s ").unwrap().name, "MX Master 3S");
        assert!(by_name("Unknown").is_none());
        assert!(identify(0xC548, "MX Master 3S").is_some());
    }

    #[test]
    fn registry_is_consistent() {
        for m in MODELS {
            assert!(m.dpi.min <= m.dpi.max && m.dpi.step > 0, "{}", m.name);
            assert!(m.to_list_ok(), "{}", m.name);
            assert!(m.dpi.to_list().contains(m.default_dpi), "{}", m.name);
            let t = &m.smartshift;
            assert!(t.min >= 1 && t.min <= t.default && t.default <= t.max, "{}", m.name);
            assert!(!m.buttons.is_empty(), "{}", m.name);
            for (i, b) in m.buttons.iter().enumerate() {
                assert!(!m.buttons[..i].iter().any(|o| o.key == b.key || o.cid == b.cid), "{}", m.name);
            }
            for key in ["left", "right"] {
                let b = m.buttons.iter().find(|b| b.key == key);
                assert!(b.is_none_or(|b| !b.remappable), "{} {key}", m.name);
            }
        }
    }

    #[test]
    fn main_buttons_are_not_remappable() {
        let keys = remappable_keys();
        assert!(!keys.contains(&"left") && !keys.contains(&"right"));
        assert!(!keys.contains(&"gesture"));
        assert!(keys.contains(&"back") && keys.contains(&"middle"));
    }

    #[test]
    fn default_model_is_first() {
        assert_eq!(default_model().name, MODELS[0].name);
        assert_eq!(default_model().default_dpi, 1000);
    }

    #[test]
    fn model_names_and_pids_are_unique() {
        for (i, a) in MODELS.iter().enumerate() {
            for b in &MODELS[i + 1..] {
                assert!(!a.name.eq_ignore_ascii_case(b.name));
                assert!(!a.pids.iter().any(|p| b.pids.contains(p)));
            }
        }
    }

    impl Model {
        fn to_list_ok(&self) -> bool {
            let l = self.dpi.to_list();
            l.contains(self.dpi.min) && l.contains(self.dpi.max)
        }
    }
}
