//! Writing the config's settings to a mouse.

use anyhow::Result;

use crate::config::{self, Config};
use crate::hidpp::device::{Device, is_link_failure};
use crate::hidpp::features::{dpi, smartshift, wheel};

/// SmartShift change derived from the config, if any. Only the threshold is
/// written: the wheel mode (ratchet or free-spin) stays as the user set it on
/// the mouse or with `mouse smartshift free`.
fn smartshift_change(c: &config::SmartShift) -> Option<smartshift::Change> {
    c.threshold.map(|t| smartshift::Change {
        ratchet: None,
        threshold: Some(t),
    })
}

/// Runs one setting. A setting that does not fit this mouse or that the
/// mouse rejects is logged and skipped; a broken link (timeout, I/O error,
/// mouse unreachable) is returned so the caller reconnects.
fn step(name: &str, r: Result<()>) -> Result<()> {
    match r {
        Ok(()) => Ok(()),
        Err(e) if is_link_failure(&e) => Err(e),
        Err(e) => {
            log::warn!("cannot apply {name}: {e}");
            Ok(())
        }
    }
}

/// Writes the DPI after checking it against the mouse's own list.
fn apply_dpi(dev: &Device, want: u16) -> Result<()> {
    let Some(list) = dpi::read_list(dev)? else {
        log::warn!("cannot apply dpi: not supported by {}", dev.name);
        return Ok(());
    };
    list.validate(want)?;
    dpi::write(dev, want).map(|_| ())
}

/// Which settings sections are written to the mouse.
#[derive(Debug, PartialEq)]
struct Sections {
    dpi: bool,
    smartshift: bool,
    wheel: bool,
}

/// With `prev`, only sections that changed are written, so a reload does not
/// undo what the user set on the mouse itself (e.g. the ratchet button).
/// Without it (first start, wake-up) everything the config sets is written.
fn sections_to_write(cfg: &Config, prev: Option<&Config>) -> Sections {
    let changed = |same: bool| prev.is_none() || !same;
    Sections {
        dpi: cfg.dpi.is_some() && changed(prev.is_some_and(|p| p.dpi == cfg.dpi)),
        smartshift: smartshift_change(&cfg.smartshift).is_some()
            && changed(prev.is_some_and(|p| p.smartshift == cfg.smartshift)),
        wheel: cfg.wheel.hires.is_some()
            && changed(prev.is_some_and(|p| p.wheel == cfg.wheel)),
    }
}

/// The config to compare with after a session ended with a reload: a reload
/// caused by the config keeps `applied` (unchanged sections are skipped); any
/// other (mouse woke up or reconnected, DPI drift) forgets it, because the
/// mouse may have lost every setting.
pub(super) fn prev_after_reload(applied: Option<Config>, config_changed: bool) -> Option<Config> {
    applied.filter(|_| config_changed)
}

/// Writes DPI, SmartShift and wheel options from the config (see
/// [`sections_to_write`]).
pub(super) fn apply_settings(dev: &Device, cfg: &Config, prev: Option<&Config>) -> Result<()> {
    let todo = sections_to_write(cfg, prev);
    if let Some(v) = cfg.dpi.filter(|_| todo.dpi) {
        step("dpi", apply_dpi(dev, v))?;
    }
    if let Some(change) = smartshift_change(&cfg.smartshift).filter(|_| todo.smartshift) {
        step(
            "smartshift",
            (|| {
                if smartshift::read(dev)?.is_some() {
                    smartshift::write(dev, change)?;
                }
                Ok(())
            })(),
        )?;
    }
    if todo.wheel {
        let c = wheel::Change { hires: cfg.wheel.hires };
        step("wheel", wheel::write(dev, c).map(|_| ()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles;
    use crate::hidpp::device::Timeout;

    #[test]
    fn maps_smartshift_config() {
        let c = |threshold| config::SmartShift { threshold };
        assert_eq!(smartshift_change(&c(None)), None);
        // Only the threshold: the wheel mode is left alone (mode byte 0).
        let change = smartshift_change(&c(Some(83))).unwrap();
        assert_eq!(change, smartshift::Change { ratchet: None, threshold: Some(83) });
        assert_eq!(smartshift::encode(change), [0, 83, 0]);
        let always = smartshift_change(&c(Some(smartshift::ALWAYS_RATCHET))).unwrap();
        assert_eq!(smartshift::encode(always), [0, 255, 0]);
        // The CLI threshold still switches to ratchet mode.
        assert_eq!(
            crate::commands::resolve(crate::commands::Arg::Threshold(83)).ratchet,
            Some(true)
        );
    }

    #[test]
    fn settings_are_written_again_after_wake_but_not_after_unrelated_reload() {
        let mut cfg = Config { dpi: Some(1600), ..Default::default() };
        cfg.smartshift.threshold = Some(83);
        cfg.wheel.hires = Some(true);
        let all = Sections { dpi: true, smartshift: true, wheel: true };
        let none = Sections { dpi: false, smartshift: false, wheel: false };
        // First start: everything is written.
        assert_eq!(sections_to_write(&cfg, None), all);
        // Config reload with nothing changed: nothing is written.
        let applied = Some(cfg.clone());
        let prev = prev_after_reload(applied.clone(), true);
        assert_eq!(sections_to_write(&cfg, prev.as_ref()), none);
        // Wake-up / reconnect / DPI drift: everything is written again.
        let prev = prev_after_reload(applied, false);
        assert!(prev.is_none());
        assert_eq!(sections_to_write(&cfg, prev.as_ref()), all);
        // Only the DPI changed in the config: only the DPI is written.
        let mut new = cfg.clone();
        new.dpi = Some(800);
        let got = sections_to_write(&new, Some(&cfg));
        assert_eq!(got, Sections { dpi: true, smartshift: false, wheel: false });
    }

    #[test]
    fn switching_to_builtin_profile_keeps_mouse_smartshift() {
        let mut mine = Config { dpi: Some(1600), ..Default::default() };
        mine.smartshift.threshold = Some(119);
        mine.wheel.hires = Some(false);
        let builtin = profiles::builtin();
        // To the built-in profile: only the DPI is written; the SmartShift
        // threshold and the wheel stay as the mouse has them.
        let got = sections_to_write(&builtin, Some(&mine));
        assert_eq!(got, Sections { dpi: true, smartshift: false, wheel: false });
        // And back: everything the own profile sets.
        let got = sections_to_write(&mine, Some(&builtin));
        assert_eq!(got, Sections { dpi: true, smartshift: true, wheel: true });
    }

    #[test]
    fn rejected_settings_are_skipped_but_link_errors_not() {
        use crate::hidpp::message::{ErrorKind, HidppError};
        let hidpp = |version, code| -> Result<()> {
            Err(HidppError { version, feature_index: 0, function: 0, sw_id: 0, code }.into())
        };
        assert!(step("x", Ok(())).is_ok());
        assert!(step("x", hidpp(ErrorKind::V2, 0x02)).is_ok()); // invalid argument
        assert!(step("x", Err(anyhow::anyhow!("dpi 99999 not supported"))).is_ok());
        assert!(step("x", hidpp(ErrorKind::V1, 0x09)).is_err()); // receiver: unreachable
        assert!(step("x", Err(Timeout.into())).is_err());
    }
}
