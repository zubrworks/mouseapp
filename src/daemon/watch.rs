//! Watching the settings folder for changes.

use std::time::{Duration, Instant};

use anyhow::Result;
use rust_i18n::t;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use super::wake::{Event, Waker};
use crate::bindings::without_unknown_actions;
use crate::config::Config;
use crate::devices::MODELS;
use crate::events;
use crate::profiles::Store;

/// Config changes are applied this long after the last file event.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// Watches the settings folder with `config.toml` and `current.toml`
/// (editors replace files atomically, so watching the files themselves would
/// lose the watch). A change of a settings file wakes the daemon loop with
/// [`Event::Files`], which reports it with [`ConfigWatch::changed`].
pub(super) struct ConfigWatch {
    store: Store,
    changed_at: Option<Instant>,
    _watcher: RecommendedWatcher,
}

impl ConfigWatch {
    pub(super) fn start(store: &Store, wake: Waker) -> Result<Self> {
        let filter = store.clone();
        let mut watcher = notify::recommended_watcher(move |ev: notify::Result<notify::Event>| {
            match ev {
                Ok(ev) if ev.paths.iter().any(|p| filter.is_settings_file(p)) => {
                    wake.send(Event::Files);
                }
                Ok(_) => {}
                Err(e) => log::warn!("config watcher error: {e}"),
            }
        })?;
        watcher.watch(store.dir(), RecursiveMode::Recursive)?;
        Ok(ConfigWatch { store: store.clone(), changed_at: None, _watcher: watcher })
    }

    /// A settings file changed just now.
    pub(super) fn changed(&mut self) {
        self.changed_at = Some(Instant::now());
    }

    /// When [`poll`](Self::poll) will have a new profile to return.
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.changed_at.map(|at| at + DEBOUNCE)
    }

    /// Returns the freshly loaded current settings once the files have
    /// settled after a change. `Some(Err)` means it is invalid and the previous
    /// settings stay.
    pub(super) fn poll(&mut self) -> Option<Result<(String, Config)>> {
        let at = self.changed_at?;
        if at.elapsed() < DEBOUNCE {
            return None;
        }
        self.changed_at = None;
        Some(load_checked(&self.store))
    }
}

/// Loads the current settings (and the name of the profile they came from)
/// and checks that their actions resolve. Entries with an unknown action
/// (e.g. written by a newer version) are left out with a warning; the file
/// keeps them and every other setting is used.
pub(super) fn load_checked(store: &Store) -> Result<(String, Config)> {
    let (name, cfg) = store.load_current()?;
    let Some(model) = MODELS.first() else {
        return Ok((name, cfg));
    };
    let Err(error) = events::validate(model, &cfg) else {
        return Ok((name, cfg));
    };
    let (usable, skipped) = without_unknown_actions(&cfg);
    if skipped.is_empty() || events::validate(model, &usable).is_err() {
        return Err(error);
    }
    log::warn!(
        "{}",
        t!(
            "daemon.actions_skipped",
            path = store.current_path().display(),
            actions = skipped.join(", ")
        )
    );
    Ok((name, usable))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A settings folder with `current.toml` = `text`, removed on drop.
    struct Folder(Store);

    impl Folder {
        fn new(name: &str, text: &str) -> Folder {
            let dir = std::env::temp_dir().join(format!("mouse-watch-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let store = Store::new(dir);
            std::fs::write(store.current_path(), text).unwrap();
            Folder(store)
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.dir());
        }
    }

    #[test]
    fn unknown_action_is_skipped_and_other_settings_kept() {
        let text = "dpi = 2400\n[buttons]\nback = \"from_newer_version\"\nforward = \"copy\"\n[scroll]\nspeed = 140\n";
        let f = Folder::new("unknown", text);
        let (_, cfg) = load_checked(&f.0).unwrap();
        assert_eq!((cfg.dpi, cfg.scroll.speed), (Some(2400), Some(140)));
        assert_eq!(cfg.buttons.keys().collect::<Vec<_>>(), ["forward"]);
        // The file keeps the entry (for the newer version).
        assert_eq!(std::fs::read_to_string(f.0.current_path()).unwrap(), text);
    }

    #[test]
    fn invalid_file_is_still_an_error() {
        let f = Folder::new("invalid", "dpi = = 1\n");
        assert!(load_checked(&f.0).is_err());
    }
}
