//! HID++ reprogrammable controls v4 (0x1B04): button diversion and raw XY.

use anyhow::{Result, bail};

use crate::hidpp::device::Device;
use crate::hidpp::message::{Message, be_u16};

pub const ID: u16 = 0x1B04;

const GET_CONTROL_REPORTING: u8 = 2;
const SET_CONTROL_REPORTING: u8 = 3;

/// Reporting flag: divert the button to software (temporary, reset when the
/// mouse power-cycles).
const DIVERT: u8 = 0x01;
const DIVERT_VALID: u8 = 0x02;
/// Reporting flag: send raw XY movement while the button is held.
const RAW_XY: u8 = 0x10;
const RAW_XY_VALID: u8 = 0x20;

/// Event numbers of the feature's notifications.
const EVENT_BUTTONS: u8 = 0;
const EVENT_RAW_XY: u8 = 1;

/// A decoded notification from the controls feature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// All diverted buttons that are currently held down.
    Buttons(Vec<u16>),
    /// Raw movement of a button held with raw XY diversion.
    Move { dx: i16, dy: i16 },
}

/// What the mouse currently does with one button (getControlReporting).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reporting {
    /// The button is diverted to software.
    pub diverted: bool,
    /// Raw XY movement is reported while the button is held.
    pub raw_xy: bool,
}

impl Reporting {
    /// True if the diversion set up for the button is gone: not diverted, or
    /// (for a gesture button) raw XY off.
    pub fn is_lost(self, want_raw_xy: bool) -> bool {
        !self.diverted || (want_raw_xy && !self.raw_xy)
    }
}

/// Decodes the getControlReporting response for `cid`: [cid, flags, target cid].
/// The response has no "valid" bits, only the state.
pub fn decode_reporting(cid: u16, params: &[u8]) -> Result<Reporting> {
    if params.len() < 3 {
        bail!("short control reporting response ({} bytes)", params.len());
    }
    let got = be_u16(params, 0);
    if got != cid {
        bail!("control reporting response for 0x{got:04X}, asked for 0x{cid:04X}");
    }
    Ok(Reporting { diverted: params[2] & DIVERT != 0, raw_xy: params[2] & RAW_XY != 0 })
}

/// Encodes setControlReporting parameters: [cid, flags, target cid].
pub fn encode_reporting(cid: u16, divert: bool, raw_xy: bool) -> [u8; 5] {
    let mut flags = DIVERT_VALID | RAW_XY_VALID;
    if divert {
        flags |= DIVERT;
    }
    if raw_xy {
        flags |= RAW_XY;
    }
    let c = cid.to_be_bytes();
    [c[0], c[1], flags, c[0], c[1]]
}

/// Decodes a notification; `None` if it belongs to another feature or event,
/// or is a response to a request (non-zero software ID).
pub fn parse_event(feature_index: u8, m: &Message) -> Option<Event> {
    if m.feature_index != feature_index || m.sw_id != 0 {
        return None;
    }
    let p = &m.params;
    match m.function {
        EVENT_BUTTONS => Some(Event::Buttons(
            p.chunks_exact(2)
                .map(|c| be_u16(c, 0))
                .take_while(|&c| c != 0)
                .collect(),
        )),
        EVENT_RAW_XY if p.len() >= 4 => Some(Event::Move {
            dx: be_u16(p, 0) as i16,
            dy: be_u16(p, 2) as i16,
        }),
        _ => None,
    }
}

/// What another program set with setControlReporting, as the mouse echoed it
/// in its response (which macOS hands to every process that has the mouse
/// open). `None`: that setting was not part of the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ForeignSet {
    pub cid: u16,
    pub divert: Option<bool>,
    pub raw_xy: Option<bool>,
}

impl ForeignSet {
    /// True if the request took away the diversion set up for the button:
    /// diversion off, or (for a gesture button) raw XY off.
    pub fn removes(self, want_raw_xy: bool) -> bool {
        self.divert == Some(false) || (want_raw_xy && self.raw_xy == Some(false))
    }
}

/// Decodes the response to a setControlReporting request (a response has a
/// non-zero software ID) without looking at whose it is: another program may
/// use an ID from our range (Options+ takes 0x08-0x0F, the daemon's), so the
/// ID says nothing. Our own responses carry a setting that never removes
/// the diversion set up for the button (see [`ForeignSet::removes`]): during
/// a session the daemon only diverts buttons (a plain one without raw XY, a
/// gesture one with it), and the diversion is removed only at the end of the
/// session, when nothing reads these responses any more. `None` for
/// anything else.
pub fn parse_foreign_set(feature_index: u8, m: &Message) -> Option<ForeignSet> {
    if m.feature_index != feature_index
        || m.function != SET_CONTROL_REPORTING
        || m.sw_id == 0
        || m.params.len() < 3
    {
        return None;
    }
    let flags = m.params[2];
    Some(ForeignSet {
        cid: be_u16(&m.params, 0),
        divert: (flags & DIVERT_VALID != 0).then_some(flags & DIVERT != 0),
        raw_xy: (flags & RAW_XY_VALID != 0).then_some(flags & RAW_XY != 0),
    })
}

/// Turns diversion of `cid` (and optionally raw XY) on or off.
pub fn set_reporting(dev: &Device, idx: u8, cid: u16, divert: bool, raw_xy: bool) -> Result<()> {
    dev.call(idx, SET_CONTROL_REPORTING, &encode_reporting(cid, divert, raw_xy))?;
    Ok(())
}

/// Reads how the mouse currently reports button `cid`.
pub fn get_reporting(dev: &Device, idx: u8, cid: u16) -> Result<Reporting> {
    decode_reporting(cid, &dev.call(idx, GET_CONTROL_REPORTING, &cid.to_be_bytes())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(function: u8, params: &[u8]) -> Message {
        Message::request(1, 9, function, 0, params).unwrap()
    }

    #[test]
    fn encodes_reporting() {
        assert_eq!(encode_reporting(0x00C3, true, true), [0x00, 0xC3, 0x33, 0x00, 0xC3]);
        assert_eq!(encode_reporting(0x0053, true, false), [0x00, 0x53, 0x23, 0x00, 0x53]);
        // Reset: both flags valid but cleared.
        assert_eq!(encode_reporting(0x00C3, false, false)[2], 0x22);
    }

    #[test]
    fn decodes_reporting() {
        let r = |p: &[u8]| decode_reporting(0x00C3, p).unwrap();
        // Diverted with raw XY (what set_reporting(cid, true, true) leaves).
        assert_eq!(r(&[0x00, 0xC3, 0x11, 0x00, 0xC3]), Reporting { diverted: true, raw_xy: true });
        assert_eq!(r(&[0x00, 0xC3, 0x01, 0x00, 0xC3]), Reporting { diverted: true, raw_xy: false });
        assert_eq!(r(&[0x00, 0xC3, 0x00, 0x00, 0x00]), Reporting { diverted: false, raw_xy: false });
        // Other flag bits do not count; a shorter response is enough for the flags.
        assert_eq!(r(&[0x00, 0xC3, 0x0C]), Reporting { diverted: false, raw_xy: false });
        // Wrong button or truncated response.
        assert!(decode_reporting(0x00C3, &[0x00, 0x53, 0x11]).is_err());
        assert!(decode_reporting(0x00C3, &[0x00, 0xC3]).is_err());
    }

    #[test]
    fn lost_diversion_depends_on_button_kind() {
        let on = Reporting { diverted: true, raw_xy: true };
        let plain = Reporting { diverted: true, raw_xy: false };
        let off = Reporting { diverted: false, raw_xy: false };
        assert!(!on.is_lost(true) && !on.is_lost(false));
        assert!(plain.is_lost(true) && !plain.is_lost(false));
        assert!(off.is_lost(true) && off.is_lost(false));
    }

    #[test]
    fn parses_buttons_and_move() {
        assert_eq!(
            parse_event(9, &msg(0, &[0x00, 0xC3, 0x00, 0x53, 0, 0])),
            Some(Event::Buttons(vec![0x00C3, 0x0053]))
        );
        assert_eq!(parse_event(9, &msg(0, &[0, 0, 0, 0])), Some(Event::Buttons(vec![])));
        assert_eq!(
            parse_event(9, &msg(1, &[0xFF, 0xFE, 0x00, 0x05])),
            Some(Event::Move { dx: -2, dy: 5 })
        );
        assert_eq!(parse_event(8, &msg(0, &[])), None);
        assert_eq!(parse_event(9, &msg(2, &[])), None);
    }

    #[test]
    fn ignores_responses_to_requests() {
        // A non-zero software ID marks a reply to a request, not an event.
        let reply = |function, params: &[u8]| {
            Message::request(1, 9, function, 1, params).unwrap()
        };
        assert_eq!(parse_event(9, &reply(0, &[0, 0, 0, 0])), None);
        assert_eq!(parse_event(9, &reply(1, &[0xFF, 0xFE, 0x00, 0x05])), None);
    }

    #[test]
    fn recognizes_the_response_to_a_foreign_set_reporting() {
        let reply = |function, sw_id, params: &[u8]| {
            Message::request(1, 9, function, sw_id, params).unwrap()
        };
        let off = reply(3, 2, &[0x00, 0xC3, 0x22, 0x00, 0xC3]);
        let got = parse_foreign_set(9, &off).unwrap();
        assert_eq!(got, ForeignSet { cid: 0x00C3, divert: Some(false), raw_xy: Some(false) });
        assert!(got.removes(false) && got.removes(true));
        // Only the diversion flag in the request: raw XY is not touched.
        let only = parse_foreign_set(9, &reply(3, 2, &[0x00, 0xC3, 0x02, 0x00, 0xC3])).unwrap();
        assert_eq!(only, ForeignSet { cid: 0x00C3, divert: Some(false), raw_xy: None });
        // The same program diverting the button for itself keeps our setting,
        // unless it turns raw XY off.
        let on = parse_foreign_set(9, &reply(3, 2, &[0x00, 0xC3, 0x33, 0x00, 0xC3])).unwrap();
        assert!(!on.removes(false) && !on.removes(true));
        let no_xy = parse_foreign_set(9, &reply(3, 2, &[0x00, 0xC3, 0x23, 0, 0xC3]));
        assert!(no_xy.is_some_and(|s| s.removes(true)));
        // The ID does not matter: Options+ may use ours (0x08-0x0F).
        let own_id = parse_foreign_set(9, &reply(3, 9, &[0x00, 0xC3, 0x22]));
        assert!(own_id.is_some_and(|s| s.removes(false)));
        // A notification, other features and functions.
        assert_eq!(parse_foreign_set(9, &reply(3, 0, &[0x00, 0xC3, 0x22])), None);
        assert_eq!(parse_foreign_set(8, &off), None);
        assert_eq!(parse_foreign_set(9, &reply(2, 2, &[0x00, 0xC3, 0x22])), None);
    }
}
