//! A worker thread: one mouse from connection to disconnection.

use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rust_i18n::t;

use super::Shared;
use super::apply::{apply_settings, prev_after_reload};
use crate::config::Config;
use crate::devices;
use crate::events::{self, End, Next};
use crate::hidpp::device::{Device, Timeout, is_link_failure};
use crate::hidpp::features::{battery, dpi, smartshift};
use crate::hub::{Hub, MouseInfo};
use crate::shutdown;

/// How often the mouse is pinged while connected. Only a backstop: the system
/// reports a vanished mouse (see [`super::arrival`]).
const HEALTH_EVERY: Duration = Duration::from_secs(60);
/// Pause before the second ping after one went unanswered.
const HEALTH_RETRY: Duration = Duration::from_secs(1);
/// Unanswered pings in a row after which the mouse counts as lost.
const PING_MISSES: u32 = 2;
/// Minimal time between re-applications caused by drifted settings.
const DRIFT_COOLDOWN: Duration = Duration::from_secs(15);
/// How often the battery and the current DPI are read from the mouse.
const STATUS_EVERY: Duration = Duration::from_secs(300);

/// Decides after an unanswered ping whether the mouse is lost: a timeout
/// must happen `PING_MISSES` times in a row, any other failure counts at once.
fn ping_lost(misses: &mut u32, timed_out: bool) -> bool {
    *misses += 1;
    !timed_out || *misses >= PING_MISSES
}

/// Periodic check that the mouse still answers and kept its settings.
struct Health {
    next_check: Instant,
    misses: u32,
    last_drift: Option<Instant>,
}

impl Health {
    fn new() -> Self {
        Health { next_check: Instant::now() + HEALTH_EVERY, misses: 0, last_drift: None }
    }

    /// Starts over after a (re)configuration; keeps the drift cooldown so a
    /// setting the mouse never accepts cannot cause a re-apply loop.
    fn restart(&mut self) {
        self.next_check = Instant::now() + HEALTH_EVERY;
        self.misses = 0;
    }

    /// Pings the mouse if it is time. `Continue(true)` = the ping was
    /// answered just now, `Continue(false)` = nothing (more) to do.
    fn check(&mut self, dev: &Device, cfg: &Config) -> ControlFlow<End, bool> {
        let now = Instant::now();
        if now < self.next_check {
            return ControlFlow::Continue(false);
        }
        self.next_check = now + HEALTH_EVERY;
        if let Err(e) = dev.ping() {
            let timed_out = e.downcast_ref::<Timeout>().is_some();
            if ping_lost(&mut self.misses, timed_out) {
                log::info!("{} does not answer: {e}", dev.name);
                return ControlFlow::Break(End::Lost);
            }
            log::debug!("{} missed a ping: {e}", dev.name);
            self.next_check = now + HEALTH_RETRY;
            return ControlFlow::Continue(false);
        }
        self.misses = 0;
        // A mouse that slept and woke up quickly answers fine but has lost
        // its settings; DPI is the cheapest one to compare.
        if let Some(want) = cfg.dpi {
            match dpi::read(dev) {
                Ok(Some(cur))
                    if cur.current != want
                        && self.last_drift.is_none_or(|t| t.elapsed() >= DRIFT_COOLDOWN) =>
                {
                    log::info!(
                        "{}: dpi is {} but config says {want}; re-applying",
                        dev.name,
                        cur.current
                    );
                    self.last_drift = Some(Instant::now());
                    return ControlFlow::Break(End::Reload);
                }
                Err(e) if is_link_failure(&e) => {
                    log::info!("{} does not answer: {e}", dev.name);
                    return ControlFlow::Break(End::Lost);
                }
                _ => {}
            }
        }
        ControlFlow::Continue(true)
    }

    /// When the next ping is due.
    fn deadline(&self) -> Instant {
        self.next_check
    }
}

/// Publishes one mouse's status for the app's menu bar and carries out its
/// requests (Easy-Switch). Removes the mouse from the menu bar when dropped.
struct Reporter<'a> {
    hub: &'a Hub,
    info: MouseInfo,
    next_read: Instant,
}

impl<'a> Reporter<'a> {
    fn new(hub: &'a Hub, key: String, dev: &Device) -> Self {
        let model = devices::identify(dev.pid, &dev.name).map(|m| m.name.to_string());
        let info = MouseInfo { key, name: dev.name.clone(), model, ..MouseInfo::default() };
        hub.publish(info.clone());
        Reporter { hub, info, next_read: Instant::now() }
    }

    /// Reads fresh values at the next `tick`.
    fn refresh_soon(&mut self) {
        self.next_read = Instant::now();
    }

    /// Reads what the app's menu bar shows; a failed read leaves the old value.
    fn read(&mut self, dev: &Device) {
        self.next_read = Instant::now() + STATUS_EVERY;
        let mut info = self.info.clone();
        info.battery = battery::read(dev).ok().flatten().or(info.battery);
        info.dpi = dpi::read(dev).ok().flatten().map(|d| d.current).or(info.dpi);
        // The DPI list and the default do not change: read them once per connection.
        if info.dpi_list.is_none() {
            info.dpi_list = dpi::read_list(dev).ok().flatten();
        }
        if info.smartshift.is_none() {
            info.smartshift = smartshift::read(dev).ok().flatten();
        }
        self.hub.publish(info.clone());
        self.info = info;
    }

    fn tick(&mut self, dev: &Device) {
        if Instant::now() >= self.next_read {
            self.read(dev);
        }
    }

    /// When `tick` next reads from the mouse.
    fn deadline(&self) -> Instant {
        self.next_read
    }
}

impl Drop for Reporter<'_> {
    fn drop(&mut self) {
        self.hub.remove(&self.info.key);
    }
}

/// How one connection ended.
enum Outcome {
    Stop,
    Disconnected,
}

/// Applies the settings and serves buttons until the mouse is lost, `gone`
/// is set (the system removed it) or Ctrl-C. A new config from the main
/// thread restarts the setup.
fn serve(dev: &Device, key: String, shared: &Shared, gone: &AtomicBool) -> Outcome {
    let Some(model) = devices::identify(dev.pid, &dev.name) else {
        log::info!("{}: unsupported model", dev.name);
        return Outcome::Disconnected;
    };
    let mut health = Health::new();
    let mut reporter = Reporter::new(&shared.hub, key, dev);
    let mut applied: Option<Config> = None;
    loop {
        if shutdown::stopped() {
            return Outcome::Stop;
        }
        if gone.load(Ordering::SeqCst) {
            return Outcome::Disconnected;
        }
        let (generation, cfg) = shared.snapshot();
        if let Err(e) = apply_settings(dev, &cfg, applied.as_ref()) {
            log::info!("{}: cannot apply settings: {e}", dev.name);
            return Outcome::Disconnected;
        }
        applied = Some(cfg.clone());
        health.restart();
        reporter.refresh_soon();
        let mut config_changed = false;
        let result = events::session(
            dev,
            model,
            &cfg,
            &mut || {
                if gone.load(Ordering::SeqCst) {
                    return ControlFlow::Break(End::Lost);
                }
                if shared.generation() != generation {
                    config_changed = true;
                    return ControlFlow::Break(End::Reload);
                }
                ControlFlow::Continue(())
            },
            // On the check thread, never while a button is held or a gesture
            // is open. The ping comes first and wakes the radio; what follows
            // in the same pass (battery, DPI, then the session's own checks)
            // is fast.
            &mut |d| {
                let contacted = match health.check(d, &cfg) {
                    ControlFlow::Break(end) => return ControlFlow::Break(end),
                    ControlFlow::Continue(pinged) => pinged,
                };
                reporter.tick(d);
                ControlFlow::Continue(Next {
                    deadline: Some(reporter.deadline().min(health.deadline())),
                    contacted,
                })
            },
        );
        match result {
            Ok(End::Stop) => return Outcome::Stop,
            Ok(End::Reload) => applied = prev_after_reload(applied.take(), config_changed),
            Ok(End::Lost) => return Outcome::Disconnected,
            Err(e) => {
                log::info!("{}: session ended: {e}", dev.name);
                return Outcome::Disconnected;
            }
        }
    }
}

/// Body of a worker thread: one mouse from connection to disconnection.
pub(super) fn worker(dev: Device, key: String, shared: Arc<Shared>, gone: &AtomicBool) {
    #[cfg(feature = "debug-access")]
    let dev = Arc::new(dev);
    #[cfg(feature = "debug-access")]
    let _listed = crate::debug::probe::hold_device(&key, &dev);
    println!("{}", t!("daemon.connected", name = dev.name, connection = dev.connection.label()));
    if matches!(serve(&dev, key, &shared, gone), Outcome::Disconnected) {
        println!("{}", t!("daemon.disconnected", name = dev.name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_missed_ping_is_tolerated_two_are_not() {
        let mut m = 0;
        assert!(!ping_lost(&mut m, true));
        assert!(ping_lost(&mut m, true));
        let mut m = 0;
        assert!(ping_lost(&mut m, false)); // I/O error: lost at once
    }
}
