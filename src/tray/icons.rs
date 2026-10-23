//! Menu bar and menu icons: the battery icon drawn pixel by pixel and the
//! SF Symbol of the menu bar item.

use anyhow::Result;
use objc2::AnyThread;
use objc2_app_kit::{NSBitmapImageRep, NSDeviceRGBColorSpace, NSGraphicsContext};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use rust_i18n::t;
use tray_icon::Icon;

use crate::hidpp::features::battery::{Battery, Charge};

/// SF Symbol used for the menu bar icon.
const SYMBOL: &str = "computermouse";

/// Digits 3x5 pixels, one row per entry, bit 2 = leftmost column.
const DIGITS: [[u8; 5]; 10] = [
    [7, 5, 5, 5, 7],
    [2, 6, 2, 2, 7],
    [7, 1, 7, 4, 7],
    [7, 1, 7, 1, 7],
    [5, 5, 7, 1, 1],
    [7, 4, 7, 1, 7],
    [7, 4, 7, 5, 7],
    [7, 1, 1, 1, 1],
    [7, 5, 7, 5, 7],
    [7, 5, 7, 1, 7],
];
/// Percent sign, 3x5.
const PERCENT: [u8; 5] = [5, 1, 2, 4, 5];
/// Lightning bolt, 5x9, drawn left of the battery while charging.
const BOLT: [&str; 9] = [
    "....#", "...##", "..##.", ".####", "####.", "..##.", ".##..", "##...", "#....",
];
const BODY_W: usize = 26;
const BODY_H: usize = 14;
const NUB_W: usize = 2;
const BOLT_W: usize = 5;
const BOLT_GAP: usize = 2;

/// Draws the battery as RGBA pixels (black on transparent, for a template
/// image): outline, fill by percent, percent text knocked out of the fill
/// (and drawn solid on the empty part), bolt on the left while charging.
pub(super) fn battery_icon(b: Battery) -> (Vec<u8>, u32, u32) {
    let bolt = b.charge == Charge::Charging;
    let left = if bolt { BOLT_W + BOLT_GAP } else { 0 };
    let w = left + BODY_W + NUB_W;
    let h = BODY_H;
    let mut a = vec![false; w * h];
    let set = |a: &mut Vec<bool>, x: usize, y: usize, v: bool| a[y * w + x] = v;
    if bolt {
        for (y, row) in BOLT.iter().enumerate() {
            for (x, c) in row.bytes().enumerate() {
                set(&mut a, x, y + (h - BOLT.len()) / 2, c == b'#');
            }
        }
    }
    // Outline with cut corners.
    for y in 0..h {
        for x in 0..BODY_W {
            let edge = x == 0 || y == 0 || x == BODY_W - 1 || y == h - 1;
            let corner = (x == 0 || x == BODY_W - 1) && (y == 0 || y == h - 1);
            set(&mut a, left + x, y, edge && !corner);
        }
    }
    for y in 4..h - 4 {
        for x in 0..NUB_W {
            set(&mut a, left + BODY_W + x, y, true);
        }
    }
    // Fill, then text toggling the pixels it covers.
    let inner_w = BODY_W - 4;
    let filled = b.percent.map_or(0, |p| {
        let p = usize::from(p.min(100));
        if p == 0 {
            0
        } else {
            (inner_w * p).div_ceil(100).max(1)
        }
    });
    for y in 2..h - 2 {
        for x in 0..filled {
            set(&mut a, left + 2 + x, y, true);
        }
    }
    if let Some(p) = b.percent {
        let text = p.min(100).to_string();
        let glyphs: Vec<[u8; 5]> = text
            .bytes()
            .map(|d| DIGITS[usize::from(d - b'0')])
            .chain(std::iter::once(PERCENT))
            .collect();
        let tw = glyphs.len() * 4 - 1;
        let x0 = left + (BODY_W - tw) / 2;
        let y0 = (h - 5) / 2;
        for (i, g) in glyphs.iter().enumerate() {
            for (dy, row) in g.iter().enumerate() {
                for dx in 0..3 {
                    if row >> (2 - dx) & 1 == 1 {
                        let (x, y) = (x0 + i * 4 + dx, y0 + dy);
                        a[y * w + x] = !a[y * w + x];
                    }
                }
            }
        }
    }
    let rgba = a
        .iter()
        .flat_map(|&on| [0, 0, 0, if on { 255 } else { 0 }])
        .collect();
    (rgba, w as u32, h as u32)
}

/// Renders the SF Symbol into RGBA pixels (colour is irrelevant: the system
/// uses only the alpha channel of a template image).
pub(super) fn symbol_icon() -> Option<Icon> {
    const PX: usize = 36; // 18 pt at 2x
    let image = crate::macos::symbol_image(SYMBOL, &t!("menu.tooltip"))?;
    // SAFETY: a null data pointer makes AppKit allocate the bitmap, so no Rust memory is shared.
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            PX as isize,
            PX as isize,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            (PX * 4) as isize,
            32,
        )
    }?;
    let ctx = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)?;
    let size = image.size();
    if size.width <= 0.0 || size.height <= 0.0 {
        return None;
    }
    let scale = (PX as f64 / size.width).min(PX as f64 / size.height);
    let (w, h) = (size.width * scale, size.height * scale);
    let rect = NSRect::new(
        NSPoint::new((PX as f64 - w) / 2.0, (PX as f64 - h) / 2.0),
        NSSize::new(w, h),
    );
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&ctx));
    image.drawInRect(rect);
    NSGraphicsContext::restoreGraphicsState_class();
    let data = rep.bitmapData();
    if data.is_null() {
        return None;
    }
    // SAFETY: the rep owns PX * PX * 4 bytes (bytesPerRow was set to PX * 4).
    let src = unsafe { std::slice::from_raw_parts(data, PX * PX * 4) };
    let mut rgba = Vec::with_capacity(src.len());
    for px in src.chunks_exact(4) {
        rgba.extend_from_slice(&[0, 0, 0, px[3]]);
    }
    if rgba.chunks_exact(4).all(|p| p[3] == 0) {
        return None;
    }
    Icon::from_rgba(rgba, PX as u32, PX as u32).ok()
}

/// Plain rounded shape used when the SF Symbol cannot be rendered.
pub(super) fn fallback_icon() -> Result<Icon> {
    const PX: u32 = 36;
    let mut rgba = vec![0u8; (PX * PX * 4) as usize];
    for y in 4..32u32 {
        for x in 10..26u32 {
            let (dx, dy) = (x as f32 - 17.5, y as f32 - 17.5);
            // Rounded rectangle: pill with radius 8.
            let (ax, ay) = (dx.abs(), (dy.abs() - 6.0).max(0.0));
            if ax * ax + ay * ay <= 64.0 {
                let i = ((y * PX + x) * 4) as usize;
                rgba[i + 3] = 255;
            }
        }
    }
    Ok(Icon::from_rgba(rgba, PX, PX)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_icon_size_and_bolt() {
        let b = |p, charge| Battery { percent: p, charge };
        let (rgba, w, h) = battery_icon(b(Some(80), Charge::Discharging));
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        assert_eq!((w, h), ((BODY_W + NUB_W) as u32, BODY_H as u32));
        let (_, w2, _) = battery_icon(b(Some(80), Charge::Charging));
        assert_eq!(w2, w + (BOLT_W + BOLT_GAP) as u32);
        // No percent: still an icon; 0 and 100 must not panic.
        battery_icon(b(None, Charge::Other));
        battery_icon(b(Some(0), Charge::Discharging));
        battery_icon(b(Some(100), Charge::Full));
    }
}
