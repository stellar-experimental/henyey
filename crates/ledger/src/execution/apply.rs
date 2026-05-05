//! Transaction apply body (operation execution phase).
//!
//! Contains `apply_body` and its helper functions: `setup_entry_loaders`,
//! `rollback_failed_tx`, `commit_successful_tx`, and the free function
//! `collect_soroban_restored_entries`. Extracted from the main executor module
//! for readability.

use std::collections::{HashMap, HashSet};

use henyey_crypto::account_id_to_strkey;
use stellar_xdr::curr::{
    AccountId, ContractEvent, DiagnosticEvent, LedgerKey, OperationBody, OperationResult,
    OperationType, SorobanTransactionData, TransactionResultCode, TrustLineFlags,
};
use tracing::debug;

/// Threshold in microseconds above which a classic transaction is logged as slow.
/// Soroban transactions are always logged regardless of duration.
const SLOW_TX_LOG_THRESHOLD_US: u64 = 5000;

use henyey_tx::{operations::OperationTypeExt, LedgerContext, OpEventManager, TransactionFrame};

use crate::snapshot::SnapshotHandle;
use crate::Result;

use super::meta::*;
use super::result_mapping::*;
use super::signatures::*;
use super::{
    DeltaSlice, FeeMode, OperationExecutionRequest, PreApplyResult, PreApplySnapshot,
    RefundableFeeTracker, TransactionExecutionResult, TransactionExecutor, TxExecTimings,
};

pub(super) const AUTHORIZED_FLAG: u32 = TrustLineFlags::AuthorizedFlag as u32;

/// Describes how a ledger key was restored during Soroban execution (CAP-0066).
///
/// Structural guarantees:
/// - `HotArchive` (data/code keys): always carries the original entry for meta comparison.
/// - `HotArchiveTtl` (synthesized TTL keys): no original entry exists (TTL is computed
///   from the data/code key hash, not restored from stored state).
/// - `LiveBucketList`: always carries the original entry (both data/code and TTL).
#[derive(Debug, Clone)]
pub(super) enum RestoreSource {
    /// Data/code key restored from hot archive with its original entry.
    HotArchive(Box<stellar_xdr::curr::LedgerEntry>),
    /// Synthesized TTL key for a hot-archive-restored entry. No original entry
    /// exists because the TTL is computed from the data/code key hash.
    HotArchiveTtl,
    /// Key restored from live bucket list with its original entry.
    LiveBucketList(Box<stellar_xdr::curr::LedgerEntry>),
}

/// Tracks entries restored from different sources per CAP-0066 (Soroban only).
///
/// Uses a single map to enforce that a key cannot appear in both hot-archive
/// and live-BL sources simultaneously (type-enforced mutual exclusion).
#[derive(Debug)]
pub struct RestoredEntries {
    entries: HashMap<LedgerKey, RestoreSource>,
}

impl RestoredEntries {
    pub(super) fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    // --- Query ---

    /// Returns the restore source for a key, enabling exhaustive `match`.
    pub(super) fn source(&self, key: &LedgerKey) -> Option<&RestoreSource> {
        self.entries.get(key)
    }

    /// Check if a key was restored from the hot archive (any variant).
    pub(super) fn is_hot_archive_restored(&self, key: &LedgerKey) -> bool {
        matches!(
            self.entries.get(key),
            Some(RestoreSource::HotArchive(_) | RestoreSource::HotArchiveTtl)
        )
    }

    /// Check if a key was restored from the live bucket list.
    pub(super) fn is_live_bl_restored(&self, key: &LedgerKey) -> bool {
        matches!(
            self.entries.get(key),
            Some(RestoreSource::LiveBucketList(_))
        )
    }

    /// Check if a key was restored from either source.
    pub(super) fn is_restored(&self, key: &LedgerKey) -> bool {
        self.entries.contains_key(key)
    }

    // --- Hot archive insertion ---

    /// Insert a data/code key restored from the hot archive with its original entry.
    pub(super) fn insert_hot_archive_entry(
        &mut self,
        key: LedgerKey,
        original: stellar_xdr::curr::LedgerEntry,
    ) {
        assert!(
            !matches!(key, LedgerKey::Ttl(_)),
            "insert_hot_archive_entry called with TTL key — use insert_hot_archive_ttl"
        );
        assert!(
            !matches!(
                self.entries.get(&key),
                Some(RestoreSource::LiveBucketList(_))
            ),
            "key already restored from live BL: {key:?}"
        );
        assert!(
            !self.entries.contains_key(&key),
            "duplicate hot archive entry insertion for key: {key:?}"
        );
        self.entries
            .insert(key, RestoreSource::HotArchive(Box::new(original)));
    }

    /// Insert a synthesized TTL key for a hot-archive-restored entry.
    pub(super) fn insert_hot_archive_ttl(&mut self, key: LedgerKey) {
        assert!(
            matches!(key, LedgerKey::Ttl(_)),
            "insert_hot_archive_ttl called with non-TTL key: {key:?}"
        );
        assert!(
            !matches!(
                self.entries.get(&key),
                Some(RestoreSource::LiveBucketList(_))
            ),
            "TTL key already restored from live BL: {key:?}"
        );
        // TTL keys may be re-inserted (once from safety-net loop, once from
        // extend_hot_archive_ttls) — allow idempotent insertion for HotArchiveTtl.
        self.entries.insert(key, RestoreSource::HotArchiveTtl);
    }

    /// Bulk-insert synthesized TTL keys into the hot archive.
    pub(super) fn extend_hot_archive_ttls(&mut self, keys: impl IntoIterator<Item = LedgerKey>) {
        for key in keys {
            self.insert_hot_archive_ttl(key);
        }
    }

    /// Iterate hot-archive entries that have original values (data/code keys only).
    pub(super) fn hot_archive_entries_with_originals(
        &self,
    ) -> impl Iterator<Item = (&LedgerKey, &stellar_xdr::curr::LedgerEntry)> {
        self.entries.iter().filter_map(|(k, v)| match v {
            RestoreSource::HotArchive(entry) => Some((k, entry.as_ref())),
            _ => None,
        })
    }

    // --- Live BL insertion ---

    /// Insert a key restored from the live BucketList with its original entry.
    pub(super) fn insert_live_bl(
        &mut self,
        key: LedgerKey,
        original: stellar_xdr::curr::LedgerEntry,
    ) {
        assert!(
            !matches!(
                self.entries.get(&key),
                Some(RestoreSource::HotArchive(_) | RestoreSource::HotArchiveTtl)
            ),
            "key already restored from hot archive: {key:?}"
        );
        self.entries
            .insert(key, RestoreSource::LiveBucketList(Box::new(original)));
    }

    /// Iterate all live-BL entries (key + original entry).
    pub(super) fn live_bl_entries(
        &self,
    ) -> impl Iterator<Item = (&LedgerKey, &stellar_xdr::curr::LedgerEntry)> {
        self.entries.iter().filter_map(|(k, v)| match v {
            RestoreSource::LiveBucketList(entry) => Some((k, entry.as_ref())),
            _ => None,
        })
    }

    /// Number of live-BL restored keys.
    pub(super) fn live_bl_len(&self) -> usize {
        self.entries
            .values()
            .filter(|v| matches!(v, RestoreSource::LiveBucketList(_)))
            .count()
    }
}

impl Default for RestoredEntries {
    fn default() -> Self {
        Self::new()
    }
}

/// Collect restored entries for a Soroban operation from both the hot archive
/// and the live bucket list.
///
/// This processes the operation's Soroban meta to determine which entries were
/// restored, filters out live BL restores from the hot archive set, computes
/// TTL keys for restored entries, and accumulates hot archive keys for the
/// bucket list batch.
// SECURITY: Soroban ops are all-or-nothing; partial restore cannot occur in successful execution
// INVARIANT: failed tx hot-archive export cannot happen; Soroban ops are all-or-nothing
///
/// # Arguments
/// - `soroban_meta`: The Soroban execution metadata (may contain restore info)
/// - `soroban_data`: The transaction's Soroban resource data
/// - `op_type`: The operation type (RestoreFootprint vs InvokeHostFunction)
/// - `op_result`: The operation result (only successful ops contribute to hot archive)
/// - `delta_slice`: The delta slice for this operation (used to determine created keys)
/// - `collected_hot_archive_keys`: Accumulator for hot archive keys across all ops
pub(super) fn collect_soroban_restored_entries(
    soroban_meta: &Option<henyey_tx::operations::execute::SorobanOperationMeta>,
    soroban_data: Option<&SorobanTransactionData>,
    op_type: OperationType,
    op_result: &OperationResult,
    delta_slice: &DeltaSlice<'_>,
    collected_hot_archive_keys: &mut HashSet<LedgerKey>,
) -> RestoredEntries {
    use sha2::{Digest, Sha256};

    let mut restored = RestoredEntries::default();

    // Get live BL restorations from the Soroban execution result
    if let Some(meta) = soroban_meta {
        for live_bl_restore in &meta.live_bucket_list_restores {
            restored.insert_live_bl(live_bl_restore.key.clone(), live_bl_restore.entry.clone());
            restored.insert_live_bl(
                live_bl_restore.ttl_key.clone(),
                live_bl_restore.ttl_entry.clone(),
            );
        }
    }

    // Get hot archive keys from two sources:
    // 1. For InvokeHostFunction: from actual_restored_indices (filtered by host)
    // 2. For RestoreFootprint: from soroban_meta.hot_archive_restores
    // NOTE: We must exclude live BL restore keys from the hot archive set.
    // Live BL restores are entries that exist in the live bucket list with
    // expired TTL but haven't been evicted yet - these are NOT hot archive
    // restores and should not be added to HotArchiveBucketList::add_batch.
    let actual_restored_indices = soroban_meta
        .as_ref()
        .map(|m| m.actual_restored_indices.as_slice())
        .unwrap_or(&[]);
    let mut hot_archive =
        extract_hot_archive_restored_keys(soroban_data, op_type, actual_restored_indices);
    // For RestoreFootprint and InvokeHostFunction, add hot archive keys from the meta.
    // Entry values are stored AFTER filtering (see below) to avoid spurious RESTORED
    // emissions for read-only auto-restores.
    if let Some(meta) = soroban_meta {
        for ha_restore in &meta.hot_archive_restores {
            hot_archive.insert(ha_restore.key.clone());
        }
    }
    let ha_before = hot_archive.len();
    hot_archive.retain(|k| !restored.is_live_bl_restored(k));
    let ha_after_live_bl = hot_archive.len();

    // Also exclude keys that were listed in archived_soroban_entries but
    // were already restored by a previous TX in this ledger. These entries
    // go into `updated` (not `created`) because they already exist in state.
    // We only want RESTORED emission for entries actually being created/restored
    // in THIS transaction.
    let created_keys: HashSet<LedgerKey> = delta_slice
        .created()
        .iter()
        .map(|entry| henyey_common::entry_to_key(entry))
        .collect();
    // For transaction meta emission: only emit RESTORED for keys in created
    // Keep original set for bucket list operations
    // For RestoreFootprint, the data entries are prefetched from hot archive
    // into state, so they won't be in `created_keys` (only the TTL is created).
    // We need to emit RESTORED for all hot archive keys without filtering.
    // For InvokeHostFunction, we filter by created_keys because the auto-restore
    // creates the entries during execution.
    let (hot_archive_for_bucket_list, hot_archive_for_meta) =
        if op_type == OperationType::RestoreFootprint {
            // Both need the full set — clone once, move the other
            let for_meta = hot_archive.clone();
            (hot_archive, for_meta)
        } else {
            // Filter by created_keys for InvokeHostFunction
            let for_meta = hot_archive
                .iter()
                .filter(|k| created_keys.contains(k))
                .cloned()
                .collect();
            (hot_archive, for_meta)
        };

    // Now store original entry values ONLY for keys that passed the meta filter.
    // This prevents read-only auto-restores from emitting spurious RESTORED changes
    // via hot_archive_entries_with_originals() post-processing at meta.rs.
    if let Some(meta) = soroban_meta {
        for ha_restore in &meta.hot_archive_restores {
            if hot_archive_for_meta.contains(&ha_restore.key) {
                restored.insert_hot_archive_entry(ha_restore.key.clone(), ha_restore.entry.clone());
            }
        }
    }
    let ha_after = hot_archive_for_meta.len();
    // Log when we filter out entries
    if ha_before != ha_after {
        tracing::debug!(
            ha_before,
            ha_after_live_bl,
            ha_after,
            live_bl_count = restored.live_bl_len(),
            created_count = created_keys.len(),
            ?hot_archive_for_bucket_list,
            ?created_keys,
            op_type = ?op_type,
            "Filtered hot archive keys: live BL restores and already-restored entries"
        );
    }
    // For transaction meta purposes, also add the corresponding TTL keys.
    // When a ContractData/ContractCode entry is restored from hot archive,
    // its TTL entry should also be emitted as RESTORED (not CREATED).
    // Use the filtered set (hot_archive_for_meta) which only includes entries
    // actually being created/restored in this TX.
    // NOTE: We don't add TTL keys to collected_hot_archive_keys because
    // HotArchiveBucketList::add_batch only receives data/code entries.
    let ttl_keys: Vec<_> = hot_archive_for_meta
        .iter()
        .map(|key| {
            let key_bytes = henyey_common::xdr_to_bytes(key);
            let key_hash = stellar_xdr::curr::Hash(Sha256::digest(&key_bytes).into());
            LedgerKey::Ttl(stellar_xdr::curr::LedgerKeyTtl { key_hash })
        })
        .collect();
    // Collect data/code keys only for HotArchiveBucketList::add_batch.
    // All hot archive keys (already filtered by live BL above) should be
    // passed to the bucket list. This is true for both RestoreFootprint
    // and InvokeHostFunction - the hot archive needs to remove ALL entries
    // that were restored, regardless of whether the contract then modifies
    // them (which would put them in `updated` rather than `created`).
    // The `created_keys` filtering above is only for transaction meta
    // emission (RESTORED vs UPDATED), not for bucket list operations.
    //
    // IMPORTANT: Only collect hot archive keys for SUCCESSFUL operations.
    // In stellar-core, handleArchivedEntry writes the restoration to
    // mOpState (a nested LedgerTxn), but if the operation fails, that
    // nested LedgerTxn is rolled back, canceling the restorations and
    // preventing any HOT_ARCHIVE_LIVE tombstones from being written.
    // For failed operations, we must not add keys to the hot archive
    // batch — doing so would produce spurious HOT_ARCHIVE_LIVE tombstones
    // in the hot archive bucket list, causing a bucket_list_hash mismatch.
    if is_operation_success(op_result) {
        collected_hot_archive_keys.extend(hot_archive_for_bucket_list.iter().cloned());
    }
    // Add filtered keys to restored for meta conversion.
    // Non-TTL keys from hot_archive_for_meta must already have been inserted
    // via insert_hot_archive_entry (from hot_archive_restores). Only TTL keys
    // should reach the insert path here — this is consistent with the assert
    // in meta.rs that non-TTL keys must have an original entry.
    for key in hot_archive_for_meta {
        if !restored.is_hot_archive_restored(&key) {
            assert!(
                matches!(key, LedgerKey::Ttl(_)),
                "non-TTL hot archive key without original entry: {key:?}"
            );
            restored.insert_hot_archive_ttl(key);
        }
    }
    restored.extend_hot_archive_ttls(ttl_keys);
    restored
}

impl TransactionExecutor {
    /// Apply phase: execute operations, build meta, handle rollback on failure.
    ///
    /// This is the second half of transaction execution, consuming the
    /// `PreApplyResult` produced by `pre_apply()`. It loads footprint entries,
    /// executes each operation, handles rollback on failure (restoring
    /// fee/seq/signer entries from the pre-apply phase), builds transaction
    /// meta, and returns the final `TransactionExecutionResult`.
    ///
    /// This matches the body of stellar-core's `parallelApply` (for Soroban) /
    /// `TransactionFrame::apply` (for classic) — everything after `commonPreApply`.
    pub(super) fn apply_body(
        &mut self,
        snapshot: &SnapshotHandle,
        pre: PreApplyResult,
    ) -> Result<TransactionExecutionResult> {
        let PreApplyResult {
            frame,
            fee_source_id,
            inner_source_id,
            tx_changes_before,
            fee_changes,
            mut refundable_fee_tracker,
            mut tx_event_manager,
            preflight_failure,
            sig_check_failure,
            fee,
            fee_entries,
            preapply_entries,
            soroban_prng_seed,
            base_fee,
            fee_mode,
            validation_us,
            fee_seq_us,
            tx_timing_start,
            val_account_load_us,
            val_tx_hash_us,
            val_ed25519_us,
            val_other_us,
            fee_deduct_us,
            op_sig_check_us,
            signer_removal_us,
            seq_bump_us,
            tx_hash,
        } = pre;

        // Create ledger context for operation execution
        let ledger_context = if let Some(prng_seed) = soroban_prng_seed {
            LedgerContext::with_prng_seed(
                self.ledger_seq,
                self.close_time,
                base_fee,
                self.base_reserve,
                self.protocol_version,
                self.network_id,
                prng_seed,
            )
        } else {
            LedgerContext::new(
                self.ledger_seq,
                self.close_time,
                base_fee,
                self.base_reserve,
                self.protocol_version,
                self.network_id,
            )
        };
        // CAP-77: Propagate frozen key config so DEX crossing can skip/delete frozen offers.
        let mut ledger_context = ledger_context;
        ledger_context.frozen_key_config = self.frozen_key_config.clone();
        ledger_context.ledger_flags = self.ledger_flags;

        let soroban_data = frame.soroban_data();

        // For Soroban transactions, load all footprint entries from the snapshot
        // before executing operations. This ensures contract data, code, and TTLs
        // are available to the Soroban host.
        //
        // NOTE: We no longer call clear_archived_entries_from_state() here because
        // archived entries from the hot archive need to be available to the Soroban
        // host for restoration. The previous approach of clearing them was designed
        // for when all entries came from the live bucket list, but with hot archive
        // support, archived entries are properly sourced and must be preserved.
        if let Some(data) = soroban_data {
            self.load_soroban_footprint(snapshot, &data.resources.footprint)?;
        }

        let footprint_us =
            tx_timing_start.elapsed().as_micros() as u64 - validation_us - fee_seq_us;

        self.state.clear_sponsorship_stack();

        // Pre-load sponsor accounts for BeginSponsoringFutureReserves operations.
        // When a BeginSponsoringFutureReserves operation is followed by other operations
        // (like SetOptions), those subsequent operations may need to update the sponsor's
        // num_sponsoring count. We must load these sponsor accounts before the operation
        // loop so they're available when needed.
        for op in frame.operations().iter() {
            if let OperationBody::BeginSponsoringFutureReserves(_) = &op.body {
                // The sponsor is the source of the BeginSponsoringFutureReserves operation
                let op_source_muxed = op
                    .source_account
                    .clone()
                    .unwrap_or_else(|| frame.inner_source_account());
                let sponsor_id = henyey_tx::muxed_to_account_id(&op_source_muxed);
                self.load_account(snapshot, &sponsor_id)?;
            }
        }

        self.setup_entry_loaders(snapshot);

        // Execute operations
        let mut operation_results = Vec::new();
        let num_ops = frame.operations().len();
        let mut op_changes = Vec::with_capacity(num_ops);
        let mut op_events: Vec<Vec<ContractEvent>> = Vec::with_capacity(num_ops);
        let mut diagnostic_events: Vec<DiagnosticEvent> = Vec::new();
        let mut soroban_return_value = None;
        let mut all_success = true;
        let mut failure = None;
        // Track pre-TX delta position so we only scan NEW created entries for contract cache.
        let pre_tx_created_count = self.state.delta().created_entries().len();
        // For multi-operation transactions, stellar-core records STATE/UPDATED
        // for every accessed entry per operation, even if values are identical.
        // For single-operation transactions, it only records if values changed.
        self.state.set_multi_op_mode(num_ops > 1);

        let tx_seq = frame.sequence_number();
        // Collect hot archive restored keys across all operations (Protocol 23+)
        // Use HashSet to deduplicate: when multiple TXs in the same ledger restore
        // the same entry (e.g., same ContractCode), it should only be sent to
        // HotArchiveBucketList::add_batch once as a single Live marker.
        let mut collected_hot_archive_keys: HashSet<LedgerKey> = HashSet::new();
        let mut op_type_timings: HashMap<OperationType, (u64, u32)> = HashMap::new();

        // Apply the signature check result from above (checked before signer removal).
        if let Some((op_results, sig_failure)) = sig_check_failure {
            all_success = false;
            operation_results = op_results;
            failure = Some(sig_failure);
        }

        if let Some(preflight_failure) = preflight_failure {
            all_success = false;
            failure = Some(preflight_failure);
        } else if all_success {
            // Clone memo once before the loop — it's constant for the entire TX.
            let tx_memo = frame.memo().clone();

            for (op_index, op) in frame.operations().iter().enumerate() {
                let op_type = OperationType::from_body(&op.body);

                let op_source_muxed = op
                    .source_account
                    .clone()
                    .unwrap_or_else(|| frame.inner_source_account());
                let op_delta_before = delta_snapshot(&self.state);
                self.state.begin_op_snapshot();
                let op_timing_start = std::time::Instant::now();

                // Load any accounts needed for this operation
                self.load_operation_accounts(snapshot, op, &inner_source_id)?;

                // Get operation source
                let op_source = henyey_tx::muxed_to_account_id(&op_source_muxed);

                let pre_claimable_balance = match &op.body {
                    OperationBody::ClaimClaimableBalance(op_data) => self
                        .state
                        .get_claimable_balance(&op_data.balance_id)
                        .cloned(),
                    OperationBody::ClawbackClaimableBalance(op_data) => self
                        .state
                        .get_claimable_balance(&op_data.balance_id)
                        .cloned(),
                    _ => None,
                };
                let pre_pool = match &op.body {
                    OperationBody::LiquidityPoolDeposit(op_data) => self
                        .state
                        .get_liquidity_pool(&op_data.liquidity_pool_id)
                        .cloned(),
                    OperationBody::LiquidityPoolWithdraw(op_data) => self
                        .state
                        .get_liquidity_pool(&op_data.liquidity_pool_id)
                        .cloned(),
                    _ => None,
                };
                let mut op_event_manager = OpEventManager::new(
                    true,
                    op_type.is_soroban(),
                    self.protocol_version,
                    self.network_id,
                    tx_memo.clone(),
                    self.classic_events,
                );

                // Execute the operation with a per-operation savepoint.
                // If the operation fails, we roll back its state changes so
                // subsequent operations see clean state (matching stellar-core LedgerTxn).
                // For single-op TXs, skip savepoint creation — TX-level rollback handles failures.
                let op_index = u32::try_from(op_index).unwrap_or(u32::MAX);

                let op_savepoint = if num_ops == 1 {
                    None
                } else {
                    Some(self.state.create_savepoint())
                };
                let result = self.execute_single_operation(OperationExecutionRequest {
                    op,
                    source: &op_source,
                    tx_source: &inner_source_id,
                    tx_seq,
                    op_index,
                    context: &ledger_context,
                    soroban_data,
                });

                match result {
                    Ok(mut op_exec) => {
                        self.state.flush_modified_entries();
                        let mut op_result = op_exec.result;

                        // Debug: Log operation result for Soroban operations
                        if op_type.is_soroban() {
                            let is_success_before_refund_check = is_operation_success(&op_result);
                            tracing::debug!(
                                ledger_seq = self.ledger_seq,
                                op_index,
                                op_type = ?op_type,
                                op_result = ?op_result,
                                is_success = is_success_before_refund_check,
                                has_soroban_meta = op_exec.soroban_meta.is_some(),
                                "Soroban operation executed"
                            );
                        }

                        if let Some(meta) = &op_exec.soroban_meta {
                            if let Some(tracker) = refundable_fee_tracker.as_mut() {
                                tracing::debug!(
                                    ledger_seq = self.ledger_seq,
                                    op_index,
                                    rent_fee = meta.rent_fee,
                                    event_size_bytes = meta.event_size_bytes,
                                    max_refundable = tracker.max_refundable_fee,
                                    consumed_rent = tracker.consumed_rent_fee,
                                    consumed_refundable = tracker.consumed_refundable_fee,
                                    "Refundable fee tracker pre-consume"
                                );
                                if !tracker.consume(
                                    &frame,
                                    self.protocol_version,
                                    &self.soroban_config,
                                    meta.event_size_bytes,
                                    meta.rent_fee,
                                ) {
                                    tracing::debug!(
                                        ledger_seq = self.ledger_seq,
                                        op_index,
                                        "InsufficientRefundableFee"
                                    );
                                    op_result = insufficient_refundable_fee_result(op);
                                    all_success = false;
                                    failure = Some(TransactionResultCode::TxFailed);
                                }
                            }
                        }
                        // Check if operation succeeded
                        if !is_operation_success(&op_result) {
                            all_success = false;
                            tracing::debug!(
                                ledger_seq = self.ledger_seq,
                                op_index,
                                op_type = ?op_type,
                                op_result = ?op_result,
                                "Operation failed"
                            );
                            if matches!(op_result, OperationResult::OpNotSupported) {
                                failure = Some(TransactionResultCode::TxNotSupported);
                            }
                            // Roll back failed operation's state changes so subsequent
                            // operations see clean state (matches stellar-core nested LedgerTxn).
                            if let Some(sp) = op_savepoint {
                                self.state.rollback_to_savepoint(sp);
                            }
                        }
                        operation_results.push(op_result.clone());

                        let op_delta_after = delta_snapshot(&self.state);
                        let op_snapshots = self.state.end_op_snapshot();
                        let delta_slice = delta_slice_between(
                            self.state.delta(),
                            op_delta_before,
                            op_delta_after,
                        );

                        // For Soroban operations, extract restored entries (hot archive and live BL)
                        let (restored_entries, footprint) = if op_type.is_soroban() {
                            let restored = collect_soroban_restored_entries(
                                &op_exec.soroban_meta,
                                soroban_data,
                                op_type,
                                &op_result,
                                &delta_slice,
                                &mut collected_hot_archive_keys,
                            );
                            (restored, soroban_data.map(|d| &d.resources.footprint))
                        } else {
                            (RestoredEntries::default(), None)
                        };

                        let change_order = delta_slice.change_order();
                        let ledger_changes = LedgerChanges {
                            created: delta_slice.created(),
                            updated: delta_slice.updated(),
                            update_states: delta_slice.update_states(),
                            deleted: delta_slice.deleted(),
                            delete_states: delta_slice.delete_states(),
                            change_order: &change_order,
                            state_overrides: &op_snapshots,
                            restored: &restored_entries,
                        };
                        let op_changes_local = build_entry_changes_with_hot_archive(
                            &self.state,
                            &ledger_changes,
                            footprint.is_some(),
                            self.ledger_seq,
                        );

                        let mut op_events_final = Vec::new();
                        // Always extract diagnostic events from soroban_meta, regardless
                        // of success. Parity: stellar-core captures diagnostics before
                        // checking success (InvokeHostFunctionOpFrame.cpp:561,
                        // TransactionMeta.cpp:1119-1126).
                        if let Some(meta) = op_exec.soroban_meta.as_mut() {
                            diagnostic_events.extend(std::mem::take(&mut meta.diagnostic_events));
                        }
                        if all_success && is_operation_success(&op_result) {
                            if let Some(meta) = op_exec.soroban_meta.as_mut() {
                                op_event_manager.set_events(std::mem::take(&mut meta.events));
                                soroban_return_value =
                                    std::mem::take(&mut meta.return_value).or(soroban_return_value);
                            }

                            if !op_type.is_soroban() {
                                emit_classic_events_for_operation(
                                    &mut op_event_manager,
                                    op,
                                    &op_result,
                                    &op_source_muxed,
                                    &self.state,
                                    pre_claimable_balance.as_ref(),
                                    pre_pool.as_ref(),
                                );
                            }

                            if op_event_manager.is_enabled() {
                                op_events_final = op_event_manager.finalize();
                            }
                        }

                        // Run invariant checks on the operation delta.
                        if let Some(ref invariant_mgr) = self.invariant_manager {
                            let inv_delta = henyey_invariant::OperationDelta {
                                created: delta_slice.created(),
                                updated: delta_slice.updated(),
                                update_states: delta_slice.update_states(),
                                deleted: delta_slice.deleted(),
                                delete_states: delta_slice.delete_states(),
                                ledger_seq: self.ledger_seq,
                                ledger_version: self.protocol_version,
                                header_current: None,
                                header_previous: None,
                                network_id: &self.network_id.0 .0,
                            };
                            invariant_mgr.check_on_operation_apply(
                                op,
                                &op_result,
                                &inv_delta,
                                &op_events_final,
                            );
                        }

                        if all_success {
                            op_changes.push(op_changes_local);
                            op_events.push(op_events_final);
                        } else {
                            op_changes.push(empty_entry_changes());
                            op_events.push(Vec::new());
                        }
                    }
                    Err(e) => {
                        if let Some(sp) = op_savepoint {
                            self.state.rollback_to_savepoint(sp);
                        }
                        self.state.end_op_snapshot();
                        all_success = false;
                        tracing::debug!(
                            error = %e,
                            op_index = op_index,
                            op_type = ?OperationType::from_body(&op.body),
                            ledger_seq = self.ledger_seq,
                            "Operation execution returned Err (mapped to txInternalError)"
                        );
                        // stellar-core maps std::runtime_error during operation execution
                        // to txINTERNAL_ERROR (not txNOT_SUPPORTED). The exception
                        // aborts all remaining operations.
                        failure = Some(TransactionResultCode::TxInternalError);
                        metrics::counter!("stellar_ledger_transaction_internal_error_total")
                            .increment(1);
                        break;
                    }
                }
                let op_elapsed_us = op_timing_start.elapsed().as_micros() as u64;
                metrics::histogram!("stellar_ledger_operation_apply_seconds")
                    .record(op_elapsed_us as f64 / 1_000_000.0);
                let entry = op_type_timings.entry(op_type).or_insert((0u64, 0u32));
                entry.0 += op_elapsed_us;
                entry.1 += 1;
            }
        }

        let ops_us = tx_timing_start.elapsed().as_micros() as u64
            - validation_us
            - fee_seq_us
            - footprint_us;

        let meta_phase_start = std::time::Instant::now();

        if all_success && self.state.has_pending_sponsorship() {
            all_success = false;
            failure = Some(TransactionResultCode::TxBadSponsorship);
        }

        if !all_success {
            let pre_apply = PreApplySnapshot {
                fee_entries,
                preapply_entries,
                fee_mode,
                fee,
            };
            self.rollback_failed_tx(
                &frame,
                &fee_source_id,
                &inner_source_id,
                &operation_results,
                &pre_apply,
                &mut refundable_fee_tracker,
            );
            op_changes.clear();
            op_events.clear();
            // Diagnostic events are NOT cleared on failure — stellar-core preserves
            // them unconditionally in V4 meta (TransactionMeta.cpp:1119-1126).
            soroban_return_value = None;
        } else {
            self.commit_successful_tx(pre_tx_created_count);
        }

        let commit_phase_us = meta_phase_start.elapsed().as_micros() as u64;

        let post_fee_changes = empty_entry_changes();
        let mut fee_refund = 0i64;
        let mut soroban_fee_info = None;
        if let Some(tracker) = refundable_fee_tracker {
            // Extract fee tracking info for soroban meta before consuming tracker
            soroban_fee_info = Some((
                tracker.non_refundable_fee,
                tracker.consumed_refundable_fee,
                tracker.consumed_rent_fee,
            ));
            let refund = tracker.refund_amount();
            let stage = stellar_xdr::curr::TransactionEventStage::AfterAllTxs;
            tx_event_manager.new_fee_event(&fee_source_id, -refund, stage);
            fee_refund = refund;
        }

        let fee_refund_phase_us = meta_phase_start.elapsed().as_micros() as u64 - commit_phase_us;

        let tx_events = tx_event_manager.finalize();
        let tx_meta = build_transaction_meta(TransactionMetaParts {
            tx_changes_before,
            op_changes: op_changes,
            op_events: op_events,
            tx_events: tx_events,
            soroban_return_value: soroban_return_value,
            diagnostic_events: diagnostic_events,
            soroban_fee_info: soroban_fee_info,
            emit_soroban_tx_meta_ext_v1: self.emit_soroban_tx_meta_ext_v1,
            enable_soroban_diagnostic_events: self.enable_soroban_diagnostic_events,
            tx_succeeded: all_success,
            is_soroban: frame.is_soroban(),
        });

        let meta_build_phase_us =
            meta_phase_start.elapsed().as_micros() as u64 - commit_phase_us - fee_refund_phase_us;

        let total_us = tx_timing_start.elapsed().as_micros() as u64;
        let meta_us = total_us - validation_us - fee_seq_us - footprint_us - ops_us;

        // Stage B: per-transaction apply duration (ops + meta, excludes validation/fees/footprint).
        let apply_duration_us = ops_us + meta_us;
        metrics::histogram!("stellar_ledger_transaction_apply_seconds")
            .record(apply_duration_us as f64 / 1_000_000.0);

        if total_us > SLOW_TX_LOG_THRESHOLD_US || frame.is_soroban() {
            // Build a compact string of per-op-type timings sorted by time desc
            let mut op_timing_vec: Vec<_> = op_type_timings.iter().collect();
            op_timing_vec.sort_by_key(|a| std::cmp::Reverse(a.1 .0));
            let op_timing_str: String = op_timing_vec
                .iter()
                .map(|(op, (us, count))| format!("{:?}:{}us×{}", op, us, count))
                .collect::<Vec<_>>()
                .join(",");
            tracing::debug!(
                ledger_seq = self.ledger_seq,
                total_us,
                validation_us,
                val_account_load_us,
                val_tx_hash_us,
                val_ed25519_us,
                val_other_us,
                fee_seq_us,
                fee_deduct_us,
                op_sig_check_us,
                signer_removal_us,
                seq_bump_us,
                footprint_us,
                ops_us,
                meta_us,
                is_soroban = frame.is_soroban(),
                num_ops = frame.operations().len(),
                success = all_success,
                op_timings = %op_timing_str,
                "TX phase timing"
            );
        }

        Ok(TransactionExecutionResult {
            success: all_success,
            fee_charged: fee.saturating_sub(fee_refund),
            fee_refund,
            operation_results,
            error: if all_success {
                None
            } else {
                Some("One or more operations failed".into())
            },
            failure: if all_success {
                None
            } else {
                Some(failure.unwrap_or(TransactionResultCode::TxFailed))
            },
            tx_meta: Some(tx_meta),
            fee_changes: Some(fee_changes),
            post_fee_changes: Some(post_fee_changes),
            // Convert HashSet to a deterministically-ordered Vec.
            // Uses LedgerKey's derived Ord which matches stellar-core's xdrpp
            // operator< (discriminant first, then fields in XDR declaration
            // order). Do NOT sort by XDR-encoded bytes — that prepends length
            // prefixes to variable-length fields, diverging from native order.
            hot_archive_restored_keys: {
                let mut keys: Vec<_> = collected_hot_archive_keys.into_iter().collect();
                keys.sort();
                keys
            },
            timings: TxExecTimings {
                op_type_timings,
                exec_time_us: total_us,
                validation_us,
                fee_seq_us,
                footprint_us,
                ops_us,
                meta_build_us: meta_us,
                meta_commit_us: commit_phase_us,
                meta_fee_refund_us: fee_refund_phase_us,
                meta_build_phase_us,
                val_account_load_us,
                val_tx_hash_us,
                val_ed25519_us,
                val_other_us,
                fee_deduct_us,
                op_sig_check_us,
                signer_removal_us,
                seq_bump_us,
            },
            tx_hash,
            fee_bump_outer_failure: false,
        })
    }

    /// Set up lazy entry loaders on the state manager for on-demand loading
    /// during operation execution.
    ///
    /// This configures three loaders:
    /// - **Entry loader**: loads a single entry from the snapshot (used during offer crossing)
    /// - **Batch entry loader**: loads multiple entries in one pass (used by path payments)
    /// - **Pool-share TLs loader**: discovers pool share trustlines by account
    pub(super) fn setup_entry_loaders(&mut self, snapshot: &SnapshotHandle) {
        let snapshot_for_loader = snapshot.clone();
        self.state.set_entry_loader(std::sync::Arc::new(move |key| {
            snapshot_for_loader
                .get_entry(key)
                .map_err(|e| henyey_tx::TxError::Internal(e.to_string()))
        }));

        let snapshot_for_batch = snapshot.clone();
        self.state
            .set_batch_entry_loader(std::sync::Arc::new(move |keys| {
                snapshot_for_batch
                    .load_entries(keys)
                    .map_err(|e| henyey_tx::TxError::Internal(e.to_string()))
            }));

        let snapshot_for_pool_shares = snapshot.clone();
        self.state
            .set_pool_share_tls_by_account_loader(std::sync::Arc::new(move |account_id| {
                snapshot_for_pool_shares
                    .pool_share_tls_by_account(account_id)
                    .map_err(|e| henyey_tx::TxError::Internal(e.to_string()))
            }));
    }

    /// Roll back state changes for a failed transaction and restore pre-apply entries.
    ///
    /// When a transaction fails, this method:
    /// 1. Rolls back all operation-level state changes
    /// 2. Restores fee, sequence, and signer entries from the pre-apply phase
    /// 3. Re-adds the fee to the delta (so failed TXs still contribute fees)
    /// 4. Resets the refundable fee tracker (full refund on failure)
    pub(super) fn rollback_failed_tx(
        &mut self,
        frame: &TransactionFrame,
        fee_source_id: &AccountId,
        inner_source_id: &AccountId,
        operation_results: &[OperationResult],
        pre_apply: &PreApplySnapshot,
        refundable_fee_tracker: &mut Option<RefundableFeeTracker>,
    ) {
        let tx_hash = frame
            .hash(&self.network_id)
            .map(|hash| hash.to_hex())
            .unwrap_or_else(|_| "unknown".to_string());
        debug!(
            tx_hash = %tx_hash,
            fee_source = %account_id_to_strkey(fee_source_id),
            inner_source = %account_id_to_strkey(inner_source_id),
            results = ?operation_results,
            "Transaction failed; rolling back changes"
        );
        self.state.rollback();
        restore_delta_entries(
            &mut self.state,
            &pre_apply.fee_entries.created,
            &pre_apply.fee_entries.updated,
            &pre_apply.fee_entries.deleted,
        );
        // Re-add the fee to the delta after rollback.
        // rollback() restores the delta from the snapshot taken BEFORE fee deduction,
        // so we must explicitly re-add this transaction's fee to preserve it.
        // This ensures failed transactions still contribute their fees to the fee pool.
        if pre_apply.fee_mode == FeeMode::Deduct && pre_apply.fee > 0 {
            self.state.delta_mut().add_fee(pre_apply.fee);
        }
        restore_delta_entries(
            &mut self.state,
            &pre_apply.preapply_entries.created,
            &pre_apply.preapply_entries.updated,
            &pre_apply.preapply_entries.deleted,
        );

        // Reset the refundable fee tracker when transaction fails.
        // This mirrors stellar-core's behavior where setError() calls resetConsumedFee(),
        // ensuring the full max_refundable_fee is refunded on any transaction failure.
        if let Some(tracker) = refundable_fee_tracker.as_mut() {
            tracing::debug!(
                ledger_seq = self.ledger_seq,
                is_soroban = frame.is_soroban(),
                max_refundable_fee = tracker.max_refundable_fee,
                consumed_before_reset = tracker.consumed_refundable_fee,
                "Resetting fee tracker due to tx failure"
            );
            tracker.reset();
        }
    }

    /// Commit a successful transaction and update the module cache.
    ///
    /// After all operations succeed, commits the state changes and scans
    /// newly created entries for contract code to add to the module cache
    /// (enabling `VmCachedInstantiation` for subsequent transactions).
    pub(super) fn commit_successful_tx(&mut self, pre_tx_created_count: usize) {
        self.state.commit();

        // Update module cache with any newly created contract code.
        // Only scan entries created by THIS TX (not all prior TXs in the cluster)
        // to avoid O(n²) iteration over the growing delta.
        let created = self.state.delta().created_entries();
        for entry in &created[pre_tx_created_count..] {
            if let stellar_xdr::curr::LedgerEntryData::ContractCode(cc) = &entry.data {
                self.add_contract_to_cache(cc.code.as_slice());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{
        ContractDataDurability, ContractDataEntry, ContractId, ExtensionPoint, Hash,
        LedgerEntryData, LedgerEntryExt, LedgerKeyContractData, LedgerKeyTtl, ScAddress, ScVal,
        TtlEntry,
    };

    fn make_contract_data_key() -> LedgerKey {
        LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([1u8; 32]))),
            key: ScVal::Void,
            durability: ContractDataDurability::Persistent,
        })
    }

    fn make_ttl_key() -> LedgerKey {
        LedgerKey::Ttl(LedgerKeyTtl {
            key_hash: Hash([2u8; 32]),
        })
    }

    fn make_entry(seq: u32) -> stellar_xdr::curr::LedgerEntry {
        stellar_xdr::curr::LedgerEntry {
            last_modified_ledger_seq: seq,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: ScAddress::Contract(ContractId(Hash([1u8; 32]))),
                key: ScVal::Void,
                durability: ContractDataDurability::Persistent,
                val: ScVal::Void,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    fn make_ttl_entry(seq: u32) -> stellar_xdr::curr::LedgerEntry {
        stellar_xdr::curr::LedgerEntry {
            last_modified_ledger_seq: seq,
            data: LedgerEntryData::Ttl(TtlEntry {
                key_hash: Hash([2u8; 32]),
                live_until_ledger_seq: 1000,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    #[test]
    fn test_source_returns_correct_variant() {
        let mut r = RestoredEntries::new();
        let data_key = make_contract_data_key();
        let ttl_key = make_ttl_key();
        let entry = make_entry(1);

        // Before insertion, source returns None
        assert!(r.source(&data_key).is_none());

        // Insert hot archive data/code
        r.insert_hot_archive_entry(data_key.clone(), entry.clone());
        assert!(matches!(
            r.source(&data_key),
            Some(RestoreSource::HotArchive(_))
        ));

        // Insert hot archive TTL
        r.insert_hot_archive_ttl(ttl_key.clone());
        assert!(matches!(
            r.source(&ttl_key),
            Some(RestoreSource::HotArchiveTtl)
        ));

        // Insert live BL
        let live_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([9u8; 32]))),
            key: ScVal::Void,
            durability: ContractDataDurability::Persistent,
        });
        r.insert_live_bl(live_key.clone(), entry.clone());
        assert!(matches!(
            r.source(&live_key),
            Some(RestoreSource::LiveBucketList(_))
        ));
    }

    #[test]
    #[should_panic(expected = "key already restored from live BL")]
    fn test_mutual_exclusion_live_bl_then_hot_archive_panics() {
        let mut r = RestoredEntries::new();
        let key = make_contract_data_key();
        let entry = make_entry(1);
        r.insert_live_bl(key.clone(), entry.clone());
        r.insert_hot_archive_entry(key, entry); // should panic
    }

    #[test]
    #[should_panic(expected = "key already restored from hot archive")]
    fn test_mutual_exclusion_hot_archive_then_live_bl_panics() {
        let mut r = RestoredEntries::new();
        let key = make_contract_data_key();
        let entry = make_entry(1);
        r.insert_hot_archive_entry(key.clone(), entry.clone());
        r.insert_live_bl(key, entry); // should panic
    }

    #[test]
    #[should_panic(expected = "insert_hot_archive_ttl called with non-TTL key")]
    fn test_hot_archive_ttl_rejects_non_ttl() {
        let mut r = RestoredEntries::new();
        let key = make_contract_data_key();
        r.insert_hot_archive_ttl(key); // should panic
    }

    #[test]
    #[should_panic(expected = "insert_hot_archive_entry called with TTL key")]
    fn test_hot_archive_entry_rejects_ttl() {
        let mut r = RestoredEntries::new();
        let key = make_ttl_key();
        let entry = make_ttl_entry(1);
        r.insert_hot_archive_entry(key, entry); // should panic
    }

    #[test]
    fn test_iterators_filter_correctly() {
        let mut r = RestoredEntries::new();
        let ha_key = make_contract_data_key();
        let ttl_key = make_ttl_key();
        let live_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([5u8; 32]))),
            key: ScVal::Void,
            durability: ContractDataDurability::Persistent,
        });
        let entry = make_entry(1);

        r.insert_hot_archive_entry(ha_key.clone(), entry.clone());
        r.insert_hot_archive_ttl(ttl_key.clone());
        r.insert_live_bl(live_key.clone(), entry.clone());

        // hot_archive_entries_with_originals returns only HotArchive variant
        let ha_entries: Vec<_> = r.hot_archive_entries_with_originals().collect();
        assert_eq!(ha_entries.len(), 1);
        assert_eq!(ha_entries[0].0, &ha_key);

        // live_bl_entries returns only LiveBucketList variant
        let lb_entries: Vec<_> = r.live_bl_entries().collect();
        assert_eq!(lb_entries.len(), 1);
        assert_eq!(lb_entries[0].0, &live_key);
    }

    #[test]
    fn test_is_restored_covers_all_sources() {
        let mut r = RestoredEntries::new();
        let ha_key = make_contract_data_key();
        let ttl_key = make_ttl_key();
        let live_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([7u8; 32]))),
            key: ScVal::Void,
            durability: ContractDataDurability::Persistent,
        });
        let entry = make_entry(1);

        r.insert_hot_archive_entry(ha_key.clone(), entry.clone());
        r.insert_hot_archive_ttl(ttl_key.clone());
        r.insert_live_bl(live_key.clone(), entry);

        assert!(r.is_restored(&ha_key));
        assert!(r.is_restored(&ttl_key));
        assert!(r.is_restored(&live_key));

        // is_hot_archive_restored covers both hot archive variants
        assert!(r.is_hot_archive_restored(&ha_key));
        assert!(r.is_hot_archive_restored(&ttl_key));
        assert!(!r.is_hot_archive_restored(&live_key));

        // is_live_bl_restored covers only live BL
        assert!(!r.is_live_bl_restored(&ha_key));
        assert!(!r.is_live_bl_restored(&ttl_key));
        assert!(r.is_live_bl_restored(&live_key));
    }

    #[test]
    fn test_live_bl_len_counts_correctly() {
        let mut r = RestoredEntries::new();
        assert_eq!(r.live_bl_len(), 0);

        let entry = make_entry(1);
        r.insert_hot_archive_entry(make_contract_data_key(), entry.clone());
        assert_eq!(r.live_bl_len(), 0);

        let live_key1 = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([3u8; 32]))),
            key: ScVal::Void,
            durability: ContractDataDurability::Persistent,
        });
        let live_key2 = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([4u8; 32]))),
            key: ScVal::Void,
            durability: ContractDataDurability::Persistent,
        });
        r.insert_live_bl(live_key1, entry.clone());
        r.insert_live_bl(live_key2, entry);
        assert_eq!(r.live_bl_len(), 2);
    }

    #[test]
    #[should_panic(expected = "duplicate hot archive entry insertion")]
    fn test_same_source_duplicate_panics() {
        let mut r = RestoredEntries::new();
        let key = make_contract_data_key();
        let entry1 = make_entry(1);
        let entry2 = make_entry(2);

        r.insert_hot_archive_entry(key.clone(), entry1);
        // Re-inserting same key panics (matches stellar-core behavior)
        r.insert_hot_archive_entry(key, entry2);
    }

    #[test]
    #[should_panic(expected = "TTL key already restored from live BL")]
    fn test_hot_archive_ttl_rejects_existing_live_bl() {
        let mut r = RestoredEntries::new();
        let ttl_key = make_ttl_key();
        let entry = make_ttl_entry(1);

        r.insert_live_bl(ttl_key.clone(), entry);
        // Inserting TTL as hot archive when already in live BL should panic
        r.insert_hot_archive_ttl(ttl_key);
    }

    #[test]
    fn test_hot_archive_ttl_idempotent() {
        let mut r = RestoredEntries::new();
        let ttl_key = make_ttl_key();

        // First insertion
        r.insert_hot_archive_ttl(ttl_key.clone());
        // Idempotent re-insertion is allowed (same variant, same key)
        r.insert_hot_archive_ttl(ttl_key.clone());

        assert!(matches!(
            r.source(&ttl_key),
            Some(RestoreSource::HotArchiveTtl)
        ));
    }
}
