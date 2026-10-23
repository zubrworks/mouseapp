//! The menu bar app's debug socket `app.sock` (next to `agent.sock`, owner
//! only): a thread takes one request per connection and hands it to the
//! main thread, which answers on its next pass (see `tray::Ui::step`).

use std::fs::{self, Permissions};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use core_foundation::runloop::{CFRunLoopGetMain, CFRunLoopWakeUp};
use rust_i18n::t;

use super::proto::AppRequest;
use crate::ipc::Reply;

const SOCKET_FILE: &str = "app.sock";
/// Only the owner may talk to the app.
const SOCKET_MODE: u32 = 0o600;
/// How long the server waits for a client's request or reading of the reply.
const IO_TIMEOUT: Duration = Duration::from_secs(1);
/// How long a request waits for the main thread (busy while a menu is open
/// or a dialog runs).
const MAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// Pause after a failed `accept`, so the thread does not spin.
const ACCEPT_ERROR_PAUSE: Duration = Duration::from_millis(100);
/// Longest request line accepted.
const MAX_REQUEST: u64 = 4096;

/// `app.sock` in the settings folder `dir`.
pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join(SOCKET_FILE)
}

/// A request waiting for the main thread's answer.
pub struct Pending {
    pub request: AppRequest,
    reply: SyncSender<Reply>,
}

impl Pending {
    pub fn answer(self, result: Result<String>) {
        let reply = match result {
            Ok(text) => Reply::Text(text),
            Err(e) => Reply::Error(format!("{e:#}")),
        };
        // The client may have given up already.
        let _ = self.reply.send(reply);
    }
}

/// The app's end of `app.sock`; dropping it removes the socket file (the
/// thread ends with the process).
pub struct AppServer {
    requests: Receiver<Pending>,
    path: PathBuf,
}

impl AppServer {
    /// Listens on `app.sock` in `dir`, replacing a file left by an app that died.
    pub fn start(dir: &Path) -> Result<AppServer> {
        fs::create_dir_all(dir)?;
        let path = socket_path(dir);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, Permissions::from_mode(SOCKET_MODE))?;
        let (tx, rx) = mpsc::channel();
        thread::Builder::new().name("app-debug".into()).spawn(move || serve(&listener, &tx))?;
        log::info!("debug access on {}", path.display());
        Ok(AppServer { requests: rx, path })
    }

    /// The next request waiting for the main thread.
    pub fn next(&self) -> Option<Pending> {
        self.requests.try_recv().ok()
    }
}

impl Drop for AppServer {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_file(&self.path) {
            log::warn!("cannot remove {}: {e}", self.path.display());
        }
    }
}

fn serve(listener: &UnixListener, main: &Sender<Pending>) {
    loop {
        match listener.accept() {
            Ok((stream, _)) => match answer(&stream, main) {
                Ok(true) => {}
                // The main thread is gone: the app is quitting.
                Ok(false) => return,
                Err(e) => log::warn!("debug request failed: {e:#}"),
            },
            Err(e) => {
                log::warn!("debug socket accept failed: {e}");
                thread::sleep(ACCEPT_ERROR_PAUSE);
            }
        }
    }
}

/// Reads one request, has the main thread answer it and writes the reply.
/// `Ok(false)` when the main thread no longer takes requests.
fn answer(stream: &UnixStream, main: &Sender<Pending>) -> Result<bool> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut line = String::new();
    BufReader::new(stream.take(MAX_REQUEST)).read_line(&mut line)?;
    log::debug!("debug request: {}", line.trim());
    let reply = match AppRequest::parse(&line) {
        None => Reply::Error(t!("ipc.bad_request").to_string()),
        Some(request) => {
            let (tx, rx) = mpsc::sync_channel(1);
            if main.send(Pending { request, reply: tx }).is_err() {
                return Ok(false);
            }
            // SAFETY: the main run loop lives as long as the process;
            // waking it is thread-safe.
            unsafe { CFRunLoopWakeUp(CFRunLoopGetMain()) };
            rx.recv_timeout(MAIN_TIMEOUT)
                .unwrap_or_else(|_| Reply::Error(t!("debug.app_busy").to_string()))
        }
    };
    let mut out = stream;
    out.write_all(reply.encode()?.as_bytes())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc;

    #[test]
    fn requests_reach_the_main_thread_and_replies_the_client() {
        let dir = std::env::temp_dir().join(format!("mouse-app-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let server = AppServer::start(&dir).unwrap();
        let socket = socket_path(&dir);
        let mode = fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, SOCKET_MODE);
        let main = thread::spawn(move || {
            for _ in 0..2 {
                let p = loop {
                    if let Some(p) = server.next() {
                        break p;
                    }
                    thread::sleep(Duration::from_millis(5));
                };
                let result = match &p.request {
                    AppRequest::Menu => Ok("- Quit  id=quit\n".into()),
                    other => Err(anyhow::anyhow!("no {other}")),
                };
                p.answer(result);
            }
            server
        });
        let ask = |line: &str| ipc::exchange(&socket, line, Duration::from_secs(2)).unwrap();
        assert_eq!(ask("menu"), Reply::Text("- Quit  id=quit\n".into()));
        assert_eq!(ask("window"), Reply::Error("no window".into()));
        assert!(matches!(ask("jump"), Reply::Error(_)));
        drop(main.join().unwrap());
        assert!(!socket.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
