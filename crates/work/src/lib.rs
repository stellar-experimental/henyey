//! Work scheduler and orchestration primitives for rs-stellar-core.
//!
//! This crate provides a dependency-aware async work scheduler modeled after
//! the work scheduling system in stellar-core. It enables concurrent
//! execution of tasks with explicit dependencies, automatic retry support,
//! and cancellation propagation.
//!
//! # Overview
//!
//! The scheduler manages work items that implement the [`Work`] trait. Each
//! work item can declare dependencies on other work items, and the scheduler
//! ensures prerequisites complete successfully before running dependent work.
//!
//! The design follows a directed acyclic graph (DAG) execution model where:
//! - Work items are nodes in the graph
//! - Dependencies form edges between nodes
//! - Execution proceeds in topological order
//! - Failed nodes block all downstream dependents
//!
//! # Key Components
//!
//! - [`Work`]: The trait that all schedulable work items must implement.
//! - [`WorkScheduler`]: The core scheduler that manages work execution.
//!
//! # Example
//!
//! ```ignore
//! use henyey_work::{Work, WorkContext, WorkOutcome, WorkScheduler, WorkSchedulerConfig};
//!
//! struct MyWork { name: String }
//!
//! #[async_trait::async_trait]
//! impl Work for MyWork {
//!     fn name(&self) -> &str { &self.name }
//!     async fn run(&mut self, ctx: &WorkContext) -> WorkOutcome {
//!         // Perform work, checking for cancellation as needed
//!         if ctx.is_cancelled() {
//!             return WorkOutcome::Cancelled;
//!         }
//!         WorkOutcome::Success
//!     }
//! }
//!
//! let mut scheduler = WorkScheduler::new(WorkSchedulerConfig::default());
//! let id = scheduler.add_work(Box::new(MyWork { name: "task".into() }), vec![], 3);
//! scheduler.run_until_done().await;
//! ```
//!
//! # Work Lifecycle
//!
//! Work items progress through a well-defined state machine:
//!
//! ```text
//!                      +----------+
//!                      | Pending  |
//!                      +----+-----+
//!                           |
//!              deps satisfied & slot available
//!                           |
//!                           v
//!                      +----------+
//!                      | Running  |
//!                      +----+-----+
//!                           |
//!        +--------+---------+---------+---------+
//!        |        |         |         |         |
//!        v        v         v         v         v
//!   +--------+ +------+ +-------+ +--------+ +-------+
//!   | Success| | Retry| | Failed| |Cancelled| |Blocked|
//!   +--------+ +------+ +-------+ +--------+ +-------+
//!                 |
//!          (if retries remain)
//!                 |
//!                 v
//!            +----------+
//!            | Pending  |
//!            +----------+
//! ```
//!
//! 1. Work items start in [`WorkState::Pending`].
//! 2. When all dependencies succeed, the scheduler moves work to [`WorkState::Running`].
//! 3. Work execution returns a [`WorkOutcome`] indicating success, failure, retry, or cancellation.
//! 4. On success, dependent work items become runnable.
//! 5. On failure or cancellation, dependent work items are blocked.
//!
//! # Cancellation
//!
//! The scheduler supports cooperative cancellation. Work items should periodically
//! check [`WorkContext::is_cancelled()`] and return [`WorkOutcome::Cancelled`] if
//! cancellation is requested. The scheduler propagates cancellation to all
//! registered work items when [`WorkScheduler::cancel_all()`] is called.
//!
//! # Thread Safety
//!
//! The scheduler itself is not thread-safe and should be driven from a single
//! async task. However, work items execute on Tokio's thread pool and must be
//! `Send`. Shared state between work items should use appropriate synchronization
//! primitives (e.g., `Arc<Mutex<T>>`, channels).

mod scheduler;
mod types;

pub use scheduler::{WorkScheduler, WorkSchedulerConfig, WorkSchedulerMetrics};
pub use types::{Work, WorkContext, WorkEvent, WorkId, WorkOutcome, WorkState};
