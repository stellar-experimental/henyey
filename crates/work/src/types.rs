//! Core work trait and state types.

use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Unique identifier for a work item within a scheduler.
///
/// Work IDs are assigned sequentially starting from 1 when work items are
/// added to the scheduler. They can be used to track dependencies, query
/// state, and cancel specific work items.
pub type WorkId = u64;

/// Result of a single work execution attempt.
///
/// Work items return this type from their [`Work::run`] method to indicate
/// the outcome of execution. The scheduler uses this to determine whether
/// to mark the work as complete, retry it, or handle failure.
///
/// # Retry Behavior
///
/// When returning [`WorkOutcome::Retry`], the scheduler will wait for the
/// specified delay before re-attempting the work, provided retries remain.
/// If no retries remain, the work transitions to [`WorkState::Failed`].
#[derive(Debug)]
pub enum WorkOutcome {
    /// Work completed successfully.
    ///
    /// Dependent work items will become runnable after this outcome.
    Success,

    /// Work was cancelled by the caller.
    ///
    /// Work items should return this when they detect cancellation via
    /// [`WorkContext::is_cancelled()`]. Dependent work items will be blocked.
    Cancelled,

    /// Work should be retried after the specified delay.
    ///
    /// If `delay` is zero, the scheduler's configured `retry_delay` is used.
    /// Retries are only attempted if the work item has remaining retry budget.
    Retry {
        /// Time to wait before the next attempt.
        delay: Duration,
    },

    /// Work failed with an error message.
    ///
    /// This is a terminal failure - the work will not be retried regardless
    /// of remaining retry budget. Dependent work items will be blocked.
    Failed(String),
}

/// Current state of a work item in the scheduler.
///
/// Work items transition through states as they are scheduled and executed.
/// The scheduler maintains state for each registered work item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkState {
    /// Work is waiting to be scheduled.
    ///
    /// The work item is registered but either has unfinished dependencies
    /// or is waiting for a concurrency slot.
    Pending,

    /// Work is currently executing.
    Running,

    /// Work completed successfully.
    Success,

    /// Work failed permanently (either via [`WorkOutcome::Failed`] or
    /// exhausted retries).
    Failed,

    /// Work cannot run because a dependency failed, was cancelled, or was blocked.
    ///
    /// This is a terminal state - blocked work will not be executed.
    Blocked,

    /// Work was explicitly cancelled.
    Cancelled,
}

impl WorkState {
    /// Returns `true` if this is a terminal state.
    ///
    /// Terminal states are those where no further progress will be made:
    /// [`Success`](Self::Success), [`Failed`](Self::Failed),
    /// [`Blocked`](Self::Blocked), and [`Cancelled`](Self::Cancelled).
    ///
    /// Non-terminal states are [`Pending`](Self::Pending) and
    /// [`Running`](Self::Running).
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Success | Self::Failed | Self::Blocked | Self::Cancelled
        )
    }
}

/// Execution context provided to a work item during execution.
///
/// The context provides the work item with its identity, the current attempt
/// number, and a mechanism to check for cancellation requests.
#[derive(Debug)]
pub struct WorkContext {
    /// The unique identifier of this work item.
    pub id: WorkId,

    /// The current attempt number (1-indexed).
    ///
    /// This is 1 for the first attempt, 2 for the first retry, etc.
    pub attempt: u32,

    /// Cancellation token for cooperative cancellation.
    pub(crate) cancel_token: CancellationToken,
}

impl WorkContext {
    /// Returns `true` if cancellation has been requested.
    ///
    /// Work items should check this periodically during long-running operations
    /// and return [`WorkOutcome::Cancelled`] if cancellation is detected.
    pub fn is_cancelled(&self) -> bool {
        self.cancel_token.is_cancelled()
    }

    /// Returns a reference to the cancellation token.
    ///
    /// This can be used for more advanced cancellation patterns, such as
    /// passing the token to async operations that support it directly.
    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel_token
    }
}

/// Event emitted by the scheduler when work state changes.
///
/// Events can be received via the [`crate::WorkSchedulerConfig::event_tx`] channel.
/// This enables external monitoring, logging, or progress tracking.
#[derive(Debug, Clone)]
pub struct WorkEvent {
    /// The work item this event pertains to.
    pub id: WorkId,

    /// Human-readable name of the work item.
    pub name: String,

    /// The new state of the work item.
    pub state: WorkState,

    /// The attempt number when this event occurred.
    pub attempt: u32,
}

/// A unit of schedulable, async work.
///
/// Implement this trait for types that represent work to be executed by
/// the scheduler. Work items are stateful and can maintain state across
/// retry attempts.
///
/// # Implementation Notes
///
/// - The `name` method should return a stable, human-readable identifier
///   for logging and debugging purposes.
/// - The `run` method receives a [`WorkContext`] and should check for
///   cancellation periodically during long operations.
/// - Work items are executed with `&mut self`, allowing them to update
///   internal state between retries.
#[async_trait]
pub trait Work: Send {
    /// Returns the name of this work item for logging and identification.
    fn name(&self) -> &str;

    /// Executes the work and returns an outcome.
    ///
    /// This method is called each time the work item is executed, including
    /// retries. The provided context contains the attempt number and a
    /// cancellation token.
    async fn run(&mut self, ctx: &WorkContext) -> WorkOutcome;
}

/// Optional event channel type used by the scheduler.
pub(crate) type EventSender = mpsc::Sender<WorkEvent>;
