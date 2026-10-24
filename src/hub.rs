//! Meeting point of the daemon's threads: mouse workers publish their status,
//! the socket server reads it and queues the settings app's commands.

use std::collections::VecDeque;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::hidpp::features::battery::Battery;
use crate::discovery::Connection;
use crate::hidpp::features::dpi::DpiList;
use crate::hidpp::features::fwinfo::DeviceDetails;
use crate::hidpp::features::host::Hosts;
use crate::hidpp::features::smartshift::SmartShift;
use crate::util::lock;

/// Static data for the settings window's Device tab. Everything but
/// `hosts.current` is read once per connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub connection: Connection,
    /// Firmware, serial and model IDs (DeviceFWVersion).
    #[serde(default)]
    pub firmware: Option<DeviceDetails>,
    /// Easy-Switch channel count and the current channel (refreshed on every read).
    #[serde(default)]
    pub hosts: Option<Hosts>,
    /// Friendly name per channel (index = channel - 1); empty text for an empty
    /// channel (the TOML status cannot hold `None` in a list).
    #[serde(default)]
    pub host_names: Vec<String>,
}

/// What the daemon knows about one connected mouse.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseInfo {
    /// Stable identity (see `Device::identity`).
    pub key: String,
    pub name: String,
    /// Registry model (`Model::name`) when the mouse is recognized.
    pub model: Option<String>,
    pub battery: Option<Battery>,
    pub dpi: Option<u16>,
    pub dpi_list: Option<DpiList>,
    /// SmartShift state read when the mouse connected.
    pub smartshift: Option<SmartShift>,
    /// Data for the Device tab; absent in replies of older agents.
    #[serde(default)]
    pub details: Option<DeviceInfo>,
}

/// Request to the daemon loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    SetDpi(u16),
    Reload,
}

/// Shared status and request queues.
pub struct Hub {
    mice: Mutex<Vec<MouseInfo>>,
    /// Name of the active profile, set whenever the settings are (re)loaded.
    profile: Mutex<String>,
    commands: Mutex<VecDeque<Command>>,
    /// Wakes the daemon loop after a command is queued.
    on_command: Box<dyn Fn() + Send + Sync>,
}

impl Hub {
    /// `on_command` is called after each queued command.
    pub fn new(on_command: impl Fn() + Send + Sync + 'static) -> Self {
        Hub {
            mice: Mutex::default(),
            profile: Mutex::default(),
            commands: Mutex::default(),
            on_command: Box::new(on_command),
        }
    }

    /// Adds or replaces a mouse (matched by key).
    pub fn publish(&self, info: MouseInfo) {
        let mut mice = lock(&self.mice);
        match mice.iter_mut().find(|m| m.key == info.key) {
            Some(slot) if *slot == info => {}
            Some(slot) => *slot = info,
            None => mice.push(info),
        }
    }

    pub fn remove(&self, key: &str) {
        lock(&self.mice).retain(|m| m.key != key);
    }

    /// The connected mice.
    pub fn mice(&self) -> Vec<MouseInfo> {
        lock(&self.mice).clone()
    }

    /// Name of the active profile.
    pub fn profile(&self) -> String {
        lock(&self.profile).clone()
    }

    pub fn set_profile(&self, name: &str) {
        *lock(&self.profile) = name.to_string();
    }

    /// Queues `c` and wakes the daemon loop.
    pub fn push_command(&self, c: Command) {
        lock(&self.commands).push_back(c);
        (self.on_command)();
    }

    /// Next command for the daemon loop.
    pub fn next_command(&self) -> Option<Command> {
        lock(&self.commands).pop_front()
    }
}
