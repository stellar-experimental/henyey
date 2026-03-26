use super::*;

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

    fn persist_ledger_close(
        &self,
        header: &stellar_xdr::curr::LedgerHeader,
        tx_set_variant: &TransactionSetVariant,
        tx_results: &[TransactionResultPair],
        tx_metas: Option<&[TransactionMeta]>,
    ) -> anyhow::Result<()> {
        let header_xdr = header.to_xdr(stellar_xdr::curr::Limits::none())?;
        let network_id = NetworkId::from_passphrase(&self.config.network.passphrase);
        let ordered_txs: Vec<std::sync::Arc<TransactionEnvelope>> = tx_set_variant
            .transactions_with_base_fee()
            .into_iter()
            .map(|(tx, _)| tx)
            .collect();
        let tx_count = ordered_txs.len().min(tx_results.len());
        let meta_count = tx_metas.map(|metas| metas.len()).unwrap_or(0);
        let scp_envelopes = self.herder.get_scp_envelopes(header.ledger_seq as u64);
        let mut scp_quorum_sets = Vec::new();
        for envelope in &scp_envelopes {
            if let Some(hash) = Self::scp_quorum_set_hash(&envelope.statement) {
                let hash256 = Hash256::from_bytes(hash.0);
                if let Some(qset) = self.herder.get_quorum_set_by_hash(&hash256) {
                    scp_quorum_sets.push((hash256, qset));
                } else {
                    tracing::debug!(hash = %hash256.to_hex(), "Missing quorum set for SCP history");
                }
            }
        }

        if tx_results.len() != ordered_txs.len() {
            tracing::warn!(
                tx_count = ordered_txs.len(),
                result_count = tx_results.len(),
                "Transaction count mismatch while persisting history"
            );
        }
        if tx_metas.is_some() && meta_count < tx_count {
            tracing::warn!(
                tx_count,
                meta_count,
                "Transaction meta count mismatch while persisting history"
            );
        }

        let tx_set_entry = match tx_set_variant {
            TransactionSetVariant::Classic(set) => set.clone(),
            TransactionSetVariant::Generalized(set) => {
                let stellar_xdr::curr::GeneralizedTransactionSet::V1(set_v1) = set;
                TransactionSet {
                    previous_ledger_hash: set_v1.previous_ledger_hash.clone(),
                    txs: VecM::default(),
                }
            }
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
            results: tx_results.to_vec().try_into().unwrap_or_default(),
        };
        let tx_result_entry = TransactionHistoryResultEntry {
            ledger_seq: header.ledger_seq,
            tx_result_set,
            ext: TransactionHistoryResultEntryExt::default(),
        };

        // Build HAS from current bucket list state for restart recovery.
        // This captures pending merge outputs so a restarted node can
        // reconstruct the bucket list without re-downloading from archives.
        let has_json = {
            let bucket_list = self.ledger_manager.bucket_list();
            let hot_archive_guard = self.ledger_manager.hot_archive_bucket_list();
            let hot_archive_ref = hot_archive_guard.as_ref();

            // Ensure hot archive buckets are persisted to disk for restart recovery.
            // Hot archive merges are all in-memory, so after each close the curr/snap
            // buckets may have no backing file.
            if let Some(habl) = hot_archive_ref {
                self.persist_hot_archive_buckets(habl);
            }

            // Only include hot archive in HAS when protocol >= 23, matching
            // stellar-core. At earlier protocols the header's bucket_list_hash
            // is live-only, so including hot archive would cause Horizon to
            // compute a combined hash that doesn't match the header.
            let hot_archive_for_has = if header.ledger_version >= 23 {
                hot_archive_ref
            } else {
                None
            };

            let has = build_history_archive_state(
                header.ledger_seq,
                &bucket_list,
                hot_archive_for_has,
                Some(self.config.network.passphrase.clone()),
            )
            .map_err(|e| anyhow::anyhow!("Failed to build HAS: {}", e))?;

            // Diagnostic: verify that the Go SDK hash from this HAS matches
            // the header's bucket_list_hash. If these differ, Horizon will
            // reject the archive.
            let expected_hash = henyey_common::Hash256::from_bytes(header.bucket_list_hash.0);
            match has.compute_bucket_list_hash() {
                Ok(go_sdk_hash) => {
                    if go_sdk_hash != expected_hash {
                        tracing::error!(
                            ledger_seq = header.ledger_seq,
                            has_version = has.version,
                            go_sdk_hash = %go_sdk_hash.to_hex(),
                            header_hash = %expected_hash.to_hex(),
                            has_hot_archive = has.hot_archive_buckets.is_some(),
                            hot_archive_levels = has.hot_archive_buckets.as_ref().map(|v| v.len()).unwrap_or(0),
                            current_bucket_levels = has.current_buckets.len(),
                            "DIAGNOSTIC: HAS bucket_list_hash does NOT match header! \
                             Horizon will reject this archive."
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        ledger_seq = header.ledger_seq,
                        error = %e,
                        "Failed to compute diagnostic Go SDK hash from HAS"
                    );
                }
            }

            has.to_json()
                .map_err(|e| anyhow::anyhow!("Failed to serialize HAS: {}", e))?
        };

        self.db.transaction(|conn| {
            conn.store_ledger_header(header, &header_xdr)?;
            conn.store_tx_history_entry(header.ledger_seq, &tx_history_entry)?;
            conn.store_tx_result_entry(header.ledger_seq, &tx_result_entry)?;
            if is_checkpoint_ledger(header.ledger_seq) {
                let levels = self.ledger_manager.bucket_list_levels();
                conn.store_bucket_list(header.ledger_seq, &levels)?;
                if self.is_validator {
                    conn.enqueue_publish(header.ledger_seq, &has_json)?;
                }
            }
            for index in 0..tx_count {
                let tx = &ordered_txs[index];
                let tx_result = &tx_results[index];
                let tx_meta = tx_metas.and_then(|metas| metas.get(index));

                let frame = TransactionFrame::with_network(tx.clone(), network_id);
                let tx_hash = frame
                    .hash(&network_id)
                    .map_err(|e| henyey_db::DbError::Integrity(e.to_string()))?;
                let tx_id = tx_hash.to_hex();

                let tx_body = tx.to_xdr(stellar_xdr::curr::Limits::none())?;
                let tx_result_xdr = tx_result.to_xdr(stellar_xdr::curr::Limits::none())?;
                let tx_meta_xdr = match tx_meta {
                    Some(meta) => Some(meta.to_xdr(stellar_xdr::curr::Limits::none())?),
                    None => None,
                };

                // Compute status from the transaction result code
                let status = {
                    use stellar_xdr::curr::TransactionResultCode;
                    let code = tx_result.result.result.discriminant();
                    if code == TransactionResultCode::TxSuccess
                        || code == TransactionResultCode::TxFeeBumpInnerSuccess
                    {
                        henyey_db::TX_STATUS_SUCCESS
                    } else {
                        henyey_db::TX_STATUS_FAILED
                    }
                };

                conn.store_transaction(&henyey_db::StoreTxParams {
                    ledger_seq: header.ledger_seq,
                    tx_index: index as u32,
                    tx_id: &tx_id,
                    body: &tx_body,
                    result: &tx_result_xdr,
                    meta: tx_meta_xdr.as_deref(),
                    status,
                })?;
            }

            // Extract and store contract events from transaction metadata
            if let Some(metas) = tx_metas {
                let events = Self::extract_contract_events(
                    header.ledger_seq,
                    &ordered_txs,
                    tx_results,
                    metas,
                    network_id,
                );
                if !events.is_empty() {
                    conn.store_events(&events)?;
                }
            }

            conn.store_scp_history(header.ledger_seq, &scp_envelopes)?;
            for (hash, qset) in &scp_quorum_sets {
                conn.store_scp_quorum_set(hash, header.ledger_seq, qset)?;
            }

            // Persist HAS and LCL for restart recovery
            conn.set_state(state_keys::HISTORY_ARCHIVE_STATE, &has_json)?;
            conn.set_last_closed_ledger(header.ledger_seq)?;

            Ok(())
        })?;

        Ok(())
    }

    /// Extract contract events from transaction metadata for indexing.
    fn extract_contract_events(
        ledger_seq: u32,
        ordered_txs: &[std::sync::Arc<stellar_xdr::curr::TransactionEnvelope>],
        tx_results: &[TransactionResultPair],
        tx_metas: &[TransactionMeta],
        network_id: NetworkId,
    ) -> Vec<henyey_db::EventRecord> {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
        use stellar_xdr::curr::{ContractEvent, ContractEventType, Limits, TransactionResultCode};

        let mut all_events = Vec::new();

        for (tx_index, meta) in tx_metas.iter().enumerate() {
            // Compute tx hash
            let tx = match ordered_txs.get(tx_index) {
                Some(tx) => tx,
                None => continue,
            };
            let frame = TransactionFrame::with_network(tx.clone(), network_id);
            let tx_hash_hex = match frame.hash(&network_id) {
                Ok(h) => h.to_hex(),
                Err(_) => continue,
            };

            // Determine if the tx succeeded
            let tx_succeeded = tx_results
                .get(tx_index)
                .map(|r| {
                    let code = r.result.result.discriminant();
                    code == TransactionResultCode::TxSuccess
                        || code == TransactionResultCode::TxFeeBumpInnerSuccess
                })
                .unwrap_or(false);

            // Extract events from the meta based on version
            let contract_events: Vec<(u32, &ContractEvent)> = match meta {
                TransactionMeta::V3(v3) => {
                    // V3: contract events in soroban_meta.events
                    if let Some(ref soroban) = v3.soroban_meta {
                        soroban.events.iter().map(|e| (0u32, e)).collect()
                    } else {
                        Vec::new()
                    }
                }
                TransactionMeta::V4(v4) => {
                    // V4: contract events are per-operation in operations[i].events
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
                _ => Vec::new(),
            };

            for (event_index, (op_index, event)) in contract_events.iter().enumerate() {
                // Compute TOID-based event ID
                let toid =
                    ((ledger_seq as u64) << 32) | ((tx_index as u64) << 12) | (*op_index as u64);
                let event_id = format!("{:019}-{:010}", toid, event_index);

                // Event type
                let event_type = match event.type_ {
                    ContractEventType::Contract => 0,
                    ContractEventType::System => 1,
                    ContractEventType::Diagnostic => 2,
                };

                // Contract ID
                let contract_id = event
                    .contract_id
                    .as_ref()
                    .map(|h| stellar_strkey::Contract(h.0.clone().into()).to_string());

                // Extract topics
                let topics: Vec<String> = match &event.body {
                    stellar_xdr::curr::ContractEventBody::V0(body) => body
                        .topics
                        .iter()
                        .filter_map(|t| t.to_xdr(Limits::none()).ok().map(|b| BASE64.encode(&b)))
                        .collect(),
                };

                // Encode full event as base64 XDR
                let event_xdr = match event.to_xdr(Limits::none()) {
                    Ok(b) => BASE64.encode(&b),
                    Err(_) => continue,
                };

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

        all_events
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
                        bucket_dir_for_merge.join(format!("{}.bucket.xdr", hash.to_hex()));
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
            .map_err(|e| anyhow::anyhow!("Cache scan thread panicked: {:?}", e))?;

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

        Ok(true)
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
                    let bucket_path = bucket_dir.join(format!("{}.bucket.xdr", hash.to_hex()));
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
        self.try_apply_buffered_ledgers().await;
    }

    /// Process any externalized slots that need ledger close.
    pub(super) async fn process_externalized_slots(&self) {
        // Get the latest externalized slot
        let latest_externalized = match self.herder.latest_externalized_slot() {
            Some(slot) => slot,
            None => {
                tracing::debug!("No externalized slots yet");
                return;
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

            let mut missing_tx_set = false;
            let mut buffered_count = 0usize;
            let mut advance_to;
            let mut skipped_stale = 0u64;
            {
                let current_ledger = *self.current_ledger.read().await;
                let mut buffer = self.syncing_ledgers.write().await;

                // Only iterate slots that peers are likely to still have
                // tx_sets for.  When the gap between last_processed and
                // latest_externalized is large (e.g., after catchup resets
                // last_processed_slot to current_ledger), iterating old
                // slots creates syncing_ledgers entries with tx_set: None
                // that trigger futile fetch requests.  Limit to the most
                // recent TX_SET_REQUEST_WINDOW slots; the gap check below
                // will trigger catchup for larger gaps.
                //
                // Exception: when the first replay ledger falls in an
                // unpublished checkpoint, archive-based catchup would fail
                // (the checkpoint file doesn't exist yet).  In that case,
                // process ALL slots so the node can close ledgers from
                // cached SCP messages + peer-fetched tx_sets instead of
                // waiting for the checkpoint.
                //
                // This is critical after catchup + rapid close: the gap
                // re-opens (e.g., 10-15 slots), the EXTERNALIZE for
                // current_ledger+1 may not be in the cache (peers evicted
                // SCP state for old slots), but we still need to create
                // syncing_ledgers entries and request tx_sets for all gap
                // slots.  When EXTERNALIZE messages arrive from the gap
                // slot handler, they match with already-fetched tx_sets.
                let (iter_start, next_advance_to) = Self::externalized_iteration_window(
                    last_processed,
                    current_ledger,
                    latest_externalized,
                );
                advance_to = next_advance_to;

                for slot in iter_start..=latest_externalized {
                    // Skip slots that have already been closed. Stale
                    // EXTERNALIZE messages (e.g., from SCP state responses)
                    // can set latest_externalized to old slots whose tx_sets
                    // are evicted from peers' caches. Creating syncing_ledgers
                    // entries for these would cause unfulfillable tx_set
                    // requests and infinite recovery loops.
                    if slot <= current_ledger as u64 {
                        skipped_stale += 1;
                        if slot == advance_to + 1 {
                            advance_to = slot;
                        }
                        continue;
                    }

                    if let Some(info) = self.herder.check_ledger_close(slot) {
                        let has_tx_set = info.tx_set.is_some();
                        // Update existing entry's tx_set if it was missing but now available,
                        // or insert new entry if slot wasn't buffered yet.
                        match buffer.entry(info.slot as u32) {
                            std::collections::btree_map::Entry::Occupied(mut entry) => {
                                let existing = entry.get_mut();
                                if existing.tx_set.is_none() && info.tx_set.is_some() {
                                    existing.tx_set = info.tx_set;
                                    tracing::info!(
                                        slot,
                                        "Updated buffered ledger with tx_set from check_ledger_close"
                                    );
                                }
                                if existing.tx_set.is_none() {
                                    missing_tx_set = true;
                                }
                            }
                            std::collections::btree_map::Entry::Vacant(entry) => {
                                if !has_tx_set {
                                    missing_tx_set = true;
                                }
                                entry.insert(info);
                            }
                        }
                        buffered_count += 1;
                        if slot == advance_to + 1 {
                            advance_to = slot;
                        }
                    } else {
                        // check_ledger_close returned None — the externalized
                        // data for this slot was evicted from cache.  If we
                        // already have a syncing_ledgers entry with a tx_set_hash,
                        // re-register the pending tx_set request so
                        // request_pending_tx_sets can try to fetch it.
                        if let Some(existing) = buffer.get(&(slot as u32)) {
                            if existing.tx_set.is_none() {
                                self.herder
                                    .scp_driver()
                                    .request_tx_set(existing.tx_set_hash, slot);
                                missing_tx_set = true;
                            }
                        }
                        if slot == advance_to + 1 {
                            advance_to = slot;
                        }
                    }
                }
            }
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
                let current_ledger = *self.current_ledger.read().await;
                let gap = latest_externalized.saturating_sub(current_ledger as u64);
                if buffered_count == 0 || gap > TX_SET_REQUEST_WINDOW {
                    self.set_phase(11); // 11 = externalized_catchup
                    self.maybe_start_externalized_catchup(latest_externalized)
                        .await;
                }
            }
        } else {
            tracing::debug!(latest_externalized, last_processed, "Already processed");
        }

        // Always try to apply buffered ledgers and check for catchup,
        // even when no new slots - we may need to trigger stuck recovery.
        self.set_phase(12); // 12 = try_apply_buffered
        self.try_apply_buffered_ledgers().await;
        self.set_phase(13); // 13 = maybe_buffered_catchup
        self.maybe_start_buffered_catchup().await;
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
        let mut buffer = self.syncing_ledgers.write().await;
        if let Some(entry) = buffer.get_mut(&slot) {
            if tx_set.hash != entry.tx_set_hash {
                tracing::warn!(
                    slot,
                    expected = %entry.tx_set_hash.to_hex(),
                    found = %tx_set.hash.to_hex(),
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
        let mut buffer = self.syncing_ledgers.write().await;
        for (slot, entry) in buffer.iter_mut() {
            if entry.tx_set.is_none() && entry.tx_set_hash == tx_set.hash {
                entry.tx_set = Some(tx_set.clone());
                tracing::debug!(slot, hash = %tx_set.hash, "Attached tx set to buffered slot");
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
            .find_externalized_slot_by_tx_set_hash(&tx_set.hash)
        else {
            return false;
        };
        let Some(info) = self.herder.check_ledger_close(slot) else {
            return false;
        };
        {
            let mut buffer = self.syncing_ledgers.write().await;
            buffer.entry(info.slot as u32).or_insert(info);
        }
        self.update_buffered_tx_set(slot as u32, Some(tx_set.clone()))
            .await;
        tracing::debug!(
            slot,
            hash = %tx_set.hash,
            "Buffered tx set after externalized lookup"
        );
        true
    }

    /// Apply a single buffered ledger (yields to tokio via `spawn_blocking`).
    ///
    /// Used by callers outside the main select loop (catchup completion, tx set
    /// handlers). If a background close is already in progress (`is_applying_ledger`),
    /// returns immediately — the select loop completion handler will chain the next close.
    ///
    /// Closes at most ONE ledger to avoid blocking the event loop. Subsequent
    /// buffered ledgers are picked up by the main select loop's `pending_close`
    /// chaining or the next `consensus_interval` tick.
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
        let success = self.handle_close_complete(pending, join_result).await;

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
            let mut buffer = self.syncing_ledgers.write().await;
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
        if our_header_hash != tx_set.previous_ledger_hash {
            tracing::error!(
                ledger_seq = next_seq,
                our_header_hash = %our_header_hash.to_hex(),
                network_prev_hash = %tx_set.previous_ledger_hash.to_hex(),
                "FATAL: pre-close hash mismatch — our header hash does not match \
                 the network's previous ledger hash. This means our ledger state \
                 has diverged from the network. Shutting down."
            );
            std::process::exit(1);
        }
        if tx_set.hash != close_info.tx_set_hash {
            tracing::error!(
                ledger_seq = next_seq,
                expected = %close_info.tx_set_hash.to_hex(),
                found = %tx_set.hash.to_hex(),
                "Buffered tx set hash mismatch"
            );
            let mut buffer = self.syncing_ledgers.write().await;
            if let Some(entry) = buffer.get_mut(&next_seq) {
                entry.tx_set = None;
            }
            return None;
        }

        tracing::debug!(
            ledger_seq = next_seq,
            tx_count = tx_set.transactions.len(),
            close_time = close_info.close_time,
            prev_ledger_hash = %tx_set.previous_ledger_hash.to_hex(),
            "Starting background ledger close"
        );

        // Build LedgerCloseData (same as HerderCallback::close_ledger).
        let prev_hash = tx_set.previous_ledger_hash;
        let tx_set_variant = if let Some(gen_tx_set) = tx_set.generalized_tx_set.clone() {
            TransactionSetVariant::Generalized(gen_tx_set)
        } else {
            TransactionSetVariant::Classic(TransactionSet {
                previous_ledger_hash: Hash::from(prev_hash),
                txs: match tx_set.transactions.clone().try_into() {
                    Ok(txs) => txs,
                    Err(_) => {
                        tracing::error!(
                            ledger_seq = next_seq,
                            "Failed to create tx set for background close"
                        );
                        return None;
                    }
                },
            })
        };

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
            let mut buffer = self.syncing_ledgers.write().await;
            buffer.remove(&next_seq);
        }

        // Spawn blocking close.
        let lm = self.ledger_manager.clone();
        let runtime_handle = tokio::runtime::Handle::current();
        self.set_applying_ledger(true);

        let join_handle = tokio::task::spawn_blocking(move || {
            lm.close_ledger(close_data, Some(runtime_handle))
                .map_err(|e| e.to_string())
        });

        Some(PendingLedgerClose {
            handle: join_handle,
            ledger_seq: next_seq,
            tx_set,
            tx_set_variant,
            close_time,
            upgrades: close_info.upgrades.clone(),
        })
    }

    /// Handle completion of a background ledger close.
    ///
    /// Performs all post-close work: meta emission, DB persistence, herder
    /// notification, and state updates. Returns `true` on success.
    pub(super) async fn handle_close_complete(
        &self,
        pending: PendingLedgerClose,
        join_result: Result<std::result::Result<LedgerCloseResult, String>, tokio::task::JoinError>,
    ) -> bool {
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
                    let mut buffer = self.syncing_ledgers.write().await;
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

        // Emit LedgerCloseMeta to stream.
        if let Some(ref meta) = result.meta {
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

        // Persist ledger close data.
        let tx_metas = result.meta.as_ref().map(Self::extract_tx_metas);
        if let Err(err) = self.persist_ledger_close(
            &result.header,
            &pending.tx_set_variant,
            &result.tx_results,
            tx_metas.as_deref(),
        ) {
            tracing::warn!(error = %err, "Failed to persist ledger close data");
        }

        // Persist full LedgerCloseMeta for RPC serving (getTransactions, getLedgers).
        if let Some(ref meta) = result.meta {
            match meta.to_xdr(stellar_xdr::curr::Limits::none()) {
                Ok(meta_xdr) => {
                    if let Err(err) = self
                        .db
                        .store_ledger_close_meta(pending.ledger_seq, &meta_xdr)
                    {
                        tracing::warn!(
                            error = %err,
                            ledger_seq = pending.ledger_seq,
                            "Failed to persist LedgerCloseMeta"
                        );
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        ledger_seq = pending.ledger_seq,
                        "Failed to serialize LedgerCloseMeta"
                    );
                }
            }
        }

        // Separate successful and failed transactions for queue management.
        let mut applied_hashes = Vec::new();
        let mut failed_hashes = Vec::new();
        for (tx, tx_result) in pending
            .tx_set
            .transactions
            .iter()
            .zip(result.tx_results.iter())
        {
            if let Some(hash) = self.tx_hash(tx) {
                use stellar_xdr::curr::TransactionResultResult;
                let is_success = matches!(
                    tx_result.result.result,
                    TransactionResultResult::TxSuccess(_)
                        | TransactionResultResult::TxFeeBumpInnerSuccess(_)
                );
                if is_success {
                    applied_hashes.push(hash);
                } else {
                    failed_hashes.push(hash);
                }
            }
        }

        // Track non-empty ledger closes for the `ledger.transaction.count` metric.
        if !pending.tx_set.transactions.is_empty() {
            self.ledger_tx_count.fetch_add(1, Ordering::Relaxed);
        }

        // Pass both applied and failed hashes so account_states are cleared
        // for all transactions, allowing the account to submit new ones.
        let mut all_hashes = applied_hashes.clone();
        all_hashes.extend_from_slice(&failed_hashes);
        self.herder.ledger_closed(
            pending.ledger_seq as u64,
            &all_hashes,
            &pending.upgrades,
            pending.close_time,
        );

        // Clear per-ledger overlay state (flood gate, etc.) for old ledgers.
        // Mirrors upstream HerderImpl::eraseBelow() -> clearLedgersBelow().
        {
            if let Some(overlay) = self.overlay().await {
                overlay.clear_ledgers_below(pending.ledger_seq, pending.ledger_seq);
            }
        }

        // Notify peers if max tx size increased due to a protocol upgrade.
        // Mirrors upstream HerderImpl::maybeHandleUpgrade().
        {
            let soroban_tx_max = self
                .soroban_network_info()
                .map(|info| info.tx_max_size_bytes);
            let new_max = compute_max_tx_size(result.header.ledger_version, soroban_tx_max);
            let old_max = self.max_tx_size_bytes.load(Ordering::Relaxed);
            let diff = new_max.saturating_sub(old_max);
            self.max_tx_size_bytes.store(new_max, Ordering::Relaxed);
            if diff > 0 {
                if let Some(overlay) = self.overlay().await {
                    overlay.handle_max_tx_size_increase(diff).await;
                }
            }
        }

        // Clean up old survey rate limiter entries.
        // Mirrors upstream SurveyManager::clearOldLedgers() called from clearLedgersBelow().
        {
            let mut limiter = self.survey_limiter.write().await;
            limiter.clear_old_ledgers(pending.ledger_seq);
        }

        if !failed_hashes.is_empty() {
            tracing::debug!(
                failed_count = failed_hashes.len(),
                "Banning failed transactions"
            );
            self.herder.tx_queue().ban(&failed_hashes);
        }

        // Record externalized close time for drift tracking.
        if let Ok(mut tracker) = self.drift_tracker.lock() {
            if let Some(warning) =
                tracker.record_externalized_close_time(pending.ledger_seq, pending.close_time)
            {
                tracing::warn!("{}", warning);
            }
        }

        self.herder.tx_queue().update_validation_context(
            pending.ledger_seq,
            result.header.scp_value.close_time.0,
            result.header.ledger_version,
            result.header.base_fee,
        );

        let shift_result = self.herder.tx_queue().shift();
        if shift_result.unbanned_count > 0 || shift_result.evicted_due_to_age > 0 {
            tracing::debug!(
                unbanned = shift_result.unbanned_count,
                evicted = shift_result.evicted_due_to_age,
                "Shifted transaction ban queue"
            );
        }

        // Update current ledger tracking.
        *self.current_ledger.write().await = pending.ledger_seq;
        *self.last_processed_slot.write().await = pending.ledger_seq as u64;
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

        self.tx_set_all_peers_exhausted
            .store(false, Ordering::SeqCst);

        true
    }
}
