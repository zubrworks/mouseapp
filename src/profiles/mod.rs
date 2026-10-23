//! Settings and profiles. The settings in use are `current.toml` (see
//! `current`); every change is written only there. Profiles are templates:
//! the built-in read-only "default" profile (factory behaviour, see
//! `builtin`) and user profiles stored as `profiles/<name>.toml`. Choosing a
//! profile copies it into `current.toml`. `config.toml` keeps only the UI
//! language and the name of the profile the current settings came from. An
//! old single-file `config.toml` is moved into a profile once, and a missing
//! `current.toml` is created from the active profile (see [`Store::prepare`]).

mod builtin;
mod current;
mod migrate;
mod names;
mod store;

pub use builtin::builtin_text;
#[cfg(test)]
pub use builtin::builtin;
pub use names::{BUILTIN, display_name, marked_name};
#[cfg(test)]
pub use names::validate_name;
pub use current::switch_to;
pub use store::{Stamp, Store};

#[cfg(test)]
mod tests {
    use std::path::Path;

    use toml_edit::DocumentMut;

    use super::migrate::migrated;
    use super::names::MAX_NAME;
    use super::store::remove_key_keeping_comments;
    use super::*;
    use crate::config::{self, Config};
    use crate::devices;

    /// A store in a fresh temporary folder, removed on drop.
    struct TempStore(Store);

    impl TempStore {
        fn new(name: &str) -> TempStore {
            let dir = std::env::temp_dir()
                .join(format!("mouse-profiles-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempStore(Store::new(dir))
        }
    }

    impl std::ops::Deref for TempStore {
        type Target = Store;
        fn deref(&self) -> &Store {
            &self.0
        }
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.dir());
        }
    }

    /// An old single-file config with many settings.
    const OLD: &str = "# my notes\n\n# pointer\ndpi = 1600\n\n# UI language\nlanguage = \"pl\"\n\n[smartshift]\nthreshold = 119\n\n[wheel]\nhires = false\n\n[thumbwheel]\ninvert = true\nspeed = 215\nmode = \"tabs\"\n\n[buttons]\n\n[scroll]\nspeed = 140\ndirection = \"standard\"\n\n[gestures]\nthreshold = 50\nclick = \"mission_control\"\n\n[apps.\"com.apple.Safari\".buttons]\nback = \"desktop_left\"\n";

    #[test]
    fn builtin_profile_is_factory_behaviour() {
        let c = builtin();
        c.validate().unwrap();
        assert_eq!(c.dpi, Some(devices::default_model().default_dpi));
        // SmartShift threshold is not written: the mouse keeps its own.
        assert_eq!(c.smartshift.threshold, None);
        // Back, forward, middle and top button stay native (not diverted).
        assert!(c.buttons.is_empty());
        assert!(c.apps.is_empty());
        let g = &c.gestures.thumb;
        assert_eq!(g.click.as_deref(), Some("mission_control"));
        assert_eq!(g.up.as_deref(), Some("mission_control"));
        assert_eq!(g.down.as_deref(), Some("app_expose"));
        assert_eq!(g.left.as_deref(), Some("desktop_left"));
        assert_eq!(g.right.as_deref(), Some("desktop_right"));
        assert_eq!(c.thumbwheel.mode.as_deref(), Some("scroll"));
        assert_eq!((c.thumbwheel.invert, c.thumbwheel.speed), (None, None));
        // Wheel scrolling is left to macOS.
        assert_eq!(c.scroll, config::Scroll::default());
        assert_eq!(c.wheel, config::Wheel::default());
        // Actions resolve, and its file text (for copies) has the same values.
        for m in devices::MODELS {
            crate::events::validate(m, &c).unwrap();
        }
        assert_eq!(Config::parse(&builtin_text(), Path::new("t.toml")).unwrap(), c);
        assert!(builtin_text().contains("# Gesture button"));
    }

    #[test]
    fn fresh_install_uses_builtin_profile() {
        let s = TempStore::new("fresh");
        let (name, cfg) = s.load_current().unwrap();
        assert_eq!(name, BUILTIN);
        assert_eq!(cfg, builtin());
        assert!(s.main_path().is_file());
        // The current settings start as the built-in profile, with comments.
        assert_eq!(std::fs::read_to_string(s.current_path()).unwrap(), builtin_text());
        assert_eq!(s.prepare().unwrap(), None);
        assert!(s.list().is_empty());
        assert!(!s.is_modified());
        assert_eq!(s.settings_file(), s.current_path());
    }

    #[test]
    fn old_config_moves_into_profile_with_backup() {
        let s = TempStore::new("migrate");
        std::fs::write(s.main_path(), OLD).unwrap();
        // An older backup is not overwritten.
        let old_backup = s.dir().join("config.toml.bak");
        std::fs::write(&old_backup, "older").unwrap();
        let old_cfg = {
            let mut doc: DocumentMut = OLD.parse().unwrap();
            doc.remove("language");
            Config::parse(&doc.to_string(), Path::new("t")).unwrap()
        };

        assert_eq!(s.prepare().unwrap().as_deref(), Some(migrated().as_str()));
        assert_eq!(std::fs::read_to_string(&old_backup).unwrap(), "older");
        let backup = s.dir().join("config.toml.bak.2");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), OLD);
        // config.toml: language and profile only.
        let main: toml::Table = std::fs::read_to_string(s.main_path()).unwrap().parse().unwrap();
        let keys: Vec<&str> = main.keys().map(String::as_str).collect();
        assert_eq!(keys, ["language", "profile"]);
        assert_eq!(s.language().as_deref(), Some("pl"));
        assert_eq!(s.active(), migrated());
        // The profile has every setting and keeps the comments.
        let text = std::fs::read_to_string(s.profile_path(&migrated())).unwrap();
        assert!(text.contains("# my notes") && text.contains("# pointer"), "{text}");
        assert!(!text.contains("language") && !text.contains("UI language"), "{text}");
        let (name, cfg) = s.load_current().unwrap();
        assert_eq!((name.as_str(), &cfg), (migrated().as_str(), &old_cfg));
        // The current settings are a copy of the new profile.
        assert_eq!(std::fs::read_to_string(s.current_path()).unwrap(), text);
        assert!(!s.is_modified());
        assert_eq!(cfg.thumbwheel.speed, Some(215));
        assert_eq!(cfg.apps["com.apple.Safari"].buttons["back"], "desktop_left");
        // Only once.
        assert_eq!(s.prepare().unwrap(), None);
        assert_eq!(s.list(), [migrated()]);
    }

    #[test]
    fn migration_without_language_and_name_clash() {
        let s = TempStore::new("clash");
        std::fs::write(s.main_path(), "dpi = 800\n").unwrap();
        std::fs::create_dir_all(s.profiles_dir()).unwrap();
        std::fs::write(s.profile_path(&migrated()), "dpi = 400\n").unwrap();
        let name = s.prepare().unwrap().unwrap();
        assert_eq!(name, format!("{} 2", migrated()));
        assert_eq!(s.language(), None);
        assert_eq!(s.load_current().unwrap().1.dpi, Some(800));
        assert_eq!(s.load(&migrated()).unwrap().dpi, Some(400));
        assert!(s.dir().join("config.toml.bak").is_file());
    }

    #[test]
    fn invalid_old_config_is_left_alone() {
        let s = TempStore::new("invalid");
        std::fs::write(s.main_path(), "dpi = 5\n").unwrap();
        assert!(s.prepare().is_err());
        assert_eq!(std::fs::read_to_string(s.main_path()).unwrap(), "dpi = 5\n");
        assert!(!s.dir().join("config.toml.bak").exists());
        assert!(s.list().is_empty());
        assert!(!s.current_path().exists());
    }

    #[test]
    fn create_switch_rename_delete() {
        let s = TempStore::new("manage");
        s.prepare().unwrap();
        // A new profile gets the current settings.
        s.update_current(|c| c.dpi = Some(2400)).unwrap();
        let name = s.save_as_new("  Work ").unwrap();
        assert_eq!(name, "Work");
        assert_eq!(s.active(), "Work");
        assert_eq!(s.load("Work").unwrap().dpi, Some(2400));
        assert!(!s.is_modified());
        assert!(s.save_as_new("work").is_err(), "case-insensitive clash");
        assert!(s.save_as_new(BUILTIN).is_err());
        assert_eq!(s.unique_name("Work"), "Work 2");
        assert_eq!(s.settings_file(), s.current_path());
        s.save_as_new("Home").unwrap();
        assert_eq!(s.load("Home").unwrap().dpi, Some(2400));
        assert_eq!(s.list(), ["Home", "Work"]);
        assert_eq!(s.all(), [BUILTIN, "Home", "Work"]);
        assert!(switch_to(&s, "Work").unwrap());
        // Renaming the active profile follows in config.toml.
        assert_eq!(s.rename("Work", "Office").unwrap(), "Office");
        assert_eq!(s.active(), "Office");
        assert!(!s.is_modified());
        assert!(s.rename("Office", "home").is_err());
        assert_eq!(s.rename("Office", "office").unwrap(), "office");
        assert!(s.rename(BUILTIN, "x").is_err());
        // Deleting the active profile chooses the built-in one.
        s.delete("office").unwrap();
        assert_eq!(s.active(), BUILTIN);
        assert_eq!(s.load_current().unwrap().1, builtin());
        assert!(s.delete(BUILTIN).is_err());
        assert!(s.select("nope").is_err());
        assert_eq!(s.list(), ["Home"]);
    }

    #[test]
    fn change_on_builtin_profile_goes_to_current_settings() {
        let s = TempStore::new("on-builtin");
        // No copy is needed: the change is written to current.toml only.
        s.update_current(|c| c.dpi = Some(2400)).unwrap();
        assert_eq!(s.active(), BUILTIN);
        assert!(s.list().is_empty());
        assert_eq!(s.load_current().unwrap(), (BUILTIN.to_string(), Config { dpi: Some(2400), ..builtin() }));
        assert!(s.is_modified());
        // The comments of the file stay.
        let text = std::fs::read_to_string(s.current_path()).unwrap();
        assert!(text.contains("# Gesture button") && text.contains("dpi = 2400"), "{text}");
        // The built-in profile is read-only.
        assert!(s.save_to_profile().is_err());
        assert_eq!(s.load(BUILTIN).unwrap(), builtin());
    }

    #[test]
    fn restart_keeps_current_settings() {
        let s = TempStore::new("restart");
        s.update_current(|c| c.dpi = Some(1200)).unwrap();
        // A new start (a new store on the same folder) reads the same file.
        let again = Store::new(s.dir());
        assert_eq!(again.prepare().unwrap(), None);
        let (name, cfg) = again.load_current().unwrap();
        assert_eq!((name.as_str(), cfg.dpi), (BUILTIN, Some(1200)));
        assert!(again.is_modified());
    }

    #[test]
    fn choosing_a_profile_overwrites_current_settings() {
        let s = TempStore::new("choose");
        std::fs::create_dir_all(s.profiles_dir()).unwrap();
        std::fs::write(s.profile_path("Work"), "# work\ndpi = 3200\n").unwrap();
        s.update_current(|c| c.dpi = Some(800)).unwrap();
        assert!(switch_to(&s, "Work").unwrap());
        assert_eq!(s.active(), "Work");
        assert_eq!(std::fs::read_to_string(s.current_path()).unwrap(), "# work\ndpi = 3200\n");
        assert!(!s.is_modified());
        // Choosing the active profile again keeps the changes.
        s.update_current(|c| c.dpi = Some(1000)).unwrap();
        assert!(!switch_to(&s, "Work").unwrap());
        assert_eq!(s.load_current().unwrap().1.dpi, Some(1000));
        // Back to the built-in profile: its settings, changes dropped.
        assert!(switch_to(&s, BUILTIN).unwrap());
        assert_eq!(s.load_current().unwrap().1, builtin());
        // An invalid profile changes nothing.
        std::fs::write(s.profile_path("Bad"), "dpi = 5\n").unwrap();
        s.update_current(|c| c.dpi = Some(1000)).unwrap();
        assert!(switch_to(&s, "Bad").is_err());
        assert_eq!(s.active(), BUILTIN);
        assert_eq!(s.load_current().unwrap().1.dpi, Some(1000));
        assert!(switch_to(&s, "Gone").is_err());
    }

    #[test]
    fn unsaved_changes_are_detected_saved_and_reverted() {
        let s = TempStore::new("modified");
        std::fs::create_dir_all(s.profiles_dir()).unwrap();
        std::fs::write(s.profile_path("Work"), "# notes\ndpi = 1600\n").unwrap();
        s.select("Work").unwrap();
        assert!(!s.is_modified());
        s.update_current(|c| c.dpi = Some(2000)).unwrap();
        assert!(s.is_modified());
        // The same value again: no difference.
        s.update_current(|c| c.dpi = Some(1600)).unwrap();
        assert!(!s.is_modified());
        // Saving into the profile keeps its comments.
        s.update_current(|c| c.dpi = Some(2000)).unwrap();
        s.save_to_profile().unwrap();
        assert!(!s.is_modified());
        let text = std::fs::read_to_string(s.profile_path("Work")).unwrap();
        assert!(text.starts_with("# notes\ndpi = 2000\n"), "{text}");
        // Reverting drops the changes.
        s.update_current(|c| c.dpi = Some(400)).unwrap();
        s.revert().unwrap();
        assert!(!s.is_modified());
        assert_eq!(s.load_current().unwrap().1.dpi, Some(2000));
        // A profile file edited by hand is a difference too.
        std::fs::write(s.profile_path("Work"), "dpi = 800\n").unwrap();
        assert!(s.is_modified());
    }

    #[test]
    fn first_start_copies_active_profile_into_current_settings() {
        let s = TempStore::new("first");
        std::fs::create_dir_all(s.profiles_dir()).unwrap();
        let work = "# mine\ndpi = 2400\n\n[scroll]\nspeed = 140\n";
        std::fs::write(s.profile_path("Work"), work).unwrap();
        std::fs::write(s.main_path(), "language = \"pl\"\nprofile = \"Work\"\n").unwrap();
        assert_eq!(s.prepare().unwrap(), None);
        assert_eq!(std::fs::read_to_string(s.current_path()).unwrap(), work);
        let (name, cfg) = s.load_current().unwrap();
        assert_eq!((name.as_str(), cfg.dpi, cfg.scroll.speed), ("Work", Some(2400), Some(140)));
        assert!(!s.is_modified());
        // Only once: later changes of the profile do not touch it.
        std::fs::write(s.profile_path("Work"), "dpi = 800\n").unwrap();
        s.prepare().unwrap();
        assert_eq!(std::fs::read_to_string(s.current_path()).unwrap(), work);
        assert_eq!(s.language().as_deref(), Some("pl"));
    }

    #[test]
    fn failed_copy_leaves_profile_choice_unchanged() {
        let s = TempStore::new("failed-copy");
        std::fs::create_dir_all(s.profiles_dir()).unwrap();
        std::fs::write(s.profile_path("Work"), "dpi = 2400\n").unwrap();
        std::fs::write(s.profile_path("Home"), "dpi = 800\n").unwrap();
        s.select("Work").unwrap();
        // current.toml cannot be replaced: a non-empty folder in its place.
        std::fs::remove_file(s.current_path()).unwrap();
        std::fs::create_dir_all(s.current_path().join("x")).unwrap();
        assert!(s.select("Home").is_err());
        assert_eq!(s.active(), "Work");
        let main = std::fs::read_to_string(s.main_path()).unwrap();
        assert!(main.contains("profile = \"Work\""), "{main}");
    }

    #[test]
    fn broken_current_settings_are_kept_and_recovered() {
        let s = TempStore::new("broken");
        std::fs::create_dir_all(s.profiles_dir()).unwrap();
        std::fs::write(s.profile_path("Work"), "dpi = 2400\n").unwrap();
        s.select("Work").unwrap();
        std::fs::write(s.current_path(), "dpi = = 1\n").unwrap();
        assert!(s.load_current().is_err());
        assert!(s.is_modified());
        // A change does not overwrite the broken file, nor does a start.
        assert!(s.update_current(|c| c.dpi = Some(800)).is_err());
        assert!(s.save_to_profile().is_err());
        s.prepare().unwrap();
        assert_eq!(std::fs::read_to_string(s.current_path()).unwrap(), "dpi = = 1\n");
        // Recovery keeps a copy and takes the active profile.
        let backup = s.recover_current().unwrap();
        assert_eq!(backup, s.dir().join("current.toml.bak"));
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "dpi = = 1\n");
        assert_eq!(s.load_current().unwrap(), ("Work".to_string(), s.load("Work").unwrap()));
        // An invalid profile: nothing is changed.
        std::fs::write(s.current_path(), "dpi = = 2\n").unwrap();
        std::fs::write(s.profile_path("Work"), "dpi = 5\n").unwrap();
        assert!(s.recover_current().is_err());
        assert_eq!(std::fs::read_to_string(s.current_path()).unwrap(), "dpi = = 2\n");
        assert!(!s.dir().join("current.toml.bak.2").exists());
    }

    #[test]
    fn missing_profile_file_means_builtin() {
        let s = TempStore::new("missing");
        std::fs::write(s.main_path(), "profile = \"Gone\"\n").unwrap();
        assert_eq!(s.active(), BUILTIN);
        assert_eq!(s.load_current().unwrap().1, builtin());
    }

    #[test]
    fn language_is_set_and_removed_keeping_profile() {
        let s = TempStore::new("lang");
        s.set_language(Some("pl")).unwrap();
        assert_eq!(s.language().as_deref(), Some("pl"));
        s.set_language(None).unwrap();
        assert_eq!(s.language(), None);
        assert_eq!(s.active(), BUILTIN);
        let text = std::fs::read_to_string(s.main_path()).unwrap();
        assert!(text.contains("# Mouse:"), "{text}");
    }

    #[test]
    fn main_key_change_keeps_comments_and_order() {
        let s = TempStore::new("keys");
        std::fs::write(s.main_path(), "# top\n\nlanguage = \"en\" # ui\nprofile = \"Work\"\n").unwrap();
        s.set_main_key("language", Some("pl")).unwrap();
        let text = std::fs::read_to_string(s.main_path()).unwrap();
        assert_eq!(text, "# top\n\nlanguage = \"pl\" # ui\nprofile = \"Work\"\n");
        s.set_main_key("language", None).unwrap();
        let text = std::fs::read_to_string(s.main_path()).unwrap();
        assert_eq!(text, "# top\n\nprofile = \"Work\"\n");
    }

    #[test]
    fn removed_key_leaves_its_header_comment() {
        let mut doc: DocumentMut = "# top\nlanguage = \"en\"\n[a]\nx = 1\n".parse().unwrap();
        remove_key_keeping_comments(&mut doc, "language");
        assert_eq!(doc.to_string(), "# top\n[a]\nx = 1\n");
    }

    #[test]
    fn names_are_checked() {
        assert_eq!(validate_name(" Mój ").unwrap(), "Mój");
        for bad in ["", "  ", "a/b", "a:b", ".hidden", "default", "Default", "DOMYŚLNY"] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
        assert!(validate_name(&"x".repeat(MAX_NAME + 1)).is_err());
        assert_eq!(display_name("Mój"), "Mój");
        assert_ne!(display_name(BUILTIN), BUILTIN);
    }

    #[test]
    fn watched_files() {
        let s = Store::new("/x/Mouse");
        assert!(s.is_settings_file(Path::new("/x/Mouse/config.toml")));
        assert!(s.is_settings_file(Path::new("/x/Mouse/current.toml")));
        // Profiles are templates: changing one does not change the settings.
        assert!(!s.is_settings_file(Path::new("/x/Mouse/profiles/Mój.toml")));
        assert!(!s.is_settings_file(Path::new("/x/Mouse/profiles/current.toml")));
        assert!(!s.is_settings_file(Path::new("/x/Mouse/other.txt")));
        assert!(!s.is_settings_file(Path::new("/x/Mouse/current.toml.bak")));
        // Temporary files of an atomic write must not trigger a reload.
        assert!(!s.is_settings_file(Path::new("/x/Mouse/.config.toml.1-0.tmp")));
        assert!(!s.is_settings_file(Path::new("/x/Mouse/.current.toml.1-0.tmp")));
    }
}
