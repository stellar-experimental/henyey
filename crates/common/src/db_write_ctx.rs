//! Write-lock hold instrumentation for the SQLite single-writer path.
//!
//! SQLite in WAL mode serializes **all** write transactions through one
//! global write lock. Under the restore-from-disk near-tip back-fill path
//! (see #3702), several logical writers contend for that single lock:
//!
//! - per-slot `scp-persist-{slot}` purge / persist transactions,
//! - back-to-back per-ledger close persists,
//! - the maintainer's row-delete cycle.
//!
//! When one of these pins the write lock for a long time, other writers (e.g.
//! the peer-record writer) log `database is locked` after the busy-timeout,
//! but the log gives no clue *which* logical writer held it. [`WriteCtxGuard`]
//! attaches a stable `db_write_ctx` label + timing to each holder-side write
//! transaction so a long hold becomes self-naming in the logs.
//!
//! This is **instrumentation only** — it changes no transaction scope,
//! ordering, or semantics, and adds no locks. Overhead is a couple of tracing
//! events plus one `Instant::now()` per guarded transaction.

use std::time::{Duration, Instant};

use tracing::{debug, warn};

/// Threshold above which a write-lock hold is considered long enough to warn
/// about. Chosen well below the 30s SQLite busy-timeout so a warn is emitted
/// before a *victim* writer times out with `database is locked`.
pub const LONG_WRITE_HOLD_WARN: Duration = Duration::from_secs(5);

/// RAII guard that times a logical DB write transaction and names its context.
///
/// Construct one at the start of a holder-side write transaction with a stable
/// `context` label (e.g. `"scp-persist-purge slot=63281545"`,
/// `"ledger-close-persist seq=63281546"`, `"maintenance-delete"`). On drop
/// (transaction end), it logs the elapsed hold at `debug!`, and — if the hold
/// exceeded [`LONG_WRITE_HOLD_WARN`] — at `warn!`, naming the context so the
/// next wedge identifies the dominant WAL write-lock holder.
///
/// The `context` is owned (`String`) so callers can embed a slot/seq without
/// juggling lifetimes; construction is off any hot inner loop (once per txn).
#[must_use = "the guard must be held for the duration of the write transaction"]
pub struct WriteCtxGuard {
    context: String,
    start: Instant,
}

impl WriteCtxGuard {
    /// Begin timing a guarded write transaction, emitting a `debug!` start
    /// event tagged with `db_write_ctx=<context>`.
    pub fn new(context: impl Into<String>) -> Self {
        let context = context.into();
        debug!(db_write_ctx = %context, "db write txn begin");
        Self {
            context,
            start: Instant::now(),
        }
    }
}

impl Drop for WriteCtxGuard {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed();
        if elapsed >= LONG_WRITE_HOLD_WARN {
            warn!(
                db_write_ctx = %self.context,
                elapsed_ms = elapsed.as_millis() as u64,
                "db write txn held the WAL write lock a long time \
                 (candidate `database is locked` cause, #3702)"
            );
        } else {
            debug!(
                db_write_ctx = %self.context,
                elapsed_ms = elapsed.as_millis() as u64,
                "db write txn end"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_guard_constructs_and_drops() {
        // Smoke: construction + drop must not panic and must record context.
        let g = WriteCtxGuard::new("test-ctx");
        assert_eq!(g.context, "test-ctx");
        drop(g);
    }

    #[test]
    fn test_guard_accepts_owned_context() {
        let slot = 63281545u64;
        let g = WriteCtxGuard::new(format!("scp-persist-purge slot={slot}"));
        assert!(g.context.contains("63281545"));
    }
}
