//! `mouse debug ...`: the client side. Asks the daemon or the menu bar app
//! and prints the answer.

use std::path::Path;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use clap::Subcommand;
use rust_i18n::t;

use super::proto::{AppRequest, DaemonRequest, Gesture, MAX_LOG_MINUTES, parse_level};
use super::server;
use crate::ipc::{self, Reply, Request};

/// How long to wait for the daemon (`debug device` talks to the mice).
const DAEMON_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for the app (it waits up to 5 s for its main thread).
const APP_TIMEOUT: Duration = Duration::from_secs(8);
/// Extra wait after a played gesture before its result is read.
const GESTURE_SETTLE: Duration = Duration::from_millis(300);

#[derive(Subcommand)]
pub enum DebugCommand {
    /// Daemon: profile, settings, mice, desktops, last gesture and action (TOML)
    State,
    /// Daemon: protocol, features and controls of the devices it has open
    Device,
    /// Daemon: play a gesture through the real gesture code; RUNS REAL ACTIONS.
    /// Steps: press | press:0x00C3 | release | 50ms | wait:50 | 0,40 | 0,40*6 | gap:15
    Gesture {
        /// e.g. press 50ms 0,40*6 release
        #[arg(required = true, num_args = 1.., allow_hyphen_values = true)]
        steps: Vec<String>,
    },
    /// Daemon: log at LEVEL (off, error, warn, info, debug, trace) for MINUTES (0 = back now)
    Log { level: String, minutes: u32 },
    /// App: the menu bar menu as a tree (titles, enabled, checked, keys, ids)
    Menu,
    /// App: choose a menu item by id or by titles joined with ">"; RUNS IT
    MenuClick {
        /// e.g. settings or "Profile: Default > Default"
        #[arg(required = true, num_args = 1..)]
        path: Vec<String>,
    },
    /// App: controls and texts of the settings window
    Window,
}

/// Runs one `mouse debug` command.
pub fn run(cmd: DebugCommand) -> Result<()> {
    let dir = crate::paths::support_dir().ok_or_else(|| anyhow!(t!("debug.no_home").to_string()))?;
    let text = match cmd {
        DebugCommand::State => ask_daemon(&dir, DaemonRequest::State)?,
        DebugCommand::Device => ask_daemon(&dir, DaemonRequest::Device)?,
        DebugCommand::Log { level, minutes } => {
            if minutes > MAX_LOG_MINUTES {
                bail!(t!("debug.bad_minutes", max = MAX_LOG_MINUTES));
            }
            let level = parse_level(&level)?;
            ask_daemon(&dir, DaemonRequest::Log { level, minutes })?
        }
        DebugCommand::Gesture { steps } => {
            let g = Gesture::parse(&steps.join(" "))?;
            let wait = Duration::from_millis(g.duration_ms()) + GESTURE_SETTLE;
            print!("{}", ask_daemon(&dir, DaemonRequest::Gesture(g))?);
            std::thread::sleep(wait);
            ask_daemon(&dir, DaemonRequest::State)?
        }
        DebugCommand::Menu => ask_app(&dir, &AppRequest::Menu)?,
        DebugCommand::MenuClick { path } => ask_app(&dir, &AppRequest::MenuClick(path.join(" ")))?,
        DebugCommand::Window => ask_app(&dir, &AppRequest::Window)?,
    };
    print!("{text}");
    Ok(())
}

fn ask_daemon(dir: &Path, req: DaemonRequest) -> Result<String> {
    let socket = ipc::socket_path(dir);
    let reply = ipc::exchange(&socket, &Request::Debug(req).to_string(), DAEMON_TIMEOUT).map_err(|e| {
        anyhow!(t!("debug.daemon_unreachable", path = socket.display(), error = format!("{e:#}")).to_string())
    })?;
    match reply {
        Reply::Text(text) => Ok(text),
        // A daemon without debug access does not know the request.
        Reply::Error(e) if e == t!("ipc.bad_request") => bail!(t!("debug.not_test_build")),
        Reply::Error(e) => Err(anyhow!(e)),
        _ => bail!(t!("ipc.bad_reply")),
    }
}

fn ask_app(dir: &Path, req: &AppRequest) -> Result<String> {
    let socket = server::socket_path(dir);
    let reply = ipc::exchange(&socket, &req.to_string(), APP_TIMEOUT).map_err(|e| {
        anyhow!(t!("debug.app_unreachable", path = socket.display(), error = format!("{e:#}")).to_string())
    })?;
    match reply {
        Reply::Text(text) => Ok(text),
        Reply::Error(e) => Err(anyhow!(e)),
        _ => bail!(t!("ipc.bad_reply")),
    }
}
