//! HID++ high-resolution wheel (0x2121, HiResWheel).

use anyhow::Result;

use super::modify;
use crate::hidpp::device::Device;
use crate::hidpp::message::byte;

pub const ID: u16 = 0x2121;

const GET_CAPABILITY: u8 = 0;
const GET_MODE: u8 = 1;
const SET_MODE: u8 = 2;

const BIT_DIVERT: u8 = 0x01;
const BIT_HIRES: u8 = 0x02;

/// Wheel mode bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WheelMode {
    /// Events are diverted to software instead of the normal HID wheel.
    pub diverted: bool,
    pub hires: bool,
}

/// Requested change; `None` fields stay as they are.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Change {
    pub hires: Option<bool>,
}

/// Wheel capabilities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capability {
    /// High-resolution multiplier (events per notch).
    pub multiplier: u8,
}

pub fn parse_capability(p: &[u8]) -> Capability {
    Capability {
        multiplier: byte(p, 0),
    }
}

pub fn parse_mode(p: &[u8]) -> WheelMode {
    let b = byte(p, 0);
    WheelMode {
        diverted: b & BIT_DIVERT != 0,
        hires: b & BIT_HIRES != 0,
    }
}

pub fn encode_mode(m: WheelMode) -> u8 {
    let mut b = 0;
    if m.diverted {
        b |= BIT_DIVERT;
    }
    if m.hires {
        b |= BIT_HIRES;
    }
    b
}

/// Applies a change to a mode.
pub fn apply(m: WheelMode, c: Change) -> WheelMode {
    WheelMode {
        diverted: m.diverted,
        hires: c.hires.unwrap_or(m.hires),
    }
}

/// Reads capability and mode; `None` if unsupported.
pub fn read(dev: &Device) -> Result<Option<(Capability, WheelMode)>> {
    let Some(idx) = dev.feature(ID)? else {
        return Ok(None);
    };
    let cap = parse_capability(&dev.call(idx, GET_CAPABILITY, &[])?);
    let mode = parse_mode(&dev.call(idx, GET_MODE, &[])?);
    Ok(Some((cap, mode)))
}

/// Applies a change and returns the mode read back.
pub fn write(dev: &Device, c: Change) -> Result<WheelMode> {
    let idx = dev.require(ID, "wheel.unsupported")?;
    modify(dev, idx, (GET_MODE, SET_MODE), parse_mode, |cur| [encode_mode(apply(cur, c))])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_mode() {
        let m = parse_mode(&[0x02]);
        assert_eq!(
            m,
            WheelMode {
                diverted: false,
                hires: true
            }
        );
    }

    #[test]
    fn encodes_mode() {
        let m = WheelMode {
            diverted: true,
            hires: true,
        };
        assert_eq!(encode_mode(m), 0x03);
        assert_eq!(parse_mode(&[encode_mode(m)]), m);
    }

    #[test]
    fn applies_change_keeping_rest() {
        let m = WheelMode {
            diverted: true,
            hires: false,
        };
        let n = apply(
            m,
            Change {
                hires: Some(true),
            },
        );
        assert_eq!(
            n,
            WheelMode {
                diverted: true,
                hires: true
            }
        );
    }

    #[test]
    fn decodes_capability() {
        let c = parse_capability(&[8, 0x0C]);
        assert_eq!(c, Capability { multiplier: 8 });
    }
}
