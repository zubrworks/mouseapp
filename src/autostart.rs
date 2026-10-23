//! Launch at login of the daemon (`MouseAgent.app`) via a LaunchAgent
//! (`~/Library/LaunchAgents/<label>.plist`). Enabling writes the plist,
//! disabling removes it. launchd starts the daemon at the next login; nothing
//! is started right now.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rust_i18n::t;

use crate::bundle::DaemonLaunch;

/// LaunchAgent label, same as the identifier of `MouseAgent.app`.
const LABEL: &str = crate::bundle::AGENT_ID;

/// Path of the plist in the user's LaunchAgents folder.
fn agent_path() -> Result<PathBuf> {
    let dir = crate::paths::launch_agents_dir().context("HOME is not set")?;
    Ok(dir.join(format!("{LABEL}.plist")))
}

/// Writes the LaunchAgent plist for the program at `exe` to `path`.
fn install(path: &Path, exe: &Path) -> Result<()> {
    crate::fsutil::write_atomic(path, plist_contents(LABEL, exe))
        .with_context(|| format!("cannot write {}", path.display()))
}

/// Whether the LaunchAgent is installed.
pub fn is_enabled() -> bool {
    agent_path().is_ok_and(|p| p.exists())
}

/// Installs (`true`) or removes (`false`) the LaunchAgent.
pub fn set_enabled(enabled: bool) -> Result<()> {
    let path = agent_path()?;
    if enabled {
        let exe = std::env::current_exe().context("cannot find the program path")?;
        install(&path, &exe)
    } else {
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("cannot remove {}", path.display())),
        }
    }
}

/// Installs or removes the LaunchAgent; a failure is logged (the state is
/// then unchanged, so the menu and window show the real one).
pub fn toggle(enabled: bool) {
    if let Err(e) = set_enabled(enabled) {
        log::warn!("{}", t!("autostart.failed", error = format!("{e:#}")));
    }
}

/// Builds the LaunchAgent plist. From an app bundle it runs
/// `/usr/bin/open -a <MouseAgent.app>` (so the daemon gets a proper
/// LaunchServices process); outside a bundle it starts `<exe> run` directly.
/// No KeepAlive, so a stopped daemon stays stopped until the next login.
fn plist_contents(label: &str, exe: &Path) -> String {
    let args = DaemonLaunch::for_exe(exe, false)
        .argv("-a")
        .iter()
        .map(|a| format!("        <string>{}</string>", xml_escape(&a.to_string_lossy())))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{args}
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#,
        label = xml_escape(label),
    )
}

/// Escapes the characters that are special in XML text.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_script_agent_id_matches_label() {
        let script = include_str!("../scripts/bundle.sh");
        let line = script
            .lines()
            .find(|l| l.starts_with("AGENT_ID="))
            .expect("AGENT_ID line in scripts/bundle.sh");
        assert_eq!(line.trim_start_matches("AGENT_ID=").trim_matches('"'), LABEL);
    }

    #[test]
    fn plist_starts_run_at_login() {
        let p = plist_contents(
            LABEL,
            Path::new("/Applications/Mouse.app/Contents/MacOS/mouse-bin"),
        );
        assert!(p.contains("<key>Label</key>\n    <string>com.zubrworks.mouse.agent</string>"));
        assert!(p.contains("<string>/usr/bin/open</string>\n        <string>-a</string>\n        <string>/Applications/Mouse.app/Contents/Library/LoginItems/MouseAgent.app</string>"));
        assert!(!p.contains("mouse-bin"));
        assert!(p.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(!p.contains("KeepAlive"));
    }

    #[test]
    fn plist_outside_bundle_runs_binary() {
        let p = plist_contents(LABEL, Path::new("/tmp/target/debug/mouse"));
        assert!(p.contains("<string>/tmp/target/debug/mouse</string>\n        <string>run</string>"));
        assert!(!p.contains("/usr/bin/open"));
    }

    #[test]
    fn plist_escapes_path() {
        let p = plist_contents("x", Path::new("/Users/A & B/<mouse>"));
        assert!(p.contains("/Users/A &amp; B/&lt;mouse&gt;"));
        assert!(!p.contains("A & B"));
    }

    #[test]
    fn agent_path_is_in_launch_agents() {
        if let Ok(path) = agent_path() {
            assert!(path.ends_with(format!("Library/LaunchAgents/{LABEL}.plist")));
        }
    }
}
