//! Listens to mouse notifications: diverted buttons and gesture buttons (the
//! thumb button and any button set to `gestures`) trigger the actions from
//! the config. Diversion is removed on exit. The checks that talk to the
//! mouse run on their own thread ([`checks`]), so this one never waits.

use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use rust_i18n::t;

use crate::actions::keyboard;
use crate::actions::{self, Action};
use crate::bindings::{BindingSet, GestureBinding, binding_set};
use crate::checks::{self, Checker, Checks};
use crate::config::{Config, DEFAULT_THRESHOLD};
use crate::devices::{self, Model};
use crate::gestures::Tracker;
use crate::hidpp::connection::{self, Link};
use crate::hidpp::device::{Device, LinkStats, is_link_failure};
use crate::hidpp::features::controls::{self, Event};
use crate::hidpp::features::{thumbwheel, wireless};
use crate::shutdown;
use crate::spaces;
use crate::thumb::{self, Thumb};
use crate::util::Throttle;

/// Longest wait for data from the mouse. The loop wakes at least this often
/// to see the stop flag, a new config or a removed mouse, so a change reaches
/// the mouse within this time.
const MAX_WAIT: Duration = Duration::from_secs(1);
/// Poll interval while a thumb wheel scroll gesture is open.
const FAST_POLL_MS: i32 = 20;

/// What the caller's checks tell the check thread: `Break` ends the
/// session; `Continue` carries [`Next`].
pub type Tick = ControlFlow<End, Next>;

/// The answer of the caller's checks when they did not end the session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Next {
    /// The caller's nearest deadline (`None` if it has none), so the check
    /// thread can sleep until then.
    pub deadline: Option<Instant>,
    /// The caller has just talked to the mouse successfully (a ping): the
    /// radio is awake, so the series of checks runs right now.
    pub contacted: bool,
}

/// How long to wait for data from the mouse: until `next`, at most
/// [`MAX_WAIT`]; zero if `next` has passed. Rounded up, so the loop never
/// wakes just before the deadline and spins.
fn wait_ms(now: Instant, next: Option<Instant>) -> i32 {
    let wait = next.map_or(MAX_WAIT, |t| t.saturating_duration_since(now).min(MAX_WAIT));
    i32::try_from(wait.as_micros().div_ceil(1000)).unwrap_or(i32::MAX)
}

/// Removes all diversion when dropped (normal exit, error or panic).
pub(crate) struct Restore<'a> {
    pub(crate) dev: &'a Device,
    pub(crate) idx: u8,
    pub(crate) cids: Vec<u16>,
    /// Thumb wheel is diverted; holds the native `invert` read from the mouse
    /// before the diversion (`None` if it could not be read: left as is).
    pub(crate) thumb: Option<Option<bool>>,
}

impl Drop for Restore<'_> {
    fn drop(&mut self) {
        if let Some(invert) = self.thumb.take() {
            let c = thumb::thumb_restore(invert);
            if let Err(e) = thumbwheel::write(self.dev, c) {
                log::warn!("cannot restore thumb wheel: {e}");
            }
        }
        for &cid in &self.cids {
            if let Err(e) = controls::set_reporting(self.dev, self.idx, cid, false, false) {
                log::warn!("cannot restore button 0x{cid:04X}: {e}");
            }
        }
    }
}

/// True if the action is the macOS app switcher shortcut (cmd+tab variants).
fn is_switcher(a: Option<&Action>) -> bool {
    matches!(a, Some(Action::Key(s)) if s.is_app_switcher())
}

/// Holds Cmd while the user swipes left/right to browse the app switcher.
/// Cmd is always released on drop (button release, Ctrl-C, error, panic).
struct Switcher {
    cmd_down: bool,
    /// Presses or releases Cmd (replaced in tests).
    cmd_key: fn(bool) -> Result<()>,
}

impl Default for Switcher {
    fn default() -> Self {
        Switcher { cmd_down: false, cmd_key: keyboard::cmd_key }
    }
}

impl Switcher {
    fn step(
        &mut self,
        steps: i32,
        left: &Action,
        right: &Action,
        hold_cmd: bool,
        run: &mut dyn FnMut(&Action),
    ) {
        if steps == 0 {
            return;
        }
        if hold_cmd && !self.cmd_down {
            if let Err(e) = (self.cmd_key)(true) {
                log::warn!("{}", t!("events.action_failed", error = e));
                return;
            }
            self.cmd_down = true;
        }
        let action = if steps > 0 { right } else { left };
        for _ in 0..steps.abs() {
            run(action);
        }
    }

    fn release(&mut self) {
        if std::mem::take(&mut self.cmd_down) {
            let _ = (self.cmd_key)(false);
        }
    }
}

impl Drop for Switcher {
    fn drop(&mut self) {
        self.release();
    }
}

/// Runs an action on the mouse thread. The wheel mode toggle talks to the
/// mouse, so the check thread does it (see [`checks::Link`]).
fn run_action(dev: &Device, link: &checks::Link, action: &Action) {
    if *action == Action::ShiftWheelMode {
        link.request_wheel_toggle();
        return;
    }
    let start = Instant::now();
    let result = actions::run(action, Some(dev));
    log::info!(
        "action: {action:?} took_ms={} ok={}",
        start.elapsed().as_millis(),
        result.is_ok()
    );
    #[cfg(feature = "debug-access")]
    crate::debug::probe::note_action(crate::debug::probe::ActionRecord {
        action: format!("{action:?}"),
        ok: result.is_ok(),
        took_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
        at: std::time::SystemTime::now(),
    });
    match result {
        Ok(()) => {
            if let Some(dir) = spaces::direction(action) {
                spaces::note_step(dir);
            }
        }
        Err(e) => log::warn!("{}", t!("events.action_failed", error = e)),
    }
}

/// Buttons as `[0x0050,0x00C3]` for the log.
fn cid_list(cids: &[u16]) -> String {
    let items: Vec<String> = cids.iter().map(|c| format!("0x{c:04X}")).collect();
    format!("[{}]", items.join(","))
}

/// A gap in the event loop longer than the expected wait by this much is logged.
const STALL_MARGIN: Duration = Duration::from_millis(500);

/// Why [`session`] ended without an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// Ctrl-C / SIGTERM.
    Stop,
    /// The caller wants a fresh setup (new config, mouse woke up).
    Reload,
    /// The mouse is gone or unresponsive.
    Lost,
}

/// Checks that the config's buttons and gestures resolve to known actions.
pub fn validate(model: &Model, cfg: &Config) -> Result<()> {
    binding_set(model, cfg).map(|_| ())
}

/// `mouse buttons`: diverts the configured buttons and runs their actions
/// until Ctrl-C.
pub fn run() -> Result<()> {
    let (_, cfg) = crate::profiles::Store::open()?.load_current()?;
    let mut dev = Device::open_default()?;
    dev.refine_name();
    let Some(model) = devices::identify(dev.pid, &dev.name) else {
        bail!(t!("info.model_unknown"));
    };
    let b = binding_set(model, &cfg)?;
    if b.button_cids().is_empty() && b.gesture_cids().is_empty() {
        println!("{}", t!("events.nothing"));
        return Ok(());
    }
    println!("{}", t!("events.running"));
    session(
        &dev,
        model,
        &cfg,
        &mut || ControlFlow::Continue(()),
        &mut |_| ControlFlow::Continue(Next::default()),
    )?;
    println!("{}", t!("events.stopping"));
    Ok(())
}

/// Diverts the configured buttons and runs their actions until Ctrl-C,
/// `tick` or `checks` return an [`End`] or the device fails. `tick` runs on
/// this thread after every message from the mouse and at least once a
/// second; it must not talk to the mouse. `checks` runs on the check thread
/// (with the thumb wheel and button diversion checks), never while a button
/// is held or a gesture is open, and returns its nearest deadline.
/// Diversion is removed on `Stop` and `Reload`;
/// after `Lost` or an error the device is assumed unreachable and nothing
/// is written to it.
pub fn session(
    dev: &Device,
    model: &Model,
    cfg: &Config,
    tick: &mut dyn FnMut() -> ControlFlow<End>,
    checks: &mut Checks,
) -> Result<End> {
    let b = binding_set(model, cfg)?;
    let Some(idx) = dev.feature(controls::ID)? else {
        bail!(t!("events.unsupported"));
    };
    let mut guard = Restore { dev, idx, cids: Vec::new(), thumb: None };
    let result = divert_and_pump(dev, idx, cfg, &b, &mut guard, tick, checks);
    if !matches!(result, Ok(End::Stop | End::Reload)) {
        guard.cids.clear();
        guard.thumb = None;
    }
    drop(guard);
    result
}

/// Diverts the configured buttons; a button the mouse refuses to divert is
/// skipped, a lost link ends the session. Returns the gesture buttons, which
/// are diverted with raw movement reports.
fn divert_cids(dev: &Device, idx: u8, set: &BindingSet, guard: &mut Restore) -> Result<Vec<u16>> {
    for cid in set.button_cids() {
        guard.cids.push(cid);
        if let Err(e) = controls::set_reporting(dev, idx, cid, true, false) {
            if is_link_failure(&e) {
                return Err(e);
            }
            log::warn!("cannot divert button 0x{cid:04X}: {e}");
            guard.cids.pop();
        }
    }
    let mut gesture_cids = Vec::new();
    for cid in set.gesture_cids() {
        guard.cids.push(cid);
        if let Err(e) = controls::set_reporting(dev, idx, cid, true, true) {
            if is_link_failure(&e) {
                return Err(e);
            }
            log::warn!("cannot divert gesture button 0x{cid:04X}: {e}");
            guard.cids.pop();
            continue;
        }
        gesture_cids.push(cid);
    }
    Ok(gesture_cids)
}

/// Minimal time between two warnings of the same kind about lost button events.
const EVENT_WARN_EVERY: Duration = Duration::from_secs(10);
/// A button "held" this long without any button or movement notification is
/// taken as released (its release was lost). Longer than a normal hold, even
/// at the app switcher, and no longer than the series of checks it blocks.
const HELD_MAX: Duration = Duration::from_secs(60);

/// Movement reported this soon after the gesture button was pressed is
/// ignored: the first report after a press carries the rest of the movement
/// from before it (2-33 ms after the press, often in the direction of the
/// previous gesture, one report of 200-300 units), which made a false step
/// or a false "down". The skipped movement is logged as `skipped=`.
const PRESS_SETTLE: Duration = Duration::from_millis(40);

/// State of the gesture button being held and the buttons pressed so far.
struct GestureState<'a> {
    gesture_cids: Vec<u16>,
    tracker: Tracker,
    /// The gesture button being held and its actions (one button at a time;
    /// others pressed meanwhile are ignored). The actions come from the
    /// application that was frontmost when the button was pressed.
    active: Option<(u16, &'a GestureBinding)>,
    pair: Option<(Action, Action)>,
    hold_cmd: bool,
    switcher: Switcher,
    held: Vec<u16>,
    /// When the current gesture started and how many movement reports it had.
    started: Instant,
    moves: u32,
    /// Movement ignored in the first [`PRESS_SETTLE`] of the gesture: the
    /// number of reports and their sums.
    skipped: (u32, i32, i32),
    /// When the last button or movement notification arrived.
    last_event: Instant,
    lost_log: Throttle,
    move_log: Throttle,
}

impl<'a> GestureState<'a> {
    fn new(gesture_cids: Vec<u16>) -> Self {
        GestureState {
            gesture_cids,
            tracker: Tracker::new(DEFAULT_THRESHOLD),
            active: None,
            pair: None,
            hold_cmd: false,
            switcher: Switcher::default(),
            held: Vec::new(),
            started: Instant::now(),
            moves: 0,
            skipped: (0, 0, 0),
            last_event: Instant::now(),
            lost_log: Throttle::new(EVENT_WARN_EVERY),
            move_log: Throttle::new(EVENT_WARN_EVERY),
        }
    }

    /// True while a button is held or a gesture is open.
    fn busy(&self) -> bool {
        !self.held.is_empty() || self.active.is_some()
    }

    /// Ends the open gesture without running its action: the release of its
    /// button is lost or presumed lost. Cmd of the app switcher is released.
    /// Returns true if a gesture was open.
    fn drop_gesture(&mut self, at: Instant) -> bool {
        let Some((cid, _)) = self.active.take() else {
            return false;
        };
        self.switcher.release();
        let _ = self.tracker.release();
        let (x, y) = self.tracker.sums();
        log::info!(
            "gesture end: button=0x{cid:04X} result=dropped moves={} x={x} y={y} skipped={} skip_x={} skip_y={} took_ms={}",
            self.moves,
            self.skipped.0,
            self.skipped.1,
            self.skipped.2,
            at.saturating_duration_since(self.started).as_millis()
        );
        #[cfg(feature = "debug-access")]
        self.note_end(cid, "dropped", at);
        true
    }

    /// Gives up a button that has been "held" for [`HELD_MAX`] without a
    /// single notification: its release was most likely lost, and the held
    /// state would block the periodic checks (and keep Cmd down) for good.
    fn expire(&mut self, at: Instant) {
        if !self.busy() || at.saturating_duration_since(self.last_event) < HELD_MAX {
            return;
        }
        let dropped = self.drop_gesture(at);
        log::warn!(
            "button held without events: held={} for_s={} gesture_dropped={dropped}; assuming the release was lost",
            cid_list(&self.held),
            at.saturating_duration_since(self.last_event).as_secs()
        );
        self.held.clear();
    }

    /// Handles the list of buttons now held, `at` being the time it arrived.
    /// Actions are run through `run`.
    fn buttons(
        &mut self,
        set: &'a BindingSet,
        now: Vec<u16>,
        at: Instant,
        run: &mut dyn FnMut(&Action),
    ) {
        self.last_event = at;
        // The mouse reports a change only, so the same list again means an
        // event was lost: the release (and this is a new press) or the
        // press of a button released since. Treat it as a new press.
        if !now.is_empty() && now == self.held {
            if self.lost_log.allow(at) {
                log::warn!(
                    "button event lost: held={} sent again; ending the previous press without action, treating this as a new press",
                    cid_list(&now)
                );
            }
            self.drop_gesture(at);
            self.held.clear();
        }
        if now != self.held {
            log::info!("buttons: held={}", cid_list(&now));
        }
        for &cid in now.iter().filter(|c| !self.held.contains(c)) {
            let bound = set.current();
            if self.gesture_cids.contains(&cid)
                && let Some(cur) = bound.gestures.get(&cid)
            {
                if self.active.is_some() {
                    continue;
                }
                self.active = Some((cid, cur));
                // Left and right both set: repeat the action per
                // `threshold` of movement while the button is held.
                // Cmd is held too for the app switcher.
                self.pair = match (cur.actions.get("left"), cur.actions.get("right")) {
                    (Some(l), Some(r)) => Some((l.clone(), r.clone())),
                    _ => None,
                };
                self.hold_cmd = self
                    .pair
                    .as_ref()
                    .is_some_and(|(l, r)| is_switcher(Some(l)) && is_switcher(Some(r)));
                self.tracker = Tracker::new(cur.threshold);
                self.tracker.set_continuous(self.pair.is_some());
                self.tracker.press();
                self.started = at;
                self.moves = 0;
                self.skipped = (0, 0, 0);
                log::info!(
                    "gesture start: button=0x{cid:04X} mode={}",
                    if self.pair.is_some() { "repeat" } else { "single" }
                );
            } else if let Some(a) = bound.buttons.get(&cid) {
                run(a);
            }
        }
        if let Some((cid, cur)) = self.active
            && !now.contains(&cid)
        {
            self.active = None;
            self.switcher.release();
            let gesture = self.tracker.release();
            let (x, y) = self.tracker.sums();
            let result = match gesture {
                Some(g) => g.key(),
                None if self.tracker.ambiguous() => "ambiguous",
                None => "none",
            };
            log::info!(
                "gesture end: button=0x{cid:04X} result={result} moves={} x={x} y={y} skipped={} skip_x={} skip_y={} took_ms={}",
                self.moves,
                self.skipped.0,
                self.skipped.1,
                self.skipped.2,
                at.saturating_duration_since(self.started).as_millis()
            );
            #[cfg(feature = "debug-access")]
            self.note_end(cid, result, at);
            if let Some(g) = gesture
                && let Some(a) = cur.actions.get(g.key())
            {
                run(a);
            }
        }
        self.held = now;
    }

    /// Test build: keeps the end of the gesture for `debug state`.
    #[cfg(feature = "debug-access")]
    fn note_end(&self, cid: u16, result: &'static str, at: Instant) {
        let (x, y) = self.tracker.sums();
        crate::debug::probe::note_gesture(crate::debug::probe::GestureRecord {
            button: cid,
            result,
            moves: self.moves,
            x,
            y,
            skipped: self.skipped.0,
            took_ms: u64::try_from(at.saturating_duration_since(self.started).as_millis())
                .unwrap_or(u64::MAX),
            at: std::time::SystemTime::now(),
        });
    }

    /// Test build: handles an event played by `debug gesture` as if the
    /// mouse had sent it.
    #[cfg(feature = "debug-access")]
    fn inject(
        &mut self,
        set: &'a BindingSet,
        ev: crate::debug::proto::Inject,
        run: &mut dyn FnMut(&Action),
    ) {
        use crate::debug::proto::Inject;
        let now = Instant::now();
        match ev {
            Inject::Press(cid) => {
                let mut held = self.held.clone();
                if !held.contains(&cid) {
                    held.push(cid);
                }
                self.buttons(set, held, now, run);
            }
            Inject::Release(cid) => {
                let held = self.held.iter().copied().filter(|&c| c != cid).collect();
                self.buttons(set, held, now, run);
            }
            Inject::Move(dx, dy) => self.motion(dx, dy, now, run),
        }
    }

    /// Handles raw movement of the held gesture button.
    fn motion(&mut self, dx: i16, dy: i16, at: Instant, run: &mut dyn FnMut(&Action)) {
        self.last_event = at;
        if self.active.is_none() {
            if self.move_log.allow(at) {
                log::warn!(
                    "move without press: dx={dx} dy={dy} held={}; the press of the gesture button was probably lost, movement ignored",
                    cid_list(&self.held)
                );
            }
            return;
        }
        if at.saturating_duration_since(self.started) < PRESS_SETTLE {
            self.skipped.0 += 1;
            self.skipped.1 = self.skipped.1.saturating_add(i32::from(dx));
            self.skipped.2 = self.skipped.2.saturating_add(i32::from(dy));
            return;
        }
        self.moves += 1;
        let steps = self.tracker.motion(dx, dy);
        if let Some((l, r)) = &self.pair {
            self.switcher.step(steps, l, r, self.hold_cmd, run);
        }
    }
}

/// How often the traffic summary is logged.
const TRAFFIC_EVERY: Duration = Duration::from_secs(60);
/// How often it is logged while the mouse sent nothing at all.
const TRAFFIC_IDLE_EVERY: Duration = Duration::from_secs(600);

/// Counts what the mouse sends, to tell later whether it sent anything while
/// a gesture did not work. Logged once a minute as `mouse traffic`
/// (every 10 minutes while all counters are zero).
struct Traffic {
    next_log: Instant,
    /// Earliest time for a line with only zeros.
    next_idle_log: Instant,
    /// HID++ messages for this mouse.
    messages: u32,
    buttons: u32,
    moves: u32,
    thumb: u32,
    last_button: Option<Instant>,
}

impl Traffic {
    fn new(now: Instant) -> Self {
        Traffic {
            next_log: now + TRAFFIC_EVERY,
            next_idle_log: now + TRAFFIC_IDLE_EVERY,
            messages: 0, buttons: 0, moves: 0, thumb: 0,
            last_button: None,
        }
    }

    /// The summary line if it is due, and starts a new period.
    fn line(&mut self, now: Instant, link: impl FnOnce() -> LinkStats) -> Option<String> {
        if now < self.next_log {
            return None;
        }
        self.next_log = now + TRAFFIC_EVERY;
        let link = link();
        if self.messages == 0 && link == LinkStats::default() && now < self.next_idle_log {
            return None;
        }
        self.next_idle_log = now + TRAFFIC_IDLE_EVERY;
        let other = self.messages.saturating_sub(self.buttons + self.moves + self.thumb);
        let last = self.last_button.map_or("none".to_string(), |t| {
            now.saturating_duration_since(t).as_secs().to_string()
        });
        let line = format!(
            "mouse traffic: messages={} buttons={} moves={} thumb={} other={other} last_button_s={last} non_hidpp={} non_hidpp_id={} other_index={} queue_dropped={} max_write_ms={}",
            self.messages,
            self.buttons,
            self.moves,
            self.thumb,
            link.non_hidpp,
            link.non_hidpp_id.map_or("none".to_string(), |id| format!("{id:#04x}")),
            link.other_index,
            link.queue_dropped,
            link.max_write.as_millis()
        );
        self.messages = 0;
        self.buttons = 0;
        self.moves = 0;
        self.thumb = 0;
        Some(line)
    }
}

fn divert_and_pump(
    dev: &Device,
    idx: u8,
    cfg: &Config,
    set: &BindingSet,
    guard: &mut Restore,
    tick: &mut dyn FnMut() -> ControlFlow<End>,
    checks: &mut Checks,
) -> Result<End> {
    let gesture_cids = divert_cids(dev, idx, set, guard)?;
    let mut thumb = Thumb::start(dev, cfg, guard)?;
    let targets = guard.cids.iter().map(|&c| (c, gesture_cids.contains(&c))).collect();
    let thumb_check = thumb.as_ref().map(Thumb::checker);
    let checker = Checker::new(idx, thumb_check, targets, checks, Instant::now());
    // Without 0x1D4B a power-cycled mouse is only noticed by the caller's
    // health check (`checks`).
    let wireless_idx = wireless::find(dev)?;
    log::debug!("wireless status feature index: {wireless_idx:?}");
    // A stop request wakes the loop at once, so the buttons are given back
    // without waiting for the next poll.
    let interrupter = dev.interrupter();
    let _wake = shutdown::on_stop_while(move || interrupter.interrupt());
    let link = checks::Link::default();
    let link = &link;
    std::thread::scope(|s| {
        let check_thread = std::thread::Builder::new()
            .name("mouse-checks".into())
            .spawn_scoped(s, move || checker.run(dev, link))?;
        let mouse = Mouse { dev, idx, set, wireless_idx, link };
        let result = mouse.pump(GestureState::new(gesture_cids), &mut thumb, tick);
        // A request the check thread waits for is answered quickly (or
        // times out); the buttons are restored after it.
        link.quit();
        if check_thread.join().is_err() {
            log::error!("the check thread panicked");
        }
        result
    })
}

/// What the loop of the mouse thread works with.
struct Mouse<'a> {
    dev: &'a Device,
    /// Feature index of the button controls (0x1B04).
    idx: u8,
    set: &'a BindingSet,
    wireless_idx: Option<u8>,
    link: &'a checks::Link,
}

impl<'a> Mouse<'a> {
    /// Handles notifications until the session ends.
    fn pump(
        &self,
        mut state: GestureState<'a>,
        thumb: &mut Option<Thumb>,
        tick: &mut dyn FnMut() -> ControlFlow<End>,
    ) -> Result<End> {
        let (dev, idx, set, link) = (self.dev, self.idx, self.set, self.link);
        #[cfg(feature = "debug-access")]
        let injection =
            crate::debug::probe::Injection::register(&dev.name, &state.gesture_cids, dev.interrupter());
        let mut traffic = Traffic::new(Instant::now());
        // End of the previous read: the time from there to the next read is
        // spent in our own work (ticks, actions).
        let mut work_from = Instant::now();
        loop {
            if shutdown::stopped() {
                return Ok(End::Stop);
            }
            if let Some(t) = thumb.as_mut() {
                t.expire(Instant::now());
            }
            state.expire(Instant::now());
            #[cfg(feature = "debug-access")]
            for ev in injection.drain() {
                state.inject(set, ev, &mut |a| run_action(dev, link, a));
            }
            link.set_busy(state.busy() || thumb.as_ref().is_some_and(|t| t.busy()));
            if let ControlFlow::Break(end) = tick() {
                return Ok(end);
            }
            if let Some(end) = link.take_end() {
                return end;
            }
            if let Some(line) = traffic.line(Instant::now(), || dev.take_link_stats()) {
                log::info!("{line}");
            }
            // An open scroll gesture must be ended soon after the last rotation.
            let timeout = match thumb.as_ref() {
                Some(t) if t.busy() => FAST_POLL_MS,
                _ => wait_ms(Instant::now(), Some(traffic.next_log)),
            };
            let read_start = Instant::now();
            let work = read_start.duration_since(work_from);
            if work > STALL_MARGIN {
                log::warn!("loop stall: phase=work took_ms={}", work.as_millis());
            }
            let msg = dev.read_message(timeout)?;
            work_from = Instant::now();
            let waited = work_from.duration_since(read_start);
            let expected = Duration::from_millis(u64::try_from(timeout).unwrap_or(0));
            if waited > expected + STALL_MARGIN {
                log::warn!(
                    "loop stall: phase=read took_ms={} expected_ms={timeout}",
                    waited.as_millis()
                );
            }
            let Some(msg) = msg else {
                continue;
            };
            traffic.messages += 1;
            log::debug!(
                "hid++ in: feature {:#04x} function {} sw_id {} params {:02x?}",
                msg.feature_index,
                msg.function,
                msg.sw_id,
                msg.params
            );
            match connection::parse_connection(&msg) {
                Some(Link::Connected) => {
                    log::info!("mouse connected (receiver notification)");
                    return Ok(End::Reload);
                }
                Some(Link::Lost) => {
                    log::info!("mouse link lost (receiver notification)");
                    continue;
                }
                None => {}
            }
            if let Some(s) = self.wireless_idx.and_then(|i| wireless::parse_event(i, &msg)) {
                let why = if s.power_switch() { " (power switch)" } else { "" };
                log::info!(
                    "wireless status: status {} request {} reason {}{why}",
                    s.status,
                    s.request,
                    s.reason
                );
                if s.needs_setup() {
                    log::info!("wireless status: mouse reconnected, re-applying settings");
                    return Ok(End::Reload);
                }
                continue;
            }
            if let Some(t) = thumb.as_mut()
                && t.handle(&msg, Instant::now())
            {
                traffic.thumb += 1;
                continue;
            }
            // The response to a setCidReporting (of another program, which may
            // use one of our software IDs): the check thread restores our
            // diversion if it took it away. Our own responses never do.
            if let Some(set) = controls::parse_foreign_set(idx, &msg) {
                log::debug!("foreign setCidReporting: {set:?}");
                link.report_foreign(set);
                continue;
            }
            match controls::parse_event(idx, &msg) {
                Some(Event::Buttons(now)) => {
                    traffic.buttons += 1;
                    traffic.last_button = Some(Instant::now());
                    link.note_notification(Instant::now());
                    state.buttons(set, now, Instant::now(), &mut |a| run_action(dev, link, a));
                }
                Some(Event::Move { dx, dy }) => {
                    traffic.moves += 1;
                    link.note_notification(Instant::now());
                    state.motion(dx, dy, Instant::now(), &mut |a| run_action(dev, link, a));
                }
                None => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_app_switcher() {
        let a = actions::parse("shortcut:cmd+tab").unwrap();
        let b = actions::parse("shortcut:cmd+shift+tab").unwrap();
        assert!(is_switcher(Some(&a)) && is_switcher(Some(&b)));
        assert!(!is_switcher(Some(&actions::parse("desktop_left").unwrap())));
        assert!(!is_switcher(None));
    }

    #[test]
    fn waits_until_the_nearest_deadline_but_at_most_a_second() {
        let now = Instant::now();
        let after = |ms| Some(now + Duration::from_millis(ms));
        assert_eq!(wait_ms(now, None), 1000);
        assert_eq!(wait_ms(now, after(60_000)), 1000);
        assert_eq!(wait_ms(now, after(300)), 300);
        // Rounded up: never a wake-up just before the deadline.
        assert_eq!(wait_ms(now, Some(now + Duration::from_micros(300_100))), 301);
        // A deadline that has passed: no wait, but no negative either.
        assert_eq!(wait_ms(now, Some(now)), 0);
        assert_eq!(wait_ms(now + Duration::from_secs(5), Some(now)), 0);
    }

    #[test]
    fn traffic_line_is_logged_once_a_minute_and_counts_restart() {
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        let link = || LinkStats {
            non_hidpp: 7,
            non_hidpp_id: Some(0x02),
            other_index: 1,
            queue_dropped: 4,
            max_write: Duration::from_millis(480),
        };
        let mut t = Traffic::new(t0);
        assert_eq!(t.line(at(59), link), None);
        t.messages = 20;
        t.buttons = 2;
        t.moves = 10;
        t.thumb = 3;
        t.last_button = Some(at(40));
        assert_eq!(
            t.line(at(60), link).unwrap(),
            "mouse traffic: messages=20 buttons=2 moves=10 thumb=3 other=5 last_button_s=20 non_hidpp=7 non_hidpp_id=0x02 other_index=1 queue_dropped=4 max_write_ms=480"
        );
        // Counters start over; last button is kept; the next line is a minute later.
        assert_eq!(t.line(at(119), link), None);
        // Only zeros: not before 10 minutes after the previous line.
        assert_eq!(t.line(at(120), LinkStats::default), None);
        assert_eq!(t.line(at(600), LinkStats::default), None);
        assert_eq!(
            t.line(at(660), LinkStats::default).unwrap(),
            "mouse traffic: messages=0 buttons=0 moves=0 thumb=0 other=0 last_button_s=620 non_hidpp=0 non_hidpp_id=none other_index=0 queue_dropped=0 max_write_ms=0"
        );
        // Any traffic brings back the minute interval.
        t.messages = 1;
        assert!(t.line(at(720), LinkStats::default).is_some());
    }

    #[test]
    fn traffic_idle_line_waits_ten_minutes_from_start() {
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        let mut t = Traffic::new(t0);
        assert_eq!(t.line(at(60), LinkStats::default), None);
        assert_eq!(t.line(at(540), LinkStats::default), None);
        assert!(t.line(at(600), LinkStats::default).unwrap().contains("last_button_s=none"));
        // Link problems alone also count as traffic.
        let bad = || LinkStats { queue_dropped: 1, ..LinkStats::default() };
        assert!(t.line(at(660), bad).is_some());
    }

    const THUMB: u16 = 0x00C3;
    const BACK: u16 = 0x0053;
    const MIDDLE: u16 = 0x0052;

    thread_local! {
        /// Calls of the replaced `cmd_key` in this test: true = down.
        static CMD_CALLS: std::cell::RefCell<Vec<bool>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    fn fake_cmd_key(down: bool) -> Result<()> {
        CMD_CALLS.with(|c| c.borrow_mut().push(down));
        Ok(())
    }

    fn cmd_calls() -> Vec<bool> {
        CMD_CALLS.with(|c| c.borrow().clone())
    }

    fn set_for(text: &str) -> BindingSet {
        let cfg = Config::parse(text, std::path::Path::new("t.toml")).unwrap();
        binding_set(&devices::mx_master_3s::MODEL, &cfg).unwrap()
    }

    const SWITCHER: &str =
        "[gestures]\nthreshold = 50\nleft = \"shortcut:cmd+shift+tab\"\nright = \"shortcut:cmd+tab\"\n";
    const SINGLE: &str = "[buttons]\nback = \"mission_control\"\n[gestures]\nthreshold = 50\nclick = \"app_expose\"\n";

    /// Click and Down are different actions, to tell a click from a false "down".
    const CLICK_AND_DOWN: &str =
        "[gestures]\nthreshold = 50\nclick = \"shortcut:cmd+a\"\ndown = \"shortcut:cmd+b\"\n";

    fn new_state() -> GestureState<'static> {
        let mut s = GestureState::new(vec![THUMB]);
        s.switcher.cmd_key = fake_cmd_key;
        s
    }

    /// Runs `buttons`/`motion` and collects the actions they ran.
    struct Rig {
        actions: Vec<String>,
        at: Instant,
    }

    impl Rig {
        fn new() -> Self {
            Rig { actions: Vec::new(), at: Instant::now() }
        }

        fn buttons<'a>(&mut self, s: &mut GestureState<'a>, set: &'a BindingSet, now: &[u16]) {
            let actions = &mut self.actions;
            s.buttons(set, now.to_vec(), self.at, &mut |a| actions.push(format!("{a:?}")));
            // Movement right after a press is ignored: let it settle.
            self.at += PRESS_SETTLE + Duration::from_millis(10);
        }

        fn motion(&mut self, s: &mut GestureState, dx: i16) {
            self.motion_xy(s, dx, 0);
        }

        fn motion_xy(&mut self, s: &mut GestureState, dx: i16, dy: i16) {
            let actions = &mut self.actions;
            s.motion(dx, dy, self.at, &mut |a| actions.push(format!("{a:?}")));
        }
    }

    #[test]
    fn a_normal_press_and_release_runs_the_action_once() {
        let set = set_for(SINGLE);
        let (mut s, mut rig) = (new_state(), Rig::new());
        rig.buttons(&mut s, &set, &[THUMB]);
        assert!(s.active.is_some() && s.busy());
        rig.buttons(&mut s, &set, &[]);
        assert!(s.active.is_none() && !s.busy());
        assert_eq!(rig.actions.len(), 1); // click
    }

    /// Presses the thumb button and returns the time of the press.
    fn press<'a>(s: &mut GestureState<'a>, rig: &mut Rig, set: &'a BindingSet) -> Instant {
        let t = rig.at;
        rig.buttons(s, set, &[THUMB]);
        t
    }

    #[test]
    fn a_big_move_right_after_the_press_is_skipped_and_gives_no_gesture() {
        let set = set_for(CLICK_AND_DOWN);
        let (mut s, mut rig) = (new_state(), Rig::new());
        let t0 = press(&mut s, &mut rig, &set);
        rig.at = t0 + Duration::from_millis(10);
        rig.motion_xy(&mut s, 0, 200); // would be "down"
        assert_eq!((s.moves, s.skipped), (0, (1, 0, 200)));
        assert_eq!(s.tracker.sums(), (0, 0));
        rig.at = t0 + Duration::from_millis(60);
        rig.motion_xy(&mut s, 3, 2);
        assert_eq!(s.moves, 1);
        rig.buttons(&mut s, &set, &[]);
        assert_eq!(rig.actions.len(), 1, "a click, not Down");
        assert!(rig.actions[0].contains("code: 0,"), "{:?}", rig.actions); // cmd+a, not cmd+b
    }

    #[test]
    fn a_normal_move_after_the_settle_time_counts_as_before() {
        let set = set_for(SWITCHER);
        let (mut s, mut rig) = (new_state(), Rig::new());
        let t0 = press(&mut s, &mut rig, &set);
        rig.at = t0 + Duration::from_millis(50);
        rig.motion(&mut s, 60);
        assert_eq!((s.moves, s.skipped.0), (1, 0));
        assert_eq!(rig.actions.len(), 1, "the first step");
        assert_eq!(s.tracker.sums(), (60, 0));
        // Just before the limit it is still skipped, at the limit it counts.
        rig.buttons(&mut s, &set, &[]);
        let t1 = press(&mut s, &mut rig, &set);
        rig.at = t1 + PRESS_SETTLE - Duration::from_millis(1);
        rig.motion(&mut s, 60);
        assert_eq!((s.moves, s.skipped.0), (0, 1));
        rig.at = t1 + PRESS_SETTLE;
        rig.motion(&mut s, 60);
        assert_eq!((s.moves, s.skipped.0), (1, 1));
        rig.buttons(&mut s, &set, &[]);
    }

    #[test]
    fn a_vertical_gesture_needs_several_reports() {
        let set = set_for(CLICK_AND_DOWN);
        let (mut s, mut rig) = (new_state(), Rig::new());
        // One report of y=200 after the settle time: no "down" (a click
        // is not made of it either: the movement is over the threshold).
        let t0 = press(&mut s, &mut rig, &set);
        rig.at = t0 + Duration::from_millis(50);
        rig.motion_xy(&mut s, -5, 200);
        assert_eq!(s.moves, 1);
        rig.at = t0 + Duration::from_millis(129);
        rig.buttons(&mut s, &set, &[]);
        assert!(rig.actions.is_empty(), "{:?}", rig.actions);
        // Six reports of 40 units down: "down".
        let t1 = press(&mut s, &mut rig, &set);
        for i in 0..6 {
            rig.at = t1 + Duration::from_millis(50 + 10 * i);
            rig.motion_xy(&mut s, 0, 40);
        }
        assert_eq!(s.moves, 6);
        rig.buttons(&mut s, &set, &[]);
        assert_eq!(rig.actions.len(), 1);
        assert!(rig.actions[0].contains("code: 11,"), "{:?}", rig.actions); // cmd+b
    }

    #[test]
    fn a_click_with_false_movement_stays_a_click() {
        let set = set_for(CLICK_AND_DOWN);
        let (mut s, mut rig) = (new_state(), Rig::new());
        let t0 = press(&mut s, &mut rig, &set);
        rig.at = t0 + Duration::from_millis(7);
        rig.motion_xy(&mut s, 318, 0);
        rig.at = t0 + Duration::from_millis(120);
        rig.buttons(&mut s, &set, &[]);
        assert_eq!(rig.actions.len(), 1);
        assert!(rig.actions[0].contains("code: 0,"), "{:?}", rig.actions);
        assert_eq!(s.skipped, (1, 318, 0));
    }

    #[test]
    fn a_repeated_list_is_a_new_press_and_the_old_gesture_ends_without_action() {
        let set = set_for(SINGLE);
        let (mut s, mut rig) = (new_state(), Rig::new());
        rig.buttons(&mut s, &set, &[THUMB]);
        rig.motion(&mut s, 3);
        assert_eq!(s.moves, 1);
        // The release was lost: the next press brings the same list again.
        rig.buttons(&mut s, &set, &[THUMB]);
        assert!(rig.actions.is_empty(), "the old gesture must not click");
        assert!(s.active.is_some());
        assert_eq!(s.moves, 0, "a fresh gesture");
        assert_eq!(s.held, [THUMB]);
        // Its release now ends it normally (click).
        rig.buttons(&mut s, &set, &[]);
        assert_eq!(rig.actions.len(), 1);
        assert!(!s.busy());
    }

    #[test]
    fn a_repeated_list_presses_a_plain_button_again() {
        let set = set_for(SINGLE);
        let (mut s, mut rig) = (new_state(), Rig::new());
        rig.buttons(&mut s, &set, &[BACK]);
        rig.buttons(&mut s, &set, &[BACK]); // release lost, pressed again
        assert_eq!(rig.actions.len(), 2);
        // Other lists are not repeats: a second (unbound) button joins, then leaves.
        rig.buttons(&mut s, &set, &[BACK, 0x0052]);
        rig.buttons(&mut s, &set, &[BACK]);
        assert_eq!(rig.actions.len(), 2);
    }

    #[test]
    fn the_warning_about_a_lost_event_is_limited_to_once_per_ten_seconds() {
        let set = set_for(SINGLE);
        let (mut s, mut rig) = (new_state(), Rig::new());
        rig.buttons(&mut s, &set, &[BACK]);
        rig.buttons(&mut s, &set, &[BACK]); // warns
        let later = rig.at + Duration::from_secs(9);
        assert!(!s.lost_log.allow(later));
        assert!(s.lost_log.allow(rig.at + Duration::from_secs(10)));
    }

    #[test]
    fn movement_without_a_press_is_ignored() {
        let set = set_for(SWITCHER);
        let (mut s, mut rig) = (new_state(), Rig::new());
        rig.motion(&mut s, 200);
        assert!(rig.actions.is_empty() && cmd_calls().is_empty());
        assert_eq!(s.moves, 0);
        assert!(!s.busy());
        // The warning was throttled just now; a press afterwards works.
        assert!(!s.move_log.allow(rig.at + Duration::from_secs(9)));
        rig.buttons(&mut s, &set, &[THUMB]);
        rig.motion(&mut s, 200);
        assert_eq!(rig.actions.len(), 1);
    }

    #[test]
    fn a_lost_release_at_the_app_switcher_lets_go_of_cmd() {
        let set = set_for(SWITCHER);
        let (mut s, mut rig) = (new_state(), Rig::new());
        rig.buttons(&mut s, &set, &[THUMB]);
        rig.motion(&mut s, 60);
        assert_eq!(cmd_calls(), [true], "Cmd goes down with the first step");
        assert_eq!(rig.actions.len(), 1);
        // The release is lost; the next press repeats the list.
        rig.buttons(&mut s, &set, &[THUMB]);
        assert_eq!(cmd_calls(), [true, false], "Cmd released");
        assert!(!s.switcher.cmd_down);
        assert_eq!(rig.actions.len(), 1, "no action from the old gesture");
        // The new gesture works: Cmd goes down again and comes up on release.
        rig.motion(&mut s, 60);
        assert_eq!(cmd_calls(), [true, false, true]);
        rig.buttons(&mut s, &set, &[]);
        assert_eq!(cmd_calls(), [true, false, true, false]);
        assert!(!s.busy());
    }

    #[test]
    fn a_button_held_for_a_minute_without_events_is_taken_as_released() {
        let set = set_for(SWITCHER);
        let (mut s, mut rig) = (new_state(), Rig::new());
        rig.buttons(&mut s, &set, &[THUMB]);
        rig.motion(&mut s, 60);
        let t0 = rig.at;
        s.expire(t0 + Duration::from_secs(59));
        assert!(s.busy(), "a long hold is fine");
        assert_eq!(cmd_calls(), [true]);
        s.expire(t0 + Duration::from_secs(60));
        assert!(!s.busy());
        assert_eq!(cmd_calls(), [true, false]);
        // The real release later changes nothing.
        let n = rig.actions.len();
        rig.at = t0 + Duration::from_secs(90);
        rig.buttons(&mut s, &set, &[]);
        assert_eq!(rig.actions.len(), n);
    }

    #[test]
    fn any_notification_keeps_a_long_hold_alive() {
        let set = set_for(SWITCHER);
        let (mut s, mut rig) = (new_state(), Rig::new());
        rig.buttons(&mut s, &set, &[THUMB]);
        let t0 = rig.at;
        rig.at = t0 + Duration::from_secs(50);
        rig.motion(&mut s, 1);
        s.expire(t0 + Duration::from_secs(100));
        assert!(s.busy());
        s.expire(t0 + Duration::from_secs(110));
        assert!(!s.busy());
    }

    /// A button notification of the controls feature, as the mouse sends it.
    fn button_report(held: &[u16]) -> Vec<u8> {
        use crate::hidpp::message::Message;
        let params: Vec<u8> = held.iter().flat_map(|c| c.to_be_bytes()).collect();
        Message::request(0xFF, CONTROLS_IDX, 0, 0, &params).unwrap().to_bytes()
    }

    const CONTROLS_IDX: u8 = 9;

    /// A quick series of presses and releases arrives while the check thread
    /// waits for the mouse to answer its read of the button diversion. The
    /// loop handles every one at once, the lost diversion is not written
    /// while a button is held, and the checks (the caller's too) wait for
    /// the quiet. The mouse is played by the script thread: it answers a
    /// request only when it wants to.
    #[test]
    fn a_fast_series_of_gestures_is_handled_at_once_and_the_checks_wait_for_the_quiet() {
        use crate::hidpp::message::Message;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Mutex, mpsc};
        use std::thread;

        /// Pause between two button events: tens of ms, as a fast hand.
        const GAP: Duration = Duration::from_millis(30);
        const QUIET: Duration = Duration::from_millis(150);

        let set = set_for("[buttons]\nmiddle = \"gestures\"\n[gestures.middle]\nthreshold = 50\n");
        assert!(
            set.current().gestures[&MIDDLE].actions.is_empty(),
            "the gesture must not run any action in this test"
        );
        let (to_mouse, requests) = mpsc::channel::<Message>();
        let (dev, feed) = Device::fake(move |raw| to_mouse.send(Message::parse(raw).unwrap()).unwrap());
        let link = checks::Link::default();
        // The caller's checks: whether a gesture was open at each call.
        let calls = Mutex::new(Vec::new());
        let mut caller = |_: &Device| {
            let mut calls = calls.lock().unwrap();
            let first = calls.is_empty();
            calls.push(link.is_busy());
            ControlFlow::Continue(Next { deadline: None, contacted: first })
        };
        let done = AtomicBool::new(false);
        let mut tick = || {
            if done.load(Ordering::SeqCst) { ControlFlow::Break(End::Stop) } else { ControlFlow::Continue(()) }
        };
        let eventually = |f: &dyn Fn() -> bool| {
            let start = Instant::now();
            while !f() {
                assert!(start.elapsed() < Duration::from_secs(5), "the loop did not follow the mouse");
                thread::sleep(Duration::from_millis(2));
            }
        };

        let script = {
            let (link, calls, feed, eventually) = (&link, &calls, &feed, &eventually);
            move || {
            let next = || requests.recv_timeout(Duration::from_secs(5)).expect("a request of the checks");
            let read = || {
                let req = next();
                assert_eq!((req.feature_index, req.function), (CONTROLS_IDX, 2), "a read");
                req
            };
            let answer = |req: &Message| {
                let reply = Message::request(req.device_index, req.feature_index, req.function, req.sw_id, &req.params);
                feed.push(&reply.unwrap().to_bytes());
            };
            // The mouse thread must follow each event at once, although the
            // check thread waits for an answer all the time.
            let hold = |held: bool| {
                feed.push(&button_report(if held { &[MIDDLE] } else { &[] }));
                eventually(&|| link.is_busy() == held);
                thread::sleep(GAP);
            };
            let silence = |what: &str| match requests.recv_timeout(QUIET) {
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                other => panic!("{what}: {other:?}"),
            };
            let rapid_series = || {
                for _ in 0..2 {
                    hold(true);
                    hold(false);
                }
                hold(true);
            };
            // The first pass reads the diversion; the answer is slow.
            let r1 = read();
            rapid_series();
            // The button is still held: the lost diversion is not written.
            answer(&r1);
            silence("a write during a gesture (1)");
            assert_eq!(*calls.lock().unwrap(), [false], "no check while a button is held (1)");
            // The release wakes the check thread: it reads again at once.
            hold(false);
            let r2 = read();
            rapid_series();
            answer(&r2);
            silence("a write during a gesture (2)");
            assert_eq!(*calls.lock().unwrap(), [false, false], "no check while a button is held (2)");
            // The quiet: the third read, then the diversion, written once.
            hold(false);
            let r3 = read();
            answer(&r3);
            let divert = next();
            assert_eq!((divert.feature_index, divert.function), (CONTROLS_IDX, 3), "divert again");
            assert_eq!(divert.params[..5], controls::encode_reporting(MIDDLE, true, true));
            answer(&divert);
            silence("more writes after the diversion was restored");
            }
        };

        let result = thread::scope(|s| {
            let checker = Checker::new(CONTROLS_IDX, None, vec![(MIDDLE, true)], &mut caller, Instant::now());
            let check_thread = s.spawn(|| checker.run(&dev, &link));
            let script = s.spawn(|| {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(script));
                // Whatever happened, the loop must end.
                done.store(true, Ordering::SeqCst);
                dev.interrupter().interrupt();
                outcome
            });
            let mouse = Mouse { dev: &dev, idx: CONTROLS_IDX, set: &set, wireless_idx: None, link: &link };
            let result = mouse.pump(GestureState::new(vec![MIDDLE]), &mut None, &mut tick);
            link.quit();
            check_thread.join().unwrap();
            if let Err(panic) = script.join().unwrap() {
                std::panic::resume_unwind(panic);
            }
            result
        });
        assert!(matches!(result, Ok(End::Stop)));
        assert!(link.take_end().is_none());
        let calls = calls.into_inner().unwrap();
        assert!(calls.len() >= 3 && calls.iter().all(|busy| !busy), "{calls:?}");
    }

    /// The response to a setCidReporting of MIDDLE with `flags` (0x22: clears
    /// the diversion, 0x33: diverts with raw XY, as the daemon does).
    fn set_response(sw_id: u8, flags: u8) -> Vec<u8> {
        use crate::hidpp::message::Message;
        let mut params = MIDDLE.to_be_bytes().to_vec();
        params.extend([flags, 0x00, 0x00]);
        Message::request(0xFF, CONTROLS_IDX, 3, sw_id, &params).unwrap().to_bytes()
    }

    /// The response to a setCidReporting that clears the diversion of MIDDLE.
    fn foreign_reset(sw_id: u8) -> Vec<u8> {
        set_response(sw_id, 0x22)
    }

    /// Another program takes the diversion of the gesture button: it is
    /// written again at once, after the gesture when one is open, at most
    /// three times a minute, also when the other program uses an ID from our
    /// range; our own setting is no reason to write again.
    #[test]
    fn a_diversion_taken_by_another_program_is_restored_at_once_but_not_in_a_gesture() {
        use crate::hidpp::message::Message;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;
        use std::thread;

        const GAP: Duration = Duration::from_millis(30);
        const QUIET: Duration = Duration::from_millis(150);
        const FOREIGN_ID: u8 = 0x02;
        // The ID of one of our requests (a program may use any ID).
        const OWN_ID: u8 = 0x01;

        let set = set_for("[buttons]\nmiddle = \"gestures\"\n[gestures.middle]\nthreshold = 50\n");
        let (to_mouse, requests) = mpsc::channel::<Message>();
        let (dev, feed) = Device::fake(move |raw| to_mouse.send(Message::parse(raw).unwrap()).unwrap());
        let link = checks::Link::default();
        let mut caller = |_: &Device| ControlFlow::Continue(Next { deadline: None, contacted: false });
        let done = AtomicBool::new(false);
        let mut tick = || {
            if done.load(Ordering::SeqCst) { ControlFlow::Break(End::Stop) } else { ControlFlow::Continue(()) }
        };

        let script = {
            let (link, feed) = (&link, &feed);
            move || {
                let wait = |what: &str| {
                    let req = requests.recv_timeout(Duration::from_secs(5)).expect(what);
                    let reply = Message::request(req.device_index, req.feature_index, req.function, req.sw_id, &req.params);
                    feed.push(&reply.unwrap().to_bytes());
                    req
                };
                let silence = |what: &str| match requests.recv_timeout(QUIET) {
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    other => panic!("{what}: {other:?}"),
                };
                let divert_written = |req: &Message| {
                    assert_eq!((req.feature_index, req.function), (CONTROLS_IDX, 3));
                    assert_eq!(req.params[..5], controls::encode_reporting(MIDDLE, true, true));
                };
                let hold = |held: bool| {
                    feed.push(&button_report(if held { &[MIDDLE] } else { &[] }));
                    let start = Instant::now();
                    while link.is_busy() != held {
                        assert!(start.elapsed() < Duration::from_secs(5), "the loop did not follow the mouse");
                        thread::sleep(Duration::from_millis(2));
                    }
                    thread::sleep(GAP);
                };
                // Our own setting, e.g. a late response: nothing.
                feed.push(&set_response(OWN_ID, 0x33));
                silence("a write for our own setting");
                // A response that clears the diversion: one write, whatever
                // the ID.
                feed.push(&foreign_reset(OWN_ID));
                divert_written(&wait("restore (1)"));
                silence("more than one write for one response");
                // During a gesture: only after the button is released.
                hold(true);
                feed.push(&foreign_reset(FOREIGN_ID));
                silence("a write during a gesture");
                hold(false);
                divert_written(&wait("restore (2)"));
                silence("more than one write after the gesture");
                // The third time in a minute is still restored, the fourth is not.
                feed.push(&foreign_reset(FOREIGN_ID));
                divert_written(&wait("restore (3)"));
                feed.push(&foreign_reset(FOREIGN_ID));
                silence("a fourth write within a minute");
            }
        };

        let result = thread::scope(|s| {
            let checker = Checker::new(CONTROLS_IDX, None, vec![(MIDDLE, true)], &mut caller, Instant::now());
            let check_thread = s.spawn(|| checker.run(&dev, &link));
            let script = s.spawn(|| {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(script));
                done.store(true, Ordering::SeqCst);
                dev.interrupter().interrupt();
                outcome
            });
            let mouse = Mouse { dev: &dev, idx: CONTROLS_IDX, set: &set, wireless_idx: None, link: &link };
            let result = mouse.pump(GestureState::new(vec![MIDDLE]), &mut None, &mut tick);
            link.quit();
            check_thread.join().unwrap();
            if let Err(panic) = script.join().unwrap() {
                std::panic::resume_unwind(panic);
            }
            result
        });
        assert!(matches!(result, Ok(End::Stop)));
        assert!(link.take_end().is_none());
    }
}
