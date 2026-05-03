//! State machine for the ledger close + persist pipeline.
//!
//! The event loop manages a two-phase pipeline: first a ledger close runs on
//! a blocking thread, then its persist job (DB writes + bucket flush) runs on
//! another blocking thread. The invariant is that a new close cannot start
//! until the previous persist completes.
//!
//! This module encapsulates the pipeline state using a private enum that makes
//! the invalid "both active" state unrepresentable at the type level.

use super::types::{PendingLedgerClose, PendingPersist};
use henyey_ledger::LedgerCloseResult;
use tokio::task::JoinError;

/// Internal pipeline state. Private to this module — cannot be constructed
/// or pattern-matched from outside.
enum State {
    /// No operation in progress. Ready for a new close.
    Idle,
    /// A ledger close is running on a blocking thread.
    Closing(PendingLedgerClose),
    /// A persist (DB writes + bucket flush) is running on a blocking thread.
    Persisting(PendingPersist),
}

/// Event produced when [`ClosePipeline::poll_completion`] resolves.
pub(super) enum PipelineEvent {
    /// The close handle resolved. Caller must follow with [`ClosePipeline::take_close`].
    CloseComplete(Box<Result<Result<LedgerCloseResult, henyey_ledger::LedgerError>, JoinError>>),
    /// The persist handle resolved. Caller must follow with [`ClosePipeline::take_persist`].
    PersistComplete(Result<(), JoinError>),
}

/// State machine for the close + persist pipeline.
///
/// # States
///
/// | Variant      | Description                          |
/// |-------------|--------------------------------------|
/// | `Idle`       | Ready for a new close                |
/// | `Closing`    | Close running on blocking thread     |
/// | `Persisting` | Persist running on blocking thread   |
///
/// The invalid "both active" state is unrepresentable — the enum can only
/// hold one variant at a time.
///
/// # Transitions
///
/// ```text
/// Idle → Closing       (start_close / try_start_close)
/// Closing → Idle       (take_close — caller installs persist next)
/// Idle → Persisting    (start_persist — after close or from catchup)
/// Persisting → Idle    (take_persist)
/// ```
///
/// # Select! Integration
///
/// Use [`poll_completion()`](Self::poll_completion) in `tokio::select!` to
/// await whichever operation is currently in progress. The method is
/// cancel-safe: if the future is dropped, the pipeline state is unchanged.
pub(super) struct ClosePipeline {
    state: State,
}

impl ClosePipeline {
    /// Create a new idle pipeline.
    pub(super) fn new() -> Self {
        Self { state: State::Idle }
    }

    /// True when no close or persist is in progress. A new close can start.
    pub(super) fn is_idle(&self) -> bool {
        matches!(self.state, State::Idle)
    }

    /// True when a close is in progress.
    pub(super) fn is_closing(&self) -> bool {
        matches!(self.state, State::Closing(_))
    }

    /// True when a persist is in progress.
    pub(super) fn is_persisting(&self) -> bool {
        matches!(self.state, State::Persisting(_))
    }

    /// Transition Idle → Closing.
    ///
    /// # Panics
    /// Panics if the pipeline is not idle.
    pub(super) fn start_close(&mut self, pending: PendingLedgerClose) {
        assert!(
            self.is_idle(),
            "start_close: pipeline not idle (state={})",
            self.state_name(),
        );
        self.state = State::Closing(pending);
    }

    /// Convenience: start a close only if `pending` is `Some`.
    ///
    /// # Panics
    /// Panics if `pending` is `Some` and the pipeline is not idle.
    pub(super) fn try_start_close(&mut self, pending: Option<PendingLedgerClose>) {
        if let Some(p) = pending {
            self.start_close(p);
        }
    }

    /// Transition Closing → Idle. Returns the `PendingLedgerClose` for
    /// consumption by `handle_close_complete`.
    ///
    /// After calling this, the pipeline is idle. The caller must follow
    /// with `start_persist()` (on success) or leave idle.
    ///
    /// # Panics
    /// Panics if not in the Closing state.
    pub(super) fn take_close(&mut self) -> PendingLedgerClose {
        match std::mem::replace(&mut self.state, State::Idle) {
            State::Closing(p) => p,
            other => {
                self.state = other;
                panic!(
                    "take_close: not in Closing state (state={})",
                    self.state_name()
                );
            }
        }
    }

    /// Transition Idle → Persisting.
    ///
    /// Used after close completion (normal path) and after catchup
    /// completion (direct Idle → Persisting path).
    ///
    /// # Panics
    /// Panics if the pipeline is not idle.
    pub(super) fn start_persist(&mut self, persist: PendingPersist) {
        assert!(
            self.is_idle(),
            "start_persist: pipeline not idle (state={})",
            self.state_name(),
        );
        self.state = State::Persisting(persist);
    }

    /// Transition Persisting → Idle. Returns the `PendingPersist`.
    ///
    /// # Panics
    /// Panics if not in the Persisting state.
    pub(super) fn take_persist(&mut self) -> PendingPersist {
        match std::mem::replace(&mut self.state, State::Idle) {
            State::Persisting(p) => p,
            other => {
                self.state = other;
                panic!(
                    "take_persist: not in Persisting state (state={})",
                    self.state_name()
                );
            }
        }
    }

    /// The ledger sequence of the active persist, if any (for logging).
    #[allow(dead_code)]
    pub(super) fn persist_ledger_seq(&self) -> Option<u32> {
        match &self.state {
            State::Persisting(p) => Some(p.ledger_seq),
            _ => None,
        }
    }

    /// Await completion of the current in-flight operation.
    ///
    /// - **Closing:** awaits the close handle, returns `CloseComplete`. State
    ///   remains `Closing` — caller must follow with [`take_close()`](Self::take_close).
    /// - **Persisting:** awaits the persist handle, returns `PersistComplete`.
    ///   State remains `Persisting` — caller must follow with [`take_persist()`](Self::take_persist).
    /// - **Idle:** returns a future that never resolves. Designed for use in
    ///   `tokio::select!` where other branches will fire instead.
    ///
    /// # Cancel-safety
    ///
    /// Cancel-safe. If the future is dropped before resolving (e.g., another
    /// `select!` branch fires), the pipeline state is unchanged. `JoinHandle`
    /// is itself cancel-safe (can be polled, dropped, re-polled).
    pub(super) async fn poll_completion(&mut self) -> PipelineEvent {
        match &mut self.state {
            State::Idle => std::future::pending().await,
            State::Closing(p) => {
                let result = (&mut p.handle).await;
                PipelineEvent::CloseComplete(Box::new(result))
            }
            State::Persisting(p) => {
                let result = (&mut p.handle).await;
                PipelineEvent::PersistComplete(result)
            }
        }
    }

    /// Human-readable state name for panic messages.
    fn state_name(&self) -> &'static str {
        match &self.state {
            State::Idle => "Idle",
            State::Closing(_) => "Closing",
            State::Persisting(_) => "Persisting",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::task::JoinHandle;

    /// Create a dummy PendingLedgerClose for testing (completes immediately).
    fn dummy_close(seq: u32) -> PendingLedgerClose {
        use henyey_common::Hash256;

        let handle: JoinHandle<
            Result<henyey_ledger::LedgerCloseResult, henyey_ledger::LedgerError>,
        > = tokio::task::spawn_blocking(|| {
            Err(henyey_ledger::LedgerError::Internal("dummy".to_string()))
        });
        PendingLedgerClose {
            handle,
            ledger_seq: seq,
            tx_set: henyey_herder::TransactionSet::new_legacy(Hash256::default(), Vec::new()),
            close_time: 0,
            upgrades: Vec::new(),
            dispatch_time: std::time::Instant::now(),
        }
    }

    /// Create a blocked PendingLedgerClose that only completes when `tx` is sent.
    fn blocked_close(seq: u32) -> (PendingLedgerClose, tokio::sync::oneshot::Sender<()>) {
        use henyey_common::Hash256;

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let handle: JoinHandle<
            Result<henyey_ledger::LedgerCloseResult, henyey_ledger::LedgerError>,
        > = tokio::task::spawn_blocking(move || {
            rx.blocking_recv().ok();
            Err(henyey_ledger::LedgerError::Internal("dummy".to_string()))
        });
        let pending = PendingLedgerClose {
            handle,
            ledger_seq: seq,
            tx_set: henyey_herder::TransactionSet::new_legacy(Hash256::default(), Vec::new()),
            close_time: 0,
            upgrades: Vec::new(),
            dispatch_time: std::time::Instant::now(),
        };
        (pending, tx)
    }

    /// Create a dummy PendingPersist for testing (completes immediately).
    fn dummy_persist(seq: u32) -> PendingPersist {
        let handle: JoinHandle<()> = tokio::task::spawn_blocking(|| {});
        PendingPersist {
            handle,
            ledger_seq: seq,
            dispatch_time: std::time::Instant::now(),
        }
    }

    /// Create a blocked PendingPersist that only completes when `tx` is sent.
    fn blocked_persist(seq: u32) -> (PendingPersist, tokio::sync::oneshot::Sender<()>) {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            rx.await.ok();
        });
        let pending = PendingPersist {
            handle,
            ledger_seq: seq,
            dispatch_time: std::time::Instant::now(),
        };
        (pending, tx)
    }

    #[tokio::test]
    async fn test_new_is_idle() {
        let pipeline = ClosePipeline::new();
        assert!(pipeline.is_idle());
        assert!(!pipeline.is_closing());
        assert!(!pipeline.is_persisting());
    }

    #[tokio::test]
    async fn test_start_close_from_idle() {
        let mut pipeline = ClosePipeline::new();
        pipeline.start_close(dummy_close(100));
        assert!(!pipeline.is_idle());
        assert!(pipeline.is_closing());
        assert!(!pipeline.is_persisting());
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
    #[should_panic(expected = "take_close: not in Closing state")]
    async fn test_take_close_panics_when_idle() {
        let mut pipeline = ClosePipeline::new();
        pipeline.take_close(); // should panic
    }

    #[tokio::test]
    async fn test_start_persist_from_idle() {
        let mut pipeline = ClosePipeline::new();
        pipeline.start_persist(dummy_persist(50));
        assert!(!pipeline.is_idle());
        assert!(!pipeline.is_closing());
        assert!(pipeline.is_persisting());
    }

    #[tokio::test]
    #[should_panic(expected = "start_persist: pipeline not idle")]
    async fn test_start_persist_panics_when_persisting() {
        let mut pipeline = ClosePipeline::new();
        pipeline.start_persist(dummy_persist(50));
        pipeline.start_persist(dummy_persist(51)); // should panic
    }

    #[tokio::test]
    #[should_panic(expected = "start_persist: pipeline not idle")]
    async fn test_start_persist_panics_when_closing() {
        let mut pipeline = ClosePipeline::new();
        pipeline.start_close(dummy_close(100));
        pipeline.start_persist(dummy_persist(101)); // should panic
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
    #[should_panic(expected = "take_persist: not in Persisting state")]
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
        assert!(pipeline.is_closing());

        // Closing → Idle (take)
        let _pending = pipeline.take_close();
        assert!(pipeline.is_idle());

        // Idle → Persisting
        pipeline.start_persist(dummy_persist(10));
        assert!(pipeline.is_persisting());

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
        assert!(pipeline.is_closing());
    }

    #[tokio::test]
    async fn test_persist_ledger_seq() {
        let mut pipeline = ClosePipeline::new();
        assert_eq!(pipeline.persist_ledger_seq(), None);
        pipeline.start_persist(dummy_persist(123));
        assert_eq!(pipeline.persist_ledger_seq(), Some(123));
    }

    #[tokio::test]
    async fn test_poll_completion_close() {
        let mut pipeline = ClosePipeline::new();
        pipeline.start_close(dummy_close(42));

        let event = pipeline.poll_completion().await;
        assert!(matches!(event, PipelineEvent::CloseComplete(_)));
        // State remains Closing until take_close()
        assert!(pipeline.is_closing());

        let pending = pipeline.take_close();
        assert_eq!(pending.ledger_seq, 42);
        assert!(pipeline.is_idle());
    }

    #[tokio::test]
    async fn test_poll_completion_persist() {
        let mut pipeline = ClosePipeline::new();
        pipeline.start_persist(dummy_persist(77));

        let event = pipeline.poll_completion().await;
        assert!(matches!(event, PipelineEvent::PersistComplete(Ok(()))));
        // State remains Persisting until take_persist()
        assert!(pipeline.is_persisting());

        let persist = pipeline.take_persist();
        assert_eq!(persist.ledger_seq, 77);
        assert!(pipeline.is_idle());
    }

    #[tokio::test]
    async fn test_poll_completion_close_cancel_safe() {
        let mut pipeline = ClosePipeline::new();
        let (pending, tx) = blocked_close(42);
        pipeline.start_close(pending);

        // Cancel poll_completion via a branch that resolves first.
        // The blocked handle ensures poll_completion cannot resolve.
        tokio::select! {
            biased;
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            _ = pipeline.poll_completion() => { panic!("handle is blocked") }
        }

        // Pipeline still in Closing state — cancellation did not corrupt state
        assert!(pipeline.is_closing());

        // Unblock and poll to completion
        tx.send(()).unwrap();
        let event = pipeline.poll_completion().await;
        assert!(matches!(event, PipelineEvent::CloseComplete(_)));
        let _pending = pipeline.take_close();
        assert!(pipeline.is_idle());
    }

    #[tokio::test]
    async fn test_poll_completion_persist_cancel_safe() {
        let mut pipeline = ClosePipeline::new();
        let (pending, tx) = blocked_persist(55);
        pipeline.start_persist(pending);

        // Cancel poll_completion via a branch that resolves first
        tokio::select! {
            biased;
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            _ = pipeline.poll_completion() => { panic!("handle is blocked") }
        }

        // Pipeline still in Persisting state
        assert!(pipeline.is_persisting());

        // Unblock and poll to completion
        tx.send(()).unwrap();
        let event = pipeline.poll_completion().await;
        assert!(matches!(event, PipelineEvent::PersistComplete(Ok(()))));
        let _persist = pipeline.take_persist();
        assert!(pipeline.is_idle());
    }
}
