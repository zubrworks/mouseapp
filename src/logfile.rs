//! Log files: started from Finder, `open` or by the settings app there is no
//! terminal, so stdout and stderr (println, eprintln, log) are redirected to
//! `~/Library/Logs/Mouse/` — `mouse.log` for the daemon, `app.log` for the
//! settings app.

use std::fs::{File, OpenOptions};
use std::io::IsTerminal;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The log is moved to `<file>.1` when it grows past this size.
const MAX_BYTES: u64 = 512 * 1024;

unsafe extern "C" {
    fn dup2(from: i32, to: i32) -> i32;
}

/// Which process writes the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Log {
    Daemon,
    App,
}

impl Log {
    fn file_name(self) -> &'static str {
        match self {
            Log::Daemon => "mouse.log",
            Log::App => "app.log",
        }
    }
}

/// `~/Library/Logs/Mouse/<file>`.
pub(crate) fn path(log: Log) -> Option<PathBuf> {
    Some(crate::paths::logs_dir()?.join(log.file_name()))
}

/// How often the size of the log is checked while the app runs.
const CHECK_EVERY: Duration = Duration::from_secs(60);

/// Path of the single old copy: `mouse.log` -> `mouse.log.1`.
fn old_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
}

/// When the log is bigger than `max` bytes, copies it to `<path>.1`
/// (replacing the older copy) and empties it. The file is opened for
/// appending and shared with stdout/stderr, so it is copied, not renamed;
/// writes continue at the new end. When the copy fails the log is emptied
/// anyway, so it cannot grow without limit. Returns true when emptied.
fn trim(file: &File, path: &Path, max: u64) -> bool {
    if !file.metadata().is_ok_and(|m| m.len() > max) {
        return false;
    }
    let _ = std::fs::copy(path, old_path(path));
    file.set_len(0).is_ok()
}

fn open_log(path: &Path) -> Option<File> {
    std::fs::create_dir_all(path.parent()?).ok()?;
    let file = OpenOptions::new().create(true).append(true).open(path).ok()?;
    trim(&file, path, MAX_BYTES);
    Some(file)
}

/// Redirects stdout and stderr to the log file of `log` (for a process
/// started without a terminal). Returns true when the output now goes to the file.
pub fn redirect(log: Log) -> bool {
    if std::io::stderr().is_terminal() {
        return false;
    }
    let Some(log_path) = path(log) else {
        return false;
    };
    let Some(file) = open_log(&log_path) else {
        return false;
    };
    // SAFETY: both descriptors are valid; dup2 replaces fd 1 and 2 atomically.
    let ok = unsafe { dup2(file.as_raw_fd(), 1) >= 0 && dup2(file.as_raw_fd(), 2) >= 0 };
    if ok {
        // The process may run for weeks: keep the log from growing without limit.
        let _ = std::thread::Builder::new().name("logfile".into()).spawn(move || {
            loop {
                std::thread::sleep(CHECK_EVERY);
                trim(&file, &log_path, MAX_BYTES);
            }
        });
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn daemon_and_app_write_separate_files() {
        if let (Some(daemon), Some(app)) = (path(Log::Daemon), path(Log::App)) {
            assert!(daemon.ends_with("Library/Logs/Mouse/mouse.log"));
            assert!(app.ends_with("Library/Logs/Mouse/app.log"));
        }
    }

    #[test]
    fn big_log_is_cut_and_writing_continues() {
        let path = std::env::temp_dir().join(format!("mouse-logtest-{}.log", std::process::id()));
        let mut f = OpenOptions::new().create(true).append(true).open(&path).unwrap();
        f.write_all(&[b'x'; 100]).unwrap();
        assert!(!trim(&f, &path, 100));
        f.write_all(b"y").unwrap();
        assert!(trim(&f, &path, 100));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        let old = old_path(&path);
        assert_eq!(std::fs::read(&old).unwrap().len(), 101); // the old content is kept
        f.write_all(b"z").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"z"); // no sparse hole
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&old);
    }

    #[test]
    fn big_log_is_cut_even_when_the_copy_fails() {
        let path = std::env::temp_dir().join(format!("mouse-logtest-nocopy-{}.log", std::process::id()));
        let old = old_path(&path);
        std::fs::create_dir_all(&old).unwrap(); // a directory: the copy cannot replace it
        let mut f = OpenOptions::new().create(true).append(true).open(&path).unwrap();
        f.write_all(&[b'x'; 101]).unwrap();
        assert!(trim(&f, &path, 100));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&old);
    }
}
