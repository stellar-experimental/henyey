//! Ledger close orchestration via the HerderCallback trait.
//!
//! Contains the `HerderCallback` and `SyncRecoveryCallback` trait
//! implementations for `App`, which drive ledger closing and sync recovery.

use std::sync::atomic::Ordering;

use henyey_herder::{sync_recovery::SyncRecoveryCallback, HerderCallback};
use stellar_xdr::curr::{ScpEnvelope, StellarValueExt, UpgradeType};

use super::types::{decode_upgrades, PendingLedgerClose};
use super::App;

/// Implementation of HerderCallback for App.
///
/// This enables the herder to trigger ledger closes through the app.
#[async_trait::async_trait]
impl HerderCallback for App {
    async fn close_ledger(
        &self,
        ledger_seq: u32,
        tx_set: henyey_herder::TransactionSet,
        close_time: u64,
        upgrades: Vec<UpgradeType>,
        stellar_value_ext: StellarValueExt,
    ) -> henyey_herder::Result<henyey_common::Hash256> {
        let tx_summary = tx_set.summary();
        tracing::info!(
            ledger_seq,
            tx_count = tx_set.len(),
            close_time,
            summary = %tx_summary,
            "Closing ledger"
        );

        // Get the previous ledger hash
        let prev_hash = tx_set.previous_ledger_hash();

        // Create close data
        let decoded_upgrades = decode_upgrades(upgrades);
        let mut close_data = henyey_ledger::LedgerCloseData::new(
            ledger_seq,
            tx_set.clone().into_variant(),
            close_time,
            prev_hash,
        )
        .with_stellar_value_ext(stellar_value_ext);
        if !decoded_upgrades.is_empty() {
            close_data = close_data.with_upgrades(decoded_upgrades);
        }
        if let Some(entry) = self.build_scp_history_entry(ledger_seq) {
            close_data = close_data.with_scp_history(vec![entry]);
        }

        // Close the ledger on a blocking thread (yields the tokio worker).
        let lm = self.ledger_manager.clone();
        let runtime_handle = tokio::runtime::Handle::current();
        self.set_applying_ledger(true);

        let join_handle =
            tokio::task::spawn_blocking(move || lm.close_ledger(close_data, Some(runtime_handle)));

        let mut pending = PendingLedgerClose {
            handle: join_handle,
            ledger_seq,
            tx_set,
            close_time,
            upgrades: Vec::new(),
            dispatch_time: std::time::Instant::now(),
        };

        let join_result = (&mut pending.handle).await;

        // Extract header hash before passing ownership to handle_close_complete.
        let header_hash = match &join_result {
            Ok(Ok(result)) => Some(result.header_hash),
            _ => None,
        };

        // Manual close (admin HTTP / simulation) — Inline finalizer drives
        // persist to completion before `handle_close_complete` returns.
        let success = self
            .handle_close_complete(
                pending,
                join_result,
                super::persist::LedgerCloseFinalizer::inline(),
            )
            .await;

        if success {
            Ok(header_hash.unwrap())
        } else {
            Err(henyey_herder::HerderError::Internal(format!(
                "Failed to close ledger {}",
                ledger_seq
            )))
        }
    }

    async fn broadcast_scp_message(&self, envelope: ScpEnvelope) {
        let slot = envelope.statement.slot_index;
        // Send through the channel to be picked up by the main loop
        if let Err(e) = self.scp_envelope_tx.try_send(envelope) {
            tracing::warn!(slot, error = %e, "Failed to queue SCP envelope for broadcast");
        }
    }
}

impl SyncRecoveryCallback for App {
    fn on_lost_sync(&self) {
        tracing::warn!("Lost sync with network - transitioning to syncing state");
        self.lost_sync_count.fetch_add(1, Ordering::Relaxed);
        // Update herder state to syncing
        self.herder.set_state(henyey_herder::HerderState::Syncing);
    }

    fn on_out_of_sync_recovery(&self) {
        tracing::debug!("SyncRecoveryManager triggered out-of-sync recovery");
        // Set flag so the main event loop will trigger recovery and buffered catchup.
        // The main loop checks this flag and calls maybe_start_buffered_catchup()
        // which handles the actual recovery logic including timeout-based catchup.
        self.sync_recovery_pending.store(true, Ordering::SeqCst);
    }

    fn is_applying_ledger(&self) -> bool {
        self.is_applying_ledger.load(Ordering::Relaxed)
    }

    fn is_tracking(&self) -> bool {
        self.herder.is_tracking()
    }

    fn get_v_blocking_slots(&self) -> Vec<henyey_scp::SlotIndex> {
        // Return slots where we've received v-blocking messages
        // For now, return the tracking slot range
        let tracking = self.herder.tracking_slot();
        if tracking > 0 {
            vec![tracking]
        } else {
            vec![]
        }
    }

    fn purge_slots_below(&self, slot: henyey_scp::SlotIndex) {
        tracing::debug!(slot, "Purging SCP slots below");
        self.herder.purge_slots_below(slot);
    }

    fn broadcast_latest_messages(&self, from_slot: henyey_scp::SlotIndex) {
        tracing::debug!(from_slot, "Broadcasting latest SCP messages");
        // Get and broadcast latest messages for the slot
        if let Some(messages) = self.herder.get_latest_messages(from_slot) {
            for envelope in messages {
                let _ = self.scp_envelope_tx.try_send(envelope);
            }
        }
    }

    fn request_scp_state_from_peers(&self) {
        // Can't call async from sync callback, so just log the request.
        // The main event loop's heartbeat will pick up stalled state and request SCP state.
        tracing::debug!("Sync recovery requested SCP state - will be handled by main loop");
    }
}
