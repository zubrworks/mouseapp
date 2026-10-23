//! The settings app's link to the daemon ("the mouse driver"). A background
//! thread asks the daemon for its status (every second while the settings
//! window is open, else every few seconds) and sends the
//! app's requests through the control socket ([`ipc`]), so a slow or missing
//! daemon never blocks the window or the menu. The daemon is started from
//! here too ([`launch`]).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use rust_i18n::t;

use crate::bundle::{self, DaemonLaunch};
use crate::macos;
use crate::ipc::{self, AgentInfo, Reply, Request, Status};
use crate::util::lock;

/// How often the status is asked for while the settings window is open.
const POLL: Duration = Duration::from_secs(1);
/// How often the status is asked for while it is closed (the menu only).
const POLL_IDLE: Duration = Duration::from_secs(3);
/// How often a daemon that is starting is asked for its status.
const POLL_STARTING: Duration = Duration::from_millis(200);
/// How long a started daemon gets to answer before it counts as not running.
const START_WAIT: Duration = Duration::from_secs(5);
/// How long an old daemon gets to quit when the app replaces it.
const REPLACE_WAIT: Duration = Duration::from_secs(10);
/// How often the old daemon is asked while it quits.
const REPLACE_STEP: Duration = Duration::from_millis(100);
/// Pause after the old daemon's socket is gone, so its process is gone too
/// before the new one is opened through LaunchServices.
const REPLACE_SETTLE: Duration = Duration::from_millis(300);

/// What the app does with a daemon that is already running when it starts.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Same version and same bundle: leave it.
    Keep,
    /// The app is not in a bundle (`cargo run`): never replaces anything.
    NoBundle,
    /// Another version or bundle (the text says why): quit it, start ours.
    Replace(String),
}

/// Compares the running daemon (`status`) with the app `own` (the identity of
/// the app program, run from `exe`).
fn verdict(exe: &Path, own: &AgentInfo, status: &Status) -> Verdict {
    let Some(bundle) = bundle::agent_bundle(exe) else {
        return Verdict::NoBundle;
    };
    let Some(agent) = &status.agent else {
        return Verdict::Replace("it does not report its version (0.4.0 or older)".into());
    };
    if agent.version != own.version || agent.build != own.build {
        return Verdict::Replace(format!(
            "it is version {} (build {:?}), the app is {} (build {:?})",
            agent.version, agent.build, own.version, own.build
        ));
    }
    let running = bundle::agent_bundle(Path::new(&agent.program));
    if running.as_deref().map(same_path) != Some(same_path(&bundle)) {
        return Verdict::Replace(format!("it runs from {}, not from {}", agent.program, bundle.display()));
    }
    Verdict::Keep
}

/// Calls `done` every `step` until it returns true; false if `wait` passes first.
fn wait_until(wait: Duration, step: Duration, mut done: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + wait;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        thread::sleep(step);
    }
}

/// The path with symlinks resolved when possible, for comparing two paths.
fn same_path(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// What the app knows about the daemon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Started (by the app) and not answering yet.
    Starting,
    Running(Status),
    /// Runs, but is an older version (or from another bundle) that did not
    /// quit when the app tried to replace it at its start.
    Outdated(Status),
    /// Does not answer.
    Stopped,
}

impl State {
    /// The daemon's status while it runs.
    pub fn status(&self) -> Option<&Status> {
        match self {
            State::Running(s) | State::Outdated(s) => Some(s),
            State::Starting | State::Stopped => None,
        }
    }
}

enum Job {
    Send(Request),
    Start,
    /// Ask for the status now.
    Poll,
}

/// Latest state and a counter that grows with every change of it.
struct Shared {
    version: u64,
    state: State,
}

/// Handle of the background thread; dropping it ends the thread.
pub struct Driver {
    shared: Arc<Mutex<Shared>>,
    jobs: Sender<Job>,
    /// The settings window is open: poll faster.
    watching: Arc<AtomicBool>,
}

impl Driver {
    /// Starts talking to the daemon listening on `socket`; with `start` the
    /// daemon is launched when it does not answer.
    pub fn connect(socket: PathBuf, start: bool) -> Result<Driver> {
        // Until the first answer: starting, if the app is going to start it.
        let state = if start { State::Starting } else { State::Stopped };
        let shared = Arc::new(Mutex::new(Shared { version: 0, state }));
        let (jobs, rx) = channel();
        let watching = Arc::new(AtomicBool::new(false));
        let link = Link {
            socket,
            shared: Arc::clone(&shared),
            watching: Arc::clone(&watching),
            starting_until: None,
            outdated: false,
        };
        thread::Builder::new()
            .name("driver".into())
            .spawn(move || link.serve(&rx, start))?;
        Ok(Driver { shared, jobs, watching })
    }

    /// State version (changes with the state) and the state.
    pub fn snapshot(&self) -> (u64, State) {
        let s = lock(&self.shared);
        (s.version, s.state.clone())
    }

    /// Sends `req` to the daemon (in the background); a refusal is logged.
    pub fn send(&self, req: Request) {
        let _ = self.jobs.send(Job::Send(req));
    }

    /// Tells whether the settings window is open (fast polling); opening it
    /// asks for the status at once.
    pub fn set_watching(&self, open: bool) {
        if self.watching.swap(open, Ordering::Relaxed) != open && open {
            let _ = self.jobs.send(Job::Poll);
        }
    }

    /// Launches the daemon unless it already answers.
    pub fn start(&self) {
        let _ = self.jobs.send(Job::Start);
    }
}

/// The background thread's side.
struct Link {
    socket: PathBuf,
    shared: Arc<Mutex<Shared>>,
    watching: Arc<AtomicBool>,
    /// Set after a launch: until then a silent daemon is still starting.
    starting_until: Option<Instant>,
    /// The daemon that answers did not quit when asked to (see
    /// [`Link::replace_old`]); cleared when it stops answering.
    outdated: bool,
}

impl Link {
    fn serve(mut self, jobs: &Receiver<Job>, start: bool) {
        if start {
            if self.replace_old() {
                self.start();
            }
        } else {
            self.poll();
        }
        let mut next_poll = Instant::now() + self.interval();
        loop {
            match jobs.recv_timeout(next_poll.saturating_duration_since(Instant::now())) {
                Ok(Job::Send(req)) => {
                    self.send(&req);
                    // Show the effect of the request right away.
                    self.poll();
                }
                Ok(Job::Start) => self.start(),
                Ok(Job::Poll) => self.poll(),
                Err(RecvTimeoutError::Timeout) => self.poll(),
                // The app is gone.
                Err(RecvTimeoutError::Disconnected) => return,
            }
            next_poll = Instant::now() + self.interval();
        }
    }

    fn interval(&self) -> Duration {
        if self.starting_until.is_some() {
            POLL_STARTING
        } else if self.watching.load(Ordering::Relaxed) {
            POLL
        } else {
            POLL_IDLE
        }
    }

    fn set(&self, state: State) {
        let mut s = lock(&self.shared);
        if s.state != state {
            s.state = state;
            s.version += 1;
        }
    }

    /// Asks for the status and records the answer (or the silence).
    fn poll(&mut self) {
        match ipc::status(&self.socket) {
            Ok(status) => {
                self.starting_until = None;
                self.set(if self.outdated { State::Outdated(status) } else { State::Running(status) });
            }
            Err(e) => {
                self.outdated = false;
                log::debug!("the mouse driver does not answer: {e:#}");
                match self.starting_until {
                    Some(until) if Instant::now() < until => self.set(State::Starting),
                    Some(_) => {
                        self.starting_until = None;
                        log::warn!("{}", t!("driver.no_answer"));
                        self.set(State::Stopped);
                    }
                    None => self.set(State::Stopped),
                }
            }
        }
    }

    /// At the start of the app: a running daemon of another version, or from
    /// another bundle, is asked to quit and waited for (until its process is
    /// gone); [`Link::start`] then launches the one of this app. Done once
    /// per start of the app, never repeated. The settings are not touched:
    /// the new daemon reads the same files. Returns whether the daemon may
    /// be started now.
    ///
    /// A daemon that does not answer yet is not replaced: if launchd is just
    /// starting an old one and its socket is not up, this check sees nothing
    /// and [`Link::start`] finds the socket taken or launches ours; the old
    /// one is dealt with at the next start of the app (no loop, no second
    /// daemon).
    fn replace_old(&mut self) -> bool {
        self.poll();
        let State::Running(status) = lock(&self.shared).state.clone() else {
            return true;
        };
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => {
                log::warn!("cannot check the running mouse driver: {e}");
                return true;
            }
        };
        match verdict(&exe, &AgentInfo::current(), &status) {
            Verdict::Keep => true,
            Verdict::NoBundle => {
                log::info!("not replacing the running mouse driver (not run from an app bundle)");
                true
            }
            Verdict::Replace(why) => {
                log::info!("replacing the running mouse driver: {why}");
                self.set(State::Starting);
                if self.quit_old(REPLACE_WAIT, || !macos::app_is_running(bundle::AGENT_ID)) {
                    return true;
                }
                log::warn!("{}", t!("driver.replace_timeout"));
                // Shown as an outdated driver while it answers; if only its
                // process lingers, it shows as stopped. Either way no second
                // daemon is started: the user can quit the old one and start.
                self.outdated = true;
                self.poll();
                false
            }
        }
    }

    /// Asks the daemon to quit and waits up to `wait` until it no longer
    /// answers and `process_gone` tells that its process is gone too (the
    /// socket closes before the daemon has released the mouse; opening the
    /// new bundle while the old process lives would only activate that one).
    /// False on timeout.
    fn quit_old(&self, wait: Duration, process_gone: impl Fn() -> bool) -> bool {
        if let Err(e) = ipc::request(&self.socket, &Request::Quit) {
            log::debug!("the old mouse driver does not take the request: {e:#}");
        }
        let gone = wait_until(wait, REPLACE_STEP, || {
            ipc::status(&self.socket).is_err() && process_gone()
        });
        if gone {
            thread::sleep(REPLACE_SETTLE);
        }
        gone
    }

    /// Launches the daemon when it does not answer (and is not starting yet).
    fn start(&mut self) {
        self.poll();
        let state = lock(&self.shared).state.clone();
        if state != State::Stopped {
            return;
        }
        match launch() {
            Ok(()) => {
                self.starting_until = Some(Instant::now() + START_WAIT);
                self.set(State::Starting);
            }
            Err(e) => log::error!("{}", t!("driver.start_failed", error = format!("{e:#}"))),
        }
    }

    fn send(&self, req: &Request) {
        match ipc::request(&self.socket, req) {
            Ok(Reply::Error(e)) => log::warn!("{}", t!("driver.refused", error = e)),
            Ok(_) => {}
            Err(e) => log::warn!("{}", t!("driver.unreachable", error = format!("{e:#}"))),
        }
    }
}

/// Starts the daemon in the background. From an app bundle it opens the
/// bundled `MouseAgent.app` with `open -g` (no window, no focus change) so
/// the daemon is a proper LaunchServices process that holds the privacy
/// permissions. Elsewhere (e.g. `cargo run`) it starts the same program with
/// `run`, in its own process group (so Ctrl-C in a terminal that started the
/// app does not reach it). This is the only place that knows how the daemon
/// is started.
fn launch() -> Result<()> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()?;
    let launch = DaemonLaunch::for_exe(&exe, true);
    let argv = launch.argv("-g");
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    if matches!(launch, DaemonLaunch::Binary(_)) {
        command.process_group(0);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    log::info!("started the mouse driver ({:?}, pid {})", command.get_program(), child.id());
    // Collect its exit status so it does not linger as a zombie while the app runs.
    thread::Builder::new()
        .name("driver-child".into())
        .spawn(move || {
            let _ = child.wait();
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{Bind, Server};
    use std::sync::atomic::{AtomicBool, Ordering};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mouse-drv-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Waits until the driver's state matches `want`.
    fn wait_for(driver: &Driver, want: impl Fn(&State) -> bool) -> State {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let (_, state) = driver.snapshot();
            if want(&state) || Instant::now() > until {
                return state;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn info(version: &str, build: Option<&str>, program: &str) -> AgentInfo {
        AgentInfo { version: version.into(), build: build.map(Into::into), program: program.into() }
    }

    const APP: &str = "/A/Mouse.app/Contents/MacOS/mouse-bin";
    const AGENT: &str = "/A/Mouse.app/Contents/Library/LoginItems/MouseAgent.app/Contents/MacOS/mouse-agent";

    fn judge(exe: &str, running: Option<AgentInfo>) -> Verdict {
        let own = info("0.5.0", Some("120"), exe);
        verdict(Path::new(exe), &own, &Status { agent: running, ..Status::default() })
    }

    #[test]
    fn replaces_only_a_different_daemon() {
        // Same version, build and bundle: left alone.
        assert_eq!(judge(APP, Some(info("0.5.0", Some("120"), AGENT))), Verdict::Keep);
        // Daemon 0.4.0 (no identity at all).
        assert!(matches!(judge(APP, None), Verdict::Replace(_)));
        // Other version or other build of the same version.
        assert!(matches!(judge(APP, Some(info("0.4.0", Some("120"), AGENT))), Verdict::Replace(_)));
        assert!(matches!(judge(APP, Some(info("0.5.0", Some("119"), AGENT))), Verdict::Replace(_)));
        assert!(matches!(judge(APP, Some(info("0.5.0", None, AGENT))), Verdict::Replace(_)));
        // Same version from another bundle, or not from a bundle at all.
        let other = "/B/Mouse.app/Contents/Library/LoginItems/MouseAgent.app/Contents/MacOS/mouse-agent";
        assert!(matches!(judge(APP, Some(info("0.5.0", Some("120"), other))), Verdict::Replace(_)));
        let bare = "/tmp/target/debug/mouse";
        assert!(matches!(judge(APP, Some(info("0.5.0", Some("120"), bare))), Verdict::Replace(_)));
        // Without a bundle (`cargo run`) nothing is replaced, even a 0.4.0 daemon.
        assert_eq!(judge(bare, None), Verdict::NoBundle);
    }

    #[test]
    fn wait_until_waits_for_the_condition_within_the_limit() {
        let step = Duration::from_millis(1);
        let mut calls = 0;
        assert!(wait_until(Duration::from_secs(5), step, || {
            calls += 1;
            calls == 4
        }));
        assert_eq!(calls, 4);
        assert!(!wait_until(Duration::from_millis(20), step, || false));
        // A condition that is true at once is not delayed.
        assert!(wait_until(Duration::ZERO, step, || true));
    }

    #[test]
    fn quit_old_waits_for_the_process_too() {
        let dir = temp_dir("proc");
        let link = Link {
            socket: ipc::socket_path(&dir),
            shared: Arc::new(Mutex::new(Shared { version: 0, state: State::Stopped })),
            watching: Arc::new(AtomicBool::new(false)),
            starting_until: None,
            outdated: false,
        };
        // No socket, but the process lingers: not gone within the limit.
        assert!(!link.quit_old(Duration::from_millis(200), || false));
        // It leaves after a few checks.
        let checks = std::cell::Cell::new(0);
        assert!(link.quit_old(Duration::from_secs(5), || {
            checks.set(checks.get() + 1);
            checks.get() > 3
        }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quit_old_waits_for_the_daemon_to_go() {
        let dir = temp_dir("replace");
        let socket = ipc::socket_path(&dir);
        let link = Link {
            socket: socket.clone(),
            shared: Arc::new(Mutex::new(Shared { version: 0, state: State::Stopped })),
            watching: Arc::new(AtomicBool::new(false)),
            starting_until: None,
            outdated: false,
        };
        // Nothing runs: nothing to wait for.
        assert!(link.quit_old(Duration::from_secs(1), || true));

        let Bind::Ready(server) = Server::bind(&dir).unwrap() else { panic!("not bound") };
        let stop = Arc::new(AtomicBool::new(false));
        let (quit, flag) = (Arc::clone(&stop), Arc::clone(&stop));
        let thread = server
            .spawn(
                move |req| {
                    if req == Request::Quit {
                        quit.store(true, Ordering::SeqCst);
                    }
                    Reply::Ok
                },
                move || flag.load(Ordering::SeqCst),
            )
            .unwrap();
        assert!(link.quit_old(Duration::from_secs(5), || true));
        thread.join().unwrap();
        assert!(ipc::status(&socket).is_err());

        // A daemon that ignores `quit` is reported as still running.
        let Bind::Ready(server) = Server::bind(&dir).unwrap() else { panic!("not bound") };
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = server
            .spawn(|_| Reply::Status(Status::default()), move || flag.load(Ordering::SeqCst))
            .unwrap();
        assert!(!link.quit_old(Duration::from_millis(300), || true));
        stop.store(true, Ordering::SeqCst);
        ipc::wake(&socket).unwrap();
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn follows_the_daemon_and_sends_requests() {
        let dir = temp_dir("link");
        let socket = ipc::socket_path(&dir);
        let driver = Driver::connect(socket.clone(), false).unwrap();
        driver.set_watching(true);
        assert_eq!(wait_for(&driver, |s| *s == State::Stopped), State::Stopped);
        let (v1, _) = driver.snapshot();

        let Bind::Ready(server) = Server::bind(&dir).unwrap() else { panic!("not bound") };
        let stop = Arc::new(AtomicBool::new(false));
        let got = Arc::new(Mutex::new(Vec::new()));
        let (seen, quit) = (Arc::clone(&got), Arc::clone(&stop));
        let status = Status { profile: "Mój".into(), ..Status::default() };
        let answer = status.clone();
        let flag = Arc::clone(&stop);
        let thread = server
            .spawn(
                move |req| {
                    seen.lock().unwrap().push(req.clone());
                    match req {
                        Request::Status => Reply::Status(answer.clone()),
                        Request::Quit => {
                            quit.store(true, Ordering::SeqCst);
                            Reply::Ok
                        }
                        _ => Reply::Ok,
                    }
                },
                move || flag.load(Ordering::SeqCst),
            )
            .unwrap();

        // The next poll (at most a second away) finds the daemon.
        let state = wait_for(&driver, |s| matches!(s, State::Running(_)));
        assert_eq!(state.status(), Some(&status));
        assert!(driver.snapshot().0 > v1);

        driver.send(Request::Dpi(800));
        driver.send(Request::Quit);
        thread.join().unwrap();
        assert!(got.lock().unwrap().contains(&Request::Dpi(800)));
        assert_eq!(wait_for(&driver, |s| *s == State::Stopped), State::Stopped);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
