//! Scrolling tab: mouse wheel and thumb wheel.

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::NSView;
use rust_i18n::t;

use super::{
    Context, Effect, as_view, form, is_on, popup, popup_index, read_config, slider, slider_value,
    switch, update_config,
};
use crate::config::{
    Config, DEFAULT_SPEED, SCROLL_SPEED_RANGE, THUMB_SPEED_RANGE, ThumbMode, slider_bounds,
};
use crate::hidpp::features::{thumbwheel, wheel};

const ID_DIRECTION: &str = "scrolling.direction";
const ID_SPEED: &str = "scrolling.speed";
const ID_SMOOTHING: &str = "scrolling.smoothing";
const ID_HIRES: &str = "scrolling.wheel_hires";
const ID_THUMB_MODE: &str = "scrolling.thumb_mode";
const ID_THUMB_INVERT: &str = "scrolling.thumb_invert";
const ID_THUMB_SPEED: &str = "scrolling.thumb_speed";

/// Index of the configured thumb wheel mode in [`ThumbMode::ALL`] (0 = scroll).
fn mode_index(mode: Option<&str>) -> usize {
    mode.and_then(ThumbMode::from_name)
        .and_then(|m| ThumbMode::ALL.iter().position(|x| *x == m))
        .unwrap_or(0)
}

fn percent(v: impl std::fmt::Display) -> String {
    t!("settings.percent", value = v.to_string()).to_string()
}

pub fn build(ctx: &Context, target: &AnyObject, mtm: MainThreadMarker) -> Retained<NSView> {
    let cfg = read_config(ctx);
    // Not set yet: show what the system does now.
    let natural = match cfg.scroll.direction.as_deref() {
        Some(d) => d == "natural",
        None => crate::scroll::system_natural(),
    };
    let direction = switch(natural, ID_DIRECTION, target, mtm);
    let speed = cfg.scroll.speed.unwrap_or(DEFAULT_SPEED);
    let smoothing = cfg.scroll.smoothing.unwrap_or(0);
    let thumb_speed = cfg.thumbwheel.speed.unwrap_or(DEFAULT_SPEED);
    let model = ctx.mouse.borrow().model;
    let mut rows = vec![
        (
            t!("settings.scrolling.direction").to_string(),
            as_view(&direction),
        ),
        (
            t!("settings.scrolling.speed").to_string(),
            slider(
                slider_bounds(&SCROLL_SPEED_RANGE),
                f64::from(speed),
                &percent(speed),
                ID_SPEED,
                target,
                mtm,
            ),
        ),
        (
            t!("settings.scrolling.smoothing").to_string(),
            slider(
                (0.0, 90.0),
                f64::from(smoothing),
                &percent(smoothing),
                ID_SMOOTHING,
                target,
                mtm,
            ),
        ),
    ];
    if model.has(wheel::ID) {
        rows.push((
            t!("settings.scrolling.wheel_hires").to_string(),
            as_view(&switch(
                cfg.wheel.hires.unwrap_or(true),
                ID_HIRES,
                target,
                mtm,
            )),
        ));
    }
    // Models without a thumb wheel get no rows for it.
    if model.has(thumbwheel::ID) {
        let titles: Vec<String> = ThumbMode::ALL
            .iter()
            .map(|m| {
                let key = format!("settings.scrolling.thumb_mode_{}", m.name());
                t!(key.as_str()).to_string()
            })
            .collect();
        rows.push((
            t!("settings.scrolling.thumb_mode").to_string(),
            as_view(&popup(
                &titles,
                mode_index(cfg.thumbwheel.mode.as_deref()),
                ID_THUMB_MODE,
                target,
                mtm,
            )),
        ));
        rows.push((
            t!("settings.scrolling.thumb_invert").to_string(),
            as_view(&switch(
                cfg.thumbwheel.invert.unwrap_or(false),
                ID_THUMB_INVERT,
                target,
                mtm,
            )),
        ));
        rows.push((
            t!("settings.scrolling.thumb_speed").to_string(),
            slider(
                slider_bounds(&THUMB_SPEED_RANGE),
                f64::from(thumb_speed),
                &percent(thumb_speed),
                ID_THUMB_SPEED,
                target,
                mtm,
            ),
        ));
    }
    form(rows, mtm)
}

/// Stores the new value of a slider in the config.
fn set_slider(c: &mut Config, id: &str, v: f64) {
    match id {
        ID_SPEED => {
            let (min, max) = slider_bounds(&SCROLL_SPEED_RANGE);
            c.scroll.speed = Some(v.clamp(min, max) as u16);
        }
        ID_SMOOTHING => c.scroll.smoothing = Some(v.clamp(0.0, 90.0) as u8),
        ID_THUMB_SPEED => {
            let (min, max) = slider_bounds(&THUMB_SPEED_RANGE);
            c.thumbwheel.speed = Some(v.clamp(min, max) as u16);
        }
        _ => {}
    }
}

/// Stores the new state of a switch in the config.
fn set_switch(c: &mut Config, id: &str, on: bool) {
    match id {
        ID_HIRES => c.wheel.hires = Some(on),
        ID_THUMB_INVERT => c.thumbwheel.invert = Some(on),
        _ => {}
    }
}

pub fn changed(id: &str, sender: &AnyObject, ctx: &Context, mtm: MainThreadMarker) -> Vec<Effect> {
    match id {
        ID_DIRECTION => {
            let Some(on) = is_on(sender) else {
                return Vec::new();
            };
            log::info!("settings: {id} = {on}");
            let d = if on { "natural" } else { "standard" };
            update_config(ctx, mtm, |c| c.scroll.direction = Some(d.to_string()))
        }
        ID_SPEED | ID_SMOOTHING | ID_THUMB_SPEED => {
            let Some(v) = slider_value(sender, 5.0) else {
                return Vec::new();
            };
            log::info!("settings: {id} = {v}");
            update_config(ctx, mtm, |c| set_slider(c, id, v))
        }
        ID_THUMB_MODE => {
            let Some(mode) = popup_index(sender).and_then(|i| ThumbMode::ALL.get(i)) else {
                return Vec::new();
            };
            log::info!("settings: {id} = {}", mode.name());
            update_config(ctx, mtm, |c| c.thumbwheel.mode = Some(mode.name().to_string()))
        }
        ID_HIRES | ID_THUMB_INVERT => {
            let Some(on) = is_on(sender) else {
                return Vec::new();
            };
            log::info!("settings: {id} = {on}");
            update_config(ctx, mtm, |c| set_switch(c, id, on))
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config file in its own temp directory, removed when the guard is dropped.
    struct TempConfig(std::path::PathBuf);

    impl std::ops::Deref for TempConfig {
        type Target = std::path::PathBuf;
        fn deref(&self) -> &std::path::PathBuf {
            &self.0
        }
    }

    impl Drop for TempConfig {
        fn drop(&mut self) {
            if let Some(dir) = self.0.parent() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    fn temp_config(name: &str, text: &str) -> TempConfig {
        let dir = std::env::temp_dir().join(format!("mouse-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, text).unwrap();
        TempConfig(path)
    }

    /// The hi-res switch must keep its value when other settings change.
    #[test]
    fn hires_survives_other_changes() {
        let path = temp_config("hires", crate::config::EXAMPLE);
        Config::update_file(&path, |c| set_switch(c, ID_HIRES, false)).unwrap();
        for (id, v) in [
            (ID_SPEED, 300.0),
            (ID_SMOOTHING, 40.0),
            (ID_THUMB_SPEED, 150.0),
        ] {
            Config::update_file(&path, |c| set_slider(c, id, v)).unwrap();
            let c = Config::read_existing(&path).unwrap();
            assert_eq!(c.wheel.hires, Some(false), "after {id}");
        }
        Config::update_file(&path, |c| c.scroll.direction = Some("natural".into())).unwrap();
        let c = Config::read_existing(&path).unwrap();
        assert_eq!(c.wheel.hires, Some(false));
        assert_eq!(c.scroll.speed, Some(300));
        Config::update_file(&path, |c| set_switch(c, ID_HIRES, true)).unwrap();
        assert_eq!(
            Config::read_existing(&path).unwrap().wheel.hires,
            Some(true)
        );
    }

    /// Same with a file written by the window itself (no `hires` key at first).
    #[test]
    fn hires_is_added_to_a_minimal_file() {
        let path = temp_config("minimal", "dpi = 1600\n[scroll]\nspeed = 1000\n");
        Config::update_file(&path, |c| set_switch(c, ID_HIRES, false)).unwrap();
        Config::update_file(&path, |c| set_slider(c, ID_SPEED, 50.0)).unwrap();
        let c = Config::read_existing(&path).unwrap();
        assert_eq!(c.wheel.hires, Some(false));
        assert_eq!(c.scroll.speed, Some(50));
    }

    #[test]
    fn thumb_modes_have_titles_and_index() {
        assert_eq!(mode_index(None), 0);
        assert_eq!(mode_index(Some("tabs")), 3);
        assert_eq!(mode_index(Some("nope")), 0);
        for lang in crate::i18n::available() {
            for m in ThumbMode::ALL {
                let key = format!("settings.scrolling.thumb_mode_{}", m.name());
                let text = t!(key.as_str(), locale = lang.as_str()).to_string();
                assert_ne!(text, key, "{lang}: {key}");
            }
        }
    }

    #[test]
    fn sliders_are_clamped() {
        let mut c = Config::default();
        set_slider(&mut c, ID_SPEED, 5.0);
        assert_eq!(c.scroll.speed, Some(10));
        set_slider(&mut c, ID_SPEED, 5000.0);
        assert_eq!(c.scroll.speed, Some(1000));
        set_slider(&mut c, ID_SMOOTHING, 95.0);
        assert_eq!(c.scroll.smoothing, Some(90));
    }
}
