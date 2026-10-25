//! Keyboard shortcut and media key synthesis (CoreGraphics / AppKit).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, Once};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};

/// Virtual key code of the Launchpad key.
pub const KEY_LAUNCHPAD: u16 = 131;

/// A key code with modifier flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shortcut {
    code: u16,
    flags: u64,
}

/// Media / system keys (NX system-defined events).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Media {
    VolumeUp,
    VolumeDown,
    Mute,
    PlayPause,
    Next,
    Previous,
}

impl Media {
    fn nx_code(self) -> i64 {
        match self {
            Media::VolumeUp => 0,
            Media::VolumeDown => 1,
            Media::Mute => 7,
            Media::PlayPause => 16,
            Media::Next => 17,
            Media::Previous => 18,
        }
    }
}

const FLAG_SHIFT: u64 = 0x20000;
const FLAG_CTRL: u64 = 0x40000;
const FLAG_ALT: u64 = 0x80000;
const FLAG_CMD: u64 = 0x100000;
const FLAG_FN: u64 = 0x800000;
const FLAG_NUMPAD: u64 = 0x200000;

fn key_code(name: &str) -> Option<u16> {
    const LETTERS: [u16; 26] = [
        0, 11, 8, 2, 14, 3, 5, 4, 34, 38, 40, 37, 46, 45, 31, 35, 12, 15, 1, 17, 32, 9, 13, 7, 16, 6,
    ];
    const DIGITS: [u16; 10] = [29, 18, 19, 20, 21, 23, 22, 26, 28, 25];
    const FKEYS: [u16; 12] = [122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111];
    let b = name.as_bytes();
    if b.len() == 1 {
        match b[0] {
            c @ b'a'..=b'z' => return Some(LETTERS[(c - b'a') as usize]),
            c @ b'0'..=b'9' => return Some(DIGITS[(c - b'0') as usize]),
            b'[' => return Some(33),
            b']' => return Some(30),
            b'-' => return Some(27),
            b'=' => return Some(24),
            b',' => return Some(43),
            b'.' => return Some(47),
            b'/' => return Some(44),
            b';' => return Some(41),
            b'\'' => return Some(39),
            b'\\' => return Some(42),
            b'`' => return Some(50),
            _ => {}
        }
    }
    if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<usize>().ok())
        && (1..=12).contains(&n)
    {
        return Some(FKEYS[n - 1]);
    }
    Some(match name {
        "return" | "enter" => 36,
        "tab" => 48,
        "space" => 49,
        "delete" | "backspace" => 51,
        "esc" | "escape" => 53,
        "left" => 123,
        "right" => 124,
        "down" => 125,
        "up" => 126,
        _ => return None,
    })
}

impl Shortcut {
    pub fn raw(code: u16) -> Self {
        Shortcut { code, flags: 0 }
    }

    /// Parses e.g. `cmd+shift+4`, `ctrl+left`, `f11`.
    pub fn parse(s: &str) -> Result<Self> {
        let mut flags = 0u64;
        let mut code = None;
        for part in s.split('+').map(|p| p.trim().to_lowercase()) {
            match part.as_str() {
                "cmd" | "command" => flags |= FLAG_CMD,
                "shift" => flags |= FLAG_SHIFT,
                "ctrl" | "control" => flags |= FLAG_CTRL,
                "alt" | "opt" | "option" => flags |= FLAG_ALT,
                other => {
                    if code.is_some() {
                        bail!("{}", rust_i18n::t!("action.bad_shortcut", value = s));
                    }
                    code = key_code(other);
                    if code.is_none() {
                        bail!("{}", rust_i18n::t!("action.bad_shortcut", value = s));
                    }
                }
            }
        }
        let Some(code) = code else {
            bail!("{}", rust_i18n::t!("action.bad_shortcut", value = s));
        };
        // Arrow keys carry the Fn and NumericPad flags on real keyboards;
        // WindowServer may ignore e.g. Ctrl+Left sent without them.
        if matches!(code, 123..=126) {
            flags |= FLAG_FN | FLAG_NUMPAD;
        }
        Ok(Shortcut { code, flags })
    }

    /// True for `cmd+tab` and `cmd+shift+tab` (the app switcher).
    pub fn is_app_switcher(&self) -> bool {
        self.code == 48 && (self.flags == FLAG_CMD || self.flags == FLAG_CMD | FLAG_SHIFT)
    }
}

/// Virtual key code of the left Command key.
const KEY_CMD: u16 = 55;

/// Cmd is held on purpose (app switcher); not reported as stuck.
static CMD_HELD: AtomicBool = AtomicBool::new(false);
/// `kCGEventSourceStateHIDSystemState`.
const HID_SYSTEM_STATE: i32 = 1;
/// Cmd, Ctrl, Opt and Shift.
const FLAGS_MODIFIERS: u64 = FLAG_CMD | FLAG_CTRL | FLAG_ALT | FLAG_SHIFT;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceFlagsState(state_id: i32) -> u64;
}

/// Modifiers (Cmd, Ctrl, Opt, Shift) the system currently believes are held.
fn system_modifiers() -> u64 {
    // SAFETY: plain query of the global event state, no pointers involved.
    unsafe { CGEventSourceFlagsState(HID_SYSTEM_STATE) & FLAGS_MODIFIERS }
}

/// True if `flags` hold Cmd, Ctrl or Opt: keys that give the mouse wheel
/// another meaning (zoom, ...). Shift is left out on purpose.
fn has_wheel_modifier(flags: u64) -> bool {
    flags & (FLAG_CMD | FLAG_CTRL | FLAG_ALT) != 0
}

/// True while Cmd, Ctrl or Opt is physically down (the scroll glide stops
/// then, so the app does not turn it into zoom).
pub fn wheel_modifier_held() -> bool {
    has_wheel_modifier(system_modifiers())
}

/// Modifiers held on purpose right now: the user's on a real keyboard plus
/// the Cmd of the app switcher.
fn held_modifiers() -> u64 {
    let cmd = if CMD_HELD.load(Ordering::Relaxed) { FLAG_CMD } else { 0 };
    system_modifiers() | cmd
}

/// Modifiers that are down but should not be: everything the system reports
/// except what was held on purpose when the shortcut was sent.
fn stuck_flags(system: u64, held: u64) -> u64 {
    system & FLAGS_MODIFIERS & !held
}

/// Minimum time between "modifiers stuck" log entries.
const STUCK_LOG_EVERY: Duration = Duration::from_secs(10);
/// Quiet time after the last shortcut before the stuck check runs, as the
/// system state can lag behind the events and a repeating gesture sends
/// shortcuts at irregular intervals (each one legitimately holds its
/// modifiers). The delay only makes a check rarely wasted; `ShortcutCounters`
/// makes a check that overlaps a shortcut impossible to report.
const STUCK_CHECK_DELAY: Duration = Duration::from_millis(300);

/// Shortcuts started and finished so far. A check may report only when no
/// shortcut was in progress before reading the flags and none started during
/// the read; then a modifier that is down belongs to no shortcut.
struct ShortcutCounters {
    started: AtomicU64,
    done: AtomicU64,
}

impl ShortcutCounters {
    const fn new() -> Self {
        ShortcutCounters { started: AtomicU64::new(0), done: AtomicU64::new(0) }
    }

    /// Marks a shortcut as in progress until the guard is dropped.
    fn begin(&self) -> ShortcutGuard<'_> {
        self.started.fetch_add(1, Ordering::SeqCst);
        ShortcutGuard(self)
    }

    /// Runs `read` and returns its result only if no shortcut was in progress
    /// before it and none started while it ran.
    fn read_quiet<T>(&self, read: impl FnOnce() -> T) -> Option<T> {
        let started = self.started.load(Ordering::SeqCst);
        if self.done.load(Ordering::SeqCst) != started {
            return None;
        }
        let value = read();
        (self.started.load(Ordering::SeqCst) == started).then_some(value)
    }
}

struct ShortcutGuard<'a>(&'a ShortcutCounters);

impl Drop for ShortcutGuard<'_> {
    fn drop(&mut self) {
        self.0.done.fetch_add(1, Ordering::SeqCst);
    }
}

static SHORTCUTS: ShortcutCounters = ShortcutCounters::new();

/// Last "modifiers stuck" log entry and the occurrences skipped since then.
static STUCK_LOG: Mutex<(Option<Instant>, u32)> = Mutex::new((None, 0));

/// Counts an occurrence; returns the number to report when an entry is due
/// (at most one per `STUCK_LOG_EVERY`).
fn stuck_log_due(now: Instant) -> Option<u32> {
    let mut log = STUCK_LOG.lock().unwrap_or_else(|e| e.into_inner());
    log.1 += 1;
    if log.0.is_some_and(|last| now.duration_since(last) < STUCK_LOG_EVERY) {
        return None;
    }
    log.0 = Some(now);
    Some(std::mem::take(&mut log.1))
}

/// The most recent shortcut waiting for its stuck check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingCheck {
    due: Instant,
    code: u16,
    flags: u64,
    /// Modifiers held on purpose when the shortcut was sent.
    held: u64,
}

/// At most one pending check: a new shortcut replaces and postpones it.
#[derive(Default)]
struct StuckCheck {
    pending: Option<PendingCheck>,
}

impl StuckCheck {
    fn schedule(&mut self, now: Instant, code: u16, flags: u64, held: u64) {
        self.pending = Some(PendingCheck { due: now + STUCK_CHECK_DELAY, code, flags, held });
    }

    /// The pending check when it is due at `now`; otherwise how long to wait
    /// (`None` when nothing is pending).
    fn poll(&mut self, now: Instant) -> Result<PendingCheck, Option<Duration>> {
        match self.pending {
            None => Err(None),
            Some(p) if p.due > now => Err(Some(p.due - now)),
            Some(p) => {
                self.pending = None;
                Ok(p)
            }
        }
    }
}

struct StuckChecker {
    state: Mutex<StuckCheck>,
    wake: Condvar,
}

static STUCK_CHECKER: StuckChecker = StuckChecker { state: Mutex::new(StuckCheck { pending: None }), wake: Condvar::new() };
/// Starts the single checker thread on first use.
static STUCK_THREAD: Once = Once::new();

fn stuck_checker_loop() {
    let checker = &STUCK_CHECKER;
    let mut state = checker.state.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        match state.poll(Instant::now()) {
            Err(None) => state = checker.wake.wait(state).unwrap_or_else(|e| e.into_inner()),
            Err(Some(wait)) => {
                state = checker.wake.wait_timeout(state, wait).unwrap_or_else(|e| e.into_inner()).0;
            }
            Ok(check) => {
                drop(state);
                // A shortcut in progress (or started meanwhile) legitimately
                // holds its modifiers: skip, its own check follows.
                let stuck = SHORTCUTS.read_quiet(system_modifiers).map_or(0, |system| stuck_flags(system, check.held));
                if stuck != 0
                    && let Some(count) = stuck_log_due(Instant::now())
                {
                    log::warn!(
                        "modifiers stuck: flags={stuck:#x} after key={} shortcut_flags={:#x} (x{count} since last entry)",
                        check.code,
                        check.flags
                    );
                }
                state = checker.state.lock().unwrap_or_else(|e| e.into_inner());
            }
        }
    }
}

/// Schedules the stuck check (a stuck modifier turns clicks into
/// Ctrl/Cmd-clicks). One thread serves all shortcuts and checks only once the
/// shortcuts stop, so the mouse thread never waits.
fn warn_if_modifiers_stuck(code: u16, flags: u64, held: u64) {
    STUCK_THREAD.call_once(|| {
        let _ = std::thread::Builder::new().name("stuck-check".into()).spawn(stuck_checker_loop);
    });
    STUCK_CHECKER.state.lock().unwrap_or_else(|e| e.into_inner()).schedule(Instant::now(), code, flags, held);
    STUCK_CHECKER.wake.notify_one();
}

/// Presses (`down`) or releases the Command key on its own, so that following
/// shortcuts with the Cmd flag behave as if Cmd were held.
pub fn cmd_key(down: bool) -> Result<()> {
    CMD_HELD.store(down, Ordering::Relaxed);
    let src = super::event_source()?;
    let ev = CGEvent::new_keyboard_event(src, KEY_CMD, down)
        .map_err(|_| anyhow::anyhow!("cannot create keyboard event"))?;
    ev.set_flags(CGEventFlags::from_bits_retain(if down { FLAG_CMD } else { 0 }));
    ev.post(CGEventTapLocation::HID);
    Ok(())
}

/// Modifier keys in press order: (virtual key code, flag).
const MODIFIER_KEYS: [(u16, u64); 4] =
    [(59, FLAG_CTRL), (56, FLAG_SHIFT), (58, FLAG_ALT), (KEY_CMD, FLAG_CMD)];

/// Events of a shortcut as a real keyboard sends them: (key code, down, flags).
/// Modifiers go down first (flags growing), then the key, then the key goes up
/// and the modifiers are released in reverse order (flags shrinking). A
/// modifier of the shortcut that is already `held` (by the user, or Cmd of the
/// app switcher) is neither pressed nor released, but stays in the flags.
fn shortcut_events(s: &Shortcut, held: u64) -> Vec<(u16, bool, u64)> {
    let held = held & s.flags;
    let mods: Vec<(u16, u64)> =
        MODIFIER_KEYS.into_iter().filter(|&(_, flag)| s.flags & flag != 0 && held & flag == 0).collect();
    let mut events = Vec::with_capacity(mods.len() * 2 + 2);
    let mut flags = held;
    for &(code, flag) in &mods {
        flags |= flag;
        events.push((code, true, flags));
    }
    events.push((s.code, true, s.flags));
    events.push((s.code, false, s.flags));
    for &(code, flag) in mods.iter().rev() {
        flags &= !flag;
        events.push((code, false, flags));
    }
    events
}

/// Presses and releases a shortcut, modifiers included.
pub fn press(s: &Shortcut) -> Result<()> {
    let _in_progress = SHORTCUTS.begin();
    let src = super::event_source()?;
    let held = held_modifiers();
    for (code, down, flags) in shortcut_events(s, held) {
        let ev = CGEvent::new_keyboard_event(src.clone(), code, down)
            .map_err(|_| anyhow::anyhow!("cannot create keyboard event"))?;
        ev.set_flags(CGEventFlags::from_bits_retain(flags));
        ev.post(CGEventTapLocation::HID);
    }
    if s.flags & FLAGS_MODIFIERS != 0 {
        warn_if_modifiers_stuck(s.code, s.flags, held);
    }
    Ok(())
}

/// Layout of IOKit's `NXEventData` for a system-defined (media key) event:
/// `compound.subType` at offset 2 and `compound.misc.L[0]` at offset 4.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NxEventData {
    reserved: i16,
    sub_type: i16,
    misc: [i32; 11],
}

/// `NX_SUBTYPE_AUX_CONTROL_BUTTONS`: media / auxiliary control keys.
const NX_SUBTYPE_AUX_CONTROL_BUTTONS: i16 = 8;
/// `NX_SYSDEFINED` event type.
const NX_SYSDEFINED: u32 = 14;
/// `kNXEventDataVersion`.
const NX_EVENT_DATA_VERSION: u32 = 2;
/// `kIOHIDParamConnectType`: connection used to post events.
const IOHID_PARAM_CONNECT_TYPE: u32 = 1;

/// Event data of a media key press (`down`) or release.
fn media_event_data(m: Media, down: bool) -> NxEventData {
    let state: i32 = if down { 0xA } else { 0xB };
    let mut misc = [0; 11];
    misc[0] = ((m.nx_code() as i32) << 16) | (state << 8);
    NxEventData { reserved: 0, sub_type: NX_SUBTYPE_AUX_CONTROL_BUTTONS, misc }
}

#[repr(C)]
struct IoGPoint {
    x: i16,
    y: i16,
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const std::ffi::c_char) -> *const std::ffi::c_void;
    fn IOServiceGetMatchingService(main_port: u32, matching: *const std::ffi::c_void) -> u32;
    fn IOServiceOpen(service: u32, task: u32, kind: u32, connect: *mut u32) -> i32;
    fn IOObjectRelease(object: u32) -> i32;
    fn IOHIDPostEvent(
        connect: u32,
        event_type: u32,
        location: IoGPoint,
        data: *const NxEventData,
        version: u32,
        flags: u32,
        options: u32,
    ) -> i32;
}

unsafe extern "C" {
    /// This task's mach port (libSystem).
    static mach_task_self_: u32;
}

/// Connection to the HID system, opened on first use and kept.
static HID_CONNECTION: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

fn hid_connection() -> Result<u32> {
    if let Some(&c) = HID_CONNECTION.get() {
        return Ok(c);
    }
    // SAFETY: the class name is a NUL-terminated string; the matching
    // dictionary is consumed by `IOServiceGetMatchingService`; the service is
    // released after the connection is opened; `connect` is a valid out pointer.
    let connect = unsafe {
        let matching = IOServiceMatching(c"IOHIDSystem".as_ptr());
        let service = IOServiceGetMatchingService(0, matching);
        if service == 0 {
            bail!("cannot find the HID system service");
        }
        let mut connect = 0;
        let rc = IOServiceOpen(service, mach_task_self_, IOHID_PARAM_CONNECT_TYPE, &mut connect);
        IOObjectRelease(service);
        if rc != 0 {
            bail!("cannot open the HID system (error {rc})");
        }
        connect
    };
    Ok(*HID_CONNECTION.get_or_init(|| connect))
}

/// Sends a media key press + release as NX system-defined events.
///
/// The events go straight to the HID system (IOKit). Building them with
/// `NSEvent` needs AppKit, which is not safe off the main thread, and
/// `CGEvent` has no field for the media key data.
pub fn media(m: Media) -> Result<()> {
    let connect = hid_connection()?;
    for down in [true, false] {
        let data = media_event_data(m, down);
        // SAFETY: `data` is a valid `NXEventData` (48 bytes) for the call.
        let rc = unsafe {
            IOHIDPostEvent(connect, NX_SYSDEFINED, IoGPoint { x: 0, y: 0 }, &data, NX_EVENT_DATA_VERSION, 0, 0)
        };
        if rc != 0 {
            bail!("cannot post the media key event (error {rc})");
        }
    }
    Ok(())
}

type CFStringRef = *const std::ffi::c_void;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// Private Dock API; returns 0 on success.
    fn CoreDockSendNotification(notification: CFStringRef, unused: *const std::ffi::c_void) -> i32;
}

/// Toggles Show Desktop through the Dock notification (F11 with Fn does not work reliably).
pub fn show_desktop() -> Result<()> {
    dock_notification("com.apple.showdesktop.awake")
}

/// Sends a notification to the Dock, e.g. `com.apple.expose.awake` (Mission Control).
pub fn dock_notification(name: &str) -> Result<()> {
    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;
    let name = CFString::new(name);
    // SAFETY: `name` is a valid CFString that outlives the call; the second argument is unused.
    let rc = unsafe { CoreDockSendNotification(name.as_concrete_TypeRef().cast(), std::ptr::null()) };
    if rc != 0 {
        bail!("{}", rust_i18n::t!("action.dock_failed", rc = rc));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_modifiers_are_cmd_ctrl_opt() {
        assert!(has_wheel_modifier(FLAG_CMD));
        assert!(has_wheel_modifier(FLAG_CTRL));
        assert!(has_wheel_modifier(FLAG_ALT));
        assert!(has_wheel_modifier(FLAG_SHIFT | FLAG_CMD));
        assert!(!has_wheel_modifier(FLAG_SHIFT));
        assert!(!has_wheel_modifier(0));
    }

    #[test]
    fn media_event_data_layout() {
        assert_eq!(std::mem::size_of::<NxEventData>(), 48);
        let down = media_event_data(Media::VolumeUp, true);
        assert_eq!(down.sub_type, 8);
        assert_eq!(down.misc[0], 0xA00); // key 0 (volume up), key down
        assert_eq!(media_event_data(Media::VolumeUp, false).misc[0], 0xB00);
        assert_eq!(media_event_data(Media::Mute, true).misc[0], (7 << 16) | 0xA00);
        assert_eq!(media_event_data(Media::Previous, false).misc[0], (18 << 16) | 0xB00);
    }

    #[test]
    fn parse_shortcuts() {
        let s = Shortcut::parse("cmd+shift+4").unwrap();
        assert_eq!(s.code, 21);
        assert_eq!(s.flags, FLAG_CMD | FLAG_SHIFT);
        assert_eq!(Shortcut::parse("ctrl+up").unwrap().code, 126);
        assert_eq!(Shortcut::parse("ctrl+left").unwrap().flags, FLAG_CTRL | FLAG_FN | FLAG_NUMPAD);
        assert_eq!(Shortcut::parse("f11").unwrap().flags, 0);
        assert!(Shortcut::parse("cmd+tab").unwrap().is_app_switcher());
        assert!(Shortcut::parse("cmd+shift+tab").unwrap().is_app_switcher());
        assert!(!Shortcut::parse("cmd+4").unwrap().is_app_switcher());
        assert!(Shortcut::parse("cmd+").is_err());
        assert!(Shortcut::parse("a+b").is_err());
        assert!(Shortcut::parse("zzz").is_err());
    }

    #[test]
    fn shortcut_event_order() {
        let s = Shortcut::parse("ctrl+left").unwrap();
        assert_eq!(
            shortcut_events(&s, 0),
            vec![
                (59, true, FLAG_CTRL),
                (123, true, FLAG_CTRL | FLAG_FN | FLAG_NUMPAD),
                (123, false, FLAG_CTRL | FLAG_FN | FLAG_NUMPAD),
                (59, false, 0),
            ]
        );
        let s = Shortcut::parse("cmd+shift+4").unwrap();
        assert_eq!(
            shortcut_events(&s, 0),
            vec![
                (56, true, FLAG_SHIFT),
                (55, true, FLAG_SHIFT | FLAG_CMD),
                (21, true, FLAG_SHIFT | FLAG_CMD),
                (21, false, FLAG_SHIFT | FLAG_CMD),
                (55, false, FLAG_SHIFT),
                (56, false, 0),
            ]
        );
        assert_eq!(shortcut_events(&Shortcut::raw(131), 0), vec![(131, true, 0), (131, false, 0)]);
    }

    #[test]
    fn shortcut_events_skip_held_cmd() {
        let s = Shortcut::parse("cmd+shift+tab").unwrap();
        assert_eq!(
            shortcut_events(&s, FLAG_CMD),
            vec![
                (56, true, FLAG_CMD | FLAG_SHIFT),
                (48, true, FLAG_CMD | FLAG_SHIFT),
                (48, false, FLAG_CMD | FLAG_SHIFT),
                (56, false, FLAG_CMD),
            ]
        );
        let s = Shortcut::parse("cmd+tab").unwrap();
        assert_eq!(shortcut_events(&s, FLAG_CMD), vec![(48, true, FLAG_CMD), (48, false, FLAG_CMD)]);
    }

    #[test]
    fn stuck_log_is_throttled() {
        let t0 = Instant::now();
        let first = stuck_log_due(t0);
        assert!(first.is_some());
        assert_eq!(stuck_log_due(t0 + Duration::from_secs(1)), None);
        assert_eq!(stuck_log_due(t0 + Duration::from_secs(2)), None);
        assert_eq!(stuck_log_due(t0 + STUCK_LOG_EVERY), Some(3));
    }

    #[test]
    fn shortcut_events_skip_modifiers_held_by_user() {
        let s = Shortcut::parse("ctrl+shift+left").unwrap();
        let all = FLAG_CTRL | FLAG_SHIFT | FLAG_FN | FLAG_NUMPAD;
        // Shift held on a real keyboard: only Ctrl is pressed and released.
        assert_eq!(
            shortcut_events(&s, FLAG_SHIFT),
            vec![
                (59, true, FLAG_SHIFT | FLAG_CTRL),
                (123, true, all),
                (123, false, all),
                (59, false, FLAG_SHIFT),
            ]
        );
        // Both held: just the key.
        assert_eq!(
            shortcut_events(&s, FLAG_CTRL | FLAG_SHIFT),
            vec![(123, true, all), (123, false, all)]
        );
        // A held modifier that is not part of the shortcut is ignored.
        let s = Shortcut::parse("ctrl+left").unwrap();
        assert_eq!(shortcut_events(&s, FLAG_ALT), shortcut_events(&s, 0));
    }

    #[test]
    fn stuck_flags_ignore_held_modifiers() {
        assert_eq!(stuck_flags(FLAG_CTRL, 0), FLAG_CTRL);
        assert_eq!(stuck_flags(FLAG_CTRL | FLAG_CMD, FLAG_CMD), FLAG_CTRL);
        assert_eq!(stuck_flags(FLAG_CTRL, FLAG_CTRL), 0);
        // Non-modifier flags (Fn, caps lock, ...) are never reported.
        assert_eq!(stuck_flags(FLAG_FN | 0x10000, 0), 0);
    }

    #[test]
    fn stuck_check_waits_for_quiet_and_keeps_one_pending() {
        let t0 = Instant::now();
        let mut check = StuckCheck::default();
        assert_eq!(check.poll(t0), Err(None));

        // Shortcuts 15 ms apart: every new one postpones and replaces the check.
        check.schedule(t0, 124, FLAG_CTRL, 0);
        let t1 = t0 + Duration::from_millis(15);
        check.schedule(t1, 124, FLAG_CTRL | FLAG_FN, FLAG_CMD);
        let early = t0 + STUCK_CHECK_DELAY;
        assert_eq!(check.poll(early), Err(Some(Duration::from_millis(15))));

        // Due only after the last shortcut + delay; reports the latest one once.
        let due = check.poll(t1 + STUCK_CHECK_DELAY).unwrap();
        assert_eq!((due.code, due.flags, due.held), (124, FLAG_CTRL | FLAG_FN, FLAG_CMD));
        assert_eq!(check.poll(t1 + STUCK_CHECK_DELAY * 2), Err(None));
    }

    #[test]
    fn stuck_read_is_skipped_while_a_shortcut_runs() {
        let counters = ShortcutCounters::new();
        // Quiet: the value is returned.
        assert_eq!(counters.read_quiet(|| 7), Some(7));

        // A shortcut already in progress before the read.
        let guard = counters.begin();
        let mut ran = false;
        assert_eq!(
            counters.read_quiet(|| {
                ran = true;
                FLAG_CTRL
            }),
            None
        );
        assert!(!ran);
        drop(guard);
        assert_eq!(counters.read_quiet(|| FLAG_CTRL), Some(FLAG_CTRL));

        // A shortcut that starts (and even finishes) while the flags are read.
        assert_eq!(counters.read_quiet(|| drop(counters.begin())), None);
        let mut guard = None;
        assert_eq!(counters.read_quiet(|| guard = Some(counters.begin())), None);
        // ... and keeps later reads out until it is done.
        assert_eq!(counters.read_quiet(|| 1), None);
        drop(guard);
        assert_eq!(counters.read_quiet(|| 1), Some(1));
    }

    #[test]
    fn real_stuck_modifier_is_still_reported() {
        let counters = ShortcutCounters::new();
        drop(counters.begin()); // the last shortcut has finished
        let system = counters.read_quiet(|| FLAG_CTRL | FLAG_FN).unwrap();
        assert_eq!(stuck_flags(system, 0), FLAG_CTRL);
    }
}
