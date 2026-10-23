//! CLI handlers for reading and writing mouse settings.

use anyhow::{Result, bail};
use clap::ValueEnum;
use rust_i18n::t;

use crate::hidpp::device::Device;
use crate::hidpp::features::{dpi, host, smartshift, thumbwheel, wheel};

/// `on` / `off` command-line value.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum OnOff {
    On,
    Off,
}

impl OnOff {
    fn get(self) -> bool {
        matches!(self, OnOff::On)
    }
}

fn onoff(v: bool) -> String {
    if v { t!("common.on") } else { t!("common.off") }.to_string()
}

/// `mouse dpi [VALUE]`
pub fn dpi(value: Option<u16>) -> Result<()> {
    let dev = Device::open_default()?;
    match value {
        Some(v) => {
            let d = dpi::write(&dev, v)?;
            println!("{}", t!("dpi.set", value = d.current));
        }
        None => match (dpi::read(&dev)?, dpi::read_list(&dev)?) {
            (Some(d), Some(l)) => println!(
                "{}",
                t!(
                    "dpi.current",
                    value = d.current,
                    default = d.default,
                    min = l.min(),
                    max = l.max()
                )
            ),
            _ => println!("{}", t!("dpi.unsupported")),
        },
    }
    Ok(())
}

fn print_smartshift(s: smartshift::SmartShift) {
    let state = if !s.ratchet {
        t!("smartshift.state_free")
    } else if s.threshold == smartshift::ALWAYS_RATCHET {
        t!("smartshift.state_off")
    } else {
        t!("smartshift.state_on", threshold = s.threshold)
    };
    println!("{}", t!("smartshift.current", state = state));
    println!("{}", t!("smartshift.default", value = s.default));
}

/// `mouse smartshift [free|1-255]`
pub fn smartshift(value: Option<String>) -> Result<()> {
    let dev = Device::open_default()?;
    match value {
        Some(v) => {
            let arg = parse_arg(&v)?;
            print_smartshift(smartshift::write(&dev, resolve(arg))?);
        }
        None => match smartshift::read(&dev)? {
            Some(s) => print_smartshift(s),
            None => println!("{}", t!("smartshift.unsupported")),
        },
    }
    Ok(())
}

/// `mouse wheel [--hires on|off] [--ratchet on|off]`
pub fn wheel(hires: Option<OnOff>, ratchet: Option<OnOff>) -> Result<()> {
    let dev = Device::open_default()?;
    let change = wheel::Change {
        hires: hires.map(OnOff::get),
    };
    if change.hires.is_some() {
        wheel::write(&dev, change)?;
    }
    if let Some(r) = ratchet {
        smartshift::write(
            &dev,
            smartshift::Change {
                ratchet: Some(r.get()),
                threshold: None,
            },
        )?;
    }
    match wheel::read(&dev)? {
        Some((_, m)) => println!(
            "{}",
            t!("wheel.current", hires = onoff(m.hires))
        ),
        None => println!("{}", t!("wheel.unsupported")),
    }
    if let Some(s) = smartshift::read(&dev)? {
        println!("{}", t!("wheel.ratchet", state = onoff(s.ratchet)));
    }
    Ok(())
}

/// `mouse thumbwheel [--invert on|off] [--divert on|off]`
pub fn thumbwheel(invert: Option<OnOff>, divert: Option<OnOff>) -> Result<()> {
    let dev = Device::open_default()?;
    let change = thumbwheel::Change {
        diverted: divert.map(OnOff::get),
        invert: invert.map(OnOff::get),
    };
    let state = if change.diverted.is_some() || change.invert.is_some() {
        Some(thumbwheel::write(&dev, change)?)
    } else {
        thumbwheel::read(&dev)?
    };
    match state {
        Some(s) => println!(
            "{}",
            t!(
                "thumbwheel.current",
                invert = onoff(s.invert),
                divert = onoff(s.diverted)
            )
        ),
        None => println!("{}", t!("thumbwheel.unsupported")),
    }
    if let Some(idx) = dev.feature(thumbwheel::ID)?
        && let Ok(i) = thumbwheel::read_info(&dev, idx)
    {
        println!(
            "{}",
            t!(
                "thumbwheel.info",
                native = i.native_res,
                diverted = i.diverted_res,
                right = onoff(i.positive_right)
            )
        );
    }
    Ok(())
}

/// `mouse host [N]`
pub fn host(channel: Option<u8>) -> Result<()> {
    let dev = Device::open_default()?;
    match channel {
        Some(n) => {
            let n = host::switch(&dev, n)?;
            println!("{}", t!("host.switched", channel = n));
        }
        None => match host::read(&dev)? {
            Some(h) => println!(
                "{}",
                t!(
                    "host.current",
                    channel = h.current as u16 + 1,
                    count = h.count
                )
            ),
            None => println!("{}", t!("host.unsupported")),
        },
    }
    Ok(())
}

/// Argument of `mouse smartshift`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arg {
    Free,
    Threshold(u8),
}

/// Parses `free` or a threshold 1-255.
pub fn parse_arg(s: &str) -> Result<Arg> {
    match s.to_ascii_lowercase().as_str() {
        "free" => Ok(Arg::Free),
        other => match other.parse::<u8>() {
            Ok(n) if n >= 1 => Ok(Arg::Threshold(n)),
            _ => bail!(t!("smartshift.invalid", value = s)),
        },
    }
}

/// Resolves a CLI argument into a change; a threshold also switches to ratchet
/// mode (the daemon writes only the threshold, see `daemon::smartshift_change`).
pub fn resolve(arg: Arg) -> smartshift::Change {
    match arg {
        Arg::Free => smartshift::Change {
            ratchet: Some(false),
            threshold: None,
        },
        Arg::Threshold(n) => smartshift::Change {
            ratchet: Some(true),
            threshold: Some(n),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_args() {
        assert_eq!(parse_arg("FREE").unwrap(), Arg::Free);
        assert_eq!(parse_arg("1").unwrap(), Arg::Threshold(1));
        assert_eq!(parse_arg("255").unwrap(), Arg::Threshold(255));
        assert!(parse_arg("0").is_err());
        assert!(parse_arg("256").is_err());
        assert!(parse_arg("abc").is_err());
        assert!(parse_arg("on").is_err());
        assert!(parse_arg("off").is_err());
    }

    #[test]
    fn resolves_args() {
        assert_eq!(
            resolve(Arg::Free),
            smartshift::Change {
                ratchet: Some(false),
                threshold: None
            }
        );
        assert_eq!(
            resolve(Arg::Threshold(40)),
            smartshift::Change {
                ratchet: Some(true),
                threshold: Some(40)
            }
        );
    }
}
