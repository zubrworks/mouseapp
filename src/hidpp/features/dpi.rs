//! HID++ DPI feature (0x2201, AdjustableDPI).

use anyhow::{Result, bail};
use rust_i18n::t;
use serde::{Deserialize, Serialize};

use crate::hidpp::device::Device;
use crate::hidpp::message::be_u16;

pub const ID: u16 = 0x2201;

const GET_LIST: u8 = 1;
const GET_DPI: u8 = 2;
const SET_DPI: u8 = 3;
/// Sensor used for all requests (mice have a single sensor).
const SENSOR: u8 = 0;
/// Words at or above this value in the DPI list mark a step ("range") entry.
const STEP_MARK: u16 = 0xE000;

/// Current and default DPI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dpi {
    pub current: u16,
    pub default: u16,
}

/// Supported DPI values: explicit values and/or `(min, max, step)` ranges.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DpiList {
    pub values: Vec<u16>,
    pub ranges: Vec<(u16, u16, u16)>,
}

impl DpiList {
    /// Smallest supported DPI.
    pub fn min(&self) -> u16 {
        let v = self.values.iter().copied().min();
        let r = self.ranges.iter().map(|r| r.0).min();
        v.into_iter().chain(r).min().unwrap_or(0)
    }

    /// Largest supported DPI.
    pub fn max(&self) -> u16 {
        let v = self.values.iter().copied().max();
        let r = self.ranges.iter().map(|r| r.1).max();
        v.into_iter().chain(r).max().unwrap_or(0)
    }

    /// True if `dpi` is exactly one of the supported values.
    pub fn contains(&self, dpi: u16) -> bool {
        self.values.contains(&dpi)
            || self.ranges.iter().any(|&(lo, hi, step)| {
                dpi >= lo && dpi <= hi && step != 0 && (dpi - lo).is_multiple_of(step)
            })
    }

    /// Fails with a readable message if `dpi` is not supported.
    pub fn validate(&self, dpi: u16) -> Result<()> {
        if self.contains(dpi) {
            return Ok(());
        }
        let step = self.ranges.first().map_or(0, |r| r.2);
        bail!(t!(
            "dpi.invalid",
            value = dpi,
            min = self.min(),
            max = self.max(),
            step = step
        ))
    }
}

/// Parses getSensorDpiList: [sensor, words...] terminated by a zero word.
pub fn parse_list(p: &[u8]) -> DpiList {
    let mut list = DpiList::default();
    let words: Vec<u16> = p
        .get(1..)
        .unwrap_or(&[])
        .chunks_exact(2)
        .map(|c| be_u16(c, 0))
        .take_while(|&w| w != 0)
        .collect();
    let mut i = 0;
    while i < words.len() {
        let w = words[i];
        if w >= STEP_MARK {
            // Range: previous value is the minimum, next word the maximum.
            if let (Some(lo), Some(&hi)) = (list.values.pop(), words.get(i + 1)) {
                list.ranges.push((lo, hi, w - STEP_MARK));
            }
            i += 2;
        } else {
            list.values.push(w);
            i += 1;
        }
    }
    list
}

/// Parses getSensorDpi: [sensor, dpi (2), default dpi (2)].
pub fn parse_dpi(p: &[u8]) -> Dpi {
    Dpi {
        current: be_u16(p, 1),
        default: be_u16(p, 3),
    }
}

/// Encodes setSensorDpi parameters.
pub fn encode_set(dpi: u16) -> [u8; 3] {
    let b = dpi.to_be_bytes();
    [SENSOR, b[0], b[1]]
}

/// Reads the current DPI; `None` if unsupported.
pub fn read(dev: &Device) -> Result<Option<Dpi>> {
    let Some(idx) = dev.feature(ID)? else {
        return Ok(None);
    };
    Ok(Some(parse_dpi(&dev.call(idx, GET_DPI, &[SENSOR])?)))
}

/// Reads the list of supported DPI values; `None` if unsupported.
pub fn read_list(dev: &Device) -> Result<Option<DpiList>> {
    let Some(idx) = dev.feature(ID)? else {
        return Ok(None);
    };
    Ok(Some(parse_list(&dev.call(idx, GET_LIST, &[SENSOR])?)))
}

/// Validates against the device's list and writes the DPI. Returns the value read back.
pub fn write(dev: &Device, dpi: u16) -> Result<Dpi> {
    let idx = dev.require(ID, "dpi.unsupported")?;
    let list = parse_list(&dev.call(idx, GET_LIST, &[SENSOR])?);
    list.validate(dpi)?;
    dev.call(idx, SET_DPI, &encode_set(dpi))?;
    Ok(parse_dpi(&dev.call(idx, GET_DPI, &[SENSOR])?))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Sensor 0, 200, step 50 (0xE032), 8000, terminator.
    const LIST: [u8; 9] = [0, 0x00, 0xC8, 0xE0, 0x32, 0x1F, 0x40, 0, 0];

    #[test]
    fn parses_range_list() {
        let l = parse_list(&LIST);
        assert!(l.values.is_empty());
        assert_eq!(l.ranges, vec![(200, 8000, 50)]);
        assert_eq!((l.min(), l.max()), (200, 8000));
    }

    #[test]
    fn parses_value_list() {
        let l = parse_list(&[0, 0x03, 0x20, 0x06, 0x40, 0, 0]);
        assert_eq!(l.values, vec![800, 1600]);
    }

    #[test]
    fn validates_range_and_step() {
        let l = parse_list(&LIST);
        assert!(l.validate(1600).is_ok());
        assert!(l.validate(200).is_ok());
        assert!(l.validate(8000).is_ok());
        assert!(l.validate(1620).is_err());
        assert!(l.validate(100).is_err());
        assert!(l.validate(8050).is_err());
    }

    #[test]
    fn decodes_and_encodes_dpi() {
        let d = parse_dpi(&[0, 0x06, 0x40, 0x04, 0x00]);
        assert_eq!(
            d,
            Dpi {
                current: 1600,
                default: 1024
            }
        );
        assert_eq!(encode_set(1600), [0, 0x06, 0x40]);
    }
}
