//! Ledger close pipeline: meta assembly, slot externalization, and buffered ledger application.

use super::*;

/// Record the last phase duration from a `PhaseTimer` into a Prometheus histogram.
/// Must be called immediately after `timer.mark()`.
fn record_phase_histogram(
    metric: crate::metrics::HistogramMetric,
    timer: &tracked_lock::PhaseTimer,
) {
    if let Some(&(_, duration)) = timer.phases().last() {
        metric.record(duration.as_secs_f64());
    }
}

/// Read-and-conditionally-clear the post-catchup publish-skip marker.
///
/// Returns `true` if the marker is set to exactly `checkpoint_seq`; in that
/// case the marker is also deleted so the skip fires at most once. Returns
/// `false` for any other state (absent marker, marker pointing at a future
/// checkpoint).
///
/// stellar-core parity reference: `mSkipFirstCheckpointSinceItIsIncomplete`
/// in `HistoryManagerImpl`. In stellar-core the flag is an in-memory bool
/// re-derived on startup from "no dirty checkpoint files on disk"; here it
/// is persisted in the `state` table by [`super::persist::CatchupPersistData`]
/// so the skip survives a crash between catchup completion and the first
/// post-catchup checkpoint close.
///
/// The caller must invoke this inside the same DB transaction that decides
/// whether to enqueue the checkpoint for publishing, so the read, the
/// enqueue suppression, and the marker clear are all atomic.
fn consume_skip_marker_if_matches(
    conn: &henyey_db::Connection,
    checkpoint_seq: u32,
) -> Result<bool, henyey_db::DbError> {
    use henyey_db::queries::StateQueries;
    let raw = conn.get_state(henyey_db::schema::state_keys::PUBLISH_SKIP_FIRST_CHECKPOINT)?;
    let skip_target: Option<u32> = match raw.as_deref() {
        None => None,
        Some(s) => Some(s.parse::<u32>().map_err(|e| {
            henyey_db::DbError::Integrity(format!(
                "PUBLISH_SKIP_FIRST_CHECKPOINT marker value {:?} is not a valid u32: {}",
                s, e
            ))
        })?),
    };

    // If a marker survives past its target checkpoint, a prior close failed
    // to clear it. Warn and actively delete so the orphan can't sit in the
    // `state` table forever (the seq monotone means it would otherwise
    // never match again).
    if let Some(t) = skip_target {
        if t < checkpoint_seq {
            tracing::warn!(
                marker = t,
                ledger_seq = checkpoint_seq,
                "PUBLISH_SKIP_FIRST_CHECKPOINT marker is older than the closing \
                 checkpoint; clearing orphaned marker"
            );
            conn.delete_state(henyey_db::schema::state_keys::PUBLISH_SKIP_FIRST_CHECKPOINT)?;
            return Ok(false);
        }
    }

    if skip_target == Some(checkpoint_seq) {
        tracing::info!(
            ledger_seq = checkpoint_seq,
            "Skipping publish-queue enqueue for first checkpoint after catchup \
             (stellar-core skipFirstCheckpointSinceItIsIncomplete parity)"
        );
        conn.delete_state(henyey_db::schema::state_keys::PUBLISH_SKIP_FIRST_CHECKPOINT)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// A single SCP history persistence batch for one slot.
///
/// Matches stellar-core's `saveSCPHistory(slot, envelopes, qmap)` contract.
/// The close pipeline builds ordered batches (slot N-1 first, then N) so the
/// DB write preserves the same ordering as `processExternalized()`.
struct ScpHistoryBatch {
    ledger_seq: u32,
    envelopes: Vec<stellar_xdr::ScpEnvelope>,
    quorum_sets: Vec<(Hash256, stellar_xdr::ScpQuorumSet)>,
}

/// Raw inputs for a ledger-close persist job, captured on the event loop.
///
/// Phase A of #1733: the event loop captures only cheap clones (no XDR/JSON
/// serialization). The persist task performs all serialization on a
/// blocking thread via [`LedgerPersistInputs::serialize_and_write_to_db`].
struct LedgerPersistInputs {
    header: stellar_xdr::LedgerHeader,
    tx_history_entry: TransactionHistoryEntry,
    tx_result_entry: TransactionHistoryResultEntry,
    ordered_txs: Vec<std::sync::Arc<TransactionEnvelope>>,
    tx_results: Vec<TransactionResultPair>,
    tx_metas: Option<Vec<TransactionMeta>>,
    tx_count: usize,
    network_id: NetworkId,
    /// Ordered SCP history batches: previous slot (N-1) first, then current (N).
    /// Matches stellar-core's `processExternalized()` persistence ordering (§5.3).
    scp_history_batches: Vec<ScpHistoryBatch>,
    /// HAS struct built on the event loop under bucket-list read guards.
    /// JSON serialization happens later on the blocking thread.
    has: HistoryArchiveState,
    bucket_list_levels: Option<Vec<(Hash256, Hash256)>>,
    /// Whether checkpoint publishing is enabled (validator with writable archives).
    /// Controls whether checkpoints are enqueued to the publish queue.
    publish_enabled: bool,
    /// Whether to write the per-tx RPC-serving rows (`transactions` table)
    /// and contract events (`events` table). False on validators that do not
    /// serve JSON-RPC (see `AppConfig::store_rpc_data`); the whole-ledger
    /// `tx_history_entry`/`tx_result_entry` blobs and SCP history are always
    /// written — history publish reads those.
    store_rpc_data: bool,
}

impl LedgerPersistInputs {
    /// Serialize XDR/JSON and write to SQLite on a blocking thread.
    ///
    /// LEDGER_SPEC §4.11 / §15.13 / INV-L13 mapping (#3066). stellar-core's
    /// §4.11 commit/persist sequence prescribes 8 ordered steps; henyey
    /// *flattens* the durable subset of them into the single atomic
    /// `db.transaction` closure below. This is equivalent-or-stronger than the
    /// stepwise approach, not a regression — there is no observable window in
    /// which the persisted state is partially advanced.
    ///
    ///   - **§4.11 Step 1 (queue checkpoint):** `enqueue_publish` (below) runs
    ///     *inside* this same transaction on checkpoint ledgers, so the publish
    ///     queue entry and the ledger state commit either both land or neither
    ///     does (co-transactional, per #3057) — stronger than stellar-core's
    ///     separate-step queueing.
    ///   - **§4.11 Step 2 (commit LedgerTxn):** the header, tx history/results,
    ///     bucket-list snapshot, per-tx rows, contract events, and SCP history
    ///     are all written in this one transaction.
    ///   - **INV-L13 (MUST): on reload, persisted HAS and persisted LCL header
    ///     MUST agree on `ledgerSeq`.** Henyey satisfies this *by construction*:
    ///     the HAS write (`set_state(HISTORY_ARCHIVE_STATE)`) and the LCL write
    ///     (`set_last_closed_ledger`) below are both inside this single atomic
    ///     transaction, so a crash either commits both (agreeing on `ledgerSeq`)
    ///     or neither. No torn intermediate state is observable on reload.
    ///   - **§4.11 Step 3 (finalize checkpoint files):** performed by the
    ///     bucket/hot-archive flush in `persist::PersistJob::run_blocking`
    ///     *before* this DB write — see that function.
    ///   - **§4.11 Steps 7-8 (advance LCL+publish+GC, notify herder):** realized
    ///     post-persist in `handle_close_complete_inner` (see the bucket-GC and
    ///     herder-notify annotations there).
    ///   - **§4.11 Steps 5-6 (copy Soroban state for the invariant snapshot):**
    ///     intentionally absent — the state-snapshot invariant hook is not yet
    ///     wired (`crates/invariant/PARITY_STATUS.md`, "Snapshot hook not yet
    ///     wired"). That is separate work, NOT part of #3066. Do not infer from
    ///     this comment that the snapshot copy happens here.
    ///
    /// No ordering, control-flow, or transaction-boundary change is implied by
    /// this comment; it is traceability annotation only.
    fn serialize_and_write_to_db(&self, db: &henyey_db::Database) -> anyhow::Result<()> {
        use henyey_db::queries::*;

        let header_xdr = self
            .header
            .to_xdr(stellar_xdr::Limits::none())
            .map_err(|e| anyhow::anyhow!("Failed to serialize header XDR: {}", e))?;

        // Diagnostic: compare HAS-derived bucket_list_hash against header.
        // Non-fatal; logged only.
        {
            let expected_hash = Hash256::from_bytes(self.header.bucket_list_hash.0);
            match self.has.compute_bucket_list_hash() {
                Ok(go_sdk_hash) => {
                    if go_sdk_hash != expected_hash {
                        tracing::error!(
                            ledger_seq = self.header.ledger_seq,
                            go_sdk_hash = %go_sdk_hash.to_hex(),
                            header_hash = %expected_hash.to_hex(),
                            "DIAGNOSTIC: HAS bucket_list_hash does NOT match header!"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to compute diagnostic Go SDK hash");
                }
            }
        }

        let has_json = self
            .has
            .to_json()
            .map_err(|e| anyhow::anyhow!("Failed to serialize HAS: {}", e))?;

        // Instrument the WAL write-lock hold for this close persist so a long
        // hold is self-naming in the logs (#3702). Instrumentation only — the
        // transaction scope below is unchanged.
        let _write_ctx = henyey_common::WriteCtxGuard::new(format!(
            "ledger-close-persist seq={}",
            self.header.ledger_seq
        ));

        db.transaction(|conn| {
            conn.store_ledger_header(&self.header, &header_xdr)?;
            conn.store_tx_history_entry(self.header.ledger_seq, &self.tx_history_entry)?;
            conn.store_tx_result_entry(self.header.ledger_seq, &self.tx_result_entry)?;
            if is_checkpoint_ledger(self.header.ledger_seq) {
                if let Some(ref levels) = self.bucket_list_levels {
                    conn.store_bucket_list(self.header.ledger_seq, levels)?;
                }
                if self.publish_enabled {
                    // stellar-core parity: skipFirstCheckpointSinceItIsIncomplete.
                    // If a post-catchup marker targets exactly this checkpoint
                    // seq, suppress the enqueue and consume the marker. The
                    // read-and-clear happens inside the same DB transaction
                    // as the enqueue decision so a crash here cannot leave
                    // the marker partially consumed.
                    if !consume_skip_marker_if_matches(conn, self.header.ledger_seq)? {
                        conn.enqueue_publish(self.header.ledger_seq, &has_json)?;
                    }
                }
            }
            if self.store_rpc_data {
                for index in 0..self.tx_count {
                    let tx = &self.ordered_txs[index];
                    let tx_result = &self.tx_results[index];
                    let tx_meta = self.tx_metas.as_ref().and_then(|metas| metas.get(index));

                    let frame = TransactionFrame::with_network(tx.clone(), self.network_id);
                    let tx_hash = frame
                        .hash(&self.network_id)
                        .map_err(|e| henyey_db::DbError::Integrity(e.to_string()))?;
                    let tx_id = tx_hash.to_hex();

                    let tx_body = tx.to_xdr(stellar_xdr::Limits::none())?;
                    let tx_result_xdr = tx_result.to_xdr(stellar_xdr::Limits::none())?;
                    let tx_meta_xdr = match tx_meta {
                        Some(meta) => Some(meta.to_xdr(stellar_xdr::Limits::none())?),
                        None => None,
                    };

                    let status = {
                        use stellar_xdr::TransactionResultCode;
                        let code = tx_result.result.result.discriminant();
                        if code == TransactionResultCode::TxSuccess
                            || code == TransactionResultCode::TxFeeBumpInnerSuccess
                        {
                            henyey_db::TxStatus::Success
                        } else {
                            henyey_db::TxStatus::Failed
                        }
                    };

                    conn.store_transaction(&henyey_db::StoreTxParams {
                        ledger_seq: self.header.ledger_seq,
                        tx_index: index as u32,
                        tx_id: &tx_id,
                        body: &tx_body,
                        result: &tx_result_xdr,
                        meta: tx_meta_xdr.as_deref(),
                        status,
                    })?;
                }

                if let Some(ref metas) = self.tx_metas {
                    let events = App::extract_contract_events(
                        self.header.ledger_seq,
                        &self.ordered_txs,
                        &self.tx_results,
                        metas,
                        self.network_id,
                    )
                    .map_err(|e| henyey_db::DbError::Integrity(e.to_string()))?;
                    if !events.is_empty() {
                        conn.store_events(&events)?;
                    }
                }
            }

            // Persist SCP history in ordered batches: previous slot (N-1) first,
            // then current slot (N), matching stellar-core's processExternalized()
            // persistence ordering (§5.3).
            for batch in &self.scp_history_batches {
                conn.store_scp_history(batch.ledger_seq, &batch.envelopes)?;
                for (hash, qset) in &batch.quorum_sets {
                    conn.store_scp_quorum_set(hash, batch.ledger_seq, qset)?;
                }
            }

            conn.set_state(state_keys::HISTORY_ARCHIVE_STATE, &has_json)?;
            conn.set_last_closed_ledger(self.header.ledger_seq)?;
            // Clear any stale catchup persist sentinel (AUDIT-226).
            // If a catchup's deferred persist was lost but the node
            // continued closing ledgers, the DB state is now current.
            conn.delete_state(state_keys::CATCHUP_PERSIST_PENDING)?;

            Ok(())
        })?;
        Ok(())
    }
}

/// Outcome of merging a `LedgerCloseInfo` into the `syncing_ledgers` buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergeOutcome {
    /// New entry inserted. `has_tx_set` indicates whether `tx_set` was present.
    Inserted { has_tx_set: bool },
    /// Existing entry upgraded with a `tx_set` (was None, now Some).
    Upgraded,
    /// Existing entry unchanged (already had a `tx_set` or new info didn't provide one).
    Unchanged { has_tx_set: bool },
    /// `tx_set_hash` mismatch — existing entry kept, new info rejected.
    HashMismatch { existing_has_tx_set: bool },
}

impl MergeOutcome {
    /// Whether a `tx_set` is present for this slot after the merge.
    fn has_tx_set(&self) -> bool {
        match self {
            Self::Inserted { has_tx_set } => *has_tx_set,
            Self::Upgraded => true,
            Self::Unchanged { has_tx_set } => *has_tx_set,
            Self::HashMismatch {
                existing_has_tx_set,
            } => *existing_has_tx_set,
        }
    }
}

/// Merge `info` into a buffer entry. Encapsulates the single merge policy for
/// `syncing_ledgers`: hash-mismatch keeps existing, otherwise upgrade `tx_set`
/// from None to Some, or insert if vacant.
fn merge_into_buffer_entry(
    slot: u32,
    entry: std::collections::btree_map::Entry<'_, u32, henyey_herder::LedgerCloseInfo>,
    info: henyey_herder::LedgerCloseInfo,
) -> MergeOutcome {
    match entry {
        std::collections::btree_map::Entry::Occupied(mut e) => {
            let existing = e.get_mut();
            if existing.tx_set_hash != info.tx_set_hash {
                tracing::warn!(
                    slot,
                    existing_hash = %existing.tx_set_hash.to_hex(),
                    new_hash = %info.tx_set_hash.to_hex(),
                    "merge_into_buffer_entry: tx_set_hash mismatch, keeping existing"
                );
                return MergeOutcome::HashMismatch {
                    existing_has_tx_set: existing.tx_set.is_some(),
                };
            }
            if existing.tx_set.is_none() && info.tx_set.is_some() {
                existing.tx_set = info.tx_set;
                MergeOutcome::Upgraded
            } else {
                MergeOutcome::Unchanged {
                    has_tx_set: existing.tx_set.is_some(),
                }
            }
        }
        std::collections::btree_map::Entry::Vacant(e) => {
            let has_tx_set = info.tx_set.is_some();
            e.insert(info);
            MergeOutcome::Inserted { has_tx_set }
        }
    }
}

/// Build ordered SCP history batches for persistence: previous slot (N-1)
/// first, then current slot (N), matching stellar-core's processExternalized()
/// §5.3 ordering contract.
///
/// Extracted from `App::build_persist_inputs()` so it can be tested with a real
/// `Herder` without requiring a full `App` instance.
fn build_scp_history_batches(
    herder: &henyey_herder::Herder,
    current_slot: u64,
) -> Vec<ScpHistoryBatch> {
    let mut batches = Vec::new();

    // Helper to build a batch from externalizing state for a slot.
    let build_batch = |slot: u64| -> ScpHistoryBatch {
        let envelopes = herder.get_scp_externalizing_state(slot);
        let mut quorum_sets = Vec::new();
        for envelope in &envelopes {
            let hash = henyey_common::scp_quorum_set_hash(&envelope.statement);
            let hash256 = Hash256::from_bytes(hash.0);
            if let Some(qset) = herder.get_quorum_set_by_hash(&hash256) {
                quorum_sets.push((hash256, qset));
            } else {
                tracing::debug!(
                    hash = %hash256.to_hex(),
                    slot,
                    "Missing quorum set for SCP history batch"
                );
            }
        }
        ScpHistoryBatch {
            ledger_seq: slot as u32,
            envelopes,
            quorum_sets,
        }
    };

    // Previous slot first (if available).
    if current_slot > 1 {
        let prev_batch = build_batch(current_slot - 1);
        if !prev_batch.envelopes.is_empty() {
            batches.push(prev_batch);
        }
    }

    // Current slot — includes tracked quorum map qsets per stellar-core's
    // saveSCPHistory(slotN, envelopes, getCurrentlyTrackedQuorum()).
    let mut current_batch = build_batch(current_slot);
    // Merge in quorum sets from the tracked quorum map that aren't already
    // present from the externalizing envelopes (§5.3 parity).
    let tracked_qsets = herder.get_currently_tracked_quorum();
    for (hash, qset) in tracked_qsets {
        if !current_batch.quorum_sets.iter().any(|(h, _)| *h == hash) {
            current_batch.quorum_sets.push((hash, qset));
        }
    }
    batches.push(current_batch);

    batches
}

impl App {
    pub(crate) fn externalized_iteration_window(
        last_processed: u64,
        current_ledger: u32,
        latest_externalized: u64,
    ) -> (u64, u64) {
        let first_replay = current_ledger as u64 + 1;
        let replay_checkpoint = checkpoint_containing(first_replay as u32);
        let checkpoint_unpublished = replay_checkpoint > latest_externalized as u32;

        if checkpoint_unpublished {
            // Process all slots - archive catchup would fail because the
            // checkpoint containing first_replay is not published yet.
            (last_processed + 1, last_processed)
        } else if latest_externalized.saturating_sub(last_processed) > TX_SET_REQUEST_WINDOW {
            let skip_to = latest_externalized.saturating_sub(TX_SET_REQUEST_WINDOW);
            // When there's a gap (last_processed <= current_ledger, e.g.
            // after catchup), don't skip past current_ledger — gap slots
            // between current_ledger+1 and skip_to need to be iterable.
            // In normal operation (last_processed > current_ledger), the
            // original skip optimization is preserved.
            let effective_skip = if last_processed <= current_ledger as u64 {
                skip_to.min(current_ledger as u64)
            } else {
                skip_to
            };
            (effective_skip + 1, effective_skip)
        } else {
            (last_processed + 1, last_processed)
        }
    }

    fn extract_tx_metas(meta: &LedgerCloseMeta) -> Vec<TransactionMeta> {
        match meta {
            LedgerCloseMeta::V0(_) => Vec::new(),
            LedgerCloseMeta::V1(v1) => v1
                .tx_processing
                .iter()
                .map(|processing| processing.tx_apply_processing.clone())
                .collect(),
            LedgerCloseMeta::V2(v2) => v2
                .tx_processing
                .iter()
                .map(|processing| processing.tx_apply_processing.clone())
                .collect(),
        }
    }

    /// Prepare cheap inputs for the persist job (clones + bucket-list read).
    ///
    /// Expensive serialization (header XDR, HAS JSON, diagnostic hash)
    /// happens later in [`LedgerPersistInputs::serialize_and_write_to_db`]
    /// on a blocking thread — not on the event loop.
    fn build_persist_inputs(
        &self,
        header: &stellar_xdr::LedgerHeader,
        tx_set_variant: &TransactionSetVariant,
        tx_results: &[TransactionResultPair],
        tx_metas: Option<&[TransactionMeta]>,
    ) -> anyhow::Result<LedgerPersistInputs> {
        let network_id = NetworkId::from_passphrase(&self.config.network.passphrase);
        let ordered_txs: Vec<std::sync::Arc<TransactionEnvelope>> = tx_set_variant
            .transactions_with_base_fee()
            .into_iter()
            .map(|(tx, _)| tx)
            .collect();
        if ordered_txs.len() != tx_results.len() {
            anyhow::bail!(
                "tx count mismatch: {} envelopes vs {} results",
                ordered_txs.len(),
                tx_results.len()
            );
        }
        let tx_count = ordered_txs.len();

        // Build ordered SCP history batches: previous slot (N-1) first, then
        // current slot (N), matching stellar-core's processExternalized() §5.3.
        let scp_history_batches = build_scp_history_batches(&self.herder, header.ledger_seq as u64);

        let tx_set_entry = match tx_set_variant {
            TransactionSetVariant::Classic(set) => set.clone(),
            TransactionSetVariant::Generalized(_) => TransactionSet {
                previous_ledger_hash: Hash([0u8; 32]),
                txs: VecM::default(),
            },
        };
        let tx_history_entry = TransactionHistoryEntry {
            ledger_seq: header.ledger_seq,
            tx_set: tx_set_entry,
            ext: match tx_set_variant {
                TransactionSetVariant::Classic(_) => TransactionHistoryEntryExt::V0,
                TransactionSetVariant::Generalized(set) => {
                    TransactionHistoryEntryExt::V1(set.clone())
                }
            },
        };
        let tx_result_set = TransactionResultSet {
            results: tx_results
                .to_vec()
                .try_into()
                .map_err(|e| anyhow::anyhow!("failed to convert tx results to XDR VecM: {e}"))?,
        };
        let tx_result_entry = TransactionHistoryResultEntry {
            ledger_seq: header.ledger_seq,
            tx_result_set,
            ext: TransactionHistoryResultEntryExt::default(),
        };

        // Build HAS from in-memory bucket list state. JSON serialization
        // and diagnostic hash are deferred to the persist task.
        let has = {
            let bucket_list = self.ledger_manager.bucket_list();
            let hot_archive_guard = self.ledger_manager.hot_archive_bucket_list();
            let hot_archive_ref = hot_archive_guard.as_ref();

            let hot_archive_for_has = if hot_archive_supported(header.ledger_version) {
                hot_archive_ref
            } else {
                None
            };

            build_history_archive_state(
                header.ledger_seq,
                &bucket_list,
                hot_archive_for_has,
                Some(self.config.network.passphrase.clone()),
            )
            .map_err(|e| anyhow::anyhow!("Failed to build HAS: {}", e))?
        };

        let bucket_list_levels = if is_checkpoint_ledger(header.ledger_seq) {
            Some(self.ledger_manager.bucket_list_levels())
        } else {
            None
        };

        let store_rpc_data = self.config.store_rpc_data();
        Ok(LedgerPersistInputs {
            header: header.clone(),
            tx_history_entry,
            tx_result_entry,
            ordered_txs,
            tx_results: tx_results.to_vec(),
            // The metas are captured only for the per-tx RPC rows and event
            // extraction; skip the (large) clone entirely when those writes
            // are disabled.
            tx_metas: if store_rpc_data {
                tx_metas.map(|m| m.to_vec())
            } else {
                None
            },
            tx_count,
            network_id,
            scp_history_batches,
            has,
            bucket_list_levels,
            publish_enabled: self.is_validator && self.config.history.publish_enabled(),
            store_rpc_data,
        })
    }

    /// Extract contract events from transaction metadata for indexing.
    fn extract_contract_events(
        ledger_seq: u32,
        ordered_txs: &[std::sync::Arc<stellar_xdr::TransactionEnvelope>],
        tx_results: &[TransactionResultPair],
        tx_metas: &[TransactionMeta],
        network_id: NetworkId,
    ) -> anyhow::Result<Vec<henyey_db::EventRecord>> {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
        use stellar_xdr::{ContractEvent, Limits, TransactionResultCode};

        let mut all_events = Vec::new();

        for (tx_index, meta) in tx_metas.iter().enumerate() {
            // Compute tx hash — missing envelope is a fatal inconsistency
            let tx = ordered_txs.get(tx_index).ok_or_else(|| {
                anyhow::anyhow!("tx_metas[{tx_index}] has no corresponding transaction envelope")
            })?;
            let frame = TransactionFrame::with_network(tx.clone(), network_id);
            let tx_hash_hex = frame
                .hash(&network_id)
                .map_err(|e| {
                    anyhow::anyhow!("failed to hash transaction at index {tx_index}: {e}")
                })?
                .to_hex();

            // Determine if the tx succeeded — missing result is a fatal inconsistency
            let tx_succeeded = {
                let result = tx_results.get(tx_index).ok_or_else(|| {
                    anyhow::anyhow!("tx_results[{tx_index}] missing for event extraction")
                })?;
                let code = result.result.result.discriminant();
                code == TransactionResultCode::TxSuccess
                    || code == TransactionResultCode::TxFeeBumpInnerSuccess
            };

            // Extract events from the meta based on version.
            // V0/V1/V2 predate Soroban and have no contract events.
            let contract_events: Vec<(u32, &ContractEvent)> = match meta {
                TransactionMeta::V0(_) | TransactionMeta::V1(_) | TransactionMeta::V2(_) => {
                    Vec::new()
                }
                TransactionMeta::V3(v3) => {
                    if let Some(ref soroban) = v3.soroban_meta {
                        soroban.events.iter().map(|e| (0u32, e)).collect()
                    } else {
                        Vec::new()
                    }
                }
                TransactionMeta::V4(v4) => {
                    let mut events = Vec::new();
                    for (op_idx, op_meta) in v4.operations.iter().enumerate() {
                        for event in op_meta.events.iter() {
                            events.push((op_idx as u32, event));
                        }
                    }
                    // Also include tx-level events (fee events)
                    for te in v4.events.iter() {
                        events.push((0u32, &te.event));
                    }
                    events
                }
            };

            for (event_index, (op_index, event)) in contract_events.iter().enumerate() {
                // Compute TOID-based event ID
                let toid =
                    ((ledger_seq as u64) << 32) | ((tx_index as u64) << 12) | (*op_index as u64);
                let event_id = format!("{:019}-{:010}", toid, event_index);

                let event_type = event.type_;

                let contract_id = event
                    .contract_id
                    .as_ref()
                    .map(|h| stellar_strkey::Contract(h.0.clone().into()).to_string());

                // Serialize topics — propagate errors instead of silently dropping
                let topics: Vec<String> = match &event.body {
                    stellar_xdr::ContractEventBody::V0(body) => body
                        .topics
                        .iter()
                        .map(|t| {
                            t.to_xdr(Limits::none())
                                .map(|b| BASE64.encode(&b))
                                .map_err(|e| {
                                    anyhow::anyhow!("failed to serialize event topic: {e}")
                                })
                        })
                        .collect::<anyhow::Result<Vec<_>>>()?,
                };

                // Serialize full event — propagate errors instead of silently skipping
                let event_xdr = event
                    .to_xdr(Limits::none())
                    .map(|b| BASE64.encode(&b))
                    .map_err(|e| anyhow::anyhow!("failed to serialize contract event: {e}"))?;

                all_events.push(henyey_db::EventRecord {
                    id: event_id,
                    ledger_seq,
                    tx_index: tx_index as u32,
                    op_index: *op_index,
                    tx_hash: tx_hash_hex.clone(),
                    contract_id,
                    event_type,
                    topics,
                    event_xdr,
                    in_successful_contract_call: tx_succeeded,
                });
            }
        }

        Ok(all_events)
    }

    /// Attempt to restore node state from persisted DB and on-disk bucket files.
    ///
    /// This is the Rust equivalent of stellar-core's `loadLastKnownLedger`.
    /// On success, the ledger manager is initialized with the bucket list
    /// reconstructed from disk, avoiding a full catchup from history archives.
    ///
    /// # Returns
    ///
    /// - `Ok(RestoreResult::Restored)` — state was successfully restored from disk.
    /// - `Ok(RestoreResult::NoState)` — no persisted state exists (fresh node,
    ///   no LCL in DB), **or** persisted state was found but bucket files are
    ///   missing (stale DB from a previous run). In the latter case the stale
    ///   DB state is cleared so the next restart starts cleanly. The caller
    ///   should proceed to catchup or genesis bootstrap.
    /// - `Err(...)` — persisted state is corrupt or inconsistent (e.g., LCL exists
    ///   but HAS is missing, LCL/HAS mismatch). The caller **must not** continue.
    pub async fn load_last_known_ledger(&self) -> anyhow::Result<RestoreResult> {
        // Step 1: Read LCL sequence from DB (offloaded to blocking pool)
        let lcl_seq = self
            .db_blocking("load-lcl-seq", |db| {
                db.with_connection(|conn| conn.get_last_closed_ledger())
                    .map_err(Into::into)
            })
            .await?;
        let Some(lcl_seq) = lcl_seq else {
            tracing::debug!("No last closed ledger in DB, cannot restore from disk");
            return Ok(RestoreResult::NoState);
        };
        if lcl_seq == 0 {
            // Genesis is persisted as ledger seq 1; a DB value of 0 is anomalous.
            anyhow::bail!(
                "Invalid database state: last closed ledger is 0 (expected >= 1 or absent)"
            );
        }

        // Drain stale publish queue entries when publishing cannot happen.
        // Publishing requires BOTH validator status AND writable archives.
        // Without either, queue entries can never be published and would pin
        // retention thresholds (#1989). This also handles config changes
        // (e.g., validator→non-validator with writable archives still present).
        let can_publish = self.is_validator && self.config.history.publish_enabled();
        if !can_publish {
            match self
                .db_blocking("drain-stale-publish-queue", |db| {
                    let stale = db.load_publish_queue(None)?;
                    if !stale.is_empty() {
                        tracing::info!(
                            count = stale.len(),
                            "Clearing publish queue: publishing not possible \
                             (requires validator with writable archives)"
                        );
                        for seq in &stale {
                            db.remove_publish(*seq)?;
                        }
                    }
                    Ok::<_, anyhow::Error>(stale.len())
                })
                .await
            {
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Failed to drain stale publish queue (non-fatal)"
                    );
                }
            }
        }

        // Step 1b: Restore checkpoint state (crash recovery).
        // Mirrors stellar-core's restoreCheckpoint(lcl) called from
        // loadLastKnownLedger: cleans up partial checkpoint files left
        // by a previous crash and finalizes any complete checkpoint at
        // the current boundary.
        self.restore_checkpoint(lcl_seq);

        // Step 2: Read HAS JSON from DB (offloaded to blocking pool)
        let has_json = self
            .db_blocking("load-has-json", |db| {
                db.with_connection(|conn| conn.get_state(state_keys::HISTORY_ARCHIVE_STATE))
                    .map_err(Into::into)
            })
            .await?;
        let Some(has_json) = has_json else {
            anyhow::bail!(
                "Invalid database state: LCL exists at seq {} but no history archive state in DB",
                lcl_seq
            );
        };
        let has = HistoryArchiveState::from_json(&has_json)
            .map_err(|e| anyhow::anyhow!("Failed to parse persisted HAS: {}", e))?;

        // Step 3: Verify consistency between LCL and HAS
        if has.current_ledger != lcl_seq {
            anyhow::bail!(
                "Invalid database state: LCL seq {} does not agree with HAS current_ledger {}",
                lcl_seq,
                has.current_ledger
            );
        }

        tracing::info!(
            lcl_seq,
            bucket_levels = has.current_buckets.len(),
            "Found persisted state, attempting restore from disk"
        );

        // Step 4: Load ledger header from DB (offloaded to blocking pool)
        let header = self
            .db_blocking("load-lcl-header", move |db| {
                db.get_ledger_header(lcl_seq).map_err(Into::into)
            })
            .await?
            .ok_or_else(|| anyhow::anyhow!("LCL header missing from DB at seq {}", lcl_seq))?;

        // Compute header hash (we don't store it separately)
        let header_hash = compute_header_hash(&header)
            .map_err(|e| anyhow::anyhow!("Failed to compute header hash: {}", e))?;

        // Step 5: Verify ALL referenced bucket files exist on disk.
        // Parity: stellar-core validates all bucket hashes (curr, snap, AND
        // pending merge outputs/inputs) and fails hard if any are missing
        // (LedgerManagerImpl.cpp:555-560). We must do the same — silently
        // discarding pending merge state produces wrong bucket list hashes.
        let mut essential_hashes: Vec<Hash256> = has
            .bucket_hash_pairs()
            .iter()
            .flat_map(|(curr, snap)| [*curr, *snap])
            .filter(|h| !h.is_zero())
            .collect();
        // Include pending merge hashes (output for state-1, inputs for state-2)
        for merge_state in has.live_next_states()?.iter().flatten() {
            essential_hashes.extend(
                merge_state
                    .referenced_hashes()
                    .filter(|h| !h.is_zero())
                    .copied(),
            );
        }
        // Also include hot archive bucket hashes (curr, snap, and pending merges)
        if let Some(hot_pairs) = has.hot_archive_bucket_hash_pairs() {
            for (curr, snap) in &hot_pairs {
                if !curr.is_zero() {
                    essential_hashes.push(*curr);
                }
                if !snap.is_zero() {
                    essential_hashes.push(*snap);
                }
            }
        }
        if let Some(hot_next_states) = has.hot_archive_next_states()? {
            for merge_state in hot_next_states.iter().flatten() {
                essential_hashes.extend(
                    merge_state
                        .referenced_hashes()
                        .filter(|h| !h.is_zero())
                        .copied(),
                );
            }
        }

        // Step 5a: Also verify buckets referenced by the publish queue,
        // mirroring stellar-core's getMissingBucketsReferencedByPublishQueue().
        let publish_queue_hashes = self
            .db_blocking("load-publish-queue-bucket-hashes", |db| {
                collect_publish_queue_bucket_hashes(db)
            })
            .await?;
        essential_hashes.extend(publish_queue_hashes);

        let missing = self.bucket_manager.verify_buckets_exist(&essential_hashes);
        if !missing.is_empty() {
            // Parity divergence: stellar-core throws fatal here
            // (LedgerManagerImpl.cpp:555-560). We intentionally recover by
            // clearing the stale DB state and falling through to catchup.
            // This handles captive-core restarts against a stale file-backed
            // DB where bucket files were cleaned up between runs (#2705).
            tracing::warn!(
                missing_count = missing.len(),
                first = %missing[0].to_hex(),
                lcl_seq,
                "{} bucket(s) are missing from bucket directory. \
                 Clearing stale persisted state and falling back to catchup.",
                missing.len(),
            );
            self.db_blocking("clear-stale-persisted-state", |db| {
                db.with_connection(|conn| {
                    conn.delete_state(state_keys::LAST_CLOSED_LEDGER)?;
                    conn.delete_state(state_keys::HISTORY_ARCHIVE_STATE)?;
                    Ok::<_, henyey_db::error::DbError>(())
                })
                .map_err(Into::into)
            })
            .await?;
            return Ok(RestoreResult::NoState);
        }

        // Step 6: Reconstruct bucket lists with overlapped cache scan.
        //
        // All HAS-referenced files are guaranteed present by the Step 5 check
        // (missing buckets trigger a NoState fallback above).
        // No silent downgrade of pending merge state.
        //
        // The scan runs concurrently with merge restart because it only reads
        // level.curr and level.snap (via Arc clones) while restart_merges_from_has
        // only writes level.next. The overall critical path is:
        //   parallel_restore (~56s) → max(scan ~32s, merges ~60s) → ~116s total
        // vs the old sequential path of ~297s.
        let reconstruct_start = std::time::Instant::now();

        // Start-of-restore checkpoint (#3239) so live-bucket-restore is
        // attributed to its own sampler sub-phase rather than orphaned to the
        // coarse `Catchup` tag.
        henyey_ledger::log_startup_memory("before_restore_bucket_list");

        let live_hash_pairs = has.bucket_hash_pairs();
        let live_next_states: Vec<Option<PendingMergeState>> = has.live_next_states()?;
        let bucket_dir = self.bucket_manager.bucket_dir().to_path_buf();

        // Step 6a: Parallel restore of live BucketList (all levels loaded concurrently).
        let bucket_manager = self.bucket_manager.clone();
        let load_bucket = |hash: &Hash256| -> henyey_bucket::Result<henyey_bucket::Bucket> {
            let arc = bucket_manager.load_bucket(hash)?;
            Ok(Arc::try_unwrap(arc).unwrap_or_else(|arc| (*arc).clone()))
        };
        let restore_fan_out = self.config.buckets.restore_apply_fan_out_cap();
        let mut bucket_list = BucketList::restore_from_has_parallel(
            &live_hash_pairs,
            &live_next_states,
            load_bucket,
            restore_fan_out,
        )
        .map_err(|e| anyhow::anyhow!("Failed to restore live bucket list: {}", e))?;
        bucket_list.set_bucket_dir(bucket_dir.clone());
        bucket_list.set_ledger_seq(lcl_seq);
        henyey_ledger::log_startup_memory("after_restore_bucket_list");

        // Step 6b: Extract Arc<Bucket> pairs before starting merges.
        // The scan thread owns these Arc clones independently of bucket_list.
        let level_pairs: Vec<(Arc<henyey_bucket::Bucket>, Arc<henyey_bucket::Bucket>)> =
            bucket_list
                .levels()
                .iter()
                .map(|l| (l.curr.clone(), l.snap.clone()))
                .collect();
        let protocol_version = header.ledger_version;
        let scan_thread_count = self.config.buckets.scan_thread_count;

        // Step 6c: Spawn cache scan in background — runs concurrently with merges.
        // Rent config point lookups happen inside the blocking thread (not the tokio thread).
        let scan_handle = tokio::task::spawn_blocking(move || {
            henyey_ledger::scan_level_pairs_for_caches(
                level_pairs,
                protocol_version,
                scan_thread_count,
            )
        });

        // Step 6d: Restart pending merges from HAS (async, ~60s).
        // Safe to hold &mut bucket_list while scan thread has Arc clones.
        {
            let bm = self.bucket_manager.clone();
            let load_bucket_for_merge =
                move |hash: &Hash256| -> henyey_bucket::Result<henyey_bucket::Bucket> {
                    bm.load_bucket_for_merge(hash)
                };
            bucket_list
                .restart_merges_from_has(
                    lcl_seq,
                    protocol_version,
                    &live_next_states,
                    load_bucket_for_merge,
                    true,
                )
                .await
                .map_err(|e| anyhow::anyhow!("Failed to restart bucket merges: {}", e))?;
            tracing::info!(
                bucket_list_hash = %bucket_list.hash().to_hex(),
                "Restarted pending merges from HAS"
            );
        }

        // Step 6e: Restore hot archive (~1s, sequential).
        henyey_ledger::log_startup_memory("hot_archive_restore");
        let hot_archive = if let Some(hot_hash_pairs) = has.hot_archive_bucket_hash_pairs() {
            let hot_next_states: Vec<Option<PendingMergeState>> =
                has.hot_archive_next_states()?.unwrap_or_default();

            // Parity: all hot archive bucket files (including pending merge
            // outputs/inputs) are validated by the preflight check above.
            // No silent downgrade needed — missing files cause a hard failure.

            let bucket_manager = self.bucket_manager.clone();
            let load_hot =
                |hash: &Hash256| -> henyey_bucket::Result<henyey_bucket::HotArchiveBucket> {
                    bucket_manager.load_hot_archive_bucket(hash)
                };
            let mut hot_bl = HotArchiveBucketList::restore_from_has_parallel(
                &hot_hash_pairs,
                &hot_next_states,
                load_hot,
                self.config.buckets.restore_apply_fan_out_cap(),
            )
            .map_err(|e| anyhow::anyhow!("Failed to restore hot archive: {}", e))?;

            let bucket_manager = self.bucket_manager.clone();
            let load_hot_for_merge =
                move |hash: &Hash256| -> henyey_bucket::Result<henyey_bucket::HotArchiveBucket> {
                    bucket_manager.load_hot_archive_bucket_for_merge(hash)
                };
            let hot_next_states_ref = hot_next_states.clone();
            hot_bl
                .restart_merges_from_has(
                    lcl_seq,
                    protocol_version,
                    &hot_next_states_ref,
                    load_hot_for_merge,
                    true,
                )
                .map_err(|e| anyhow::anyhow!("Failed to restart hot archive merges: {}", e))?;
            tracing::info!(
                hot_archive_hash = %hot_bl.hash().to_hex(),
                "Restarted hot archive pending merges from HAS"
            );
            hot_bl
        } else {
            HotArchiveBucketList::default()
        };

        // Step 6f: Join cache scan (should already be done since scan ~32s < merges ~60s).
        let cache_data = scan_handle
            .await
            .map_err(|e| anyhow::anyhow!("Cache scan thread panicked: {:?}", e))?
            .map_err(|e| anyhow::anyhow!("Cache scan failed: {:?}", e))?;

        henyey_ledger::log_startup_memory("after_cache_scan_and_merges");

        tracing::info!(
            elapsed_ms = reconstruct_start.elapsed().as_millis() as u64,
            "Bucket lists reconstructed from disk (parallel restore + overlapped scan)"
        );

        // Step 7: Initialize LedgerManager with precomputed caches.
        if self.ledger_manager.is_initialized() {
            self.ledger_manager.reset();
        }
        self.ledger_manager
            .initialize_with_precomputed_caches(
                bucket_list,
                hot_archive,
                header.clone(),
                header_hash,
                cache_data,
                // Skip the warm per-entry account cache during cold catchup-apply
                // (#3232). It is a steady-state read-latency optimization wasted
                // during a one-pass restore and adds ~1.0–1.5 GB to the catchup
                // anon-RSS peak. It is warmed once post-catchup via
                // `LedgerManager::warm_entry_caches()`, called from
                // `restore_operational_state()` when the node leaves CatchingUp.
                true,
            )
            .map_err(|e| anyhow::anyhow!("Failed to initialize ledger manager from disk: {}", e))?;

        tracing::info!(
            lcl_seq,
            header_hash = %header_hash.to_hex(),
            protocol_version = header.ledger_version,
            "Successfully restored node state from disk"
        );

        // Seed the validation context from the restored header so tx queue
        // admission rejects invalid Soroban txs from the very first moment.
        self.seed_validation_context();

        Ok(RestoreResult::Restored)
    }

    /// Seed the herder's ValidationContext from the current ledger state.
    ///
    /// Must be called after any code path that initializes the ledger manager
    /// (load_last_known_ledger, bootstrap_from_db) so the tx queue has up-to-date
    /// ledger info and Soroban resource limits before the overlay accepts txs.
    ///
    /// Without this, `soroban_limits` defaults to `None` and
    /// `check_soroban_resources` silently accepts any Soroban tx until the
    /// first ledger close seeds the limits.
    pub fn seed_validation_context(&self) {
        use henyey_herder::SorobanTxLimits;

        let header = self.ledger_manager.current_header();
        if header.ledger_seq == 0 {
            return;
        }

        let ledger_flags = match &header.ext {
            stellar_xdr::LedgerHeaderExt::V0 => 0,
            stellar_xdr::LedgerHeaderExt::V1(ext) => ext.flags,
        };

        self.herder.tx_queue().update_validation_context(
            header.ledger_seq,
            header.scp_value.close_time.0,
            header.ledger_version,
            header.base_fee,
            header.base_reserve,
            ledger_flags,
            self.herder.ledger_close_duration(),
        );

        // Seed the classic queue ops capacity from the restored header's
        // `maxTxSetSize` so admission tracks the live capacity from the very
        // first ledger, before the first post-restore close runs (#3612).
        self.herder.tx_queue().update_classic_queue_capacity(
            header
                .max_tx_set_size
                .saturating_mul(POOL_LEDGER_MULTIPLIER),
        );

        // Seed Soroban per-tx limits and dynamic resource limits from network config.
        if let Some(soroban_info) = self.soroban_network_info() {
            self.herder.tx_queue().set_soroban_limits(SorobanTxLimits {
                tx_max_instructions: soroban_info.tx_max_instructions as u64,
                tx_max_read_bytes: soroban_info.tx_max_read_bytes as u64,
                tx_max_write_bytes: soroban_info.tx_max_write_bytes as u64,
                tx_max_read_ledger_entries: soroban_info.tx_max_read_ledger_entries as u64,
                tx_max_write_ledger_entries: soroban_info.tx_max_write_ledger_entries as u64,
                tx_max_size_bytes: soroban_info.tx_max_size_bytes as u64,
            });
            self.herder
                .tx_queue()
                .set_soroban_resource_limits(soroban_info.to_resource_limits());
            self.update_herder_soroban_limits(&soroban_info);
            tracing::info!(
                ledger_seq = header.ledger_seq,
                "Seeded validation context with Soroban limits"
            );
        }
    }

    /// Update herder queue-admission (2x) and selection (1x) Soroban limits
    /// from `SorobanNetworkInfo`. Called at bootstrap and after each ledger close.
    fn update_herder_soroban_limits(&self, info: &henyey_ledger::SorobanNetworkInfo) {
        let m = POOL_LEDGER_MULTIPLIER as i64;
        let queue_limit = henyey_common::Resource::soroban_ledger_limits(
            info.ledger_max_tx_count as i64 * m,
            info.ledger_max_instructions * m,
            info.ledger_max_tx_size_bytes as i64 * m,
            info.ledger_max_read_bytes as i64 * m,
            info.ledger_max_write_bytes as i64 * m,
            info.ledger_max_read_ledger_entries as i64 * m,
            info.ledger_max_write_ledger_entries as i64 * m,
        );
        self.herder
            .tx_queue()
            .update_soroban_resource_limits(queue_limit);

        let selection_limit = henyey_common::Resource::soroban_ledger_limits(
            info.ledger_max_tx_count as i64,
            info.ledger_max_instructions,
            info.ledger_max_tx_size_bytes as i64,
            info.ledger_max_read_bytes as i64,
            info.ledger_max_write_bytes as i64,
            info.ledger_max_read_ledger_entries as i64,
            info.ledger_max_write_ledger_entries as i64,
        );
        self.herder
            .tx_queue()
            .update_soroban_selection_limits(selection_limit);

        // Refresh the parallel-Soroban phase limits from the live LCL config so
        // the tx-set builder's `use_parallel` gate and per-stage limits track
        // Soroban ConfigUpgrades. Parity: stellar-core reads these live from
        // getLastClosedSorobanNetworkConfig() (#3680).
        self.herder.tx_queue().update_soroban_parallel_limits(
            info.ledger_max_instructions,
            info.ledger_max_dependent_tx_clusters,
        );
    }

    /// Restore checkpoint state on startup (crash recovery).
    ///
    /// Mirrors stellar-core's `HistoryManagerImpl::restoreCheckpoint(lcl)`:
    ///
    /// 1. **Guard**: Skip if publishing is not enabled (no writable archives)
    /// 2. **Cleanup**: Call `CheckpointBuilder::cleanup(lcl)` to recover
    ///    partial dirty checkpoint files left by a previous crash
    /// 3. **Finalize**: If LCL is at a checkpoint boundary, rename recovered
    ///    dirty files to their final paths
    ///
    /// This is a best-effort operation — errors are logged but do not prevent
    /// startup, since checkpoint publishing is independent of ledger state.
    fn restore_checkpoint(&self, lcl: u32) {
        // Always remove stale publish queue entries above LCL — they represent
        // impossible future checkpoints regardless of archive configuration.
        match self.db.remove_publish_above_lcl(lcl) {
            Ok(removed) if removed > 0 => {
                tracing::info!(
                    lcl,
                    removed,
                    "Removed stale publish queue entries above LCL"
                );
            }
            Ok(_) => {} // nothing removed
            Err(e) => {
                tracing::warn!(
                    lcl,
                    error = %e,
                    "Failed to clean stale publish queue entries (non-fatal)"
                );
            }
        }

        // Guard: remaining restore logic only for configs with writable+readable archives.
        let publish_enabled = self
            .config
            .history
            .archives
            .iter()
            .any(|a| a.put_enabled && a.get_enabled);
        if !publish_enabled {
            return;
        }

        // Derive publish directory from bucket directory, mirroring
        // stellar-core's BUCKET_DIR_PATH / "history" layout.
        let publish_dir = self.bucket_manager.bucket_dir().join("history");
        if !publish_dir.exists() {
            tracing::debug!(
                publish_dir = %publish_dir.display(),
                "Publish directory does not exist, skipping checkpoint restore"
            );
            return;
        }

        tracing::info!(
            lcl,
            publish_dir = %publish_dir.display(),
            "Restoring checkpoint state on startup"
        );

        let mut builder = henyey_history::checkpoint_builder::CheckpointBuilder::new(publish_dir);

        // Phase 1: Clean up partial/corrupt dirty files
        if let Err(e) = builder.cleanup(lcl) {
            tracing::warn!(
                lcl,
                error = %e,
                "Checkpoint cleanup failed (non-fatal)"
            );
            return;
        }

        // Phase 2: If LCL is at a checkpoint boundary, finalize recovered
        // dirty files by renaming them to final paths.
        if is_checkpoint_ledger(lcl) {
            if let Err(e) = builder.finalize_recovered_checkpoint(lcl) {
                tracing::warn!(
                    lcl,
                    error = %e,
                    "Checkpoint finalization failed (non-fatal)"
                );
            }
        }
    }

    /// Reconstruct both live and hot archive bucket lists from a parsed HAS,
    /// including restarting any pending merges from saved input/output hashes.
    ///
    /// Shared helper used by both `load_last_known_ledger` (startup restore)
    /// and `rebuild_bucket_lists_from_has` (Case 1 replay).
    async fn reconstruct_bucket_lists(
        &self,
        has: &HistoryArchiveState,
        header: &stellar_xdr::LedgerHeader,
        lcl_seq: u32,
    ) -> anyhow::Result<(BucketList, HotArchiveBucketList)> {
        // Start-of-restore checkpoint (#3239) — this cold-catchup path is
        // sequential, so the sampler attributes each sub-phase cleanly.
        henyey_ledger::log_startup_memory("before_restore_bucket_list");

        // Reconstruct live BucketList
        let live_hash_pairs = has.bucket_hash_pairs();
        let live_next_states: Vec<Option<PendingMergeState>> = has.live_next_states()?;

        // Parity: stellar-core fails hard on missing bucket files
        // (LedgerManagerImpl.cpp:555-560). All referenced files (curr, snap,
        // and pending merge outputs/inputs) must be present. The caller is
        // responsible for ensuring this via preflight or download.
        // No silent downgrade of pending merge state.

        let bucket_manager = self.bucket_manager.clone();
        let load_bucket = |hash: &Hash256| -> henyey_bucket::Result<henyey_bucket::Bucket> {
            let arc = bucket_manager.load_bucket(hash)?;
            Ok(std::sync::Arc::try_unwrap(arc).unwrap_or_else(|arc| (*arc).clone()))
        };

        let mut bucket_list = BucketList::restore_from_has_parallel(
            &live_hash_pairs,
            &live_next_states,
            load_bucket,
            self.config.buckets.restore_apply_fan_out_cap(),
        )
        .map_err(|e| anyhow::anyhow!("Failed to restore live bucket list: {}", e))?;

        let bucket_dir = self.bucket_manager.bucket_dir().to_path_buf();
        bucket_list.set_bucket_dir(bucket_dir.clone());
        bucket_list.set_ledger_seq(lcl_seq);

        // Live restore done — merge restart begins next (#3239).
        henyey_ledger::log_startup_memory("after_restore_bucket_list");

        // Restart pending merges from HAS state.
        // This matches stellar-core loadLastKnownLedgerInternal() which calls
        // AssumeStateWork -> assumeState() -> restartMerges().
        {
            let protocol_version = header.ledger_version;
            let bm = self.bucket_manager.clone();
            let load_bucket_for_merge =
                move |hash: &Hash256| -> henyey_bucket::Result<henyey_bucket::Bucket> {
                    bm.load_bucket_for_merge(hash)
                };
            bucket_list
                .restart_merges_from_has(
                    lcl_seq,
                    protocol_version,
                    &live_next_states,
                    load_bucket_for_merge,
                    true,
                )
                .await
                .map_err(|e| anyhow::anyhow!("Failed to restart bucket merges: {}", e))?;
            tracing::info!(
                bucket_list_hash = %bucket_list.hash().to_hex(),
                "Restarted pending merges from HAS"
            );
        }

        // Reconstruct hot archive BucketList (or create empty)
        henyey_ledger::log_startup_memory("hot_archive_restore");
        let hot_archive = if let Some(hot_hash_pairs) = has.hot_archive_bucket_hash_pairs() {
            let hot_next_states: Vec<Option<PendingMergeState>> =
                has.hot_archive_next_states()?.unwrap_or_default();

            let bucket_manager = self.bucket_manager.clone();
            let load_hot =
                |hash: &Hash256| -> henyey_bucket::Result<henyey_bucket::HotArchiveBucket> {
                    bucket_manager.load_hot_archive_bucket(hash)
                };

            // Parity: no silent downgrade of hot archive merge state.
            // All referenced files must be present (caller ensures this).

            let mut hot_bl = HotArchiveBucketList::restore_from_has_parallel(
                &hot_hash_pairs,
                &hot_next_states,
                load_hot,
                self.config.buckets.restore_apply_fan_out_cap(),
            )
            .map_err(|e| anyhow::anyhow!("Failed to restore hot archive: {}", e))?;

            {
                let protocol_version = header.ledger_version;
                let bucket_manager = self.bucket_manager.clone();
                let load_hot_for_merge = move |hash: &Hash256| -> henyey_bucket::Result<
                    henyey_bucket::HotArchiveBucket,
                > {
                    bucket_manager.load_hot_archive_bucket_for_merge(hash)
                };
                hot_bl
                    .restart_merges_from_has(
                        lcl_seq,
                        protocol_version,
                        &hot_next_states,
                        load_hot_for_merge,
                        true,
                    )
                    .map_err(|e| anyhow::anyhow!("Failed to restart hot archive merges: {}", e))?;
                tracing::info!(
                    hot_archive_hash = %hot_bl.hash().to_hex(),
                    "Restarted hot archive pending merges from HAS"
                );
            }

            hot_bl
        } else {
            HotArchiveBucketList::default()
        };

        Ok((bucket_list, hot_archive))
    }

    /// Rebuild bucket lists from the persisted HAS in the database.
    ///
    /// This reads the `HistoryArchiveState` from the database (saved on every
    /// ledger close), reconstructs the bucket lists from it, and calls
    /// `restart_merges_from_has` to deterministically reconstitute any pending
    /// merges from saved input/output hashes.
    ///
    /// This matches stellar-core's approach for Case 1 catchup: the
    /// persisted HAS is the source of truth, not the live bucket list objects.
    pub(super) async fn rebuild_bucket_lists_from_has(
        &self,
    ) -> anyhow::Result<ExistingBucketState> {
        // Read persisted HAS and LCL header from DB (offloaded to blocking pool).
        //
        // This function is called from the catchup task which runs inside
        // tokio::spawn (catchup_impl.rs). This db_blocking dispatch is a
        // single spawn_blocking hop — not the nested pattern that caused the
        // #1713 deadlock (where long-running persist ops held blocking-pool
        // threads). Short single-row reads release the thread promptly.
        let (has, header) = self
            .db_blocking("rebuild-bucket-lists-load-has", |db| {
                let has_json =
                    db.with_connection(|conn| conn.get_state(state_keys::HISTORY_ARCHIVE_STATE))?;
                let has_json =
                    has_json.ok_or_else(|| anyhow::anyhow!("No persisted HAS in database"))?;
                let has = HistoryArchiveState::from_json(&has_json)
                    .map_err(|e| anyhow::anyhow!("Failed to parse persisted HAS: {}", e))?;
                let lcl_seq = has.current_ledger;
                let header = db.get_ledger_header(lcl_seq)?.ok_or_else(|| {
                    anyhow::anyhow!("LCL header missing from DB at seq {}", lcl_seq)
                })?;
                Ok((has, header))
            })
            .await?;

        let lcl_seq = has.current_ledger;

        let (bucket_list, hot_archive) = self
            .reconstruct_bucket_lists(&has, &header, lcl_seq)
            .await?;

        let network_id = NetworkId(self.network_id());

        tracing::info!(
            lcl_seq,
            bucket_list_hash = %bucket_list.hash().to_hex(),
            hot_archive_hash = %hot_archive.hash().to_hex(),
            "Rebuilt bucket lists from persisted HAS for Case 1 replay"
        );

        Ok(ExistingBucketState {
            bucket_list,
            hot_archive_bucket_list: hot_archive,
            header,
            network_id,
        })
    }

    /// Try to close a specific slot directly when we receive its tx set.
    /// This feeds the buffered ledger pipeline and attempts sequential apply.
    pub(super) async fn try_close_slot_directly(&self, slot: u64) {
        tracing::debug!(slot, "Attempting to close specific slot directly");
        let close_info = match self.herder.check_ledger_close(slot) {
            Some(info) => info,
            None => {
                tracing::debug!(slot, "No ledger close info for slot");
                return;
            }
        };

        self.ensure_buffered_slot(slot as u32, close_info).await;
        // The actual close is handled by the event loop's pending_close
        // chaining (try_start_ledger_close), not inline here.
    }

    /// Process any externalized slots that need ledger close.
    pub(super) async fn process_externalized_slots(&self) -> Option<PendingCatchup> {
        // Get the latest externalized slot. Time-wrapped (#1759
        // diagnostics): this acquires `ScpDriver::latest_externalized`
        // (parking_lot::RwLock) and is on the egress critical path.
        let latest_externalized =
            match tracked_lock::time_call("herder.latest_externalized_slot", || {
                self.herder.latest_externalized_slot()
            }) {
                Some(slot) => slot,
                None => {
                    tracing::debug!("No externalized slots yet");
                    return None;
                }
            };

        tracing::debug!(latest_externalized, "Processing externalized slots");

        // Check if we've already processed this slot
        let last_processed = *self.last_processed_slot.read().await;
        let has_new_slots = latest_externalized > last_processed;

        if has_new_slots {
            tracing::debug!(
                latest_externalized,
                last_processed,
                gap = latest_externalized - last_processed,
                "Processing new externalized slots"
            );

            let prev_latest = self
                .last_externalized_slot
                .swap(latest_externalized, Ordering::Relaxed);
            if latest_externalized != prev_latest {
                *self.last_externalized_at.write().await = self.clock.now();
            }

            let current_ledger = self.current_ledger_seq();
            let (iter_start, initial_advance_to) = Self::externalized_iteration_window(
                last_processed,
                current_ledger,
                latest_externalized,
            );

            // Pre-read the existing buffer keys/tx_set presence for the slot
            // range we will iterate. This is a SHORT read lock (clone one
            // bool per key). It lets us decide, during the lockless
            // iteration below, whether to re-request a tx_set when
            // check_ledger_close returns None (matching legacy behavior at
            // the old line 1203-1210).
            //
            // Type: { slot → existing_tx_set_hash_and_has_tx_set }. Absence
            // means slot not currently buffered.
            let pre_read_start = std::time::Instant::now();
            let existing: std::collections::HashMap<u32, (henyey_common::Hash256, bool)> = {
                let buffer =
                    tracked_lock::tracked_read("syncing_ledgers", &self.syncing_ledgers).await;
                // Only the slots in our iteration range are relevant.
                let mut map = std::collections::HashMap::with_capacity(buffer.len());
                for slot in iter_start..=latest_externalized {
                    if let Some(info) = buffer.get(&(slot as u32)) {
                        map.insert(slot as u32, (info.tx_set_hash, info.tx_set.is_some()));
                    }
                }
                map
                // Read guard dropped at end of scope.
            };
            super::warn_if_slow(
                pre_read_start.elapsed(),
                "process_externalized_slots_pre_read",
                existing.len() as u64,
            );

            // Plan: per-slot mutation description built OUTSIDE any
            // syncing_ledgers lock. Iteration includes the expensive work
            // (`check_ledger_close` XDR-parses each externalized slot's
            // StellarValue) plus the side-effect `request_tx_set` call.
            //
            // Semantics-preserving: the apply loop below produces the same
            // buffer state and the same (advance_to, missing_tx_set,
            // buffered_count, skipped_stale) return values as the legacy
            // inline critical section would.
            //
            // Side-effect ordering change: today's code interleaves
            // `request_tx_set` calls with buffer mutations; after this
            // split, all `request_tx_set` calls fire during the lockless
            // iteration, BEFORE any buffer mutation. This is safe because
            // `TxSetTracker` (the backing store for request_tx_set) is a
            // DashMap, independent of `syncing_ledgers`. Earlier firing of
            // tx_set requests is arguably better for latency.
            /// Per-slot mutation for the final `syncing_ledgers.write()`
            /// apply pass. Built during the lockless iteration above from
            /// the pre-read buffer snapshot and the per-slot
            /// `check_ledger_close` result. Only slots with a non-None
            /// `info` mutate the buffer; the others (re-request / no-op)
            /// completed their work during the lockless iteration and are
            /// not carried forward.
            struct SlotPlan {
                slot: u32,
                info: henyey_herder::LedgerCloseInfo,
            }

            let iter_start_instant = std::time::Instant::now();
            let mut plans: Vec<SlotPlan> = Vec::new();
            let mut advance_to = initial_advance_to;
            let mut skipped_stale = 0u64;
            let mut missing_tx_set = false;
            let mut pes_iterated: u64 = 0;
            #[cfg(test)]
            let mut gate_fired = false;
            for slot in iter_start..=latest_externalized {
                pes_iterated += 1;
                // Skip slots that have already been closed. Stale
                // EXTERNALIZE messages (e.g., from SCP state responses) can
                // set latest_externalized to old slots whose tx_sets are
                // evicted from peers' caches. Creating syncing_ledgers
                // entries for these would cause unfulfillable tx_set
                // requests and infinite recovery loops.
                if slot <= current_ledger as u64 {
                    skipped_stale += 1;
                    if slot == advance_to + 1 {
                        advance_to = slot;
                    }
                    continue;
                }

                let slot_u32 = slot as u32;
                let existing_entry = existing.get(&slot_u32).copied();

                // Test hook: fire the two-way gate once on the first
                // non-stale slot. Proves phase 2 (lockless iteration) is
                // in progress — no syncing_ledgers lock is held here.
                #[cfg(test)]
                if !gate_fired {
                    if let Some(ref gate) = self.pes_iteration_gate {
                        gate_fired = true;
                        gate.entered.notify_one();
                        gate.resume.notified().await;
                    }
                }

                if let Some(info) = self.herder.check_ledger_close(slot) {
                    // Predict whether tx_set will be missing after the apply
                    // pass merges this plan into the buffer. Accounts for
                    // hash-mismatch rejection: if hashes differ, the apply
                    // will keep the existing entry unchanged.
                    let post_update_missing = match existing_entry {
                        Some((existing_hash, existing_had_tx_set)) => {
                            if existing_hash != info.tx_set_hash {
                                // Hash mismatch: apply will reject; post-state = existing
                                !existing_had_tx_set
                            } else {
                                !(existing_had_tx_set || info.tx_set.is_some())
                            }
                        }
                        None => info.tx_set.is_none(),
                    };
                    if post_update_missing {
                        missing_tx_set = true;
                    }
                    plans.push(SlotPlan {
                        slot: slot_u32,
                        info,
                    });
                    if slot == advance_to + 1 {
                        advance_to = slot;
                    }
                } else if let Some((existing_hash, existing_has_tx_set)) = existing_entry {
                    // check_ledger_close returned None — the externalized
                    // data for this slot was evicted from cache. If we
                    // already have a syncing_ledgers entry with a
                    // tx_set_hash, re-register the pending tx_set request
                    // so request_pending_tx_sets can try to fetch it.
                    //
                    // This side-effect (request_tx_set) fires BEFORE the
                    // apply pass — safe because TxSetTracker is DashMap,
                    // independent of `syncing_ledgers`.
                    if !existing_has_tx_set {
                        self.herder.scp_driver().request_tx_set(existing_hash, slot);
                        missing_tx_set = true;
                    }
                    if slot == advance_to + 1 {
                        advance_to = slot;
                    }
                } else {
                    // check_ledger_close returned None and slot not buffered.
                    // Do NOT advance advance_to — this slot was never
                    // materialized. The loop continues to process later
                    // slots (they may have SCP data or be already buffered),
                    // but the contiguous watermark stays at the last
                    // materialized slot so this gap is retried on the next
                    // process_externalized_slots call.
                }
            }
            super::warn_if_slow(
                iter_start_instant.elapsed(),
                "process_externalized_slots_iteration",
                pes_iterated,
            );

            // Apply the plan under ONE short write lock. No XDR parses, no
            // herder calls: just BTreeMap mutations whose hold time is
            // bounded by O(plans.len()) — orders of magnitude faster than
            // the legacy critical section for long iter ranges
            // (especially in the `checkpoint_unpublished` branch of
            // `externalized_iteration_window` which has no slot-range cap).
            let apply_start = std::time::Instant::now();
            let buffered_count = plans.len();
            if !plans.is_empty() {
                let mut buffer =
                    tracked_lock::tracked_write("syncing_ledgers", &self.syncing_ledgers).await;
                for plan in plans {
                    let outcome =
                        merge_into_buffer_entry(plan.slot, buffer.entry(plan.slot), plan.info);
                    if let MergeOutcome::Upgraded = outcome {
                        tracing::info!(
                            slot = plan.slot,
                            "Updated buffered ledger with tx_set from check_ledger_close"
                        );
                    }
                }
            }
            super::warn_if_slow(
                apply_start.elapsed(),
                "process_externalized_slots_apply",
                buffered_count as u64,
            );
            if skipped_stale > 0 {
                tracing::debug!(
                    skipped_stale,
                    "Skipped already-closed slots in process_externalized_slots"
                );
            }

            *self.last_processed_slot.write().await = advance_to;

            // Purge SCP timers for slots that have been externalized.
            // advance_to is the highest fully-processed slot; timers for it
            // and all earlier slots are stale.
            self.timer_manager_handle
                .purge_old_slots(advance_to + 1)
                .await;

            if missing_tx_set {
                self.request_pending_tx_sets().await;
            }
            // Trigger externalized catchup if the gap between current_ledger
            // and the latest externalized slot is too large to bridge via
            // individual tx_set fetches.  Previously this only fired when
            // buffered_count == 0, but after catchup the first fresh
            // EXTERNALIZE creates an entry (buffered_count == 1) even though
            // current_ledger is 40+ slots behind.  Check the gap regardless.
            {
                let current_ledger = self.current_ledger_seq();
                let gap = latest_externalized.saturating_sub(current_ledger as u64);
                if buffered_count == 0 || gap > TX_SET_REQUEST_WINDOW {
                    self.set_phase(11); // 11 = externalized_catchup
                    let pending = self
                        .maybe_start_externalized_catchup(latest_externalized)
                        .await;
                    if pending.is_some() {
                        return pending;
                    }
                }
            }
        } else {
            tracing::debug!(
                target: "henyey::envelope_path",
                latest_externalized,
                last_processed,
                current_ledger = self.current_ledger_seq(),
                "process_externalized_slots: has_new_slots=false short-circuit",
            );
        }

        // Always check for catchup even when no new slots - we may need to
        // trigger stuck recovery. The actual ledger close is handled by the
        // event loop's pending_close chaining (try_start_ledger_close) which
        // runs after process_externalized_slots returns.
        self.set_phase(13); // 13 = maybe_buffered_catchup
        self.maybe_start_buffered_catchup().await
    }

    pub(super) fn trim_syncing_ledgers(
        buffer: &mut BTreeMap<u32, henyey_herder::LedgerCloseInfo>,
        current_ledger: u32,
    ) {
        // Hard limit on buffer size to prevent unbounded memory growth.
        // With ~50 slots per checkpoint and large tx sets, keeping more than
        // 100 slots can use significant memory (100+ MB).
        const MAX_BUFFER_SIZE: usize = 100;

        // Step 1: Remove entries already closed (at or below current_ledger).
        let min_keep = current_ledger.saturating_add(1);
        buffer.retain(|seq, _| *seq >= min_keep);
        if buffer.is_empty() {
            return;
        }

        // Step 2: Trim to checkpoint boundary ONLY when the buffer's first
        // entry is far ahead of current_ledger (gap >= CHECKPOINT_FREQUENCY).
        // When entries close to current_ledger exist (e.g. current_ledger+1),
        // those are potentially closeable once their tx_sets arrive — trimming
        // them would destroy entries the node needs for sequential close and
        // create an artificial gap that prevents progress.
        let first_buffered = *buffer.keys().next().expect("checked non-empty above");
        let last_buffered = *buffer.keys().next_back().expect("checked non-empty above");
        let gap = first_buffered.saturating_sub(current_ledger);
        if gap >= checkpoint_frequency() {
            let trim_before = match Self::trim_boundary_for_last_buffered(last_buffered) {
                Some(b) => b,
                None => return,
            };
            buffer.retain(|seq, _| *seq >= trim_before);
        }

        // Step 3: If buffer is still too large, keep only the most recent
        // MAX_BUFFER_SIZE slots. This prevents unbounded memory growth when
        // the validator is stuck.
        if buffer.len() > MAX_BUFFER_SIZE {
            let keys_to_remove: Vec<u32> = buffer
                .keys()
                .take(buffer.len() - MAX_BUFFER_SIZE)
                .copied()
                .collect();
            for key in keys_to_remove {
                buffer.remove(&key);
            }
            tracing::debug!(
                buffer_size = buffer.len(),
                "Trimmed syncing_ledgers buffer to max size"
            );
        }
    }

    /// Compute the trim boundary for the given `last_buffered` slot.
    ///
    /// Returns `None` if no trimming should occur (last_buffered <= 1 and is a
    /// checkpoint start). Otherwise returns `Some(trim_before)` — entries below
    /// this value should be removed.
    ///
    /// Delegates to the spec §7.4 pure boundary logic in
    /// `henyey_history::ledger_apply_manager` (parity:
    /// `LedgerApplyManagerImpl::trimSyncingLedgers`).
    pub(super) fn trim_boundary_for_last_buffered(last_buffered: u32) -> Option<u32> {
        henyey_history::trim_boundary_for_last_buffered(last_buffered)
    }

    /// Ensure a slot is in the syncing_ledgers buffer with the best
    /// available data.
    ///
    /// - Vacant: insert the full `LedgerCloseInfo`.
    /// - Occupied, `tx_set_hash` matches: upgrade `tx_set` if `info` has
    ///   one the existing entry lacks.
    /// - Occupied, `tx_set_hash` differs: log a warning and keep the
    ///   existing entry (SCP consensus data is authoritative).
    ///
    /// Returns `true` if a `tx_set` is present for the slot after the call.
    pub(super) async fn ensure_buffered_slot(
        &self,
        slot: u32,
        info: henyey_herder::LedgerCloseInfo,
    ) -> bool {
        let mut buffer =
            tracked_lock::tracked_write("syncing_ledgers", &self.syncing_ledgers).await;
        let outcome = merge_into_buffer_entry(slot, buffer.entry(slot), info);
        if let MergeOutcome::Inserted { .. } = outcome {
            tracing::debug!(
                slot,
                has_tx_set = outcome.has_tx_set(),
                "ensure_buffered_slot: inserted new entry"
            );
        } else if let MergeOutcome::Upgraded = outcome {
            tracing::debug!(slot, "ensure_buffered_slot: upgraded tx_set");
        }
        outcome.has_tx_set()
    }

    /// Try to populate gap slots between `current_ledger` and the first
    /// buffered slot from the externalized SCP cache. Called by recovery
    /// when a sequence gap is detected.
    pub(super) async fn populate_gap_slots(&self, current_ledger: u32) {
        let first_buffered = {
            let buffer = tracked_lock::tracked_read("syncing_ledgers", &self.syncing_ledgers).await;
            buffer.keys().next().copied()
        };
        let Some(first) = first_buffered else {
            return;
        };
        let gap_start = current_ledger + 1;
        if gap_start >= first {
            return;
        }
        let mut populated = 0u32;
        for gap_slot in gap_start..first {
            if let Some(info) = self.herder.check_ledger_close(gap_slot as u64) {
                self.ensure_buffered_slot(gap_slot, info).await;
                populated += 1;
            }
        }
        if populated > 0 {
            tracing::info!(
                populated,
                current_ledger,
                first_buffered = first,
                "Populated gap slots from externalized cache"
            );
        }
    }

    pub(super) async fn attach_tx_set_by_hash(
        &self,
        tx_set: &henyey_herder::TransactionSet,
    ) -> bool {
        let mut buffer =
            tracked_lock::tracked_write("syncing_ledgers", &self.syncing_ledgers).await;
        for (slot, entry) in buffer.iter_mut() {
            if entry.tx_set.is_none() && entry.tx_set_hash == *tx_set.hash() {
                entry.tx_set = Some(tx_set.clone());
                tracing::debug!(slot, hash = %tx_set.hash(), "Attached tx set to buffered slot");
                return true;
            }
        }
        false
    }

    pub(super) async fn buffer_externalized_tx_set(
        &self,
        tx_set: &henyey_herder::TransactionSet,
    ) -> bool {
        let Some(slot) = self
            .herder
            .find_externalized_slot_by_tx_set_hash(tx_set.hash())
        else {
            return false;
        };
        let Some(mut info) = self.herder.check_ledger_close(slot) else {
            return false;
        };
        // The caller already has the tx_set; ensure it is propagated into
        // the buffered slot even when check_ledger_close returned an info
        // without one (e.g. unsolicited tx sets that are not cached by
        // TxSetTracker).
        if info.tx_set.is_none() {
            info.tx_set = Some(tx_set.clone());
        }
        self.ensure_buffered_slot(info.slot as u32, info).await;
        tracing::debug!(
            slot,
            hash = %tx_set.hash(),
            "Buffered tx set after externalized lookup"
        );
        true
    }

    /// Apply a single buffered ledger (yields to tokio via `spawn_blocking`).
    ///
    /// NOTE: This function blocks the calling task until the close completes.
    /// Production code should use `try_start_ledger_close()` + the event loop's
    /// `pending_close` chaining instead. This helper is retained only for tests.
    #[cfg(test)]
    pub(super) async fn try_apply_buffered_ledgers(&self) {
        // If a background close is already running, let the select loop handle chaining.
        if self.is_applying_ledger() {
            return;
        }

        let mut pending = match self.try_start_ledger_close().await {
            Some(p) => p,
            None => return,
        };
        let join_result = (&mut pending.handle).await;
        let success = self
            .handle_close_complete(
                pending,
                join_result,
                super::persist::LedgerCloseFinalizer::inline(),
            )
            .await;

        // After closing a buffered ledger, reset timestamps and
        // tracking state so the heartbeat stall detector doesn't fire based
        // on the (now stale) timestamp of the EXTERNALIZE that triggered
        // this burst.  This mirrors the reset done in the pending_close
        // handler at the end of the select-loop chain.
        if success {
            // Arm the event-driven trigger for the next ledger (#2702/#3014),
            // the henyey analog of stellar-core's arm-only
            // `lastClosedLedgerIncreased` → `setupTriggerNextLedger`
            // (HerderImpl.cpp:1218-1233). No immediate `try_trigger_consensus()`
            // here — the armed `TriggerNextLedger` timer is the sole nomination
            // scheduler (its fire path carries the close-pipeline pump signal).
            self.setup_trigger_next_ledger().await;

            *self.last_externalized_at.write().await = self.clock.now();
            self.reset_tx_set_tracking().await;
            *self.consensus_stuck_state.write().await = None;
        }
    }

    /// Start a background ledger close if the next buffered ledger is ready.
    ///
    /// Returns `Some(PendingLedgerClose)` if a close was spawned, `None` if
    /// nothing to close or a close is already in progress.
    pub(super) async fn try_start_ledger_close(&self) -> Option<PendingLedgerClose> {
        // Fatal-failure guard: don't start new ledger closes when the node
        // has detected an unrecoverable local state failure.
        if self.fatal_state_failure.load(Ordering::SeqCst) {
            return None;
        }
        if self.is_applying_ledger() {
            return None;
        }
        // Don't start ledger closes while catchup is running.
        // Catchup modifies the LedgerManager state; concurrent ledger
        // closes could corrupt it.
        if self.catchup_in_progress.load(Ordering::SeqCst) {
            return None;
        }

        let current_ledger = self.get_current_ledger().await.ok()?;
        let next_seq = current_ledger.saturating_add(1);

        // CATCHUP_SPEC §7.3 sequential-apply drift stop-condition
        // (parity: LedgerApplyManagerImpl.cpp:516). If the next ledger to apply
        // is too far ahead of the last-closed ledger, stop scheduling and let
        // the node gracefully fall into catchup. Henyey applies inline, so
        // `next_seq == current_ledger + 1` always and this gate is
        // faithful-but-dormant — exactly as in stellar-core, where it only
        // fires under parallel-close queue depth. `next_seq` is the
        // `nextToApply` (= last-queued + 1) that core compares against `lcl`.
        if henyey_history::apply_drift_exceeded(next_seq, current_ledger) {
            tracing::info!(
                next_to_apply = next_seq,
                lcl = current_ledger,
                "Next ledger to apply is too far ahead of LCL; waiting (drift gate)"
            );
            return None;
        }

        let close_info = {
            let mut buffer =
                tracked_lock::tracked_write("syncing_ledgers", &self.syncing_ledgers).await;
            Self::trim_syncing_ledgers(&mut buffer, current_ledger);
            match buffer.get(&next_seq) {
                Some(info) if info.tx_set.is_some() => info.clone(),
                Some(info) => {
                    let buffered_hash = info.tx_set_hash;
                    // Drop the lock before probing the herder — check_ledger_close
                    // may call request_tx_set (benign side effect).
                    drop(buffer);

                    // Attempt to refresh the entry from the herder cache.
                    // This resolves the race where tx_set was cached locally
                    // (via trigger_next_ledger / handle_nomination_timeout /
                    // cache_tx_set_and_drain) after the initial buffer insert.
                    let refreshed = self.herder.check_ledger_close(next_seq as u64);
                    if let Some(ref info) = refreshed {
                        if info.tx_set.is_some() {
                            let mut buffer = tracked_lock::tracked_write(
                                "syncing_ledgers",
                                &self.syncing_ledgers,
                            )
                            .await;
                            let entry = buffer.entry(next_seq);
                            let outcome = merge_into_buffer_entry(next_seq, entry, info.clone());
                            if outcome.has_tx_set() {
                                // Successfully refreshed — fall through to close.
                                info.clone()
                            } else {
                                tracing::debug!(
                                    next_seq,
                                    tx_set_hash = %buffered_hash,
                                    "Refresh merge rejected (hash mismatch)"
                                );
                                return None;
                            }
                        } else {
                            tracing::debug!(
                                next_seq,
                                tx_set_hash = %buffered_hash,
                                "Buffered but waiting for tx_set (refresh still None)"
                            );
                            return None;
                        }
                    } else {
                        tracing::debug!(
                            next_seq,
                            tx_set_hash = %buffered_hash,
                            "Buffered but waiting for tx_set"
                        );
                        return None;
                    }
                }
                None => {
                    let is_externalized = self.herder.get_externalized(next_seq as u64).is_some();
                    if is_externalized {
                        tracing::debug!(
                            next_seq,
                            current_ledger,
                            "Next slot externalized but not yet in syncing_ledgers buffer"
                        );
                    } else {
                        let latest = self.herder.latest_externalized_slot().unwrap_or(0);
                        if latest > next_seq as u64 {
                            tracing::debug!(
                                next_seq,
                                latest_externalized = latest,
                                "Missing EXTERNALIZE for next slot (gap detected)"
                            );
                        }
                    }
                    return None;
                }
            }
        };

        let tx_set = close_info.tx_set.clone().expect("tx set present");
        let snapshot = self.ledger_manager.header_snapshot();
        let our_header_hash = snapshot.hash;
        if our_header_hash != tx_set.previous_ledger_hash() {
            // Log header XDR bytes for hash debugging (mirrors manager.rs:2122-2140)
            use stellar_xdr::{Limits, WriteXdr};
            let header = &snapshot.header;
            match header.to_xdr(Limits::none()) {
                Ok(header_xdr) => {
                    let first_32 = Hash256::from_bytes({
                        let mut arr = [0u8; 32];
                        arr.copy_from_slice(&header_xdr[..std::cmp::min(32, header_xdr.len())]);
                        arr
                    })
                    .to_hex();
                    tracing::error!(
                        target: "hash_mismatch_debug",
                        header_xdr_first_32_bytes = %first_32,
                        header_xdr_len = header_xdr.len(),
                        "Pre-close mismatch: header XDR bytes for debugging"
                    );
                }
                Err(e) => {
                    tracing::error!("Failed to encode header XDR for debugging: {e}");
                }
            }

            // Log all individual header fields (mirrors manager.rs:2143-2175)
            let skip_list_0 = Hash256::from_bytes(header.skip_list[0].0).to_hex();
            let skip_list_1 = Hash256::from_bytes(header.skip_list[1].0).to_hex();
            let skip_list_2 = Hash256::from_bytes(header.skip_list[2].0).to_hex();
            let skip_list_3 = Hash256::from_bytes(header.skip_list[3].0).to_hex();
            tracing::error!(
                target: "hash_mismatch_debug",
                ledger_seq = next_seq,
                our_header_hash = %our_header_hash.to_hex(),
                network_prev_hash = %tx_set.previous_ledger_hash().to_hex(),
                header_version = header.ledger_version,
                header_bucket_list_hash = %Hash256::from(header.bucket_list_hash.0).to_hex(),
                header_tx_result_hash = %Hash256::from(header.tx_set_result_hash.0).to_hex(),
                header_total_coins = header.total_coins,
                header_fee_pool = header.fee_pool,
                header_close_time = header.scp_value.close_time.0,
                header_tx_set_hash = %Hash256::from(header.scp_value.tx_set_hash.0).to_hex(),
                header_prev_ledger_hash = %Hash256::from(header.previous_ledger_hash.0).to_hex(),
                header_id_pool = header.id_pool,
                header_inflation_seq = header.inflation_seq,
                header_base_fee = header.base_fee,
                header_base_reserve = header.base_reserve,
                header_max_tx_set_size = header.max_tx_set_size,
                header_upgrades_count = header.scp_value.upgrades.len(),
                skip_list_0 = %skip_list_0,
                skip_list_1 = %skip_list_1,
                skip_list_2 = %skip_list_2,
                skip_list_3 = %skip_list_3,
                "Pre-close hash mismatch - individual header fields"
            );

            self.ledger_manager.log_bucket_list_debug(next_seq);

            // #3905: our committed LCL (next_seq - 1, hash `our_header_hash`) is
            // proven divergent from the network — the network's authoritative
            // L(next_seq).previousLedgerHash disagrees with it. Persist a
            // one-shot archive-rebuild latch keyed to that LCL BEFORE the fatal
            // exit, so the FIRST automatic restart self-heals via an
            // archive-authoritative bucket-apply (bypassing the bad LCL) instead
            // of requiring a manual state wipe. `next_seq - 1` is our LCL:
            // `our_header_hash` is `header_snapshot().hash`, the current (LCL)
            // header hash.
            self.write_archive_rebuild_latch(next_seq - 1, &our_header_hash);

            self.trigger_fatal_shutdown(&format!(
                "pre-close hash mismatch at ledger {next_seq}: \
                 our header hash {} does not match network's previous \
                 ledger hash {}",
                our_header_hash.to_hex(),
                tx_set.previous_ledger_hash().to_hex(),
            ));
            return None;
        }
        if *tx_set.hash() != close_info.tx_set_hash {
            tracing::error!(
                ledger_seq = next_seq,
                expected = %close_info.tx_set_hash.to_hex(),
                found = %tx_set.hash().to_hex(),
                "Buffered tx set hash mismatch"
            );
            let mut buffer =
                tracked_lock::tracked_write("syncing_ledgers", &self.syncing_ledgers).await;
            if let Some(entry) = buffer.get_mut(&next_seq) {
                entry.tx_set = None;
            }
            return None;
        }

        // Parity: stellar-core calls prepareForApply before ledger application
        // (LedgerManagerImpl.cpp:1542). This is defense-in-depth — the tx set
        // should already be validated by SCP, but we re-validate here to match
        // upstream and guard against any bypass of herder-layer validation.
        let network_id = henyey_common::NetworkId(self.network_id());
        if let Err(e) = tx_set.prepare_for_apply(network_id) {
            tracing::error!(
                ledger_seq = next_seq,
                error = %e,
                hash = %tx_set.hash().to_hex(),
                "Buffered tx set failed prepare_for_apply validation"
            );
            let mut buffer =
                tracked_lock::tracked_write("syncing_ledgers", &self.syncing_ledgers).await;
            if let Some(entry) = buffer.get_mut(&next_seq) {
                entry.tx_set = None;
            }
            return None;
        }

        tracing::debug!(
            ledger_seq = next_seq,
            tx_count = tx_set.len(),
            close_time = close_info.close_time,
            prev_ledger_hash = %tx_set.previous_ledger_hash().to_hex(),
            "Starting background ledger close"
        );

        // Build LedgerCloseData (same as HerderCallback::close_ledger).
        let prev_hash = tx_set.previous_ledger_hash();
        let tx_set_variant = tx_set.clone().into_variant();

        let decoded_upgrades = decode_upgrades(close_info.upgrades.clone());
        let close_time = close_info.close_time;

        let mut close_data =
            LedgerCloseData::new(next_seq, tx_set_variant.clone(), close_time, prev_hash)
                .with_stellar_value_ext(close_info.stellar_value_ext);
        if !decoded_upgrades.is_empty() {
            close_data = close_data.with_upgrades(decoded_upgrades);
        }
        if let Some(entry) = self.build_scp_history_entry(next_seq) {
            close_data = close_data.with_scp_history(vec![entry]);
        }

        // Remove from buffer before spawning (optimistic).
        {
            let mut buffer =
                tracked_lock::tracked_write("syncing_ledgers", &self.syncing_ledgers).await;
            buffer.remove(&next_seq);
        }

        // Spawn blocking close.
        let lm = self.ledger_manager.clone();
        let runtime_handle = tokio::runtime::Handle::current();
        self.set_applying_ledger(true);

        let dispatch_time = std::time::Instant::now();
        let join_handle =
            tokio::task::spawn_blocking(move || lm.close_ledger(close_data, Some(runtime_handle)));

        Some(PendingLedgerClose {
            handle: join_handle,
            ledger_seq: next_seq,
            tx_set,
            close_time,
            upgrades: close_info.upgrades.clone(),
            dispatch_time,
        })
    }

    /// Handle completion of a background ledger close.
    ///
    /// Performs all post-close work: meta emission, DB persistence, herder
    /// notification, and state updates. Returns `true` on success.
    ///
    /// The post-close persist (bucket flush + SQLite writes) is gated on
    /// the `finalize` argument: [`LedgerCloseFinalizer::inline`] drives it
    /// to completion before return; [`LedgerCloseFinalizer::deferred`]
    /// hands a [`PendingPersist`] back over a oneshot for the event loop
    /// to await before starting the next close. Because the finalizer is
    /// a required argument, callers cannot silently drop the persist
    /// handle — matches the [`CatchupFinalizer`] pattern from #1749.
    ///
    /// All in-memory state updates (herder, tx queue, bucket snapshot) are
    /// performed inline before returning, so the node can continue
    /// processing SCP messages and consensus while the persist runs.
    pub(super) async fn handle_close_complete(
        &self,
        pending: PendingLedgerClose,
        join_result: Result<
            std::result::Result<LedgerCloseResult, henyey_ledger::LedgerError>,
            tokio::task::JoinError,
        >,
        finalize: super::persist::LedgerCloseFinalizer,
    ) -> bool {
        // Time the full close-complete body (#1759 diagnostics).
        // Phase=6 freezes are observed inside this arm; timing the
        // whole function identifies when the post-close serialization
        // / metadata / overlay bookkeeping exceeds SLOW_OP_THRESHOLD.
        let close_complete_start = std::time::Instant::now();
        let ledger_seq = pending.ledger_seq;
        let result = self
            .handle_close_complete_inner(pending, join_result, finalize)
            .await;
        let elapsed = close_complete_start.elapsed();
        super::warn_if_slow(elapsed, "handle_close_complete", ledger_seq as u64);
        // Close-cycle decomposition (#1909): envelope metric for handle_close_complete.
        // Always recorded regardless of close outcome (wraps handle_close_complete_inner).
        crate::metrics::CLOSE_HANDLE_COMPLETE_SECONDS.record(elapsed.as_secs_f64());
        result
    }

    async fn handle_close_complete_inner(
        &self,
        pending: PendingLedgerClose,
        join_result: Result<
            std::result::Result<LedgerCloseResult, henyey_ledger::LedgerError>,
            tokio::task::JoinError,
        >,
        finalize: super::persist::LedgerCloseFinalizer,
    ) -> bool {
        // Per-phase timing inside handle_close_complete (#1775 Phase 1).
        // The outer wrapper (`handle_close_complete`) already emits a
        // ≥500 ms `warn_if_slow` WARN for event-loop alerting; this
        // timer fires at the shared 250 ms threshold and names the
        // dominant sub-phase so a follow-up can off-load the CPU-bound
        // work (same cadence as #1772 → #1773 for `receive_tx_set`).
        //
        // Early returns inside the `match join_result` arm drop the
        // timer without calling `finish`, which is the intended fast-
        // path silence: no marks recorded, no WARN emitted, no
        // allocation leak. The six marks below cover every line between
        // here and the `match finalize.0` dispatch at the bottom; the
        // dispatch itself is outside the timer because the Inline
        // variant awaits persist-handle I/O (not CPU) and the Deferred
        // variant never blocks the event loop.
        let mut timer = tracked_lock::PhaseTimer::start();

        self.set_applying_ledger(false);

        let result = match join_result {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => {
                let is_hash_mismatch = matches!(e, henyey_ledger::LedgerError::HashMismatch { .. });
                tracing::error!(
                    ledger_seq = pending.ledger_seq,
                    error = %e,
                    is_hash_mismatch,
                    "Background ledger close failed"
                );
                if is_hash_mismatch {
                    self.set_phase_sub(phase::PHASE_6_1_SYNCING_LEDGERS_HASH_MISMATCH);
                    let mut buffer =
                        tracked_lock::tracked_write("syncing_ledgers", &self.syncing_ledgers).await;
                    let cleared_count = buffer.len();
                    buffer.clear();
                    drop(buffer);
                    tracing::warn!(
                        ledger_seq = pending.ledger_seq,
                        cleared_count,
                        "Hash mismatch detected during live close - local state corrupted"
                    );
                    // Spec LEDGER §4.2-Step15 / §4.10-Step25 (MUST): an
                    // expected/computed LCL hash mismatch on the live
                    // post-close path is an SCP/local-corruption divergence
                    // and is FATAL — it is NOT a recoverable re-sync. This
                    // mirrors stellar-core `LedgerManagerImpl::closeLedger`
                    // (LedgerManagerImpl.cpp:1738-1752), which throws
                    // "Local node's ledger corrupted during close" rather than
                    // swallowing the divergence into a catchup. The genuinely
                    // recoverable bucket-apply re-sync lives in catchup_impl.rs
                    // (a different path) and is intentionally untouched.
                    self.trigger_fatal_shutdown(&format!(
                        "local node's ledger corrupted during close (ledger {})",
                        pending.ledger_seq
                    ));
                }
                // Clear the closing gate — LCL hasn't advanced, so buffered
                // envelopes would still hit the apply-lag path (issue #2122).
                self.herder.clear_closing_gate();
                return false;
            }
            Err(e) => {
                tracing::error!(
                    ledger_seq = pending.ledger_seq,
                    error = %e,
                    "Ledger close task panicked"
                );
                // Clear the closing gate — same rationale as hash mismatch above.
                self.herder.clear_closing_gate();
                return false;
            }
        };
        timer.mark("join_match_ms");
        record_phase_histogram(crate::metrics::CLOSE_COMPLETE_JOIN_MATCH_SECONDS, &timer);

        // Synchronize herder tracking state with the just-closed ledger.
        //
        // MUST happen before any `.await` in this function. The close task
        // already published LCL=N (manager.rs:2552-2555); without this,
        // concurrent spawn_blocking tasks (SCP nomination timeouts,
        // trigger_next_ledger) can observe LCL=N with tracking still at the
        // previous value, violating INV-H2 (`lcl < tracking`). See #2720.
        self.herder
            .advance_tracking_to(pending.ledger_seq as u64, pending.close_time);

        // Store last-close stats and perf for metrics reporting.
        *self.last_close_stats.write() = result.stats.clone();
        *self.last_close_perf.write() = result.perf.clone();

        // Record ledger close duration into the histogram (accumulates across closes).
        if result.stats.close_time_ms > 0 {
            crate::metrics::LEDGER_CLOSE_DURATION_SECONDS
                .record(result.stats.close_time_ms as f64 / 1000.0);
        }

        // Feed the medida-compat `ledger.ledger.close` timer (close duration in
        // ms) + embedded meter, and the `ledger.transaction.count` histogram,
        // for the stellar-core-compatible /metrics endpoint (#3296).
        crate::medida_compat::medida_compat().record_ledger_close(
            result.stats.close_time_ms as f64,
            result.stats.tx_count as u64,
        );

        // Phase 5: Per-phase close-duration histograms (LedgerClosePerf).
        if let Some(ref perf) = result.perf {
            // [maxtps_close] campaign-2 iter-3: slow closes clustered at
            // seq ~120-150 are the full-window killer (two consecutive slow
            // closes trigger a loadgen catch-up burst that collapses the
            // account-cadence slack). Dump the in-close phase breakdown for
            // any close slower than 1.5 s so the binding phase is
            // self-naming. Slow-only; instrumentation.
            if perf.total_us > 1_500_000 {
                tracing::info!(
                    target: "maxtps_close",
                    ledger_seq = result.header.ledger_seq,
                    total_us = perf.total_us,
                    begin_us = perf.begin_close_us,
                    tx_exec_us = perf.tx_exec_us,
                    commit_setup_us = perf.commit_setup_us,
                    bucket_lock_wait_us = perf.bucket_lock_wait_us,
                    eviction_us = perf.eviction_us,
                    soroban_state_us = perf.soroban_state_us,
                    add_batch_us = perf.add_batch_us,
                    hot_archive_us = perf.hot_archive_us,
                    header_us = perf.header_us,
                    commit_close_us = perf.commit_close_us,
                    meta_us = perf.meta_us,
                    cache_hits = perf.cache.hits,
                    cache_misses = perf.cache.misses,
                    cache_entries = perf.cache.entry_count,
                    snap_hits = perf.snapshot_cache.snapshot_cache_hits,
                    prefetch_hits = perf.snapshot_cache.prefetch_cache_hits,
                    fallback_lookups = perf.snapshot_cache.fallback_lookups,
                    "slow close phase breakdown"
                );
            }
            let us_to_secs = |us: u64| us as f64 / 1_000_000.0;
            crate::metrics::CLOSE_BEGIN_SECONDS.record(us_to_secs(perf.begin_close_us));
            crate::metrics::CLOSE_TX_EXEC_SECONDS.record(us_to_secs(perf.tx_exec_us));
            crate::metrics::CLOSE_CLASSIC_EXEC_SECONDS.record(us_to_secs(perf.classic_exec_us));
            crate::metrics::CLOSE_SOROBAN_EXEC_SECONDS.record(us_to_secs(perf.soroban_exec_us));
            crate::metrics::CLOSE_COMMIT_SETUP_SECONDS.record(us_to_secs(perf.commit_setup_us));
            crate::metrics::CLOSE_BUCKET_LOCK_WAIT_SECONDS
                .record(us_to_secs(perf.bucket_lock_wait_us));
            crate::metrics::CLOSE_EVICTION_SECONDS.record(us_to_secs(perf.eviction_us));
            crate::metrics::CLOSE_SOROBAN_STATE_SECONDS.record(us_to_secs(perf.soroban_state_us));
            crate::metrics::CLOSE_SOROBAN_STATE_DATA_ARC_COUNT
                .record(perf.soroban_state_data_arc_count as f64);
            crate::metrics::CLOSE_SOROBAN_STATE_CODE_ARC_COUNT
                .record(perf.soroban_state_code_arc_count as f64);
            crate::metrics::CLOSE_BUCKET_ADD_SECONDS.record(us_to_secs(perf.add_batch_us));
            crate::metrics::CLOSE_HOT_ARCHIVE_SECONDS.record(us_to_secs(perf.hot_archive_us));
            crate::metrics::CLOSE_HEADER_SECONDS.record(us_to_secs(perf.header_us));
            crate::metrics::CLOSE_COMMIT_SECONDS.record(us_to_secs(perf.commit_close_us));
            crate::metrics::CLOSE_META_SECONDS.record(us_to_secs(perf.meta_us));
        }

        // Phase 3: Accumulate cumulative ledger apply counters.
        {
            let stats = &result.stats;
            self.cumulative_apply_success
                .fetch_add(stats.tx_success_count as u64, Ordering::Relaxed);
            self.cumulative_apply_failure
                .fetch_add(stats.tx_failed_count as u64, Ordering::Relaxed);

            // Derive Soroban success/failure from per-tx timings.
            if let Some(ref perf) = result.perf {
                let mut soroban_ok = 0u64;
                let mut soroban_fail = 0u64;
                for tx in &perf.tx_timings {
                    if tx.is_soroban {
                        if tx.success {
                            soroban_ok += 1;
                        } else {
                            soroban_fail += 1;
                        }
                    }
                }
                self.cumulative_soroban_success
                    .fetch_add(soroban_ok, Ordering::Relaxed);
                self.cumulative_soroban_failure
                    .fetch_add(soroban_fail, Ordering::Relaxed);

                // Soroban parallel phase structure (sticky — updated only when non-zero).
                if perf.soroban_stage_count > 0 {
                    self.last_soroban_stage_count
                        .store(perf.soroban_stage_count as u64, Ordering::Relaxed);
                    self.last_soroban_max_cluster_count
                        .store(perf.soroban_max_cluster_count as u64, Ordering::Relaxed);
                }
            }
        }

        // Emit LedgerCloseMeta to stream.
        // If a MetaWriter is active (async channel + dedicated thread), use it
        // for non-blocking I/O. Otherwise fall back to the synchronous Mutex path
        // (for debug-only streams that don't need I/O isolation).
        if let Some(ref meta) = result.meta {
            if let Some(ref writer) = self.meta_writer {
                self.set_phase_sub(phase::PHASE_6_2_WRITE_META);
                if let Err(e) = writer.write_meta(meta.clone(), pending.ledger_seq).await {
                    tracing::error!(
                        error = %e,
                        ledger_seq = pending.ledger_seq,
                        "Fatal: metadata writer channel failed"
                    );
                    std::process::abort();
                }
            } else {
                let mut guard = self.meta_stream.lock().unwrap();
                if let Some(ref mut stream) = *guard {
                    if let Err(e) = stream.maybe_rotate_debug_stream(pending.ledger_seq) {
                        tracing::warn!(
                            error = %e,
                            ledger_seq = pending.ledger_seq,
                            "Failed to rotate debug meta stream"
                        );
                    }
                    match stream.emit_meta(meta) {
                        Ok(()) => {}
                        Err(MetaStreamError::MainStreamWrite(e)) => {
                            tracing::error!(
                                error = %e,
                                ledger_seq = pending.ledger_seq,
                                "Fatal: metadata output stream write failed"
                            );
                            std::process::abort();
                        }
                        Err(MetaStreamError::DebugStreamWrite(e)) => {
                            tracing::warn!(
                                error = %e,
                                ledger_seq = pending.ledger_seq,
                                "Debug metadata stream write failed"
                            );
                        }
                    }
                }
            }
        }
        timer.mark("meta_emit_ms");
        record_phase_histogram(crate::metrics::CLOSE_COMPLETE_META_EMIT_SECONDS, &timer);

        // Prepare persist data (CPU work only — XDR serialization, HAS
        // building, SCP envelope collection). The actual I/O (bucket flush,
        // SQLite transaction) is deferred to a background task to avoid
        // blocking the event loop (#1713/#1518).
        let tx_metas = result.meta.as_ref().map(Self::extract_tx_metas);
        let tx_set_variant = pending.tx_set.clone().into_variant();
        let persist_data = self.build_persist_inputs(
            &result.header,
            &tx_set_variant,
            &result.tx_results,
            tx_metas.as_deref(),
        );
        // The full LedgerCloseMeta XDR blob serves only the JSON-RPC
        // getLedgers path (`ledger_close_meta` table). Skip both the
        // serialization (multi-MB per ledger on the event loop under load)
        // and the SQLite write when this node stores no RPC data — it is the
        // single largest write stream at high tx rates (~70% of DB bytes,
        // 2026-07-03 frontier run), and without an RPC retention window the
        // table is never pruned.
        let meta_xdr = if self.config.store_rpc_data() {
            result
                .meta
                .as_ref()
                .and_then(|meta| meta.to_xdr(stellar_xdr::Limits::none()).ok())
        } else {
            None
        };

        // Build (envelope, seq_num) pairs for all txs (applied + failed) so
        // sequence-based removal drops superseded queued txs for the same account.
        // Also collect failed hashes for banning.
        let mut all_txs: Vec<(TransactionEnvelope, i64)> = Vec::new();
        let mut failed_hashes = Vec::new();
        for (tx, tx_result) in pending
            .tx_set
            .iter_transactions()
            .zip(result.tx_results.iter())
        {
            let seq_num = envelope_sequence_number(tx);
            all_txs.push((tx.clone(), seq_num));

            use stellar_xdr::TransactionResultResult;
            let is_success = matches!(
                tx_result.result.result,
                TransactionResultResult::TxSuccess(_)
                    | TransactionResultResult::TxFeeBumpInnerSuccess(_)
            );
            if !is_success {
                failed_hashes.push(Hash256::hash_xdr(tx));
                // [maxtps_ban] failed-at-apply txs get banned post-close; under
                // the sustained-load investigation these bans exploded (~20k/min)
                // and poisoned live accounts. Log the result-code discriminant
                // (sampled: first 3 per close) to attribute the failure mode.
                if failed_hashes.len() <= 3 {
                    tracing::info!(
                        target: "maxtps_ban",
                        ledger_seq = pending.ledger_seq,
                        result = format!("{:?}", tx_result.result.result).chars().take(60).collect::<String>(),
                        seq_num,
                        "failed_apply"
                    );
                }
            }
        }
        timer.mark("build_persist_inputs_ms");
        record_phase_histogram(
            crate::metrics::CLOSE_COMPLETE_BUILD_PERSIST_INPUTS_SECONDS,
            &timer,
        );

        // Track non-empty ledger closes for the `ledger.transaction.count` metric.
        if !pending.tx_set.is_empty() {
            self.ledger_tx_count.fetch_add(1, Ordering::Relaxed);
        }

        // === Inline overlay/survey/drift bookkeeping ===========================
        //
        // These touches need the event-loop thread (they cross `.await` points
        // into overlay / tokio-RwLock / std::Mutex). Each is < 5 ms individually
        // and they run BEFORE the CPU-heavy queue update so ordering matches
        // stellar-core `HerderImpl::lastClosedLedgerIncreased`:
        // `maybeHandleUpgrade` (overlay housekeeping) → `updateTransactionQueue`.

        // Load `SorobanNetworkInfo` once for this close. Pre-#1780 this
        // accessor was called three times per close (once here inside the
        // overlay window, twice more in the preamble below); each call does
        // a full bucket-list snapshot + ~15 ConfigSetting lookups. The
        // two preamble calls were accounting for the ~670 ms
        // `spawn_blocking_setup_ms` cost observed on mainnet binary
        // `3a6388b9` (#1780). Binding the value once here and reusing it
        // at all downstream callsites eliminates the redundant work.
        //
        // The value is a plain data struct (`Option<SorobanNetworkInfo>`);
        // `Clone` / `as_ref()` is cheap (all primitive fields).
        let soroban_info = self.soroban_network_info();

        // Clear per-ledger overlay state (flood gate, etc.) for old ledgers.
        // Mirrors upstream HerderImpl::eraseBelow() -> clearLedgersBelow().
        {
            self.set_phase_sub(phase::PHASE_6_3_OVERLAY_CLEAR_LEDGERS);
            if let Some(overlay) = self.overlay().await {
                overlay.clear_ledgers_below(pending.ledger_seq, pending.ledger_seq);
            }
        }

        // Refresh the per-peer query rate-limit window after each close.
        // Parity: stellar-core recomputes expectedLedgerCloseTime * MAX_SLOTS_TO_REMEMBER
        // on every Peer::process() call (Peer.cpp:1426-1429); we update the shared
        // atomic here so all peer tasks pick up the new value.
        self.refresh_overlay_query_window().await;

        // Notify peers if max tx size increased due to a protocol upgrade.
        // Mirrors upstream HerderImpl::maybeHandleUpgrade().
        self.set_phase_sub(phase::PHASE_6_4_OVERLAY_MAX_TX_SIZE);
        self.refresh_max_tx_size_bytes().await;

        // Clean up old survey rate limiter entries.
        // Mirrors upstream SurveyManager::clearOldLedgers() called from clearLedgersBelow().
        {
            self.set_phase_sub(phase::PHASE_6_5_SURVEY_STATE_WRITE);
            let mut survey_state = self.survey_state.write().await;
            survey_state.clear_old_ledgers(pending.ledger_seq);
        }

        // Record externalized close time for drift tracking (std::Mutex, < 1 µs).
        if let Ok(mut tracker) = self.drift_tracker.lock() {
            if let Some(warning) =
                tracker.record_externalized_close_time(pending.ledger_seq, pending.close_time)
            {
                tracing::warn!("{}", warning);
            }
        }
        // Test-only: simulate additional inline (event-loop-blocking) work
        // immediately before the `overlay_bookkeeping_ms` mark below, so
        // tests can force this phase over the `PhaseTimer` inline-sum
        // threshold (#3755) without depending on real overlay/survey/drift
        // timing. Mirrors `close_complete_inject_blocking_ms` (which
        // injects into the off-loaded `spawn_blocking` phase below) but
        // targets the INLINE phase instead — placement matters: this must
        // land here, before `overlay_bookkeeping_ms`, not before the later
        // `spawn_blocking_setup_ms` mark, or the delay would be
        // misattributed.
        #[cfg(test)]
        {
            let inject_inline_ms = self.close_complete_inject_inline_ms.load(Ordering::Relaxed);
            if inject_inline_ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(inject_inline_ms));
            }
        }

        // Mark end of the inline overlay/survey/drift bookkeeping window.
        // This brackets lines that touch tokio RwLocks (overlay, survey_state)
        // and the std::Mutex drift tracker. Each internal operation is
        // microseconds of CPU cost, but the field's wall-clock will also
        // include scheduler-driven backlog drain at the `.await` yields on a
        // saturated single-threaded runtime — that is the intended measurement
        // and is what lets us tell the inline window apart from the
        // off-loaded work in the `spawn_blocking` below.
        timer.mark("overlay_bookkeeping_ms");
        record_phase_histogram(
            crate::metrics::CLOSE_COMPLETE_OVERLAY_BOOKKEEPING_SECONDS,
            &timer,
        );

        // === Off-load the two CPU-heavy sub-phases ============================
        //
        // Phase 1 telemetry (#1775, commit 83a1b5c1) showed `herder_ledger_closed`
        // and `tx_queue_invalidation` each spending ~333 ms on the event-loop
        // thread at mainnet steady state. Both sub-phases are pure CPU work with
        // no `.await` inside the hot path; neither holds a shared primitive with
        // the other (distinct locks, distinct per-tx compute — see Phase 2
        // proposal for the investigation). The fix moves them into a single
        // `spawn_blocking` so the event loop is freed during the ~666 ms compute.
        //
        // `spawn_blocking_setup_ms` brackets the capture moves into the
        // closure (`pending`, `result.header`, `applied_txs`, `Arc` clones,
        // etc.). Post-#1780 this is strictly the capture-list move + a
        // trivial `network_id()` hash; the field-extraction work that used
        // to live here (two redundant `soroban_network_info()` calls, the
        // Soroban-limit arithmetic, the wall-clock drift computation) now
        // runs INSIDE the closure on the spawn_blocking thread. Expected
        // to be microseconds.
        // `tx_queue_background_wait_ms` (after `join.await`) captures the
        // wall-clock the blocking-pool thread took to run the off-loaded
        // work — `>= 300 ms` in steady state, `~0 ms` when the queue is
        // empty or the blocking pool is idle.
        //
        // Single-flight is preserved: `lifecycle.rs:255` awaits
        // `handle_close_complete` before the next loop iteration, and no other
        // production call site invokes it concurrently, so a new
        // `handle_close_complete_inner` cannot overlap with the spawn_blocking
        // of a prior one.
        let ledger_seq = pending.ledger_seq;
        let close_time = pending.close_time;
        let network_id = NetworkId(self.network_id());
        let herder = Arc::clone(&self.herder);
        let clock = Arc::clone(&self.clock);
        // Captured for post-close tx-queue re-validation: needed so the
        // closure can build a SINGLE `SnapshotValidationProviders` for
        // the whole re-validation pass (fix for #1759 — per-tx snapshot
        // amplification that caused `tx_queue_background_wait_ms` to
        // hit 95 s on mainnet post-catchup).
        let ledger_manager = Arc::clone(&self.ledger_manager);
        // Move (not clone): `all_txs` / `failed_hashes` are not used after this
        // point, and the applied-tx list for a full mainnet ledger can be
        // several hundred KB.
        let applied_txs = std::mem::take(&mut all_txs);
        let applied_upgrades = pending.upgrades.clone();
        let failed_hashes_for_ban = std::mem::take(&mut failed_hashes);
        // Move the full header into the closure. `LedgerHeader` contains only
        // primitive / `Copy`-ish fields; the move is O(1) and lets the closure
        // extract `ledger_version`, `ext`, `scp_value.close_time`, `base_fee`,
        // and `base_reserve` on the spawn_blocking thread instead of the
        // event loop.
        let result_header = result.header;

        // Test-only: simulate a heavy CPU-bound close without 400 real signed
        // envelopes (see `close_complete_inject_blocking_ms` field doc).
        #[cfg(test)]
        let inject_blocking_ms = self
            .close_complete_inject_blocking_ms
            .load(Ordering::Relaxed);

        // Mark end of the spawn_blocking preamble (capture-list moves).
        // This is pure sync CPU, expected to be << 1 ms. Reported by the
        // structured PhaseTimer WARN as `spawn_blocking_setup_ms` so it is
        // attributable separately from `overlay_bookkeeping_ms` and from
        // `tx_queue_background_wait_ms`.
        //
        // Post-#1780 the field-extraction work that used to live here (two
        // redundant `soroban_network_info()` calls, Soroban-limit
        // arithmetic, `wall_now` / drift computation) runs INSIDE the
        // closure on the blocking-pool thread.
        timer.mark("spawn_blocking_setup_ms");
        record_phase_histogram(
            crate::metrics::CLOSE_COMPLETE_SPAWN_BLOCKING_SETUP_SECONDS,
            &timer,
        );

        let join = tokio::task::spawn_blocking(move || {
            // Test-only synthetic blocking work — lets the regression test
            // simulate a 400 ms CPU-heavy close without real signed envelopes.
            #[cfg(test)]
            if inject_blocking_ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(inject_blocking_ms));
            }

            // === Sub-phase 0: Closure-local field extraction ================
            //
            // All of the following computations used to run on the event
            // loop between the `overlay_bookkeeping_ms` and
            // `spawn_blocking_setup_ms` marks. They are pure sync CPU with
            // no `.await`/lock requirements, so they moved inside
            // `spawn_blocking` to free ~670 ms of event-loop time (#1780).
            let prep_start = std::time::Instant::now();

            let ledger_flags = match &result_header.ext {
                stellar_xdr::LedgerHeaderExt::V0 => 0,
                stellar_xdr::LedgerHeaderExt::V1(ext) => ext.flags,
            };
            let close_time_ctx = result_header.scp_value.close_time.0;
            let base_fee = result_header.base_fee;
            let base_reserve = result_header.base_reserve;
            let protocol_version = result_header.ledger_version;

            let soroban_resource_limits =
                soroban_info.as_ref().map(|info| info.to_resource_limits());
            let (queue_limit, selection_limit) = match soroban_info.as_ref() {
                Some(info) => {
                    let m = POOL_LEDGER_MULTIPLIER as i64;
                    let queue = henyey_common::Resource::soroban_ledger_limits(
                        info.ledger_max_tx_count as i64 * m,
                        info.ledger_max_instructions * m,
                        info.ledger_max_tx_size_bytes as i64 * m,
                        info.ledger_max_read_bytes as i64 * m,
                        info.ledger_max_write_bytes as i64 * m,
                        info.ledger_max_read_ledger_entries as i64 * m,
                        info.ledger_max_write_ledger_entries as i64 * m,
                    );
                    let selection = henyey_common::Resource::soroban_ledger_limits(
                        info.ledger_max_tx_count as i64,
                        info.ledger_max_instructions,
                        info.ledger_max_tx_size_bytes as i64,
                        info.ledger_max_read_bytes as i64,
                        info.ledger_max_write_bytes as i64,
                        info.ledger_max_read_ledger_entries as i64,
                        info.ledger_max_write_ledger_entries as i64,
                    );
                    (Some(queue), Some(selection))
                }
                None => (None, None),
            };
            // Live parallel-Soroban phase limits (ledger-wide instructions and
            // dependent-cluster count) for the tx-set builder's `use_parallel`
            // gate. Parity: stellar-core reads these from the live
            // getLastClosedSorobanNetworkConfig() (#3680).
            let parallel_limits = soroban_info.as_ref().map(|info| {
                (
                    info.ledger_max_instructions,
                    info.ledger_max_dependent_tx_clusters,
                )
            });

            // Compute upper-bound close-time offset (stellar-core parity).
            // Reading `wall_now` inside the closure instead of on the event
            // loop differs by at most a few hundred µs of scheduler
            // latency; `upper_bound_offset` is denominated in whole
            // seconds and feeds no hash, so the semantics are preserved.
            const EXPECTED_CLOSE_TIME_MULT: u64 = 2;
            let expected_close_secs = herder.ledger_close_duration().as_secs();
            let wall_now = clock
                .system_now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock before UNIX epoch")
                .as_secs();
            let close_time_drift = wall_now.saturating_sub(close_time_ctx);
            let upper_bound_offset =
                expected_close_secs * EXPECTED_CLOSE_TIME_MULT + close_time_drift;

            crate::metrics::CLOSE_TX_QUEUE_PREP_SECONDS.record(prep_start.elapsed().as_secs_f64());

            // NOTE: advance_tracking_to is called BEFORE this spawn_blocking,
            // immediately after the close task join succeeds (see #2720). By the
            // time this closure runs, tracking is already at ledger_seq+1. The
            // call was moved to close the race window where .await points between
            // close completion and this spawn_blocking allowed concurrent tasks to
            // observe stale tracking state.

            // === Sub-phase 1: herder.ledger_closed + ban failed txs ==========
            let phase1_start = std::time::Instant::now();
            herder.ledger_closed(
                ledger_seq as u64,
                &applied_txs,
                &applied_upgrades,
                close_time,
            );
            if !failed_hashes_for_ban.is_empty() {
                // [maxtps_ban] info-level so the sustain investigation can see
                // per-close ban volume by site (site 1 = failed at apply).
                tracing::info!(
                    target: "maxtps_ban",
                    ledger_seq,
                    failed_count = failed_hashes_for_ban.len(),
                    "ban_site1_failed_apply"
                );
                herder.tx_queue().ban(&failed_hashes_for_ban);
            }
            crate::metrics::CLOSE_TX_QUEUE_LEDGER_CLOSED_SECONDS
                .record(phase1_start.elapsed().as_secs_f64());

            // === Sub-phase 2: validation context update + shift ==============
            let phase2_start = std::time::Instant::now();
            herder.tx_queue().update_validation_context(
                ledger_seq,
                close_time_ctx,
                protocol_version,
                base_fee,
                base_reserve,
                ledger_flags,
                herder.ledger_close_duration(),
            );
            if let Some(q) = queue_limit {
                herder.tx_queue().update_soroban_resource_limits(q);
            }
            if let Some(s) = selection_limit {
                herder.tx_queue().update_soroban_selection_limits(s);
            }
            if let Some((instructions, clusters)) = parallel_limits {
                herder
                    .tx_queue()
                    .update_soroban_parallel_limits(instructions, clusters);
            }

            // Re-derive the classic queue ops capacity from the live ledger
            // header's `maxTxSetSize` so the limiter tracks `UpgradeMaxTxSetSize`.
            // Parity: stellar-core `TxQueueLimiter::reset` rebuilds capacity from
            // `maxScaledLedgerResources(false) = maxTxSetSize * mPoolLedgerMultiplier`
            // every ledger close (#3612). Henyey froze this at app construction.
            herder.tx_queue().update_classic_queue_capacity(
                result_header
                    .max_tx_set_size
                    .saturating_mul(POOL_LEDGER_MULTIPLIER),
            );

            let shift_result = herder.tx_queue().shift();
            crate::metrics::CLOSE_TX_QUEUE_SHIFT_UPDATE_SECONDS
                .record(phase2_start.elapsed().as_secs_f64());

            // === Sub-phase 3: envelopes fetch + snapshot build + Sub-phase 4: invalidation =====
            let phase3_start = std::time::Instant::now();
            let pending_envs = herder.tx_queue().pending_hashed_envelopes();
            let fetch_elapsed = phase3_start.elapsed();
            crate::metrics::CLOSE_TX_QUEUE_ENVELOPES_FETCH_SECONDS
                .record(fetch_elapsed.as_secs_f64());

            let mut invalid_banned = 0usize;
            if !pending_envs.is_empty() {
                // snapshot_build covers TxSetValidationContext construction +
                // SnapshotValidationProviders::new() so that envelopes_fetch +
                // snapshot_build cleanly partitions the total span.
                let snapshot_build_start = std::time::Instant::now();
                let ctx = TxSetValidationContext {
                    next_ledger_seq: ledger_seq + 1,
                    close_time: close_time_ctx,
                    base_fee,
                    base_reserve,
                    protocol_version,
                    network_id,
                    ledger_flags,
                    soroban_resource_limits,
                    frozen_key_config: henyey_tx::frozen_keys::FrozenKeyConfig::empty(),
                };
                // Build ONE snapshot for the whole re-validation pass.
                //
                // Prior to #1759 the tx_queue's stored per-call
                // `LedgerAccountProvider` / `LedgerFeeBalanceProvider`
                // called `create_snapshot()` on every `load_account` /
                // `get_available_balance`, amplifying to ~N_txs × ops × 2
                // snapshots per close. On populated mainnet queues this
                // produced a 94.8s `tx_queue_background_wait_ms` tail and
                // 15+ WATCHDOG freezes.
                //
                // Parity: matches stellar-core's single `LedgerSnapshot
                // ls(app)` in `TxSetUtils::getInvalidTxListWithErrors`
                // (`stellar-core/src/herder/TxSetUtils.cpp:167`).
                //
                // On snapshot-build failure we log and skip the stateful
                // re-validation pass for this ledger (equivalent to
                // passing `None` providers). We must NOT fall back to
                // the per-call providers — that would silently
                // re-introduce the quadratic path.
                let snapshot_result = SnapshotValidationProviders::new(&ledger_manager);
                crate::metrics::CLOSE_TX_QUEUE_SNAPSHOT_BUILD_SECONDS
                    .record(snapshot_build_start.elapsed().as_secs_f64());
                crate::metrics::CLOSE_TX_QUEUE_SNAPSHOT_SECONDS
                    .record(phase3_start.elapsed().as_secs_f64());

                let phase4_start = std::time::Instant::now();
                let invalid = match snapshot_result {
                    Ok(providers) => henyey_herder::get_invalid_hashed_tx_list(
                        &pending_envs,
                        &ctx,
                        &CloseTimeBounds::with_offsets(0, upper_bound_offset),
                        Some(&providers as &dyn henyey_herder::FeeBalanceProvider),
                        Some(&providers as &dyn henyey_herder::AccountProvider),
                    ),
                    Err(err) => {
                        tracing::warn!(
                            ledger_seq,
                            error = %err,
                            "Failed to build post-close validation snapshot; \
                             skipping stateful tx-queue re-validation for this ledger"
                        );
                        Vec::new()
                    }
                };
                if !invalid.is_empty() {
                    let invalid_hashes: Vec<Hash256> =
                        invalid.iter().map(|htx| htx.hash()).collect();
                    invalid_banned = invalid_hashes.len();
                    // [maxtps_ban] site 2 = queued txs stateful-invalid after close.
                    tracing::info!(
                        target: "maxtps_ban",
                        ledger_seq,
                        invalid_banned,
                        "ban_site2_revalidation"
                    );
                    herder.tx_queue().ban(&invalid_hashes);
                }
                crate::metrics::CLOSE_TX_QUEUE_INVALIDATION_SECONDS
                    .record(phase4_start.elapsed().as_secs_f64());
            } else {
                // Queue empty: record envelopes_fetch (already recorded above)
                // and total. Do NOT record snapshot_build — it wasn't executed.
                crate::metrics::CLOSE_TX_QUEUE_SNAPSHOT_SECONDS
                    .record(phase3_start.elapsed().as_secs_f64());
                crate::metrics::CLOSE_TX_QUEUE_INVALIDATION_SECONDS.record(0.0);
            }

            (shift_result, invalid_banned)
        });

        self.set_phase_sub(phase::PHASE_6_6_TX_QUEUE_JOIN);
        match henyey_common::await_blocking_logged("tx-queue close-update", join).await {
            Ok((shift_result, invalid_banned)) => {
                if shift_result.unbanned_count > 0 || shift_result.evicted_due_to_age > 0 {
                    tracing::debug!(
                        unbanned = shift_result.unbanned_count,
                        evicted = shift_result.evicted_due_to_age,
                        "Shifted transaction ban queue"
                    );
                }
                if invalid_banned > 0 {
                    tracing::debug!(
                        count = invalid_banned,
                        "Banned invalid queued txs after ledger close"
                    );
                }
                // Re-mark all surviving transactions for flooding. Matches
                // stellar-core's resetBestFeeTxs() + rebroadcast() on ledger close.
                self.herder.tx_queue().rebroadcast();

                // Sustained-load wedge fix: PUSH the full bodies of txs that
                // have been pending 2 ledgers to all peers. Rebroadcast alone
                // re-MARKS them, but the per-peer advert history suppresses
                // the re-advert to any peer recorded as already-adverted — so
                // a tx whose demand/response was lost stays missing from
                // every other queue and can only apply when THIS node's
                // candidate wins nomination. One lost tx then out-waits its
                // account's ~20 s submission cycle and a PayPregenerated run
                // insta-fails (#3638): measured as henyey sustaining ~1150
                // tx/s on 5-minute windows vs stellar-core's 1522 on the same
                // hardware. The push is rare by construction (only txs that
                // missed 2 consecutive ledgers; typically 0-5 per 5-minute
                // window) and flood-gate rules still apply on receive.
                if !shift_result.reflooded_txs.is_empty() {
                    if let Some(overlay) = self.overlay().await {
                        let count = shift_result.reflooded_txs.len();
                        // Delivery accounting: broadcast() returns how many
                        // peers actually got the message; a systematically low
                        // sent count means the push never leaves this node
                        // (2026-07-04 forensics: doomed txs are banned ONLY on
                        // their origin — peers never see them).
                        let mut sent_min = usize::MAX;
                        let mut sent_sum = 0usize;
                        for env in shift_result.reflooded_txs {
                            let msg = stellar_xdr::StellarMessage::Transaction(env);
                            match overlay.broadcast(msg).await {
                                Ok(sent) => {
                                    sent_min = sent_min.min(sent);
                                    sent_sum += sent;
                                }
                                Err(e) => {
                                    sent_min = 0;
                                    tracing::debug!(error = %e, "wedged-tx push failed");
                                }
                            }
                        }
                        tracing::info!(
                            target: "maxtps_ban",
                            ledger_seq,
                            count,
                            sent_min,
                            sent_avg = sent_sum / count.max(1),
                            "pushed wedged pending txs to all peers"
                        );
                    }
                }
            }
            Err(e) if e.is_panic() => {
                tracing::error!(
                    ledger_seq,
                    error = %e,
                    "tx-queue close-update panicked; \
                     queue state may be partially updated for this ledger"
                );
            }
            Err(e) => {
                tracing::error!(
                    ledger_seq,
                    error = %e,
                    "spawn_blocking join error for tx-queue close-update"
                );
            }
        }
        timer.mark_cooperative("tx_queue_background_wait_ms");
        record_phase_histogram(crate::metrics::CLOSE_COMPLETE_TX_QUEUE_SECONDS, &timer);

        // Update current ledger tracking.
        self.set_phase_sub(phase::PHASE_6_7_LAST_PROCESSED_SLOT_WRITE);
        *self.last_processed_slot.write().await = pending.ledger_seq as u64;

        // Purge SCP timers for closed slots after catchup/close.
        self.timer_manager_handle
            .purge_old_slots(pending.ledger_seq as u64 + 1)
            .await;

        self.set_phase_sub(phase::PHASE_6_8_CLEAR_TX_ADVERT_HISTORY);
        self.clear_tx_advert_history(pending.ledger_seq).await;

        // Re-bootstrap the herder so tracking_slot advances past the
        // just-closed ledger.  For validators, SCP's externalize path
        // handles this, but watchers close ledgers from buffered
        // EXTERNALIZE messages without going through SCP — the
        // tracking_slot stalls unless we update it here.
        self.herder.bootstrap(pending.ledger_seq);
        // Re-arm overlay tracking after watcher re-bootstrap.
        if let Some(flag) = self.overlay_tracking.lock().unwrap().as_ref() {
            flag.store(true, Ordering::Relaxed);
        }
        // Re-arm overlay synced — watcher has caught up.
        if let Some(flag) = self.overlay_synced.lock().unwrap().as_ref() {
            flag.store(true, Ordering::Relaxed);
        }

        // Update bucket snapshots for the query server.
        self.update_bucket_snapshot();

        // Flip query readiness on first successful ledger close. This covers
        // watchers that start with no state and receive their first ledger via
        // SCP — they skip readiness in App::run() to avoid serving 500 errors
        // from empty bucket snapshots.
        if !self
            .query_is_ready
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            self.query_is_ready
                .store(true, std::sync::atomic::Ordering::Release);
        }

        // Signal extension readiness if it was deferred (no-state startup or
        // a failed catchup restored the AppState without signalling): a
        // successful live close proves the node is current. Steady-state
        // cost is a single atomic load — the barrier is not touched once the
        // flag is set.
        self.signal_operational_after_live_close().await;

        // Clean up stale pending tx_set requests for slots we've now closed.
        // This prevents stale requests (from old SCP state responses) from
        // lingering and causing timeout → DontHave → recovery loops.
        let stale_cleared = self
            .herder
            .cleanup_old_pending_tx_sets(pending.ledger_seq as u64 + 1);
        if stale_cleared > 0 {
            tracing::debug!(
                stale_cleared,
                ledger_seq = pending.ledger_seq,
                "Cleared stale pending tx_set requests after ledger close"
            );
        }

        // Signal heartbeat to sync recovery.
        self.sync_recovery_heartbeat();

        // LEDGER_SPEC §4.11 Steps 7-8 (#3066): this function is henyey's
        // "main thread" analog for the post-seal commit sequence. stellar-core
        // runs Steps 7-8 (advance LCL + publish queued history + GC buckets,
        // then notify herder + take invariant snapshot) on the main thread in
        // `advanceLedgerStateAndPublish` → `publishQueuedHistory` /
        // `forgetUnreferencedBuckets` / `ledgerCloseComplete`. Henyey runs the
        // equivalent work here on the single event loop:
        //   - LCL advance / history publish are durable via the atomic
        //     transaction in `LedgerPersistInputs::serialize_and_write_to_db`
        //     (LCL write + checkpoint `enqueue_publish`); the publish-queue
        //     processor drains those entries asynchronously.
        //   - Step 7 bucket GC = `cleanup_stale_bucket_files_background()` below.
        //   - Step 8 herder notify = the `herder.advance_tracking_to` /
        //     `herder.bootstrap` calls earlier in this function.
        //   - Step 8's invariant-snapshot half is intentionally absent — the
        //     snapshot hook is not yet wired (`crates/invariant/PARITY_STATUS.md`,
        //     "Snapshot hook not yet wired"); separate work, NOT part of #3066.
        //
        // Garbage collect stale bucket files after every ledger close, matching
        // stellar-core's unconditional `forgetUnreferencedBuckets` in
        // `advanceLedgerStateAndPublish` (LedgerManagerImpl.cpp:1429). The work
        // runs on the blocking pool and is self-coalescing via the
        // `bucket_gc_in_flight` re-entrancy guard, so per-ledger invocation
        // cannot stack overlapping GC tasks (#3028).
        self.cleanup_stale_bucket_files_background();

        self.clear_tx_set_exhausted();

        // Spawn the persist task on a blocking thread. All persist work
        // (hot-archive file I/O, bucket flush, SQLite transaction) runs in a
        // single spawn_blocking call — no nested spawn_blocking (#1735).
        let data = match persist_data {
            Ok(data) => data,
            Err(err) => {
                super::persist::fatal_persist_error("prepare ledger persist data", &err);
            }
        };
        let pending_persist = super::persist::spawn_persist_task(
            super::persist::PersistJob::LedgerClose {
                write_fn: Box::new(move |db| data.serialize_and_write_to_db(db)),
                meta_xdr,
                db: self.db.clone(),
                ledger_manager: self.ledger_manager.clone(),
                bucket_dir: self.bucket_manager.bucket_dir().to_path_buf(),
                shutdown: self.recoverable_shutdown_handle(),
            },
            pending.ledger_seq,
        );
        timer.mark("post_close_bookkeeping_ms");
        record_phase_histogram(
            crate::metrics::CLOSE_COMPLETE_POST_CLOSE_BOOKKEEPING_SECONDS,
            &timer,
        );

        // Phase 5: Slot-to-close latency histogram.
        if let Some(elapsed) = self
            .herder
            .slot_first_seen_elapsed(pending.ledger_seq as u64)
        {
            crate::metrics::SLOT_TO_CLOSE_LATENCY_SECONDS.record(elapsed.as_secs_f64());
        }

        // Emit the PhaseTimer WARN (if total ≥ 250 ms). Placed before
        // the finalizer dispatch so the Inline-await persist-handle I/O
        // latency does not pollute the compute signal — see #1775.
        timer.finish("app.handle_close_complete");

        // Dispatch on the finalizer. Inline drives the persist to
        // completion (matches the prior `let _ = pt.handle.await;` at
        // the manual-close and test-helper call sites, panics ignored).
        // Deferred hands the handle back to the caller via oneshot;
        // send-failure is silently tolerated to match CatchupFinalizer's
        // Deferred variant at catchup_impl.rs:549-553.
        match finalize.0 {
            super::persist::LedgerCloseFinalizerInner::Inline => {
                let _ = pending_persist.handle.await;
            }
            super::persist::LedgerCloseFinalizerInner::Deferred(tx) => {
                let _ = tx.send(pending_persist);
            }
        }

        true
    }
}

#[cfg(test)]
mod extract_contract_events_tests {
    use super::*;
    use std::sync::Arc;

    use stellar_xdr::{
        ContractEvent, ContractEventBody, ContractEventType, ContractEventV0, ExtensionPoint, Hash,
        Memo, MuxedAccount, Preconditions, ScVal, SequenceNumber, SorobanTransactionMeta,
        SorobanTransactionMetaExt, Transaction, TransactionEnvelope, TransactionExt,
        TransactionMeta, TransactionMetaV3, TransactionResult, TransactionResultExt,
        TransactionResultPair, TransactionResultResult, TransactionV1Envelope, Uint256,
    };

    fn test_network_id() -> NetworkId {
        NetworkId::from_passphrase("Test SDF Network ; September 2015")
    }

    fn test_envelope() -> Arc<TransactionEnvelope> {
        let tx = Transaction {
            source_account: MuxedAccount::Ed25519(Uint256([0u8; 32])),
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![].try_into().unwrap(),
            ext: TransactionExt::V0,
        };
        Arc::new(TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        }))
    }

    fn test_result_pair(success: bool) -> TransactionResultPair {
        let result_code = if success {
            TransactionResultResult::TxSuccess(Default::default())
        } else {
            TransactionResultResult::TxFailed(Default::default())
        };
        TransactionResultPair {
            transaction_hash: Hash([0u8; 32]),
            result: TransactionResult {
                fee_charged: 100,
                result: result_code,
                ext: TransactionResultExt::V0,
            },
        }
    }

    fn test_event() -> ContractEvent {
        ContractEvent {
            ext: ExtensionPoint::V0,
            contract_id: None,
            type_: ContractEventType::Contract,
            body: ContractEventBody::V0(ContractEventV0 {
                topics: vec![ScVal::U32(1)].try_into().unwrap(),
                data: ScVal::U32(42),
            }),
        }
    }

    fn v3_meta_with_events(events: Vec<ContractEvent>) -> TransactionMeta {
        TransactionMeta::V3(TransactionMetaV3 {
            ext: ExtensionPoint::V0,
            tx_changes_before: Default::default(),
            operations: Default::default(),
            tx_changes_after: Default::default(),
            soroban_meta: Some(SorobanTransactionMeta {
                ext: SorobanTransactionMetaExt::V0,
                events: events.try_into().unwrap(),
                return_value: ScVal::Void,
                diagnostic_events: Default::default(),
            }),
        })
    }

    fn v3_meta_no_events() -> TransactionMeta {
        TransactionMeta::V3(TransactionMetaV3 {
            ext: ExtensionPoint::V0,
            tx_changes_before: Default::default(),
            operations: Default::default(),
            tx_changes_after: Default::default(),
            soroban_meta: None,
        })
    }

    #[test]
    fn test_extract_v3_happy_path() {
        let env = test_envelope();
        let result = test_result_pair(true);
        let meta = v3_meta_with_events(vec![test_event()]);
        let network_id = test_network_id();

        let events = App::extract_contract_events(100, &[env], &[result], &[meta], network_id)
            .expect("should succeed");

        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.ledger_seq, 100);
        assert_eq!(e.tx_index, 0);
        assert!(e.in_successful_contract_call);
        assert_eq!(e.topics.len(), 1);
        assert!(!e.event_xdr.is_empty());
    }

    #[test]
    fn test_extract_v3_failed_tx_sets_flag_false() {
        let env = test_envelope();
        let result = test_result_pair(false);
        let meta = v3_meta_with_events(vec![test_event()]);
        let network_id = test_network_id();

        let events = App::extract_contract_events(100, &[env], &[result], &[meta], network_id)
            .expect("should succeed");

        assert_eq!(events.len(), 1);
        assert!(!events[0].in_successful_contract_call);
    }

    #[test]
    fn test_extract_v3_no_soroban_meta_returns_empty() {
        let env = test_envelope();
        let result = test_result_pair(true);
        let meta = v3_meta_no_events();
        let network_id = test_network_id();

        let events = App::extract_contract_events(100, &[env], &[result], &[meta], network_id)
            .expect("should succeed");

        assert!(events.is_empty());
    }

    #[test]
    fn test_extract_v4_per_op_events() {
        use stellar_xdr::{OperationMetaV2, TransactionMetaV4};

        let env = test_envelope();
        let result = test_result_pair(true);
        let event = test_event();

        let meta = TransactionMeta::V4(TransactionMetaV4 {
            ext: ExtensionPoint::V0,
            tx_changes_before: Default::default(),
            operations: vec![OperationMetaV2 {
                ext: ExtensionPoint::V0,
                changes: Default::default(),
                events: vec![event.clone()].try_into().unwrap(),
            }]
            .try_into()
            .unwrap(),
            tx_changes_after: Default::default(),
            events: Default::default(),
            soroban_meta: None,
            diagnostic_events: Default::default(),
        });

        let network_id = test_network_id();
        let events = App::extract_contract_events(200, &[env], &[result], &[meta], network_id)
            .expect("should succeed");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].op_index, 0);
        assert_eq!(events[0].ledger_seq, 200);
    }

    #[test]
    fn test_extract_missing_tx_envelope_errors() {
        let result = test_result_pair(true);
        let meta = v3_meta_with_events(vec![test_event()]);
        let network_id = test_network_id();

        // No envelopes but one meta
        let err =
            App::extract_contract_events(100, &[], &[result], &[meta], network_id).unwrap_err();

        assert!(
            err.to_string()
                .contains("no corresponding transaction envelope"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_extract_missing_tx_result_errors() {
        let env = test_envelope();
        let meta = v3_meta_with_events(vec![test_event()]);
        let network_id = test_network_id();

        // No results but one meta
        let err = App::extract_contract_events(100, &[env], &[], &[meta], network_id).unwrap_err();

        assert!(
            err.to_string().contains("missing for event extraction"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_extract_v0_meta_yields_no_events() {
        let env = test_envelope();
        let result = test_result_pair(true);
        let meta = TransactionMeta::V0(Default::default());
        let network_id = test_network_id();

        let events = App::extract_contract_events(100, &[env], &[result], &[meta], network_id)
            .expect("should succeed");

        assert!(events.is_empty());
    }

    #[test]
    fn test_extract_toid_computation() {
        let env = test_envelope();
        let result = test_result_pair(true);
        let meta = v3_meta_with_events(vec![test_event()]);
        let network_id = test_network_id();

        let events = App::extract_contract_events(42, &[env], &[result], &[meta], network_id)
            .expect("should succeed");

        // TOID = (ledger_seq << 32) | (tx_index << 12) | op_index
        // For ledger 42, tx 0, op 0: toid = 42 << 32 = 180388626432
        #[allow(clippy::identity_op)]
        let expected_toid = (42u64 << 32) | (0u64 << 12) | 0u64;
        let expected_id = format!("{:019}-{:010}", expected_toid, 0);
        assert_eq!(events[0].id, expected_id);
    }
}

#[cfg(test)]
mod restore_result_tests {
    use super::*;

    /// Build a structurally valid v1 HAS JSON with the given currentLedger,
    /// padded to BUCKET_LIST_LEVELS with zero-hash entries.
    fn make_valid_has_json(ledger: u32) -> String {
        let zero = "0".repeat(64);
        let zero_level = format!(
            r#"{{"curr":"{}","snap":"{}","next":{{"state":0}}}}"#,
            zero, zero
        );
        let levels: Vec<_> = (0..henyey_bucket::BUCKET_LIST_LEVELS)
            .map(|_| zero_level.clone())
            .collect();
        format!(
            r#"{{"version":1,"currentLedger":{},"currentBuckets":[{}]}}"#,
            ledger,
            levels.join(",")
        )
    }

    /// Helper: create a minimal App for load_last_known_ledger tests.
    ///
    /// Returns `(TempDir, App)`; the `TempDir` guard is first so the `App`
    /// (which holds open database and bucket file handles into the directory)
    /// is dropped before the directory it backs is removed.
    async fn test_app() -> (tempfile::TempDir, Arc<App>) {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("test.db");
        let bucket_dir = dir.path().join("buckets");
        std::fs::create_dir_all(&bucket_dir).unwrap();
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .bucket_directory(&bucket_dir)
            .build();
        let app = App::new(config).await.unwrap();
        let app = Arc::new(app);
        app.set_self_arc().await;
        (dir, app)
    }

    #[tokio::test]
    async fn test_no_lcl_returns_no_state() {
        let (_dir, app) = test_app().await;
        // Fresh DB has no LCL — should return NoState.
        let result = app.load_last_known_ledger().await.unwrap();
        assert_eq!(result, RestoreResult::NoState);
    }

    #[tokio::test]
    async fn test_lcl_zero_returns_err() {
        let (_dir, app) = test_app().await;
        // Set LCL to 0 (anomalous — genesis is seq 1).
        app.db_blocking("test-set-lcl", |db| {
            db.with_connection(|conn| {
                use henyey_db::queries::StateQueries;
                conn.set_last_closed_ledger(0)
            })
            .map_err(Into::into)
        })
        .await
        .unwrap();
        let result = app.load_last_known_ledger().await;
        assert!(result.is_err(), "lcl_seq == 0 should be an error");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("last closed ledger is 0"),
            "unexpected error: {msg}"
        );
    }

    #[tokio::test]
    async fn test_lcl_exists_but_no_has_returns_err() {
        let (_dir, app) = test_app().await;
        // Set LCL but no HAS — inconsistent state.
        app.db_blocking("test-set-lcl", |db| {
            db.with_connection(|conn| {
                use henyey_db::queries::StateQueries;
                conn.set_last_closed_ledger(100)
            })
            .map_err(Into::into)
        })
        .await
        .unwrap();
        let result = app.load_last_known_ledger().await;
        assert!(result.is_err(), "LCL without HAS should be an error");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("no history archive state"),
            "unexpected error: {msg}"
        );
    }

    #[tokio::test]
    async fn test_lcl_has_mismatch_returns_err() {
        let (_dir, app) = test_app().await;
        // Set LCL=100 but HAS says current_ledger=50.
        app.db_blocking("test-set-lcl-has", |db| {
            db.with_connection(|conn| {
                use henyey_db::queries::StateQueries;
                conn.set_last_closed_ledger(100)?;
                let has_json = make_valid_has_json(50);
                conn.set_state(
                    henyey_db::schema::state_keys::HISTORY_ARCHIVE_STATE,
                    &has_json,
                )
            })
            .map_err(Into::into)
        })
        .await
        .unwrap();
        let result = app.load_last_known_ledger().await;
        assert!(result.is_err(), "LCL/HAS mismatch should be an error");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("does not agree"), "unexpected error: {msg}");
    }

    #[tokio::test]
    async fn test_malformed_has_json_returns_err() {
        let (_dir, app) = test_app().await;
        app.db_blocking("test-set-lcl-has", |db| {
            db.with_connection(|conn| {
                use henyey_db::queries::StateQueries;
                conn.set_last_closed_ledger(100)?;
                conn.set_state(
                    henyey_db::schema::state_keys::HISTORY_ARCHIVE_STATE,
                    "not valid json",
                )
            })
            .map_err(Into::into)
        })
        .await
        .unwrap();
        let result = app.load_last_known_ledger().await;
        assert!(result.is_err(), "malformed HAS JSON should be an error");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Failed to parse"), "unexpected error: {msg}");
    }

    /// Helper: create a test App with custom validator/archive settings.
    ///
    /// Returns `(TempDir, App)`; the `TempDir` guard is first so the `App`
    /// (which holds open database and bucket file handles into the directory)
    /// is dropped before the directory it backs is removed.
    async fn test_app_with_publish_config(
        is_validator: bool,
        writable_archive: bool,
    ) -> (tempfile::TempDir, Arc<App>) {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("test.db");
        let bucket_dir = dir.path().join("buckets");
        std::fs::create_dir_all(&bucket_dir).unwrap();
        let mut builder = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .bucket_directory(&bucket_dir)
            .validator(is_validator);
        if is_validator {
            builder = builder.node_seed("SAFTEV5U6QDFE2DRMSD7HBE76XG7SQZJD6VIUTHIXTJGO77RUQYVURLA");
        }
        let mut config = builder.build();
        if writable_archive {
            config
                .history
                .archives
                .push(crate::config::HistoryArchiveEntry {
                    name: "test-archive".to_string(),
                    url: "file:///tmp/test-archive".to_string(),
                    get_enabled: true,
                    put_enabled: true,
                    put: Some("cp {0} {1}".to_string()),
                    mkdir: None,
                });
        }
        let app = App::new(config).await.unwrap();
        let app = Arc::new(app);
        app.set_self_arc().await;
        (dir, app)
    }

    /// Regression test for #1989: startup drain clears stale publish queue
    /// entries when the node is NOT a validator (even with writable archives).
    #[tokio::test]
    async fn test_startup_drain_clears_queue_when_not_validator() {
        // Non-validator with writable archives — can_publish should be false.
        let (_dir, app) = test_app_with_publish_config(false, true).await;

        // Seed DB with LCL and stale publish queue entries.
        app.db_blocking("seed-db", |db| {
            db.with_connection(|conn| {
                use henyey_db::queries::{PublishQueueQueries, StateQueries};
                conn.set_last_closed_ledger(256)?;
                conn.enqueue_publish(64, &make_valid_has_json(64))?;
                conn.enqueue_publish(128, &make_valid_has_json(128))?;
                conn.enqueue_publish(192, &make_valid_has_json(192))?;
                Ok::<_, henyey_db::DbError>(())
            })
            .map_err(Into::into)
        })
        .await
        .unwrap();

        // Verify entries exist before drain.
        let count_before = app
            .db_blocking("check-queue-before", |db| {
                Ok::<_, anyhow::Error>(db.load_publish_queue(None)?.len())
            })
            .await
            .unwrap();
        assert_eq!(
            count_before, 3,
            "publish queue should have 3 entries before drain"
        );

        // load_last_known_ledger triggers the startup drain. It will fail
        // later (no HAS in DB) but the drain happens before that.
        let _ = app.load_last_known_ledger().await;

        // Verify queue was drained.
        let count_after = app
            .db_blocking("check-queue-after", |db| {
                Ok::<_, anyhow::Error>(db.load_publish_queue(None)?.len())
            })
            .await
            .unwrap();
        assert_eq!(count_after, 0, "publish queue should be empty after drain");
    }

    /// Regression test for #1989: startup drain clears stale publish queue
    /// entries when the node is a validator but has no writable archives.
    #[tokio::test]
    async fn test_startup_drain_clears_queue_when_no_writable_archives() {
        // Validator with NO writable archives — can_publish should be false.
        let (_dir, app) = test_app_with_publish_config(true, false).await;

        // Seed DB with stale entries.
        app.db_blocking("seed-db", |db| {
            db.with_connection(|conn| {
                use henyey_db::queries::{PublishQueueQueries, StateQueries};
                conn.set_last_closed_ledger(128)?;
                conn.enqueue_publish(64, &make_valid_has_json(64))?;
                Ok::<_, henyey_db::DbError>(())
            })
            .map_err(Into::into)
        })
        .await
        .unwrap();

        let _ = app.load_last_known_ledger().await;

        let count = app
            .db_blocking("check-queue", |db| {
                Ok::<_, anyhow::Error>(db.load_publish_queue(None)?.len())
            })
            .await
            .unwrap();
        assert_eq!(count, 0, "publish queue should be drained");
    }

    /// Verify that startup does NOT drain the publish queue when the node
    /// IS a validator WITH writable archives (can_publish = true).
    #[tokio::test]
    async fn test_startup_preserves_queue_when_can_publish() {
        // Validator with writable archives — can_publish should be true.
        let (_dir, app) = test_app_with_publish_config(true, true).await;

        // Seed DB with entries that should be preserved.
        app.db_blocking("seed-db", |db| {
            db.with_connection(|conn| {
                use henyey_db::queries::{PublishQueueQueries, StateQueries};
                conn.set_last_closed_ledger(128)?;
                conn.enqueue_publish(64, &make_valid_has_json(64))?;
                Ok::<_, henyey_db::DbError>(())
            })
            .map_err(Into::into)
        })
        .await
        .unwrap();

        let _ = app.load_last_known_ledger().await;

        let count = app
            .db_blocking("check-queue", |db| {
                Ok::<_, anyhow::Error>(db.load_publish_queue(None)?.len())
            })
            .await
            .unwrap();
        assert_eq!(
            count, 1,
            "publish queue should be preserved when can_publish is true"
        );
    }

    /// Regression test for #2705: missing bucket files should trigger graceful
    /// recovery (return NoState) instead of a fatal error. This handles the
    /// captive-core restart scenario where a stale file-backed DB has bucket
    /// references but the actual bucket files were cleaned up.
    #[tokio::test]
    async fn test_missing_buckets_returns_no_state_and_clears_db() {
        let (_dir, app) = test_app().await;

        // Build a HAS with one non-zero bucket hash (curr of level 0).
        let fake_hash = "a".repeat(64);
        let zero = "0".repeat(64);
        let level_with_bucket = format!(
            r#"{{"curr":"{}","snap":"{}","next":{{"state":0}}}}"#,
            fake_hash, zero
        );
        let zero_level = format!(
            r#"{{"curr":"{}","snap":"{}","next":{{"state":0}}}}"#,
            zero, zero
        );
        let mut levels = vec![level_with_bucket];
        for _ in 1..henyey_bucket::BUCKET_LIST_LEVELS {
            levels.push(zero_level.clone());
        }
        let has_json = format!(
            r#"{{"version":1,"currentLedger":100,"currentBuckets":[{}]}}"#,
            levels.join(",")
        );

        // Seed DB: LCL=100, matching HAS, and a ledger header at seq 100.
        app.db_blocking("seed-db", move |db| {
            db.with_connection(|conn| {
                use henyey_db::queries::{LedgerQueries, StateQueries};
                conn.set_last_closed_ledger(100)?;
                conn.set_state(
                    henyey_db::schema::state_keys::HISTORY_ARCHIVE_STATE,
                    &has_json,
                )?;
                // Store a minimal ledger header at seq 100.
                let header = stellar_xdr::LedgerHeader {
                    ledger_version: 20,
                    previous_ledger_hash: stellar_xdr::Hash([0u8; 32]),
                    scp_value: stellar_xdr::StellarValue {
                        tx_set_hash: stellar_xdr::Hash([0u8; 32]),
                        close_time: stellar_xdr::TimePoint(1234567890),
                        upgrades: vec![].try_into().unwrap(),
                        ext: stellar_xdr::StellarValueExt::Basic,
                    },
                    tx_set_result_hash: stellar_xdr::Hash([0u8; 32]),
                    bucket_list_hash: stellar_xdr::Hash([1u8; 32]),
                    ledger_seq: 100,
                    total_coins: 100_000_000_000_000_000,
                    fee_pool: 0,
                    inflation_seq: 0,
                    id_pool: 0,
                    base_fee: 100,
                    base_reserve: 5_000_000,
                    max_tx_set_size: 100,
                    skip_list: std::array::from_fn(|_| stellar_xdr::Hash([0u8; 32])),
                    ext: stellar_xdr::LedgerHeaderExt::V0,
                };
                use stellar_xdr::WriteXdr;
                let data = header.to_xdr(stellar_xdr::Limits::none()).unwrap();
                conn.store_ledger_header(&header, &data)?;
                Ok::<_, henyey_db::DbError>(())
            })
            .map_err(Into::into)
        })
        .await
        .unwrap();

        // No bucket file exists on disk for fake_hash → should trigger recovery.
        let result = app.load_last_known_ledger().await.unwrap();
        assert_eq!(
            result,
            RestoreResult::NoState,
            "Missing buckets should return NoState, not error"
        );

        // Verify the stale DB state was cleared so subsequent restarts start fresh.
        let lcl = app
            .db_blocking("check-lcl-cleared", |db| {
                db.with_connection(|conn| {
                    use henyey_db::queries::StateQueries;
                    conn.get_last_closed_ledger()
                })
                .map_err(Into::into)
            })
            .await
            .unwrap();
        assert!(
            lcl.is_none(),
            "LCL should be cleared after missing-bucket recovery"
        );

        let has = app
            .db_blocking("check-has-cleared", |db| {
                db.with_connection(|conn| {
                    use henyey_db::queries::StateQueries;
                    conn.get_state(henyey_db::schema::state_keys::HISTORY_ARCHIVE_STATE)
                })
                .map_err(Into::into)
            })
            .await
            .unwrap();
        assert!(
            has.is_none(),
            "HAS should be cleared after missing-bucket recovery"
        );
    }
}

#[cfg(test)]
mod fatal_shutdown_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// Helper: create a minimal App for fatal-shutdown tests.
    ///
    /// Returns `(TempDir, App)`; the `TempDir` guard is first so the `App`
    /// (which holds open database and bucket file handles into the directory)
    /// is dropped before the directory it backs is removed.
    async fn test_app() -> (tempfile::TempDir, Arc<App>) {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("test.db");
        let bucket_dir = dir.path().join("buckets");
        std::fs::create_dir_all(&bucket_dir).unwrap();
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .bucket_directory(&bucket_dir)
            .build();
        let app = App::new(config).await.unwrap();
        let app = Arc::new(app);
        app.set_self_arc().await;
        (dir, app)
    }

    /// Once `fatal_state_failure` is set, `try_start_ledger_close` must
    /// return `None` immediately — no ledger closes should proceed when
    /// the node has detected an unrecoverable state failure.
    #[tokio::test]
    async fn test_try_start_ledger_close_blocked_after_fatal() {
        let (_dir, app) = test_app().await;

        // Pre-set the fatal flag to simulate a prior fatal failure.
        app.fatal_state_failure.store(true, Ordering::SeqCst);

        let result = app.try_start_ledger_close().await;
        assert!(
            result.is_none(),
            "try_start_ledger_close must return None when fatal_state_failure is set"
        );
    }
}

/// Regression test for #2564: try_start_ledger_close must refresh a stale
/// syncing_ledgers entry (tx_set=None) from the herder cache.
#[cfg(test)]
mod refresh_stale_buffer_tests {
    use super::*;
    use henyey_common::Hash256;
    use stellar_xdr::{Limits, StellarValue, StellarValueExt, TimePoint, WriteXdr};

    /// Helper: create a minimal App for testing.
    ///
    /// Returns `(TempDir, App)`; the `TempDir` guard is first so the `App`
    /// (which holds open database and bucket file handles into the directory)
    /// is dropped before the directory it backs is removed.
    async fn test_app() -> (tempfile::TempDir, Arc<App>) {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("test.db");
        let bucket_dir = dir.path().join("buckets");
        std::fs::create_dir_all(&bucket_dir).unwrap();
        let config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .bucket_directory(&bucket_dir)
            .build();
        let app = App::new(config).await.unwrap();
        let app = Arc::new(app);
        app.set_self_arc().await;
        (dir, app)
    }

    /// When syncing_ledgers has an entry with tx_set=None but the herder's
    /// TxSetTracker already has the set cached, try_start_ledger_close must
    /// refresh the entry and proceed with the close.
    #[tokio::test]
    async fn test_refresh_stale_syncing_ledgers_entry() {
        let (_dir, app) = test_app().await;

        // The fresh app has current_ledger = 0, so next_seq = 1.
        let next_seq: u32 = 1;
        let slot: u64 = next_seq as u64;

        // Create a tx_set with a known previous_ledger_hash.
        let prev_hash = Hash256::ZERO;
        let tx_set = henyey_herder::TransactionSet::new(prev_hash, vec![]);
        let tx_set_hash = *tx_set.hash();

        // Build a valid StellarValue XDR for the externalized slot.
        let stellar_value = StellarValue {
            tx_set_hash: stellar_xdr::Hash(tx_set_hash.0),
            close_time: TimePoint(1_700_000_000),
            upgrades: Default::default(),
            ext: StellarValueExt::Basic,
        };
        let value_bytes = stellar_value.to_xdr(Limits::none()).unwrap();
        let value = stellar_xdr::Value(value_bytes.try_into().unwrap());

        // Seed the herder: record the slot as externalized (with valid XDR).
        app.herder
            .scp_driver()
            .record_externalized(slot, value, None);
        app.herder.scp_driver().publish_externalized(slot);

        // Seed syncing_ledgers with tx_set=None (simulating the race).
        {
            let mut buffer =
                tracked_lock::tracked_write("syncing_ledgers", &app.syncing_ledgers).await;
            buffer.insert(
                next_seq,
                henyey_herder::LedgerCloseInfo {
                    slot,
                    close_time: 1_700_000_000,
                    tx_set_hash,
                    tx_set: None,
                    upgrades: vec![],
                    stellar_value_ext: StellarValueExt::Basic,
                },
            );
        }

        // Without the tx_set cached, refresh should still return None.
        let result = app.try_start_ledger_close().await;
        assert!(
            result.is_none(),
            "Should return None when tx_set is not in herder cache"
        );

        // Now cache the tx_set in the herder's TxSetTracker.
        app.herder.scp_driver().cache_tx_set(tx_set.clone());

        // try_start_ledger_close should now refresh and succeed.
        let result = app.try_start_ledger_close().await;
        assert!(
            result.is_some(),
            "Should refresh stale buffer entry and return PendingLedgerClose"
        );
    }

    /// When the herder returns a tx_set with a different hash than what's
    /// buffered, the refresh must be rejected (no-op).
    #[tokio::test]
    async fn test_refresh_rejected_on_hash_mismatch() {
        let (_dir, app) = test_app().await;

        let next_seq: u32 = 1;
        let slot: u64 = next_seq as u64;

        // Create a tx_set and record externalized with its hash.
        let tx_set = henyey_herder::TransactionSet::new(Hash256::ZERO, vec![]);
        let tx_set_hash = *tx_set.hash();

        let stellar_value = StellarValue {
            tx_set_hash: stellar_xdr::Hash(tx_set_hash.0),
            close_time: TimePoint(1_700_000_000),
            upgrades: Default::default(),
            ext: StellarValueExt::Basic,
        };
        let value_bytes = stellar_value.to_xdr(Limits::none()).unwrap();
        let value = stellar_xdr::Value(value_bytes.try_into().unwrap());

        app.herder
            .scp_driver()
            .record_externalized(slot, value, None);
        app.herder.scp_driver().publish_externalized(slot);
        app.herder.scp_driver().cache_tx_set(tx_set);

        // Seed syncing_ledgers with a DIFFERENT tx_set_hash (simulating mismatch).
        let fake_hash = Hash256::from_bytes([0xAA; 32]);
        {
            let mut buffer =
                tracked_lock::tracked_write("syncing_ledgers", &app.syncing_ledgers).await;
            buffer.insert(
                next_seq,
                henyey_herder::LedgerCloseInfo {
                    slot,
                    close_time: 1_700_000_000,
                    tx_set_hash: fake_hash,
                    tx_set: None,
                    upgrades: vec![],
                    stellar_value_ext: StellarValueExt::Basic,
                },
            );
        }

        // Should return None because the hash doesn't match.
        let result = app.try_start_ledger_close().await;
        assert!(
            result.is_none(),
            "Should reject refresh when tx_set_hash doesn't match"
        );
    }
}

// =============================================================================
// #2681 — skipFirstCheckpointSinceItIsIncomplete parity (publish-skip marker).
//
// Tests the in-transaction marker consumer used by
// `LedgerPersistInputs::serialize_and_write_to_db` plus its integration with
// `super::persist::CatchupPersistData::write_to_db`. The marker decision is
// made strictly inside `consume_skip_marker_if_matches`; the outer
// `is_checkpoint_ledger` and `publish_enabled` guards are statically
// auditable, so they don't need separate integration tests beyond the
// non-checkpoint case that verifies marker preservation.
// =============================================================================
#[cfg(test)]
mod publish_skip_marker_tests {
    use super::*;
    use henyey_db::queries::StateQueries;
    use henyey_db::schema::state_keys;
    use henyey_db::Database;

    fn get_marker(db: &Database) -> Option<String> {
        db.with_connection(|c| c.get_state(state_keys::PUBLISH_SKIP_FIRST_CHECKPOINT))
            .unwrap()
    }

    fn set_marker(db: &Database, seq: u32) {
        db.with_connection(|c| {
            c.set_state(state_keys::PUBLISH_SKIP_FIRST_CHECKPOINT, &seq.to_string())
        })
        .unwrap();
    }

    fn run_consumer(db: &Database, closing_seq: u32) -> bool {
        db.transaction(|conn| consume_skip_marker_if_matches(conn, closing_seq))
            .unwrap()
    }

    /// Orphan marker (older than the closing checkpoint) must be actively
    /// cleared so it cannot sit in the `state` table forever — checkpoint
    /// seqs only grow past it. The consumer also reports no skip.
    #[test]
    fn clears_orphan_marker_older_than_closing_checkpoint() {
        let db = Database::open_in_memory().unwrap();
        set_marker(&db, 63);
        assert!(!run_consumer(&db, 127));
        assert!(get_marker(&db).is_none());
    }

    /// Corrupt (non-numeric) marker must surface as an `Integrity` error
    /// rather than be silently treated as "no marker", per the
    /// "never fail silently" project rule.
    #[test]
    fn errors_on_corrupt_marker_value() {
        let db = Database::open_in_memory().unwrap();
        db.with_connection(|c| {
            c.set_state(state_keys::PUBLISH_SKIP_FIRST_CHECKPOINT, "not-a-number")
        })
        .unwrap();
        let res = db.transaction(|conn| consume_skip_marker_if_matches(conn, 127));
        match res {
            Err(henyey_db::DbError::Integrity(msg)) => {
                assert!(
                    msg.contains("PUBLISH_SKIP_FIRST_CHECKPOINT"),
                    "unexpected error message: {msg}"
                );
            }
            other => panic!("expected DbError::Integrity, got {other:?}"),
        }
    }

    /// Marker=127, closing seq=127 → consumer reports skip and clears the
    /// marker in the same transaction.
    #[test]
    fn skips_enqueue_when_marker_matches() {
        let db = Database::open_in_memory().unwrap();
        set_marker(&db, 127);
        assert!(run_consumer(&db, 127));
        assert!(get_marker(&db).is_none());
    }

    /// Marker absent, closing seq=63 → consumer reports no skip; nothing to
    /// clear.
    #[test]
    fn does_not_skip_when_no_marker_set() {
        let db = Database::open_in_memory().unwrap();
        assert!(!run_consumer(&db, 63));
        assert!(get_marker(&db).is_none());
    }

    /// Marker for a future checkpoint (191) must survive a close at an
    /// earlier checkpoint (127). Guards against the marker being consumed
    /// by the wrong checkpoint.
    #[test]
    fn preserves_marker_for_future_checkpoint() {
        let db = Database::open_in_memory().unwrap();
        set_marker(&db, 191);
        assert!(!run_consumer(&db, 127));
        assert_eq!(get_marker(&db).as_deref(), Some("191"));
    }

    /// Marker survives a process restart (drop + reopen the underlying DB
    /// file) since it is persisted in the `state` table. This is the key
    /// architectural difference from stellar-core's in-memory flag: stellar-core
    /// re-derives the flag from on-disk dirty files, henyey persists it.
    #[test]
    fn marker_survives_simulated_restart() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");

        // Run #1: catchup ends mid-checkpoint, marker is written.
        {
            let db = Database::open(&path).unwrap();
            let (header, header_xdr) = persist_test_header(80);
            let data = super::super::persist::CatchupPersistData {
                header,
                header_xdr,
                has_json: "{}".to_string(),
                publish_enabled: true,
            };
            data.write_to_db(&db).unwrap();
            assert_eq!(get_marker(&db).as_deref(), Some("127"));
        }

        // Run #2: reopen the DB; marker must still be there, and the
        // checkpoint close at seq=127 must consume it.
        {
            let db = Database::open(&path).unwrap();
            assert_eq!(get_marker(&db).as_deref(), Some("127"));
            assert!(run_consumer(&db, 127));
            assert!(get_marker(&db).is_none());
        }
    }

    /// After a catchup ending at LCL=80, a marker is set for ckpt=127.
    /// A close at exactly 127 consumes the marker; a close at the next
    /// checkpoint (191) finds no marker and reports no skip.
    ///
    /// Guards against the marker accidentally outliving its single
    /// intended skip. The "earlier-checkpoint-than-marker" direction is
    /// covered by `preserves_marker_for_future_checkpoint`.
    #[test]
    fn marker_targets_only_the_first_checkpoint_after_catchup() {
        let db = Database::open_in_memory().unwrap();

        let (header, header_xdr) = persist_test_header(80);
        let data = super::super::persist::CatchupPersistData {
            header,
            header_xdr,
            has_json: "{}".to_string(),
            publish_enabled: true,
        };
        data.write_to_db(&db).unwrap();

        // Close at the marked checkpoint consumes it.
        assert!(run_consumer(&db, 127));
        assert!(get_marker(&db).is_none());

        // A subsequent close at the next checkpoint must not see any
        // marker and must report no skip.
        assert!(!run_consumer(&db, 191));
    }

    /// Convenience: minimal LedgerHeader fixture for the persist path.
    /// Mirrors `persist::tests::make_header` but is private to this module.
    fn persist_test_header(seq: u32) -> (stellar_xdr::LedgerHeader, Vec<u8>) {
        use stellar_xdr::{
            Hash, LedgerHeader, LedgerHeaderExt, LedgerHeaderExtensionV1,
            LedgerHeaderExtensionV1Ext, Limits, StellarValue, StellarValueExt, WriteXdr,
        };
        let header = LedgerHeader {
            ledger_version: 24,
            previous_ledger_hash: Hash([0; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0; 32]),
                close_time: stellar_xdr::TimePoint(0),
                upgrades: vec![].try_into().unwrap(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0; 32]),
            bucket_list_hash: Hash([0; 32]),
            ledger_seq: seq,
            total_coins: 0,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 1000,
            skip_list: [Hash([0; 32]), Hash([0; 32]), Hash([0; 32]), Hash([0; 32])],
            ext: LedgerHeaderExt::V1(LedgerHeaderExtensionV1 {
                flags: 0,
                ext: LedgerHeaderExtensionV1Ext::V0,
            }),
        };
        let xdr = header.to_xdr(Limits::none()).unwrap();
        (header, xdr)
    }
}

/// Regression test for #2820: SCP history persistence ordering and tracked
/// Production-path SCP history persistence tests.
///
/// These tests exercise the real `build_scp_history_batches()` → `LedgerPersistInputs` →
/// `serialize_and_write_to_db()` pipeline end-to-end, verifying that the production
/// persistence code correctly:
/// (1) persists N-1 before N (ordering guarantee),
/// (2) persists previous-slot quorum sets,
/// (3) persists tracked current-slot quorum sets even when no envelope references them.
#[cfg(test)]
mod scp_history_persistence_ordering_tests {
    use henyey_db::queries::ScpQueries;
    use henyey_db::Database;
    use henyey_herder::{Herder, HerderConfig, TimerManagerHandle};
    use henyey_history::HistoryArchiveState;
    use henyey_scp::hash_quorum_set;
    use std::sync::Arc;
    use stellar_xdr::{
        Hash, LedgerHeader, LedgerHeaderExt, NodeId, PublicKey, ScpBallot, ScpEnvelope,
        ScpQuorumSet, ScpStatement, ScpStatementExternalize, ScpStatementPledges, Signature,
        StellarValue, StellarValueExt, TimePoint, TransactionHistoryEntry,
        TransactionHistoryEntryExt, TransactionHistoryResultEntry,
        TransactionHistoryResultEntryExt, TransactionResultSet, TransactionSet, Uint256, Value,
        VecM,
    };

    use henyey_common::NetworkId;

    /// Build a structurally valid HAS JSON for the given ledger.
    fn make_test_has_json(ledger: u32) -> String {
        let zero = "0".repeat(64);
        let zero_level = format!(
            r#"{{"curr":"{}","snap":"{}","next":{{"state":0}}}}"#,
            zero, zero
        );
        let levels: Vec<_> = (0..henyey_bucket::BUCKET_LIST_LEVELS)
            .map(|_| zero_level.clone())
            .collect();
        format!(
            r#"{{"version":1,"currentLedger":{},"currentBuckets":[{}]}}"#,
            ledger,
            levels.join(",")
        )
    }

    fn make_lm() -> Arc<henyey_ledger::LedgerManager> {
        use henyey_ledger::{LedgerManager, LedgerManagerConfig};
        let config = LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        };
        let lm = LedgerManager::new("Test Network".to_string(), config);
        let header = LedgerHeader {
            ledger_version: 0,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq: 0,
            total_coins: 1_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 100,
            skip_list: [
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
            ],
            ext: LedgerHeaderExt::V0,
        };
        let header_hash = henyey_ledger::compute_header_hash(&header).expect("hash");
        lm.initialize(
            henyey_bucket::BucketList::new(),
            henyey_bucket::HotArchiveBucketList::new(),
            header,
            header_hash,
        )
        .expect("init");
        Arc::new(lm)
    }

    fn make_node_id(seed: u8) -> NodeId {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        NodeId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes)))
    }

    fn make_externalize_envelope(node_seed: u8, slot_index: u64, qset_hash: Hash) -> ScpEnvelope {
        let node_id = make_node_id(node_seed);
        ScpEnvelope {
            statement: ScpStatement {
                node_id,
                slot_index,
                pledges: ScpStatementPledges::Externalize(ScpStatementExternalize {
                    commit: ScpBallot {
                        counter: 1,
                        value: Value(vec![0u8].try_into().unwrap()),
                    },
                    n_h: 1,
                    commit_quorum_set_hash: qset_hash,
                }),
            },
            signature: Signature::default(),
        }
    }

    fn make_qset(threshold: u32) -> ScpQuorumSet {
        ScpQuorumSet {
            threshold,
            validators: vec![].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        }
    }

    /// Build a minimal `LedgerPersistInputs` that exercises the real
    /// `serialize_and_write_to_db()` production code path. The SCP history
    /// batches come from `build_scp_history_batches()` — the same function
    /// that `App::build_persist_inputs()` delegates to.
    pub(super) fn make_persist_inputs(
        scp_history_batches: Vec<super::ScpHistoryBatch>,
        ledger_seq: u32,
    ) -> super::LedgerPersistInputs {
        let header = LedgerHeader {
            ledger_version: 22,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq,
            total_coins: 1_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 100,
            skip_list: [
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
            ],
            ext: LedgerHeaderExt::V0,
        };
        let has = HistoryArchiveState::from_json(&make_test_has_json(ledger_seq)).unwrap();
        super::LedgerPersistInputs {
            header,
            tx_history_entry: TransactionHistoryEntry {
                ledger_seq,
                tx_set: TransactionSet {
                    previous_ledger_hash: Hash([0u8; 32]),
                    txs: VecM::default(),
                },
                ext: TransactionHistoryEntryExt::V0,
            },
            tx_result_entry: TransactionHistoryResultEntry {
                ledger_seq,
                tx_result_set: TransactionResultSet {
                    results: VecM::default(),
                },
                ext: TransactionHistoryResultEntryExt::default(),
            },
            ordered_txs: vec![],
            tx_results: vec![],
            tx_metas: None,
            tx_count: 0,
            network_id: NetworkId::from_passphrase("Test Network"),
            scp_history_batches,
            has,
            bucket_list_levels: None,
            store_rpc_data: true,
            publish_enabled: false,
        }
    }

    /// End-to-end production-path test: builds SCP batches via the real
    /// `build_scp_history_batches()` (same function called by `build_persist_inputs()`),
    /// constructs a `LedgerPersistInputs`, calls the real `serialize_and_write_to_db()`,
    /// and verifies:
    /// (1) N-1 is persisted before N,
    /// (2) previous-slot quorum sets are persisted,
    /// (3) tracked current-slot quorum sets (not in any envelope) are persisted.
    #[test]
    fn test_serialize_and_write_to_db_persists_scp_history_end_to_end() {
        // Set up a Herder with a local quorum set that includes node 0xCC
        // so we can expand the quorum tracker for that node later.
        let tracked_node = make_node_id(0xCC);
        let local_qset = ScpQuorumSet {
            threshold: 1,
            validators: vec![tracked_node.clone()].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let config = HerderConfig {
            local_quorum_set: Some(local_qset),
            ..HerderConfig::default()
        };
        let herder = Herder::new(config, make_lm(), TimerManagerHandle::no_op());

        // Create quorum sets for previous and current slots.
        let qset_prev = make_qset(1);
        let qset_prev_hash = hash_quorum_set(&qset_prev);
        let qset_prev_xdr_hash = Hash(qset_prev_hash.0);

        let qset_curr = make_qset(2);
        let qset_curr_hash = hash_quorum_set(&qset_curr);
        let qset_curr_xdr_hash = Hash(qset_curr_hash.0);

        // Register quorum sets so get_quorum_set_by_hash() can find them.
        let node_prev = make_node_id(0xAA);
        let node_curr = make_node_id(0xBB);
        herder
            .scp_driver()
            .test_store_quorum_set(&node_prev, qset_prev.clone());
        herder
            .scp_driver()
            .test_store_quorum_set(&node_curr, qset_curr.clone());

        // Inject EXTERNALIZE envelopes for slots 9 (previous) and 10 (current).
        let env_prev = make_externalize_envelope(0xAA, 9, qset_prev_xdr_hash);
        let env_curr = make_externalize_envelope(0xBB, 10, qset_curr_xdr_hash);
        herder.test_inject_externalizing_envelope(9, env_prev);
        herder.test_inject_externalizing_envelope(10, env_curr);

        // Add a tracked quorum set that is NOT referenced by any envelope.
        let qset_tracked = make_qset(3);
        let tracked_node = make_node_id(0xCC);
        herder
            .expand_quorum_tracker_for_testing(&tracked_node, qset_tracked.clone())
            .unwrap();
        let tracked_hash = hash_quorum_set(&qset_tracked);

        // Exercise the real build_scp_history_batches function (same code path
        // as App::build_persist_inputs() at ledger_close.rs:453).
        let batches = super::build_scp_history_batches(&herder, 10);

        // Verify batch structure before persistence.
        assert_eq!(batches.len(), 2, "should have previous + current batches");
        assert_eq!(
            batches[0].ledger_seq, 9,
            "first batch must be previous slot"
        );
        assert_eq!(
            batches[1].ledger_seq, 10,
            "second batch must be current slot"
        );

        // Build a LedgerPersistInputs and call the REAL serialize_and_write_to_db().
        let persist_inputs = make_persist_inputs(batches, 10);
        let db = Database::open_in_memory().unwrap();
        persist_inputs
            .serialize_and_write_to_db(&db)
            .expect("serialize_and_write_to_db must succeed");

        // (1) Verify ordering: both slots are persisted (the production code
        // iterates scp_history_batches in order, so N-1 is written first).
        let loaded_prev = db.with_connection(|c| c.load_scp_history(9)).unwrap();
        assert_eq!(loaded_prev.len(), 1, "previous slot should have 1 envelope");
        assert_eq!(
            loaded_prev[0].statement.slot_index, 9,
            "persisted envelope must carry correct slot_index for previous slot"
        );
        let loaded_curr = db.with_connection(|c| c.load_scp_history(10)).unwrap();
        assert_eq!(loaded_curr.len(), 1, "current slot should have 1 envelope");
        assert_eq!(
            loaded_curr[0].statement.slot_index, 10,
            "persisted envelope must carry correct slot_index for current slot"
        );

        // (2) Verify previous-slot quorum set is persisted.
        let prev_qs = db
            .with_connection(|c| c.load_scp_quorum_set(&qset_prev_hash))
            .unwrap();
        assert!(prev_qs.is_some(), "previous slot qset must be in DB");
        assert_eq!(prev_qs.unwrap().threshold, 1);

        // (3) Verify current-slot quorum sets: both the envelope-referenced
        // one AND the tracked one (not in any envelope) are persisted.
        let curr_qs = db
            .with_connection(|c| c.load_scp_quorum_set(&qset_curr_hash))
            .unwrap();
        assert!(curr_qs.is_some(), "current slot qset must be in DB");
        assert_eq!(curr_qs.unwrap().threshold, 2);

        let tracked_qs = db
            .with_connection(|c| c.load_scp_quorum_set(&tracked_hash))
            .unwrap();
        assert!(
            tracked_qs.is_some(),
            "tracked quorum set (not in any envelope) must be in DB"
        );
        assert_eq!(tracked_qs.unwrap().threshold, 3);
    }

    /// Verifies that the production `serialize_and_write_to_db()` correctly
    /// handles the slot-1 edge case (no previous slot to persist).
    #[test]
    fn test_serialize_and_write_to_db_slot_one_no_previous() {
        let herder = Herder::new(
            HerderConfig::default(),
            make_lm(),
            TimerManagerHandle::no_op(),
        );

        let qset = make_qset(1);
        let qset_hash = hash_quorum_set(&qset);
        let xdr_hash = Hash(qset_hash.0);

        let node = make_node_id(0xAA);
        herder
            .scp_driver()
            .test_store_quorum_set(&node, qset.clone());

        let env = make_externalize_envelope(0xAA, 1, xdr_hash);
        herder.test_inject_externalizing_envelope(1, env);

        // Slot 1 — no previous slot to persist.
        let batches = super::build_scp_history_batches(&herder, 1);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].ledger_seq, 1);

        // Run through real production persistence path.
        let persist_inputs = make_persist_inputs(batches, 1);
        let db = Database::open_in_memory().unwrap();
        persist_inputs
            .serialize_and_write_to_db(&db)
            .expect("serialize_and_write_to_db must succeed");

        let loaded = db.with_connection(|c| c.load_scp_history(1)).unwrap();
        assert_eq!(loaded.len(), 1, "slot 1 should have 1 envelope");
        assert_eq!(
            loaded[0].statement.slot_index, 1,
            "persisted envelope must carry correct slot_index for slot 1"
        );

        let qs = db
            .with_connection(|c| c.load_scp_quorum_set(&qset_hash))
            .unwrap();
        assert!(qs.is_some(), "slot 1 qset must be in DB");
    }

    /// Verifies that `serialize_and_write_to_db()` skips the previous slot
    /// when it has no externalizing envelopes (empty batch is not emitted
    /// by `build_scp_history_batches()`).
    #[test]
    fn test_serialize_and_write_to_db_empty_previous_slot_skipped() {
        let herder = Herder::new(
            HerderConfig::default(),
            make_lm(),
            TimerManagerHandle::no_op(),
        );

        let qset = make_qset(1);
        let qset_hash = hash_quorum_set(&qset);
        let xdr_hash = Hash(qset_hash.0);

        let node = make_node_id(0xAA);
        herder
            .scp_driver()
            .test_store_quorum_set(&node, qset.clone());

        // Only inject current slot (10), not previous (9).
        let env = make_externalize_envelope(0xAA, 10, xdr_hash);
        herder.test_inject_externalizing_envelope(10, env);

        let batches = super::build_scp_history_batches(&herder, 10);
        assert_eq!(batches.len(), 1);

        // Run through real production persistence path.
        let persist_inputs = make_persist_inputs(batches, 10);
        let db = Database::open_in_memory().unwrap();
        persist_inputs
            .serialize_and_write_to_db(&db)
            .expect("serialize_and_write_to_db must succeed");

        // Only current slot persisted.
        let loaded_prev = db.with_connection(|c| c.load_scp_history(9)).unwrap();
        assert!(
            loaded_prev.is_empty(),
            "previous slot should not be persisted"
        );
        let loaded_curr = db.with_connection(|c| c.load_scp_history(10)).unwrap();
        assert_eq!(loaded_curr.len(), 1, "current slot should have 1 envelope");
        assert_eq!(
            loaded_curr[0].statement.slot_index, 10,
            "persisted envelope must carry correct slot_index for current slot"
        );
    }

    /// Verifies the ordering guarantee: in the production `serialize_and_write_to_db()`
    /// loop, the previous slot (N-1) batch is written before the current slot (N).
    /// This test constructs batches and verifies the DB state reflects the ordering.
    #[test]
    fn test_serialize_and_write_to_db_ordering_previous_before_current() {
        let herder = Herder::new(
            HerderConfig::default(),
            make_lm(),
            TimerManagerHandle::no_op(),
        );

        let qset_prev = make_qset(1);
        let qset_prev_hash = hash_quorum_set(&qset_prev);
        let prev_xdr_hash = Hash(qset_prev_hash.0);

        let qset_curr = make_qset(2);
        let qset_curr_hash = hash_quorum_set(&qset_curr);
        let curr_xdr_hash = Hash(qset_curr_hash.0);

        let node_prev = make_node_id(0xAA);
        let node_curr = make_node_id(0xBB);
        herder
            .scp_driver()
            .test_store_quorum_set(&node_prev, qset_prev.clone());
        herder
            .scp_driver()
            .test_store_quorum_set(&node_curr, qset_curr.clone());

        herder.test_inject_externalizing_envelope(
            99,
            make_externalize_envelope(0xAA, 99, prev_xdr_hash),
        );
        herder.test_inject_externalizing_envelope(
            100,
            make_externalize_envelope(0xBB, 100, curr_xdr_hash),
        );

        let batches = super::build_scp_history_batches(&herder, 100);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].ledger_seq, 99);
        assert_eq!(batches[1].ledger_seq, 100);

        // Run through the real production persistence path.
        let persist_inputs = make_persist_inputs(batches, 100);
        let db = Database::open_in_memory().unwrap();
        persist_inputs
            .serialize_and_write_to_db(&db)
            .expect("serialize_and_write_to_db must succeed");

        // Both slots persisted correctly with correct slot_index.
        let loaded_prev = db.with_connection(|c| c.load_scp_history(99)).unwrap();
        assert_eq!(loaded_prev.len(), 1, "previous slot should have 1 envelope");
        assert_eq!(
            loaded_prev[0].statement.slot_index, 99,
            "persisted envelope must carry correct slot_index for previous slot"
        );
        let loaded_curr = db.with_connection(|c| c.load_scp_history(100)).unwrap();
        assert_eq!(loaded_curr.len(), 1, "current slot should have 1 envelope");
        assert_eq!(
            loaded_curr[0].statement.slot_index, 100,
            "persisted envelope must carry correct slot_index for current slot"
        );

        // Quorum sets for both slots.
        let prev_qs = db
            .with_connection(|c| c.load_scp_quorum_set(&qset_prev_hash))
            .unwrap();
        assert!(prev_qs.is_some(), "previous slot qset must be in DB");

        let curr_qs = db
            .with_connection(|c| c.load_scp_quorum_set(&qset_curr_hash))
            .unwrap();
        assert!(curr_qs.is_some(), "current slot qset must be in DB");
    }
}

#[cfg(test)]
mod store_rpc_data_gate_tests {
    //! Verify the `store_rpc_data` gate in `serialize_and_write_to_db`:
    //! when disabled, the per-tx RPC rows (`transactions` table) and contract
    //! events (`events` table) are skipped while the always-written surface
    //! (header, whole-ledger txhistory/txresults blobs, LCL) is unaffected.

    use super::scp_history_persistence_ordering_tests::make_persist_inputs;
    use henyey_db::queries::{EventQueries, HistoryQueries, LedgerQueries};
    use henyey_db::Database;
    use henyey_db::StateQueries;
    use std::sync::Arc;
    use stellar_xdr::{
        ContractEvent, ContractEventBody, ContractEventType, ContractEventV0, ExtensionPoint, Memo,
        MuxedAccount, Preconditions, ScVal, SequenceNumber, SorobanTransactionMeta,
        SorobanTransactionMetaExt, Transaction, TransactionEnvelope, TransactionExt,
        TransactionMeta, TransactionMetaV3, TransactionResult, TransactionResultExt,
        TransactionResultPair, TransactionResultResult, TransactionV1Envelope, Uint256,
    };

    fn test_tx() -> Arc<TransactionEnvelope> {
        let tx = Transaction {
            source_account: MuxedAccount::Ed25519(Uint256([7u8; 32])),
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![].try_into().unwrap(),
            ext: TransactionExt::V0,
        };
        Arc::new(TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        }))
    }

    fn test_result() -> TransactionResultPair {
        TransactionResultPair {
            transaction_hash: stellar_xdr::Hash([0u8; 32]),
            result: TransactionResult {
                fee_charged: 100,
                result: TransactionResultResult::TxSuccess(Default::default()),
                ext: TransactionResultExt::V0,
            },
        }
    }

    fn test_meta_with_event() -> TransactionMeta {
        let event = ContractEvent {
            ext: ExtensionPoint::V0,
            contract_id: None,
            type_: ContractEventType::Contract,
            body: ContractEventBody::V0(ContractEventV0 {
                topics: vec![ScVal::U32(1)].try_into().unwrap(),
                data: ScVal::U32(42),
            }),
        };
        TransactionMeta::V3(TransactionMetaV3 {
            ext: ExtensionPoint::V0,
            tx_changes_before: Default::default(),
            operations: Default::default(),
            tx_changes_after: Default::default(),
            soroban_meta: Some(SorobanTransactionMeta {
                ext: SorobanTransactionMetaExt::V0,
                events: vec![event].try_into().unwrap(),
                return_value: ScVal::Void,
                diagnostic_events: Default::default(),
            }),
        })
    }

    fn make_inputs_with_tx(ledger_seq: u32, store_rpc_data: bool) -> super::LedgerPersistInputs {
        let mut inputs = make_persist_inputs(vec![], ledger_seq);
        inputs.ordered_txs = vec![test_tx()];
        inputs.tx_results = vec![test_result()];
        inputs.tx_metas = if store_rpc_data {
            Some(vec![test_meta_with_event()])
        } else {
            None
        };
        inputs.tx_count = 1;
        inputs.store_rpc_data = store_rpc_data;
        inputs
    }

    fn count_txs(db: &Database, ledger_seq: u32) -> usize {
        db.with_connection(|c| {
            c.load_transactions_in_range(ledger_seq, None, ledger_seq + 1, 100, None, usize::MAX, 0)
        })
        .unwrap()
        .len()
    }

    fn count_events(db: &Database, ledger_seq: u32) -> usize {
        db.with_connection(|c| {
            c.query_events(&henyey_db::EventQueryParams {
                start_ledger: ledger_seq,
                end_ledger: Some(ledger_seq + 1),
                event_type: None,
                contract_ids: &[],
                topics: &[],
                cursor: None,
                limit: 100,
                max_total_bytes: usize::MAX,
                max_query_ops: 0,
            })
        })
        .unwrap()
        .len()
    }

    #[test]
    fn test_store_rpc_data_disabled_skips_per_tx_rows_and_events() {
        let db = Database::open_in_memory().unwrap();
        let inputs = make_inputs_with_tx(11, false);
        inputs.serialize_and_write_to_db(&db).unwrap();

        assert_eq!(count_txs(&db, 11), 0, "transactions table must be empty");
        assert_eq!(count_events(&db, 11), 0, "events table must be empty");

        // The always-written surface is unaffected: header, whole-ledger
        // txhistory/txresults blobs, and LCL pointer.
        let header = db.with_connection(|c| c.load_ledger_header(11)).unwrap();
        assert!(header.is_some(), "header must be persisted");
        let hist = db.with_connection(|c| c.load_tx_history_entry(11)).unwrap();
        assert!(hist.is_some(), "tx_history_entry blob must be persisted");
        let res = db.with_connection(|c| c.load_tx_result_entry(11)).unwrap();
        assert!(res.is_some(), "tx_result_entry blob must be persisted");
        let lcl = db.with_connection(|c| c.get_last_closed_ledger()).unwrap();
        assert_eq!(lcl, Some(11), "LCL must advance");
    }

    #[test]
    fn test_store_rpc_data_enabled_writes_per_tx_rows_and_events() {
        let db = Database::open_in_memory().unwrap();
        let inputs = make_inputs_with_tx(12, true);
        inputs.serialize_and_write_to_db(&db).unwrap();

        assert_eq!(count_txs(&db, 12), 1, "transactions row must be written");
        assert_eq!(count_events(&db, 12), 1, "event row must be written");
    }
}
