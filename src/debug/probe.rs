//! What the daemon's debug requests read and drive, filled in by the
//! daemon's own code: the last gesture and action, the mice the daemon has
//! open (read for `debug device`, gestures played into their gesture code)
//! and a temporary log level.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryIter};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use log::LevelFilter;

use super::proto::Inject;
use crate::hidpp::device::Device;
use crate::hidpp::transport::Interrupter;
use crate::util::lock;

/// The end of a gesture, as logged by `gesture end`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GestureRecord {
    pub button: u16,
    /// `click`, `up`, `down`, `left`, `right`, `ambiguous`, `none` or `dropped`.
    pub result: &'static str,
    pub moves: u32,
    pub x: i32,
    pub y: i32,
    /// Movement reports ignored right after the press.
    pub skipped: u32,
    pub took_ms: u64,
    pub at: SystemTime,
}

/// An action run by a button or gesture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionRecord {
    pub action: String,
    pub ok: bool,
    pub took_ms: u64,
    pub at: SystemTime,
}

#[derive(Default)]
struct Last {
    gesture: Option<GestureRecord>,
    action: Option<ActionRecord>,
}

static LAST: Mutex<Last> = Mutex::new(Last { gesture: None, action: None });

pub fn note_gesture(r: GestureRecord) {
    lock(&LAST).gesture = Some(r);
}

pub fn note_action(r: ActionRecord) {
    lock(&LAST).action = Some(r);
}

/// The last gesture and the last action.
pub fn last() -> (Option<GestureRecord>, Option<ActionRecord>) {
    let l = lock(&LAST);
    (l.gesture.clone(), l.action.clone())
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// A mouse thread that takes played gestures.
struct Target {
    id: u64,
    name: String,
    gesture_cids: Vec<u16>,
    events: Sender<Inject>,
    wake: Arc<Interrupter>,
}

static TARGETS: Mutex<Vec<Target>> = Mutex::new(Vec::new());

/// The mouse thread's end of played gestures; registered while it lives.
pub struct Injection {
    id: u64,
    events: Receiver<Inject>,
}

impl Injection {
    /// Registers the mouse thread of `name` with its gesture buttons; `wake`
    /// interrupts its wait for the mouse.
    pub fn register(name: &str, gesture_cids: &[u16], wake: Interrupter) -> Injection {
        let (tx, rx) = mpsc::channel();
        let id = next_id();
        lock(&TARGETS).push(Target {
            id,
            name: name.to_string(),
            gesture_cids: gesture_cids.to_vec(),
            events: tx,
            wake: Arc::new(wake),
        });
        Injection { id, events: rx }
    }

    /// The played events that arrived since the last call.
    pub fn drain(&self) -> TryIter<'_, Inject> {
        self.events.try_iter()
    }
}

impl Drop for Injection {
    fn drop(&mut self) {
        lock(&TARGETS).retain(|t| t.id != self.id);
    }
}

/// Sends played events to one mouse thread.
pub struct Player {
    pub name: String,
    pub default_cid: u16,
    events: Sender<Inject>,
    wake: Arc<Interrupter>,
}

impl Player {
    /// The first mouse with a gesture button.
    pub fn first() -> Option<Player> {
        let targets = lock(&TARGETS);
        let t = targets.iter().find(|t| !t.gesture_cids.is_empty())?;
        Some(Player {
            name: t.name.clone(),
            default_cid: t.gesture_cids[0],
            events: t.events.clone(),
            wake: Arc::clone(&t.wake),
        })
    }

    /// False once the mouse thread is gone (session ended or restarted).
    pub fn send(&self, ev: Inject) -> bool {
        let ok = self.events.send(ev).is_ok();
        self.wake.interrupt();
        ok
    }
}

/// A device the daemon has open.
struct Held {
    id: u64,
    key: String,
    dev: Arc<Device>,
    /// False for a device left alone as an unsupported model.
    supported: bool,
}

static DEVICES: Mutex<Vec<Held>> = Mutex::new(Vec::new());

/// Keeps a served mouse listed while it lives.
pub struct DeviceGuard(u64);

impl Drop for DeviceGuard {
    fn drop(&mut self) {
        lock(&DEVICES).retain(|h| h.id != self.0);
    }
}

/// Lists the served mouse `dev` until the guard is dropped.
pub fn hold_device(key: &str, dev: &Arc<Device>) -> DeviceGuard {
    let id = next_id();
    lock(&DEVICES).push(Held { id, key: key.to_string(), dev: Arc::clone(dev), supported: true });
    DeviceGuard(id)
}

/// Keeps an unsupported device open, so a new model can be read through
/// the daemon; replaces the one kept before under the same key.
pub fn park_unsupported(key: String, dev: Device) {
    let mut held = lock(&DEVICES);
    held.retain(|h| h.supported || h.key != key);
    held.push(Held { id: next_id(), key, dev: Arc::new(dev), supported: false });
}

/// The open devices: key, handle and whether it is a supported model.
pub fn devices() -> Vec<(String, Arc<Device>, bool)> {
    lock(&DEVICES).iter().map(|h| (h.key.clone(), Arc::clone(&h.dev), h.supported)).collect()
}

struct LogState {
    /// The level the process started with.
    base: LevelFilter,
    /// Counts changes, so an older timer does not undo a newer change.
    generation: u64,
    /// When a temporary level ends.
    until: Option<SystemTime>,
}

static LOG: Mutex<LogState> =
    Mutex::new(LogState { base: LevelFilter::Info, generation: 0, until: None });

/// Remembers the level the process started with (see `debug::init_logger`).
pub fn set_base_level(level: LevelFilter) {
    lock(&LOG).base = level;
}

/// The current log level and when a temporary one ends.
pub fn log_level() -> (LevelFilter, Option<SystemTime>) {
    (log::max_level(), lock(&LOG).until)
}

/// Logs at `level` for `minutes`, then returns to the base level; 0
/// minutes returns at once. Returns the level now in effect.
pub fn set_log_level(level: LevelFilter, minutes: u32) -> LevelFilter {
    let mut s = lock(&LOG);
    s.generation += 1;
    if minutes == 0 {
        s.until = None;
        log::set_max_level(s.base);
        return s.base;
    }
    let generation = s.generation;
    let lasts = Duration::from_secs(u64::from(minutes) * 60);
    s.until = Some(SystemTime::now() + lasts);
    log::set_max_level(level);
    drop(s);
    let back = move || {
        std::thread::sleep(lasts);
        let mut s = lock(&LOG);
        if s.generation == generation {
            s.until = None;
            log::set_max_level(s.base);
        }
    };
    if let Err(e) = std::thread::Builder::new().name("debug-log".into()).spawn(back) {
        log::warn!("cannot start the log level timer: {e}");
    }
    level
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_records_are_kept() {
        let at = SystemTime::now();
        note_action(ActionRecord { action: "Key(cmd+a)".into(), ok: true, took_ms: 3, at });
        let g = GestureRecord {
            button: 0xC3,
            result: "down",
            moves: 6,
            x: 0,
            y: 240,
            skipped: 1,
            took_ms: 140,
            at,
        };
        note_gesture(g.clone());
        let (gesture, action) = last();
        assert_eq!(gesture, Some(g));
        assert_eq!(action.map(|a| a.took_ms), Some(3));
    }
}
