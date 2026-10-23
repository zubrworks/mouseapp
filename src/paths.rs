//! Locations of the app's files under the user's home folder.

use std::path::PathBuf;

/// The user's home folder (`None` when `HOME` is not set).
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// `~/Library/Application Support/Mouse`: settings and the control socket.
pub fn support_dir() -> Option<PathBuf> {
    Some(home()?.join("Library/Application Support/Mouse"))
}

/// `~/Library/Logs/Mouse`.
pub fn logs_dir() -> Option<PathBuf> {
    Some(home()?.join("Library/Logs/Mouse"))
}

/// `~/Library/LaunchAgents`.
pub fn launch_agents_dir() -> Option<PathBuf> {
    Some(home()?.join("Library/LaunchAgents"))
}
