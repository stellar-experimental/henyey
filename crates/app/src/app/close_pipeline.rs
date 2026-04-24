//! State machine for the ledger close + persist pipeline.
//!
//! The event loop manages a two-phase pipeline: first a ledger close runs on
//! a blocking thread, then its persist job (DB writes + bucket flush) runs on
//! another blocking thread. The invariant is that a new close cannot start
//! until the previous persist completes.
//!
//! This module encapsulates the pipeline state and provides methods that
//! enforce valid transitions with `debug_assert!` checks.

use super::types::{PendingLedgerClose, PendingPersist};

/// State machine for the close + persist pipeline.
///
/// # Valid States
///
/// | State       | `closing`  | `persisting` | Description                          |
/// |-------------|-----------|--------------|--------------------------------------|
/// | Idle        | `None`    | `None`       | Ready for a new close                |
/// | Closing     | `Some`    | `None`       | Close running on blocking thread     |
/// | Persisting  | `None`    | `Some`       | Persist running on blocking thread   |
///
/// # Invalid State
///
/// `closing=Some, persisting=Some` — both active simultaneously — is prevented
/// by the API. Every method that sets one field asserts the other is `None`.
///
/// # Transitions
///
/// ```text
/// Idle → Closing       (start_close / try_start_close)
/// Closing → Idle       (take_close — temporary, caller installs persist next)
/// Idle → Persisting    (start_persist — after close or from catchup)
/// Persisting → Idle    (take_persist)
/// ```
///
/// # Select! Access
///
/// Fields are `pub(super)` to allow `tokio::select!` to borrow `.closing` and
/// `.persisting` independently (they occupy different memory locations). All
/// state mutations must go through methods — direct field assignment outside
/// this module is a logic error.
pub(super) struct ClosePipeline {
    /// In-progress background ledger close. Polled in the select loop.
    pub(super) closing: Option<PendingLedgerClose>,
    /// In-progress background persist (DB writes + bucket flush).
    /// Gated: next close won't start until persist completes.
    pub(super) persisting: Option<PendingPersist>,
}

impl ClosePipeline {
    /// Create a new idle pipeline.
    pub fn new() -> Self {
        Self {
            closing: None,
            persisting: None,
        }
    }

    /// True when no close or persist is in progress. A new close can start.
    pub fn is_idle(&self) -> bool {
        self.closing.is_none() && self.persisting.is_none()
    }

    /// Transition Idle → Closing.
    ///
    /// # Panics (debug only)
    /// Panics if the pipeline is not idle.
    pub fn start_close(&mut self, pending: PendingLedgerClose) {
        debug_assert!(
            self.is_idle(),
            "start_close: pipeline not idle (closing={}, persisting={})",
            self.closing.as_ref().map(|c| c.ledger_seq).unwrap_or(0),
            self.persisting.as_ref().map(|p| p.ledger_seq).unwrap_or(0),
        );
        self.closing = Some(pending);
    }

    /// Convenience: start a close only if `pending` is `Some`.
    ///
    /// # Panics (debug only)
    /// Panics if `pending` is `Some` and the pipeline is not idle.
    pub fn try_start_close(&mut self, pending: Option<PendingLedgerClose>) {
        if let Some(p) = pending {
            self.start_close(p);
        }
    }

    /// Transition Closing → Idle. Returns the `PendingLedgerClose` for
    /// consumption by `handle_close_complete`.
    ///
    /// After calling this, the pipeline is temporarily idle. The caller
    /// must follow with `start_persist()` (on success) or leave idle.
    ///
    /// # Panics
    /// Panics if not in the Closing state.
    pub fn take_close(&mut self) -> PendingLedgerClose {
        self.closing.take().expect("take_close: no close pending")
    }

    /// Transition Idle → Persisting.
    ///
    /// Used after close completion (normal path) and after catchup
    /// completion (direct Idle → Persisting path).
    ///
    /// # Panics (debug only)
    /// Panics if a close or persist is already in progress.
    pub fn start_persist(&mut self, persist: PendingPersist) {
        debug_assert!(
            self.closing.is_none(),
            "start_persist: close still pending (seq={})",
            self.closing.as_ref().map(|c| c.ledger_seq).unwrap_or(0),
        );
        debug_assert!(
            self.persisting.is_none(),
            "start_persist: persist already pending (seq={})",
            self.persisting.as_ref().map(|p| p.ledger_seq).unwrap_or(0),
        );
        self.persisting = Some(persist);
    }

    /// Transition Persisting → Idle. Returns the `PendingPersist`.
    ///
    /// # Panics
    /// Panics if not in the Persisting state.
    pub fn take_persist(&mut self) -> PendingPersist {
        self.persisting
            .take()
            .expect("take_persist: no persist pending")
    }

    /// The ledger sequence of the active persist, if any (for logging).
    #[allow(dead_code)]
    pub fn persist_ledger_seq(&self) -> Option<u32> {
        self.persisting.as_ref().map(|p| p.ledger_seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::task::JoinHandle;

    /// Create a dummy PendingLedgerClose for testing.
    fn dummy_close(seq: u32) -> PendingLedgerClose {
        use henyey_common::Hash256;

        // Spawn a trivial task that returns immediately.
        let handle: JoinHandle<Result<henyey_ledger::LedgerCloseResult, String>> =
            tokio::task::spawn_blocking(|| Err("dummy".to_string()));
        PendingLedgerClose {
            handle,
            ledger_seq: seq,
            tx_set: henyey_herder::TransactionSet::new_legacy(Hash256::default(), Vec::new()),
            close_time: 0,
            upgrades: Vec::new(),
            dispatch_time: std::time::Instant::now(),
        }
    }

    /// Create a dummy PendingPersist for testing.
    fn dummy_persist(seq: u32) -> PendingPersist {
        let handle: JoinHandle<()> = tokio::task::spawn_blocking(|| {});
        PendingPersist {
            handle,
            ledger_seq: seq,
            dispatch_time: std::time::Instant::now(),
        }
    }

    #[tokio::test]
    async fn test_new_is_idle() {
        let pipeline = ClosePipeline::new();
        assert!(pipeline.is_idle());
    }

    #[tokio::test]
    async fn test_start_close_from_idle() {
        let mut pipeline = ClosePipeline::new();
        pipeline.start_close(dummy_close(100));
        assert!(!pipeline.is_idle());
        assert!(pipeline.closing.is_some());
        assert!(pipeline.persisting.is_none());
    }

    #[tokio::test]
    #[should_panic(expected = "start_close: pipeline not idle")]
    async fn test_start_close_panics_when_closing() {
        let mut pipeline = ClosePipeline::new();
        pipeline.start_close(dummy_close(100));
        pipeline.start_close(dummy_close(101)); // should panic
    }

    #[tokio::test]
    #[should_panic(expected = "start_close: pipeline not idle")]
    async fn test_start_close_panics_when_persisting() {
        let mut pipeline = ClosePipeline::new();
        pipeline.start_persist(dummy_persist(100));
        pipeline.start_close(dummy_close(101)); // should panic
    }

    #[tokio::test]
    async fn test_take_close_returns_pending() {
        let mut pipeline = ClosePipeline::new();
        pipeline.start_close(dummy_close(42));
        let pending = pipeline.take_close();
        assert_eq!(pending.ledger_seq, 42);
        assert!(pipeline.is_idle());
    }

    #[tokio::test]
    #[should_panic(expected = "take_close: no close pending")]
    async fn test_take_close_panics_when_idle() {
        let mut pipeline = ClosePipeline::new();
        pipeline.take_close(); // should panic
    }

    #[tokio::test]
    async fn test_start_persist_from_idle() {
        let mut pipeline = ClosePipeline::new();
        pipeline.start_persist(dummy_persist(50));
        assert!(!pipeline.is_idle());
        assert!(pipeline.closing.is_none());
        assert!(pipeline.persisting.is_some());
    }

    #[tokio::test]
    #[should_panic(expected = "start_persist: persist already pending")]
    async fn test_start_persist_panics_when_persisting() {
        let mut pipeline = ClosePipeline::new();
        pipeline.start_persist(dummy_persist(50));
        pipeline.start_persist(dummy_persist(51)); // should panic
    }

    #[tokio::test]
    async fn test_take_persist_returns_pending() {
        let mut pipeline = ClosePipeline::new();
        pipeline.start_persist(dummy_persist(77));
        let persist = pipeline.take_persist();
        assert_eq!(persist.ledger_seq, 77);
        assert!(pipeline.is_idle());
    }

    #[tokio::test]
    #[should_panic(expected = "take_persist: no persist pending")]
    async fn test_take_persist_panics_when_idle() {
        let mut pipeline = ClosePipeline::new();
        pipeline.take_persist(); // should panic
    }

    #[tokio::test]
    async fn test_full_close_persist_cycle() {
        let mut pipeline = ClosePipeline::new();
        assert!(pipeline.is_idle());

        // Idle → Closing
        pipeline.start_close(dummy_close(10));
        assert!(!pipeline.is_idle());

        // Closing → Idle (take)
        let _pending = pipeline.take_close();
        assert!(pipeline.is_idle());

        // Idle → Persisting
        pipeline.start_persist(dummy_persist(10));
        assert!(!pipeline.is_idle());

        // Persisting → Idle (take)
        let _persist = pipeline.take_persist();
        assert!(pipeline.is_idle());
    }

    #[tokio::test]
    async fn test_try_start_close_none_is_noop() {
        let mut pipeline = ClosePipeline::new();
        pipeline.try_start_close(None);
        assert!(pipeline.is_idle());
    }

    #[tokio::test]
    async fn test_try_start_close_some() {
        let mut pipeline = ClosePipeline::new();
        pipeline.try_start_close(Some(dummy_close(99)));
        assert!(!pipeline.is_idle());
        assert_eq!(pipeline.closing.as_ref().unwrap().ledger_seq, 99);
    }

    #[tokio::test]
    async fn test_persist_ledger_seq() {
        let mut pipeline = ClosePipeline::new();
        assert_eq!(pipeline.persist_ledger_seq(), None);
        pipeline.start_persist(dummy_persist(123));
        assert_eq!(pipeline.persist_ledger_seq(), Some(123));
    }
}
