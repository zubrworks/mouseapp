//! Requests of `mouse debug`: to the daemon (one line on `agent.sock`, see
//! [`crate::ipc`]) and to the menu bar app (one line on `app.sock`), and the
//! gesture script the daemon plays into its gesture code.

use std::fmt;
use std::str::FromStr;

use anyhow::{Result, bail};
use log::LevelFilter;
use rust_i18n::t;

/// Longest gesture script in time.
pub const MAX_GESTURE_MS: u64 = 10_000;
/// Most movement reports in one script.
const MAX_MOVES: u32 = 1000;
/// Pause after each movement report unless the script sets `gap:<ms>`
/// (a mouse reports movement about every 8-15 ms).
const DEFAULT_GAP_MS: u32 = 15;
/// Longest temporary change of the daemon's log level.
pub const MAX_LOG_MINUTES: u32 = 240;

/// One step of a gesture script.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Press a button (`None`: the mouse's first gesture button).
    Press(Option<u16>),
    /// Release the button pressed last.
    Release,
    /// Wait this many milliseconds.
    Wait(u32),
    /// `times` movement reports of `(dx, dy)`, each followed by the gap.
    Move { dx: i16, dy: i16, times: u32 },
}

/// An event played into the mouse thread, as if the mouse had sent it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inject {
    Press(u16),
    Release(u16),
    Move(i16, i16),
}

/// A gesture script, e.g. `press 50ms 0,40*6 release`: steps separated by
/// spaces. `press` / `press:0x00C3`, `release`, `<ms>ms` / `wait:<ms>`,
/// `<dx>,<dy>` / `<dx>,<dy>*<n>` / `move:<dx>,<dy>`, `gap:<ms>` (pause after
/// every movement report, default 15 ms). A button still held at the end is
/// released.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gesture {
    pub gap: u32,
    pub steps: Vec<Step>,
}

impl Gesture {
    pub fn parse(text: &str) -> Result<Gesture> {
        let mut gap = DEFAULT_GAP_MS;
        let mut steps = Vec::new();
        let mut held = false;
        for token in text.split_whitespace() {
            let bad = || anyhow::anyhow!(t!("debug.bad_step", step = token).to_string());
            let lower = token.to_ascii_lowercase();
            let step = if let Some(ms) = lower.strip_prefix("gap:") {
                gap = ms.parse().map_err(|_| bad())?;
                continue;
            } else if lower == "press" {
                Step::Press(None)
            } else if let Some(cid) = lower.strip_prefix("press:") {
                Step::Press(Some(parse_cid(cid).ok_or_else(bad)?))
            } else if lower == "release" {
                Step::Release
            } else if let Some(ms) = lower.strip_prefix("wait:").or_else(|| lower.strip_suffix("ms")) {
                Step::Wait(ms.parse().map_err(|_| bad())?)
            } else {
                parse_move(lower.strip_prefix("move:").unwrap_or(&lower)).ok_or_else(bad)?
            };
            match step {
                Step::Press(_) => held = true,
                Step::Release => held = false,
                _ => {}
            }
            steps.push(step);
        }
        if steps.is_empty() {
            bail!(t!("debug.empty_gesture"));
        }
        if held {
            steps.push(Step::Release);
        }
        let g = Gesture { gap, steps };
        let moves: u64 = g.steps.iter().map(|s| match s {
            Step::Move { times, .. } => u64::from(*times),
            _ => 0,
        }).sum();
        if moves > u64::from(MAX_MOVES) {
            bail!(t!("debug.too_many_moves", count = moves, max = MAX_MOVES));
        }
        let ms = g.duration_ms();
        if ms > MAX_GESTURE_MS {
            bail!(t!("debug.too_long", ms = ms, max = MAX_GESTURE_MS));
        }
        Ok(g)
    }

    /// How long playing takes.
    pub fn duration_ms(&self) -> u64 {
        self.steps
            .iter()
            .map(|s| match s {
                Step::Wait(ms) => u64::from(*ms),
                Step::Move { times, .. } => u64::from(*times) * u64::from(self.gap),
                _ => 0,
            })
            .sum()
    }

    /// The events with their time from the start (ms); a press without a
    /// button presses `default_cid`, a release lets go of the last pressed.
    pub fn timeline(&self, default_cid: u16) -> Vec<(u64, Inject)> {
        let mut out = Vec::new();
        let mut at = 0;
        let mut pressed = default_cid;
        for step in &self.steps {
            match *step {
                Step::Wait(ms) => at += u64::from(ms),
                Step::Press(cid) => {
                    pressed = cid.unwrap_or(default_cid);
                    out.push((at, Inject::Press(pressed)));
                }
                Step::Release => out.push((at, Inject::Release(pressed))),
                Step::Move { dx, dy, times } => {
                    for _ in 0..times {
                        out.push((at, Inject::Move(dx, dy)));
                        at += u64::from(self.gap);
                    }
                }
            }
        }
        out
    }
}

impl fmt::Display for Gesture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "gap:{}", self.gap)?;
        for step in &self.steps {
            match *step {
                Step::Press(None) => write!(f, " press")?,
                Step::Press(Some(cid)) => write!(f, " press:0x{cid:04X}")?,
                Step::Release => write!(f, " release")?,
                Step::Wait(ms) => write!(f, " {ms}ms")?,
                Step::Move { dx, dy, times: 1 } => write!(f, " {dx},{dy}")?,
                Step::Move { dx, dy, times } => write!(f, " {dx},{dy}*{times}")?,
            }
        }
        Ok(())
    }
}

/// `0x00C3` or `195`.
fn parse_cid(s: &str) -> Option<u16> {
    match s.strip_prefix("0x") {
        Some(hex) => u16::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

/// `<dx>,<dy>` or `<dx>,<dy>*<n>` (n >= 1).
fn parse_move(s: &str) -> Option<Step> {
    let (xy, times) = match s.split_once('*') {
        Some((xy, n)) => (xy, n.parse().ok().filter(|&n| n >= 1)?),
        None => (s, 1),
    };
    let (dx, dy) = xy.split_once(',')?;
    Some(Step::Move { dx: dx.parse().ok()?, dy: dy.parse().ok()?, times })
}

/// Parses a log level name (`off`, `error`, `warn`, `info`, `debug`, `trace`).
pub fn parse_level(s: &str) -> Result<LevelFilter> {
    LevelFilter::from_str(s).map_err(|_| anyhow::anyhow!(t!("debug.bad_level", value = s).to_string()))
}

/// A debug request to the daemon (after the word `debug`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DaemonRequest {
    State,
    Device,
    Gesture(Gesture),
    /// Log at `level` for `minutes` (0: back to the normal level now).
    Log { level: LevelFilter, minutes: u32 },
}

impl DaemonRequest {
    /// Parses the rest of a `debug ...` line; `None` if malformed.
    pub fn parse(rest: &str) -> Option<DaemonRequest> {
        let rest = rest.trim();
        let (word, args) = rest.split_once(' ').map_or((rest, ""), |(w, a)| (w, a.trim()));
        match (word, args) {
            ("state", "") => Some(DaemonRequest::State),
            ("device", "") => Some(DaemonRequest::Device),
            ("gesture", g) => Gesture::parse(g).ok().map(DaemonRequest::Gesture),
            ("log", a) => {
                let (level, minutes) = a.split_once(' ')?;
                let minutes = minutes.trim().parse().ok().filter(|&m| m <= MAX_LOG_MINUTES)?;
                Some(DaemonRequest::Log { level: parse_level(level).ok()?, minutes })
            }
            _ => None,
        }
    }
}

impl fmt::Display for DaemonRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DaemonRequest::State => write!(f, "state"),
            DaemonRequest::Device => write!(f, "device"),
            DaemonRequest::Gesture(g) => write!(f, "gesture {g}"),
            DaemonRequest::Log { level, minutes } => {
                write!(f, "log {} {minutes}", level.as_str().to_ascii_lowercase())
            }
        }
    }
}

/// A request to the menu bar app (one line on `app.sock`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppRequest {
    Menu,
    /// Click the menu item with this id or path of titles (see `menu::find`).
    MenuClick(String),
    Window,
}

impl AppRequest {
    /// Parses one request line; `None` if unknown or malformed.
    pub fn parse(line: &str) -> Option<AppRequest> {
        let line = line.trim();
        let (word, rest) = line.split_once(' ').map_or((line, ""), |(w, r)| (w, r.trim()));
        match (word, rest) {
            ("menu", "") => Some(AppRequest::Menu),
            ("window", "") => Some(AppRequest::Window),
            ("menu-click", path) if !path.is_empty() => Some(AppRequest::MenuClick(path.into())),
            _ => None,
        }
    }
}

impl fmt::Display for AppRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppRequest::Menu => write!(f, "menu"),
            AppRequest::MenuClick(path) => write!(f, "menu-click {path}"),
            AppRequest::Window => write!(f, "window"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_gesture_script() {
        let g = Gesture::parse("press 50ms 0,40*6 -60,5 wait:20 release").unwrap();
        assert_eq!(g.gap, DEFAULT_GAP_MS);
        assert_eq!(
            g.steps,
            [
                Step::Press(None),
                Step::Wait(50),
                Step::Move { dx: 0, dy: 40, times: 6 },
                Step::Move { dx: -60, dy: 5, times: 1 },
                Step::Wait(20),
                Step::Release,
            ]
        );
        assert_eq!(g.duration_ms(), 50 + 7 * 15 + 20);
        let g = Gesture::parse("GAP:10 Press:0x00C3 move:1,2 release").unwrap();
        assert_eq!(g.gap, 10);
        assert_eq!(g.steps[0], Step::Press(Some(0x00C3)));
        assert_eq!(Gesture::parse("press:195").unwrap().steps[0], Step::Press(Some(0xC3)));
    }

    #[test]
    fn a_held_button_is_released_at_the_end() {
        let g = Gesture::parse("press 0,40").unwrap();
        assert_eq!(g.steps.last(), Some(&Step::Release));
        let g = Gesture::parse("press release 10ms").unwrap();
        assert_eq!(g.steps.len(), 3);
    }

    #[test]
    fn rejects_bad_scripts() {
        for bad in ["", "jump", "press:xyz", "1,2,3", "0,40*0", "0,40*x", "70000,0", "5s", "gap:x"] {
            assert!(Gesture::parse(bad).is_err(), "{bad}");
        }
        assert!(Gesture::parse("press 10001ms").is_err());
        assert!(Gesture::parse("press 10000ms").is_ok());
        assert!(Gesture::parse("gap:0 1,1*1001").is_err());
    }

    #[test]
    fn gesture_survives_display_and_parse() {
        for text in ["press 50ms 0,40*6 release", "gap:8 press:0x0053 -5,-5 release", "1,1"] {
            let g = Gesture::parse(text).unwrap();
            assert_eq!(Gesture::parse(&g.to_string()).unwrap(), g, "{text}");
        }
        assert_eq!(
            Gesture::parse("press 50ms 0,40*2 release").unwrap().to_string(),
            "gap:15 press 50ms 0,40*2 release"
        );
    }

    #[test]
    fn timeline_spaces_moves_by_the_gap() {
        let g = Gesture::parse("gap:10 press 40ms 1,2*3 5ms release press:0x0053").unwrap();
        assert_eq!(
            g.timeline(0x00C3),
            [
                (0, Inject::Press(0x00C3)),
                (40, Inject::Move(1, 2)),
                (50, Inject::Move(1, 2)),
                (60, Inject::Move(1, 2)),
                (75, Inject::Release(0x00C3)),
                (75, Inject::Press(0x0053)),
                (75, Inject::Release(0x0053)),
            ]
        );
    }

    #[test]
    fn parses_daemon_requests() {
        assert_eq!(DaemonRequest::parse("state"), Some(DaemonRequest::State));
        assert_eq!(DaemonRequest::parse(" device "), Some(DaemonRequest::Device));
        assert_eq!(
            DaemonRequest::parse("log DEBUG 10"),
            Some(DaemonRequest::Log { level: LevelFilter::Debug, minutes: 10 })
        );
        let requests = [
            DaemonRequest::State,
            DaemonRequest::Device,
            DaemonRequest::Log { level: LevelFilter::Trace, minutes: 0 },
            DaemonRequest::Gesture(Gesture::parse("press 50ms 0,40*6 release").unwrap()),
        ];
        for req in requests {
            assert_eq!(DaemonRequest::parse(&req.to_string()), Some(req));
        }
        for bad in ["", "state now", "log", "log debug", "log loud 5", "log info 241", "gesture", "gesture jump", "menu"] {
            assert_eq!(DaemonRequest::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn parses_app_requests() {
        assert_eq!(AppRequest::parse("menu\n"), Some(AppRequest::Menu));
        assert_eq!(AppRequest::parse("window"), Some(AppRequest::Window));
        assert_eq!(
            AppRequest::parse("menu-click Profile: Mój > Default"),
            Some(AppRequest::MenuClick("Profile: Mój > Default".into()))
        );
        for req in [AppRequest::Menu, AppRequest::Window, AppRequest::MenuClick("quit".into())] {
            assert_eq!(AppRequest::parse(&req.to_string()), Some(req));
        }
        for bad in ["", "menu-click", "menu-click  ", "menu x", "state"] {
            assert_eq!(AppRequest::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn parses_levels() {
        assert_eq!(parse_level("warn").unwrap(), LevelFilter::Warn);
        assert_eq!(parse_level("OFF").unwrap(), LevelFilter::Off);
        assert!(parse_level("loud").is_err());
    }
}
