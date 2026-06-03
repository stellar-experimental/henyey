//! Timer management for SCP consensus timeouts.
//!
//! This module provides an async timer manager that integrates with tokio to schedule
//! and manage SCP nomination and ballot timeouts. It enables the Herder to:
//!
//! - Schedule nomination timeouts when starting a new nomination round
//! - Schedule ballot timeouts when heard from quorum during ballot phase
//! - Cancel and reschedule timers as consensus progresses
//! - Handle timeout expiration by invoking Herder callbacks
//!
//! # Architecture
//!
//! The timer manager runs as a background task that:
//! 1. Receives timer commands via a channel (start, cancel, reschedule)
//! 2. Maintains active timers per slot
//! 3. Fires callbacks when timers expire
//! 4. Cleans up timers for externalized or purged slots
//!
//! # Example
//!
//! ```ignore
//! use henyey_herder::timer_manager::{TimerManager, TimerManagerHandle};
//!
//! // Create timer manager
//! let herder = Arc::new(herder);
//! let (handle, task) = TimerManager::new(herder.clone());
//!
//! // Spawn the background task
//! tokio::spawn(task);
//!
//! // Schedule a nomination timeout
//! handle.schedule_nomination_timeout(slot, duration).await;
//!
//! // Schedule a ballot timeout
//! handle.schedule_ballot_timeout(slot, duration).await;
//!
//! // Cancel timers for a slot
//! handle.cancel_slot_timers(slot).await;
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, info, trace};

use henyey_scp::SlotIndex;

/// Commands sent to the timer manager task.
#[derive(Debug)]
pub enum TimerCommand {
    /// Schedule a nomination timeout for a slot (overwrites existing timer).
    ///
    /// `epoch` is `None` for normal scheduling (the timer manager stamps the
    /// timer with the current global tracking epoch). It is `Some(e)` when an
    /// already-fired timer is being re-armed (the future-slot 1-second defer
    /// loop), so the re-armed timer inherits the *originating* epoch instead of
    /// re-sampling the current one. This prevents a pre-sync-loss timer from
    /// being "reborn" into a newer epoch and surviving into Syncing — a race
    /// that has no analog in stellar-core, where `timerCallbackWrapper` and
    /// `setupTimer` operate on the same in-process timer state.
    ScheduleNominationTimeout {
        slot: SlotIndex,
        duration: Duration,
        epoch: Option<u64>,
    },
    /// Schedule a ballot timeout for a slot (overwrites existing timer).
    ///
    /// See [`TimerCommand::ScheduleNominationTimeout`] for the meaning of
    /// `epoch`.
    ScheduleBallotTimeout {
        slot: SlotIndex,
        duration: Duration,
        epoch: Option<u64>,
    },
    /// Schedule the event-driven consensus trigger timer for a slot
    /// (overwrites existing timer). Mirrors stellar-core's `mTriggerTimer`
    /// armed by `setupTriggerNextLedger` (#2702). Unlike the SCP
    /// nomination/ballot timers, this is not subject to the future-slot defer
    /// loop — it is the primary "time to nominate slot N" wake-up.
    ScheduleTriggerNextLedger {
        slot: SlotIndex,
        duration: Duration,
        epoch: Option<u64>,
    },
    /// Cancel the consensus trigger timer for a slot (#2702).
    CancelTriggerNextLedger { slot: SlotIndex },
    /// Cancel all timers for a slot.
    CancelSlotTimers { slot: SlotIndex },
    /// Cancel the nomination timer for a slot (but keep ballot timer).
    CancelNominationTimer { slot: SlotIndex },
    /// Cancel the ballot timer for a slot (but keep nomination timer).
    CancelBallotTimer { slot: SlotIndex },
    /// Purge timers for slots older than the given slot.
    PurgeOldSlots { min_slot: SlotIndex },
    /// Cancel all active timers regardless of slot.
    CancelAllTimers,
    /// Shutdown the timer manager.
    Shutdown,
}

/// Handle for sending commands to the timer manager.
#[derive(Clone)]
pub struct TimerManagerHandle {
    sender: mpsc::UnboundedSender<TimerCommand>,
}

impl TimerManagerHandle {
    /// Create a no-op handle for tests.
    ///
    /// Commands are accepted but never processed. The receiver is leaked
    /// to prevent `Closed` errors during the test lifetime.
    pub fn no_op() -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        // Leak the receiver so the channel stays open.
        std::mem::forget(receiver);
        Self { sender }
    }

    /// Schedule a nomination timeout for a slot.
    ///
    /// The timer is stamped with the current global tracking epoch at
    /// schedule-time. To re-arm an already-fired timer while preserving its
    /// originating epoch (the future-slot defer loop), use
    /// [`Self::reschedule_nomination_timeout_with_epoch`] instead.
    pub async fn schedule_nomination_timeout(&self, slot: SlotIndex, duration: Duration) {
        let _ = self.sender.send(TimerCommand::ScheduleNominationTimeout {
            slot,
            duration,
            epoch: None,
        });
    }

    /// Schedule a ballot timeout for a slot.
    ///
    /// See [`Self::schedule_nomination_timeout`] for epoch semantics.
    pub async fn schedule_ballot_timeout(&self, slot: SlotIndex, duration: Duration) {
        let _ = self.sender.send(TimerCommand::ScheduleBallotTimeout {
            slot,
            duration,
            epoch: None,
        });
    }

    /// Schedule the event-driven consensus trigger timer for a slot (#2702).
    ///
    /// Mirrors stellar-core's `setupTriggerNextLedger` arming `mTriggerTimer`:
    /// when it fires, the app calls `try_trigger_consensus()` for `slot`.
    pub async fn schedule_trigger_next_ledger(&self, slot: SlotIndex, duration: Duration) {
        let _ = self.sender.send(TimerCommand::ScheduleTriggerNextLedger {
            slot,
            duration,
            epoch: None,
        });
    }

    /// Cancel the consensus trigger timer for a slot (#2702).
    pub async fn cancel_trigger_next_ledger(&self, slot: SlotIndex) {
        let _ = self
            .sender
            .send(TimerCommand::CancelTriggerNextLedger { slot });
    }

    /// Re-arm a nomination timeout for a slot, preserving the originating
    /// tracking `epoch` rather than re-sampling the current global epoch.
    ///
    /// Used by the future-slot 1-second defer loop: when a future-slot timer
    /// fires while tracking, it is re-scheduled for 1 second. If the node loses
    /// sync between the fire and the re-arm, sampling the *current* epoch would
    /// let the re-armed timer survive into Syncing. Carrying the firing event's
    /// epoch keeps the re-armed timer attributable to the epoch in which it was
    /// originally created, so the epoch guard rejects it after sync loss —
    /// matching stellar-core's in-process defer loop, which cannot rebirth a
    /// pre-sync-loss timer.
    pub async fn reschedule_nomination_timeout_with_epoch(
        &self,
        slot: SlotIndex,
        duration: Duration,
        epoch: u64,
    ) {
        let _ = self.sender.send(TimerCommand::ScheduleNominationTimeout {
            slot,
            duration,
            epoch: Some(epoch),
        });
    }

    /// Re-arm a ballot timeout for a slot, preserving the originating tracking
    /// `epoch`. See [`Self::reschedule_nomination_timeout_with_epoch`].
    pub async fn reschedule_ballot_timeout_with_epoch(
        &self,
        slot: SlotIndex,
        duration: Duration,
        epoch: u64,
    ) {
        let _ = self.sender.send(TimerCommand::ScheduleBallotTimeout {
            slot,
            duration,
            epoch: Some(epoch),
        });
    }

    /// Cancel all timers for a slot (both nomination and ballot).
    pub async fn cancel_slot_timers(&self, slot: SlotIndex) {
        let _ = self.sender.send(TimerCommand::CancelSlotTimers { slot });
    }

    /// Cancel only the nomination timer for a slot.
    pub async fn cancel_nomination_timer(&self, slot: SlotIndex) {
        let _ = self
            .sender
            .send(TimerCommand::CancelNominationTimer { slot });
    }

    /// Cancel only the ballot timer for a slot.
    pub async fn cancel_ballot_timer(&self, slot: SlotIndex) {
        let _ = self.sender.send(TimerCommand::CancelBallotTimer { slot });
    }

    /// Purge timers for slots older than the given slot.
    pub async fn purge_old_slots(&self, min_slot: SlotIndex) {
        let _ = self.sender.send(TimerCommand::PurgeOldSlots { min_slot });
    }

    /// Shutdown the timer manager.
    pub async fn shutdown(&self) {
        let _ = self.sender.send(TimerCommand::Shutdown);
    }

    /// Cancel all active timers regardless of slot.
    pub async fn cancel_all_timers(&self) {
        let _ = self.sender.send(TimerCommand::CancelAllTimers);
    }

    // --- Non-blocking methods for use from synchronous (spawn_blocking) contexts ---

    /// Schedule a nomination timeout (non-blocking, infallible while channel open).
    pub fn schedule_nomination_timeout_nonblocking(&self, slot: SlotIndex, duration: Duration) {
        if self
            .sender
            .send(TimerCommand::ScheduleNominationTimeout {
                slot,
                duration,
                epoch: None,
            })
            .is_err()
        {
            tracing::warn!(slot, "timer channel closed: schedule nomination dropped");
        }
    }

    /// Schedule a ballot timeout (non-blocking, infallible while channel open).
    pub fn schedule_ballot_timeout_nonblocking(&self, slot: SlotIndex, duration: Duration) {
        if self
            .sender
            .send(TimerCommand::ScheduleBallotTimeout {
                slot,
                duration,
                epoch: None,
            })
            .is_err()
        {
            tracing::warn!(slot, "timer channel closed: schedule ballot dropped");
        }
    }

    /// Schedule the consensus trigger timer (non-blocking). Used from the
    /// synchronous post-close / cold-start arming paths (#2702).
    pub fn schedule_trigger_next_ledger_nonblocking(&self, slot: SlotIndex, duration: Duration) {
        if self
            .sender
            .send(TimerCommand::ScheduleTriggerNextLedger {
                slot,
                duration,
                epoch: None,
            })
            .is_err()
        {
            tracing::warn!(slot, "timer channel closed: schedule trigger dropped");
        }
    }

    /// Cancel the consensus trigger timer for a slot (non-blocking) (#2702).
    pub fn cancel_trigger_next_ledger_nonblocking(&self, slot: SlotIndex) {
        if self
            .sender
            .send(TimerCommand::CancelTriggerNextLedger { slot })
            .is_err()
        {
            tracing::warn!(slot, "timer channel closed: cancel trigger dropped");
        }
    }

    /// Cancel all timers for a slot (non-blocking).
    pub fn cancel_slot_timers_nonblocking(&self, slot: SlotIndex) {
        if self
            .sender
            .send(TimerCommand::CancelSlotTimers { slot })
            .is_err()
        {
            tracing::warn!(slot, "timer channel closed: cancel slot timers dropped");
        }
    }

    /// Cancel the nomination timer for a slot (non-blocking).
    pub fn cancel_nomination_timer_nonblocking(&self, slot: SlotIndex) {
        if self
            .sender
            .send(TimerCommand::CancelNominationTimer { slot })
            .is_err()
        {
            tracing::warn!(slot, "timer channel closed: cancel nomination dropped");
        }
    }

    /// Cancel the ballot timer for a slot (non-blocking).
    pub fn cancel_ballot_timer_nonblocking(&self, slot: SlotIndex) {
        if self
            .sender
            .send(TimerCommand::CancelBallotTimer { slot })
            .is_err()
        {
            tracing::warn!(slot, "timer channel closed: cancel ballot dropped");
        }
    }

    /// Cancel all active timers (non-blocking).
    /// Used from synchronous contexts like `on_lost_sync` where outstanding
    /// SCP timers must be invalidated immediately on state transition.
    pub fn cancel_all_timers_nonblocking(&self) {
        if self.sender.send(TimerCommand::CancelAllTimers).is_err() {
            tracing::warn!("timer channel closed: cancel all timers dropped");
        }
    }
}

/// Callback trait for timer expiration events.
///
/// Implement this trait to receive timer expiration callbacks.
/// The `epoch` parameter is the tracking epoch captured at timer-schedule time,
/// allowing receivers to discard stale events from prior epochs.
pub trait TimerCallback: Send + Sync + 'static {
    /// Called when a nomination timeout expires.
    /// `epoch` is the tracking epoch that was current when this timer was scheduled.
    fn on_nomination_timeout(&self, slot: SlotIndex, epoch: u64);

    /// Called when a ballot timeout expires.
    /// `epoch` is the tracking epoch that was current when this timer was scheduled.
    fn on_ballot_timeout(&self, slot: SlotIndex, epoch: u64);

    /// Called when the event-driven consensus trigger timer expires (#2702).
    /// The receiver should attempt to trigger consensus for `slot`.
    ///
    /// Default no-op so callbacks that predate the trigger timer (and tests
    /// that only exercise SCP timeouts) continue to compile unchanged.
    fn on_trigger_next_ledger(&self, _slot: SlotIndex, _epoch: u64) {}
}

/// Timer type for a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimerType {
    Nomination,
    Ballot,
    /// Event-driven consensus trigger (#2702): mirrors stellar-core's
    /// `mTriggerTimer`. Fires when it is time to nominate the next ledger.
    TriggerNextLedger,
}

/// Active timer state.
#[derive(Debug)]
struct ActiveTimer {
    timer_type: TimerType,
    slot: SlotIndex,
    expires_at: Instant,
    /// Unique ID to detect if timer was rescheduled (stored for future use)
    _generation: u64,
    /// The tracking epoch at which this timer was scheduled. Passed to the
    /// callback on fire so the receiver can discard events from prior epochs.
    epoch: u64,
}

/// The timer manager that runs as a background task.
pub struct TimerManager<C: TimerCallback> {
    callback: Arc<C>,
    receiver: mpsc::UnboundedReceiver<TimerCommand>,
    /// Active timers indexed by (slot, timer_type)
    timers: HashMap<(SlotIndex, TimerType), ActiveTimer>,
    /// Generation counter for timer identification
    generation: u64,
    /// Shared tracking epoch — read at schedule-time and stored in each timer.
    epoch: Arc<AtomicU64>,
}

impl<C: TimerCallback> TimerManager<C> {
    /// Create a new timer manager with the given callback and shared epoch.
    ///
    /// The `epoch` is read at timer-schedule time and stored in each active timer.
    /// When a timer fires, the stored epoch is passed to the callback, allowing
    /// the receiver to detect and discard stale events from prior tracking epochs.
    ///
    /// Returns a handle for sending commands and the manager itself which should
    /// be spawned as a tokio task.
    pub fn new(callback: Arc<C>, epoch: Arc<AtomicU64>) -> (TimerManagerHandle, Self) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let handle = TimerManagerHandle { sender };
        let manager = Self {
            callback,
            receiver,
            timers: HashMap::new(),
            generation: 0,
            epoch,
        };
        (handle, manager)
    }

    /// Run the timer manager.
    ///
    /// This method runs indefinitely, processing timer commands and firing
    /// callbacks when timers expire.
    pub async fn run(mut self) {
        info!("Timer manager started");

        loop {
            let next_timeout = self.next_timeout();

            tokio::select! {
                cmd = self.receiver.recv() => {
                    match cmd {
                        Some(cmd) => {
                            if !self.handle_command(cmd) {
                                break;
                            }
                        }
                        None => {
                            info!("Timer manager shutting down (channel closed)");
                            break;
                        }
                    }
                }

                _ = crate::herder_utils::sleep_until_or_forever(next_timeout) => {
                    self.fire_expired_timers();
                }
            }
        }
    }

    /// Handle a single timer command. Returns `false` on Shutdown.
    pub(crate) fn handle_command(&mut self, cmd: TimerCommand) -> bool {
        match cmd {
            TimerCommand::ScheduleNominationTimeout {
                slot,
                duration,
                epoch,
            } => {
                self.schedule_timer(slot, TimerType::Nomination, duration, epoch);
            }
            TimerCommand::ScheduleBallotTimeout {
                slot,
                duration,
                epoch,
            } => {
                self.schedule_timer(slot, TimerType::Ballot, duration, epoch);
            }
            TimerCommand::ScheduleTriggerNextLedger {
                slot,
                duration,
                epoch,
            } => {
                self.schedule_timer(slot, TimerType::TriggerNextLedger, duration, epoch);
            }
            TimerCommand::CancelTriggerNextLedger { slot } => {
                self.cancel_timer(slot, TimerType::TriggerNextLedger);
            }
            TimerCommand::CancelSlotTimers { slot } => {
                self.cancel_slot_timers(slot);
            }
            TimerCommand::CancelNominationTimer { slot } => {
                self.cancel_timer(slot, TimerType::Nomination);
            }
            TimerCommand::CancelBallotTimer { slot } => {
                self.cancel_timer(slot, TimerType::Ballot);
            }
            TimerCommand::PurgeOldSlots { min_slot } => {
                self.purge_old_slots(min_slot);
            }
            TimerCommand::CancelAllTimers => {
                if !self.timers.is_empty() {
                    debug!(count = self.timers.len(), "Cancelled all timers");
                    self.timers.clear();
                }
            }
            TimerCommand::Shutdown => {
                info!("Timer manager shutting down");
                return false;
            }
        }
        true
    }

    /// Schedule a timer for a slot (overwrites any existing timer).
    ///
    /// `epoch_override` is `None` for normal scheduling (the timer is stamped
    /// with the current global tracking epoch). It is `Some(e)` when re-arming
    /// an already-fired timer (the future-slot defer loop), in which case the
    /// timer inherits the originating epoch `e` rather than re-sampling the
    /// current one — see [`TimerCommand::ScheduleNominationTimeout`].
    fn schedule_timer(
        &mut self,
        slot: SlotIndex,
        timer_type: TimerType,
        duration: Duration,
        epoch_override: Option<u64>,
    ) {
        self.generation = self.generation.wrapping_add(1);
        let expires_at = Instant::now() + duration;
        let epoch = epoch_override.unwrap_or_else(|| self.epoch.load(Ordering::Acquire));

        let timer = ActiveTimer {
            timer_type,
            slot,
            expires_at,
            _generation: self.generation,
            epoch,
        };

        debug!(
            slot,
            timer_type = ?timer_type,
            duration_ms = duration.as_millis(),
            "Scheduled timer"
        );

        self.timers.insert((slot, timer_type), timer);
    }

    /// Cancel all timers for a slot.
    fn cancel_slot_timers(&mut self, slot: SlotIndex) {
        let removed_nom = self.timers.remove(&(slot, TimerType::Nomination)).is_some();
        let removed_bal = self.timers.remove(&(slot, TimerType::Ballot)).is_some();
        // The consensus trigger timer (#2702) is keyed per-slot too; clear it
        // alongside the SCP timers so a stale slot leaves no lingering trigger.
        let removed_trig = self
            .timers
            .remove(&(slot, TimerType::TriggerNextLedger))
            .is_some();

        if removed_nom || removed_bal || removed_trig {
            debug!(slot, "Cancelled slot timers");
        }
    }

    /// Cancel a specific timer type for a slot.
    fn cancel_timer(&mut self, slot: SlotIndex, timer_type: TimerType) {
        if self.timers.remove(&(slot, timer_type)).is_some() {
            debug!(slot, timer_type = ?timer_type, "Cancelled timer");
        }
    }

    /// Purge timers for slots older than the given slot.
    fn purge_old_slots(&mut self, min_slot: SlotIndex) {
        let old_count = self.timers.len();
        self.timers.retain(|(slot, _), _| *slot >= min_slot);
        let removed = old_count - self.timers.len();

        if removed > 0 {
            debug!(min_slot, removed, "Purged old slot timers");
        }
    }

    /// Get the next timeout instant, if any.
    fn next_timeout(&self) -> Option<Instant> {
        self.timers.values().map(|t| t.expires_at).min()
    }

    /// Fire all expired timers and remove them.
    fn fire_expired_timers(&mut self) {
        let now = Instant::now();

        // Collect expired timers
        let expired: Vec<_> = self
            .timers
            .iter()
            .filter(|(_, t)| t.expires_at <= now)
            .map(|(k, t)| (*k, t.timer_type, t.slot, t.epoch))
            .collect();

        // Remove and fire
        for (key, timer_type, slot, epoch) in expired {
            self.timers.remove(&key);

            trace!(slot, timer_type = ?timer_type, epoch, "Firing timer");

            match timer_type {
                TimerType::Nomination => {
                    self.callback.on_nomination_timeout(slot, epoch);
                }
                TimerType::Ballot => {
                    self.callback.on_ballot_timeout(slot, epoch);
                }
                TimerType::TriggerNextLedger => {
                    self.callback.on_trigger_next_ledger(slot, epoch);
                }
            }
        }
    }
}

/// Statistics about the timer manager.
#[derive(Debug, Clone, Default)]
pub struct TimerStats {
    /// Number of active nomination timers.
    pub nomination_timers: usize,
    /// Number of active ballot timers.
    pub ballot_timers: usize,
    /// Total active timers.
    pub total_timers: usize,
}

/// A timer manager that tracks stats and provides introspection.
///
/// This wraps the basic timer manager with additional tracking for monitoring.
pub struct TimerManagerWithStats<C: TimerCallback> {
    inner: TimerManager<C>,
    stats: Arc<RwLock<TimerStats>>,
}

impl<C: TimerCallback> TimerManagerWithStats<C> {
    /// Create a new timer manager with stats tracking.
    pub fn new(
        callback: Arc<C>,
        epoch: Arc<AtomicU64>,
    ) -> (TimerManagerHandle, Arc<RwLock<TimerStats>>, Self) {
        let (handle, inner) = TimerManager::new(callback, epoch);
        let stats = Arc::new(RwLock::new(TimerStats::default()));
        let manager = Self {
            inner,
            stats: stats.clone(),
        };
        (handle, stats, manager)
    }

    /// Run the timer manager with stats updates.
    pub async fn run(mut self) {
        info!("Timer manager with stats started");

        loop {
            self.update_stats();

            let next_timeout = self.inner.next_timeout();

            tokio::select! {
                cmd = self.inner.receiver.recv() => {
                    match cmd {
                        Some(cmd) => {
                            if !self.inner.handle_command(cmd) {
                                break;
                            }
                        }
                        None => {
                            info!("Timer manager shutting down (channel closed)");
                            break;
                        }
                    }
                }

                _ = crate::herder_utils::sleep_until_or_forever(next_timeout) => {
                    self.inner.fire_expired_timers();
                }
            }
        }
    }

    fn update_stats(&self) {
        let nomination_count = self
            .inner
            .timers
            .values()
            .filter(|t| t.timer_type == TimerType::Nomination)
            .count();
        let ballot_count = self
            .inner
            .timers
            .values()
            .filter(|t| t.timer_type == TimerType::Ballot)
            .count();

        let mut stats = self.stats.write();
        stats.nomination_timers = nomination_count;
        stats.ballot_timers = ballot_count;
        stats.total_timers = self.inner.timers.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::time::timeout;

    struct TestCallback {
        nomination_fired: AtomicU64,
        ballot_fired: AtomicU64,
        /// Slot of the most recent trigger-next-ledger callback (#2702).
        trigger_fired: AtomicU64,
        /// Epoch carried by the most recent nomination callback. Sentinel
        /// `u64::MAX` means "no nomination has fired yet".
        last_nomination_epoch: AtomicU64,
        /// Epoch carried by the most recent ballot callback. Sentinel
        /// `u64::MAX` means "no ballot has fired yet".
        last_ballot_epoch: AtomicU64,
    }

    impl TestCallback {
        fn new() -> Self {
            Self {
                nomination_fired: AtomicU64::new(0),
                ballot_fired: AtomicU64::new(0),
                trigger_fired: AtomicU64::new(0),
                last_nomination_epoch: AtomicU64::new(u64::MAX),
                last_ballot_epoch: AtomicU64::new(u64::MAX),
            }
        }
    }

    impl TimerCallback for TestCallback {
        fn on_nomination_timeout(&self, slot: SlotIndex, epoch: u64) {
            self.nomination_fired.store(slot, Ordering::SeqCst);
            self.last_nomination_epoch.store(epoch, Ordering::SeqCst);
        }

        fn on_ballot_timeout(&self, slot: SlotIndex, epoch: u64) {
            self.ballot_fired.store(slot, Ordering::SeqCst);
            self.last_ballot_epoch.store(epoch, Ordering::SeqCst);
        }

        fn on_trigger_next_ledger(&self, slot: SlotIndex, _epoch: u64) {
            self.trigger_fired.store(slot, Ordering::SeqCst);
        }
    }

    /// #2702: the event-driven consensus trigger timer (`TriggerNextLedger`)
    /// must fire its callback after the scheduled duration. Fails to compile on
    /// pre-#2702 main because `TimerType::TriggerNextLedger`,
    /// `schedule_trigger_next_ledger`, and `on_trigger_next_ledger` do not exist.
    #[tokio::test]
    async fn test_trigger_next_ledger_fires() {
        let callback = Arc::new(TestCallback::new());
        let (handle, manager) = TimerManager::new(callback.clone(), Arc::new(AtomicU64::new(0)));

        let manager_task = tokio::spawn(manager.run());

        handle
            .schedule_trigger_next_ledger(7, Duration::from_millis(50))
            .await;

        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(callback.trigger_fired.load(Ordering::SeqCst), 7);

        handle.shutdown().await;
        let _ = timeout(Duration::from_millis(100), manager_task).await;
    }

    /// #2702: rescheduling the trigger timer cancels the prior arming, and
    /// `cancel_trigger_next_ledger` prevents an armed trigger from firing.
    #[tokio::test]
    async fn test_trigger_next_ledger_cancel_prevents_firing() {
        let callback = Arc::new(TestCallback::new());
        let (handle, manager) = TimerManager::new(callback.clone(), Arc::new(AtomicU64::new(0)));

        let manager_task = tokio::spawn(manager.run());

        handle
            .schedule_trigger_next_ledger(9, Duration::from_millis(100))
            .await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        handle.cancel_trigger_next_ledger(9).await;

        tokio::time::sleep(Duration::from_millis(150)).await;

        assert_eq!(callback.trigger_fired.load(Ordering::SeqCst), 0);

        handle.shutdown().await;
        let _ = timeout(Duration::from_millis(100), manager_task).await;
    }

    /// #2702: a trigger timer is independent of the nomination/ballot timers for
    /// the same slot — cancelling the trigger leaves SCP timers intact and vice
    /// versa, and `cancel_slot_timers` / `cancel_all_timers` / `purge_old_slots`
    /// all account for the trigger variant.
    #[tokio::test]
    async fn test_trigger_next_ledger_independent_of_scp_timers() {
        let callback = Arc::new(TestCallback::new());
        let (handle, manager) = TimerManager::new(callback.clone(), Arc::new(AtomicU64::new(0)));

        let manager_task = tokio::spawn(manager.run());

        // Arm both a nomination timer and a trigger timer for slot 5.
        handle
            .schedule_nomination_timeout(5, Duration::from_millis(60))
            .await;
        handle
            .schedule_trigger_next_ledger(5, Duration::from_millis(60))
            .await;
        // Cancelling just the trigger leaves the nomination timer to fire.
        handle.cancel_trigger_next_ledger(5).await;

        tokio::time::sleep(Duration::from_millis(120)).await;

        assert_eq!(callback.nomination_fired.load(Ordering::SeqCst), 5);
        assert_eq!(callback.trigger_fired.load(Ordering::SeqCst), 0);

        handle.shutdown().await;
        let _ = timeout(Duration::from_millis(100), manager_task).await;
    }

    /// #2702: `cancel_all_timers` and `purge_old_slots` must remove trigger
    /// timers along with SCP timers.
    #[tokio::test]
    async fn test_trigger_next_ledger_purged_by_cancel_all_and_purge() {
        let callback = Arc::new(TestCallback::new());
        let (handle, manager) = TimerManager::new(callback.clone(), Arc::new(AtomicU64::new(0)));
        let manager_task = tokio::spawn(manager.run());

        // purge_old_slots removes a stale trigger timer.
        handle
            .schedule_trigger_next_ledger(3, Duration::from_millis(80))
            .await;
        handle.purge_old_slots(10).await;
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(callback.trigger_fired.load(Ordering::SeqCst), 0);

        // cancel_all_timers removes a live trigger timer.
        handle
            .schedule_trigger_next_ledger(11, Duration::from_millis(80))
            .await;
        handle.cancel_all_timers().await;
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(callback.trigger_fired.load(Ordering::SeqCst), 0);

        handle.shutdown().await;
        let _ = timeout(Duration::from_millis(100), manager_task).await;
    }

    #[tokio::test]
    async fn test_nomination_timeout_fires() {
        let callback = Arc::new(TestCallback::new());
        let (handle, manager) = TimerManager::new(callback.clone(), Arc::new(AtomicU64::new(0)));

        let manager_task = tokio::spawn(manager.run());

        // Schedule a nomination timeout
        handle
            .schedule_nomination_timeout(42, Duration::from_millis(50))
            .await;

        // Wait for it to fire
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Verify it fired
        assert_eq!(callback.nomination_fired.load(Ordering::SeqCst), 42);

        // Shutdown
        handle.shutdown().await;
        let _ = timeout(Duration::from_millis(100), manager_task).await;
    }

    #[tokio::test]
    async fn test_ballot_timeout_fires() {
        let callback = Arc::new(TestCallback::new());
        let (handle, manager) = TimerManager::new(callback.clone(), Arc::new(AtomicU64::new(0)));

        let manager_task = tokio::spawn(manager.run());

        // Schedule a ballot timeout
        handle
            .schedule_ballot_timeout(100, Duration::from_millis(50))
            .await;

        // Wait for it to fire
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Verify it fired
        assert_eq!(callback.ballot_fired.load(Ordering::SeqCst), 100);

        // Shutdown
        handle.shutdown().await;
        let _ = timeout(Duration::from_millis(100), manager_task).await;
    }

    #[tokio::test]
    async fn test_cancel_prevents_firing() {
        let callback = Arc::new(TestCallback::new());
        let (handle, manager) = TimerManager::new(callback.clone(), Arc::new(AtomicU64::new(0)));

        let manager_task = tokio::spawn(manager.run());

        // Schedule a timeout
        handle
            .schedule_nomination_timeout(42, Duration::from_millis(100))
            .await;

        // Cancel it before it fires
        tokio::time::sleep(Duration::from_millis(20)).await;
        handle.cancel_slot_timers(42).await;

        // Wait past when it would have fired
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Verify it did NOT fire
        assert_eq!(callback.nomination_fired.load(Ordering::SeqCst), 0);

        // Shutdown
        handle.shutdown().await;
        let _ = timeout(Duration::from_millis(100), manager_task).await;
    }

    #[tokio::test]
    async fn test_reschedule_timer() {
        let callback = Arc::new(TestCallback::new());
        let (handle, manager) = TimerManager::new(callback.clone(), Arc::new(AtomicU64::new(0)));

        let manager_task = tokio::spawn(manager.run());

        // Schedule a timeout
        handle
            .schedule_nomination_timeout(42, Duration::from_millis(50))
            .await;

        // Reschedule with longer duration
        tokio::time::sleep(Duration::from_millis(20)).await;
        handle
            .schedule_nomination_timeout(42, Duration::from_millis(200))
            .await;

        // Wait past original time but before new time
        tokio::time::sleep(Duration::from_millis(80)).await;

        // Should NOT have fired yet
        assert_eq!(callback.nomination_fired.load(Ordering::SeqCst), 0);

        // Wait for new timeout
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Now it should have fired
        assert_eq!(callback.nomination_fired.load(Ordering::SeqCst), 42);

        // Shutdown
        handle.shutdown().await;
        let _ = timeout(Duration::from_millis(100), manager_task).await;
    }

    /// Regression for #2803 (PR #2821 final bounce): the future-slot 1-second
    /// defer loop must preserve the *originating* tracking epoch across a
    /// re-arm, even if the global epoch is bumped (as on_lost_sync() does)
    /// between the timer firing and the re-arm command being processed.
    ///
    /// Without epoch preservation, `schedule_timer` re-samples the current
    /// global epoch, so a pre-sync-loss future-slot timer would be re-stamped
    /// with the new epoch and survive into Syncing — the app-layer epoch guard
    /// (which rejects events whose epoch != current) could no longer reject it.
    /// stellar-core's in-process `timerCallbackWrapper`/`setupTimer` defer loop
    /// cannot rebirth a pre-sync-loss timer this way.
    #[tokio::test]
    async fn test_reschedule_with_epoch_preserves_originating_epoch() {
        let callback = Arc::new(TestCallback::new());
        let shared_epoch = Arc::new(AtomicU64::new(0));
        let (handle, manager) = TimerManager::new(callback.clone(), shared_epoch.clone());

        let manager_task = tokio::spawn(manager.run());

        // Re-arm a nomination timer carrying the originating epoch (0) — this is
        // exactly what the Rearm branch of handle_scp_timer_event() does, using
        // the firing event's epoch rather than the current global epoch.
        handle
            .reschedule_nomination_timeout_with_epoch(42, Duration::from_millis(80), 0)
            .await;

        // Simulate on_lost_sync(): bump the global tracking epoch *after* the
        // re-arm command was sent but *before* the timer fires. A re-sampling
        // implementation would stamp the timer with epoch 1.
        shared_epoch.store(1, Ordering::Release);

        // Same race for the ballot path.
        handle
            .reschedule_ballot_timeout_with_epoch(43, Duration::from_millis(80), 0)
            .await;

        // Wait for both timers to fire.
        tokio::time::sleep(Duration::from_millis(160)).await;

        assert_eq!(callback.nomination_fired.load(Ordering::SeqCst), 42);
        assert_eq!(callback.ballot_fired.load(Ordering::SeqCst), 43);

        // The fired callbacks must carry the ORIGINATING epoch (0), not the
        // bumped global epoch (1). This is what keeps the re-armed timer
        // rejectable by the app-layer epoch guard after sync loss.
        assert_eq!(
            callback.last_nomination_epoch.load(Ordering::SeqCst),
            0,
            "re-armed nomination timer must carry the originating epoch, not the bumped global epoch"
        );
        assert_eq!(
            callback.last_ballot_epoch.load(Ordering::SeqCst),
            0,
            "re-armed ballot timer must carry the originating epoch, not the bumped global epoch"
        );

        handle.shutdown().await;
        let _ = timeout(Duration::from_millis(100), manager_task).await;
    }

    /// Sanity counterpart: a *normal* schedule (epoch = None) samples the
    /// current global epoch at schedule-time, confirming the override is the
    /// only path that preserves a stale epoch.
    #[tokio::test]
    async fn test_normal_schedule_samples_current_epoch() {
        let callback = Arc::new(TestCallback::new());
        let shared_epoch = Arc::new(AtomicU64::new(7));
        let (handle, manager) = TimerManager::new(callback.clone(), shared_epoch.clone());

        let manager_task = tokio::spawn(manager.run());

        handle
            .schedule_nomination_timeout(42, Duration::from_millis(50))
            .await;

        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(callback.nomination_fired.load(Ordering::SeqCst), 42);
        assert_eq!(
            callback.last_nomination_epoch.load(Ordering::SeqCst),
            7,
            "normal schedule must stamp the timer with the current global epoch"
        );

        handle.shutdown().await;
        let _ = timeout(Duration::from_millis(100), manager_task).await;
    }

    #[tokio::test]
    async fn test_purge_old_slots() {
        let callback = Arc::new(TestCallback::new());
        let (handle, manager) = TimerManager::new(callback.clone(), Arc::new(AtomicU64::new(0)));

        let manager_task = tokio::spawn(manager.run());

        // Schedule timers for multiple slots
        handle
            .schedule_nomination_timeout(10, Duration::from_millis(500))
            .await;
        handle
            .schedule_nomination_timeout(20, Duration::from_millis(500))
            .await;
        handle
            .schedule_nomination_timeout(30, Duration::from_millis(500))
            .await;

        // Purge slots < 25
        tokio::time::sleep(Duration::from_millis(20)).await;
        handle.purge_old_slots(25).await;

        // Wait for timeouts
        tokio::time::sleep(Duration::from_millis(600)).await;

        // Only slot 30 should have fired
        assert_eq!(callback.nomination_fired.load(Ordering::SeqCst), 30);

        // Shutdown
        handle.shutdown().await;
        let _ = timeout(Duration::from_millis(100), manager_task).await;
    }

    #[tokio::test]
    async fn test_nonblocking_schedule_nomination() {
        let callback = Arc::new(TestCallback::new());
        let (handle, manager) = TimerManager::new(callback.clone(), Arc::new(AtomicU64::new(0)));
        let manager_task = tokio::spawn(manager.run());

        handle.schedule_nomination_timeout_nonblocking(55, Duration::from_millis(50));

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(callback.nomination_fired.load(Ordering::SeqCst), 55);

        handle.shutdown().await;
        let _ = timeout(Duration::from_millis(100), manager_task).await;
    }

    #[tokio::test]
    async fn test_nonblocking_schedule_ballot() {
        let callback = Arc::new(TestCallback::new());
        let (handle, manager) = TimerManager::new(callback.clone(), Arc::new(AtomicU64::new(0)));
        let manager_task = tokio::spawn(manager.run());

        handle.schedule_ballot_timeout_nonblocking(77, Duration::from_millis(50));

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(callback.ballot_fired.load(Ordering::SeqCst), 77);

        handle.shutdown().await;
        let _ = timeout(Duration::from_millis(100), manager_task).await;
    }

    #[tokio::test]
    async fn test_nonblocking_cancel_nomination() {
        let callback = Arc::new(TestCallback::new());
        let (handle, manager) = TimerManager::new(callback.clone(), Arc::new(AtomicU64::new(0)));
        let manager_task = tokio::spawn(manager.run());

        handle.schedule_nomination_timeout_nonblocking(42, Duration::from_millis(100));
        tokio::time::sleep(Duration::from_millis(20)).await;
        handle.cancel_nomination_timer_nonblocking(42);

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(callback.nomination_fired.load(Ordering::SeqCst), 0);

        handle.shutdown().await;
        let _ = timeout(Duration::from_millis(100), manager_task).await;
    }

    #[tokio::test]
    async fn test_nonblocking_cancel_ballot() {
        let callback = Arc::new(TestCallback::new());
        let (handle, manager) = TimerManager::new(callback.clone(), Arc::new(AtomicU64::new(0)));
        let manager_task = tokio::spawn(manager.run());

        handle.schedule_ballot_timeout_nonblocking(42, Duration::from_millis(100));
        tokio::time::sleep(Duration::from_millis(20)).await;
        handle.cancel_ballot_timer_nonblocking(42);

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(callback.ballot_fired.load(Ordering::SeqCst), 0);

        handle.shutdown().await;
        let _ = timeout(Duration::from_millis(100), manager_task).await;
    }

    #[tokio::test]
    async fn test_nonblocking_cancel_slot_timers() {
        let callback = Arc::new(TestCallback::new());
        let (handle, manager) = TimerManager::new(callback.clone(), Arc::new(AtomicU64::new(0)));
        let manager_task = tokio::spawn(manager.run());

        handle.schedule_nomination_timeout_nonblocking(42, Duration::from_millis(100));
        handle.schedule_ballot_timeout_nonblocking(42, Duration::from_millis(100));
        tokio::time::sleep(Duration::from_millis(20)).await;
        handle.cancel_slot_timers_nonblocking(42);

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(callback.nomination_fired.load(Ordering::SeqCst), 0);
        assert_eq!(callback.ballot_fired.load(Ordering::SeqCst), 0);

        handle.shutdown().await;
        let _ = timeout(Duration::from_millis(100), manager_task).await;
    }

    #[test]
    fn test_no_op_handle_accepts_commands() {
        let handle = TimerManagerHandle::no_op();
        // These should not panic — the channel is open (receiver leaked)
        handle.schedule_nomination_timeout_nonblocking(1, Duration::from_secs(1));
        handle.schedule_ballot_timeout_nonblocking(1, Duration::from_secs(1));
        handle.cancel_slot_timers_nonblocking(1);
        handle.cancel_nomination_timer_nonblocking(1);
        handle.cancel_ballot_timer_nonblocking(1);
    }
}
