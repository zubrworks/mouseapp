//! What wakes the daemon loop: every source sends an [`Event`] through one
//! channel, and the loop sleeps until the next event or its nearest deadline.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Instant;

/// Why the daemon loop woke up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Event {
    /// The settings app queued a command in the hub.
    Command,
    /// A settings file changed.
    Files,
    /// A mouse worker thread ended.
    WorkerEnded,
    /// A Logitech HID device appeared (see [`super::arrival`]).
    Arrived,
    /// The Logitech HID device with this IOKit registry entry ID is gone.
    Removed(u64),
    /// The program is stopping.
    Stop,
}

/// Sending end of the channel, for any thread.
#[derive(Clone)]
pub(super) struct Waker(Sender<Event>);

impl Waker {
    /// Sends `e`; nothing happens if the loop has already ended.
    pub(super) fn send(&self, e: Event) {
        let _ = self.0.send(e);
    }
}

pub(super) fn channel() -> (Waker, Receiver<Event>) {
    let (tx, rx) = mpsc::channel();
    (Waker(tx), rx)
}

/// The next event, or `None` once `deadline` passes.
pub(super) fn wait(rx: &Receiver<Event>, deadline: Instant) -> Option<Event> {
    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(e) => Some(e),
        Err(RecvTimeoutError::Timeout) => None,
        // Cannot happen while the loop holds a sender; stop rather than spin.
        Err(RecvTimeoutError::Disconnected) => Some(Event::Stop),
    }
}

/// Owned by a worker thread: when dropped (also when the worker panics) it
/// marks the worker as ended and sends [`Event::WorkerEnded`]. The thread
/// itself finishes a moment later, so the loop checks the mark, not
/// `JoinHandle::is_finished`.
pub(super) struct EndNotice {
    wake: Waker,
    ended: Arc<AtomicBool>,
}

impl EndNotice {
    /// The notice and the mark it sets.
    pub(super) fn new(wake: Waker) -> (Self, Arc<AtomicBool>) {
        let ended = Arc::new(AtomicBool::new(false));
        (
            EndNotice {
                wake,
                ended: Arc::clone(&ended),
            },
            ended,
        )
    }
}

impl Drop for EndNotice {
    fn drop(&mut self) {
        self.ended.store(true, Ordering::SeqCst);
        self.wake.send(Event::WorkerEnded);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn waits_for_an_event_or_the_deadline() {
        let (tx, rx) = channel();
        tx.send(Event::Files);
        let far = Instant::now() + Duration::from_secs(60);
        assert_eq!(wait(&rx, far), Some(Event::Files));
        let start = Instant::now();
        assert_eq!(wait(&rx, start + Duration::from_millis(20)), None);
        assert!(start.elapsed() >= Duration::from_millis(20));
        // A deadline in the past still delivers what is queued.
        tx.send(Event::Command);
        assert_eq!(wait(&rx, start), Some(Event::Command));
    }

    #[test]
    fn a_panicking_worker_is_reported() {
        let (tx, rx) = channel();
        let (notice, ended) = EndNotice::new(tx);
        let _ = std::thread::spawn(move || {
            let _notice = notice;
            panic!("worker died");
        })
        .join();
        assert!(ended.load(Ordering::SeqCst));
        assert_eq!(rx.try_recv(), Ok(Event::WorkerEnded));
    }
}
