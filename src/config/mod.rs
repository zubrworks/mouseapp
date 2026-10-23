//! Reads and writes one settings profile (TOML). Where profiles are stored
//! and which one is active is handled by `profiles`.

mod edit;
mod io;
mod limits;
mod model;
mod validate;

pub use edit::edit_text;
pub use io::EXAMPLE;
pub use limits::*;
pub use model::*;

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    const COMMENTED: &str = "# my notes\n\n# pointer speed\ndpi = 1600 # keep me\n\n[smartshift]\n# ratchet\nthreshold = 83\n\n# Safari\n[apps.\"com.apple.Safari\".buttons]\nback = \"desktop_left\"\n";

    fn p() -> PathBuf {
        PathBuf::from("test.toml")
    }

    #[test]
    fn example_parses() {
        let c = Config::parse(EXAMPLE, &p()).unwrap();
        assert_eq!(c.dpi, Some(1600));
        assert_eq!(c.buttons["back"], "back");
        assert_eq!(c.gestures.thumb.left.as_deref(), Some("desktop_left"));
        assert_eq!(c.thumbwheel.mode.as_deref(), Some("scroll"));
        for m in ThumbMode::ALL.map(ThumbMode::name) {
            let text = format!("[thumbwheel]\nmode = \"{m}\"");
            assert!(Config::parse(&text, &p()).is_ok(), "{m}");
        }
    }

    #[test]
    fn empty_is_default() {
        assert_eq!(Config::parse("", &p()).unwrap(), Config::default());
    }

    #[test]
    fn rejects_bad_values() {
        for bad in [
            "dpi = 100000",
            "dpi = 10",
            "[smartshift]\nthreshold = 0",
            "[buttons]\nnope = \"x\"",
            "[buttons]\nback = \"\"",
            "[gestures]\nthreshold = 0",
            "[gestures]\nfoo = 1",
            "[gestures.middle]\nthreshold = 0",
            "[gestures.middle]\nup = \"\"",
            "[gestures.middle]\nsideways = \"mute\"",
            "[gestures.nope]\nup = \"mute\"",
            "[gestures.gesture]\nup = \"mute\"",
            "[apps.\"x\".gestures.middle]\nthreshold = 0",
            "[thumbwheel]\nspeed = 0",
            "[thumbwheel]\nmode = \"pan\"",
            "[scroll]\ndirection = \"up\"",
            "[scroll]\nspeed = 5",
            "[scroll]\nsmoothing = 95",
            "unknown_key = 1",
            // The language lives in config.toml, not in a profile.
            "language = \"en\"",
        ] {
            assert!(Config::parse(bad, &p()).is_err(), "{bad}");
        }
    }

    #[test]
    fn parses_app_profiles() {
        let c = Config::parse(
            "[buttons]\nback = \"back\"\n[apps.\"com.apple.Safari\".buttons]\nback = \"desktop_left\"\n[apps.\"com.apple.Safari\".gestures]\nup = \"app_expose\"\n",
            &p(),
        )
        .unwrap();
        let s = &c.apps["com.apple.Safari"];
        assert_eq!(s.buttons["back"], "desktop_left");
        assert_eq!(s.gestures.thumb.up.as_deref(), Some("app_expose"));
        assert_eq!(s.gestures.thumb.down, None);
        assert!(Config::parse("[apps.\"x\".buttons]\nnope = \"a\"", &p()).is_err());
        assert!(Config::parse("[apps.\"x\".gestures]\nup = \"\"", &p()).is_err());
        assert!(Config::parse("[apps.\"x\"]\nwheel = 1", &p()).is_err());
    }

    #[test]
    fn gesture_button_is_set_in_gestures_only() {
        assert!(Config::parse("[buttons]\ngesture = \"back\"", &p()).is_err());
        assert!(Config::parse("[apps.\"x\".buttons]\ngesture = \"back\"", &p()).is_err());
        assert!(Config::parse("[gestures.gesture]\nup = \"copy\"", &p()).is_err());
        assert!(Config::parse("[gestures]\nup = \"copy\"", &p()).is_ok());
    }

    #[test]
    fn main_buttons_cannot_be_remapped() {
        assert!(Config::parse("[buttons]\nleft = \"back\"", &p()).is_err());
        assert!(Config::parse("[buttons]\nright = \"back\"", &p()).is_err());
        assert!(Config::parse("[gestures.left]\nup = \"copy\"", &p()).is_err());
        assert!(Config::parse("[apps.\"x\".buttons]\nleft = \"back\"", &p()).is_err());
    }

    #[test]
    fn parses_gesture_sets_per_button() {
        let text = "[gestures]\nthreshold = 40\nup = \"mute\"\n[gestures.middle]\nclick = \"middle_click\"\nthreshold = 80\n[gestures.back]\nleft = \"back\"\n[apps.\"x\".gestures.middle]\nup = \"copy\"\n";
        let c = Config::parse(text, &p()).unwrap();
        let g = &c.gestures;
        assert_eq!((g.thumb.threshold, g.thumb.up.as_deref()), (Some(40), Some("mute")));
        assert_eq!(g.buttons.len(), 2);
        let m = g.for_button("middle");
        assert_eq!((m.threshold, m.click.as_deref(), m.up), (Some(80), Some("middle_click"), None));
        assert_eq!(g.for_button("back").left.as_deref(), Some("back"));
        assert_eq!(g.for_button("gesture"), g.thumb);
        // No section: the standard set.
        assert_eq!(g.for_button("forward"), GestureSet::standard());
        assert_eq!(c.apps["x"].gestures.buttons["middle"].up.as_deref(), Some("copy"));
    }

    #[test]
    fn edit_writes_gesture_set_per_button() {
        let p = Path::new("t.toml");
        let text = "# notes\n[gestures]\nup = \"mute\"\n";
        let out = edit_text(text, p, |c| {
            c.gestures.for_button_mut("middle").up = Some("copy".into());
            c.gestures.for_button_mut("gesture").down = Some("paste".into());
        })
        .unwrap();
        assert!(out.contains("# notes") && out.contains("[gestures.middle]"), "{out}");
        let c = Config::parse(&out, p).unwrap();
        assert_eq!(c.gestures.thumb.up.as_deref(), Some("mute"));
        assert_eq!(c.gestures.thumb.down.as_deref(), Some("paste"));
        // A new section starts from the standard set.
        let mut want = GestureSet::standard();
        want.up = Some("copy".into());
        assert_eq!(c.gestures.buttons["middle"], want);
        // Removing the section again.
        let out = edit_text(&out, p, |c| {
            c.gestures.buttons.remove("middle");
        })
        .unwrap();
        assert!(!out.contains("[gestures.middle]"), "{out}");
        assert!(Config::parse(&out, p).unwrap().gestures.buttons.is_empty());
    }

    #[test]
    fn syntax_error_mentions_path() {
        let e = Config::parse("dpi = = 1", &p()).unwrap_err().to_string();
        assert!(e.contains("test.toml"), "{e}");
    }

    #[test]
    fn edit_keeps_comments_and_changes_one_key() {
        let p = Path::new("t.toml");
        let out = edit_text(COMMENTED, p, |c| c.dpi = Some(2000)).unwrap();
        assert!(out.contains("# my notes") && out.contains("# pointer speed"));
        assert!(out.contains("dpi = 2000 # keep me"), "{out}");
        assert!(out.contains("# ratchet") && out.contains("# Safari"));
        assert_eq!(Config::parse(&out, p).unwrap().dpi, Some(2000));
    }

    #[test]
    fn edit_adds_and_removes_keys_and_apps() {
        let p = Path::new("t.toml");
        let out = edit_text(COMMENTED, p, |c| {
            c.scroll.speed = Some(150);
            c.smartshift.threshold = None;
            c.apps.remove("com.apple.Safari");
            c.apps.entry("com.apple.finder".into()).or_default();
        })
        .unwrap();
        let c = Config::parse(&out, p).unwrap();
        assert_eq!(c.scroll.speed, Some(150));
        assert_eq!(c.smartshift.threshold, None);
        assert!(!c.apps.contains_key("com.apple.Safari"));
        assert!(c.apps.contains_key("com.apple.finder"));
        assert!(out.contains("# my notes"));
        assert!(edit_text(COMMENTED, p, |c| c.dpi = Some(5)).is_err());
    }
}
