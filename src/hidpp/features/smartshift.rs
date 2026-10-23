//! HID++ SmartShift ratchet control: enhanced (0x2111), fallback legacy (0x2110).

use anyhow::{Result, bail};
use rust_i18n::t;
use serde::{Deserialize, Serialize};

use crate::hidpp::device::{Device, is_link_failure};
use crate::hidpp::message::byte;

/// Feature ID of the enhanced SmartShift.
pub const ID: u16 = 0x2111;
/// Feature ID of the legacy SmartShift.
pub const LEGACY_ID: u16 = 0x2110;
/// Threshold value meaning "always ratcheted" (SmartShift never disengages).
pub const ALWAYS_RATCHET: u8 = 0xFF;

/// Function numbers: enhanced (0x2111) and legacy (0x2110).
const ENHANCED_GET_CAPABILITIES: u8 = 0;
const ENHANCED_GET_STATUS: u8 = 1;
const ENHANCED_SET_STATUS: u8 = 2;
const LEGACY_GET_STATUS: u8 = 0;
const LEGACY_SET_STATUS: u8 = 1;

const MODE_FREESPIN: u8 = 1;
const MODE_RATCHET: u8 = 2;

/// Wheel mode and auto-disengage threshold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmartShift {
    /// True when the wheel is in ratchet mode, false for free-spin.
    pub ratchet: bool,
    /// Scroll speed at which the wheel frees up (1-254); 255 = always ratchet.
    pub threshold: u8,
    pub default: u8,
}

impl SmartShift {
    /// Default threshold reported by the mouse; `None` when it is 0 or 255.
    pub fn usable_default(&self) -> Option<u8> {
        match self.default {
            0 | ALWAYS_RATCHET => None,
            d => Some(d),
        }
    }
}

/// Requested change; `None` fields stay as they are.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Change {
    pub ratchet: Option<bool>,
    pub threshold: Option<u8>,
}

/// Parses the legacy (0x2110) status payload: [mode, auto-disengage, default auto-disengage].
pub fn parse(p: &[u8]) -> SmartShift {
    SmartShift {
        ratchet: byte(p, 0) == MODE_RATCHET,
        threshold: byte(p, 1),
        default: byte(p, 2),
    }
}

/// Parses the enhanced (0x2111) payloads. Status is [mode, auto-disengage, ratchet force]
/// (the third byte is NOT a default threshold); the default comes from the
/// capabilities payload [?, default auto-disengage, ...] (function 0, byte 1).
pub fn parse_enhanced(status: &[u8], caps: &[u8]) -> SmartShift {
    SmartShift {
        default: byte(caps, 1),
        ..parse(&status[..status.len().min(2)])
    }
}

/// Encodes the set payload; a zero byte means "no change".
pub fn encode(c: Change) -> [u8; 3] {
    let mode = match c.ratchet {
        Some(true) => MODE_RATCHET,
        Some(false) => MODE_FREESPIN,
        None => 0,
    };
    [mode, c.threshold.unwrap_or(0), 0]
}

/// Feature index, (get, set) function numbers and whether it is the enhanced version.
fn locate(dev: &Device) -> Result<Option<(u8, u8, u8, bool)>> {
    if let Some(i) = dev.feature(ID)? {
        return Ok(Some((i, ENHANCED_GET_STATUS, ENHANCED_SET_STATUS, true)));
    }
    Ok(dev
        .feature(LEGACY_ID)?
        .map(|i| (i, LEGACY_GET_STATUS, LEGACY_SET_STATUS, false)))
}

/// Reads the status; the enhanced version also needs getCapabilities for the default.
fn read_at(dev: &Device, idx: u8, get: u8, enhanced: bool) -> Result<SmartShift> {
    let status = dev.call(idx, get, &[])?;
    if !enhanced {
        return Ok(parse(&status));
    }
    // A device error on getCapabilities only loses the default (callers fall
    // back); a link failure is passed on.
    let caps = match dev.call(idx, ENHANCED_GET_CAPABILITIES, &[]) {
        Ok(caps) => caps,
        Err(e) if is_link_failure(&e) => return Err(e),
        Err(_) => Vec::new(),
    };
    Ok(parse_enhanced(&status, &caps))
}

/// Reads the state; `None` if unsupported.
pub fn read(dev: &Device) -> Result<Option<SmartShift>> {
    let Some((idx, get, _, enhanced)) = locate(dev)? else {
        return Ok(None);
    };
    Ok(Some(read_at(dev, idx, get, enhanced)?))
}

/// Applies a change and returns the state read back.
pub fn write(dev: &Device, c: Change) -> Result<SmartShift> {
    let Some((idx, get, set, enhanced)) = locate(dev)? else {
        bail!(t!("smartshift.unsupported"));
    };
    dev.call(idx, set, &encode(c))?;
    read_at(dev, idx, get, enhanced)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_status() {
        let s = parse(&[2, 30, 10]);
        assert_eq!(
            s,
            SmartShift {
                ratchet: true,
                threshold: 30,
                default: 10
            }
        );
        assert!(!parse(&[1, 255, 10]).ratchet);
    }

    #[test]
    fn decodes_enhanced_status() {
        // Third status byte (0x32) is the ratchet force, default comes from capabilities.
        let s = parse_enhanced(&[2, 30, 0x32], &[1, 15, 0x64]);
        assert_eq!(
            s,
            SmartShift {
                ratchet: true,
                threshold: 30,
                default: 15
            }
        );
        // Missing capabilities: no usable default.
        assert_eq!(parse_enhanced(&[1, 255, 0x32], &[]).usable_default(), None);
        // Legacy keeps the third byte as default.
        assert_eq!(parse(&[2, 30, 10]).default, 10);
    }

    #[test]
    fn encodes_change() {
        let c = Change {
            ratchet: Some(true),
            threshold: Some(40),
        };
        assert_eq!(encode(c), [2, 40, 0]);
        assert_eq!(
            encode(Change {
                ratchet: Some(false),
                threshold: None
            }),
            [1, 0, 0]
        );
        assert_eq!(encode(Change::default()), [0, 0, 0]);
    }
}
