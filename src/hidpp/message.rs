//! Building and parsing HID++ 2.0 messages.

use anyhow::{Result, bail};

use super::transport::{LONG_LEN, REPORT_LONG, report_len};

/// Feature index that marks a HID++ 2.0 error response.
const ERR_FEATURE_V2: u8 = 0xFF;
/// Sub-ID that marks a HID++ 1.0 error response.
const ERR_SUB_ID_V1: u8 = 0x8F;

/// Byte `i` of a payload; 0 when the payload is too short.
pub fn byte(p: &[u8], i: usize) -> u8 {
    p.get(i).copied().unwrap_or(0)
}

/// Big-endian 16-bit word at `i` of a payload; missing bytes count as 0.
pub fn be_u16(p: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([byte(p, i), byte(p, i + 1)])
}

/// A decoded HID++ 2.0 message (request, response or notification).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub report_id: u8,
    pub device_index: u8,
    pub feature_index: u8,
    /// Function number (high nibble of byte 3).
    pub function: u8,
    /// Software ID (low nibble of byte 3); echoed back in responses.
    pub sw_id: u8,
    /// Payload bytes (3 for short reports, 16 for long ones).
    pub params: Vec<u8>,
}

impl Message {
    /// Builds a long request; `params` is zero-padded to 16 bytes.
    pub fn request(
        device_index: u8,
        feature_index: u8,
        function: u8,
        sw_id: u8,
        params: &[u8],
    ) -> Result<Self> {
        if params.len() > LONG_LEN - 4 {
            bail!("too many parameters: {}", params.len());
        }
        let mut p = params.to_vec();
        p.resize(LONG_LEN - 4, 0);
        Ok(Self {
            report_id: REPORT_LONG,
            device_index,
            feature_index,
            function: function & 0x0F,
            sw_id: sw_id & 0x0F,
            params: p,
        })
    }

    /// Serializes the message into raw report bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = vec![
            self.report_id,
            self.device_index,
            self.feature_index,
            (self.function << 4) | (self.sw_id & 0x0F),
        ];
        b.extend_from_slice(&self.params);
        b
    }

    /// Parses raw report bytes; fails on unknown report ID or wrong length.
    pub fn parse(raw: &[u8]) -> Result<Self> {
        let Some(expected) = raw.first().and_then(|id| report_len(*id)) else {
            bail!("not a HID++ report");
        };
        if raw.len() < expected {
            bail!("report too short: {} (expected {expected})", raw.len());
        }
        Ok(Self {
            report_id: raw[0],
            device_index: raw[1],
            feature_index: raw[2],
            function: raw[3] >> 4,
            sw_id: raw[3] & 0x0F,
            params: raw[4..expected].to_vec(),
        })
    }

    /// If this is an error response, returns the decoded error.
    pub fn as_error(&self) -> Option<HidppError> {
        let version = match self.feature_index {
            // V2: byte 3 = feature index of the failed request, then
            // function/sw ID, then the error code.
            ERR_FEATURE_V2 => ErrorKind::V2,
            // V1: byte 3 = sub-ID of the failed request (the feature index),
            // then address (function/sw ID), then the error code.
            ERR_SUB_ID_V1 => ErrorKind::V1,
            _ => return None,
        };
        let p = &self.params;
        Some(HidppError {
            version,
            feature_index: (self.function << 4) | self.sw_id,
            function: *p.first()? >> 4,
            sw_id: *p.first()? & 0x0F,
            code: *p.get(1)?,
        })
    }

    /// True if this message answers `req` (normal response or error).
    pub fn answers(&self, req: &Message) -> bool {
        if self.device_index != req.device_index {
            return false;
        }
        if let Some(e) = self.as_error() {
            return e.feature_index == req.feature_index
                && e.function == req.function
                && e.sw_id == req.sw_id;
        }
        self.feature_index == req.feature_index
            && self.function == req.function
            && self.sw_id == req.sw_id
    }
}

/// Which protocol generation produced an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    V1,
    V2,
}

/// An error reported by the device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HidppError {
    pub version: ErrorKind,
    pub feature_index: u8,
    pub function: u8,
    pub sw_id: u8,
    pub code: u8,
}

impl HidppError {
    /// True if the device asked us to try again later.
    pub fn is_busy(&self) -> bool {
        matches!((self.version, self.code), (ErrorKind::V2, 0x08) | (ErrorKind::V1, 0x07))
    }

    /// True if the error means the mouse is not reachable through its
    /// receiver (asleep, switched off or on another host), as opposed to
    /// the mouse rejecting one particular request.
    pub fn is_link_down(&self) -> bool {
        matches!(
            (self.version, self.code),
            (ErrorKind::V1, 0x04 | 0x08 | 0x09)
        )
    }

    /// Human-readable error name.
    pub fn name(&self) -> &'static str {
        match (self.version, self.code) {
            (ErrorKind::V2, 0x00) => "no error",
            (ErrorKind::V2, 0x01) => "unknown",
            (ErrorKind::V2, 0x02) => "invalid argument",
            (ErrorKind::V2, 0x03) => "out of range",
            (ErrorKind::V2, 0x04) => "hardware error",
            (ErrorKind::V2, 0x05) => "Logitech internal error",
            (ErrorKind::V2, 0x06) => "invalid feature index",
            (ErrorKind::V2, 0x07) => "invalid function",
            (ErrorKind::V2, 0x08) => "busy",
            (ErrorKind::V2, 0x09) => "unsupported",
            (ErrorKind::V1, 0x01) => "invalid sub-ID (HID++ 1.0 device)",
            (ErrorKind::V1, 0x02) => "invalid address",
            (ErrorKind::V1, 0x03) => "invalid value",
            (ErrorKind::V1, 0x04) => "connection request failed",
            (ErrorKind::V1, 0x05) => "too many devices",
            (ErrorKind::V1, 0x06) => "already exists",
            (ErrorKind::V1, 0x07) => "busy",
            (ErrorKind::V1, 0x08) => "unknown device",
            (ErrorKind::V1, 0x09) => "resource error",
            (ErrorKind::V1, 0x0A) => "request unavailable",
            (ErrorKind::V1, 0x0B) => "invalid parameter value",
            (ErrorKind::V1, 0x0C) => "wrong PIN code",
            _ => "unrecognized error",
        }
    }
}

impl std::fmt::Display for HidppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HID++ error 0x{:02X}: {}", self.code, self.name())
    }
}

impl std::error::Error for HidppError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_ping_request() {
        let m = Message::request(0xFF, 0x00, 0x1, 0xA, &[0, 0, 0x5B]).unwrap();
        let b = m.to_bytes();
        assert_eq!(b.len(), 20);
        assert_eq!(&b[..7], &[0x11, 0xFF, 0x00, 0x1A, 0, 0, 0x5B]);
        assert!(b[7..].iter().all(|&x| x == 0));
    }

    #[test]
    fn parses_ping_response() {
        let raw = [
            0x11, 0xFF, 0x00, 0x1A, 0x04, 0x02, 0x5B, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let m = Message::parse(&raw).unwrap();
        assert_eq!((m.function, m.sw_id), (1, 0xA));
        assert_eq!(&m.params[..3], &[4, 2, 0x5B]);
        let req = Message::request(0xFF, 0, 1, 0xA, &[0, 0, 0x5B]).unwrap();
        assert!(m.answers(&req));
        assert_eq!(Message::parse(&m.to_bytes()).unwrap(), m);
    }

    #[test]
    fn reads_payload_bytes() {
        assert_eq!(byte(&[1, 2], 1), 2);
        assert_eq!(byte(&[1, 2], 2), 0);
        assert_eq!(be_u16(&[0x12, 0x34], 0), 0x1234);
        assert_eq!(be_u16(&[0x12], 0), 0x1200);
    }

    #[test]
    fn rejects_bad_reports() {
        assert!(Message::parse(&[0x20, 0, 0, 0]).is_err());
        assert!(Message::parse(&[0x11, 0, 0]).is_err());
        assert!(Message::parse(&[]).is_err());
    }

    #[test]
    fn parses_v2_error() {
        let raw = [0x11, 0x01, 0xFF, 0x05, 0x31, 0x07, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let m = Message::parse(&raw).unwrap();
        let e = m.as_error().unwrap();
        assert_eq!(e.code, 7);
        assert_eq!(e.name(), "invalid function");
        assert_eq!((e.feature_index, e.function, e.sw_id), (0x05, 3, 1));
        let req = Message::request(0x01, 0x05, 3, 1, &[]).unwrap();
        assert!(m.answers(&req));
        let other = Message::request(0x01, 0x05, 3, 2, &[]).unwrap();
        assert!(!m.answers(&other));
    }

    #[test]
    fn parses_v1_error_for_ping() {
        // HID++ 1.0 device answering a HID++ 2.0 ping: invalid sub-ID.
        let raw = [0x10, 0x01, 0x8F, 0x00, 0x1A, 0x01, 0x00];
        let m = Message::parse(&raw).unwrap();
        let e = m.as_error().unwrap();
        assert_eq!(e.version, ErrorKind::V1);
        assert_eq!(e.code, 1);
        let req = Message::request(0x01, 0, 1, 0xA, &[]).unwrap();
        assert!(m.answers(&req));
    }

    #[test]
    fn classifies_errors() {
        let e = |version, code| HidppError { version, feature_index: 0, function: 0, sw_id: 0, code };
        assert!(e(ErrorKind::V2, 0x08).is_busy());
        assert!(!e(ErrorKind::V2, 0x02).is_busy());
        assert!(e(ErrorKind::V1, 0x09).is_link_down());
        assert!(!e(ErrorKind::V2, 0x09).is_link_down()); // "unsupported" is not a link problem
    }

    #[test]
    fn short_error_reports_do_not_panic() {
        // Truncated error payloads must decode to None, never panic.
        for len in 4..=7 {
            let mut raw = vec![0x10, 0x01, 0xFF, 0x05];
            raw.resize(len, 0);
            let _ = Message::parse(&raw).map(|m| m.as_error());
        }
        let m = Message { report_id: 0x10, device_index: 1, feature_index: 0xFF, function: 0, sw_id: 0, params: vec![] };
        assert!(m.as_error().is_none());
        assert!(!m.answers(&Message::request(1, 0, 0, 1, &[]).unwrap()));
    }

    #[test]
    fn ignores_other_device_and_notifications() {
        let req = Message::request(0x01, 0x04, 0, 0xA, &[]).unwrap();
        let n = Message::parse(&[0x11, 0x01, 0x04, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        assert!(!n.answers(&req)); // function 1 / sw_id 0: a notification
        let other = Message::parse(&[0x11, 0x02, 0x04, 0x0A, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        assert!(!other.answers(&req));
    }
}
