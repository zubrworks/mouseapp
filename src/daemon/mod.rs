//! `mouse run`: keeps the active settings profile applied to the mice.
//! Detects every mouse (one worker thread each), applies the settings,
//! listens for buttons and gestures, re-applies after sleep / Easy-Switch /
//! reconnect and reloads when the profile or the choice of profile changes. A mouse reachable over several connections
//! (Bolt and Bluetooth) is served once. The settings app talks to it through
//! the control socket ([`ipc`]); only one daemon runs at a time.

mod apply;
mod arrival;
mod control;
mod scan;
mod wake;
mod watch;
mod worker;

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::{Result, anyhow};
use hidapi::HidApi;
use objc2::MainThreadMarker;
use rust_i18n::t;

use crate::app_overrides::FrontmostWatch;
use crate::config::Config;
use crate::hub::Hub;
use crate::ipc::{self, Bind, Server};
use crate::permissions;
use crate::profiles::{self, Store};
use crate::runloop;
use crate::scroll;
use crate::shutdown;
use crate::spaces::SpaceWatch;
use control::{answer, handle_command, reloaded};
use arrival::ArrivalScans;
use scan::{Backoff, ErrorLog, Running, SCAN_IDLE, discover, let_go, reap, spawn};
use wake::{Event, Waker};
use watch::{ConfigWatch, load_checked};

/// The config every worker follows. `generation` grows with each change so
/// a worker notices it without comparing configs.
struct Shared {
    state: Mutex<(u64, Config)>,
    /// Status for the app's menu bar, and requests coming from the app.
    hub: Arc<Hub>,
    /// Wakes the daemon loop.
    wake: Waker,
}

impl Shared {
    fn new(cfg: Config, wake: Waker) -> Self {
        let on_command = wake.clone();
        Shared {
            state: Mutex::new((0, cfg)),
            hub: Arc::new(Hub::new(move || on_command.send(Event::Command))),
            wake,
        }
    }

    /// Stores `cfg` and makes every worker re-apply it, even if unchanged.
    fn reapply(&self, cfg: Config) {
        let mut s = self.lock();
        *s = (s.0 + 1, cfg);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, (u64, Config)> {
        // A worker that panicked must not take the others down with it.
        crate::util::lock(&self.state)
    }

    fn snapshot(&self) -> (u64, Config) {
        self.lock().clone()
    }

    fn generation(&self) -> u64 {
        self.lock().0
    }

    /// Stores `cfg`; returns false if it equals the current one.
    fn replace(&self, cfg: Config) -> bool {
        let mut s = self.lock();
        if s.1 == cfg {
            return false;
        }
        *s = (s.0 + 1, cfg);
        true
    }
}

/// A daemon that owns the control socket and has loaded its settings.
struct Started {
    store: Store,
    shared: Arc<Shared>,
    /// Events for the daemon loop (see [`wake`]).
    events: Receiver<Event>,
    /// Socket server thread; ends with the stop request.
    socket: JoinHandle<()>,
}

/// Takes the control socket (or reports the daemon that already has it),
/// loads the settings and starts the scroll tap and the socket server.
/// `None` means another daemon is running.
fn start() -> Result<Option<Started>> {
    let store = Store::open()?;
    let server = match Server::bind(store.dir()) {
        Ok(Bind::Ready(server)) => server,
        Ok(Bind::Running) => {
            already_running(store.dir());
            return Ok(None);
        }
        Err(e) => anyhow::bail!(t!("daemon.socket_failed", error = format!("{e:#}")).to_string()),
    };
    if let Some(name) = store.prepare()? {
        println!("{}", t!("daemon.migrated", name = name));
    }
    let (name, cfg) = load_or_recover(&store)?;
    println!("{}", t!("daemon.profile", name = profiles::display_name(&name)));
    let (wake, events) = wake::channel();
    let on_stop = wake.clone();
    shutdown::on_stop(move || on_stop.send(Event::Stop));
    let shared = Arc::new(Shared::new(cfg, wake));
    shared.hub.set_profile(&name);
    scroll::spawn(&shared.snapshot().1.scroll);
    let socket_shared = Arc::clone(&shared);
    let socket = server.spawn(move |req| answer(req, &socket_shared), shutdown::stopped)?;
    Ok(Some(Started { store, shared, events, socket }))
}

/// Loads the current settings at start. Settings that cannot be used (a
/// broken `current.toml`) are kept as a `.bak` copy and replaced by the
/// active profile, so the mouse works; if that fails too, the first error
/// is returned.
fn load_or_recover(store: &Store) -> Result<(String, Config)> {
    let error = match load_checked(store) {
        Ok(loaded) => return Ok(loaded),
        Err(e) => e,
    };
    match store.recover_current() {
        Ok(backup) => {
            log::warn!(
                "{}",
                t!("daemon.current_restored", error = format!("{error:#}"), backup = backup.display())
            );
            load_checked(store)
        }
        Err(e) => {
            log::debug!("current settings not restored: {e}");
            Err(error)
        }
    }
}

/// Tells the user that another daemon is running (with its profile if it answers).
fn already_running(dir: &Path) {
    match ipc::status(&ipc::socket_path(dir)) {
        Ok(s) => log::error!(
            "{}",
            t!("daemon.already_running_profile", name = profiles::display_name(&s.profile))
        ),
        Err(e) => {
            log::debug!("the running daemon does not answer: {e:#}");
            log::error!("{}", t!("daemon.already_running"));
        }
    }
}

/// Stops the socket server after the daemon loop ended. The server waits
/// in `accept`, so a connection wakes it to see the stop request.
fn stop_socket(socket: JoinHandle<()>, path: &Path) {
    shutdown::request_stop();
    if !socket.is_finished()
        && let Err(e) = ipc::wake(path)
    {
        // Already gone (it ended on a `quit` request) or unreachable: the
        // process exits without waiting for it.
        log::debug!("cannot wake the socket thread: {e}");
        return;
    }
    if socket.join().is_err() {
        log::error!("socket thread panicked");
    }
}

/// `mouse run`: the daemon without a user interface. Runs until Ctrl-C,
/// SIGTERM or a `quit` request.
pub fn run() -> Result<()> {
    crate::hidpp::device::use_daemon_sw_ids();
    let Some(Started { store, shared, events, socket }) = start()? else {
        return Ok(());
    };
    permissions::request_missing();
    let result = match MainThreadMarker::new() {
        Some(mtm) => run_with_events(mtm, &store, &shared, events),
        None => run_loop(&store, &shared, &events),
    };
    stop_socket(socket, &ipc::socket_path(store.dir()));
    result
}

/// The daemon loop on its own thread while the main thread runs the event
/// loop ([`runloop::run_events_until_stopped`], without `NSApplication`,
/// which would cost several MB). Once the process talks to WindowServer
/// (posting actions, `NSWorkspace`), WindowServer sends it messages; the
/// event loop takes them, so its queue for this process never fills up (a
/// full queue stalls the input of the whole system). Ends on a stop
/// request, after the daemon loop.
fn run_with_events(
    mtm: MainThreadMarker,
    store: &Store,
    shared: &Arc<Shared>,
    events: Receiver<Event>,
) -> Result<()> {
    let frontmost = FrontmostWatch::start(mtm);
    let spaces = SpaceWatch::start(mtm);
    let (store, shared) = (store.clone(), Arc::clone(shared));
    let daemon = std::thread::Builder::new()
        .name("daemon".into())
        .spawn(move || {
            // Also an error or a panic of the loop ends the event loop.
            let _stop = StopOnDrop;
            run_loop(&store, &shared, &events)
        })?;
    runloop::run_events_until_stopped(mtm);
    drop(spaces);
    drop(frontmost);
    daemon.join().unwrap_or_else(|_| Err(anyhow!("the daemon loop panicked")))
}

/// Requests a stop when dropped.
struct StopOnDrop;

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        shutdown::request_stop();
    }
}

/// Serves the mice until a stop request. Sleeps until an [`Event`] or the
/// nearest deadline: the next scan for mice (regular or after a device
/// appeared) or the settling of changed files.
fn run_loop(store: &Store, shared: &Arc<Shared>, events: &Receiver<Event>) -> Result<()> {
    let mut watch = match ConfigWatch::start(store, shared.wake.clone()) {
        Ok(w) => Some(w),
        Err(e) => {
            log::warn!("{}", t!("daemon.watch_failed", error = e));
            None
        }
    };
    let mut api = HidApi::new()?;
    let arrivals = match arrival::spawn(shared.wake.clone()) {
        Ok(h) => Some(h),
        Err(e) => {
            log::warn!("cannot start watching for mice coming and going: {e}");
            None
        }
    };
    let mut arrived = ArrivalScans::default();
    let mut backoff = Backoff::new();
    let mut running: Vec<Running> = Vec::new();
    let mut ignored = HashMap::new();
    let mut errors = ErrorLog::default();
    let mut waiting_shown = false;
    let mut next_scan = Instant::now();
    println!("{}", t!("daemon.started"));
    let mut scroll_gen = shared.generation();
    while !shutdown::stopped() {
        while let Some(cmd) = shared.hub.next_command() {
            handle_command(cmd, store, shared);
        }
        if let Some(w) = watch.as_mut()
            && let Some(loaded) = w.poll()
        {
            match loaded {
                Ok((name, c)) => {
                    shared.hub.set_profile(&name);
                    if shared.replace(c) {
                        reloaded(&name);
                    }
                }
                Err(e) => log::warn!("{}", t!("daemon.reload_failed", error = e)),
            }
        }
        if shared.generation() != scroll_gen {
            let (g, c) = shared.snapshot();
            scroll_gen = g;
            scroll::update(&c.scroll);
        }
        if reap(&mut running, &mut backoff) {
            next_scan = Instant::now() + backoff.step();
        }
        let now = Instant::now();
        if now >= next_scan || arrived.due().is_some_and(|t| now >= t) {
            arrived.scanned(now);
            if let Err(e) = crate::discovery::refresh_logitech(&mut api) {
                log::warn!("cannot refresh device list: {e}");
            }
            let fresh = discover(&api, &running, &mut ignored, &mut errors);
            if fresh.is_empty() {
                if running.is_empty() && !std::mem::replace(&mut waiting_shown, true) {
                    println!("{}", t!("daemon.waiting"));
                }
                next_scan = Instant::now()
                    + if running.is_empty() { backoff.step() } else { SCAN_IDLE };
            } else {
                waiting_shown = false;
                for (key, dev) in fresh {
                    spawn(key, dev, shared, &mut running);
                }
                next_scan = Instant::now() + SCAN_IDLE;
            }
        }
        let settled = watch.as_ref().and_then(ConfigWatch::deadline);
        let scan_at = arrived.due().map_or(next_scan, |t| t.min(next_scan));
        let deadline = settled.map_or(scan_at, |d| d.min(scan_at));
        match wake::wait(events, deadline) {
            Some(Event::Stop) => break,
            Some(Event::Files) => {
                if let Some(w) = watch.as_mut() {
                    w.changed();
                }
            }
            Some(Event::Arrived) => arrived.arrived(Instant::now()),
            Some(Event::Removed(id)) => let_go(&running, id),
            // Commands and ended workers are handled at the top of the loop.
            Some(Event::Command | Event::WorkerEnded) | None => {}
        }
    }
    // Workers see the stop flag within a poll interval and release the buttons.
    for r in running {
        if r.handle.join().is_err() {
            log::warn!("{}", t!("daemon.worker_failed", name = r.name));
        }
    }
    // The loop ends only on a stop request, which also ends this thread.
    if let Some(h) = arrivals
        && h.join().is_err()
    {
        log::warn!("the hid arrivals thread panicked");
    }
    println!("{}", t!("events.stopping"));
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn config_generation_changes_only_on_real_change() {
        let shared = Shared::new(Config::default(), wake::channel().0);
        assert_eq!(shared.generation(), 0);
        assert!(!shared.replace(Config::default()));
        assert_eq!(shared.generation(), 0);
        let changed = Config { dpi: Some(1600), ..Config::default() };
        assert!(shared.replace(changed.clone()));
        assert_eq!(shared.snapshot(), (1, changed));
    }

    #[test]
    fn shared_config_survives_a_panicked_worker() {
        let shared = Arc::new(Shared::new(Config::default(), wake::channel().0));
        let s = Arc::clone(&shared);
        let _ = thread::spawn(move || {
            let _guard = s.lock();
            panic!("worker died while holding the lock");
        })
        .join();
        assert_eq!(shared.generation(), 0); // poisoned lock is still usable
    }
}
