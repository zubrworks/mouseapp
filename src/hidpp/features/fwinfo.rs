//! DeviceFWVersion (0x0003): the unit ID, identical for one physical mouse
//! over every connection (Bolt, Bluetooth). Used to tell mice apart.

use anyhow::Result;

use crate::hidpp::device::Device;

pub const ID: u16 = 0x0003;

const GET_DEVICE_INFO: u8 = 0;

/// Extracts the unit ID from a getDeviceInfo response:
/// `[entity count, unit id (4 bytes), ...]`. `None` if absent or not set.
pub fn parse_unit_id(p: &[u8]) -> Option<[u8; 4]> {
    let id: [u8; 4] = p.get(1..5)?.try_into().ok()?;
    (id != [0; 4] && id != [0xFF; 4]).then_some(id)
}

/// Reads the unit ID; `None` if the device does not report one.
pub fn read_unit_id(dev: &Device) -> Result<Option<[u8; 4]>> {
    let Some(idx) = dev.feature(ID)? else {
        return Ok(None);
    };
    Ok(parse_unit_id(&dev.call(idx, GET_DEVICE_INFO, &[])?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unit_id() {
        assert_eq!(parse_unit_id(&[3, 0xDE, 0xAD, 0xBE, 0xEF, 0, 1]), Some([0xDE, 0xAD, 0xBE, 0xEF]));
        assert_eq!(parse_unit_id(&[3, 0, 0, 0, 0]), None);
        assert_eq!(parse_unit_id(&[3, 0xFF, 0xFF, 0xFF, 0xFF]), None);
        assert_eq!(parse_unit_id(&[3, 1, 2]), None);
        assert_eq!(parse_unit_id(&[]), None);
    }
}
