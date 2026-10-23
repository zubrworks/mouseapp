//! Control socket of the daemon: `agent.sock` in the settings folder
//! (`~/Library/Application Support/Mouse`), a Unix domain socket readable
//! by its owner only. One request per connection: the client writes one line
//! (`status`, `dpi <v>`, `reload`, `quit`; `debug ...` in the test build), closes
//! its writing half and reads the reply. The lock file `agent.lock` next to
//! it keeps a single daemon running.

use std::fmt;
use std::fs::{self, File, OpenOptions, Permissions, TryLockError};
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use rust_i18n::t;
use serde::{Deserialize, Serialize};

use crate::hub::MouseInfo;
use crate::permissions::Permission;

const SOCKET_FILE: &str = "agent.sock";
const LOCK_FILE: &str = "agent.lock";
/// Only the owner may talk to the daemon.
const SOCKET_MODE: u32 = 0o600;
/// How long a client waits for the daemon.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the daemon waits for a client's request or reading of the reply.
const SERVER_TIMEOUT: Duration = Duration::from_secs(1);
/// Pause after a failed `accept` (e.g. out of file descriptors), so the
/// server does not spin.
const ACCEPT_ERROR_PAUSE: Duration = Duration::from_millis(100);
/// Longest request line accepted.
const MAX_REQUEST: u64 = 4096;

/// Socket path in the settings folder `dir`.
pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join(SOCKET_FILE)
}

/// What the daemon reports to the settings app.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub mice: Vec<MouseInfo>,
    /// Privacy permissions the daemon process lacks.
    pub missing: Vec<Permission>,
    /// Why the mouse wheel settings are not in effect (`scroll::problem`).
    pub scroll_problem: Option<String>,
    /// Active settings profile.
    pub profile: String,
    /// Which program answers; `None` for a daemon from before the field
    /// existed (0.4.0), which the app treats as an old daemon to replace.
    pub agent: Option<AgentInfo>,
}

/// Identity of a daemon: lets a newly started app tell whether the daemon
/// that is running comes from its own version and bundle.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    /// `CARGO_PKG_VERSION` of the program.
    pub version: String,
    /// Build number from the bundle's `Info.plist`; `None` outside a bundle.
    pub build: Option<String>,
    /// Path of the program that runs.
    pub program: String,
}

impl AgentInfo {
    /// The identity of the running program.
    pub fn current() -> AgentInfo {
        AgentInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            build: crate::macos::bundle_build(),
            program: std::env::current_exe()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }
}

/// A request line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Status,
    Dpi(u16),
    Reload,
    Quit,
    #[cfg(feature = "debug-access")]
    Debug(crate::debug::proto::DaemonRequest),
}

impl Request {
    /// Parses one request line; `None` for anything unknown or malformed.
    pub fn parse(line: &str) -> Option<Request> {
        let line = line.trim();
        let (word, rest) = line
            .split_once(' ')
            .map_or((line, ""), |(w, r)| (w, r.trim()));
        match (word, rest) {
            ("status", "") => Some(Request::Status),
            ("reload", "") => Some(Request::Reload),
            ("quit", "") => Some(Request::Quit),
            ("dpi", v) => v.parse().ok().map(Request::Dpi),
            #[cfg(feature = "debug-access")]
            ("debug", d) => crate::debug::proto::DaemonRequest::parse(d).map(Request::Debug),
            _ => None,
        }
    }
}

impl fmt::Display for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Request::Status => write!(f, "status"),
            Request::Dpi(v) => write!(f, "dpi {v}"),
            Request::Reload => write!(f, "reload"),
            Request::Quit => write!(f, "quit"),
            #[cfg(feature = "debug-access")]
            Request::Debug(d) => write!(f, "debug {d}"),
        }
    }
}

/// The daemon's answer. On the wire: a first line `ok`, `error` or `status`
/// (`text` in the test build), then the error text, the status as TOML or
/// the text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Ok,
    Error(String),
    Status(Status),
    /// Answer to a debug request.
    #[cfg(feature = "debug-access")]
    Text(String),
}

impl Reply {
    pub(crate) fn encode(&self) -> Result<String> {
        Ok(match self {
            Reply::Ok => "ok\n".to_string(),
            Reply::Error(e) => format!("error\n{e}"),
            Reply::Status(s) => format!("status\n{}", toml::to_string(s)?),
            #[cfg(feature = "debug-access")]
            Reply::Text(t) => format!("text\n{t}"),
        })
    }

    fn decode(text: &str) -> Result<Reply> {
        let (head, body) = text.split_once('\n').unwrap_or((text, ""));
        match head {
            "ok" => Ok(Reply::Ok),
            "error" => Ok(Reply::Error(body.to_string())),
            "status" => Ok(Reply::Status(toml::from_str(body)?)),
            #[cfg(feature = "debug-access")]
            "text" => Ok(Reply::Text(body.to_string())),
            _ => bail!(t!("ipc.bad_reply").to_string()),
        }
    }
}

/// Sends one request to the daemon listening on `socket` and returns its reply.
pub fn request(socket: &Path, req: &Request) -> Result<Reply> {
    exchange(socket, &req.to_string(), CLIENT_TIMEOUT)
}

/// Writes the request `line` to the server on `socket` and reads its
/// reply, waiting up to `timeout` for each.
pub(crate) fn exchange(socket: &Path, line: &str, timeout: Duration) -> Result<Reply> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    writeln!(stream, "{line}")?;
    stream.shutdown(Shutdown::Write)?;
    let mut text = String::new();
    stream.read_to_string(&mut text)?;
    Reply::decode(&text)
}

/// Asks the daemon for its status.
pub fn status(socket: &Path) -> Result<Status> {
    match request(socket, &Request::Status)? {
        Reply::Status(s) => Ok(s),
        Reply::Error(e) => Err(anyhow!(e)),
        _ => bail!(t!("ipc.bad_reply").to_string()),
    }
}

/// Result of [`Server::bind`].
pub enum Bind {
    Ready(Server),
    /// Another daemon holds the lock.
    Running,
}

/// The daemon's end of the socket. Dropping it removes the socket file and
/// releases the single-instance lock.
pub struct Server {
    listener: UnixListener,
    path: PathBuf,
    /// Held (locked) for the server's whole life.
    _lock: File,
}

impl Server {
    /// Takes the single-instance lock in `dir` and listens on its socket.
    /// A socket file left by a daemon that died is replaced.
    pub fn bind(dir: &Path) -> Result<Bind> {
        fs::create_dir_all(dir)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(LOCK_FILE))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(Bind::Running),
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
        let path = socket_path(dir);
        // The lock is ours, so a socket file here belongs to a dead daemon.
        match fs::remove_file(&path) {
            Ok(()) => log::info!("removed a stale socket {}", path.display()),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let listener = UnixListener::bind(&path)?;
        let server = Server {
            listener,
            path,
            _lock: lock,
        };
        fs::set_permissions(&server.path, Permissions::from_mode(SOCKET_MODE))?;
        Ok(Bind::Ready(server))
    }

    /// Answers requests with `handler` on a new thread until `stop` returns
    /// true. `stop` is checked after each connection: a request whose
    /// handler stops the server ends it, otherwise [`wake`] does.
    pub fn spawn(
        self,
        handler: impl Fn(Request) -> Reply + Send + 'static,
        stop: impl Fn() -> bool + Send + 'static,
    ) -> io::Result<JoinHandle<()>> {
        thread::Builder::new()
            .name("socket".into())
            .spawn(move || self.serve(&handler, &stop))
    }

    fn serve(&self, handler: &dyn Fn(Request) -> Reply, stop: &dyn Fn() -> bool) {
        while !stop() {
            match self.listener.accept() {
                // The connection from `wake` is not a request.
                Ok(_) if stop() => break,
                Ok((stream, _)) => {
                    if let Err(e) = answer(&stream, handler) {
                        log::warn!("socket request failed: {e:#}");
                    }
                }
                Err(e) => {
                    log::warn!("socket accept failed: {e}");
                    thread::sleep(ACCEPT_ERROR_PAUSE);
                }
            }
        }
    }
}

/// Wakes the server listening on `socket` (waiting in `accept`) so it sees
/// its stop request. Fails if nothing listens there.
pub fn wake(socket: &Path) -> io::Result<()> {
    UnixStream::connect(socket).map(drop)
}

impl Drop for Server {
    fn drop(&mut self) {
        // Runs before the lock is released (fields drop after this).
        if let Err(e) = fs::remove_file(&self.path) {
            log::warn!("cannot remove {}: {e}", self.path.display());
        }
    }
}

/// Reads one request from `stream` and writes the reply.
fn answer(stream: &UnixStream, handler: &dyn Fn(Request) -> Reply) -> Result<()> {
    stream.set_read_timeout(Some(SERVER_TIMEOUT))?;
    stream.set_write_timeout(Some(SERVER_TIMEOUT))?;
    let mut line = String::new();
    BufReader::new(stream.take(MAX_REQUEST)).read_line(&mut line)?;
    log::debug!("socket request: {}", line.trim());
    let reply = match Request::parse(&line) {
        Some(req) => handler(req),
        None => Reply::Error(t!("ipc.bad_request").to_string()),
    };
    let mut out = stream;
    out.write_all(reply.encode()?.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hidpp::features::battery::{Battery, Charge};
    use crate::hidpp::features::dpi::DpiList;
    use crate::hidpp::features::smartshift::SmartShift;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn full_status() -> Status {
        Status {
            mice: vec![
                MouseInfo {
                    key: "unit:aa".into(),
                    name: "MX Master 3S".into(),
                    model: Some("MX Master 3S".into()),
                    battery: Some(Battery {
                        percent: Some(80),
                        charge: Charge::Charging,
                    }),
                    dpi: Some(1600),
                    dpi_list: Some(DpiList {
                        values: vec![400, 800],
                        ranges: vec![(200, 8000, 50)],
                    }),
                    smartshift: Some(SmartShift {
                        ratchet: true,
                        threshold: 10,
                        default: 10,
                    }),
                },
                MouseInfo {
                    key: "slot:/dev/x:1".into(),
                    name: "Other".into(),
                    ..MouseInfo::default()
                },
            ],
            missing: Permission::ALL.to_vec(),
            scroll_problem: Some("no \"permission\"\nsecond line".into()),
            profile: "Mój".into(),
            agent: Some(AgentInfo {
                version: "0.5.0".into(),
                build: Some("120".into()),
                program: "/A B/Mouse.app/Contents/Library/LoginItems/MouseAgent.app/Contents/MacOS/mouse-agent"
                    .into(),
            }),
        }
    }

    /// Fresh empty folder for one test (short: socket paths are limited to 104 bytes).
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mouse-ipc-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn status_survives_encoding() {
        for s in [full_status(), Status::default()] {
            let reply = Reply::Status(s);
            assert_eq!(Reply::decode(&reply.encode().unwrap()).unwrap(), reply);
        }
        for reply in [Reply::Ok, Reply::Error("bad\nthing".into())] {
            assert_eq!(Reply::decode(&reply.encode().unwrap()).unwrap(), reply);
        }
        assert!(Reply::decode("").is_err());
        assert!(Reply::decode("status\nmice = 5").is_err());
        // Permissions travel as their ids.
        let text = Reply::Status(full_status()).encode().unwrap();
        assert!(text.contains("\"input_monitoring\""), "{text}");
    }

    #[test]
    fn status_from_an_old_daemon_has_no_agent() {
        // Reply of daemon 0.4.0: none of the identity fields.
        let old = "status\nmissing = []\nprofile = \"Default\"\n\n[[mice]]\nkey = \"unit:aa\"\nname = \"MX\"\n";
        let Reply::Status(s) = Reply::decode(old).unwrap() else { panic!("not a status") };
        assert_eq!(s.agent, None);
        assert_eq!(s.profile, "Default");
        assert_eq!(s.mice.len(), 1);
        // A new reply carries it (build is optional).
        let new = "status\nmissing = []\nprofile = \"Default\"\nmice = []\n\n[agent]\nversion = \"0.5.0\"\nprogram = \"/x/mouse-agent\"\n";
        let Reply::Status(s) = Reply::decode(new).unwrap() else { panic!("not a status") };
        let agent = s.agent.unwrap();
        assert_eq!((agent.version.as_str(), agent.build, agent.program.as_str()), ("0.5.0", None, "/x/mouse-agent"));
    }

    #[test]
    fn parses_requests() {
        assert_eq!(Request::parse("status\n"), Some(Request::Status));
        assert_eq!(Request::parse(" reload "), Some(Request::Reload));
        assert_eq!(Request::parse("quit"), Some(Request::Quit));
        assert_eq!(Request::parse("dpi 1600"), Some(Request::Dpi(1600)));
        for req in [
            Request::Status,
            Request::Reload,
            Request::Quit,
            Request::Dpi(800),
        ] {
            assert_eq!(Request::parse(&req.to_string()), Some(req));
        }
        for bad in [
            "",
            "dpi",
            "dpi x",
            "dpi 70000",
            "status now",
            "host 2",
            "jump",
        ] {
            assert_eq!(Request::parse(bad), None, "{bad}");
        }
    }

    #[cfg(not(feature = "debug-access"))]
    #[test]
    fn debug_requests_are_unknown_without_debug_access() {
        assert_eq!(Request::parse("debug state"), None);
        assert!(Reply::decode("text\nhello").is_err());
    }

    #[cfg(feature = "debug-access")]
    #[test]
    fn debug_requests_and_text_replies_survive_encoding() {
        use crate::debug::proto::DaemonRequest;
        assert_eq!(Request::parse("debug state"), Some(Request::Debug(DaemonRequest::State)));
        let req = Request::parse("debug log debug 5").unwrap();
        assert_eq!(req.to_string(), "debug log debug 5");
        assert_eq!(Request::parse("debug jump"), None);
        let reply = Reply::Text("profile = \"x\"\nsecond line\n".into());
        assert_eq!(Reply::decode(&reply.encode().unwrap()).unwrap(), reply);
    }

    #[test]
    fn server_answers_client_and_cleans_up() {
        let dir = temp_dir("serve");
        let socket = socket_path(&dir);
        // A socket file left by a dead daemon is replaced.
        drop(UnixListener::bind(&socket).unwrap());
        assert!(socket.exists());
        let Bind::Ready(server) = Server::bind(&dir).unwrap() else {
            panic!("not bound")
        };
        let mode = fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, SOCKET_MODE);
        // A second daemon sees the first one.
        assert!(matches!(Server::bind(&dir).unwrap(), Bind::Running));

        let stop = Arc::new(AtomicBool::new(false));
        let quit = Arc::clone(&stop);
        let handler = move |req| match req {
            Request::Status => Reply::Status(full_status()),
            Request::Quit => {
                quit.store(true, Ordering::SeqCst);
                Reply::Ok
            }
            Request::Dpi(v) if v > 8000 => Reply::Error(format!("{v} too high")),
            _ => Reply::Ok,
        };
        let flag = Arc::clone(&stop);
        let thread = server
            .spawn(handler, move || flag.load(Ordering::SeqCst))
            .unwrap();

        assert_eq!(status(&socket).unwrap(), full_status());
        assert_eq!(request(&socket, &Request::Dpi(1600)).unwrap(), Reply::Ok);
        assert_eq!(
            request(&socket, &Request::Dpi(9000)).unwrap(),
            Reply::Error("9000 too high".into())
        );
        // A malformed request gets an error, not a dropped connection.
        let mut raw = UnixStream::connect(&socket).unwrap();
        raw.write_all(b"jump\n").unwrap();
        raw.shutdown(Shutdown::Write).unwrap();
        let mut text = String::new();
        raw.read_to_string(&mut text).unwrap();
        assert!(matches!(Reply::decode(&text).unwrap(), Reply::Error(_)));

        assert_eq!(request(&socket, &Request::Quit).unwrap(), Reply::Ok);
        thread.join().unwrap();
        assert!(!socket.exists());
        assert!(status(&socket).is_err());
        // The lock is free again.
        assert!(matches!(Server::bind(&dir).unwrap(), Bind::Ready(_)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wake_ends_an_idle_server() {
        let dir = temp_dir("wake");
        let socket = socket_path(&dir);
        let Bind::Ready(server) = Server::bind(&dir).unwrap() else {
            panic!("not bound")
        };
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = server
            .spawn(|_| Reply::Ok, move || flag.load(Ordering::SeqCst))
            .unwrap();
        assert_eq!(request(&socket, &Request::Reload).unwrap(), Reply::Ok);
        stop.store(true, Ordering::SeqCst);
        // Fails only if the server saw the flag on its own and is gone.
        let _ = wake(&socket);
        thread.join().unwrap();
        assert!(wake(&socket).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
