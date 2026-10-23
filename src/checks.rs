//! The periodic checks that talk to the mouse: the caller's (ping, battery,
//! DPI), the thumb wheel diversion and the button diversion. They run on
//! their own thread, so the mouse thread never waits for the mouse: a write
//! that wakes the Bluetooth radio of an idle mouse blocks ~0.5 s, and button
//! notifications that arrive meanwhile are handled at once. The checks still
//! never start while a button is held or a gesture is open; they wait for
//! its end.

use std::ops::ControlFlow;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use rust_i18n::t;

use crate::actions;
use crate::events::{End, Tick};
use crate::hidpp::device::{Device, Timeout, is_link_failure};
use crate::hidpp::features::controls::{self, ForeignSet, Reporting};
use crate::thumb::ThumbCheck;
use crate::util::{Throttle, lock};

/// The caller's checks, run on the check thread (see [`crate::events::session`]).
pub type Checks<'a> = dyn FnMut(&Device) -> Tick + Send + 'a;

/// State the mouse thread shares with the check thread.
#[derive(Default)]
pub(crate) struct Link {
    state: Mutex<LinkState>,
    changed: Condvar,
}

#[derive(Default)]
struct LinkState {
    /// A button is held or a gesture (also a thumb wheel scroll) is open.
    busy: bool,
    /// Counts the ends of such gestures, so the check thread cannot miss one.
    released: u64,
    /// When the last button or movement notification arrived.
    last_notification: Option<Instant>,
    /// The session is over: the check thread quits.
    quit: bool,
    /// The checks ended the session (a lost mouse, settings to re-apply).
    end: Option<Result<End>>,
    /// Button presses that asked to toggle the wheel mode, not yet done.
    wheel_toggles: u32,
    /// Settings of the button diversion made by other programs, not yet
    /// looked at by the check thread.
    foreign: Vec<ForeignSet>,
}

/// What the check thread saw when it woke up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Seen {
    busy: bool,
    released: u64,
}

impl Link {
    /// Mouse thread: whether a button is held or a gesture is open. The end
    /// of one wakes the check thread, which may have been waiting for it.
    pub(crate) fn set_busy(&self, busy: bool) {
        let mut s = lock(&self.state);
        if s.busy == busy {
            return;
        }
        s.busy = busy;
        if !busy {
            s.released += 1;
            drop(s);
            self.changed.notify_all();
        }
    }

    /// Mouse thread: a button or movement notification arrived.
    pub(crate) fn note_notification(&self, at: Instant) {
        lock(&self.state).last_notification = Some(at);
    }

    /// Mouse thread: a button asked to toggle the wheel mode. The check
    /// thread talks to the mouse for it, so the mouse thread does not wait.
    pub(crate) fn request_wheel_toggle(&self) {
        let mut s = lock(&self.state);
        s.wheel_toggles = s.wheel_toggles.saturating_add(1);
        drop(s);
        self.changed.notify_all();
    }

    /// Mouse thread: another program set the diversion of a button (the
    /// mouse's response to it reached us). The check thread restores ours
    /// if that removed it, as the mouse thread must not talk to the mouse.
    pub(crate) fn report_foreign(&self, set: ForeignSet) {
        lock(&self.state).foreign.push(set);
        self.changed.notify_all();
    }

    /// Check thread: the settings by other programs reported since the last call.
    fn take_foreign(&self) -> Vec<ForeignSet> {
        std::mem::take(&mut lock(&self.state).foreign)
    }

    /// Mouse thread: how the checks ended the session, if they did.
    pub(crate) fn take_end(&self) -> Option<Result<End>> {
        lock(&self.state).end.take()
    }

    /// Mouse thread: the session is over; the check thread quits after the
    /// request it is waiting for, if any.
    pub(crate) fn quit(&self) {
        lock(&self.state).quit = true;
        self.changed.notify_all();
    }

    fn busy(&self) -> bool {
        lock(&self.state).busy
    }

    fn quitting(&self) -> bool {
        lock(&self.state).quit
    }

    /// Check thread: the wheel mode toggles asked for since the last call.
    fn take_wheel_toggles(&self) -> u32 {
        std::mem::take(&mut lock(&self.state).wheel_toggles)
    }

    fn last_notification(&self) -> Option<Instant> {
        lock(&self.state).last_notification
    }

    fn finish(&self, end: Result<End>) {
        lock(&self.state).end = Some(end);
    }

    /// Check thread: sleeps until `deadline` (`None`: until woken), the end
    /// of a gesture after `seen`, a wheel mode request, a setting by another
    /// program or a quit. `None` means quit.
    fn wait(&self, deadline: Option<Instant>, seen: Option<Seen>) -> Option<Seen> {
        let mut s = lock(&self.state);
        let ended_since = |s: &LinkState| seen.is_some_and(|v| v.released != s.released);
        if !s.quit && !ended_since(&s) && s.wheel_toggles == 0 && s.foreign.is_empty() {
            s = match deadline {
                None => self.changed.wait(s).unwrap_or_else(PoisonError::into_inner),
                Some(d) => {
                    let left = d.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        s
                    } else {
                        self.changed.wait_timeout(s, left).unwrap_or_else(PoisonError::into_inner).0
                    }
                }
            };
        }
        (!s.quit).then_some(Seen { busy: s.busy, released: s.released })
    }
}

/// The check thread of one mouse session.
pub(crate) struct Checker<'a> {
    /// Feature index of the button controls (0x1B04).
    controls: u8,
    thumb: Option<ThumbCheck>,
    watch: ControlsWatch,
    series: Series,
    checks: &'a mut Checks<'a>,
}

impl<'a> Checker<'a> {
    /// `targets`: diverted buttons and whether each is a gesture button.
    pub(crate) fn new(
        controls: u8,
        thumb: Option<ThumbCheck>,
        targets: Vec<(u16, bool)>,
        checks: &'a mut Checks<'a>,
        now: Instant,
    ) -> Self {
        Checker {
            controls,
            thumb,
            watch: ControlsWatch::new(targets, now),
            series: Series::new(now),
            checks,
        }
    }

    /// Body of the check thread: runs the checks until the session is over
    /// (`link.quit`) or a check ends it, which it reports through `link`
    /// and by waking the mouse thread.
    pub(crate) fn run(mut self, dev: &Device, link: &Link) {
        let end = match catch_unwind(AssertUnwindSafe(|| self.serve(dev, link))) {
            Ok(None) => return,
            Ok(Some(end)) => end,
            Err(_) => Err(anyhow!("the check thread panicked")),
        };
        link.finish(end);
        dev.interrupter().interrupt();
    }

    /// `None` when told to quit, else how the checks ended the session.
    fn serve(&mut self, dev: &Device, link: &Link) -> Option<Result<End>> {
        // The first pass at once: the caller reads the status right after
        // the settings were applied (the radio is awake).
        let mut deadline = Some(Instant::now());
        let mut seen = None;
        loop {
            let now = link.wait(deadline, seen)?;
            let ended = seen.is_none_or(|v| v.released != now.released);
            seen = Some(now);
            // A button asked for it: done at once, also while a button is held.
            for _ in 0..link.take_wheel_toggles() {
                if let Err(e) = toggle_wheel_mode(dev) {
                    return Some(Err(e));
                }
            }
            // Another program took a button's diversion: back at once, or
            // right after the gesture (the end of one wakes this loop).
            for set in link.take_foreign() {
                self.watch.note_foreign(set);
            }
            let idx = self.controls;
            if let Err(e) = self.watch.restore_foreign(
                Instant::now(),
                &mut || link.busy(),
                &mut |cid, raw_xy| controls::set_reporting(dev, idx, cid, true, raw_xy),
            ) {
                return Some(Err(e));
            }
            // Only a request woke the thread: the checks keep their schedule.
            let due = deadline.is_some_and(|d| Instant::now() >= d);
            if !ended && !due {
                continue;
            }
            match self.pass(dev, link, now.busy) {
                Ok(ControlFlow::Continue(next)) => deadline = next,
                Ok(ControlFlow::Break(end)) => return Some(Ok(end)),
                Err(e) => return Some(Err(e)),
            }
        }
    }

    /// Runs what is due; returns when to look again (`None`: at the end of
    /// the gesture, which wakes the thread).
    fn pass(
        &mut self,
        dev: &Device,
        link: &Link,
        busy: bool,
    ) -> Result<ControlFlow<End, Option<Instant>>> {
        if busy {
            // Marks the series deferred if it is due; wakes for it if not yet.
            let now = Instant::now();
            let _ = self.series.start(now, true);
            return Ok(ControlFlow::Continue(self.series.deadline(now, true)));
        }
        // The caller's ping comes first and wakes the radio; what follows in
        // the same pass (battery, DPI, then the series) is fast.
        let next = match (self.checks)(dev) {
            ControlFlow::Break(end) => return Ok(ControlFlow::Break(end)),
            ControlFlow::Continue(next) => next,
        };
        if link.quitting() {
            return Ok(ControlFlow::Continue(None));
        }
        let now = Instant::now();
        if next.contacted {
            self.series.pull_forward(now);
        }
        // A gesture may have started meanwhile: the rest waits for its end.
        let busy = link.busy();
        let started = self.series.start(now, busy);
        let run_series = started != Start::No;
        if let Start::Deferred(late) = started {
            log::info!("checks deferred: waited_ms={}", late.as_millis());
        }
        if run_series && let Some(t) = self.thumb.as_mut() {
            t.check(dev, now)?;
        }
        if let Some(t) = link.last_notification() {
            self.watch.note_notification(t);
        }
        let idx = self.controls;
        self.watch.check(
            Instant::now(),
            &mut || link.busy(),
            run_series,
            dev.last_rx(),
            &mut |cid| controls::get_reporting(dev, idx, cid),
            &mut |cid, raw_xy| controls::set_reporting(dev, idx, cid, true, raw_xy),
        )?;
        let now = Instant::now();
        // An overdue button check waits for a working mouse: look again soon.
        let watch = self.watch.deadline(now).unwrap_or(now + WATCH_ACTIVE_WITHIN);
        let mut deadline = earliest(next.deadline, watch);
        if let Some(t) = self.series.deadline(now, link.busy()) {
            deadline = earliest(deadline, t);
        }
        Ok(ControlFlow::Continue(deadline))
    }
}

/// Toggles the wheel mode for a button. A failure is only logged, except a
/// lost link, which ends the session like any other check would.
fn toggle_wheel_mode(dev: &Device) -> Result<()> {
    let start = Instant::now();
    let result = actions::toggle_wheel_mode(dev);
    log::info!(
        "action: ShiftWheelMode took_ms={} ok={}",
        start.elapsed().as_millis(),
        result.is_ok()
    );
    match result {
        Err(e) if is_link_failure(&e) && e.downcast_ref::<Timeout>().is_none() => Err(e),
        Err(e) => {
            log::warn!("{}", t!("events.action_failed", error = e));
            Ok(())
        }
        Ok(()) => Ok(()),
    }
}

/// How often the mouse is asked whether the buttons are still diverted.
const WATCH_EVERY: Duration = Duration::from_secs(10);
/// The check runs only if the mouse sent something this recently: an idle
/// mouse needs ~0.5 s to answer the first request (the radio sleeps), and
/// the check right after the caller's ping (a report, too) is cheap.
const WATCH_ACTIVE_WITHIN: Duration = Duration::from_secs(2);
/// Minimal time between two warnings about lost diversion.
const WATCH_WARN_EVERY: Duration = Duration::from_secs(60);
/// Another program (Options+) can keep taking the diversion back; a button
/// is restored at once after that at most this many times per window, the
/// periodic check does the rest.
const FOREIGN_RESTORES_MAX: usize = 3;
const FOREIGN_RESTORES_WINDOW: Duration = Duration::from_secs(60);

/// Checks now and then that the mouse still diverts the buttons (it can lose
/// the setting without telling us) and diverts them again if not. Also
/// restores a button at once when the response to another program's
/// setting shows that it took the diversion away.
struct ControlsWatch {
    /// Diverted buttons and whether each is a gesture button (raw XY).
    targets: Vec<(u16, bool)>,
    next_check: Instant,
    /// Limits the warning about lost diversion.
    warn: Throttle,
    /// Limits the warning about a skipped restore (the restore limit), apart
    /// from `warn` so that neither hides the other.
    limit_warn: Throttle,
    /// When the last button or movement notification arrived.
    last_notification: Option<Instant>,
    /// Buttons whose diversion another program removed, not yet restored.
    foreign: Vec<u16>,
    /// When buttons were restored after that, for the limit per window.
    restored: Vec<(u16, Instant)>,
}

impl ControlsWatch {
    fn new(targets: Vec<(u16, bool)>, now: Instant) -> Self {
        ControlsWatch {
            targets,
            next_check: now + WATCH_EVERY,
            warn: Throttle::new(WATCH_WARN_EVERY),
            limit_warn: Throttle::new(WATCH_WARN_EVERY),
            last_notification: None,
            foreign: Vec::new(),
            restored: Vec::new(),
        }
    }

    /// Another program set the diversion of a button: remember it if that
    /// removed the diversion of one of ours.
    fn note_foreign(&mut self, set: ForeignSet) {
        let Some(&(_, raw_xy)) = self.targets.iter().find(|(cid, _)| *cid == set.cid) else {
            return;
        };
        if set.removes(raw_xy) && !self.foreign.contains(&set.cid) {
            self.foreign.push(set.cid);
        }
    }

    /// Diverts again through `divert(cid, raw_xy)` the buttons another
    /// program took the diversion from. `busy` is asked right before every
    /// write: during a gesture nothing is written and the buttons stay
    /// pending until the next call, after the gesture. A button restored
    /// [`FOREIGN_RESTORES_MAX`] times within [`FOREIGN_RESTORES_WINDOW`] is
    /// left to the periodic check. A link failure is returned.
    fn restore_foreign(
        &mut self,
        now: Instant,
        busy: &mut dyn FnMut() -> bool,
        divert: &mut dyn FnMut(u16, bool) -> Result<()>,
    ) -> Result<()> {
        self.restored.retain(|&(_, t)| now.saturating_duration_since(t) < FOREIGN_RESTORES_WINDOW);
        while let Some(&cid) = self.foreign.first() {
            if busy() {
                log::debug!("divert restore deferred: reason=foreign cid=0x{cid:04X} a gesture is open");
                return Ok(());
            }
            self.foreign.remove(0);
            if self.restored.iter().filter(|(c, _)| *c == cid).count() >= FOREIGN_RESTORES_MAX {
                if self.limit_warn.allow(now) {
                    log::warn!(
                        "divert restore skipped: reason=foreign cid=0x{cid:04X} limit={FOREIGN_RESTORES_MAX}/min"
                    );
                }
                continue;
            }
            let raw_xy = self.targets.iter().any(|&(c, raw)| c == cid && raw);
            self.restored.push((cid, now));
            match divert(cid, raw_xy) {
                Ok(()) => log::info!("divert restored: reason=foreign cid=0x{cid:04X}"),
                Err(e) if is_link_failure(&e) && e.downcast_ref::<Timeout>().is_none() => {
                    return Err(e);
                }
                Err(e) => log::warn!("cannot divert button 0x{cid:04X} again: {e}"),
            }
        }
        Ok(())
    }

    fn note_notification(&mut self, now: Instant) {
        self.last_notification = Some(now);
    }

    /// When the check is next due, if that is still ahead.
    fn deadline(&self, now: Instant) -> Option<Instant> {
        (self.next_check > now).then_some(self.next_check)
    }

    /// True if the check should run now (`busy` is asked once). Never while a button is held, a
    /// gesture or a scroll gesture is open (`busy`): the mouse is busy
    /// sending the gesture, and a lost diversion is restored after it
    /// (the gesture works then anyway). Otherwise it
    /// runs as part of the series of checks (`series`) or, between series,
    /// when it is due and the mouse is working.
    fn is_due(
        &self,
        now: Instant,
        busy: &mut dyn FnMut() -> bool,
        series: bool,
        last_rx: Option<Instant>,
    ) -> bool {
        !busy()
            && (series
                || now >= self.next_check
                    && last_rx
                        .is_some_and(|t| now.saturating_duration_since(t) <= WATCH_ACTIVE_WITHIN))
    }

    /// Reads the state of every button through `read` and diverts again
    /// through `divert(cid, raw_xy)` those that lost it. A link failure is
    /// returned; a timeout only ends this pass. `busy` tells whether a
    /// button is held or a gesture is open; it is asked again right before
    /// every `divert`, because a gesture may start after the buttons were
    /// read. Then nothing more is written and the check stays due, so it
    /// runs again after the gesture.
    fn check(
        &mut self,
        now: Instant,
        busy: &mut dyn FnMut() -> bool,
        series: bool,
        last_rx: Option<Instant>,
        read: &mut dyn FnMut(u16) -> Result<Reporting>,
        divert: &mut dyn FnMut(u16, bool) -> Result<()>,
    ) -> Result<()> {
        if !self.is_due(now, busy, series, last_rx) {
            return Ok(());
        }
        self.next_check = now + WATCH_EVERY;
        let mut lost = Vec::new();
        for &(cid, raw_xy) in &self.targets {
            match read(cid) {
                Ok(r) if r.is_lost(raw_xy) => lost.push((cid, raw_xy, r)),
                Ok(_) => {}
                Err(e) if e.downcast_ref::<Timeout>().is_some() => {
                    log::debug!("cannot read button 0x{cid:04X} reporting: {e}");
                    break;
                }
                Err(e) if is_link_failure(&e) => return Err(e),
                Err(e) => log::debug!("cannot read button 0x{cid:04X} reporting: {e}"),
            }
        }
        if lost.is_empty() {
            return Ok(());
        }
        let warn = self.warn.allow(now);
        if warn {
            let items: Vec<String> = lost
                .iter()
                .map(|(cid, _, r)| {
                    format!("0x{cid:04X}(divert={} raw_xy={})", r.diverted, r.raw_xy)
                })
                .collect();
            let since = self.last_notification.map_or("none".to_string(), |t| {
                now.saturating_duration_since(t).as_secs().to_string()
            });
            log::warn!(
                "button diversion lost: buttons={} since_last_button_notification_s={since}; diverting again",
                items.join(",")
            );
        }
        for (cid, raw_xy, _) in lost {
            if busy() {
                log::debug!("button diversion restore deferred: a gesture started");
                self.next_check = now;
                return Ok(());
            }
            match divert(cid, raw_xy) {
                Err(e) if is_link_failure(&e) && e.downcast_ref::<Timeout>().is_none() => {
                    return Err(e);
                }
                Err(e) if warn => log::warn!("cannot divert button 0x{cid:04X} again: {e}"),
                Ok(()) => self.foreign.retain(|&c| c != cid),
                Err(_) => {}
            }
        }
        Ok(())
    }
}

/// How often the series of checks runs: ping (the caller), thumb wheel
/// diversion, button diversion.
const SERIES_EVERY: Duration = Duration::from_secs(60);

/// When the checks that talk to the mouse run. They run together, one after
/// another: the first one pays for waking the mouse's radio (~0.5 s), the
/// rest are fast, so the radio is woken once a minute, not once per check.
/// They never start while a button is held or a gesture is open; they wait
/// for its end.
struct Series {
    next: Instant,
    /// It was due while a button was held or a gesture was open.
    deferred: bool,
}

/// Result of [`Series::start`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Start {
    /// Not now.
    No,
    /// Run now, on time.
    OnTime,
    /// Run now, after waiting this long past its time for a gesture to end.
    Deferred(Duration),
}

impl Series {
    fn new(now: Instant) -> Self {
        Series { next: now + SERIES_EVERY, deferred: false }
    }

    /// The caller's ping went through: the series follows it now.
    fn pull_forward(&mut self, now: Instant) {
        self.next = self.next.min(now);
    }

    /// Whether the series should run now; if so, schedules the next one.
    fn start(&mut self, now: Instant, busy: bool) -> Start {
        if now < self.next {
            return Start::No;
        }
        if busy {
            self.deferred = true;
            return Start::No;
        }
        let late = now.saturating_duration_since(self.next);
        self.next = now + SERIES_EVERY;
        if std::mem::take(&mut self.deferred) { Start::Deferred(late) } else { Start::OnTime }
    }

    /// When the check thread must wake for the series. While busy only if
    /// it is not due yet (to note the deferral): the end of the gesture
    /// wakes the thread, and a past deadline would make it spin.
    fn deadline(&self, now: Instant, busy: bool) -> Option<Instant> {
        (!busy || self.next > now).then_some(self.next)
    }
}

/// The earlier of an optional and a certain time.
fn earliest(a: Option<Instant>, b: Instant) -> Option<Instant> {
    Some(a.map_or(b, |a| a.min(b)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Next;
    use crate::hidpp::features::controls::Event;
    use crate::hidpp::message::Message;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;

    const IDX: u8 = 9;
    const THUMB: u16 = 0x00C3;

    impl Link {
        /// For the tests of the event loop: whether a button is held or a
        /// gesture is open, as the check thread sees it.
        pub(crate) fn is_busy(&self) -> bool {
            self.busy()
        }
    }

    #[test]
    fn the_check_thread_wakes_at_the_end_of_a_gesture_and_on_quit() {
        let link = Link::default();
        link.set_busy(true);
        let seen = link.wait(Some(Instant::now()), None).unwrap();
        assert!(seen.busy);
        thread::scope(|s| {
            let waiting = s.spawn(|| link.wait(None, Some(seen)));
            thread::sleep(Duration::from_millis(20));
            assert!(!waiting.is_finished());
            link.set_busy(false);
            assert_eq!(waiting.join().unwrap().map(|v| v.busy), Some(false));
        });
        // A gesture that ended before the thread went to sleep is not missed.
        link.set_busy(true);
        let seen = link.wait(Some(Instant::now()), None).unwrap();
        link.set_busy(false);
        let start = Instant::now();
        assert!(link.wait(None, Some(seen)).is_some());
        assert!(start.elapsed() < Duration::from_secs(1));
        // Quit wakes it, too.
        let seen = link.wait(Some(Instant::now()), None).unwrap();
        thread::scope(|s| {
            let waiting = s.spawn(|| link.wait(None, Some(seen)));
            link.quit();
            assert_eq!(waiting.join().unwrap(), None);
        });
    }

    /// Polls `f` until it is true (at most 5 s).
    fn eventually(f: impl Fn() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if f() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn the_checks_wait_for_the_end_of_a_gesture() {
        let (dev, _feed) = Device::fake(|_| panic!("nothing is written"));
        let link = Link::default();
        let calls = AtomicUsize::new(0);
        let mut checks = |_: &Device| {
            calls.fetch_add(1, Ordering::SeqCst);
            ControlFlow::Continue(Next::default())
        };
        // A button is held when the session starts.
        link.set_busy(true);
        let checker = Checker::new(IDX, None, Vec::new(), &mut checks, Instant::now());
        thread::scope(|s| {
            let t = s.spawn(|| checker.run(&dev, &link));
            thread::sleep(Duration::from_millis(50));
            assert_eq!(calls.load(Ordering::SeqCst), 0, "nothing while the button is held");
            link.set_busy(false);
            assert!(eventually(|| calls.load(Ordering::SeqCst) == 1), "runs at the release");
            link.quit();
            t.join().unwrap();
        });
        assert!(link.take_end().is_none());
    }

    #[test]
    fn a_check_that_ends_the_session_wakes_the_mouse_thread() {
        let (dev, _feed) = Device::fake(|_| panic!("nothing is written"));
        let link = Link::default();
        let mut checks = |_: &Device| ControlFlow::Break(End::Lost);
        let checker = Checker::new(IDX, None, Vec::new(), &mut checks, Instant::now());
        thread::scope(|s| {
            s.spawn(|| checker.run(&dev, &link));
            // The mouse thread waits for notifications and is woken.
            let start = Instant::now();
            assert_eq!(dev.read_message(5_000).unwrap(), None);
            assert!(start.elapsed() < Duration::from_secs(2));
        });
        assert!(matches!(link.take_end(), Some(Ok(End::Lost))));
    }

    /// A button notification of the controls feature.
    fn buttons(cids: &[u16]) -> Vec<u8> {
        let params: Vec<u8> = cids.iter().flat_map(|c| c.to_be_bytes()).collect();
        Message::request(0xFF, IDX, 0, 0, &params).unwrap().to_bytes()
    }

    #[test]
    fn buttons_are_handled_at_once_while_the_checks_restore_the_diversion() {
        let (to_mouse, requests) = mpsc::channel::<Message>();
        let (dev, feed) = Device::fake(move |raw| {
            // A write that wakes the radio of an idle mouse blocks a while.
            thread::sleep(Duration::from_millis(100));
            to_mouse.send(Message::parse(raw).unwrap()).unwrap();
        });
        let link = Link::default();
        // The caller's ping, as the daemon's health check does it.
        let mut checks = |d: &Device| {
            let contacted = d.ping().is_ok();
            ControlFlow::Continue(Next { deadline: None, contacted })
        };
        let checker = Checker::new(IDX, None, vec![(THUMB, true)], &mut checks, Instant::now());
        let (seen_tx, seen) = mpsc::channel::<Message>();
        thread::scope(|s| {
            s.spawn(|| checker.run(&dev, &link));
            // The mouse answers every request by echoing it: the ping, then
            // "button not diverted" (flags 0) and the new diversion.
            let mouse_feed = feed.clone();
            s.spawn(move || {
                while let Ok(req) = requests.recv_timeout(Duration::from_secs(5)) {
                    let (i, f) = (req.feature_index, req.function);
                    let reply = Message::request(req.device_index, i, f, req.sw_id, &req.params);
                    mouse_feed.push(&reply.unwrap().to_bytes());
                    let done = req.function == 3;
                    seen_tx.send(req).unwrap();
                    if done {
                        break;
                    }
                }
            });
            // The mouse thread: every button arrives at once while the
            // checks wait for the slow mouse.
            for i in 0..4 {
                let held = if i % 2 == 0 { vec![THUMB] } else { vec![] };
                feed.push(&buttons(&held));
                let start = Instant::now();
                let msg = dev.read_message(1_000).unwrap().unwrap();
                assert!(start.elapsed() < Duration::from_millis(50));
                assert_eq!(controls::parse_event(IDX, &msg), Some(Event::Buttons(held)));
                thread::sleep(Duration::from_millis(60));
            }
            let all: Vec<Message> = seen.iter().collect();
            let keys: Vec<(u8, u8)> = all.iter().map(|m| (m.feature_index, m.function)).collect();
            assert_eq!(keys, [(0, 1), (IDX, 2), (IDX, 3)], "ping, read, divert again");
            assert_eq!(all[2].params[..5], controls::encode_reporting(THUMB, true, true));
            link.quit();
        });
        // The responses went to the checks, never to the mouse thread.
        assert_eq!(dev.read_message(0).unwrap(), None);
        assert!(link.take_end().is_none());
    }

    /// A mouse with legacy SmartShift (feature index 5) in ratchet mode: it
    /// answers every request and passes it on through the receiver. A write
    /// to it is slow, as for an idle mouse.
    fn smartshift_mouse(supported: bool) -> (Device, super::super::hidpp::transport::Feed, mpsc::Receiver<Message>) {
        let (to_mouse, requests) = mpsc::channel::<Message>();
        let (dev, feed) = Device::fake(move |raw| {
            thread::sleep(Duration::from_millis(100));
            to_mouse.send(Message::parse(raw).unwrap()).unwrap();
        });
        let (answers, seen) = mpsc::channel::<Message>();
        let mouse_feed = feed.clone();
        thread::spawn(move || {
            let mut mode = 2u8;
            while let Ok(req) = requests.recv() {
                let reply: &[u8] = match (req.feature_index, req.function) {
                    (0, 0) if supported && req.params[..2] == [0x21, 0x10] => &[5, 0, 0],
                    (0, 0) => &[0, 0, 0],
                    (5, 1) => {
                        mode = req.params[0];
                        &req.params
                    }
                    _ => &[mode, 10, 10],
                };
                let m = Message::request(req.device_index, req.feature_index, req.function, req.sw_id, reply);
                mouse_feed.push(&m.unwrap().to_bytes());
                if answers.send(req).is_err() {
                    break;
                }
            }
        });
        (dev, feed, seen)
    }

    #[test]
    fn a_wheel_mode_change_does_not_block_the_buttons() {
        let (dev, feed, seen) = smartshift_mouse(true);
        let link = Link::default();
        let calls = AtomicUsize::new(0);
        let mut checks = |_: &Device| {
            calls.fetch_add(1, Ordering::SeqCst);
            ControlFlow::Continue(Next::default())
        };
        let checker = Checker::new(IDX, None, vec![(THUMB, true)], &mut checks, Instant::now());
        thread::scope(|s| {
            let t = s.spawn(|| checker.run(&dev, &link));
            assert!(eventually(|| calls.load(Ordering::SeqCst) == 1), "first pass");
            // A button is held, as when the toggling button is pressed.
            link.set_busy(true);
            link.request_wheel_toggle();
            // The write takes 100 ms; the next presses are handled at once.
            for i in 0..3 {
                let held = if i % 2 == 0 { vec![THUMB] } else { vec![] };
                feed.push(&buttons(&held));
                let start = Instant::now();
                let msg = dev.read_message(1_000).unwrap().unwrap();
                assert!(start.elapsed() < Duration::from_millis(50));
                assert_eq!(controls::parse_event(IDX, &msg), Some(Event::Buttons(held)));
                thread::sleep(Duration::from_millis(30));
            }
            // The mouse got the lookup, the read, the write and the read back.
            let mut got = Vec::new();
            while let Ok(m) = seen.recv_timeout(Duration::from_secs(5)) {
                got.push((m.feature_index, m.function));
                if got.len() == 5 {
                    break;
                }
            }
            assert_eq!(got, [(0, 0), (0, 0), (5, 0), (5, 1), (5, 0)]);
            link.quit();
            t.join().unwrap();
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the request is not a reason for the checks");
        assert!(link.take_end().is_none());
    }

    #[test]
    fn a_wheel_mode_change_that_fails_is_only_logged() {
        // No SmartShift: the toggle fails with a message, the session goes on.
        let (dev, _feed, _seen) = smartshift_mouse(false);
        assert!(toggle_wheel_mode(&dev).is_ok());
    }

    fn watch(now: Instant) -> ControlsWatch {
        ControlsWatch::new(vec![(0x00C3, true), (0x0053, false)], now)
    }

    const ON: Reporting = Reporting { diverted: true, raw_xy: true };
    const PLAIN: Reporting = Reporting { diverted: true, raw_xy: false };
    const OFF: Reporting = Reporting { diverted: false, raw_xy: false };

    /// Runs one check; returns the cids read and the (cid, raw_xy) re-diverted.
    fn run_check(
        w: &mut ControlsWatch,
        now: Instant,
        busy: bool,
        rx: Option<Instant>,
        state: impl Fn(u16) -> Reporting,
    ) -> (Vec<u16>, Vec<(u16, bool)>) {
        let mut reads = Vec::new();
        let mut fixes = Vec::new();
        w.check(
            now,
            &mut || busy,
            false,
            rx,
            &mut |cid| {
                reads.push(cid);
                Ok(state(cid))
            },
            &mut |cid, raw| {
                fixes.push((cid, raw));
                Ok(())
            },
        )
        .unwrap();
        (reads, fixes)
    }

    #[test]
    fn watch_runs_every_ten_seconds_and_leaves_good_buttons_alone() {
        let t0 = Instant::now();
        let mut w = watch(t0);
        let at = |s| t0 + Duration::from_secs(s);
        let good = |cid| if cid == 0x00C3 { ON } else { PLAIN };
        assert!(run_check(&mut w, at(9), false, Some(at(9)), good).0.is_empty());
        let (reads, fixes) = run_check(&mut w, at(10), false, Some(at(10)), good);
        assert_eq!(reads, [0x00C3, 0x0053]);
        assert!(fixes.is_empty());
        // Next one only 10 s later.
        assert!(run_check(&mut w, at(19), false, Some(at(19)), good).0.is_empty());
        assert_eq!(run_check(&mut w, at(20), false, Some(at(20)), good).0.len(), 2);
        assert_eq!(w.deadline(at(20)), Some(at(30)));
        assert_eq!(w.deadline(at(30)), None);
    }

    #[test]
    fn watch_never_reads_while_a_button_is_held() {
        let t0 = Instant::now();
        let mut w = watch(t0);
        let due = t0 + Duration::from_secs(30);
        let (reads, fixes) = run_check(&mut w, due, true, Some(due), |_| OFF);
        assert!(reads.is_empty() && fixes.is_empty());
        // Still due once the button is released.
        assert_eq!(run_check(&mut w, due, false, Some(due), |_| OFF).0.len(), 2);
    }

    #[test]
    fn watch_waits_for_a_mouse_that_is_working() {
        let t0 = Instant::now();
        let mut w = watch(t0);
        let due = t0 + Duration::from_secs(30);
        // Nothing heard for 5 s, or ever: idle mouse, do not wake it.
        assert!(run_check(&mut w, due, false, Some(due - Duration::from_secs(5)), |_| OFF).0.is_empty());
        assert!(run_check(&mut w, due, false, None, |_| OFF).0.is_empty());
        assert_eq!(run_check(&mut w, due, false, Some(due - Duration::from_secs(2)), |_| ON).0.len(), 2);
    }

    #[test]
    fn watch_diverts_lost_buttons_again() {
        let t0 = Instant::now();
        let mut w = watch(t0);
        let due = t0 + Duration::from_secs(10);
        // Gesture button lost raw XY, plain button not diverted at all.
        let (_, fixes) = run_check(&mut w, due, false, Some(due), |cid| {
            if cid == 0x00C3 { PLAIN } else { OFF }
        });
        assert_eq!(fixes, [(0x00C3, true), (0x0053, false)]);
        // A plain button with raw XY off is fine; the gesture button is not.
        let due = due + Duration::from_secs(10);
        let (_, fixes) = run_check(&mut w, due, false, Some(due), |cid| {
            if cid == 0x00C3 { OFF } else { PLAIN }
        });
        assert_eq!(fixes, [(0x00C3, true)]);
    }

    #[test]
    fn watch_does_not_divert_during_a_gesture_that_starts_after_the_read() {
        let t0 = Instant::now();
        let mut w = watch(t0);
        let due = t0 + Duration::from_secs(10);
        // Not busy when the pass starts, busy at the first write.
        let asked = std::cell::Cell::new(0);
        let mut fixes = Vec::new();
        w.check(
            due,
            &mut || {
                asked.set(asked.get() + 1);
                asked.get() > 1
            },
            false,
            Some(due),
            &mut |_| Ok(OFF),
            &mut |cid, raw| {
                fixes.push((cid, raw));
                Ok(())
            },
        )
        .unwrap();
        assert!(fixes.is_empty(), "no write during the gesture");
        // The gesture ended: the check is due at once, not in 10 s.
        let (reads, fixes) = run_check(&mut w, due, false, Some(due), |_| OFF);
        assert_eq!(reads.len(), 2);
        assert_eq!(fixes, [(0x00C3, true), (0x0053, false)]);
    }

    #[test]
    fn watch_stops_the_pass_on_timeout_but_fails_on_link_loss() {
        let t0 = Instant::now();
        let due = t0 + Duration::from_secs(10);
        let mut w = watch(t0);
        let mut reads = 0;
        let r = w.check(due, &mut || false, false, Some(due), &mut |_| {
            reads += 1;
            Err(Timeout.into())
        }, &mut |_, _| Ok(()));
        assert!(r.is_ok());
        assert_eq!(reads, 1);
        let mut w = watch(t0);
        let r = w.check(
            due,
            &mut || false,
            false,
            Some(due),
            &mut |_| Err(anyhow::anyhow!("gone").context(crate::hidpp::transport::IoFailed("read"))),
            &mut |_, _| Ok(()),
        );
        assert!(r.is_err());
    }

    #[test]
    fn watch_in_a_series_runs_even_for_an_idle_mouse_but_not_while_busy() {
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        let mut w = watch(t0);
        let reads = std::cell::Cell::new(0);
        let go = |w: &mut ControlsWatch, now, busy, series, rx| {
            let read = &mut |_| {
                reads.set(reads.get() + 1);
                Ok(ON)
            };
            w.check(now, &mut || busy, series, rx, read, &mut |_, _| Ok(())).unwrap();
        };
        // Not due yet by its own timer, mouse silent for a minute: the series reads anyway.
        go(&mut w, at(3), false, true, Some(t0));
        // A busy gesture defers it, series or not.
        go(&mut w, at(60), true, true, Some(at(60)));
        assert_eq!(reads.get(), 2); // two buttons, once
    }

    fn removed(cid: u16) -> ForeignSet {
        ForeignSet { cid, divert: Some(false), raw_xy: Some(false) }
    }

    /// One restore after foreign settings; returns the (cid, raw_xy) written.
    fn restore(w: &mut ControlsWatch, now: Instant, busy: bool) -> Vec<(u16, bool)> {
        let mut writes = Vec::new();
        w.restore_foreign(now, &mut || busy, &mut |cid, raw| {
            writes.push((cid, raw));
            Ok(())
        })
        .unwrap();
        writes
    }

    #[test]
    fn a_diversion_taken_by_another_program_is_restored_once() {
        let t0 = Instant::now();
        let mut w = watch(t0);
        w.note_foreign(removed(0x00C3));
        w.note_foreign(removed(0x00C3));
        assert_eq!(restore(&mut w, t0, false), [(0x00C3, true)]);
        assert!(restore(&mut w, t0, false).is_empty());
        // A plain button is diverted without raw XY.
        w.note_foreign(removed(0x0053));
        assert_eq!(restore(&mut w, t0, false), [(0x0053, false)]);
        // Not ours, or a setting that keeps the diversion: nothing.
        w.note_foreign(removed(0x1234));
        w.note_foreign(ForeignSet { cid: 0x0053, divert: Some(true), raw_xy: None });
        w.note_foreign(ForeignSet { cid: 0x0053, divert: None, raw_xy: Some(false) });
        assert!(restore(&mut w, t0, false).is_empty());
    }

    #[test]
    fn a_diversion_taken_during_a_gesture_is_restored_after_it() {
        let t0 = Instant::now();
        let mut w = watch(t0);
        w.note_foreign(removed(0x00C3));
        assert!(restore(&mut w, t0, true).is_empty(), "no write in a gesture");
        assert!(restore(&mut w, t0, true).is_empty());
        assert_eq!(restore(&mut w, t0, false), [(0x00C3, true)]);
        assert!(restore(&mut w, t0, false).is_empty());
    }

    #[test]
    fn a_diversion_is_restored_at_most_three_times_a_minute() {
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        let mut w = watch(t0);
        for s in [0, 10, 20] {
            w.note_foreign(removed(0x00C3));
            assert_eq!(restore(&mut w, at(s), false).len(), 1, "restore at {s} s");
        }
        w.note_foreign(removed(0x00C3));
        assert!(restore(&mut w, at(30), false).is_empty(), "the fourth within a minute");
        // Another button has its own count; the first one is free again later.
        w.note_foreign(removed(0x0053));
        assert_eq!(restore(&mut w, at(31), false), [(0x0053, false)]);
        w.note_foreign(removed(0x00C3));
        assert_eq!(restore(&mut w, at(61), false), [(0x00C3, true)]);
    }

    #[test]
    fn only_a_setting_that_removes_the_diversion_is_restored_whoever_sent_it() {
        let t0 = Instant::now();
        let mut w = watch(t0);
        // What the daemon writes itself (the response carries its own ID, but
        // the program does not look at it): a plain button diverted without
        // raw XY, a gesture button with it. Neither removes anything.
        w.note_foreign(ForeignSet { cid: 0x0053, divert: Some(true), raw_xy: Some(false) });
        w.note_foreign(ForeignSet { cid: 0x00C3, divert: Some(true), raw_xy: Some(true) });
        assert!(restore(&mut w, t0, false).is_empty());
        // A response that removes the diversion is restored, with the ID of
        // our range as well (Options+ may use 0x08-0x0F).
        let m = Message::request(1, 9, 3, 0x09, &[0x00, 0x53, 0x22, 0x00, 0x53]).unwrap();
        let set = crate::hidpp::features::controls::parse_foreign_set(9, &m).unwrap();
        w.note_foreign(set);
        assert_eq!(restore(&mut w, t0, false), [(0x0053, false)]);
        // Our write after that does not start a loop.
        w.note_foreign(ForeignSet { cid: 0x0053, divert: Some(true), raw_xy: Some(false) });
        assert!(restore(&mut w, t0, false).is_empty());
    }

    #[test]
    fn the_restore_limit_warning_and_the_lost_diversion_warning_are_independent() {
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        let skip_limit = |w: &mut ControlsWatch, now| {
            for _ in 0..=FOREIGN_RESTORES_MAX {
                w.note_foreign(removed(0x00C3));
                restore(w, now, false);
            }
        };
        // The limit warning does not use up the lost diversion warning.
        let mut w = watch(t0);
        skip_limit(&mut w, at(1));
        assert!(!w.limit_warn.allow(at(2)), "the limit warning was given");
        assert!(w.warn.allow(at(2)), "the lost diversion warning is still free");
        // The lost diversion warning does not use up the limit warning.
        let mut w = watch(t0);
        run_check(&mut w, at(10), false, Some(at(10)), |_| OFF);
        assert!(!w.warn.allow(at(11)), "the lost diversion warning was given");
        skip_limit(&mut w, at(12));
        assert!(!w.limit_warn.allow(at(13)), "the limit warning was given as well");
    }

    #[test]
    fn a_failed_restore_returns_a_link_failure_only() {
        let t0 = Instant::now();
        let mut w = watch(t0);
        w.note_foreign(removed(0x00C3));
        assert!(w.restore_foreign(t0, &mut || false, &mut |_, _| Err(Timeout.into())).is_ok());
        w.note_foreign(removed(0x0053));
        let lost = |_: u16, _: bool| {
            Err(anyhow::anyhow!("gone").context(crate::hidpp::transport::IoFailed("write")))
        };
        assert!(w.restore_foreign(t0, &mut || false, &mut { lost }).is_err());
    }

    #[test]
    fn series_runs_every_minute_and_waits_for_the_end_of_a_gesture() {
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        let mut s = Series::new(t0);
        assert_eq!(s.start(at(59), false), Start::No);
        assert_eq!(s.deadline(at(59), false), Some(at(60)));
        // A button is held before it is due: wake at its time to note the deferral.
        assert_eq!(s.deadline(at(59), true), Some(at(60)));
        // Due, but a button is held: deferred, no deadline (the thread must not spin).
        assert_eq!(s.start(at(60), true), Start::No);
        assert_eq!(s.start(at(100), true), Start::No);
        assert_eq!(s.deadline(at(100), true), None);
        // The gesture ends: runs at once and reports how long it waited;
        // the next one comes a minute later, on time.
        assert_eq!(s.start(at(101), false), Start::Deferred(Duration::from_secs(41)));
        assert_eq!(s.start(at(160), false), Start::No);
        assert_eq!(s.start(at(161), false), Start::OnTime);
    }

    #[test]
    fn a_series_that_ran_on_time_reports_no_wait() {
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        let mut s = Series::new(t0);
        // Late wake-up without a gesture is not a deferral.
        assert_eq!(s.start(at(65), false), Start::OnTime);
        // Busy before it was due is not a deferral either.
        assert_eq!(s.start(at(100), true), Start::No);
        assert_eq!(s.start(at(125), false), Start::OnTime);
    }

    #[test]
    fn a_ping_pulls_the_series_forward() {
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        let mut s = Series::new(t0);
        s.pull_forward(at(20));
        assert_eq!(s.start(at(20), false), Start::OnTime);
        // Pulled forward during a gesture: waits for its end.
        s.pull_forward(at(30));
        assert_eq!(s.start(at(30), true), Start::No);
        assert_eq!(s.start(at(35), false), Start::Deferred(Duration::from_secs(5)));
        // A late ping never pushes the series back.
        let mut s = Series::new(t0);
        s.pull_forward(at(90));
        assert_eq!(s.start(at(60), false), Start::OnTime);
    }

    #[test]
    fn earliest_picks_the_sooner_time() {
        let now = Instant::now();
        let later = now + Duration::from_secs(1);
        assert_eq!(earliest(None, later), Some(later));
        assert_eq!(earliest(Some(now), later), Some(now));
        assert_eq!(earliest(Some(later), now), Some(now));
    }
}
