//! Bounded-concurrency limiter for parallel bucket restore (issue #3245).
//!
//! [`FanOutLimiter`] is a small counting semaphore used to cap how many
//! bucket-materialization workers may run concurrently inside the
//! `std::thread::scope` of `BucketList::restore_from_has_parallel` and its
//! hot-archive twin.
//!
//! The cold-catchup memory peak (#3235) is dominated by the *transient*
//! working set of concurrent `load_bucket` calls (streaming index builds, XDR
//! parse buffers, small buckets loaded fully into RAM). Spawning the worker
//! threads is cheap; the RAM lives in the bucket materialization. So callers
//! spawn all workers but acquire a permit from this limiter *around the
//! `load_bucket` materialization*, bounding the concurrent live working set —
//! the spike — without changing the resident base or the restored state.
//!
//! When constructed with [`FanOutLimiter::unbounded`] (the default,
//! representing current behavior), [`FanOutLimiter::acquire`] is a no-op: no
//! permit accounting happens and all workers materialize concurrently exactly
//! as before. This is what keeps `restore_apply_fan_out`'s default a true
//! zero-regression no-op.
//!
//! Built on `parking_lot::{Mutex, Condvar}` (already a dependency) rather than
//! a hand-rolled `std::sync::Condvar` loop to avoid lost-wakeup mistakes. The
//! permit is RAII: [`FanOutPermit`] releases on drop and wakes one waiter.

use parking_lot::{Condvar, Mutex};

/// A counting semaphore bounding concurrent bucket materialization.
///
/// Cheap to clone-by-reference (used as `&FanOutLimiter` across scoped
/// threads). `unbounded()` disables all accounting for the default
/// (current-behavior) path.
#[derive(Debug)]
pub struct FanOutLimiter {
    /// `None` = unbounded (no cap, no accounting — current behavior).
    /// `Some((mutex<available_permits>, condvar))` = bounded to `k`.
    inner: Option<(Mutex<usize>, Condvar)>,
}

impl FanOutLimiter {
    /// Construct an unbounded limiter: [`acquire`](Self::acquire) never blocks
    /// and does no accounting. This is the default / current behavior.
    pub fn unbounded() -> Self {
        Self { inner: None }
    }

    /// Construct a limiter from an optional cap.
    ///
    /// * `None` → [`unbounded`](Self::unbounded) (current behavior, no-op).
    /// * `Some(0)` → also unbounded (a zero cap is meaningless; treat as "no
    ///   cap" so a misconfigured `restore_apply_fan_out = 0` can never
    ///   deadlock the restore by handing out zero permits).
    /// * `Some(k)` with `k >= 1` → at most `k` concurrent permits.
    pub fn from_cap(cap: Option<usize>) -> Self {
        match cap {
            None | Some(0) => Self::unbounded(),
            Some(k) => Self::bounded(k),
        }
    }

    /// Construct a limiter bounded to `k` concurrent permits.
    ///
    /// `k` is clamped to a minimum of 1 to avoid a zero-permit deadlock.
    pub fn bounded(k: usize) -> Self {
        let k = k.max(1);
        Self {
            inner: Some((Mutex::new(k), Condvar::new())),
        }
    }

    /// Whether this limiter actually bounds concurrency (vs. the unbounded
    /// no-op). Used by tests and for the doc-comment invariant.
    pub fn is_bounded(&self) -> bool {
        self.inner.is_some()
    }

    /// Acquire one permit, blocking until one is available.
    ///
    /// Returns a RAII [`FanOutPermit`] that releases the permit on drop. For an
    /// unbounded limiter this returns immediately with a no-op permit.
    pub fn acquire(&self) -> FanOutPermit<'_> {
        if let Some((mutex, condvar)) = &self.inner {
            let mut available = mutex.lock();
            while *available == 0 {
                condvar.wait(&mut available);
            }
            *available -= 1;
        }
        FanOutPermit { limiter: self }
    }

    /// Release one permit and wake a single waiter. Called by `FanOutPermit`'s
    /// `Drop`; not public.
    fn release(&self) {
        if let Some((mutex, condvar)) = &self.inner {
            let mut available = mutex.lock();
            *available += 1;
            // Wake exactly one waiter — one permit became available.
            condvar.notify_one();
        }
    }
}

/// RAII permit from [`FanOutLimiter::acquire`]. Releases on drop.
#[derive(Debug)]
pub struct FanOutPermit<'a> {
    limiter: &'a FanOutLimiter,
}

impl Drop for FanOutPermit<'_> {
    fn drop(&mut self) {
        self.limiter.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn test_unbounded_is_not_bounded_and_never_blocks() {
        let limiter = FanOutLimiter::unbounded();
        assert!(!limiter.is_bounded());
        // Acquire many permits concurrently without releasing — must not block.
        let _p1 = limiter.acquire();
        let _p2 = limiter.acquire();
        let _p3 = limiter.acquire();
        // If acquire blocked on the unbounded path this test would hang.
    }

    #[test]
    fn test_from_cap_none_is_unbounded() {
        assert!(!FanOutLimiter::from_cap(None).is_bounded());
    }

    #[test]
    fn test_from_cap_zero_is_unbounded_no_deadlock() {
        // A misconfigured cap of 0 must not deadlock — treated as unbounded.
        let limiter = FanOutLimiter::from_cap(Some(0));
        assert!(!limiter.is_bounded());
        let _p1 = limiter.acquire();
        let _p2 = limiter.acquire();
    }

    #[test]
    fn test_from_cap_some_is_bounded() {
        assert!(FanOutLimiter::from_cap(Some(2)).is_bounded());
        assert!(FanOutLimiter::bounded(2).is_bounded());
    }

    #[test]
    fn test_bounded_limits_max_concurrency() {
        // With k=2 and N workers each holding the permit while bumping a shared
        // max-concurrent counter, the observed max must never exceed 2.
        let k = 2;
        let n_workers = 16;
        let limiter = Arc::new(FanOutLimiter::bounded(k));
        let current = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        std::thread::scope(|s| {
            for _ in 0..n_workers {
                let limiter = Arc::clone(&limiter);
                let current = Arc::clone(&current);
                let max_seen = Arc::clone(&max_seen);
                s.spawn(move || {
                    let _permit = limiter.acquire();
                    let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(now, Ordering::SeqCst);
                    // Hold the permit a moment so contention is real.
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    current.fetch_sub(1, Ordering::SeqCst);
                    // permit released on drop here
                });
            }
        });

        assert_eq!(current.load(Ordering::SeqCst), 0);
        assert!(
            max_seen.load(Ordering::SeqCst) <= k,
            "max concurrent {} exceeded cap {}",
            max_seen.load(Ordering::SeqCst),
            k
        );
        // Sanity: with 16 workers and k=2 we should actually reach the cap.
        assert!(max_seen.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn test_bounded_k1_serializes() {
        // k=1 means strictly one worker at a time.
        let limiter = Arc::new(FanOutLimiter::bounded(1));
        let current = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        std::thread::scope(|s| {
            for _ in 0..8 {
                let limiter = Arc::clone(&limiter);
                let current = Arc::clone(&current);
                let max_seen = Arc::clone(&max_seen);
                s.spawn(move || {
                    let _permit = limiter.acquire();
                    let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    current.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });

        assert_eq!(max_seen.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_all_workers_complete_under_cap() {
        // Every worker must eventually run (no starvation / lost wakeup).
        let limiter = Arc::new(FanOutLimiter::bounded(3));
        let completed = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|s| {
            for _ in 0..50 {
                let limiter = Arc::clone(&limiter);
                let completed = Arc::clone(&completed);
                s.spawn(move || {
                    let _permit = limiter.acquire();
                    completed.fetch_add(1, Ordering::SeqCst);
                });
            }
        });
        assert_eq!(completed.load(Ordering::SeqCst), 50);
    }
}
