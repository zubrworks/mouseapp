//! HID++ thumb wheel (0x2150, ThumbWheel).

use anyhow::{Result, bail};

use super::modify;
use crate::hidpp::device::Device;
use crate::hidpp::message::{Message, be_u16, byte};

pub const ID: u16 = 0x2150;

const GET_INFO: u8 = 0;
const GET_STATUS: u8 = 1;
const SET_REPORTING: u8 = 2;
/// Event number of the rotation notification.
const EVENT_ROTATION: u8 = 0;
/// Reporting mode value that diverts the wheel to software.
const MODE_DIVERTED: u8 = 1;

/// Thumb wheel reporting state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Thumbwheel {
    /// Events are diverted to software instead of the normal HID horizontal scroll.
    pub diverted: bool,
    pub invert: bool,
}

/// Requested change; `None` fields stay as they are.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Change {
    pub diverted: Option<bool>,
    pub invert: Option<bool>,
}

/// Parses getThumbwheelStatus: [reporting mode, invert, ...].
pub fn parse(p: &[u8]) -> Thumbwheel {
    Thumbwheel {
        diverted: byte(p, 0) == MODE_DIVERTED,
        invert: byte(p, 1) & 1 != 0,
    }
}

/// Encodes setThumbwheelReporting: [reporting mode, invert].
pub fn encode(t: Thumbwheel) -> [u8; 2] {
    [t.diverted as u8, t.invert as u8]
}

/// Applies a change to a state.
pub fn apply(t: Thumbwheel, c: Change) -> Thumbwheel {
    Thumbwheel {
        diverted: c.diverted.unwrap_or(t.diverted),
        invert: c.invert.unwrap_or(t.invert),
    }
}

/// Resolution used when getThumbwheelInfo cannot be read (MX Master 3/3S value).
pub const DEFAULT_DIVERTED_RES: u16 = 120;

/// `diverted_res` as reported by the mouse; 0 (unknown) becomes [`DEFAULT_DIVERTED_RES`].
pub fn diverted_res_or_default(diverted_res: u16) -> u16 {
    if diverted_res == 0 { DEFAULT_DIVERTED_RES } else { diverted_res }
}

/// getThumbwheelInfo (function 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    /// Ratchets per revolution in native (HID) mode.
    pub native_res: u16,
    /// Rotation increments per revolution in diverted mode.
    pub diverted_res: u16,
    /// Positive rotation means right/front (default direction bit set);
    /// otherwise positive means left/back.
    pub positive_right: bool,
}

/// Parses getThumbwheelInfo: [nativeRes(2), divertedRes(2), defaultDirection, ...].
pub fn parse_info(p: &[u8]) -> Option<Info> {
    if p.len() < 6 {
        return None;
    }
    Some(Info {
        native_res: be_u16(p, 0),
        diverted_res: be_u16(p, 2),
        positive_right: byte(p, 4) & 1 != 0,
    })
}

/// Reads getThumbwheelInfo from the feature at `idx`.
pub fn read_info(dev: &Device, idx: u8) -> Result<Info> {
    match parse_info(&dev.call(idx, GET_INFO, &[])?) {
        Some(i) => Ok(i),
        None => bail!("short thumb wheel info response"),
    }
}

/// Reads getThumbwheelStatus from the feature at `idx`.
pub fn read_status(dev: &Device, idx: u8) -> Result<Thumbwheel> {
    Ok(parse(&dev.call(idx, GET_STATUS, &[])?))
}

/// Phase of a thumb wheel movement as reported by the mouse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RotationStatus {
    Inactive,
    Start,
    Active,
    Stop,
    Other(u8),
}

impl RotationStatus {
    fn from_byte(b: u8) -> Self {
        match b {
            0 => RotationStatus::Inactive,
            1 => RotationStatus::Start,
            2 => RotationStatus::Active,
            3 => RotationStatus::Stop,
            x => RotationStatus::Other(x),
        }
    }
}

/// A thumb wheel notification (event 0, sent only while diverted).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rotation {
    /// Signed increments in units of `Info::diverted_res` per revolution.
    pub delta: i16,
    pub status: RotationStatus,
}

/// Decodes a thumb wheel notification: [rotation(2), timestamp(2), status, ...].
/// `None` for anything else, including responses to requests (non-zero software ID).
pub fn parse_rotation(feature_index: u8, m: &Message) -> Option<Rotation> {
    if m.feature_index != feature_index
        || m.function != EVENT_ROTATION
        || m.sw_id != 0
        || m.params.len() < 2
    {
        return None;
    }
    let p = &m.params;
    Some(Rotation {
        delta: be_u16(p, 0) as i16,
        status: RotationStatus::from_byte(byte(p, 4)),
    })
}

/// Reads the state; `None` if unsupported.
pub fn read(dev: &Device) -> Result<Option<Thumbwheel>> {
    let Some(idx) = dev.feature(ID)? else {
        return Ok(None);
    };
    Ok(Some(read_status(dev, idx)?))
}

/// Applies a change and returns the state read back.
pub fn write(dev: &Device, c: Change) -> Result<Thumbwheel> {
    let idx = dev.require(ID, "thumbwheel.unsupported")?;
    modify(dev, idx, (GET_STATUS, SET_REPORTING), parse, |cur| encode(apply(cur, c)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_status() {
        assert_eq!(
            parse(&[1, 1, 0]),
            Thumbwheel {
                diverted: true,
                invert: true
            }
        );
        assert_eq!(
            parse(&[0, 0]),
            Thumbwheel {
                diverted: false,
                invert: false
            }
        );
    }

    #[test]
    fn encodes_and_applies() {
        let t = Thumbwheel {
            diverted: false,
            invert: false,
        };
        let n = apply(
            t,
            Change {
                diverted: None,
                invert: Some(true),
            },
        );
        assert_eq!(encode(n), [0, 1]);
        assert_eq!(
            encode(apply(
                n,
                Change {
                    diverted: Some(true),
                    invert: None
                }
            )),
            [1, 1]
        );
    }

    #[test]
    fn parses_rotation() {
        use crate::hidpp::message::Message;
        let m = Message::request(1, 7, 0, 0, &[0xFF, 0xFD, 0, 0, 2, 0x02]).unwrap();
        assert_eq!(
            parse_rotation(7, &m),
            Some(Rotation {
                delta: -3,
                status: RotationStatus::Active
            })
        );
        assert_eq!(parse_rotation(8, &m), None);
        let stop = Message::request(1, 7, 0, 0, &[0, 0, 0, 0, 3]).unwrap();
        assert_eq!(
            parse_rotation(7, &stop).map(|r| r.status),
            Some(RotationStatus::Stop)
        );
        // Function 1 is not the rotation event.
        let other = Message::request(1, 7, 1, 0, &[0, 5]).unwrap();
        assert_eq!(parse_rotation(7, &other), None);
        // A non-zero software ID marks a reply to a request, not an event.
        let reply = Message::request(1, 7, 0, 1, &[0xFF, 0xFD, 0, 0, 2, 0x02]).unwrap();
        assert_eq!(parse_rotation(7, &reply), None);
    }

    #[test]
    fn parses_info() {
        // MX Master 3 values: 18 native, 120 diverted, positive right.
        let i = parse_info(&[0, 18, 0, 120, 1, 0x0F, 0x03, 0xE8, 0, 0]).unwrap();
        assert_eq!(
            i,
            Info {
                native_res: 18,
                diverted_res: 120,
                positive_right: true
            }
        );
        assert!(!parse_info(&[0, 18, 0, 120, 0, 0]).unwrap().positive_right);
        assert_eq!(parse_info(&[0, 18, 0]), None);
    }
}
