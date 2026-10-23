//! File helpers shared by the settings and autostart code.

use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

/// Writes `contents` to `path` atomically: the text goes to a hidden
/// temporary file in the same folder, is flushed to disk and then renamed
/// over `path`, so a reader never sees a half-written file. Creates the
/// folder if needed. The temporary file never ends in `.toml` (the settings
/// watcher ignores it) and is removed if anything fails.
pub fn write_atomic(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("path has no file name"))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(
        ".{}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = dir.join(tmp_name);
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(contents.as_ref())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Reads the text file at `path`; if it does not exist returns the built-in
/// profile template (the example file with its comments).
pub fn read_or_template(path: &Path) -> io::Result<String> {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(crate::profiles::builtin_text()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mouse-fsutil-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn write_atomic_replaces_the_file_and_leaves_no_temporary_file() {
        let dir = temp_dir("write");
        let path = dir.join("sub").join("a.toml");
        write_atomic(&path, "one").unwrap();
        write_atomic(&path, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["a.toml"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_atomic_failure_leaves_no_temporary_file() {
        let dir = temp_dir("fail");
        std::fs::create_dir_all(dir.join("target")).unwrap();
        // Renaming a file over a non-empty directory fails.
        std::fs::write(dir.join("target").join("x"), "x").unwrap();
        assert!(write_atomic(&dir.join("target"), "data").is_err());
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["target"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_or_template_uses_the_builtin_text_when_missing() {
        let dir = temp_dir("read");
        let path = dir.join("missing.toml");
        assert_eq!(
            read_or_template(&path).unwrap(),
            crate::profiles::builtin_text()
        );
        write_atomic(&path, "dpi = 1\n").unwrap();
        assert_eq!(read_or_template(&path).unwrap(), "dpi = 1\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
