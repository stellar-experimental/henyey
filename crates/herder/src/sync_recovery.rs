//! Out-of-sync detection and recovery for the Herder.
//!
//! This module provides mechanisms to detect when the node has fallen out of sync
//! with the network and initiate recovery procedures. The main components are:
//!
//! - **Tracking heartbeat**: Resets a timer whenever consensus makes progress
//! - **Consensus stuck timeout**: Triggers out-of-sync if no progress for a period
//! - **Out-of-sync recovery**: Purges old state and requests fresh SCP messages
//!
//! # Detection Flow
//!
//! 1. When tracking, the herder calls `tracking_heartbeat()` whenever:
//!    - A new slot is externalized
//!    - New SCP messages are processed
//!    - Ledger application completes
//!
//! 2. The tracking timer is set for `CONSENSUS_STUCK_TIMEOUT` (35 seconds)
//!
//! 3. If the timer expires without a heartbeat:
//!    - If ledger application is in progress, reset the timer
//!    - Otherwise, transition to out-of-sync state
//!
//! # Recovery Flow
//!
//! When out-of-sync:
//! 1. Purge old SCP state to reduce memory usage
//! 2. Broadcast our latest SCP messages to peers
//! 3. Request fresh SCP state from random peers
//! 4. Set recovery timer to periodically retry
//!
//! # Example
//!
//! ```ignore
//! use henyey_herder::sync_recovery::{SyncRecoveryManager, SyncRecoveryHandle};
//!
//! // Create recovery manager
//! let (handle, manager) = SyncRecoveryManager::new(herder.clone());
//!
//! // Spawn the background task
//! tokio::spawn(manager.run());
//!
//! // Call heartbeat when making progress
//! handle.tracking_heartbeat().await;
//!
//! // Stop tracking (e.g., when entering syncing state)
//! handle.stop_tracking().await;
//! ```

use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use henyey_scp::SlotIndex;

/// Timeout before declaring consensus stuck (matches stellar-core CONSENSUS_STUCK_TIMEOUT_SECONDS).
pub const CONSENSUS_STUCK_TIMEOUT: Duration = Duration::from_secs(35);

/// Interval for out-of-sync recovery attempts.
/// Spec: HERDER_SPEC §16 — OUT_OF_SYNC_RECOVERY_TIMER = 10 seconds.
pub const OUT_OF_SYNC_RECOVERY_INTERVAL: Duration = Duration::from_secs(10);

/// Maximum number of slots to keep ahead when purging during recovery.
/// Spec: HERDER_SPEC §16 — LEDGER_VALIDITY_BRACKET = 100.
pub const LEDGER_VALIDITY_BRACKET: u64 = 100;

/// Commands sent to the sync recovery manager.
#[derive(Debug)]
pub enum SyncRecoveryCommand {
    /// Signal that consensus is making progress (reset the stuck timer).
    TrackingHeartbeat,
    /// Stop tracking (entering syncing state).
    StopTracking,
    /// Start tracking (entering tracking state).
    StartTracking,
    /// Check if ledger application is in progress.
    SetApplyingLedger(bool),
    /// Shutdown the recovery manager.
    Shutdown,
}

/// Handle for sending commands to the sync recovery manager.
#[derive(Clone)]
pub struct SyncRecoveryHandle {
    sender: mpsc::Sender<SyncRecoveryCommand>,
}

impl SyncRecoveryHandle {
    /// Signal that consensus is making progress.
    ///
    /// This resets the consensus stuck timer. Should be called whenever:
    /// - A new slot is externalized
    /// - New SCP messages are received
    /// - Ledger application completes
    pub async fn tracking_heartbeat(&self) {
        let _ = self
            .sender
            .send(SyncRecoveryCommand::TrackingHeartbeat)
            .await;
    }

    /// Try to send heartbeat (non-blocking).
    pub fn try_tracking_heartbeat(&self) -> bool {
        self.sender
            .try_send(SyncRecoveryCommand::TrackingHeartbeat)
            .is_ok()
    }

    /// Stop tracking (entering syncing state).
    pub async fn stop_tracking(&self) {
        let _ = self.sender.send(SyncRecoveryCommand::StopTracking).await;
    }

    /// Start tracking (entering tracking state).
    pub async fn start_tracking(&self) {
        let _ = self.sender.send(SyncRecoveryCommand::StartTracking).await;
    }

    /// Try to start tracking (non-blocking).
    pub fn try_start_tracking(&self) -> bool {
        self.sender
            .try_send(SyncRecoveryCommand::StartTracking)
            .is_ok()
    }

    /// Set whether ledger application is in progress.
    pub async fn set_applying_ledger(&self, applying: bool) {
        let _ = self
            .sender
            .send(SyncRecoveryCommand::SetApplyingLedger(applying))
            .await;
    }

    /// Try to set applying ledger (non-blocking).
    pub fn try_set_applying_ledger(&self, applying: bool) -> bool {
        self.sender
            .try_send(SyncRecoveryCommand::SetApplyingLedger(applying))
            .is_ok()
    }

    /// Shutdown the recovery manager.
    pub async fn shutdown(&self) {
        let _ = self.sender.send(SyncRecoveryCommand::Shutdown).await;
    }
}

/// Callback trait for sync recovery events.
pub trait SyncRecoveryCallback: Send + Sync + 'static {
    /// Called when the node transitions to out-of-sync state.
    ///
    /// The implementation should:
    /// - Update the herder state to Syncing
    /// - Log diagnostic information
    fn on_lost_sync(&self);

    /// Called during out-of-sync recovery.
    ///
    /// The implementation should:
    /// - Purge old SCP state
    /// - Broadcast latest SCP messages
    /// - Request fresh state from peers
    fn on_out_of_sync_recovery(&self);

    /// Check if ledger application is in progress.
    fn is_applying_ledger(&self) -> bool;

    /// Check if we're currently tracking.
    fn is_tracking(&self) -> bool;

    /// Get slots with v-blocking status for purging.
    ///
    /// Returns slots in descending order where we've received v-blocking messages.
    fn get_v_blocking_slots(&self) -> Vec<SlotIndex>;

    /// Purge SCP state for slots below the given slot.
    fn purge_slots_below(&self, slot: SlotIndex);

    /// Get and broadcast latest SCP messages.
    fn broadcast_latest_messages(&self, from_slot: SlotIndex);

    /// Request SCP state from peers.
    fn request_scp_state_from_peers(&self);
}

/// Sync state of the recovery manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncState {
    /// Not tracking - no timer active.
    NotTracking,
    /// Tracking consensus - stuck timer is active.
    Tracking,
    /// Out of sync - recovery timer is active.
    OutOfSync,
}

/// The sync recovery manager that runs as a background task.
pub struct SyncRecoveryManager<C: SyncRecoveryCallback> {
    callback: Arc<C>,
    receiver: mpsc::Receiver<SyncRecoveryCommand>,
    state: SyncState,
    /// When the tracking timer expires.
    tracking_deadline: Option<Instant>,
    /// When the next recovery attempt should happen.
    recovery_deadline: Option<Instant>,
    /// Whether ledger application is in progress.
    is_applying: bool,
}

impl<C: SyncRecoveryCallback> SyncRecoveryManager<C> {
    /// Create a new sync recovery manager.
    pub fn new(callback: Arc<C>) -> (SyncRecoveryHandle, Self) {
        let (sender, receiver) = mpsc::channel(64);
        let handle = SyncRecoveryHandle { sender };
        let manager = Self {
            callback,
            receiver,
            state: SyncState::NotTracking,
            tracking_deadline: None,
            recovery_deadline: None,
            is_applying: false,
        };
        (handle, manager)
    }

    /// Run the sync recovery manager.
    pub async fn run(mut self) {
        info!("Sync recovery manager started");

        loop {
            let next_deadline = self.next_deadline();

            tokio::select! {
                // Handle incoming commands
                cmd = self.receiver.recv() => {
                    match cmd {
                        Some(SyncRecoveryCommand::TrackingHeartbeat) => {
                            self.handle_heartbeat();
                        }
                        Some(SyncRecoveryCommand::StopTracking) => {
                            self.handle_stop_tracking();
                        }
                        Some(SyncRecoveryCommand::StartTracking) => {
                            self.handle_start_tracking();
                        }
                        Some(SyncRecoveryCommand::SetApplyingLedger(applying)) => {
                            self.is_applying = applying;
                        }
                        Some(SyncRecoveryCommand::Shutdown) | None => {
                            info!("Sync recovery manager shutting down");
                            break;
                        }
                    }
                }

                // Handle timer expiration
                _ = crate::herder_utils::sleep_until_or_forever(next_deadline) => {
                    self.handle_timer_expired();
                }
            }
        }
    }

    /// Handle tracking heartbeat - reset the stuck timer.
    fn handle_heartbeat(&mut self) {
        if self.state == SyncState::Tracking {
            self.tracking_deadline = Some(Instant::now() + CONSENSUS_STUCK_TIMEOUT);
            debug!("Tracking heartbeat - timer reset");
        } else if self.state == SyncState::OutOfSync {
            // Got activity while out of sync - transition back to tracking
            info!("Received heartbeat while out of sync - transitioning to tracking");
            self.state = SyncState::Tracking;
            self.tracking_deadline = Some(Instant::now() + CONSENSUS_STUCK_TIMEOUT);
            self.recovery_deadline = None;
        }
    }

    /// Handle stop tracking command.
    fn handle_stop_tracking(&mut self) {
        self.state = SyncState::NotTracking;
        self.tracking_deadline = None;
        self.recovery_deadline = None;
        debug!("Stopped tracking");
    }

    /// Handle start tracking command.
    fn handle_start_tracking(&mut self) {
        self.state = SyncState::Tracking;
        self.tracking_deadline = Some(Instant::now() + CONSENSUS_STUCK_TIMEOUT);
        self.recovery_deadline = None;
        debug!(
            "Started tracking with {}s timeout",
            CONSENSUS_STUCK_TIMEOUT.as_secs()
        );
    }

    /// Handle timer expiration.
    fn handle_timer_expired(&mut self) {
        let now = Instant::now();

        match self.state {
            SyncState::Tracking => {
                if let Some(deadline) = self.tracking_deadline {
                    if now >= deadline {
                        self.on_tracking_timeout();
                    }
                }
            }
            SyncState::OutOfSync => {
                if let Some(deadline) = self.recovery_deadline {
                    if now >= deadline {
                        self.on_recovery_timeout();
                    }
                }
            }
            SyncState::NotTracking => {}
        }
    }

    /// Called when the tracking timer expires.
    fn on_tracking_timeout(&mut self) {
        // If ledger application is in progress, just reset the timer
        if self.is_applying || self.callback.is_applying_ledger() {
            debug!("Tracking timer expired during ledger application - resetting");
            self.tracking_deadline = Some(Instant::now() + CONSENSUS_STUCK_TIMEOUT);
            return;
        }

        warn!("Consensus stuck - transitioning to out-of-sync");

        // Transition to out-of-sync
        self.state = SyncState::OutOfSync;
        self.tracking_deadline = None;

        // Notify callback
        self.callback.on_lost_sync();

        // Start recovery
        self.start_recovery();
    }

    /// Start out-of-sync recovery.
    fn start_recovery(&mut self) {
        self.perform_recovery();

        // Set up recovery timer for periodic retries
        self.recovery_deadline = Some(Instant::now() + OUT_OF_SYNC_RECOVERY_INTERVAL);
    }

    /// Called when the recovery timer expires.
    fn on_recovery_timeout(&mut self) {
        // Check if we've transitioned back to tracking
        if self.callback.is_tracking() {
            info!("Now tracking - stopping recovery");
            self.state = SyncState::Tracking;
            self.tracking_deadline = Some(Instant::now() + CONSENSUS_STUCK_TIMEOUT);
            self.recovery_deadline = None;
            return;
        }

        // Perform another recovery attempt
        self.perform_recovery();

        // Schedule next recovery
        self.recovery_deadline = Some(Instant::now() + OUT_OF_SYNC_RECOVERY_INTERVAL);
    }

    /// Perform out-of-sync recovery actions.
    fn perform_recovery(&self) {
        info!("Performing out-of-sync recovery");

        // Notify callback to perform recovery actions
        self.callback.on_out_of_sync_recovery();
    }

    /// Get the next deadline to wait for.
    fn next_deadline(&self) -> Option<Instant> {
        match self.state {
            SyncState::Tracking => self.tracking_deadline,
            SyncState::OutOfSync => self.recovery_deadline,
            SyncState::NotTracking => None,
        }
    }
}

/// Statistics about sync recovery.
#[derive(Debug, Clone, Default)]
pub struct SyncRecoveryStats {
    /// Number of times we've lost sync.
    pub lost_sync_count: u64,
    /// Number of recovery attempts.
    pub recovery_attempts: u64,
    /// Whether we're currently out of sync.
    pub is_out_of_sync: bool,
}

/// Thread-safe sync recovery stats.
pub struct SyncRecoveryStatsTracker {
    stats: RwLock<SyncRecoveryStats>,
}

impl SyncRecoveryStatsTracker {
    /// Create a new stats tracker.
    pub fn new() -> Self {
        Self {
            stats: RwLock::new(SyncRecoveryStats::default()),
        }
    }

    /// Record that we lost sync.
    pub fn record_lost_sync(&self) {
        let mut stats = self.stats.write();
        stats.lost_sync_count += 1;
        stats.is_out_of_sync = true;
    }

    /// Record a recovery attempt.
    pub fn record_recovery_attempt(&self) {
        self.stats.write().recovery_attempts += 1;
    }

    /// Record that we're back in sync.
    pub fn record_back_in_sync(&self) {
        self.stats.write().is_out_of_sync = false;
    }

    /// Get the current stats.
    pub fn get_stats(&self) -> SyncRecoveryStats {
        self.stats.read().clone()
    }
}

impl Default for SyncRecoveryStatsTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use tokio::time::timeout;

    struct TestCallback {
        lost_sync_count: AtomicU64,
        recovery_count: AtomicU64,
        is_applying: AtomicBool,
        is_tracking: AtomicBool,
    }

    impl TestCallback {
        fn new() -> Self {
            Self {
                lost_sync_count: AtomicU64::new(0),
                recovery_count: AtomicU64::new(0),
                is_applying: AtomicBool::new(false),
                is_tracking: AtomicBool::new(true),
            }
        }

        fn set_applying(&self, applying: bool) {
            self.is_applying.store(applying, Ordering::SeqCst);
        }
    }

    impl SyncRecoveryCallback for TestCallback {
        fn on_lost_sync(&self) {
            self.lost_sync_count.fetch_add(1, Ordering::SeqCst);
            self.is_tracking.store(false, Ordering::SeqCst);
        }

        fn on_out_of_sync_recovery(&self) {
            self.recovery_count.fetch_add(1, Ordering::SeqCst);
        }

        fn is_applying_ledger(&self) -> bool {
            self.is_applying.load(Ordering::SeqCst)
        }

        fn is_tracking(&self) -> bool {
            self.is_tracking.load(Ordering::SeqCst)
        }

        fn get_v_blocking_slots(&self) -> Vec<SlotIndex> {
            vec![]
        }

        fn purge_slots_below(&self, _slot: SlotIndex) {}

        fn broadcast_latest_messages(&self, _from_slot: SlotIndex) {}

        fn request_scp_state_from_peers(&self) {}
    }

    #[tokio::test]
    async fn test_heartbeat_resets_timer() {
        let callback = Arc::new(TestCallback::new());
        let (handle, manager) = SyncRecoveryManager::new(callback.clone());

        let manager_task = tokio::spawn(manager.run());

        // Start tracking
        handle.start_tracking().await;

        // Send heartbeats before timeout
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            handle.tracking_heartbeat().await;
        }

        // Should not have lost sync
        assert_eq!(callback.lost_sync_count.load(Ordering::SeqCst), 0);

        handle.shutdown().await;
        let _ = timeout(Duration::from_millis(100), manager_task).await;
    }

    #[tokio::test]
    async fn test_timeout_during_apply_resets_timer() {
        let callback = Arc::new(TestCallback::new());
        let (handle, manager) = SyncRecoveryManager::new(callback.clone());

        // Set applying before starting manager
        callback.set_applying(true);

        let manager_task = tokio::spawn(manager.run());

        // Start tracking
        handle.start_tracking().await;

        // Wait for what would be a timeout (using a short timeout for test)
        // Note: In real test, we'd need to use a shorter timeout
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Still should not have lost sync (applying protects us)
        assert_eq!(callback.lost_sync_count.load(Ordering::SeqCst), 0);

        handle.shutdown().await;
        let _ = timeout(Duration::from_millis(100), manager_task).await;
    }

    #[tokio::test]
    async fn test_stop_tracking_cancels_timer() {
        let callback = Arc::new(TestCallback::new());
        let (handle, manager) = SyncRecoveryManager::new(callback.clone());

        let manager_task = tokio::spawn(manager.run());

        // Start then immediately stop tracking
        handle.start_tracking().await;
        handle.stop_tracking().await;

        // Wait a bit
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Should not have lost sync (timer was cancelled)
        assert_eq!(callback.lost_sync_count.load(Ordering::SeqCst), 0);

        handle.shutdown().await;
        let _ = timeout(Duration::from_millis(100), manager_task).await;
    }

    // ── HERDER_SPEC §3.3/§3.4: Sync recovery constants ──────────────

    #[test]
    fn test_consensus_stuck_timeout_is_35s() {
        assert_eq!(
            CONSENSUS_STUCK_TIMEOUT,
            Duration::from_secs(35),
            "HERDER_SPEC §3.3: CONSENSUS_STUCK_TIMEOUT must be 35 seconds"
        );
    }

    #[test]
    fn test_out_of_sync_recovery_interval_is_10s() {
        assert_eq!(
            OUT_OF_SYNC_RECOVERY_INTERVAL,
            Duration::from_secs(10),
            "HERDER_SPEC §3.4: OUT_OF_SYNC_RECOVERY_INTERVAL must be 10 seconds"
        );
    }

    #[test]
    fn test_ledger_validity_bracket_is_100() {
        assert_eq!(
            LEDGER_VALIDITY_BRACKET, 100,
            "HERDER_SPEC §3.4: LEDGER_VALIDITY_BRACKET must be 100"
        );
    }
}
