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
/// HostsInfo (0x1815) getHostInfo: parameter `[host]`.
const GET_HOST_INFO: u8 = 1;
/// HostsInfo (0x1815) getHostFriendlyName: parameters `[host, byte offset]`.
const GET_HOST_NAME: u8 = 3;
/// Upper bound on a name length and on chunks read, so a bad device cannot loop forever.
const MAX_NAME_LEN: usize = 64;
const MAX_CHUNKS: usize = 16;
/// Upper bound on channels whose names are read.
const MAX_HOSTS: u8 = 8;

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

/// Parses getHostInfo of 0x1815 (layout as in Solaar):
/// `[host, status, bus type, reserved, name length, ...]`.
/// Returns the friendly-name length; `None` for an empty (unpaired) channel
/// (status 0 or no name).
pub fn parse_host_name_len(p: &[u8]) -> Option<usize> {
    let status = p.get(1).copied()?;
    let len = usize::from(byte(p, 4));
    (status != 0 && len != 0).then_some(len.min(MAX_NAME_LEN))
}

/// Parses one getHostFriendlyName response: `[host, offset, name bytes...]`.
/// A response without that echo header is taken as raw name bytes.
pub fn parse_name_chunk(p: &[u8], host: u8, offset: usize) -> &[u8] {
    if p.len() >= 2 && p[0] == host && usize::from(p[1]) == offset {
        &p[2..]
    } else {
        p
    }
}

/// Joins name chunks into text: stops at the first NUL, cuts to `len` bytes.
/// `None` if nothing readable is left.
pub fn assemble_name(bytes: &[u8], len: usize) -> Option<String> {
    let bytes = &bytes[..bytes.len().min(len)];
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let name = String::from_utf8_lossy(&bytes[..end]).trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// Reads the friendly names of the channels (index = channel - 1). READ ONLY.
/// `None` per empty channel; an empty list if HostsInfo is not supported.
pub fn read_names(dev: &Device, count: u8) -> Result<Vec<Option<String>>> {
    let Some(idx) = dev.feature(LEGACY_ID)? else {
        return Ok(Vec::new());
    };
    let mut names = Vec::new();
    for host in 0..count.min(MAX_HOSTS) {
        let Some(len) = parse_host_name_len(&dev.call(idx, GET_HOST_INFO, &[host])?) else {
            names.push(None);
            continue;
        };
        let mut bytes: Vec<u8> = Vec::new();
        for _ in 0..MAX_CHUNKS {
            if bytes.len() >= len {
                break;
            }
            let r = dev.call(idx, GET_HOST_NAME, &[host, bytes.len() as u8])?;
            let chunk = parse_name_chunk(&r, host, bytes.len());
            if chunk.is_empty() {
                break;
            }
            bytes.extend_from_slice(chunk);
            if chunk.contains(&0) {
                break;
            }
        }
        names.push(assemble_name(&bytes, len));
    }
    Ok(names)
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
    fn name_length_and_empty_channel() {
        assert_eq!(parse_host_name_len(&[0, 1, 2, 0, 9, 0]), Some(9));
        assert_eq!(parse_host_name_len(&[0, 0, 0, 0, 0, 0]), None);
        assert_eq!(parse_host_name_len(&[0, 1, 2, 0, 0]), None);
        assert_eq!(parse_host_name_len(&[0, 1, 2, 0, 255]), Some(MAX_NAME_LEN));
    }

    #[test]
    fn short_and_garbage_responses() {
        assert_eq!(parse_host_name_len(&[]), None);
        assert_eq!(parse_host_name_len(&[1]), None);
        assert_eq!(parse_host_name_len(&[0, 1]), None);
        assert!(parse_name_chunk(&[], 0, 0).is_empty());
        assert!(parse_name_chunk(&[0], 0, 0).len() == 1);
        assert_eq!(assemble_name(&[], 5), None);
        assert_eq!(assemble_name(&[0, b'a'], 5), None);
        assert_eq!(assemble_name(&[0xFF, 0xFE], 2).map(|s| s.is_empty()), Some(false));
    }

    #[test]
    fn name_in_several_chunks() {
        let a = parse_name_chunk(&[1, 0, b'M', b'a', b'c', b'B'], 1, 0);
        assert_eq!(a, b"MacB");
        let b = parse_name_chunk(&[1, 4, b'o', b'o', b'k', 0, 0], 1, 4);
        let mut all = a.to_vec();
        all.extend_from_slice(b);
        assert_eq!(assemble_name(&all, 7).as_deref(), Some("MacBook"));
        // Cut to the announced length.
        assert_eq!(assemble_name(b"MacBookXX", 7).as_deref(), Some("MacBook"));
    }

    #[test]
    fn encodes_set_host() {
        assert_eq!(encode_set_host(2), [2]);
    }
}
