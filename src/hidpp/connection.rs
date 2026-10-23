//! HID++ 1.0 receiver "device connection" notification.

use super::message::Message;

/// HID++ 1.0 sub-ID of the receiver's device connection notification.
const SUB_ID_CONNECTION: u8 = 0x41;

/// What the receiver reports about the mouse link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    /// The mouse (re)connected.
    Connected,
    /// The link was lost.
    Lost,
}

/// Parses a connection notification; `None` if `msg` is something else.
/// Bit 0x40 of byte 3 set means the link was lost instead of established.
pub fn parse_connection(msg: &Message) -> Option<Link> {
    if msg.feature_index != SUB_ID_CONNECTION {
        return None;
    }
    let flags = (msg.function << 4) | msg.sw_id;
    Some(if flags & 0x40 == 0 { Link::Connected } else { Link::Lost })
}
