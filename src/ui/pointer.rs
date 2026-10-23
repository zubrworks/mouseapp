//! Pointer tab: DPI and SmartShift threshold.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSSlider, NSView};
use rust_i18n::t;

use super::{Context, Effect, form, read_config, set_dpi, show_slider_value, slider, slider_value, update_config};

const ID_DPI: &str = "pointer.dpi";
const ID_THRESHOLD: &str = "pointer.threshold";

/// Rounds a slider value to a valid DPI of the range `(min, max, step)`.
fn snap_dpi(value: f64, (min, max, step): (u16, u16, u16)) -> u16 {
    let steps = ((value - f64::from(min)) / f64::from(step)).round().max(0.0) as u32;
    (u32::from(min) + steps * u32::from(step)).min(u32::from(max)) as u16
}

pub fn build(ctx: &Context, target: &AnyObject, mtm: MainThreadMarker) -> Retained<NSView> {
    let cfg = read_config(ctx);
    let mouse = ctx.mouse.borrow().clone();
    let (min, max, _) = mouse.dpi_range();
    let dpi = cfg.dpi.unwrap_or(mouse.model.default_dpi);
    let dpi_slider = slider(
        (f64::from(min), f64::from(max)),
        f64::from(dpi),
        &t!("settings.pointer.dpi_value", value = dpi),
        ID_DPI,
        target,
        mtm,
    );
    let range = &mouse.model.smartshift;
    let threshold = cfg.smartshift.threshold.unwrap_or_else(|| mouse.smartshift_default());
    let threshold_slider = slider(
        (f64::from(range.min), f64::from(range.max)),
        f64::from(threshold),
        &threshold.to_string(),
        ID_THRESHOLD,
        target,
        mtm,
    );
    form(
        vec![
            (t!("settings.pointer.dpi").to_string(), dpi_slider),
            (
                t!("settings.pointer.threshold").to_string(),
                threshold_slider,
            ),
        ],
        mtm,
    )
}

pub fn changed(id: &str, sender: &AnyObject, ctx: &Context, mtm: MainThreadMarker) -> Vec<Effect> {
    match id {
        ID_DPI => {
            let Some(value) = slider_value(sender, 1.0) else {
                return Vec::new();
            };
            let dpi = snap_dpi(value, ctx.mouse.borrow().dpi_range());
            if let Some(slider) = sender.downcast_ref::<NSSlider>() {
                // Show the snapped value; the window is not rebuilt.
                show_slider_value(slider, i64::from(dpi));
            }
            set_dpi(ctx, dpi)
        }
        ID_THRESHOLD => {
            let Some(value) = slider_value(sender, 1.0) else {
                return Vec::new();
            };
            let range = &ctx.mouse.borrow().model.smartshift;
            let value = value.clamp(f64::from(range.min), f64::from(range.max)) as u8;
            update_config(ctx, mtm, |c| c.smartshift.threshold = Some(value))
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpi_snaps_to_step_and_range() {
        let r = (200, 8000, 50);
        assert_eq!(snap_dpi(1623.0, r), 1600);
        assert_eq!(snap_dpi(1.0, r), 200);
        assert_eq!(snap_dpi(99999.0, r), 8000);
        assert_eq!(snap_dpi(1640.0, r), 1650);
        assert_eq!(snap_dpi(1234.0, (400, 4000, 100)), 1200);
    }
}
