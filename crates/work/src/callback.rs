//! Callback-based work wrappers.

use std::sync::Arc;

use async_trait::async_trait;

use crate::{Work, WorkContext, WorkOutcome};

type WorkCallback = dyn Fn(&WorkOutcome, &WorkContext) + Send + Sync;

/// A work wrapper that invokes a callback after work finishes.
///
/// This is useful for integrating work completion notifications into
/// higher-level orchestration logic, such as catchup or publish workflows.
/// The callback receives both the outcome and execution context, allowing
/// for rich logging, metrics collection, or triggering downstream actions.
///
/// # Example
///
/// ```ignore
/// use henyey_work::{Work, WorkWithCallback, WorkOutcome, WorkContext};
/// use std::sync::Arc;
///
/// let callback = Arc::new(|outcome: &WorkOutcome, ctx: &WorkContext| {
///     println!("Work {} finished with {:?}", ctx.id, outcome);
/// });
///
/// let wrapped = WorkWithCallback::new(my_work, callback);
/// scheduler.add_work(Box::new(wrapped), vec![], 0);
/// ```
pub struct WorkWithCallback {
    /// The underlying work item being wrapped.
    work: Box<dyn Work + Send>,

    /// Callback invoked after each execution attempt with the outcome and context.
    callback: Arc<WorkCallback>,
}

impl WorkWithCallback {
    /// Creates a new callback-wrapped work item.
    ///
    /// # Arguments
    ///
    /// * `work` - The underlying work item to execute.
    /// * `callback` - A function called after the work completes, receiving
    ///   the outcome and execution context.
    pub fn new(work: Box<dyn Work + Send>, callback: Arc<WorkCallback>) -> Self {
        Self { work, callback }
    }
}

#[async_trait]
impl Work for WorkWithCallback {
    fn name(&self) -> &str {
        self.work.name()
    }

    async fn run(&mut self, ctx: &WorkContext) -> WorkOutcome {
        let outcome = self.work.run(ctx).await;
        (self.callback)(&outcome, ctx);
        outcome
    }
}
