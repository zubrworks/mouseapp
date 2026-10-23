//! Wireless Device Status (0x1D4B): the mouse announces that it powered on
//! or reconnected and has lost the settings software gave it (DPI, diverted
//! buttons, thumb wheel), so they have to be written again.

use anyhow::Result;

use crate::hidpp::device::{Device, is_link_failure};
use crate::hidpp::message::{Message, byte};

pub const ID: u16 = 0x1D4B;

/// Event number of the status broadcast.
const EVENT_STATUS: u8 = 0;
/// `status` byte: the device has just reconnected.
const STATUS_RECONNECTION: u8 = 1;
/// `request` byte: software must configure the device again.
const REQUEST_RECONFIGURE: u8 = 1;
/// `reason` byte: the power switch was turned on.
const REASON_POWER_SWITCH: u8 = 1;

/// A decoded status broadcast: [status, request, reason, ...].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub status: u8,
    pub request: u8,
    pub reason: u8,
}

impl Status {
    /// True if the mouse reconnected or asks to be configured again.
    pub fn needs_setup(&self) -> bool {
        self.status == STATUS_RECONNECTION || self.request == REQUEST_RECONFIGURE
    }

    /// True if the reconnection was caused by the power switch.
    pub fn power_switch(&self) -> bool {
        self.reason == REASON_POWER_SWITCH
    }
}

/// Looks up the feature index; `None` if the mouse does not have it.
/// Only a lost link is an error: any other lookup failure just means the
/// session runs without these notifications.
pub fn find(dev: &Device) -> Result<Option<u8>> {
    match dev.feature(ID) {
        Ok(idx) => Ok(idx),
        Err(e) if is_link_failure(&e) => Err(e),
        Err(e) => {
            log::debug!("wireless status feature lookup failed: {e}");
            Ok(None)
        }
    }
}

/// Decodes a status broadcast; `None` if `m` is anything else.
pub fn parse_event(feature_index: u8, m: &Message) -> Option<Status> {
    if m.feature_index != feature_index || m.function != EVENT_STATUS || m.sw_id != 0 {
        return None;
    }
    let p = &m.params;
    Some(Status {
        status: byte(p, 0),
        request: byte(p, 1),
        reason: byte(p, 2),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A raw long report: [report id, device, feature index, function|sw id, params...].
    fn report(feature_index: u8, fn_sw: u8, params: &[u8]) -> Message {
        let mut raw = vec![0x11, 0xFF, feature_index, fn_sw];
        raw.extend_from_slice(params);
        raw.resize(20, 0);
        Message::parse(&raw).unwrap()
    }

    #[test]
    fn parses_power_on_broadcast() {
        let s = parse_event(0x0B, &report(0x0B, 0x00, &[0x01, 0x01, 0x01])).unwrap();
        assert_eq!((s.status, s.request, s.reason), (1, 1, 1));
        assert!(s.needs_setup() && s.power_switch());
    }

    #[test]
    fn reconnection_or_request_alone_needs_setup() {
        let reconnect = parse_event(0x0B, &report(0x0B, 0x00, &[0x01, 0x00, 0x00])).unwrap();
        assert!(reconnect.needs_setup() && !reconnect.power_switch());
        let request = parse_event(0x0B, &report(0x0B, 0x00, &[0x00, 0x01, 0x00])).unwrap();
        assert!(request.needs_setup());
        let nothing = parse_event(0x0B, &report(0x0B, 0x00, &[0x00, 0x00, 0x00])).unwrap();
        assert!(!nothing.needs_setup());
    }

    #[test]
    fn ignores_other_messages() {
        // Another feature index, another event, a reply to a request (sw id set).
        assert_eq!(parse_event(0x0B, &report(0x0C, 0x00, &[1, 1, 1])), None);
        assert_eq!(parse_event(0x0B, &report(0x0B, 0x10, &[1, 1, 1])), None);
        assert_eq!(parse_event(0x0B, &report(0x0B, 0x01, &[1, 1, 1])), None);
    }
}
