//! DeviceFWVersion (0x0003): the unit ID, identical for one physical mouse
//! over every connection (Bolt, Bluetooth). Used to tell mice apart.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::hidpp::device::Device;
use crate::hidpp::message::byte;

pub const ID: u16 = 0x0003;

const GET_DEVICE_INFO: u8 = 0;
const GET_FW_INFO: u8 = 1;
const GET_SERIAL: u8 = 2;
/// First feature version that has getDeviceSerialNumber.
const SERIAL_MIN_VERSION: u8 = 4;
/// Capability bit of getDeviceInfo: the serial number is available.
const CAP_SERIAL: u8 = 0x01;
/// Upper bound on entities, so a garbage count cannot cause many calls.
const MAX_ENTITIES: u8 = 16;

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

/// Kind of a firmware entity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntityKind {
    Firmware,
    Bootloader,
    Hardware,
    Other,
}

/// One firmware entity (application, bootloader, hardware...).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entity {
    pub kind: EntityKind,
    /// Version text, e.g. `RBM 14.01.B0015`.
    pub version: String,
    pub active: bool,
}

/// Static device data from DeviceFWVersion.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceDetails {
    pub unit_id: Option<[u8; 4]>,
    /// Non-zero model IDs (up to three), e.g. `B034`.
    pub model_ids: Vec<u16>,
    pub entities: Vec<Entity>,
    pub serial: Option<String>,
}

impl DeviceDetails {
    /// Version of the first entity of `kind`, preferring the active one.
    pub fn version(&self, kind: EntityKind) -> Option<&str> {
        let mut of_kind = self.entities.iter().filter(|e| e.kind == kind);
        let first = of_kind.next()?;
        let best = std::iter::once(first)
            .chain(of_kind)
            .find(|e| e.active)
            .unwrap_or(first);
        Some(&best.version)
    }

    /// Model IDs as hex, comma separated (`B034`); `None` if there are none.
    pub fn model_ids_text(&self) -> Option<String> {
        let ids = self.model_ids.iter().map(|m| format!("{m:04X}")).collect::<Vec<_>>().join(", ");
        (!ids.is_empty()).then_some(ids)
    }
}

/// Parsed getDeviceInfo: `(entity count, unit id, model ids, has serial)`.
type Info = (u8, Option<[u8; 4]>, Vec<u16>, bool);

/// Parses getDeviceInfo: `[count, unit id (4), transport (2), model ids
/// (3 x 2), extended model id, capabilities]`. `None` if too short.
fn parse_device_info(p: &[u8]) -> Option<Info> {
    if p.is_empty() {
        return None;
    }
    let model_ids = (0..3)
        .filter_map(|i| {
            Some(u16::from_be_bytes(
                p.get(7 + 2 * i..9 + 2 * i)?.try_into().ok()?,
            ))
        })
        .filter(|&m| m != 0)
        .collect();
    Some((
        p[0],
        parse_unit_id(p),
        model_ids,
        byte(p, 14) & CAP_SERIAL != 0,
    ))
}

/// Parses getFwInfo: `[type, prefix (3), number, revision, build (2, BCD),
/// active, transport pid (2)]`. `None` if too short or the entity is unused.
fn parse_fw_info(p: &[u8]) -> Option<Entity> {
    if p.len() < 9 {
        return None;
    }
    let kind = match p[0] & 0x0F {
        0 => EntityKind::Firmware,
        1 => EntityKind::Bootloader,
        2 => EntityKind::Hardware,
        _ => EntityKind::Other,
    };
    let prefix: String = p[1..4]
        .iter()
        .map(|&b| if b.is_ascii_graphic() { b as char } else { ' ' })
        .collect::<String>()
        .trim()
        .to_string();
    let build = u16::from_be_bytes([p[6], p[7]]);
    // Hardware entities carry only a revision number in byte 1 (as Solaar reads it).
    let version = if kind == EntityKind::Hardware {
        p[1].to_string()
    } else if prefix.is_empty() && p[4] == 0 && p[5] == 0 && build == 0 {
        return None;
    } else {
        let mut v = format!("{prefix} {:02X}.{:02X}", p[4], p[5]);
        if build != 0 {
            v.push_str(&format!(".B{build:04X}"));
        }
        v.trim().to_string()
    };
    Some(Entity {
        kind,
        version,
        active: p[8] & 1 != 0,
    })
}

/// Serial number: up to 12 printable ASCII characters. `None` if empty.
fn parse_serial(p: &[u8]) -> Option<String> {
    let s: String = p
        .iter()
        .take(12)
        .take_while(|b| b.is_ascii_graphic())
        .map(|&b| b as char)
        .collect();
    (!s.is_empty()).then_some(s)
}

/// Reads firmware, hardware, model and serial data; `None` if the device
/// has no DeviceFWVersion. Read-only. A failing serial number read is
/// logged and leaves `serial` empty.
pub fn read(dev: &Device) -> Result<Option<DeviceDetails>> {
    let Some(idx) = dev.feature(ID)? else {
        return Ok(None);
    };
    let Some((count, unit_id, model_ids, has_serial)) =
        parse_device_info(&dev.call(idx, GET_DEVICE_INFO, &[])?)
    else {
        return Ok(None);
    };
    let mut entities = Vec::new();
    for i in 0..count.min(MAX_ENTITIES) {
        match dev.call(idx, GET_FW_INFO, &[i]) {
            Ok(r) => entities.extend(parse_fw_info(&r)),
            Err(e) => log::debug!("cannot read firmware entity {i}: {e}"),
        }
    }
    let version = dev
        .feature_table()
        .ok()
        .and_then(|t| t.iter().find(|f| f.id == ID).map(|f| f.version))
        .unwrap_or(0);
    let serial = if has_serial && version >= SERIAL_MIN_VERSION {
        match dev.call(idx, GET_SERIAL, &[]) {
            Ok(r) => parse_serial(&r),
            Err(e) => {
                log::debug!("cannot read serial number: {e}");
                None
            }
        }
    } else {
        None
    };
    Ok(Some(DeviceDetails {
        unit_id,
        model_ids,
        entities,
        serial,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unit_id() {
        assert_eq!(
            parse_unit_id(&[3, 0xDE, 0xAD, 0xBE, 0xEF, 0, 1]),
            Some([0xDE, 0xAD, 0xBE, 0xEF])
        );
        assert_eq!(parse_unit_id(&[3, 0, 0, 0, 0]), None);
        assert_eq!(parse_unit_id(&[3, 0xFF, 0xFF, 0xFF, 0xFF]), None);
        assert_eq!(parse_unit_id(&[3, 1, 2]), None);
        assert_eq!(parse_unit_id(&[]), None);
    }

    #[test]
    fn parses_device_info() {
        let mut p = vec![
            3, 0xDE, 0xAD, 0xBE, 0xEF, 0, 1, 0xB0, 0x34, 0, 0, 0, 0, 0, 1,
        ];
        let (n, id, models, serial) = parse_device_info(&p).unwrap();
        assert_eq!(
            (n, id, models, serial),
            (3, Some([0xDE, 0xAD, 0xBE, 0xEF]), vec![0xB034], true)
        );
        p.truncate(5);
        let (n, _, models, serial) = parse_device_info(&p).unwrap();
        assert_eq!((n, models, serial), (3, vec![], false));
        assert_eq!(parse_device_info(&[]), None);
    }

    #[test]
    fn parses_fw_info() {
        let fw = [0, b'R', b'B', b'M', 0x14, 0x01, 0x00, 0x15, 1, 0xB0, 0x34];
        let e = parse_fw_info(&fw).unwrap();
        assert_eq!(
            e,
            Entity {
                kind: EntityKind::Firmware,
                version: "RBM 14.01.B0015".into(),
                active: true
            }
        );
        let boot = [1, b'B', b'L', b'S', 0x02, 0x10, 0, 0, 0];
        let e = parse_fw_info(&boot).unwrap();
        assert_eq!(
            (e.kind, e.version.as_str(), e.active),
            (EntityKind::Bootloader, "BLS 02.10", false)
        );
        let hw = [2, 3, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(parse_fw_info(&hw).unwrap().version, "3");
        assert_eq!(parse_fw_info(&[0; 9]), None);
        assert_eq!(parse_fw_info(&[0, b'R', b'B']), None);
        assert_eq!(parse_fw_info(&[]), None);
        assert!(parse_fw_info(&[0xFF; 12]).is_some());
    }

    #[test]
    fn parses_serial() {
        assert_eq!(parse_serial(b"2234AB567890xx"), Some("2234AB567890".into()));
        assert_eq!(parse_serial(&[0; 12]), None);
        assert_eq!(parse_serial(&[]), None);
    }

    #[test]
    fn picks_active_version() {
        let e = |v: &str, active| Entity {
            kind: EntityKind::Firmware,
            version: v.into(),
            active,
        };
        let d = DeviceDetails {
            entities: vec![e("A", false), e("B", true)],
            ..Default::default()
        };
        assert_eq!(d.version(EntityKind::Firmware), Some("B"));
        assert_eq!(d.version(EntityKind::Hardware), None);
    }
}
