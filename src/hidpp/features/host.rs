//! Easy-Switch channels: ChangeHost (0x1814) / HostsInfo (0x1815).

use anyhow::{Result, bail};
use rust_i18n::t;
use serde::{Deserialize, Serialize};

use crate::hidpp::device::Device;
use crate::hidpp::message::byte;

/// Feature ID of ChangeHost.
pub const ID: u16 = 0x1814;
/// Feature ID of the older HostsInfo.
pub const LEGACY_ID: u16 = 0x1815;

/// Function numbers of both features (getHostInfo / getFeatureInfo) and of ChangeHost.
const GET_INFO: u8 = 0;
const SET_HOST: u8 = 1;

/// Number of Easy-Switch channels and the currently active one (0-based).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hosts {
    pub count: u8,
    pub current: u8,
}

/// Parses getHostInfo of 0x1814: [count, current host, ...].
pub fn parse_change_host(p: &[u8]) -> Hosts {
    Hosts {
        count: byte(p, 0),
        current: byte(p, 1),
    }
}

/// Parses getFeatureInfo of 0x1815: [capabilities, count, current host, ...].
pub fn parse_hosts_info(p: &[u8]) -> Hosts {
    Hosts {
        count: byte(p, 1),
        current: byte(p, 2),
    }
}

/// Reads channel info; `None` if neither feature is supported.
pub fn read(dev: &Device) -> Result<Option<Hosts>> {
    if let Some(idx) = dev.feature(ID)? {
        return Ok(Some(parse_change_host(&dev.call(idx, GET_INFO, &[])?)));
    }
    if let Some(idx) = dev.feature(LEGACY_ID)? {
        return Ok(Some(parse_hosts_info(&dev.call(idx, GET_INFO, &[])?)));
    }
    Ok(None)
}

/// Encodes setHost (0x1814 function 1) parameters; `host` is 0-based.
pub fn encode_set_host(host: u8) -> [u8; 1] {
    [host]
}

/// Switches to Easy-Switch channel `channel` (1-based). The mouse drops the link,
/// so no response is awaited. Returns the 1-based channel.
pub fn switch(dev: &Device, channel: u8) -> Result<u8> {
    let idx = dev.require(ID, "host.unsupported")?;
    let hosts = parse_change_host(&dev.call(idx, GET_INFO, &[])?);
    if channel == 0 || channel > hosts.count {
        bail!(t!("host.invalid", value = channel, count = hosts.count));
    }
    dev.send(idx, SET_HOST, &encode_set_host(channel - 1))?;
    Ok(channel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn change_host() {
        let h = parse_change_host(&[3, 1, 0, 0]);
        assert_eq!(h, Hosts { count: 3, current: 1 });
    }

    #[test]
    fn hosts_info() {
        let h = parse_hosts_info(&[0x01, 3, 2, 0]);
        assert_eq!(h, Hosts { count: 3, current: 2 });
    }

    #[test]
    fn encodes_set_host() {
        assert_eq!(encode_set_host(2), [2]);
    }
}
