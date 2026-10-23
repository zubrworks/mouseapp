//! Detects desktops (Spaces) that stop switching. The daemon notes every
//! desktop-left/right step it sends and every change of the active Space
//! (`NSWorkspaceActiveSpaceDidChangeNotification`). A step that is followed
//! by no change within [`STUCK_AFTER`] is logged as `space stuck`, the next
//! change as `space resumed`. A step towards the last Space (nothing to
//! switch to) is not a failure and is skipped. Read-only; nothing is done
//! while no step is waiting for its change. A stuck desktop makes the daemon
//! restart the Dock (see [`crate::dock`]).

use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSWorkspace, NSWorkspaceActiveSpaceDidChangeNotification};
use objc2_foundation::{NSNotification, NSObjectProtocol};

use crate::actions::{self, Action};

/// A step without a change of Space for this long is a stuck desktop.
const STUCK_AFTER: Duration = Duration::from_millis(1500);

/// Direction of a desktop step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
}

/// The Spaces of the display the active Space is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// Number of Spaces (desktops and full-screen apps).
    pub count: usize,
    /// Position of the active Space, from 0.
    pub index: usize,
}

impl Layout {
    /// True if there is no Space in direction `dir`.
    fn at_edge(self, dir: Dir) -> bool {
        match dir {
            Dir::Left => self.index == 0,
            Dir::Right => self.index + 1 >= self.count,
        }
    }
}

/// What the state machine asks to be logged.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Report {
    Stuck {
        since: Instant,
        steps: u32,
        /// Time since the last change of Space (`None` if there was none yet).
        last_change: Option<Duration>,
        /// `None` if the layout could not be read.
        layout: Option<Layout>,
    },
    Resumed {
        after: Duration,
        steps: u32,
    },
}

/// Steps sent since the last change of Space.
#[derive(Clone, Copy, Debug)]
struct Pending {
    since: Instant,
    steps: u32,
    last: Dir,
}

/// Steps and changes of Space in time order (see the module docs). Pure
/// logic: it does not read the clock or the system.
#[derive(Debug, Default)]
struct Watch {
    last_change: Option<Instant>,
    /// Steps waiting for a change of Space.
    pending: Option<Pending>,
    /// Reported as stuck: when it started and the steps sent since.
    stuck: Option<(Instant, u32)>,
    /// The checking thread must end.
    quit: bool,
}

impl Watch {
    const fn new() -> Self {
        Watch { last_change: None, pending: None, stuck: None, quit: false }
    }

    /// `steps` desktop steps in direction `dir` were just sent.
    fn step(&mut self, now: Instant, dir: Dir, steps: u32) {
        if let Some((_, n)) = self.stuck.as_mut() {
            *n += steps;
            return;
        }
        match self.pending.as_mut() {
            Some(p) => {
                p.steps += steps;
                p.last = dir;
            }
            None => self.pending = Some(Pending { since: now, steps, last: dir }),
        }
    }

    /// The active Space changed.
    fn changed(&mut self, now: Instant) -> Option<Report> {
        self.last_change = Some(now);
        self.pending = None;
        self.stuck
            .take()
            .map(|(since, steps)| Report::Resumed { after: now.saturating_duration_since(since), steps })
    }

    /// When the oldest unanswered step is to be judged.
    fn deadline(&self) -> Option<Instant> {
        self.pending.map(|p| p.since + STUCK_AFTER)
    }

    /// Judges the unanswered steps whose time has come, `layout` being what
    /// the system says about the Spaces now (no change happened, so it is
    /// also what it was at the step). A step towards the last Space is
    /// dropped without a report. A change that came meanwhile cancels it.
    fn judge(&mut self, now: Instant, layout: Option<Layout>) -> Option<Report> {
        let p = self.pending.filter(|p| now >= p.since + STUCK_AFTER)?;
        self.pending = None;
        if layout.is_some_and(|l| l.at_edge(p.last)) {
            return None;
        }
        self.stuck = Some((p.since, p.steps));
        Some(Report::Stuck {
            since: p.since,
            steps: p.steps,
            last_change: self.last_change.map(|t| p.since.saturating_duration_since(t)),
            layout,
        })
    }
}

struct Shared {
    watch: Mutex<Watch>,
    wake: Condvar,
}

static SHARED: Shared = Shared { watch: Mutex::new(Watch::new()), wake: Condvar::new() };
/// A [`SpaceWatch`] runs; without one no changes are seen, so steps are not noted.
static ENABLED: AtomicBool = AtomicBool::new(false);

fn lock() -> MutexGuard<'static, Watch> {
    crate::util::lock(&SHARED.watch)
}

/// The step direction an action sends (desktop left or right), if it is one.
/// Compared with what the built-in names resolve to, so a custom shortcut
/// with the same keys counts too.
pub fn direction(action: &Action) -> Option<Dir> {
    static KEYS: OnceLock<Option<(Action, Action)>> = OnceLock::new();
    let keys = KEYS.get_or_init(|| {
        Some((actions::parse("desktop_left").ok()?, actions::parse("desktop_right").ok()?))
    });
    let (left, right) = keys.as_ref()?;
    if action == left {
        Some(Dir::Left)
    } else if action == right {
        Some(Dir::Right)
    } else {
        None
    }
}

/// A desktop step was sent. Cheap: it only records the step.
pub fn note_step(dir: Dir) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    lock().step(Instant::now(), dir, 1);
    SHARED.wake.notify_one();
}

fn on_space_change() {
    let now = Instant::now();
    let report = lock().changed(now);
    log::debug!("active space changed");
    if let Some(r) = report {
        log_report(&r, now);
    }
}

fn log_report(report: &Report, now: Instant) {
    match report {
        Report::Stuck { since, steps, last_change, layout } => {
            let edge = match layout {
                Some(l) => format!("no spaces={} current={}", l.count, l.index + 1),
                None => "unknown".to_string(),
            };
            log::warn!(
                "space stuck: since={} steps={steps} front={} last_change_ms={} edge={edge}",
                utc_time(now.saturating_duration_since(*since)),
                crate::app_overrides::frontmost_bundle().as_deref().unwrap_or("?"),
                last_change.map_or("none".to_string(), |d| d.as_millis().to_string()),
            );
        }
        Report::Resumed { after, steps } => {
            log::warn!("space resumed: after_ms={} steps={steps}", after.as_millis());
        }
    }
}

/// UTC time of day (as in the log) of the moment `ago` before now.
pub(crate) fn utc_time(ago: Duration) -> String {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .saturating_sub(ago);
    let day = since_epoch.as_secs() % 86_400;
    format!(
        "{:02}:{:02}:{:02}.{:03}Z",
        day / 3600,
        day % 3600 / 60,
        day % 60,
        since_epoch.subsec_millis()
    )
}

/// The checking thread: sleeps until a step waits for its change, then
/// judges it once its time has come.
fn check_loop() {
    let mut watch = lock();
    loop {
        if watch.quit {
            return;
        }
        let now = Instant::now();
        match watch.deadline() {
            None => {
                watch = SHARED.wake.wait(watch).unwrap_or_else(PoisonError::into_inner);
            }
            Some(at) if at > now => {
                watch = SHARED
                    .wake
                    .wait_timeout(watch, at - now)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
            Some(_) => {
                drop(watch);
                // Reading the Spaces asks WindowServer: not under the lock.
                let layout = query_layout();
                let now = Instant::now();
                let report = lock().judge(now, layout);
                if let Some(r) = report {
                    log_report(&r, now);
                    if matches!(r, Report::Stuck { .. }) {
                        crate::dock::restart_if_allowed("stuck");
                    }
                }
                watch = lock();
            }
        }
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; `SpaceObserver` has no Drop.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "MouseSpaceObserver"]
    struct SpaceObserver;

    // SAFETY: NSObjectProtocol has no safety requirements.
    unsafe impl NSObjectProtocol for SpaceObserver {}

    impl SpaceObserver {
        /// `NSWorkspaceActiveSpaceDidChangeNotification`.
        #[unsafe(method(spaceChanged:))]
        fn space_changed(&self, _note: &NSNotification) {
            on_space_change();
        }
    }
);

/// Follows the changes of the active Space through `NSWorkspace`
/// notifications (the main run loop must run) and judges the steps noted by
/// [`note_step`]. Dropping it stops both.
pub struct SpaceWatch {
    observer: Retained<SpaceObserver>,
    checker: Option<JoinHandle<()>>,
}

impl SpaceWatch {
    pub fn start(mtm: MainThreadMarker) -> SpaceWatch {
        let observer: Retained<SpaceObserver> =
            // SAFETY: NSObject's `init` has this signature.
            unsafe { msg_send![SpaceObserver::alloc(mtm), init] };
        // SAFETY: `spaceChanged:` takes one NSNotification and returns nothing;
        // the observer is removed (Drop) before it is released.
        unsafe {
            NSWorkspace::sharedWorkspace().notificationCenter().addObserver_selector_name_object(
                &observer,
                sel!(spaceChanged:),
                Some(NSWorkspaceActiveSpaceDidChangeNotification),
                None,
            );
        }
        *lock() = Watch::new();
        let checker = std::thread::Builder::new()
            .name("spaces".into())
            .spawn(check_loop)
            .inspect_err(|e| log::warn!("cannot start the desktop check: {e}"))
            .ok();
        ENABLED.store(checker.is_some(), Ordering::Relaxed);
        SpaceWatch { observer, checker }
    }
}

impl Drop for SpaceWatch {
    fn drop(&mut self) {
        ENABLED.store(false, Ordering::Relaxed);
        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        // SAFETY: the observer was added by `start`.
        unsafe { center.removeObserver(&self.observer) };
        lock().quit = true;
        SHARED.wake.notify_all();
        if let Some(h) = self.checker.take()
            && h.join().is_err()
        {
            log::error!("the desktop check thread panicked");
        }
    }
}

// ---- reading the Spaces (private CoreGraphics Services API, read only) ----

type MainConnection = unsafe extern "C" fn() -> c_int;
type CopyDisplaySpaces = unsafe extern "C" fn(c_int) -> *const c_void;
type ActiveSpace = unsafe extern "C" fn(c_int) -> u64;

struct Api {
    connection: MainConnection,
    copy_spaces: CopyDisplaySpaces,
    active: ActiveSpace,
}

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
}

/// The functions, looked up once; `None` if the system does not have them.
fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| {
        const RTLD_LAZY: c_int = 1;
        let path = c"/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight";
        // SAFETY: valid NUL-terminated strings; the library stays loaded.
        let lib = unsafe { dlopen(path.as_ptr(), RTLD_LAZY) };
        if lib.is_null() {
            return None;
        }
        let sym = |name: &CStr| {
            // SAFETY: `lib` is a valid handle, `name` a NUL-terminated string.
            let p = unsafe { dlsym(lib, name.as_ptr()) };
            (!p.is_null()).then_some(p)
        };
        // SAFETY: these symbols have exactly these C signatures.
        unsafe {
            Some(Api {
                connection: std::mem::transmute::<*mut c_void, MainConnection>(sym(
                    c"CGSMainConnectionID",
                )?),
                copy_spaces: std::mem::transmute::<*mut c_void, CopyDisplaySpaces>(sym(
                    c"CGSCopyManagedDisplaySpaces",
                )?),
                active: std::mem::transmute::<*mut c_void, ActiveSpace>(sym(c"CGSGetActiveSpace")?),
            })
        }
    })
    .as_ref()
}

/// One display as `CGSCopyManagedDisplaySpaces` describes it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Display {
    /// "Display Identifier": the display UUID (or `Main`).
    identifier: Option<String>,
    /// Space ids, in order.
    ids: Vec<i64>,
    /// "type" of the Space shown on the display now.
    current_type: Option<i64>,
}

/// Every display and the id of the active Space.
fn query_spaces() -> Option<(Vec<Display>, i64)> {
    use core_foundation::array::CFArray;
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;

    let api = api()?;
    // SAFETY: plain calls of the looked-up functions.
    let (cid, active) = unsafe {
        let cid = (api.connection)();
        (cid, (api.active)(cid))
    };
    // SAFETY: the function returns a new CFArray (create rule) or null.
    let raw = unsafe { (api.copy_spaces)(cid) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: `raw` is a non-null CFArray we own.
    let displays: CFArray<CFType> = unsafe { CFArray::wrap_under_create_rule(raw.cast()) };
    let id_of = |d: &CFDictionary<CFString, CFType>| {
        ["ManagedSpaceID", "id64"].iter().find_map(|k| {
            d.find(CFString::new(k)).and_then(|v| v.downcast::<CFNumber>()).and_then(|n| n.to_i64())
        })
    };
    let dict = |v: &CFType| {
        v.downcast::<CFDictionary>().map(|d| {
            // SAFETY: the managed-spaces dictionaries are keyed by strings.
            unsafe { CFDictionary::<CFString, CFType>::wrap_under_get_rule(d.as_concrete_TypeRef()) }
        })
    };
    let mut out = Vec::new();
    for display in displays.iter() {
        let display = dict(&display)?;
        let spaces = display.find(CFString::new("Spaces"))?.downcast::<CFArray>()?;
        // SAFETY: the "Spaces" array holds CFTypes.
        let spaces: CFArray<CFType> = unsafe { CFArray::wrap_under_get_rule(spaces.as_concrete_TypeRef()) };
        let identifier = display
            .find(CFString::new("Display Identifier"))
            .and_then(|v| v.downcast::<CFString>())
            .map(|s| s.to_string());
        let current_type = display
            .find(CFString::new("Current Space"))
            .and_then(|v| dict(&v))
            .and_then(|d| d.find(CFString::new("type")).and_then(|v| v.downcast::<CFNumber>()))
            .and_then(|n| n.to_i64());
        out.push(Display {
            identifier,
            ids: spaces.iter().filter_map(|s| dict(&s).and_then(|d| id_of(&d))).collect(),
            current_type,
        });
    }
    Some((out, i64::try_from(active).ok()?))
}

/// The Spaces of the display that holds `active`.
fn layout_of(displays: &[Display], active: i64) -> Option<Layout> {
    displays.iter().find_map(|d| {
        d.ids.iter().position(|&id| id == active).map(|index| Layout { count: d.ids.len(), index })
    })
}

/// "type" of a full-screen app's Space.
const SPACE_TYPE_FULLSCREEN: i64 = 4;

/// True if the Space shown now on the display `cursor` (its UUID) is on is a
/// full-screen one. With a single display it is that display whatever its
/// identifier. `None` if the display or its Space type is not known.
fn fullscreen_on(displays: &[Display], cursor: Option<&str>) -> Option<bool> {
    let display = match displays {
        [only] => only,
        _ => {
            let cursor = cursor?;
            displays
                .iter()
                .find(|d| d.identifier.as_deref().is_some_and(|i| i.eq_ignore_ascii_case(cursor)))?
        }
    };
    display.current_type.map(|t| t == SPACE_TYPE_FULLSCREEN)
}

/// UUID of the display the mouse pointer is on.
fn cursor_display_uuid() -> Option<String> {
    use core_foundation::base::{CFRelease, TCFType};
    use core_foundation::string::{CFString, CFStringRef};
    use core_graphics::display::CGDisplay;
    use core_graphics::event::CGEvent;
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

    unsafe extern "C" {
        fn CGDisplayCreateUUIDFromDisplayID(display: u32) -> *const c_void;
        fn CFUUIDCreateString(alloc: *const c_void, uuid: *const c_void) -> CFStringRef;
    }

    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()?;
    let point = CGEvent::new(source).ok()?.location();
    let id = *CGDisplay::displays_with_point(point, 1).ok()?.0.first()?;
    // SAFETY: plain calls; both Create functions return owned objects (or null).
    unsafe {
        let uuid = CGDisplayCreateUUIDFromDisplayID(id);
        if uuid.is_null() {
            return None;
        }
        let string = CFUUIDCreateString(std::ptr::null(), uuid);
        CFRelease(uuid.cast());
        (!string.is_null()).then(|| CFString::wrap_under_create_rule(string).to_string())
    }
}

/// True if the Space the mouse pointer is on is a full-screen app's. `None`
/// if that cannot be read (callers then act as if it were not).
pub fn cursor_space_is_fullscreen() -> Option<bool> {
    let (displays, _) = query_spaces()?;
    fullscreen_on(&displays, cursor_display_uuid().as_deref())
}

pub(crate) fn query_layout() -> Option<Layout> {
    let (displays, active) = query_spaces()?;
    layout_of(&displays, active)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    fn display(identifier: Option<&str>, ids: &[i64], current_type: Option<i64>) -> Display {
        Display { identifier: identifier.map(str::to_string), ids: ids.to_vec(), current_type }
    }

    fn mid() -> Option<Layout> {
        Some(Layout { count: 4, index: 1 })
    }

    #[test]
    fn step_without_change_is_stuck_after_the_wait() {
        let t0 = Instant::now();
        let mut w = Watch::new();
        w.changed(t0);
        w.step(t0 + secs(2.0), Dir::Right, 1);
        w.step(t0 + secs(2.2), Dir::Right, 1);
        assert_eq!(w.deadline(), Some(t0 + secs(3.5)));
        assert_eq!(w.judge(t0 + secs(3.0), mid()), None);
        assert!(w.pending.is_some(), "not judged before its time");
        let r = w.judge(t0 + secs(3.6), mid());
        assert_eq!(
            r,
            Some(Report::Stuck {
                since: t0 + secs(2.0),
                steps: 2,
                last_change: Some(secs(2.0)),
                layout: mid()
            })
        );
        assert_eq!(w.deadline(), None);
    }

    #[test]
    fn change_in_time_cancels_the_wait() {
        let t0 = Instant::now();
        let mut w = Watch::new();
        w.step(t0, Dir::Left, 1);
        assert_eq!(w.changed(t0 + secs(0.4)), None);
        assert_eq!(w.deadline(), None);
        assert_eq!(w.judge(t0 + secs(2.0), mid()), None);
    }

    #[test]
    fn change_after_stuck_is_resumed_with_all_steps() {
        let t0 = Instant::now();
        let mut w = Watch::new();
        w.step(t0, Dir::Right, 1);
        assert!(matches!(w.judge(t0 + secs(1.6), None), Some(Report::Stuck { layout: None, .. })));
        w.step(t0 + secs(2.0), Dir::Left, 1);
        w.step(t0 + secs(2.5), Dir::Left, 1);
        assert_eq!(w.deadline(), None, "no second report while stuck");
        assert_eq!(
            w.changed(t0 + secs(5.0)),
            Some(Report::Resumed { after: secs(5.0), steps: 3 })
        );
        // Working again: the next unanswered step is a new case.
        w.step(t0 + secs(6.0), Dir::Right, 1);
        assert!(w.deadline().is_some());
        assert_eq!(w.changed(t0 + secs(6.3)), None);
    }

    #[test]
    fn step_towards_the_last_space_is_skipped() {
        let t0 = Instant::now();
        let last = Some(Layout { count: 3, index: 2 });
        let first = Some(Layout { count: 3, index: 0 });
        let mut w = Watch::new();
        w.step(t0, Dir::Right, 1);
        assert_eq!(w.judge(t0 + secs(2.0), last), None);
        assert!(w.stuck.is_none() && w.pending.is_none());
        w.step(t0 + secs(3.0), Dir::Left, 1);
        assert_eq!(w.judge(t0 + secs(5.0), last).map(|_| ()), Some(()), "the other way is not an edge");
        let mut w = Watch::new();
        w.step(t0, Dir::Left, 1);
        assert_eq!(w.judge(t0 + secs(2.0), first), None);
        // One Space only: nowhere to go either way.
        for dir in [Dir::Left, Dir::Right] {
            w.step(t0 + secs(3.0), dir, 1);
            assert_eq!(w.judge(t0 + secs(5.0), Some(Layout { count: 1, index: 0 })), None);
        }
    }

    #[test]
    fn unknown_layout_is_not_skipped() {
        let t0 = Instant::now();
        let mut w = Watch::new();
        w.step(t0, Dir::Right, 1);
        assert!(matches!(w.judge(t0 + secs(2.0), None), Some(Report::Stuck { layout: None, .. })));
    }

    #[test]
    fn layout_is_taken_from_the_display_with_the_active_space() {
        let displays = vec![display(None, &[1, 2, 3], None), display(None, &[10, 11], None)];
        assert_eq!(layout_of(&displays, 2), Some(Layout { count: 3, index: 1 }));
        assert_eq!(layout_of(&displays, 11), Some(Layout { count: 2, index: 1 }));
        assert_eq!(layout_of(&displays, 99), None);
        assert_eq!(layout_of(&[], 1), None);
    }

    #[test]
    fn fullscreen_is_read_from_the_display_under_the_cursor() {
        let desk = display(Some("AAA"), &[1, 2], Some(0));
        let full = display(Some("BBB"), &[3, 4], Some(SPACE_TYPE_FULLSCREEN));
        let unknown = display(Some("CCC"), &[5], None);
        let two = [desk.clone(), full.clone()];
        assert_eq!(fullscreen_on(&two, Some("bbb")), Some(true));
        assert_eq!(fullscreen_on(&two, Some("AAA")), Some(false));
        // Unknown display or no cursor display: not known.
        assert_eq!(fullscreen_on(&two, Some("ZZZ")), None);
        assert_eq!(fullscreen_on(&two, None), None);
        // Unknown Space type: not known.
        assert_eq!(fullscreen_on(&[desk.clone(), unknown], Some("CCC")), None);
        // One display: that one, whatever its identifier.
        assert_eq!(fullscreen_on(&[display(Some("Main"), &[3], Some(4))], None), Some(true));
        assert_eq!(fullscreen_on(&[desk], Some("other")), Some(false));
        assert_eq!(fullscreen_on(&[], Some("AAA")), None);
    }

    #[test]
    fn desktop_actions_are_recognised_by_what_they_send() {
        let parse = |s| actions::parse(s).unwrap();
        assert_eq!(direction(&parse("desktop_left")), Some(Dir::Left));
        assert_eq!(direction(&parse("shortcut:ctrl+right")), Some(Dir::Right));
        assert_eq!(direction(&parse("mission_control")), None);
        assert_eq!(direction(&parse("switch_app")), None);
    }

    #[test]
    fn reading_the_spaces_does_not_panic() {
        // The value depends on the session (None without a GUI).
        let _ = query_layout();
        let _ = cursor_space_is_fullscreen();
    }

    #[test]
    fn utc_time_has_the_log_format() {
        let s = utc_time(Duration::ZERO);
        assert_eq!(s.len(), 13);
        assert!(s.ends_with('Z') && s.as_bytes()[2] == b':' && s.as_bytes()[8] == b'.');
    }
}
