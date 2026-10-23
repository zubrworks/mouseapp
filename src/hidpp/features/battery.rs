//! Battery: UnifiedBattery (0x1004), fallback BatteryStatus (0x1000).

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::hidpp::device::Device;
use crate::hidpp::message::byte;

/// Feature ID of UnifiedBattery.
pub const ID: u16 = 0x1004;
/// Feature ID of the older BatteryStatus.
pub const LEGACY_ID: u16 = 0x1000;

/// UnifiedBattery functions.
const UNIFIED_GET_STATUS: u8 = 1;
/// BatteryStatus functions.
const LEGACY_GET_LEVEL_STATUS: u8 = 0;

/// Charging state of the battery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Charge {
    Discharging,
    Charging,
    Full,
    Error,
    Other,
}

/// Battery reading. `percent` is `None` when the device does not report a
/// percentage (it sends 0 and only a coarse level).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Battery {
    pub percent: Option<u8>,
    pub charge: Charge,
}

/// The percentage in the first byte; 0 or a missing byte means "not reported".
fn percent(p: &[u8]) -> Option<u8> {
    Some(byte(p, 0)).filter(|&v| v != 0)
}

/// Status byte -> charging state: (code, state, only in the legacy feature).
const CHARGE_STATES: &[(u8, Charge, bool)] = &[
    (0, Charge::Discharging, false),
    (1, Charge::Charging, false),
    (2, Charge::Charging, false),
    (3, Charge::Full, false),
    (4, Charge::Charging, false),
    (5, Charge::Error, false),
    (6, Charge::Error, false),
    (7, Charge::Error, true),
];

/// Charging state for the status byte `status` (`None` = missing).
fn charge(status: Option<u8>, legacy: bool) -> Charge {
    status
        .and_then(|s| {
            CHARGE_STATES
                .iter()
                .find(|(code, _, legacy_only)| *code == s && (legacy || !legacy_only))
        })
        .map_or(Charge::Other, |(_, c, _)| *c)
}

/// Parses getStatus of 0x1004: [soc %, level flags, status, ext power].
pub fn parse_unified(p: &[u8]) -> Battery {
    Battery {
        percent: percent(p),
        charge: charge(p.get(2).copied(), false),
    }
}

/// Parses getBatteryLevelStatus of 0x1000: [level %, next level %, status].
pub fn parse_legacy(p: &[u8]) -> Battery {
    Battery {
        percent: percent(p),
        charge: charge(p.get(2).copied(), true),
    }
}

/// Reads the battery; `None` if neither feature is supported.
pub fn read(dev: &Device) -> Result<Option<Battery>> {
    if let Some(idx) = dev.feature(ID)? {
        return Ok(Some(parse_unified(&dev.call(idx, UNIFIED_GET_STATUS, &[])?)));
    }
    if let Some(idx) = dev.feature(LEGACY_ID)? {
        return Ok(Some(parse_legacy(&dev.call(idx, LEGACY_GET_LEVEL_STATUS, &[])?)));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_discharging() {
        let b = parse_unified(&[75, 0x08, 0, 0, 0]);
        assert_eq!(b.percent, Some(75));
        assert_eq!(b.charge, Charge::Discharging);
    }

    #[test]
    fn levels_only_has_no_percent() {
        assert_eq!(parse_unified(&[0, 0x08, 0, 0]).percent, None);
        assert_eq!(parse_unified(&[]).percent, None);
        assert_eq!(parse_legacy(&[0, 0, 0]).percent, None);
    }

    #[test]
    fn unified_charging_and_full() {
        assert_eq!(parse_unified(&[40, 0x04, 1, 1]).charge, Charge::Charging);
        assert_eq!(parse_unified(&[100, 0x08, 3, 1]).charge, Charge::Full);
    }

    #[test]
    fn legacy_status() {
        let b = parse_legacy(&[60, 50, 0]);
        assert_eq!(b.percent, Some(60));
        assert_eq!(b.charge, Charge::Discharging);
        assert_eq!(parse_legacy(&[90, 0, 1]).charge, Charge::Charging);
        // Status 7 is an error only in the legacy feature; a missing byte is unknown.
        assert_eq!(parse_legacy(&[90, 0, 7]).charge, Charge::Error);
        assert_eq!(parse_unified(&[90, 0, 7]).charge, Charge::Other);
        assert_eq!(parse_legacy(&[90, 0]).charge, Charge::Other);
    }
}
