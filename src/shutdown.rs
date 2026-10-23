//! Process-wide stop request: set by Ctrl-C / SIGTERM or by [`request_stop`].
//! Threads that sleep until something happens register a waker with
//! [`on_stop`] so the request reaches them at once.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, Once};

const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;
/// `how` of `pthread_sigmask` on macOS.
const SIG_BLOCK: i32 = 1;
const SIG_UNBLOCK: i32 = 2;

/// macOS `sigset_t`: one bit per signal, bit `n - 1` for signal `n`.
type SigSet = u32;
const STOP_SIGNALS: SigSet = 1 << (SIGINT - 1) | 1 << (SIGTERM - 1);

unsafe extern "C" {
    fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
    fn pthread_sigmask(how: i32, set: *const SigSet, old: *mut SigSet) -> i32;
    fn sigwait(set: *const SigSet, sig: *mut i32) -> i32;
}

type Wake = Box<dyn Fn() + Send>;

/// Stop flag and the wakers of the threads waiting for it.
struct Stopper {
    flag: AtomicBool,
    /// Wakers with their ids (for [`StopWaker`]).
    wakers: Mutex<Vec<(u64, Wake)>>,
    next_id: AtomicU64,
}

impl Stopper {
    const fn new() -> Self {
        Stopper {
            flag: AtomicBool::new(false),
            wakers: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(0),
        }
    }

    fn stopped(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    fn request(&self) {
        self.flag.store(true, Ordering::SeqCst);
        for (_, wake) in crate::util::lock(&self.wakers).iter() {
            wake();
        }
    }

    /// Registers `wake`; returns its id.
    fn on_stop(&self, wake: Wake) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut wakers = crate::util::lock(&self.wakers);
        // Checked under the lock: a request either sees this waker or set
        // the flag before this check.
        if self.stopped() {
            drop(wakers);
            wake();
        } else {
            wakers.push((id, wake));
        }
        id
    }

    fn remove(&self, id: u64) {
        crate::util::lock(&self.wakers).retain(|(i, _)| *i != id);
    }
}

static STOPPER: Stopper = Stopper::new();
static INSTALL: Once = Once::new();

/// Fallback for a thread that was started before the signals were blocked:
/// only sets the flag (a waker may be unsafe in a signal handler).
extern "C" fn on_signal(_: i32) {
    STOPPER.flag.store(true, Ordering::SeqCst);
}

/// Makes Ctrl-C and SIGTERM request a stop. Call before starting any other
/// thread: threads inherit the blocked signals, so only the `signals` thread
/// receives them and can wake everyone. Later calls do nothing.
pub fn install_signal_handlers() {
    INSTALL.call_once(|| {
        // SAFETY: the handler only stores to an atomic, which is
        // async-signal-safe; the mask is a valid macOS `sigset_t`.
        unsafe {
            signal(SIGINT, on_signal);
            signal(SIGTERM, on_signal);
            if pthread_sigmask(SIG_BLOCK, &STOP_SIGNALS, std::ptr::null_mut()) != 0 {
                log::warn!("cannot block the stop signals; Ctrl-C only sets the flag");
                return;
            }
        }
        let res = std::thread::Builder::new()
            .name("signals".into())
            .spawn(|| {
                loop {
                    let mut sig = 0;
                    // SAFETY: valid set and output pointer; the signals are
                    // blocked in this thread (inherited), as `sigwait` requires.
                    if unsafe { sigwait(&STOP_SIGNALS, &mut sig) } != 0 {
                        log::error!("sigwait failed; Ctrl-C only sets the flag");
                        return;
                    }
                    log::info!("signal {sig}: stopping");
                    request_stop();
                }
            });
        if let Err(e) = res {
            log::error!("cannot start the signal thread: {e}");
            // SAFETY: as above; the handler takes over.
            unsafe { pthread_sigmask(SIG_UNBLOCK, &STOP_SIGNALS, std::ptr::null_mut()) };
        }
    });
}

/// True once a stop was requested (Ctrl-C, SIGTERM or [`request_stop`]).
pub fn stopped() -> bool {
    STOPPER.stopped()
}

/// Asks every loop to finish, as Ctrl-C does, and wakes the threads that
/// registered with [`on_stop`].
pub fn request_stop() {
    STOPPER.request();
}

/// Registers `wake`, called (on the stopping thread) when a stop is
/// requested, or at once if it already was. It must be quick and must not
/// call [`on_stop`].
pub fn on_stop(wake: impl Fn() + Send + 'static) {
    STOPPER.on_stop(Box::new(wake));
}

/// A waker registered with [`on_stop_while`]; removed when dropped.
#[must_use = "the waker is removed when this is dropped"]
pub struct StopWaker(u64);

impl Drop for StopWaker {
    fn drop(&mut self) {
        STOPPER.remove(self.0);
    }
}

/// Like [`on_stop`], for a thread that waits only for a while (one mouse
/// session): `wake` is dropped with the returned guard.
pub fn on_stop_while(wake: impl Fn() + Send + 'static) -> StopWaker {
    StopWaker(STOPPER.on_stop(Box::new(wake)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn wakers_run_on_request_or_at_once_when_late() {
        let s = Stopper::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        s.on_stop(Box::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(!s.stopped());
        s.request();
        assert!(s.stopped());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let c = Arc::clone(&calls);
        s.on_stop(Box::new(move || {
            c.fetch_add(10, Ordering::SeqCst);
        }));
        assert_eq!(calls.load(Ordering::SeqCst), 11);
    }

    #[test]
    fn a_removed_waker_is_not_called() {
        let s = Stopper::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let (a, b) = (Arc::clone(&calls), Arc::clone(&calls));
        let first = s.on_stop(Box::new(move || {
            a.fetch_add(1, Ordering::SeqCst);
        }));
        s.on_stop(Box::new(move || {
            b.fetch_add(10, Ordering::SeqCst);
        }));
        s.remove(first);
        s.request();
        assert_eq!(calls.load(Ordering::SeqCst), 10);
        assert_eq!(crate::util::lock(&s.wakers).len(), 1);
    }
}
