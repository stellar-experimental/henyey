//! Helpers for building ordered work chains.

use crate::{Work, WorkId, WorkScheduler};

/// A helper for building linear sequences of dependent work items.
///
/// `WorkSequence` simplifies the common pattern of creating a chain of work
/// items where each depends on the previous one. Instead of manually tracking
/// the last work ID and passing it as a dependency, use this helper to
/// automatically chain work items.
///
/// This is particularly useful for multi-step processes like:
/// - Download -> Verify -> Apply workflows
/// - Sequential ledger processing
/// - Build pipelines with ordered stages
///
/// # Example
///
/// ```ignore
/// use henyey_work::{WorkScheduler, WorkSchedulerConfig, WorkSequence};
///
/// let mut scheduler = WorkScheduler::new(WorkSchedulerConfig::default());
/// let mut sequence = WorkSequence::new();
///
/// // Each work item automatically depends on the previous one
/// sequence.push(&mut scheduler, Box::new(step_1), 0);
/// sequence.push(&mut scheduler, Box::new(step_2), 0);
/// sequence.push(&mut scheduler, Box::new(step_3), 0);
///
/// // Run all steps in order
/// scheduler.run_until_done().await;
/// // Execution order: step_1 -> step_2 -> step_3
/// ```
///
/// # Combining with Direct Dependencies
///
/// You can also add work items with additional dependencies beyond the sequence:
///
/// ```ignore
/// let other_id = scheduler.add_work(Box::new(other_work), vec![], 0);
/// // This work depends on both the sequence and other_id
/// let combined = scheduler.add_work(
///     Box::new(final_work),
///     vec![*sequence.ids().last().unwrap(), other_id],
///     0
/// );
/// ```
#[derive(Default)]
pub struct WorkSequence {
    /// All work IDs added to this sequence, in order of addition.
    ids: Vec<WorkId>,
}

impl WorkSequence {
    /// Creates a new empty work sequence.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a work item to the sequence.
    ///
    /// The work item will depend on the previously added item (if any).
    /// The first item in a sequence has no dependencies (from this sequence).
    ///
    /// # Arguments
    ///
    /// * `scheduler` - The scheduler to register the work with.
    /// * `work` - The work item to add.
    /// * `retries` - Number of retry attempts for this work item.
    ///
    /// # Returns
    ///
    /// The [`WorkId`] of the newly added work item.
    pub fn push(
        &mut self,
        scheduler: &mut WorkScheduler,
        work: Box<dyn Work + Send>,
        retries: u32,
    ) -> WorkId {
        let deps = self
            .ids
            .last()
            .copied()
            .map_or_else(Vec::new, |id| vec![id]);
        let id = scheduler.add_work(work, deps, retries);
        self.ids.push(id);
        id
    }

    /// Returns all work IDs in this sequence, in order of addition.
    pub fn ids(&self) -> &[WorkId] {
        &self.ids
    }
}
