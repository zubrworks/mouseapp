//! Looking for mice and starting, collecting and pacing their workers.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hidapi::HidApi;
use rust_i18n::t;

use super::Shared;
use super::arrival::is_entry;
use super::wake::EndNotice;
use super::worker::worker;
use crate::devices;
use crate::discovery::Connection;
use crate::hidpp::device::Device;

/// First wait after a failed connection attempt.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
/// Longest wait between connection attempts.
const BACKOFF_MAX: Duration = Duration::from_secs(10);
/// A connection that lasted at least this long resets the backoff.
const STABLE_AFTER: Duration = Duration::from_secs(10);
/// How often new mice are looked for while at least one is connected.
/// IOKit reports new devices at once (see [`super::arrival`]); this catches
/// a mouse that turns on behind a receiver that is already there.
pub(super) const SCAN_IDLE: Duration = Duration::from_secs(60);
/// A device that is not a supported mouse is left alone this long.
const IGNORE_FOR: Duration = Duration::from_secs(300);

/// Exponential wait between connection attempts.
pub(super) struct Backoff {
    next: Duration,
}

impl Backoff {
    pub(super) fn new() -> Self {
        Backoff { next: BACKOFF_MIN }
    }

    pub(super) fn reset(&mut self) {
        self.next = BACKOFF_MIN;
    }

    /// Returns the current wait and doubles the next one.
    pub(super) fn step(&mut self) -> Duration {
        let now = self.next;
        self.next = (self.next * 2).min(BACKOFF_MAX);
        now
    }
}

/// A mouse being served.
pub(super) struct Running {
    pub(super) key: String,
    pub(super) slot: (CString, u8),
    pub(super) name: String,
    pub(super) since: Instant,
    pub(super) handle: JoinHandle<()>,
    /// Set as the worker ends (see [`EndNotice`]).
    ended: Arc<AtomicBool>,
    /// Set when the system removed the device; the worker then ends.
    gone: Arc<AtomicBool>,
}

/// A mouse found by a scan, not yet served.
pub(super) struct Candidate<T> {
    pub(super) key: String,
    rank: u8,
    item: T,
}

/// Lower is preferred when one mouse is reachable several ways: a direct
/// link has no receiver in between.
fn rank(c: Connection) -> u8 {
    match c {
        Connection::Bluetooth => 0,
        Connection::Usb => 1,
        Connection::Bolt => 2,
        Connection::Unifying => 3,
    }
}

/// Keeps one candidate per mouse (the best-ranked) and drops mice in `taken`.
fn pick_unique<T>(mut found: Vec<Candidate<T>>, taken: &HashSet<String>) -> Vec<T> {
    found.sort_by_key(|c| c.rank); // stable: equal ranks keep scan order
    let mut seen = taken.clone();
    found
        .into_iter()
        .filter(|c| seen.insert(c.key.clone()))
        .map(|c| c.item)
        .collect()
}

/// Remembers the last open error so a permanent problem (missing Input
/// Monitoring permission) is logged once, not on every scan.
#[derive(Default)]
pub(super) struct ErrorLog {
    last: Option<String>,
}

impl ErrorLog {
    pub(super) fn report(&mut self, errors: &[anyhow::Error]) {
        let Some(e) = errors.last() else {
            self.last = None;
            return;
        };
        let text = format!("{e:#}");
        if self.last.as_ref() != Some(&text) {
            log::warn!("cannot open a HID interface: {text}");
            self.last = Some(text);
        }
    }
}

/// Looks for mice that are not being served yet.
pub(super) fn discover(
    api: &HidApi,
    running: &[Running],
    ignored: &mut HashMap<(CString, u8), Instant>,
    errors: &mut ErrorLog,
) -> Vec<(String, Device)> {
    ignored.retain(|_, at| at.elapsed() < IGNORE_FOR);
    let skip = |path: &CString, index: u8| {
        running.iter().any(|r| r.slot.0 == *path && r.slot.1 == index)
            || ignored.contains_key(&(path.clone(), index))
    };
    let scan = Device::scan(api, &skip, usize::MAX);
    errors.report(&scan.errors);
    let taken: HashSet<String> = running.iter().map(|r| r.key.clone()).collect();
    let mut found = Vec::new();
    for mut dev in scan.devices {
        let key = dev.identity();
        if taken.contains(&key) {
            log::debug!("{key} is already served; ignoring the {} link", dev.connection.label());
            continue;
        }
        // Over a receiver the interface name is the receiver's, not the mouse's.
        dev.refine_name();
        if devices::identify(dev.pid, &dev.name).is_none() {
            log::info!("ignoring unsupported device {} ({})", dev.name, key);
            ignored.insert((dev.path.clone(), dev.index), Instant::now());
            // Test build: kept open, so `debug device` can read a new model.
            #[cfg(feature = "debug-access")]
            crate::debug::probe::park_unsupported(key, dev);
            continue;
        }
        found.push(Candidate { key: key.clone(), rank: rank(dev.connection), item: (key, dev) });
    }
    pick_unique(found, &taken)
}

/// Starts a worker thread for `dev`.
pub(super) fn spawn(key: String, dev: Device, shared: &Arc<Shared>, running: &mut Vec<Running>) {
    let slot = (dev.path.clone(), dev.index);
    let name = dev.name.clone();
    let shared = Arc::clone(shared);
    let worker_key = key.clone();
    // Wakes the daemon loop to collect the thread when it ends.
    let (notice, ended) = EndNotice::new(shared.wake.clone());
    let gone = Arc::new(AtomicBool::new(false));
    let worker_gone = Arc::clone(&gone);
    match thread::Builder::new().name(format!("mouse {name}")).spawn(move || {
        let _notice = notice;
        worker(dev, worker_key, shared, &worker_gone)
    })
    {
        Ok(handle) => {
            let since = Instant::now();
            running.push(Running { key, slot, name, since, handle, ended, gone });
        }
        Err(e) => log::error!("cannot start a worker for {name}: {e}"),
    }
}

/// Ends the workers of the device with IOKit registry entry `id`, which the
/// system removed (the mouse was switched off or the receiver unplugged).
pub(super) fn let_go(running: &[Running], id: u64) {
    for r in running.iter().filter(|r| is_entry(&r.slot.0, id)) {
        log::info!("{} was removed by the system", r.name);
        r.gone.store(true, Ordering::SeqCst);
    }
}

/// Collects finished workers. Returns true if any ended while the program
/// keeps running, and tells the backoff whether the connection was stable.
pub(super) fn reap(running: &mut Vec<Running>, backoff: &mut Backoff) -> bool {
    let mut ended = false;
    let mut i = 0;
    while i < running.len() {
        let r = &running[i];
        if !r.ended.load(Ordering::SeqCst) && !r.handle.is_finished() {
            i += 1;
            continue;
        }
        let r = running.swap_remove(i);
        if r.handle.join().is_err() {
            log::warn!("{}", t!("daemon.worker_failed", name = r.name));
        }
        if r.since.elapsed() >= STABLE_AFTER {
            backoff.reset();
        }
        ended = true;
    }
    ended
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        let mut b = Backoff::new();
        let waits: Vec<u64> = (0..6).map(|_| b.step().as_secs()).collect();
        assert_eq!(waits, [1, 2, 4, 8, 10, 10]);
        b.reset();
        assert_eq!(b.step(), BACKOFF_MIN);
    }

    #[test]
    fn same_mouse_over_two_links_is_served_once() {
        let c = |key: &str, conn: Connection, item: &'static str| Candidate {
            key: key.to_string(),
            rank: rank(conn),
            item,
        };
        let found = vec![
            c("unit:a", Connection::Bolt, "a-bolt"),
            c("unit:a", Connection::Bluetooth, "a-bt"),
            c("unit:b", Connection::Bolt, "b-bolt"),
        ];
        let picked = pick_unique(found, &HashSet::new());
        assert_eq!(picked, ["a-bt", "b-bolt"]);
    }

    #[test]
    fn mice_already_served_are_not_started_again() {
        let found = vec![
            Candidate { key: "unit:a".into(), rank: 0, item: 1 },
            Candidate { key: "unit:b".into(), rank: 2, item: 2 },
        ];
        let taken = HashSet::from(["unit:a".to_string()]);
        assert_eq!(pick_unique(found, &taken), [2]);
    }

    #[test]
    fn open_errors_are_logged_once() {
        let mut log = ErrorLog::default();
        let errs = [anyhow::anyhow!("not permitted")];
        log.report(&errs);
        assert_eq!(log.last.as_deref(), Some("not permitted"));
        log.report(&[]);
        assert_eq!(log.last, None);
    }
}
