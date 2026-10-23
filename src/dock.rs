//! Restarting the Dock. A stuck desktop (see [`crate::spaces`]) is usually the
//! Dock in a mode that draws nothing; quitting it makes launchd start it again,
//! as `killall Dock` does. Only the Dock of the current user is touched.

use std::process::Command;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::util::Throttle;

/// The daemon restarts the Dock at most this often.
const MIN_INTERVAL: Duration = Duration::from_secs(30);

static LIMIT: LazyLock<Mutex<Throttle>> = LazyLock::new(|| Mutex::new(Throttle::new(MIN_INTERVAL)));

/// Quits the Dock of the current user; launchd starts it again.
pub fn restart() -> Result<()> {
    // `killall` run by a normal user only reaches that user's processes.
    let status = Command::new("/usr/bin/killall").arg("Dock").status()?;
    if !status.success() {
        bail!("killall Dock failed: {status}");
    }
    Ok(())
}

/// Restarts the Dock unless it was restarted by this function within the last
/// 30 s. Logs the result.
pub fn restart_if_allowed(reason: &str) {
    restart_limited(reason, Instant::now(), &mut crate::util::lock(&LIMIT), restart);
}

/// [`restart_if_allowed`] with the clock, the limit and the restart given.
fn restart_limited(
    reason: &str,
    now: Instant,
    limit: &mut Throttle,
    run: impl FnOnce() -> Result<()>,
) -> bool {
    if !limit.allow(now) {
        log::info!("dock restart skipped: reason={reason} (restarted less than 30 s ago)");
        return false;
    }
    match run() {
        Ok(()) => log::warn!("dock restarted: reason={reason}"),
        Err(e) => log::warn!("dock restart failed: reason={reason} error={e}"),
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stuck_restarts_the_dock_once_in_30_seconds() {
        let t0 = Instant::now();
        let mut l = Throttle::new(MIN_INTERVAL);
        let mut runs = 0;
        for secs in [0, 1, 29] {
            restart_limited("stuck", t0 + Duration::from_secs(secs), &mut l, || {
                runs += 1;
                Ok(())
            });
        }
        assert_eq!(runs, 1);
        let again = restart_limited("stuck", t0 + Duration::from_secs(31), &mut l, || {
            runs += 1;
            Ok(())
        });
        assert!(again);
        assert_eq!(runs, 2);
    }

    #[test]
    fn failed_restart_still_counts_for_the_limit() {
        let t0 = Instant::now();
        let mut l = Throttle::new(MIN_INTERVAL);
        assert!(restart_limited("stuck", t0, &mut l, || bail!("no")));
        assert!(!restart_limited("stuck", t0 + Duration::from_secs(2), &mut l, || Ok(())));
    }
}
