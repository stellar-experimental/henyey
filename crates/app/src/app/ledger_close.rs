//! Ledger close pipeline: meta assembly, slot externalization, and buffered ledger application.

use super::*;

/// Record the last phase duration from a `PhaseTimer` into a Prometheus histogram.
/// Must be called immediately after `timer.mark()`.
fn record_phase_histogram(metric: &'static str, timer: &tracked_lock::PhaseTimer) {
    if let Some(&(_, duration)) = timer.phases().last() {
        metrics::histogram!(metric).record(duration.as_secs_f64());
    }
}

/// Raw inputs for a ledger-close persist job, captured on the event loop.
///
/// Phase A of #1733: the event loop captures only cheap clones (no XDR/JSON
/// serialization). The persist task performs all serialization on a
/// blocking thread via [`LedgerPersistInputs::serialize_and_write_to_db`].
struct LedgerPersistInputs {
    header: stellar_xdr::curr::LedgerHeader,
    tx_history_entry: TransactionHistoryEntry,
    tx_result_entry: TransactionHistoryResultEntry,
    ordered_txs: Vec<std::sync::Arc<TransactionEnvelope>>,
    tx_results: Vec<TransactionResultPair>,
    tx_metas: Option<Vec<TransactionMeta>>,
    tx_count: usize,
    network_id: NetworkId,
    scp_envelopes: Vec<stellar_xdr::curr::ScpEnvelope>,
    scp_quorum_sets: Vec<(Hash256, stellar_xdr::curr::ScpQuorumSet)>,
    /// HAS struct built on the event loop under bucket-list read guards.
    /// JSON serialization happens later on the blocking thread.
    has: HistoryArchiveState,
    bucket_list_levels: Option<Vec<(Hash256, Hash256)>>,
    is_validator: bool,
}

impl LedgerPersistInputs {
    /// Serialize XDR/JSON and write to SQLite on a blocking thread.
    fn serialize_and_write_to_db(&self, db: &henyey_db::Database) -> anyhow::Result<()> {
        use henyey_db::queries::*;

        let header_xdr = self
            .header
            .to_xdr(stellar_xdr::curr::Limits::none())
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

        db.transaction(|conn| {
            conn.store_ledger_header(&self.header, &header_xdr)?;
            conn.store_tx_history_entry(self.header.ledger_seq, &self.tx_history_entry)?;
            conn.store_tx_result_entry(self.header.ledger_seq, &self.tx_result_entry)?;
            if is_checkpoint_ledger(self.header.ledger_seq) {
                if let Some(ref levels) = self.bucket_list_levels {
                    conn.store_bucket_list(self.header.ledger_seq, levels)?;
                }
                if self.is_validator {
                    conn.enqueue_publish(self.header.ledger_seq, &has_json)?;
                }
            }
            for index in 0..self.tx_count {
                let tx = &self.ordered_txs[index];
                let tx_result = &self.tx_results[index];
                let tx_meta = self.tx_metas.as_ref().and_then(|metas| metas.get(index));

                let frame = TransactionFrame::with_network(tx.clone(), self.network_id);
                let tx_hash = frame
                    .hash(&self.network_id)
                    .map_err(|e| henyey_db::DbError::Integrity(e.to_string()))?;
                let tx_id = tx_hash.to_hex();

                let tx_body = tx.to_xdr(stellar_xdr::curr::Limits::none())?;
                let tx_result_xdr = tx_result.to_xdr(stellar_xdr::curr::Limits::none())?;
                let tx_meta_xdr = match tx_meta {
                    Some(meta) => Some(meta.to_xdr(stellar_xdr::curr::Limits::none())?),
                    None => None,
                };

                let status = {
                    use stellar_xdr::curr::TransactionResultCode;
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

            conn.store_scp_history(self.header.ledger_seq, &self.scp_envelopes)?;
            for (hash, qset) in &self.scp_quorum_sets {
                conn.store_scp_quorum_set(hash, self.header.ledger_seq, qset)?;
            }

            conn.set_state(state_keys::HISTORY_ARCHIVE_STATE, &has_json)?;
            conn.set_last_closed_ledger(self.header.ledger_seq)?;

            Ok(())
        })?;
        Ok(())
    }
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
            (skip_to + 1, skip_to)
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
        header: &stellar_xdr::curr::LedgerHeader,
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

        let scp_envelopes = self.herder.get_scp_envelopes(header.ledger_seq as u64);
        let mut scp_quorum_sets = Vec::new();
        for envelope in &scp_envelopes {
            let hash = henyey_common::scp_quorum_set_hash(&envelope.statement);
            let hash256 = Hash256::from_bytes(hash.0);
            if let Some(qset) = self.herder.get_quorum_set_by_hash(&hash256) {
                scp_quorum_sets.push((hash256, qset));
            } else {
                tracing::debug!(hash = %hash256.to_hex(), "Missing quorum set for SCP history — export will skip this envelope");
            }
        }

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

        Ok(LedgerPersistInputs {
            header: header.clone(),
            tx_history_entry,
            tx_result_entry,
            ordered_txs,
            tx_results: tx_results.to_vec(),
            tx_metas: tx_metas.map(|m| m.to_vec()),
            tx_count,
            network_id,
            scp_envelopes,
            scp_quorum_sets,
            has,
            bucket_list_levels,
            is_validator: self.is_validator,
        })
    }

    /// Extract contract events from transaction metadata for indexing.
    fn extract_contract_events(
        ledger_seq: u32,
        ordered_txs: &[std::sync::Arc<stellar_xdr::curr::TransactionEnvelope>],
        tx_results: &[TransactionResultPair],
        tx_metas: &[TransactionMeta],
        network_id: NetworkId,
    ) -> anyhow::Result<Vec<henyey_db::EventRecord>> {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
        use stellar_xdr::curr::{ContractEvent, Limits, TransactionResultCode};

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
                    stellar_xdr::curr::ContractEventBody::V0(body) => body
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
    /// Returns `true` if state was successfully restored, `false` if no persisted
    /// state is available (fresh node or corrupt state).
    pub async fn load_last_known_ledger(&self) -> anyhow::Result<bool> {
        // Step 1: Read LCL sequence from DB
        let lcl_seq = self
            .db
            .with_connection(|conn| conn.get_last_closed_ledger())?;
        let Some(lcl_seq) = lcl_seq else {
            tracing::debug!("No last closed ledger in DB, cannot restore from disk");
            return Ok(false);
        };
        if lcl_seq == 0 {
            tracing::debug!("LCL is 0, cannot restore from disk");
            return Ok(false);
        }

        // Step 1b: Restore checkpoint state (crash recovery).
        // Mirrors stellar-core's restoreCheckpoint(lcl) called from
        // loadLastKnownLedger: cleans up partial checkpoint files left
        // by a previous crash and finalizes any complete checkpoint at
        // the current boundary.
        self.restore_checkpoint(lcl_seq);

        // Step 2: Read HAS JSON from DB
        let has_json = self
            .db
            .with_connection(|conn| conn.get_state(state_keys::HISTORY_ARCHIVE_STATE))?;
        let Some(has_json) = has_json else {
            tracing::warn!(lcl_seq, "LCL found but no HAS in DB, cannot restore");
            return Ok(false);
        };
        let has = HistoryArchiveState::from_json(&has_json)
            .map_err(|e| anyhow::anyhow!("Failed to parse persisted HAS: {}", e))?;

        // Step 3: Verify consistency between LCL and HAS
        if has.current_ledger != lcl_seq {
            tracing::warn!(
                lcl_seq,
                has_ledger = has.current_ledger,
                "LCL and HAS disagree on current ledger, cannot restore"
            );
            return Ok(false);
        }

        tracing::info!(
            lcl_seq,
            bucket_levels = has.current_buckets.len(),
            "Found persisted state, attempting restore from disk"
        );

        // Step 4: Load ledger header from DB
        let header = self
            .db
            .get_ledger_header(lcl_seq)?
            .ok_or_else(|| anyhow::anyhow!("LCL header missing from DB at seq {}", lcl_seq))?;

        // Compute header hash (we don't store it separately)
        let header_hash = compute_header_hash(&header)
            .map_err(|e| anyhow::anyhow!("Failed to compute header hash: {}", e))?;

        // Step 5: Verify essential bucket files exist on disk.
        // We only require curr/snap hashes — pending merge outputs (next.output)
        // are optional; if missing we'll discard the pending merge state.
        let mut essential_hashes: Vec<Hash256> = has
            .bucket_hash_pairs()
            .iter()
            .flat_map(|(curr, snap)| [*curr, *snap])
            .filter(|h| !h.is_zero())
            .collect();
        // Also include hot archive bucket hashes
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
        let missing = self.bucket_manager.verify_buckets_exist(&essential_hashes);
        if !missing.is_empty() {
            tracing::warn!(
                missing_count = missing.len(),
                first_missing = %missing[0].to_hex(),
                "Missing essential bucket files on disk, cannot restore"
            );
            return Ok(false);
        }

        // Step 5b: Check which pending merge outputs are available.
        // If a next.output hash is missing on disk, downgrade that level's
        // merge state so restore_from_has doesn't try to load it.
        let mut has = has;
        for level in &mut has.current_buckets {
            if level.next.state == 1 {
                // state 1 = FB_HASH_OUTPUT (merge completed, output hash known)
                if let Some(ref output_hex) = level.next.output {
                    if let Ok(hash) = Hash256::from_hex(output_hex) {
                        if !hash.is_zero() && !self.bucket_manager.bucket_exists(&hash) {
                            tracing::info!(
                                output = %hash.to_hex(),
                                "Pending merge output not on disk, discarding merge state"
                            );
                            level.next.state = 0;
                            level.next.output = None;
                        }
                    }
                }
            }
        }

        // Step 6: Reconstruct bucket lists with overlapped cache scan.
        //
        // The scan runs concurrently with merge restart because it only reads
        // level.curr and level.snap (via Arc clones) while restart_merges_from_has
        // only writes level.next. The overall critical path is:
        //   parallel_restore (~56s) → max(scan ~32s, merges ~60s) → ~116s total
        // vs the old sequential path of ~297s.
        let reconstruct_start = std::time::Instant::now();

        let live_hash_pairs = has.bucket_hash_pairs();
        let live_next_states: Vec<HasNextState> = has
            .live_next_states()
            .into_iter()
            .map(|s| HasNextState {
                state: s.state,
                output: s.output,
                input_curr: s.input_curr,
                input_snap: s.input_snap,
            })
            .collect();
        let bucket_dir = self.bucket_manager.bucket_dir().to_path_buf();

        // Step 6a: Parallel restore of live BucketList (all levels loaded concurrently).
        let bucket_manager = self.bucket_manager.clone();
        let load_bucket = |hash: &Hash256| -> henyey_bucket::Result<henyey_bucket::Bucket> {
            let arc = bucket_manager.load_bucket(hash)?;
            Ok(Arc::try_unwrap(arc).unwrap_or_else(|arc| (*arc).clone()))
        };
        let mut bucket_list =
            BucketList::restore_from_has_parallel(&live_hash_pairs, &live_next_states, load_bucket)
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
            let bucket_dir_for_merge = bucket_dir.clone();
            let load_bucket_for_merge =
                |hash: &Hash256| -> henyey_bucket::Result<henyey_bucket::Bucket> {
                    if hash.is_zero() {
                        return Ok(henyey_bucket::Bucket::empty());
                    }
                    let bucket_path =
                        bucket_dir_for_merge.join(henyey_bucket::canonical_bucket_filename(hash));
                    if bucket_path.exists() {
                        henyey_bucket::Bucket::from_xdr_file_disk_backed(&bucket_path)
                    } else {
                        Err(henyey_bucket::BucketError::NotFound(format!(
                            "bucket {} not found on disk",
                            hash.to_hex()
                        )))
                    }
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
        let hot_archive = if let Some(hot_hash_pairs) = has.hot_archive_bucket_hash_pairs() {
            let hot_next_states: Vec<HasNextState> = has
                .hot_archive_next_states()
                .unwrap_or_default()
                .into_iter()
                .map(|s| HasNextState {
                    state: s.state,
                    output: s.output,
                    input_curr: s.input_curr,
                    input_snap: s.input_snap,
                })
                .collect();

            let bucket_manager = self.bucket_manager.clone();
            let load_hot =
                |hash: &Hash256| -> henyey_bucket::Result<henyey_bucket::HotArchiveBucket> {
                    bucket_manager.load_hot_archive_bucket(hash)
                };
            let mut hot_bl = HotArchiveBucketList::restore_from_has_parallel(
                &hot_hash_pairs,
                &hot_next_states,
                load_hot,
            )
            .map_err(|e| anyhow::anyhow!("Failed to restore hot archive: {}", e))?;

            let bucket_manager = self.bucket_manager.clone();
            let load_hot_for_merge = move |hash: &Hash256| -> henyey_bucket::Result<
                henyey_bucket::HotArchiveBucket,
            > { bucket_manager.load_hot_archive_bucket(hash) };
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

        Ok(true)
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
            stellar_xdr::curr::LedgerHeaderExt::V0 => 0,
            stellar_xdr::curr::LedgerHeaderExt::V1(ext) => ext.flags,
        };

        self.herder.tx_queue().update_validation_context(
            header.ledger_seq,
            header.scp_value.close_time.0,
            header.ledger_version,
            header.base_fee,
            header.base_reserve,
            ledger_flags,
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
                .set_max_contract_size(soroban_info.max_contract_size);
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
        // Guard: only run if publishing is enabled (at least one writable archive).
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

        // Phase 1b: Remove stale publish queue entries above LCL.
        //
        // This mirrors stellar-core's `restoreCheckpoint()` which iterates
        // `.checkpoint.dirty` files and removes entries above LCL. Since
        // henyey uses a SQLite-backed publish queue, this is a simple DELETE.
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
        header: &stellar_xdr::curr::LedgerHeader,
        lcl_seq: u32,
    ) -> anyhow::Result<(BucketList, HotArchiveBucketList)> {
        // Reconstruct live BucketList
        let live_hash_pairs = has.bucket_hash_pairs();
        let live_next_states: Vec<HasNextState> = has
            .live_next_states()
            .into_iter()
            .map(|s| HasNextState {
                state: s.state,
                output: s.output,
                input_curr: s.input_curr,
                input_snap: s.input_snap,
            })
            .collect();

        let bucket_manager = self.bucket_manager.clone();
        let load_bucket = |hash: &Hash256| -> henyey_bucket::Result<henyey_bucket::Bucket> {
            let arc = bucket_manager.load_bucket(hash)?;
            Ok(std::sync::Arc::try_unwrap(arc).unwrap_or_else(|arc| (*arc).clone()))
        };

        let mut bucket_list =
            BucketList::restore_from_has_parallel(&live_hash_pairs, &live_next_states, load_bucket)
                .map_err(|e| anyhow::anyhow!("Failed to restore live bucket list: {}", e))?;

        let bucket_dir = self.bucket_manager.bucket_dir().to_path_buf();
        bucket_list.set_bucket_dir(bucket_dir.clone());
        bucket_list.set_ledger_seq(lcl_seq);

        // Restart pending merges from HAS state.
        // This matches stellar-core loadLastKnownLedgerInternal() which calls
        // AssumeStateWork -> assumeState() -> restartMerges().
        {
            let protocol_version = header.ledger_version;
            let load_bucket_for_merge =
                |hash: &Hash256| -> henyey_bucket::Result<henyey_bucket::Bucket> {
                    if hash.is_zero() {
                        return Ok(henyey_bucket::Bucket::empty());
                    }
                    let bucket_path =
                        bucket_dir.join(henyey_bucket::canonical_bucket_filename(hash));
                    if bucket_path.exists() {
                        henyey_bucket::Bucket::from_xdr_file_disk_backed(&bucket_path)
                    } else {
                        Err(henyey_bucket::BucketError::NotFound(format!(
                            "bucket {} not found on disk",
                            hash.to_hex()
                        )))
                    }
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
        let hot_archive = if let Some(hot_hash_pairs) = has.hot_archive_bucket_hash_pairs() {
            let hot_next_states: Vec<HasNextState> = has
                .hot_archive_next_states()
                .unwrap_or_default()
                .into_iter()
                .map(|s| HasNextState {
                    state: s.state,
                    output: s.output,
                    input_curr: s.input_curr,
                    input_snap: s.input_snap,
                })
                .collect();

            let bucket_manager = self.bucket_manager.clone();
            let load_hot =
                |hash: &Hash256| -> henyey_bucket::Result<henyey_bucket::HotArchiveBucket> {
                    bucket_manager.load_hot_archive_bucket(hash)
                };

            let mut hot_bl = HotArchiveBucketList::restore_from_has_parallel(
                &hot_hash_pairs,
                &hot_next_states,
                load_hot,
            )
            .map_err(|e| anyhow::anyhow!("Failed to restore hot archive: {}", e))?;

            {
                let protocol_version = header.ledger_version;
                let bucket_manager = self.bucket_manager.clone();
                let load_hot_for_merge = move |hash: &Hash256| -> henyey_bucket::Result<
                    henyey_bucket::HotArchiveBucket,
                > {
                    bucket_manager.load_hot_archive_bucket(hash)
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
        // Read persisted HAS from DB
        let has_json = self
            .db
            .with_connection(|conn| conn.get_state(state_keys::HISTORY_ARCHIVE_STATE))?;
        let has_json = has_json.ok_or_else(|| anyhow::anyhow!("No persisted HAS in database"))?;
        let has = HistoryArchiveState::from_json(&has_json)
            .map_err(|e| anyhow::anyhow!("Failed to parse persisted HAS: {}", e))?;

        let lcl_seq = has.current_ledger;

        let header = self
            .db
            .get_ledger_header(lcl_seq)?
            .ok_or_else(|| anyhow::anyhow!("LCL header missing from DB at seq {}", lcl_seq))?;

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

        self.update_buffered_tx_set(slot as u32, close_info.tx_set)
            .await;
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
                    // Mirror legacy `missing_tx_set` semantics on the
                    // post-apply state:
                    //  - Occupied (had existing entry): missing iff
                    //    existing had no tx_set AND info also has no
                    //    tx_set (i.e. no upgrade available).
                    //  - Vacant (no existing): missing iff info has no
                    //    tx_set.
                    let post_update_missing = match existing_entry {
                        Some((_hash, existing_had_tx_set)) => {
                            !(existing_had_tx_set || info.tx_set.is_some())
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
                    // Legacy code still advanced `advance_to` if contiguous
                    // (old ledger_close.rs:1211).
                    if slot == advance_to + 1 {
                        advance_to = slot;
                    }
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
                    match buffer.entry(plan.slot) {
                        std::collections::btree_map::Entry::Occupied(mut entry) => {
                            let existing_local = entry.get_mut();
                            if existing_local.tx_set.is_none() && plan.info.tx_set.is_some() {
                                existing_local.tx_set = plan.info.tx_set;
                                tracing::info!(
                                    slot = plan.slot,
                                    "Updated buffered ledger with tx_set from check_ledger_close"
                                );
                            }
                            // missing_tx_set already recorded from
                            // pre-read state during iteration.
                        }
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            entry.insert(plan.info);
                        }
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
            tracing::info!(
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

    pub(super) fn first_ledger_in_checkpoint(ledger: u32) -> u32 {
        let freq = checkpoint_frequency();
        (ledger / freq) * freq
    }

    pub(super) fn is_first_ledger_in_checkpoint(ledger: u32) -> bool {
        ledger % checkpoint_frequency() == 0
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
            let trim_before = if Self::is_first_ledger_in_checkpoint(last_buffered) {
                if last_buffered == 0 {
                    return;
                }
                let prev = last_buffered - 1;
                Self::first_ledger_in_checkpoint(prev)
            } else {
                Self::first_ledger_in_checkpoint(last_buffered)
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

    async fn update_buffered_tx_set(
        &self,
        slot: u32,
        tx_set: Option<henyey_herder::TransactionSet>,
    ) {
        let Some(tx_set) = tx_set else {
            return;
        };
        let mut buffer =
            tracked_lock::tracked_write("syncing_ledgers", &self.syncing_ledgers).await;
        if let Some(entry) = buffer.get_mut(&slot) {
            if *tx_set.hash() != entry.tx_set_hash {
                tracing::warn!(
                    slot,
                    expected = %entry.tx_set_hash.to_hex(),
                    found = %tx_set.hash().to_hex(),
                    "Buffered tx set hash mismatch (dropping)"
                );
                return;
            }
            entry.tx_set = Some(tx_set);
            tracing::debug!(slot, "Buffered tx set attached");
        } else {
            tracing::debug!(slot, "Received tx set for unbuffered slot");
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
        let Some(info) = self.herder.check_ledger_close(slot) else {
            return false;
        };
        {
            let mut buffer =
                tracked_lock::tracked_write("syncing_ledgers", &self.syncing_ledgers).await;
            buffer.entry(info.slot as u32).or_insert(info);
        }
        self.update_buffered_tx_set(slot as u32, Some(tx_set.clone()))
            .await;
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
            // Trigger consensus immediately after close, matching stellar-core.
            if self.is_validator {
                self.try_trigger_consensus().await;
            }

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

        let close_info = {
            let mut buffer =
                tracked_lock::tracked_write("syncing_ledgers", &self.syncing_ledgers).await;
            Self::trim_syncing_ledgers(&mut buffer, current_ledger);
            match buffer.get(&next_seq) {
                Some(info) if info.tx_set.is_some() => info.clone(),
                Some(info) => {
                    tracing::debug!(
                        next_seq,
                        tx_set_hash = %info.tx_set_hash,
                        "Buffered but waiting for tx_set"
                    );
                    return None;
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
        let our_header_hash = self.ledger_manager.current_header_hash();
        if our_header_hash != tx_set.previous_ledger_hash() {
            tracing::error!(
                ledger_seq = next_seq,
                our_header_hash = %our_header_hash.to_hex(),
                network_prev_hash = %tx_set.previous_ledger_hash().to_hex(),
                "FATAL: pre-close hash mismatch — our header hash does not match \
                 the network's previous ledger hash. This means our ledger state \
                 has diverged from the network. Shutting down."
            );
            std::process::exit(1);
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
        let join_handle = tokio::task::spawn_blocking(move || {
            lm.close_ledger(close_data, Some(runtime_handle))
                .map_err(|e| e.to_string())
        });

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
        join_result: Result<std::result::Result<LedgerCloseResult, String>, tokio::task::JoinError>,
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
        metrics::histogram!(crate::metrics::CLOSE_HANDLE_COMPLETE_SECONDS)
            .record(elapsed.as_secs_f64());
        result
    }

    async fn handle_close_complete_inner(
        &self,
        pending: PendingLedgerClose,
        join_result: Result<std::result::Result<LedgerCloseResult, String>, tokio::task::JoinError>,
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
                let is_hash_mismatch = e.contains("hash mismatch");
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
                    tracing::warn!(
                        ledger_seq = pending.ledger_seq,
                        cleared_count,
                        "Hash mismatch detected - cleared all buffered ledgers, will trigger catchup"
                    );
                }
                return false;
            }
            Err(e) => {
                tracing::error!(
                    ledger_seq = pending.ledger_seq,
                    error = %e,
                    "Ledger close task panicked"
                );
                return false;
            }
        };
        timer.mark("join_match_ms");
        record_phase_histogram(crate::metrics::CLOSE_COMPLETE_JOIN_MATCH_SECONDS, &timer);

        // Store last-close stats and perf for metrics reporting.
        *self.last_close_stats.write() = result.stats.clone();
        *self.last_close_perf.write() = result.perf.clone();

        // Record ledger close duration into the histogram (accumulates across closes).
        if result.stats.close_time_ms > 0 {
            metrics::histogram!(crate::metrics::LEDGER_CLOSE_DURATION_SECONDS)
                .record(result.stats.close_time_ms as f64 / 1000.0);
        }

        // Phase 5: Per-phase close-duration histograms (LedgerClosePerf).
        if let Some(ref perf) = result.perf {
            let us_to_secs = |us: u64| us as f64 / 1_000_000.0;
            metrics::histogram!(crate::metrics::CLOSE_BEGIN_SECONDS)
                .record(us_to_secs(perf.begin_close_us));
            metrics::histogram!(crate::metrics::CLOSE_TX_EXEC_SECONDS)
                .record(us_to_secs(perf.tx_exec_us));
            metrics::histogram!(crate::metrics::CLOSE_CLASSIC_EXEC_SECONDS)
                .record(us_to_secs(perf.classic_exec_us));
            metrics::histogram!(crate::metrics::CLOSE_SOROBAN_EXEC_SECONDS)
                .record(us_to_secs(perf.soroban_exec_us));
            metrics::histogram!(crate::metrics::CLOSE_COMMIT_SETUP_SECONDS)
                .record(us_to_secs(perf.commit_setup_us));
            metrics::histogram!(crate::metrics::CLOSE_BUCKET_LOCK_WAIT_SECONDS)
                .record(us_to_secs(perf.bucket_lock_wait_us));
            metrics::histogram!(crate::metrics::CLOSE_EVICTION_SECONDS)
                .record(us_to_secs(perf.eviction_us));
            metrics::histogram!(crate::metrics::CLOSE_SOROBAN_STATE_SECONDS)
                .record(us_to_secs(perf.soroban_state_us));
            metrics::histogram!(crate::metrics::CLOSE_BUCKET_ADD_SECONDS)
                .record(us_to_secs(perf.add_batch_us));
            metrics::histogram!(crate::metrics::CLOSE_HOT_ARCHIVE_SECONDS)
                .record(us_to_secs(perf.hot_archive_us));
            metrics::histogram!(crate::metrics::CLOSE_HEADER_SECONDS)
                .record(us_to_secs(perf.header_us));
            metrics::histogram!(crate::metrics::CLOSE_COMMIT_SECONDS)
                .record(us_to_secs(perf.commit_close_us));
            metrics::histogram!(crate::metrics::CLOSE_META_SECONDS)
                .record(us_to_secs(perf.meta_us));
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
        let meta_xdr = result
            .meta
            .as_ref()
            .and_then(|meta| meta.to_xdr(stellar_xdr::curr::Limits::none()).ok());

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

            use stellar_xdr::curr::TransactionResultResult;
            let is_success = matches!(
                tx_result.result.result,
                TransactionResultResult::TxSuccess(_)
                    | TransactionResultResult::TxFeeBumpInnerSuccess(_)
            );
            if !is_success {
                failed_hashes.push(Hash256::hash_xdr(tx));
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

        // Notify peers if max tx size increased due to a protocol upgrade.
        // Mirrors upstream HerderImpl::maybeHandleUpgrade().
        {
            let soroban_tx_max = soroban_info.as_ref().map(|info| info.tx_max_size_bytes);
            let new_max = compute_max_tx_size(result.header.ledger_version, soroban_tx_max);
            let old_max = self.max_tx_size_bytes.load(Ordering::Relaxed);
            let diff = new_max.saturating_sub(old_max);
            self.max_tx_size_bytes.store(new_max, Ordering::Relaxed);
            if diff > 0 {
                self.set_phase_sub(phase::PHASE_6_4_OVERLAY_MAX_TX_SIZE);
                if let Some(overlay) = self.overlay().await {
                    overlay.handle_max_tx_size_increase(diff).await;
                }
            }
        }

        // Clean up old survey rate limiter entries.
        // Mirrors upstream SurveyManager::clearOldLedgers() called from clearLedgersBelow().
        {
            self.set_phase_sub(phase::PHASE_6_5_SURVEY_LIMITER_WRITE);
            let mut limiter = self.survey_limiter.write().await;
            limiter.clear_old_ledgers(pending.ledger_seq);
        }

        // Record externalized close time for drift tracking (std::Mutex, < 1 µs).
        if let Ok(mut tracker) = self.drift_tracker.lock() {
            if let Some(warning) =
                tracker.record_externalized_close_time(pending.ledger_seq, pending.close_time)
            {
                tracing::warn!("{}", warning);
            }
        }
        // Mark end of the inline overlay/survey/drift bookkeeping window.
        // This brackets lines that touch tokio RwLocks (overlay, survey_limiter)
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
                stellar_xdr::curr::LedgerHeaderExt::V0 => 0,
                stellar_xdr::curr::LedgerHeaderExt::V1(ext) => ext.flags,
            };
            let close_time_ctx = result_header.scp_value.close_time.0;
            let base_fee = result_header.base_fee;
            let base_reserve = result_header.base_reserve;
            let protocol_version = result_header.ledger_version;

            let max_contract_size_bytes = soroban_info.as_ref().map(|info| info.max_contract_size);
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

            // Compute upper-bound close-time offset (stellar-core parity).
            // Reading `wall_now` inside the closure instead of on the event
            // loop differs by at most a few hundred µs of scheduler
            // latency; `upper_bound_offset` is denominated in whole
            // seconds and feeds no hash, so the semantics are preserved.
            const EXPECTED_CLOSE_TIME_MULT: u64 = 2;
            let expected_close_secs = herder.ledger_close_time() as u64;
            let wall_now = clock
                .system_now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock before UNIX epoch")
                .as_secs();
            let close_time_drift = wall_now.saturating_sub(close_time_ctx);
            let upper_bound_offset =
                expected_close_secs * EXPECTED_CLOSE_TIME_MULT + close_time_drift;

            metrics::histogram!(crate::metrics::CLOSE_TX_QUEUE_PREP_SECONDS)
                .record(prep_start.elapsed().as_secs_f64());

            // === Sub-phase 1: herder.ledger_closed + ban failed txs ==========
            let phase1_start = std::time::Instant::now();
            herder.ledger_closed(
                ledger_seq as u64,
                &applied_txs,
                &applied_upgrades,
                close_time,
            );
            if !failed_hashes_for_ban.is_empty() {
                tracing::debug!(
                    failed_count = failed_hashes_for_ban.len(),
                    "Banning failed transactions"
                );
                herder.tx_queue().ban(&failed_hashes_for_ban);
            }
            metrics::histogram!(crate::metrics::CLOSE_TX_QUEUE_LEDGER_CLOSED_SECONDS)
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
            );
            if let Some(q) = queue_limit {
                herder.tx_queue().update_soroban_resource_limits(q);
            }
            if let Some(s) = selection_limit {
                herder.tx_queue().update_soroban_selection_limits(s);
            }

            let shift_result = herder.tx_queue().shift();
            metrics::histogram!(crate::metrics::CLOSE_TX_QUEUE_SHIFT_UPDATE_SECONDS)
                .record(phase2_start.elapsed().as_secs_f64());

            // === Sub-phase 3: envelopes fetch + snapshot build + Sub-phase 4: invalidation =====
            let phase3_start = std::time::Instant::now();
            let pending_envs = herder.tx_queue().pending_hashed_envelopes();
            let fetch_elapsed = phase3_start.elapsed();
            metrics::histogram!(crate::metrics::CLOSE_TX_QUEUE_ENVELOPES_FETCH_SECONDS)
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
                    max_contract_size_bytes,
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
                metrics::histogram!(crate::metrics::CLOSE_TX_QUEUE_SNAPSHOT_BUILD_SECONDS)
                    .record(snapshot_build_start.elapsed().as_secs_f64());
                metrics::histogram!(crate::metrics::CLOSE_TX_QUEUE_SNAPSHOT_SECONDS)
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
                    herder.tx_queue().ban(&invalid_hashes);
                }
                metrics::histogram!(crate::metrics::CLOSE_TX_QUEUE_INVALIDATION_SECONDS)
                    .record(phase4_start.elapsed().as_secs_f64());
            } else {
                // Queue empty: record envelopes_fetch (already recorded above)
                // and total. Do NOT record snapshot_build — it wasn't executed.
                metrics::histogram!(crate::metrics::CLOSE_TX_QUEUE_SNAPSHOT_SECONDS)
                    .record(phase3_start.elapsed().as_secs_f64());
                metrics::histogram!(crate::metrics::CLOSE_TX_QUEUE_INVALIDATION_SECONDS)
                    .record(0.0);
            }

            (shift_result, invalid_banned)
        });

        self.set_phase_sub(phase::PHASE_6_6_TX_QUEUE_JOIN);
        match henyey_herder::await_blocking_logged("tx-queue close-update", join).await {
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
        timer.mark("tx_queue_background_wait_ms");
        record_phase_histogram(crate::metrics::CLOSE_COMPLETE_TX_QUEUE_SECONDS, &timer);

        // Update current ledger tracking.
        self.set_phase_sub(phase::PHASE_6_7_LAST_PROCESSED_SLOT_WRITE);
        *self.last_processed_slot.write().await = pending.ledger_seq as u64;
        self.set_phase_sub(phase::PHASE_6_8_CLEAR_TX_ADVERT_HISTORY);
        self.clear_tx_advert_history(pending.ledger_seq).await;

        // Re-bootstrap the herder so tracking_slot advances past the
        // just-closed ledger.  For validators, SCP's externalize path
        // handles this, but watchers close ledgers from buffered
        // EXTERNALIZE messages without going through SCP — the
        // tracking_slot stalls unless we update it here.
        self.herder.bootstrap(pending.ledger_seq);

        // Update bucket snapshots for the query server.
        self.update_bucket_snapshot();

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

        // Periodically garbage collect stale bucket files.
        if pending.ledger_seq % 100 == 0 {
            self.cleanup_stale_bucket_files_background();
        }

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
            metrics::histogram!(crate::metrics::SLOT_TO_CLOSE_LATENCY_SECONDS)
                .record(elapsed.as_secs_f64());
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

    use stellar_xdr::curr::{
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
        use stellar_xdr::curr::{OperationMetaV2, TransactionMetaV4};

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
