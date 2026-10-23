//! Sending and receiving HID reports.
//!
//! Every [`Transport`] has a reader thread that drains hidapi's input queue
//! into its own [`Inbox`] all the time. hidapi on macOS keeps only ~32
//! reports and silently drops the oldest; the mouse thread can be busy for
//! half a second (a write that wakes the Bluetooth radio blocks that long),
//! and the same handle also gets the ordinary mouse reports (cursor, wheel),
//! so button notifications used to be lost. The reader keeps only HID++
//! reports and counts the rest.
//!
//! The reader has its own HID handle: `HidDevice` is not `Sync`, and a
//! shared handle behind a lock would stop the reader during a slow write,
//! which is exactly when it must keep reading. Input reports reach every
//! open (non-exclusive) handle, so responses to writes on the writer handle
//! arrive at the reader.
//!
//! Several threads can wait for responses at once (the mouse thread and the
//! check thread): each [`Transport::exchange`] registers what its response
//! looks like before writing, and the reader hands a matching report straight
//! to it. Everything else (notifications) goes to the queue read by
//! [`Transport::read`], so a request that waits never holds up a button
//! notification.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use hidapi::{HidApi, HidDevice};

/// Report ID of a short HID++ report (7 bytes).
pub const REPORT_SHORT: u8 = 0x10;
/// Report ID of a long HID++ report (20 bytes).
pub const REPORT_LONG: u8 = 0x11;
/// Total length of a short report, including the report ID.
pub const SHORT_LEN: usize = 7;
/// Total length of a long report, including the report ID.
pub const LONG_LEN: usize = 20;

/// HID++ reports kept until the mouse thread reads them (a gesture sends
/// ~120 a second, so this covers 2 s of a blocked mouse thread); when full
/// the oldest is dropped and counted.
const INBOX_MAX: usize = 256;
/// How long one blocking read of the reader thread lasts. The reader only
/// wakes for a report, a disconnect or this timeout (when it checks whether
/// its transport was dropped), so a long value keeps the idle cost low; a
/// dropped transport's handle stays open at most this long (or until the
/// mouse sends anything).
const READER_WAIT_MS: i32 = 60_000;

/// Returns the expected total length for a HID++ report ID.
pub fn report_len(report_id: u8) -> Option<usize> {
    match report_id {
        REPORT_SHORT => Some(SHORT_LEN),
        REPORT_LONG => Some(LONG_LEN),
        _ => None,
    }
}

/// True for a complete HID++ report (known report ID, long enough).
fn is_hidpp(raw: &[u8]) -> bool {
    raw.first()
        .and_then(|id| report_len(*id))
        .is_some_and(|len| raw.len() >= len)
}

/// Marks an error of the HID layer itself (device unplugged, link gone), as
/// opposed to an answer from the device. Attached with `.context(..)`.
#[derive(Debug)]
pub struct IoFailed(pub &'static str);

impl std::fmt::Display for IoFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HID {} failed", self.0)
    }
}

/// Tells whether a report is the response a waiting request expects.
pub type Matcher = Box<dyn Fn(&[u8]) -> bool + Send>;

/// A request waiting for its response.
struct Waiter {
    id: u64,
    matches: Matcher,
    reply: Option<Vec<u8>>,
}

/// What the reader thread dropped since the last [`Transport::take_stats`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReaderStats {
    /// Reports that are not HID++ (e.g. ordinary mouse reports).
    pub non_hidpp: u32,
    /// Report ID of the latest of them.
    pub non_hidpp_id: Option<u8>,
    /// HID++ reports lost because the inbox was full.
    pub dropped: u32,
}

#[derive(Default)]
struct InboxState {
    reports: VecDeque<Vec<u8>>,
    /// When the last report (of any kind) arrived.
    last_rx: Option<Instant>,
    stats: ReaderStats,
    /// Why the reader stopped; reported once the queued reports are read.
    failed: Option<String>,
    /// Requests waiting for their responses.
    waiters: Vec<Waiter>,
    next_waiter: u64,
    /// [`Inbox::interrupt`] was called: the next [`Inbox::pop`] that would
    /// wait returns at once.
    interrupted: bool,
}

/// Reports passed from the reader thread to the mouse thread.
#[derive(Default)]
struct Inbox {
    state: Mutex<InboxState>,
    ready: Condvar,
    /// Set when the transport is dropped: the reader quits.
    stop: AtomicBool,
}

impl Inbox {
    fn lock(&self) -> MutexGuard<'_, InboxState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes one report from the reader: hands a response to the request
    /// waiting for it, queues other HID++ reports, counts the rest.
    fn push(&self, raw: &[u8], now: Instant) {
        let mut s = self.lock();
        s.last_rx = Some(now);
        if !is_hidpp(raw) {
            s.stats.non_hidpp = s.stats.non_hidpp.saturating_add(1);
            s.stats.non_hidpp_id = raw.first().copied();
            return;
        }
        if let Some(w) = s.waiters.iter_mut().find(|w| w.reply.is_none() && (w.matches)(raw)) {
            w.reply = Some(raw.to_vec());
            drop(s);
            self.ready.notify_all();
            return;
        }
        if s.reports.len() >= INBOX_MAX {
            s.reports.pop_front();
            s.stats.dropped = s.stats.dropped.saturating_add(1);
        }
        s.reports.push_back(raw.to_vec());
        drop(s);
        // The mouse thread and requests share the condition variable.
        self.ready.notify_all();
    }

    /// Registers a request before it is written, so that even a quick
    /// response finds it. Returns the id for [`Self::wait_reply`].
    fn register(&self, matches: Matcher) -> u64 {
        let mut s = self.lock();
        let id = s.next_waiter;
        s.next_waiter += 1;
        s.waiters.push(Waiter { id, matches, reply: None });
        id
    }

    /// Takes the request `id` off the list; its response if it came.
    fn unregister(&self, id: u64) -> Option<Vec<u8>> {
        let mut s = self.lock();
        let pos = s.waiters.iter().position(|w| w.id == id)?;
        s.waiters.swap_remove(pos).reply
    }

    /// Waits until the response of request `id` arrives or `deadline`
    /// passes (`None`), then unregisters it. An error if the reader failed.
    fn wait_reply(&self, id: u64, deadline: Instant) -> Result<Option<Vec<u8>>> {
        let mut s = self.lock();
        loop {
            let Some(w) = s.waiters.iter().find(|w| w.id == id) else {
                return Ok(None);
            };
            if w.reply.is_some() {
                break;
            }
            if let Some(e) = &s.failed {
                let e = anyhow!("{e}").context(IoFailed("read"));
                drop(s);
                self.unregister(id);
                return Err(e);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            s = self.ready.wait_timeout(s, left).unwrap_or_else(PoisonError::into_inner).0;
        }
        drop(s);
        Ok(self.unregister(id))
    }

    /// Wakes a [`Self::pop`] that waits (or makes the next one return at
    /// once) without a report: the stop request or the end of the checks.
    fn interrupt(&self) {
        self.lock().interrupted = true;
        self.ready.notify_all();
    }

    /// The reader stopped on a read error.
    fn fail(&self, error: String) {
        self.lock().failed = Some(error);
        self.ready.notify_all();
    }

    /// The next HID++ report, waiting up to `timeout_ms` (negative = no
    /// limit). `None` on timeout; an error once the reader has failed and
    /// everything it queued before was read.
    fn pop(&self, timeout_ms: i32) -> Result<Option<Vec<u8>>> {
        let deadline = u64::try_from(timeout_ms)
            .ok()
            .map(|ms| Instant::now() + Duration::from_millis(ms));
        let mut s = self.lock();
        loop {
            if let Some(r) = s.reports.pop_front() {
                return Ok(Some(r));
            }
            if let Some(e) = &s.failed {
                return Err(anyhow!("{e}").context(IoFailed("read")));
            }
            if std::mem::take(&mut s.interrupted) {
                return Ok(None);
            }
            s = match deadline {
                None => self.ready.wait(s).unwrap_or_else(PoisonError::into_inner),
                Some(d) => {
                    let left = d.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Ok(None);
                    }
                    self.ready.wait_timeout(s, left).unwrap_or_else(PoisonError::into_inner).0
                }
            };
        }
    }
}

/// Body of the reader thread: blocks in hidapi until a report arrives, the
/// device goes away or `READER_WAIT_MS` passes. Closes `device` on return.
fn run_reader(device: HidDevice, inbox: Arc<Inbox>) {
    let mut buf = [0u8; 64];
    while !inbox.stop.load(Ordering::Relaxed) {
        match device.read_timeout(&mut buf, READER_WAIT_MS) {
            Ok(0) => {}
            Ok(n) => inbox.push(&buf[..n], Instant::now()),
            Err(e) => {
                if !inbox.stop.load(Ordering::Relaxed) {
                    log::debug!("HID reader stopped: {e}");
                    inbox.fail(e.to_string());
                }
                return;
            }
        }
    }
}

/// Gets the written reports of a fake transport (tests).
#[cfg(test)]
type FakeWrite = Box<dyn FnMut(&[u8]) + Send>;

/// Where requests are written.
enum Writer {
    Hid(HidDevice),
    /// Records the requests (tests).
    #[cfg(test)]
    Fake(FakeWrite),
}

impl Writer {
    fn write(&mut self, report: &[u8]) -> Result<()> {
        match self {
            Writer::Hid(d) => {
                d.write(report).context(IoFailed("write"))?;
            }
            #[cfg(test)]
            Writer::Fake(f) => f(report),
        }
        Ok(())
    }
}

/// Wakes the thread waiting in [`Transport::read`] from any thread.
#[derive(Clone)]
pub struct Interrupter(Arc<Inbox>);

impl Interrupter {
    pub fn interrupt(&self) {
        self.0.interrupt();
    }
}

/// The result of [`Transport::exchange`].
#[derive(Debug)]
pub struct Exchange {
    /// The response; `None` if none came in time.
    pub reply: Option<Vec<u8>>,
    /// How long the write took (a write that wakes the Bluetooth radio of
    /// an idle mouse blocks for ~0.5 s).
    pub write: Duration,
}

/// An open HID++ channel to a single HID interface. Usable from several
/// threads: writes take turns, responses go to the request that waits for
/// them.
pub struct Transport {
    writer: Mutex<Writer>,
    inbox: Arc<Inbox>,
}

impl Transport {
    /// Opens the interface at `path` without exclusive access (twice: one
    /// handle for writing, one for the reader thread).
    ///
    /// On macOS an exclusive open stops the system from receiving cursor
    /// events from the device, so exclusivity is always turned off.
    pub fn open(api: &HidApi, path: &std::ffi::CStr) -> Result<Self> {
        api.set_open_exclusive(false);
        let open = || {
            api.open_path(path)
                .with_context(|| format!("cannot open HID device {path:?}"))
        };
        let writer = open()?;
        let reader = open()?;
        let inbox = Arc::new(Inbox::default());
        let shared = Arc::clone(&inbox);
        thread::Builder::new()
            .name("hid-reader".into())
            .spawn(move || run_reader(reader, shared))
            .context("cannot start the HID reader thread")?;
        Ok(Self { writer: Mutex::new(Writer::Hid(writer)), inbox })
    }

    /// A transport without a device: `write` gets every written report and
    /// the returned [`Feed`] plays the reader thread (tests).
    #[cfg(test)]
    pub fn fake(write: impl FnMut(&[u8]) + Send + 'static) -> (Self, Feed) {
        let inbox = Arc::new(Inbox::default());
        let feed = Feed(Arc::clone(&inbox));
        (Self { writer: Mutex::new(Writer::Fake(Box::new(write))), inbox }, feed)
    }

    /// Writes one HID++ report (first byte must be a known report ID).
    /// Waits while another thread writes.
    pub fn write(&self, report: &[u8]) -> Result<()> {
        let Some(expected) = report.first().and_then(|id| report_len(*id)) else {
            bail!("not a HID++ report");
        };
        if report.len() != expected {
            bail!("wrong report length: {} (expected {expected})", report.len());
        }
        crate::util::lock(&self.writer).write(report)
    }

    /// Writes `report` and waits up to `timeout` (from the end of the write)
    /// for the response `matches` recognizes. Reports that do not match stay
    /// for [`Self::read`], so other threads are not held up.
    pub fn exchange(&self, report: &[u8], matches: Matcher, timeout: Duration) -> Result<Exchange> {
        let id = self.inbox.register(matches);
        let start = Instant::now();
        let written = self.write(report);
        let write = start.elapsed();
        if let Err(e) = written {
            self.inbox.unregister(id);
            return Err(e);
        }
        let reply = self.inbox.wait_reply(id, Instant::now() + timeout)?;
        Ok(Exchange { reply, write })
    }

    /// Wakes [`Self::read`] from another thread.
    pub fn interrupter(&self) -> Interrupter {
        Interrupter(Arc::clone(&self.inbox))
    }

    /// Reads one HID++ report that no request is waiting for, waiting up to
    /// `timeout_ms` (negative = no limit). Returns `None` on timeout or when
    /// interrupted.
    pub fn read(&self, timeout_ms: i32) -> Result<Option<Vec<u8>>> {
        self.inbox.pop(timeout_ms)
    }

    /// When the last report of any kind (also non-HID++) arrived.
    pub fn last_rx(&self) -> Option<Instant> {
        self.inbox.lock().last_rx
    }

    /// What the reader dropped so far; starts over.
    pub fn take_stats(&self) -> ReaderStats {
        std::mem::take(&mut self.inbox.lock().stats)
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        // The reader is blocked in hidapi and cannot be woken; it quits
        // after the next report or read timeout and closes its handle.
        self.inbox.stop.store(true, Ordering::Relaxed);
    }
}

/// Feeds reports to a fake transport as its reader thread would (tests).
#[cfg(test)]
#[derive(Clone)]
pub struct Feed(Arc<Inbox>);

#[cfg(test)]
impl Feed {
    pub fn push(&self, raw: &[u8]) {
        self.0.push(raw, Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn short(n: u8) -> Vec<u8> {
        vec![REPORT_SHORT, 0xFF, n, 0, 0, 0, 0]
    }

    #[test]
    fn report_lengths() {
        assert_eq!(report_len(REPORT_SHORT), Some(7));
        assert_eq!(report_len(REPORT_LONG), Some(20));
        assert_eq!(report_len(0x20), None);
    }

    #[test]
    fn inbox_keeps_hidpp_and_counts_the_rest() {
        let inbox = Inbox::default();
        let now = Instant::now();
        inbox.push(&[0x02, 0, 1, 0, 0], now);
        inbox.push(&short(1), now);
        // Known report ID, but cut short: not HID++ either.
        inbox.push(&[REPORT_LONG, 0xFF, 0], now);
        inbox.push(&short(2), now);
        assert_eq!(inbox.pop(0).unwrap(), Some(short(1)));
        assert_eq!(inbox.pop(0).unwrap(), Some(short(2)));
        assert_eq!(inbox.pop(0).unwrap(), None);
        let s = std::mem::take(&mut inbox.lock().stats);
        assert_eq!(s, ReaderStats { non_hidpp: 2, non_hidpp_id: Some(REPORT_LONG), dropped: 0 });
        assert_eq!(inbox.lock().last_rx, Some(now));
    }

    #[test]
    fn inbox_is_bounded_and_drops_the_oldest() {
        let inbox = Inbox::default();
        for i in 0..(INBOX_MAX + 3) {
            inbox.push(&short(i as u8), Instant::now());
        }
        assert_eq!(inbox.lock().reports.len(), INBOX_MAX);
        assert_eq!(inbox.lock().stats.dropped, 3);
        assert_eq!(inbox.pop(0).unwrap(), Some(short(3)));
    }

    #[test]
    fn pop_waits_for_a_report_from_another_thread() {
        let inbox = Arc::new(Inbox::default());
        let sender = Arc::clone(&inbox);
        let t = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            sender.push(&short(7), Instant::now());
        });
        assert_eq!(inbox.pop(5_000).unwrap(), Some(short(7)));
        t.join().unwrap();
    }

    #[test]
    fn pop_times_out_when_nothing_comes() {
        let inbox = Inbox::default();
        let start = Instant::now();
        assert_eq!(inbox.pop(30).unwrap(), None);
        assert!(start.elapsed() >= Duration::from_millis(30));
    }

    /// Matches a report whose third byte is `n` (stands in for a HID++ response).
    fn third_byte_is(n: u8) -> Matcher {
        Box::new(move |raw: &[u8]| raw.get(2) == Some(&n))
    }

    #[test]
    fn a_waiting_request_gets_its_response_and_notifications_stay_queued() {
        let inbox = Arc::new(Inbox::default());
        let id = inbox.register(third_byte_is(9));
        let reader = Arc::clone(&inbox);
        let deadline = Instant::now() + Duration::from_secs(5);
        let waiter = thread::spawn(move || reader.wait_reply(id, deadline));
        // A notification while the request waits: readable at once.
        inbox.push(&short(1), Instant::now());
        let start = Instant::now();
        assert_eq!(inbox.pop(1_000).unwrap(), Some(short(1)));
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!(!waiter.is_finished());
        // The response goes to the request, not to the queue.
        inbox.push(&short(9), Instant::now());
        assert_eq!(waiter.join().unwrap().unwrap(), Some(short(9)));
        assert_eq!(inbox.pop(0).unwrap(), None);
        assert!(inbox.lock().waiters.is_empty());
    }

    #[test]
    fn two_waiting_requests_each_get_their_own_response() {
        let inbox = Arc::new(Inbox::default());
        let deadline = Instant::now() + Duration::from_secs(5);
        let ids = [inbox.register(third_byte_is(3)), inbox.register(third_byte_is(4))];
        let waiters = ids.map(|id| {
            let i = Arc::clone(&inbox);
            thread::spawn(move || i.wait_reply(id, deadline))
        });
        // Answered in the opposite order.
        inbox.push(&short(4), Instant::now());
        inbox.push(&short(3), Instant::now());
        let [a, b] = waiters.map(|w| w.join().unwrap().unwrap());
        assert_eq!((a, b), (Some(short(3)), Some(short(4))));
    }

    #[test]
    fn a_request_without_response_times_out_and_a_late_one_is_queued() {
        let inbox = Inbox::default();
        let id = inbox.register(third_byte_is(5));
        assert_eq!(inbox.wait_reply(id, Instant::now() + Duration::from_millis(20)).unwrap(), None);
        assert!(inbox.lock().waiters.is_empty());
        inbox.push(&short(5), Instant::now());
        assert_eq!(inbox.pop(0).unwrap(), Some(short(5)));
    }

    #[test]
    fn interrupt_wakes_a_waiting_read() {
        let inbox = Arc::new(Inbox::default());
        let other = Arc::clone(&inbox);
        let t = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            other.interrupt();
        });
        let start = Instant::now();
        assert_eq!(inbox.pop(5_000).unwrap(), None);
        assert!(start.elapsed() < Duration::from_secs(1));
        t.join().unwrap();
        // Only once: the next read waits again.
        assert_eq!(inbox.pop(10).unwrap(), None);
        assert!(!inbox.lock().interrupted);
        // An interrupt before the read makes it return at once, queued reports first.
        inbox.push(&short(1), Instant::now());
        inbox.interrupt();
        assert_eq!(inbox.pop(5_000).unwrap(), Some(short(1)));
        let start = Instant::now();
        assert_eq!(inbox.pop(5_000).unwrap(), None);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_failed_reader_ends_a_waiting_request() {
        let inbox = Arc::new(Inbox::default());
        let id = inbox.register(third_byte_is(5));
        let reader = Arc::clone(&inbox);
        let t = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            reader.fail("gone".into());
        });
        let e = inbox.wait_reply(id, Instant::now() + Duration::from_secs(5)).unwrap_err();
        assert!(e.downcast_ref::<IoFailed>().is_some());
        assert!(inbox.lock().waiters.is_empty());
        t.join().unwrap();
    }

    #[test]
    fn failure_is_reported_after_the_queued_reports() {
        let inbox = Arc::new(Inbox::default());
        inbox.push(&short(1), Instant::now());
        inbox.fail("device disconnected".into());
        assert_eq!(inbox.pop(-1).unwrap(), Some(short(1)));
        let e = inbox.pop(-1).unwrap_err();
        assert!(e.downcast_ref::<IoFailed>().is_some());
        // A reader failing while the mouse thread waits wakes it up.
        let inbox = Arc::new(Inbox::default());
        let reader = Arc::clone(&inbox);
        let t = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            reader.fail("gone".into());
        });
        assert!(inbox.pop(-1).is_err());
        t.join().unwrap();
    }
}
