//! Small helpers shared by several modules.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Locks `m`; a thread that panicked while holding it must not take the
/// others down, so a poisoned lock is used as is.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Lets a repeating log line through only once per interval.
pub struct Throttle {
    every: Duration,
    last: Option<Instant>,
}

impl Throttle {
    pub fn new(every: Duration) -> Self {
        Throttle { every, last: None }
    }

    pub fn allow(&mut self, now: Instant) -> bool {
        if self.last.is_some_and(|t| now.saturating_duration_since(t) < self.every) {
            return false;
        }
        self.last = Some(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttle_lets_one_line_through_per_interval() {
        let mut t = Throttle::new(Duration::from_secs(10));
        let start = Instant::now();
        assert!(t.allow(start));
        assert!(!t.allow(start + Duration::from_secs(9)));
        assert!(t.allow(start + Duration::from_secs(10)));
        assert!(!t.allow(start + Duration::from_secs(11)));
    }
}
