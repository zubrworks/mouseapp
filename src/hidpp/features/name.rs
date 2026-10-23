//! DeviceNameType (0x0005): reads the device's marketing name.

use anyhow::{Result, bail};

use crate::hidpp::device::Device;

pub const ID: u16 = 0x0005;

const GET_COUNT: u8 = 0;
const GET_NAME_CHUNK: u8 = 1;

/// Extracts the name chunk from a getDeviceName response (NUL-terminated).
pub fn parse_chunk(params: &[u8]) -> Vec<u8> {
    params.iter().copied().take_while(|&b| b != 0).collect()
}

/// Reads the full device name; `None` if the feature is unsupported.
pub fn read(dev: &Device) -> Result<Option<String>> {
    let Some(idx) = dev.feature(ID)? else {
        return Ok(None);
    };
    let len = dev.call(idx, GET_COUNT, &[])?[0] as usize;
    let mut name = Vec::with_capacity(len);
    while name.len() < len {
        let chunk = parse_chunk(&dev.call(idx, GET_NAME_CHUNK, &[name.len() as u8])?);
        if chunk.is_empty() {
            bail!("empty device name chunk");
        }
        name.extend(chunk);
    }
    name.truncate(len);
    Ok(Some(String::from_utf8_lossy(&name).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_stops_at_nul() {
        let mut p = b"MX Master 3S".to_vec();
        p.extend([0, 0, 0, 0]);
        assert_eq!(parse_chunk(&p), b"MX Master 3S");
    }

    #[test]
    fn chunk_full() {
        assert_eq!(parse_chunk(b"0123456789abcdef"), b"0123456789abcdef");
    }
}
