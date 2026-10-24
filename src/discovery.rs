//! Finds Logitech devices over Bolt, Unifying or Bluetooth.

use std::ffi::CString;

use anyhow::Result;
use hidapi::{BusType, DeviceInfo, HidApi};
use rust_i18n::t;

/// Logitech USB vendor ID.
const VID_LOGITECH: u16 = 0x046D;
/// Bolt receiver product ID.
const PID_BOLT: u16 = 0xC548;
/// Unifying receiver product ID.
const PID_UNIFYING: u16 = 0xC52B;

/// Vendor-specific usage pages that carry HID++ on macOS.
const USAGE_PAGE_RECEIVER: u16 = 0xFF00;
const USAGE_PAGE_BLE: u16 = 0xFF43;

/// How a device is connected to the Mac.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Connection {
    Bolt,
    Unifying,
    Bluetooth,
    Usb,
}

impl Connection {
    /// Localized label.
    pub fn label(self) -> String {
        match self {
            Connection::Bolt => t!("connection.bolt"),
            Connection::Unifying => t!("connection.unifying"),
            Connection::Bluetooth => t!("connection.bluetooth"),
            Connection::Usb => t!("connection.usb"),
        }
        .to_string()
    }
}

/// A HID++ capable interface found on the system.
#[derive(Clone, Debug)]
pub struct Found {
    pub name: String,
    pub pid: u16,
    pub connection: Connection,
    pub usage_page: u16,
    pub usage: u16,
    pub path: CString,
}

/// Classifies a HID interface; returns `None` if it is not a HID++ interface.
fn classify(info: &DeviceInfo) -> Option<Connection> {
    if info.vendor_id() != VID_LOGITECH {
        return None;
    }
    let page = info.usage_page();
    if page != USAGE_PAGE_RECEIVER && page != USAGE_PAGE_BLE {
        return None;
    }
    Some(match (info.product_id(), info.bus_type()) {
        (PID_BOLT, _) => Connection::Bolt,
        (PID_UNIFYING, _) => Connection::Unifying,
        (_, BusType::Bluetooth) => Connection::Bluetooth,
        _ => Connection::Usb,
    })
}

/// Re-reads the device list with Logitech devices only. hidapi keeps every
/// device of the last listing alive (with its properties), so listing all
/// HID devices of the Mac would hold dozens of them in memory.
pub fn refresh_logitech(api: &mut HidApi) -> hidapi::HidResult<()> {
    api.reset_devices()?;
    api.add_devices(VID_LOGITECH, 0)
}

/// Lists all HID++ interfaces (long-report ones are sorted first per device).
pub fn find_devices(api: &HidApi) -> Vec<Found> {
    let mut out: Vec<Found> = api
        .device_list()
        .filter_map(|info| {
            let connection = classify(info)?;
            Some(Found {
                name: info
                    .product_string()
                    .unwrap_or("Logitech device")
                    .to_string(),
                pid: info.product_id(),
                connection,
                usage_page: info.usage_page(),
                usage: info.usage(),
                path: info.path().to_owned(),
            })
        })
        .collect();
    out.sort_by_key(|d| (d.pid, !is_long_interface(d)));
    out
}

/// True for the interface that carries long (0x11) reports.
pub fn is_long_interface(d: &Found) -> bool {
    matches!(
        (d.usage_page, d.usage),
        (USAGE_PAGE_RECEIVER, 0x0002) | (USAGE_PAGE_BLE, 0x0202)
    )
}

/// Prints found devices for `mouse list`.
pub fn print_list() -> Result<()> {
    let api = HidApi::new()?;
    // Never grab devices exclusively: it freezes the cursor on macOS.
    api.set_open_exclusive(false);
    let devices: Vec<Found> = find_devices(&api)
        .into_iter()
        .filter(is_long_interface)
        .collect();
    if devices.is_empty() {
        println!("{}", t!("list.none"));
        return Ok(());
    }
    for d in devices {
        println!(
            "{}",
            t!(
                "list.row",
                name = d.name,
                pid = format!("{:04X}", d.pid),
                connection = d.connection.label()
            )
        );
    }
    Ok(())
}
