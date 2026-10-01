//! Wakes connections that wait for a WAL lock another connection of this
//! process holds, the moment that connection lets go of it.
//!
//! A statement that finds the write lock taken waits out a busy-handler delay
//! before it tries again: 1 ms, then 2, 5, 10 and on up to 100 ms. A lock held
//! for a fraction of a millisecond is mostly released early in that delay, and
//! then nobody holds it until the waiters wake. Measured with eight sessions
//! inserting into one table, the waiters' clock reads were 80% of the server's
//! CPU and the throughput was half of one session's.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

/// Counts lock releases, and wakes the connections waiting for one.
///
/// A waiter reads [`Self::releases`] before it tries the lock and passes what
/// it read to [`Self::wait_for_release_after`], so a release that happens
/// between the failed try and the wait is not missed.
#[derive(Debug, Default)]
pub struct LockReleaseSignal {
    releases: AtomicU64,
    waiters: AtomicUsize,
    #[cfg(not(any(shuttle, target_family = "wasm")))]
    sleeping: std::sync::Mutex<()>,
    #[cfg(not(any(shuttle, target_family = "wasm")))]
    woken: std::sync::Condvar,
}

impl LockReleaseSignal {
    pub fn releases(&self) -> u64 {
        self.releases.load(Ordering::SeqCst)
    }

    /// Called after a lock was released.
    pub fn released(&self) {
        self.releases.fetch_add(1, Ordering::SeqCst);
        // Either a waiter counted itself before this load and is woken
        // below, or it counts itself after the increment above and sees the
        // release before it sleeps.
        if self.waiters.load(Ordering::SeqCst) == 0 {
            return;
        }
        #[cfg(not(any(shuttle, target_family = "wasm")))]
        {
            // Taking the mutex orders this wake after a waiter's check of the
            // counter, which it makes holding the mutex.
            drop(self.sleeping.lock().unwrap_or_else(|e| e.into_inner()));
            self.woken.notify_all();
        }
    }

    /// Waits until a lock was released after `seen` was read from
    /// [`Self::releases`], or until `timeout` passes. Returns whether one was
    /// released, or `None` where this platform cannot block a thread.
    pub fn wait_for_release_after(&self, seen: u64, timeout: Duration) -> Option<bool> {
        #[cfg(any(shuttle, target_family = "wasm"))]
        {
            let _ = (seen, timeout);
            None
        }
        #[cfg(not(any(shuttle, target_family = "wasm")))]
        {
            let deadline = std::time::Instant::now() + timeout;
            self.waiters.fetch_add(1, Ordering::SeqCst);
            let mut sleeping = self.sleeping.lock().unwrap_or_else(|e| e.into_inner());
            let released = loop {
                if self.releases.load(Ordering::SeqCst) != seen {
                    break true;
                }
                let now = std::time::Instant::now();
                if now >= deadline {
                    break false;
                }
                sleeping = self
                    .woken
                    .wait_timeout(sleeping, deadline - now)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            };
            drop(sleeping);
            self.waiters.fetch_sub(1, Ordering::SeqCst);
            Some(released)
        }
    }
}

#[cfg(all(test, not(any(shuttle, target_family = "wasm"))))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    #[test]
    fn a_release_before_the_wait_is_not_missed() {
        let signal = LockReleaseSignal::default();
        let seen = signal.releases();
        signal.released();
        let started = Instant::now();
        assert_eq!(
            signal.wait_for_release_after(seen, Duration::from_secs(10)),
            Some(true)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_waiter_wakes_when_the_lock_is_released_not_when_its_delay_ends() {
        let signal = Arc::new(LockReleaseSignal::default());
        let seen = signal.releases();
        let releaser = {
            let signal = Arc::clone(&signal);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                signal.released();
            })
        };
        let started = Instant::now();
        assert_eq!(
            signal.wait_for_release_after(seen, Duration::from_secs(10)),
            Some(true)
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        releaser.join().unwrap();
    }

    #[test]
    fn a_waiter_gives_up_after_its_delay_when_nothing_is_released() {
        let signal = LockReleaseSignal::default();
        let seen = signal.releases();
        assert_eq!(
            signal.wait_for_release_after(seen, Duration::from_millis(5)),
            Some(false)
        );
    }
}
