//! Scheduler state, metrics, and execution engine.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::types::EventSender;
use crate::{Work, WorkContext, WorkEvent, WorkId, WorkOutcome, WorkState};

/// Capacity of the internal channel used for work completion notifications.
const COMPLETION_CHANNEL_CAPACITY: usize = 128;

/// Configuration for the work scheduler.
///
/// This struct controls the scheduler's behavior including concurrency limits,
/// retry timing, and event monitoring. Use [`Default::default()`] for sensible
/// defaults or customize as needed.
///
/// # Example
///
/// ```ignore
/// use henyey_work::{WorkScheduler, WorkSchedulerConfig};
/// use std::time::Duration;
///
/// let config = WorkSchedulerConfig {
///     max_concurrency: 8,
///     retry_delay: Duration::from_secs(5),
///     event_tx: None,
/// };
/// let scheduler = WorkScheduler::new(config);
/// ```
#[derive(Debug, Clone)]
pub struct WorkSchedulerConfig {
    /// Maximum number of work items that can execute concurrently.
    ///
    /// Controls the parallelism of work execution. Higher values allow more
    /// work items to run simultaneously, which can improve throughput but
    /// also increases resource usage.
    ///
    /// Defaults to 4. Set to 1 for strictly sequential execution.
    pub max_concurrency: usize,

    /// Default delay between retry attempts.
    ///
    /// Used when a work item returns [`WorkOutcome::Retry`] with a zero duration.
    /// Work items can override this by specifying a non-zero delay in their
    /// retry outcome.
    ///
    /// Defaults to 1 second.
    pub retry_delay: Duration,

    /// Optional channel for receiving work state change events.
    ///
    /// If provided, the scheduler will send [`WorkEvent`] messages as work
    /// items transition between states. This enables external monitoring,
    /// progress tracking, and debugging without modifying work items.
    ///
    /// The channel should have sufficient capacity to avoid blocking the
    /// scheduler; events are sent with `try_send` and dropped if the
    /// channel is full.
    pub event_tx: Option<EventSender>,
}

impl Default for WorkSchedulerConfig {
    fn default() -> Self {
        Self {
            max_concurrency: 4,
            retry_delay: Duration::from_secs(1),
            event_tx: None,
        }
    }
}

/// A dependency-aware async work scheduler.
///
/// The scheduler manages a collection of work items, tracks their dependencies,
/// and executes them concurrently (up to a configured limit). It handles:
///
/// - **Dependency ordering**: Work items only run after all their dependencies succeed.
/// - **Concurrency control**: Limits the number of simultaneously running work items.
/// - **Automatic retries**: Retries work items that request it, with configurable delays.
/// - **Cancellation propagation**: Cancels all work items when requested.
/// - **Failure isolation**: Blocks dependent work when a dependency fails.
///
/// # Design
///
/// The scheduler is modeled after the work scheduling system in stellar-core.
/// It uses a simple ready-queue approach: work items whose dependencies have all
/// succeeded are eligible for execution. The scheduler spawns Tokio tasks for each
/// work item and collects completion notifications via an internal channel.
///
/// The execution loop follows these steps:
/// 1. Fill available concurrency slots with ready work items
/// 2. Wait for any running work to complete
/// 3. Process the completion and update state accordingly
/// 4. Repeat until no work can make progress
///
/// # Ownership
///
/// Work items are moved into the scheduler and remain there until the scheduler
/// is dropped. The scheduler temporarily moves work items out during execution
/// (replacing them with a placeholder `EmptyWork`) and moves them back after
/// completion. This design satisfies Rust's ownership rules while allowing
/// stateful work items to be retried.
///
/// # Example
///
/// ```ignore
/// use henyey_work::{WorkScheduler, WorkSchedulerConfig, Work, WorkOutcome, WorkContext};
///
/// // Create scheduler with custom configuration
/// let mut scheduler = WorkScheduler::new(WorkSchedulerConfig {
///     max_concurrency: 4,
///     retry_delay: Duration::from_secs(1),
///     event_tx: None,
/// });
///
/// // Add work items with dependencies
/// let id1 = scheduler.add_work(Box::new(work1), vec![], 3);
/// let id2 = scheduler.add_work(Box::new(work2), vec![id1], 0); // depends on id1
///
/// // Run until all work completes or is blocked
/// scheduler.run_until_done().await;
///
/// // Check results
/// assert_eq!(scheduler.state(id1), Some(WorkState::Success));
/// ```
pub struct WorkScheduler {
    /// Scheduler configuration (concurrency, retry delay, events).
    config: WorkSchedulerConfig,

    /// Next work ID to assign (monotonically increasing).
    next_id: WorkId,

    /// All registered work entries, keyed by their ID.
    entries: HashMap<WorkId, WorkEntry>,

    /// Current state of each work item.
    states: HashMap<WorkId, WorkState>,

    /// Reverse dependency map: work ID -> IDs of work items that depend on it.
    ///
    /// Used to efficiently find and block dependents when a work item fails.
    dependents: HashMap<WorkId, Vec<WorkId>>,
}

/// Aggregate metrics for the work scheduler.
///
/// Provides a point-in-time summary of the scheduler's state, useful for
/// monitoring progress, building dashboards, or debugging scheduling issues.
/// All counts are mutually exclusive - a work item is counted in exactly one
/// state category.
///
/// # Example
///
/// ```ignore
/// let metrics = scheduler.metrics();
/// println!(
///     "Progress: {}/{} complete, {} failed, {} running",
///     metrics.success, metrics.total, metrics.failed, metrics.running
/// );
/// ```
#[derive(Debug, Clone, Default)]
pub struct WorkSchedulerMetrics {
    /// Total number of registered work items.
    pub total: usize,

    /// Number of work items waiting to run (dependencies not satisfied or no slot).
    pub pending: usize,

    /// Number of work items currently executing.
    pub running: usize,

    /// Number of work items that completed successfully.
    pub success: usize,

    /// Number of work items that failed (terminal failure or exhausted retries).
    pub failed: usize,

    /// Number of work items blocked due to failed or cancelled dependencies.
    pub blocked: usize,

    /// Number of work items that were explicitly cancelled.
    pub cancelled: usize,

    /// Total number of execution attempts across all work items.
    ///
    /// This counts each retry as a separate attempt, so it may exceed `total`.
    pub attempts: u64,

    /// Total remaining retry budget across all work items.
    pub retries_left: u64,
}

/// A point-in-time snapshot of a single work item's state.
///
/// Used for debugging, monitoring, and introspection of the scheduler's
/// internal state. Snapshots are independent copies that can be examined
/// without affecting scheduler operation.
///
/// The `deps` and `dependents` fields show the dependency graph structure,
/// which is useful for understanding why work items are blocked or not yet
/// running.
#[derive(Debug, Clone)]
pub struct WorkSnapshot {
    /// Unique identifier of this work item.
    pub id: WorkId,

    /// Human-readable name of the work item.
    pub name: String,

    /// Current state of the work item.
    pub state: WorkState,

    /// IDs of work items this one depends on (prerequisites).
    pub deps: Vec<WorkId>,

    /// IDs of work items that depend on this one (downstream).
    pub dependents: Vec<WorkId>,

    /// Number of execution attempts made so far (includes retries).
    pub attempts: u32,

    /// Remaining retry budget.
    pub retries_left: u32,

    /// Error message from the most recent failure, if any.
    pub last_error: Option<String>,

    /// Duration of the most recent execution attempt.
    pub last_duration: Option<Duration>,

    /// Cumulative execution time across all attempts.
    pub total_duration: Duration,
}

/// Internal representation of a registered work item.
struct WorkEntry {
    /// Human-readable name for logging and identification.
    name: String,

    /// Work items that must complete successfully before this one can run.
    deps: Vec<WorkId>,

    /// Number of retry attempts remaining.
    ///
    /// Decremented each time a retry is consumed. When zero, further
    /// retry requests result in failure.
    retries_left: u32,

    /// Total number of execution attempts made (1-indexed).
    ///
    /// Incremented before each execution attempt, including retries.
    attempts: u32,

    /// Error message from the most recent failure, if any.
    last_error: Option<String>,

    /// Timestamp when the current or most recent execution started.
    ///
    /// Used to calculate execution duration.
    started_at: Option<Instant>,

    /// Duration of the most recent execution attempt.
    last_duration: Option<Duration>,

    /// Cumulative execution time across all attempts.
    total_duration: Duration,

    /// Token for cooperative cancellation.
    ///
    /// Shared with the [`WorkContext`] provided to the work item during execution.
    cancel_token: CancellationToken,

    /// The actual work implementation.
    work: Box<dyn Work + Send>,
}

impl WorkScheduler {
    /// Creates a new work scheduler with the given configuration.
    ///
    /// The scheduler starts empty with no registered work items. Use
    /// [`add_work`](Self::add_work) to register work items before calling
    /// [`run_until_done`](Self::run_until_done).
    pub fn new(config: WorkSchedulerConfig) -> Self {
        Self {
            config,
            next_id: 1,
            entries: HashMap::new(),
            states: HashMap::new(),
            dependents: HashMap::new(),
        }
    }

    /// Registers a work item with the scheduler.
    ///
    /// # Arguments
    ///
    /// * `work` - The work item to schedule.
    /// * `deps` - IDs of work items that must complete successfully before this one runs.
    /// * `retries` - Number of retry attempts allowed if the work returns [`WorkOutcome::Retry`].
    ///
    /// # Returns
    ///
    /// A unique [`WorkId`] that can be used to reference this work item.
    ///
    /// # Panics
    ///
    /// Does not panic. Invalid dependency IDs will cause the work item to be
    /// blocked when its dependencies are checked.
    #[must_use]
    pub fn add_work(
        &mut self,
        work: Box<dyn Work + Send>,
        deps: Vec<WorkId>,
        retries: u32,
    ) -> WorkId {
        let id = self.next_id;
        self.next_id += 1;

        let name = work.name().to_string();
        debug!(work_id = id, name = %name, "registered work item");

        for &dep in &deps {
            self.dependents.entry(dep).or_default().push(id);
        }

        let entry = WorkEntry {
            name,
            deps,
            retries_left: retries,
            attempts: 0,
            last_error: None,
            started_at: None,
            last_duration: None,
            total_duration: Duration::ZERO,
            cancel_token: CancellationToken::new(),
            work,
        };

        self.entries.insert(id, entry);
        self.states.insert(id, WorkState::Pending);

        id
    }

    /// Returns the current state of a work item, if it exists.
    ///
    /// Returns `None` if no work item with the given ID has been registered.
    #[must_use]
    pub fn state(&self, id: WorkId) -> Option<WorkState> {
        self.states.get(&id).copied()
    }

    /// Cancels a specific work item.
    ///
    /// If the work item is pending or running, it will be marked as cancelled
    /// and its dependents will be blocked. Running work items will receive
    /// a cancellation signal via their [`WorkContext`].
    ///
    /// # Returns
    ///
    /// `true` if the work item was successfully cancelled, `false` if it was
    /// already in a terminal state or does not exist.
    pub fn cancel(&mut self, id: WorkId) -> bool {
        let Some(state) = self.states.get(&id).copied() else {
            return false;
        };
        if state.is_terminal() {
            return false;
        }

        let Some(attempts) = self.entries.get_mut(&id).map(|entry| {
            entry.cancel_token.cancel();
            entry.attempts
        }) else {
            return false;
        };
        self.finish_terminal_state(id, WorkState::Cancelled, attempts);
        true
    }

    /// Cancels all registered work items.
    ///
    /// Each work item that is pending or running will be cancelled, and their
    /// dependents will be blocked.
    pub fn cancel_all(&mut self) {
        let ids: Vec<WorkId> = self.entries.keys().copied().collect();
        for id in ids {
            let _ = self.cancel(id);
        }
    }

    /// Returns a snapshot of all work items in the scheduler.
    ///
    /// The snapshots are sorted by work ID. This is useful for debugging,
    /// logging, and monitoring the scheduler's internal state. The returned
    /// data is a copy and does not affect scheduler operation.
    pub fn snapshot(&self) -> Vec<WorkSnapshot> {
        let mut snapshots: Vec<WorkSnapshot> = self
            .entries
            .iter()
            .map(|(id, entry)| WorkSnapshot {
                id: *id,
                name: entry.name.clone(),
                state: self.state_or_pending(*id),
                deps: entry.deps.clone(),
                dependents: self.dependents.get(id).cloned().unwrap_or_default(),
                attempts: entry.attempts,
                retries_left: entry.retries_left,
                last_error: entry.last_error.clone(),
                last_duration: entry.last_duration,
                total_duration: entry.total_duration,
            })
            .collect();
        snapshots.sort_by_key(|snapshot| snapshot.id);
        snapshots
    }

    /// Returns aggregate metrics for all work items in the scheduler.
    ///
    /// This is a lightweight operation that scans all entries once.
    /// For detailed per-item information, use [`snapshot`](Self::snapshot).
    pub fn metrics(&self) -> WorkSchedulerMetrics {
        let mut metrics = WorkSchedulerMetrics {
            total: self.entries.len(),
            ..Default::default()
        };
        for (id, entry) in &self.entries {
            match self.state_or_pending(*id) {
                WorkState::Pending => metrics.pending += 1,
                WorkState::Running => metrics.running += 1,
                WorkState::Success => metrics.success += 1,
                WorkState::Failed => metrics.failed += 1,
                WorkState::Blocked => metrics.blocked += 1,
                WorkState::Cancelled => metrics.cancelled += 1,
            }
            metrics.attempts += u64::from(entry.attempts);
            metrics.retries_left += u64::from(entry.retries_left);
        }
        metrics
    }

    /// Runs the scheduler until all work items complete or are blocked.
    ///
    /// This is a convenience method that creates a non-triggered cancellation
    /// token. Use [`run_until_done_with_cancel`](Self::run_until_done_with_cancel)
    /// if you need external cancellation control.
    ///
    /// The scheduler will return when:
    /// - All work items have reached a terminal state (Success, Failed, Blocked, Cancelled)
    /// - No pending work can make progress (all blocked on failed dependencies)
    pub async fn run_until_done(&mut self) {
        let cancel = CancellationToken::new();
        self.run_until_done_with_cancel(cancel).await;
    }

    /// Runs the scheduler until all work items complete, are blocked, or cancellation is requested.
    ///
    /// The scheduler will:
    /// 1. Start work items whose dependencies have all succeeded.
    /// 2. Execute up to `max_concurrency` work items in parallel.
    /// 3. Handle retries with the configured delay.
    /// 4. Block dependents of failed or cancelled work items.
    /// 5. Stop when no more work can make progress or cancellation is triggered.
    ///
    /// # Cancellation
    ///
    /// When the provided `cancel` token is triggered, the scheduler will:
    /// - Cancel all pending and running work items.
    /// - Wait for running work items to complete (they should check for cancellation).
    /// - Return once all work has stopped.
    pub async fn run_until_done_with_cancel(&mut self, cancel: CancellationToken) {
        let (tx, mut rx) = mpsc::channel::<WorkCompletion>(COMPLETION_CHANNEL_CAPACITY);
        let mut running = HashSet::new();
        let mut queue = self.ready_queue();
        let mut queued: HashSet<WorkId> = queue.iter().copied().collect();

        let mut cancel_requested = false;

        loop {
            if !cancel_requested && cancel.is_cancelled() {
                cancel_requested = true;
                self.cancel_all();
            }

            while running.len() < self.config.max_concurrency {
                let Some(id) = queue.pop_front() else {
                    break;
                };
                queued.remove(&id);
                if running.contains(&id) {
                    continue;
                }
                if self.start_work(id, &tx) {
                    running.insert(id);
                }
            }

            if running.is_empty() && queue.is_empty() {
                break;
            }

            let completion = if cancel_requested {
                rx.recv().await
            } else {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        cancel_requested = true;
                        self.cancel_all();
                        continue;
                    }
                    completion = rx.recv() => completion,
                }
            };
            let Some(completion) = completion else {
                break;
            };
            running.remove(&completion.id);

            match self.handle_completion(completion) {
                CompletionAction::Done { completed_id } => {
                    self.enqueue_dependents(completed_id, &mut queue, &mut queued, &running);
                }
                CompletionAction::Retry { id, delay } => {
                    tokio::time::sleep(delay).await;
                    if queued.insert(id) {
                        queue.push_back(id);
                    }
                }
                CompletionAction::None => {}
            }
        }

        info!("work scheduler finished");
    }

    /// Returns all work IDs that are currently in the Pending state.
    ///
    /// This forms the initial ready queue; actual runnability is determined
    /// by checking dependency satisfaction in [`can_run`](Self::can_run).
    fn ready_queue(&self) -> VecDeque<WorkId> {
        self.entries
            .keys()
            .filter(|id| self.state_or_pending(**id) == WorkState::Pending)
            .copied()
            .collect()
    }

    /// Returns the tracked state for a work item, defaulting to pending.
    fn state_or_pending(&self, id: WorkId) -> WorkState {
        self.states.get(&id).copied().unwrap_or(WorkState::Pending)
    }

    /// Starts a ready work item and spawns its execution task.
    fn start_work(&mut self, id: WorkId, tx: &mpsc::Sender<WorkCompletion>) -> bool {
        if !self.can_run(id) {
            return false;
        }

        let Some(attempts) = self.entries.get_mut(&id).map(|entry| entry.attempts) else {
            return false;
        };
        if self
            .entries
            .get(&id)
            .is_some_and(|entry| entry.cancel_token.is_cancelled())
        {
            self.finish_terminal_state(id, WorkState::Cancelled, attempts);
            return false;
        }

        let entry = self.entries.get_mut(&id).expect("entry must exist");

        entry.attempts += 1;
        let attempt = entry.attempts;
        let mut work = std::mem::replace(&mut entry.work, Box::new(EmptyWork));
        let name = entry.name.clone();
        let completion_tx = tx.clone();
        let cancel_token = entry.cancel_token.clone();
        entry.started_at = Some(Instant::now());

        self.transition_state(id, WorkState::Running, attempt);

        tokio::spawn(async move {
            let ctx = WorkContext {
                id,
                attempt,
                cancel_token,
            };
            let outcome = work.run(&ctx).await;
            let _ = completion_tx
                .send(WorkCompletion {
                    id,
                    outcome,
                    work,
                    attempt,
                    cancelled: ctx.is_cancelled(),
                })
                .await;
            debug!(work_id = id, name = %name, "work completed");
        });

        true
    }

    /// Enqueues dependents of a completed work item that are now ready to run.
    ///
    /// Only checks the direct dependents of `completed_id` rather than
    /// scanning all entries, since those are the only items whose readiness
    /// could have changed.
    fn enqueue_dependents(
        &self,
        completed_id: WorkId,
        queue: &mut VecDeque<WorkId>,
        queued: &mut HashSet<WorkId>,
        running: &HashSet<WorkId>,
    ) {
        let Some(children) = self.dependents.get(&completed_id) else {
            return;
        };
        for &child in children {
            if running.contains(&child) {
                continue;
            }
            if self.state_or_pending(child) != WorkState::Pending {
                continue;
            }
            if self.can_run(child) && queued.insert(child) {
                queue.push_back(child);
            }
        }
    }

    /// Returns `true` if all dependencies of the given work item have succeeded.
    ///
    /// A work item can only run when every work item in its `deps` list
    /// has reached the [`WorkState::Success`] state.
    fn can_run(&self, id: WorkId) -> bool {
        let Some(entry) = self.entries.get(&id) else {
            return false;
        };
        entry
            .deps
            .iter()
            .all(|dep| matches!(self.states.get(dep), Some(WorkState::Success)))
    }

    /// Marks all direct dependents of a work item as blocked.
    ///
    /// Called when a work item fails, is cancelled, or is blocked itself.
    /// Only affects dependents that are still in the Pending state.
    fn block_dependents(&mut self, id: WorkId) {
        let Some(children) = self.dependents.get(&id) else {
            return;
        };

        let pending: Vec<WorkId> = children
            .iter()
            .copied()
            .filter(|child| self.state_or_pending(*child) == WorkState::Pending)
            .collect();

        for child in pending {
            self.transition_state(child, WorkState::Blocked, 0);
        }
    }

    /// Records the new state and emits an event.
    fn transition_state(&mut self, id: WorkId, state: WorkState, attempt: u32) {
        self.states.insert(id, state);
        self.emit_event(id, state, attempt);
    }

    /// Transitions a work item to a terminal non-success state and blocks dependents.
    fn finish_terminal_state(&mut self, id: WorkId, state: WorkState, attempt: u32) {
        self.transition_state(id, state, attempt);
        self.block_dependents(id);
    }

    /// Restores a work item after execution and records timing.
    ///
    /// Called after a spawned work task completes, regardless of outcome.
    /// Moves the work implementation back into the entry (replacing the
    /// placeholder) and records the elapsed execution time.
    fn finalize_entry(&mut self, id: WorkId, work: Box<dyn Work + Send>) {
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.work = work;
            if let Some(started_at) = entry.started_at.take() {
                let elapsed = started_at.elapsed();
                entry.last_duration = Some(elapsed);
                entry.total_duration += elapsed;
            }
        }
    }

    /// Processes a completed work item and updates scheduler state.
    ///
    /// Returns a [`CompletionAction`] indicating what the main loop should do
    /// next: enqueue dependents (on success), schedule a retry, or nothing.
    fn handle_completion(&mut self, completion: WorkCompletion) -> CompletionAction {
        let id = completion.id;
        let attempt = completion.attempt;
        let cancelled =
            completion.cancelled || matches!(self.states.get(&id), Some(WorkState::Cancelled));

        self.finalize_entry(id, completion.work);

        match completion.outcome {
            WorkOutcome::Cancelled => {
                self.finish_terminal_state(id, WorkState::Cancelled, attempt);
                CompletionAction::None
            }
            WorkOutcome::Success if cancelled => {
                self.finish_terminal_state(id, WorkState::Cancelled, attempt);
                CompletionAction::None
            }
            WorkOutcome::Success => {
                self.transition_state(id, WorkState::Success, attempt);
                CompletionAction::Done { completed_id: id }
            }
            WorkOutcome::Retry { delay: _ } if cancelled => {
                self.finish_terminal_state(id, WorkState::Cancelled, attempt);
                CompletionAction::None
            }
            WorkOutcome::Retry { delay } => self.schedule_retry(id, attempt, delay),
            WorkOutcome::Failed(_) if cancelled => {
                self.finish_terminal_state(id, WorkState::Cancelled, attempt);
                CompletionAction::None
            }
            WorkOutcome::Failed(err) => {
                warn!(work_id = id, error = %err, "work failed");
                if let Some(entry) = self.entries.get_mut(&id) {
                    entry.last_error = Some(err);
                }
                self.finish_terminal_state(id, WorkState::Failed, attempt);
                CompletionAction::None
            }
        }
    }

    /// Converts a retry outcome into either a reschedule or a terminal failure.
    fn schedule_retry(&mut self, id: WorkId, attempt: u32, delay: Duration) -> CompletionAction {
        let Some(entry) = self.entries.get_mut(&id) else {
            return CompletionAction::None;
        };
        if entry.retries_left == 0 {
            self.finish_terminal_state(id, WorkState::Failed, attempt);
            return CompletionAction::None;
        }

        entry.retries_left -= 1;
        self.transition_state(id, WorkState::Pending, attempt);

        CompletionAction::Retry {
            id,
            delay: self.retry_delay(delay),
        }
    }

    /// Resolves the effective retry delay for a retry request.
    fn retry_delay(&self, requested_delay: Duration) -> Duration {
        if requested_delay == Duration::ZERO {
            self.config.retry_delay
        } else {
            requested_delay
        }
    }

    /// Sends a work event to the configured event channel, if any.
    ///
    /// Events are sent with `try_send` to avoid blocking the scheduler.
    /// If the channel is full, the event is dropped silently.
    fn emit_event(&self, id: WorkId, state: WorkState, attempt: u32) {
        let Some(tx) = self.config.event_tx.as_ref() else {
            return;
        };
        let name = self
            .entries
            .get(&id)
            .map_or_else(|| "unknown".into(), |entry| entry.name.clone());
        let _ = tx.try_send(WorkEvent {
            id,
            name,
            state,
            attempt,
        });
    }
}

/// Action returned by [`WorkScheduler::handle_completion`] to direct the
/// main execution loop.
enum CompletionAction {
    /// A work item completed successfully. The main loop should check
    /// dependents of `completed_id` for readiness.
    Done { completed_id: WorkId },
    /// A work item should be retried after `delay`.
    Retry { id: WorkId, delay: Duration },
    /// No further action needed (failure or cancellation handled internally).
    None,
}

/// Internal message sent when a spawned work task completes.
///
/// Carries all the information needed to update scheduler state after
/// a work item finishes executing.
struct WorkCompletion {
    /// The ID of the completed work item.
    id: WorkId,

    /// The outcome returned by the work item's `run` method.
    outcome: WorkOutcome,

    /// The work item itself, returned for potential reuse on retry.
    ///
    /// This allows stateful work items to maintain state across retries.
    work: Box<dyn Work + Send>,

    /// The attempt number for this execution.
    attempt: u32,

    /// Whether cancellation was requested during execution.
    ///
    /// Used to override the outcome if the work reported success but
    /// was actually cancelled.
    cancelled: bool,
}

/// A placeholder work item used during execution.
///
/// When a work item is spawned for execution, the scheduler must move it
/// out of the `entries` map (Rust ownership). This placeholder takes its
/// place temporarily. The real work item is moved back after execution
/// completes.
///
/// This is an internal implementation detail and should never actually
/// be executed.
struct EmptyWork;

#[async_trait]
impl Work for EmptyWork {
    fn name(&self) -> &str {
        "empty"
    }

    async fn run(&mut self, _ctx: &WorkContext) -> WorkOutcome {
        WorkOutcome::Success
    }
}
