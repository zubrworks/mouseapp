//! Per-feature HID++ implementations, one file per feature.

pub mod battery;
pub mod controls;
pub mod dpi;
pub mod fwinfo;
pub mod host;
pub mod name;
pub mod smartshift;
pub mod thumbwheel;
pub mod wheel;
pub mod wireless;

use anyhow::Result;

use super::device::Device;

/// Read-modify-write of a feature setting: reads with `get`, writes the
/// bytes `change` produces from the current value with `set`, and returns
/// the value read back.
pub fn modify<T, B: AsRef<[u8]>>(
    dev: &Device,
    idx: u8,
    (get, set): (u8, u8),
    parse: impl Fn(&[u8]) -> T,
    change: impl FnOnce(T) -> B,
) -> Result<T> {
    let cur = parse(&dev.call(idx, get, &[])?);
    dev.call(idx, set, change(cur).as_ref())?;
    Ok(parse(&dev.call(idx, get, &[])?))
}
