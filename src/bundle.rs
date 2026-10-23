//! App bundle layout: names of the bundles and executables, detection of the
//! bundle a program runs from, and the one command that starts the daemon.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The daemon's bundle, inside the app bundle's `Contents/Library/LoginItems`.
const AGENT_BUNDLE: &str = "MouseAgent.app";
/// Bundle identifier of `MouseAgent.app` (set in `scripts/bundle.sh`).
pub const AGENT_ID: &str = "com.zubrworks.mouse.agent";
const AGENT_EXE: &str = "mouse-agent";
const APP_EXE: &str = "mouse-bin";

/// What a bundle executable started without arguments runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// `mouse-agent`: the daemon, without a user interface.
    Agent,
    /// `mouse-bin`: the settings app (window and menu bar icon).
    App,
}

/// Role of the main executable of an app bundle, chosen by its file name.
pub fn role(path: &Path) -> Option<Role> {
    if !is_app_executable(path) {
        return None;
    }
    match path.file_name()?.to_str()? {
        AGENT_EXE => Some(Role::Agent),
        APP_EXE => Some(Role::App),
        _ => None,
    }
}

/// True for the main executable of an app bundle (`*.app/Contents/MacOS/<exe>`).
/// The bundle must point `CFBundleExecutable` straight at this binary (no
/// launcher script): on macOS 26 the menu bar hides status items of processes
/// it cannot map back to their bundle, which happens after a script `exec`s it.
fn is_app_executable(path: &Path) -> bool {
    let Some(macos) = path.parent() else { return false };
    let Some(contents) = macos.parent() else { return false };
    macos.file_name().is_some_and(|n| n == "MacOS")
        && contents.file_name().is_some_and(|n| n == "Contents")
        && contents
            .parent()
            .and_then(|a| a.extension())
            .is_some_and(|e| e.eq_ignore_ascii_case("app"))
}

/// The enclosing `*.app` bundle of the executable, if it runs from one.
fn app_bundle(exe: &Path) -> Option<&Path> {
    exe.ancestors()
        .skip(1)
        .find(|p| p.extension().is_some_and(|e| e == "app"))
}

/// `MouseAgent.app` belonging to the program at `exe`: the one inside the
/// enclosing app bundle (`<bundle>/Contents/Library/LoginItems/MouseAgent.app`),
/// or the enclosing bundle itself when `exe` is already the daemon's. `None`
/// outside a bundle.
pub fn agent_bundle(exe: &Path) -> Option<PathBuf> {
    let app = app_bundle(exe)?;
    if app.file_name().is_some_and(|n| n == AGENT_BUNDLE) {
        return Some(app.to_path_buf());
    }
    Some(app.join("Contents/Library/LoginItems").join(AGENT_BUNDLE))
}

/// How the daemon is started.
#[derive(Debug, PartialEq, Eq)]
pub enum DaemonLaunch {
    /// Open the bundled `MouseAgent.app` through LaunchServices, so the daemon
    /// is a proper process that holds the privacy permissions.
    Bundle(PathBuf),
    /// Run the program itself with `run` (outside a bundle, e.g. `cargo run`).
    Binary(PathBuf),
}

impl DaemonLaunch {
    /// The launch for the program at `exe`. With `must_exist` a bundle that is
    /// not on disk counts as no bundle.
    pub fn for_exe(exe: &Path, must_exist: bool) -> DaemonLaunch {
        match agent_bundle(exe).filter(|b| !must_exist || b.exists()) {
            Some(bundle) => DaemonLaunch::Bundle(bundle),
            None => DaemonLaunch::Binary(exe.to_path_buf()),
        }
    }

    /// The command line (program first). `open_flag` is the `open` option
    /// used for a bundle (`-g` starts without focus change, `-a` is the plain form).
    pub fn argv(&self, open_flag: &str) -> Vec<OsString> {
        match self {
            DaemonLaunch::Bundle(app) => {
                vec!["/usr/bin/open".into(), open_flag.into(), app.into()]
            }
            DaemonLaunch::Binary(exe) => vec![exe.into(), "run".into()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_bundle_executable() {
        assert!(is_app_executable(Path::new("/Applications/Mouse.app/Contents/MacOS/mouse-bin")));
        assert!(is_app_executable(Path::new("dist/Mouse.app/Contents/MacOS/mouse-bin")));
        assert!(!is_app_executable(Path::new("/Users/x/mouse/target/release/mouse")));
        assert!(!is_app_executable(Path::new("/tmp/Contents/MacOS/mouse")));
        assert!(!is_app_executable(Path::new("mouse")));
    }

    #[test]
    fn role_follows_executable_name() {
        let app = "/Applications/Mouse.app/Contents/MacOS/mouse-bin";
        let agent = "/Applications/Mouse.app/Contents/Library/LoginItems/MouseAgent.app/Contents/MacOS/mouse-agent";
        assert_eq!(role(Path::new(app)), Some(Role::App));
        assert_eq!(role(Path::new(agent)), Some(Role::Agent));
        // Other names, or the right name outside a bundle, run nothing.
        assert_eq!(role(Path::new("/Applications/Mouse.app/Contents/MacOS/Mouse")), None);
        assert_eq!(role(Path::new("/Users/x/mouse/target/release/mouse-agent")), None);
        assert_eq!(role(Path::new("mouse-bin")), None);
    }

    #[test]
    fn agent_bundle_is_found_from_app_or_agent() {
        let agent = "/A/Mouse.app/Contents/Library/LoginItems/MouseAgent.app";
        let from_app = agent_bundle(Path::new("/A/Mouse.app/Contents/MacOS/mouse-bin"));
        assert_eq!(from_app, Some(PathBuf::from(agent)));
        let from_agent = agent_bundle(Path::new(&format!("{agent}/Contents/MacOS/mouse-agent")));
        assert_eq!(from_agent, Some(PathBuf::from(agent)));
        assert_eq!(agent_bundle(Path::new("/tmp/target/debug/mouse")), None);
    }

    #[test]
    fn daemon_launch_uses_bundle_or_binary() {
        let bundled = DaemonLaunch::for_exe(Path::new("/A/Mouse.app/Contents/MacOS/mouse-bin"), false);
        assert_eq!(
            bundled.argv("-g"),
            ["/usr/bin/open", "-g", "/A/Mouse.app/Contents/Library/LoginItems/MouseAgent.app"]
        );
        // A bundle that is not on disk falls back to the binary.
        let missing = DaemonLaunch::for_exe(Path::new("/A/Mouse.app/Contents/MacOS/mouse-bin"), true);
        assert_eq!(missing.argv("-g"), ["/A/Mouse.app/Contents/MacOS/mouse-bin", "run"]);
    }
}
