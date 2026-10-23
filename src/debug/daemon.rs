//! The daemon's answers to debug requests (`debug state`, `device`,
//! `gesture`, `log`), on the socket thread.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime};

use anyhow::{Result, bail};
use log::LevelFilter;
use rust_i18n::t;
use serde::Serialize;

use super::probe::{self, ActionRecord, GestureRecord, Player};
use super::proto::{DaemonRequest, Gesture};
use crate::config::Config;
use crate::devices;
use crate::hidpp::device::{Device, write_info};
use crate::hidpp::features::controls;
use crate::hub::{Hub, MouseInfo};
use crate::ipc::Reply;
use crate::permissions::{self, Permission};
use crate::scroll;
use crate::spaces;

/// Answers one debug request.
pub fn answer(req: DaemonRequest, hub: &Hub, cfg: &Config) -> Reply {
    let result = match req {
        DaemonRequest::State => state(hub, cfg),
        DaemonRequest::Device => Ok(device()),
        DaemonRequest::Gesture(g) => gesture(g),
        DaemonRequest::Log { level, minutes } => Ok(set_log(level, minutes)),
    };
    match result {
        Ok(text) => Reply::Text(text),
        Err(e) => Reply::Error(format!("{e:#}")),
    }
}

/// `debug state`, as TOML.
#[derive(Serialize)]
struct StateReport {
    pid: u32,
    version: &'static str,
    profile: String,
    log_level: String,
    /// Seconds until a temporary log level ends.
    log_left_s: Option<u64>,
    missing: Vec<Permission>,
    scroll_problem: Option<String>,
    config: ConfigView,
    desktops: Option<Desktops>,
    last_gesture: Option<GestureView>,
    last_action: Option<ActionView>,
    mice: Vec<MouseInfo>,
}

/// The parts of the active settings that decide how the mouse behaves.
#[derive(Serialize)]
struct ConfigView {
    dpi: Option<u16>,
    smartshift_threshold: Option<u8>,
    gesture_threshold: Option<u16>,
    scroll_direction: Option<String>,
    scroll_speed: Option<u16>,
    app_overrides: usize,
}

/// The Spaces of the display with the active one (`current` from 1).
#[derive(Serialize)]
struct Desktops {
    count: usize,
    current: usize,
}

#[derive(Serialize)]
struct GestureView {
    button: String,
    result: &'static str,
    moves: u32,
    x: i32,
    y: i32,
    skipped: u32,
    took_ms: u64,
    at: String,
    ago_s: u64,
}

#[derive(Serialize)]
struct ActionView {
    action: String,
    ok: bool,
    took_ms: u64,
    at: String,
    ago_s: u64,
}

/// Time of day (UTC, as in the log) and seconds since `at`.
fn when(at: SystemTime) -> (String, u64) {
    let ago = SystemTime::now().duration_since(at).unwrap_or_default();
    (spaces::utc_time(ago), ago.as_secs())
}

fn gesture_view(g: GestureRecord) -> GestureView {
    let (at, ago_s) = when(g.at);
    GestureView {
        button: format!("0x{:04X}", g.button),
        result: g.result,
        moves: g.moves,
        x: g.x,
        y: g.y,
        skipped: g.skipped,
        took_ms: g.took_ms,
        at,
        ago_s,
    }
}

fn action_view(a: ActionRecord) -> ActionView {
    let (at, ago_s) = when(a.at);
    ActionView { action: a.action, ok: a.ok, took_ms: a.took_ms, at, ago_s }
}

fn level_name(level: LevelFilter) -> String {
    level.as_str().to_ascii_lowercase()
}

/// What `debug state` asks the system.
#[derive(Default)]
struct System {
    missing: Vec<Permission>,
    scroll_problem: Option<String>,
    desktops: Option<Desktops>,
}

impl System {
    fn read() -> System {
        System {
            missing: permissions::missing(),
            scroll_problem: scroll::problem(),
            desktops: spaces::query_layout()
                .map(|l| Desktops { count: l.count, current: l.index + 1 }),
        }
    }
}

fn state(hub: &Hub, cfg: &Config) -> Result<String> {
    render_state(hub, cfg, System::read())
}

fn render_state(hub: &Hub, cfg: &Config, sys: System) -> Result<String> {
    let (gesture, action) = probe::last();
    let (level, until) = probe::log_level();
    let report = StateReport {
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION"),
        profile: hub.profile(),
        log_level: level_name(level),
        log_left_s: until
            .map(|u| u.duration_since(SystemTime::now()).unwrap_or_default().as_secs()),
        missing: sys.missing,
        scroll_problem: sys.scroll_problem,
        config: ConfigView {
            dpi: cfg.dpi,
            smartshift_threshold: cfg.smartshift.threshold,
            gesture_threshold: cfg.gestures.thumb.threshold,
            scroll_direction: cfg.scroll.direction.clone(),
            scroll_speed: cfg.scroll.speed,
            app_overrides: cfg.apps.len(),
        },
        desktops: sys.desktops,
        last_gesture: gesture.map(gesture_view),
        last_action: action.map(action_view),
        mice: hub.mice(),
    };
    Ok(toml::to_string(&report)?)
}

/// `debug device`: every device the daemon has open, read through its
/// own handle (as `mouse info`, plus the list of controls).
fn device() -> String {
    let held = probe::devices();
    if held.is_empty() {
        return format!("{}\n", t!("debug.no_devices"));
    }
    let mut out = Vec::new();
    for (key, dev, supported) in held {
        let _ = writeln!(
            out,
            "== {} key={key} pid=0x{:04X} connection={} index={} supported={supported} path={}",
            dev.name,
            dev.pid,
            dev.connection.label(),
            dev.index,
            dev.path.to_string_lossy()
        );
        if let Err(e) = write_info(&dev, &mut out) {
            let _ = writeln!(out, "error: {e:#}");
        }
        if let Err(e) = write_controls(&dev, &mut out) {
            let _ = writeln!(out, "error: {e:#}");
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Names of the control flags (0x1B04 getCidInfo) that are set.
fn flag_names(flags: u8, more: u8) -> String {
    const FLAGS: [(u8, &str); 8] = [
        (0x01, "mouse"),
        (0x02, "fkey"),
        (0x04, "hotkey"),
        (0x08, "fn_toggle"),
        (0x10, "reprogrammable"),
        (0x20, "divertable"),
        (0x40, "persistent"),
        (0x80, "virtual"),
    ];
    const MORE: [(u8, &str); 4] =
        [(0x01, "raw_xy"), (0x02, "force_raw_xy"), (0x04, "analytics"), (0x08, "raw_wheel")];
    let set = |table: &[(u8, &'static str)], v: u8| {
        table.iter().filter(move |(bit, _)| v & bit != 0).map(|(_, n)| *n).collect::<Vec<_>>()
    };
    let mut names = set(&FLAGS, flags);
    names.extend(set(&MORE, more));
    names.join(",")
}

/// The device's controls (0x1B04): CID, task, flags, and the label when
/// the model knows the button.
fn write_controls(dev: &Device, out: &mut dyn Write) -> Result<()> {
    let Some(idx) = dev.feature(controls::ID)? else {
        return Ok(());
    };
    let count = dev.call(idx, 0, &[])?.first().copied().unwrap_or(0);
    writeln!(out, "controls (0x1B04): {count}")?;
    let model = devices::identify(dev.pid, &dev.name);
    for i in 0..count {
        let p = dev.call(idx, 1, &[i])?;
        let [c0, c1, t0, t1, flags, pos, group, gmask, more, ..] = p[..] else {
            continue;
        };
        let cid = u16::from_be_bytes([c0, c1]);
        let label = model
            .and_then(|m| m.buttons.iter().find(|b| b.cid == cid))
            .map_or(String::new(), |b| format!(" \"{}\"", b.label()));
        writeln!(
            out,
            "  CID 0x{cid:04X} TID 0x{:04X} flags 0x{flags:02X} more 0x{more:02X} [{}] pos {pos} group {group} gmask 0x{gmask:02X}{label}",
            u16::from_be_bytes([t0, t1]),
            flag_names(flags, more)
        )?;
    }
    Ok(())
}

/// A gesture is being played (one at a time).
static PLAYING: AtomicBool = AtomicBool::new(false);

/// Clears [`PLAYING`] when the playing thread ends.
struct Playing;

impl Drop for Playing {
    fn drop(&mut self) {
        PLAYING.store(false, Ordering::SeqCst);
    }
}

/// `debug gesture`: plays the script into the first mouse with a gesture
/// button on a new thread and answers at once.
fn gesture(g: Gesture) -> Result<String> {
    let Some(player) = Player::first() else {
        bail!(t!("debug.no_gesture_mouse"));
    };
    if PLAYING.swap(true, Ordering::SeqCst) {
        bail!(t!("debug.gesture_busy"));
    }
    let playing = Playing;
    let timeline = g.timeline(player.default_cid);
    let text = t!("debug.gesture_started", name = player.name, ms = g.duration_ms()).to_string();
    log::info!("debug gesture: {g}");
    std::thread::Builder::new().name("debug-gesture".into()).spawn(move || {
        let _playing = playing;
        play(&player, &timeline);
    })?;
    Ok(format!("{text}\n"))
}

fn play(player: &Player, timeline: &[(u64, super::proto::Inject)]) {
    let start = Instant::now();
    for &(at, ev) in timeline {
        let due = start + std::time::Duration::from_millis(at);
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
        if !player.send(ev) {
            log::warn!("debug gesture: the mouse session ended while playing");
            return;
        }
    }
}

/// `debug log`.
fn set_log(level: LevelFilter, minutes: u32) -> String {
    let now = probe::set_log_level(level, minutes);
    let text = if minutes == 0 {
        t!("debug.log_reset", level = level_name(now))
    } else {
        let file = crate::logfile::path(crate::logfile::Log::Daemon)
            .map_or("?".to_string(), |p| p.display().to_string());
        t!("debug.log_set", level = level_name(now), minutes = minutes, file = file)
    };
    log::info!("debug log: level {} for {minutes} min", level_name(now));
    format!("{text}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_control_flags() {
        assert_eq!(flag_names(0x30, 0x01), "reprogrammable,divertable,raw_xy");
        assert_eq!(flag_names(0, 0), "");
    }

    #[test]
    fn state_report_is_toml() {
        let hub = Hub::new(|| {});
        hub.set_profile("Mój");
        hub.publish(MouseInfo { key: "unit:aa".into(), name: "MX".into(), ..MouseInfo::default() });
        let cfg = Config { dpi: Some(1600), ..Config::default() };
        let sys = System { desktops: Some(Desktops { count: 3, current: 2 }), ..System::default() };
        let text = render_state(&hub, &cfg, sys).unwrap();
        let v: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(v["profile"].as_str(), Some("Mój"));
        assert_eq!(v["config"]["dpi"].as_integer(), Some(1600));
        assert_eq!(v["mice"][0]["name"].as_str(), Some("MX"));
        assert_eq!(v["desktops"]["current"].as_integer(), Some(2));
        assert!(v.contains_key("log_level"), "{text}");
    }
}
