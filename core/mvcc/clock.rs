use crate::sync::atomic::{AtomicU64, Ordering};
use crate::sync::RwLock;

/// No-op callback for use with [`LogicalClock::get_timestamp`] when no
/// action needs to be taken atomically alongside timestamp generation
/// (e.g. for begin timestamps).
pub fn no_op(_: u64) {}

/// Logical clock.
pub trait LogicalClock: Send + Sync {
    /// Generates the next timestamp, calls `f` with it, then returns it.
    ///
    /// Implementations that guard concurrent commit protocols (e.g.
    /// [`MvccClock`]) hold their internal lock across the `f` call, so
    /// that the timestamp is published (e.g. stored as `Preparing(ts)`)
    /// before any other caller can observe a timestamp.
    ///
    /// Pass [`no_op`] when no atomic side-effect is needed (begin timestamps).
    fn get_timestamp<F: FnOnce(u64)>(&self, f: F) -> u64;
    /// Like [`LogicalClock::get_timestamp`], except that other calls of this
    /// method may run at the same time: only [`LogicalClock::get_timestamp`]
    /// callers wait for `f`.
    fn get_timestamp_beside_other_begins<F: FnOnce(u64)>(&self, f: F) -> u64;
    fn reset(&self, ts: u64);
}

/// A lock-guarded clock for concurrent MVCC use.
///
/// The lock is held across the `f` callback in [`get_timestamp`], ensuring
/// that a commit timestamp is published (e.g. stored as `Preparing(ts)`)
/// before any other transaction can generate a higher timestamp. This closes
/// the TOCTOU window between timestamp generation and `Preparing` state
/// publication in the commit protocol. Begins hold the lock shared, so they
/// run side by side and only wait for, and hold back, the callers of
/// [`get_timestamp`].
///
/// ## Speculative reads
///
/// We have speculative reads (and speculative ignores). That is, an active
/// transaction can see changes of another transaction which is in the
/// **preparing** phase. Assuming the other transaction successfully commits,
/// the active transaction continues to make progress. If the other transaction
/// gets aborted, then the active transaction needs to be aborted as well.
///
/// So, say `tx2` starts at `begin_ts(11)` and another transaction `tx1`,
/// started earlier, is now in its preparing phase with `end_ts(10)`. Once the
/// `end_ts` is assigned, that will be the final commit timestamp of that
/// transaction. So `tx2` should see changes made by `tx1`, since `tx1` was
/// committed (in logical time) before `tx2` started.
///
/// Whether `tx2` can see `tx1`'s changes depends on when `tx1` acquired the
/// `end_ts` timestamp during the preparing phase.
///
/// > **Note:** We need speculative reads, otherwise it's difficult to make
/// > the MVCC model work without blocking. I made an attempt in
/// > [turso#5198](https://github.com/tursodatabase/turso/pull/5198) but this
/// > introduced a subtle bug which violated snapshot isolation. So without speculative
/// > reads in the previous example, `tx2` needs to wait till `tx1` is committed or
/// > aborted.
///
/// ### Need for atomicity
///
/// We want to atomically generate `end_ts` and publish `Preparing(end_ts)`
/// while the clock lock is held. This closes the TOCTOU window.
///
/// Consider the example:
///
/// ```text
/// tx1 (Active):    generates end_ts = 10
/// tx2 (Active):    gets begin_ts = 11
/// tx2 (Active):    does queries but does not see changes by tx1 (tx1 is still Active)
/// tx1 (Preparing): stores Preparing(end_ts=10)
/// tx2 (Active):    queries again, now it can see changes by tx1 (tx1 is now Preparing)
/// ```
///
/// **This is a snapshot isolation violation** — `tx2` observes different
/// values for the same rows within the same transaction.
///
/// So we want the following two operations to be atomic:
///
/// ```text
/// let ts = get_timestamp()
/// store Preparing(ts)
/// ```
///
/// `tx2` must get its begin timestamp either **before** or **after** these
/// two operations. If it interleaves, the above bug happens.
///
/// ## Note on the Hekaton paper
///
/// The Hekaton paper doesn't mention this "gotcha". The paper says:
///
/// > "When the transaction has completed its normal processing and requests
/// > to commit, it acquires an end timestamp and switches to the Preparing
/// > state."
///
/// But it doesn't go into more detail about atomicity here.
#[derive(Debug, Default)]
pub struct MvccClock {
    next: AtomicU64,
    order: RwLock<()>,
}

impl MvccClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Generate a begin timestamp. No side-effect needed alongside generation.
    pub fn get_begin_timestamp(&self) -> u64 {
        self.get_timestamp(no_op)
    }

    /// Generate a commit timestamp and call `f` with it while the lock is
    /// held, atomically publishing the timestamp before releasing.
    pub fn get_commit_timestamp<F: FnOnce(u64)>(&self, f: F) -> u64 {
        self.get_timestamp(f)
    }
}

impl LogicalClock for MvccClock {
    fn get_timestamp<F: FnOnce(u64)>(&self, f: F) -> u64 {
        let _alone = self.order.write();
        let ts = self.next.fetch_add(1, Ordering::AcqRel);
        f(ts);
        ts
    }

    fn get_timestamp_beside_other_begins<F: FnOnce(u64)>(&self, f: F) -> u64 {
        let _with_other_begins = self.order.read();
        let ts = self.next.fetch_add(1, Ordering::AcqRel);
        f(ts);
        ts
    }

    fn reset(&self, ts: u64) {
        let _alone = self.order.write();
        self.next.store(ts, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::{no_op, LogicalClock, MvccClock};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    #[test]
    fn begins_run_side_by_side_and_a_commit_waits_for_them() {
        let clock = Arc::new(MvccClock::new());
        let (inside_send, inside) = mpsc::channel();
        let (let_go, wait_to_go) = mpsc::channel::<()>();
        let first_begin = {
            let clock = Arc::clone(&clock);
            std::thread::spawn(move || {
                clock.get_timestamp_beside_other_begins(|_| {
                    inside_send.send(()).unwrap();
                    wait_to_go.recv().unwrap();
                })
            })
        };
        inside.recv().unwrap();
        let (second_send, second) = mpsc::channel();
        {
            let clock = Arc::clone(&clock);
            std::thread::spawn(move || {
                second_send
                    .send(clock.get_timestamp_beside_other_begins(no_op))
                    .unwrap()
            });
        }
        let second_begin = second
            .recv_timeout(Duration::from_secs(10))
            .expect("a begin must not wait for another begin");

        let commit_drawn = Arc::new(AtomicBool::new(false));
        let commit = {
            let clock = Arc::clone(&clock);
            let commit_drawn = Arc::clone(&commit_drawn);
            std::thread::spawn(move || {
                let ts = clock.get_timestamp(no_op);
                commit_drawn.store(true, Ordering::SeqCst);
                ts
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !commit_drawn.load(Ordering::SeqCst),
            "a commit timestamp must wait for a begin still publishing"
        );

        let_go.send(()).unwrap();
        let first_begin = first_begin.join().unwrap();
        let commit = commit.join().unwrap();
        assert_ne!(first_begin, second_begin);
        assert!(commit > first_begin && commit > second_begin);
    }
}
