use super::*;

impl App {
    /// Run catchup to a target ledger with minimal mode.
    ///
    /// This downloads history from archives and applies it to bring the
    /// node up to date with the network. Uses Minimal mode by default.
    pub async fn catchup(&self, target: CatchupTarget) -> anyhow::Result<CatchupResult> {
        self.catchup_with_mode(target, CatchupMode::Minimal).await
    }

    /// Run catchup to a target ledger with a specific mode.
    ///
    /// The mode controls how much history is downloaded:
    /// - Minimal: Only download bucket state at latest checkpoint
    /// - Recent(N): Download and replay the last N ledgers
    /// - Complete: Download complete history from genesis
    pub async fn catchup_with_mode(
        &self,
        target: CatchupTarget,
        mode: CatchupMode,
    ) -> anyhow::Result<CatchupResult> {
        // Fatal-failure guard (spec §13.3): a previous catchup detected a
        // verification/integrity failure.  Further attempts are futile and
        // must be blocked until the operator intervenes.
        if self.catchup_fatal_failure.load(Ordering::SeqCst) {
            anyhow::bail!(
                "catchup blocked: previous fatal verification failure — \
                 manual intervention required"
            );
        }

        self.set_state(AppState::CatchingUp).await;

        let progress = Arc::new(CatchupProgress::new());

        tracing::info!(?target, ?mode, "Starting catchup");

        // Determine target ledger
        let target_ledger = match target {
            CatchupTarget::Current => {
                // Query archive for latest checkpoint (use cache to avoid repeated network calls)
                self.get_cached_archive_checkpoint().await?
            }
            CatchupTarget::Ledger(seq) => seq,
            CatchupTarget::Checkpoint(checkpoint) => checkpoint * 64,
        };

        progress.set_target(target_ledger);

        tracing::info!(target_ledger = target_ledger, "Target ledger determined");

        // Check if we're already at or past the target
        let current = self.get_current_ledger().await.unwrap_or(0);

        // Safety: for Ledger targets, verify the archive has the required checkpoint.
        // Replay from current+1 to target_ledger needs all checkpoint files covering
        // that range.  If the checkpoint containing target_ledger hasn't been published
        // (i.e., it's beyond the latest externalized slot), the download will 404.
        if matches!(target, CatchupTarget::Ledger(_)) && current > 0 {
            let target_cp = checkpoint_containing(target_ledger);
            let latest_ext = self.herder.latest_externalized_slot().unwrap_or(0);
            if target_cp as u64 > latest_ext {
                tracing::warn!(
                    target_ledger,
                    target_checkpoint = target_cp,
                    latest_externalized = latest_ext,
                    current_ledger = current,
                    "Catchup target requires unpublished checkpoint — aborting"
                );
                return Ok(CatchupResult {
                    ledger_seq: current,
                    ledger_hash: Hash256::default(),
                    buckets_applied: 0,
                    ledgers_replayed: 0,
                });
            }
        }

        if target_ledger <= current {
            tracing::info!(
                current_ledger = current,
                target_ledger,
                "Already at or past target; skipping catchup"
            );
            // Record skip time for cooldown to prevent repeated catchup attempts.
            // We need to wait for the next checkpoint to become available.
            *self.last_catchup_completed_at.write().await = Some(Instant::now());
            return Ok(CatchupResult {
                ledger_seq: current,
                ledger_hash: Hash256::default(),
                buckets_applied: 0,
                ledgers_replayed: 0,
            });
        }

        // For replay-only catchup (Case 1: LCL > genesis), we need the bucket
        // lists at the current LCL to replay ledgers from LCL+1 to target.
        //
        // Fast path: if the ledger manager is already initialized, clone the
        // bucket lists directly. This is instant (Bucket uses Arc internally)
        // and avoids the expensive rebuild_bucket_lists_from_has path which
        // loads all buckets from disk + runs full merge restarts (~2+ min on
        // mainnet). It also ensures exact state parity — the HAS reconstruction
        // path can produce subtly different pending merge states.
        //
        // Slow path: if the ledger manager is NOT initialized (e.g., first
        // startup with existing DB), fall back to rebuilding from persisted HAS.
        let (existing_state, override_lcl) =
            if current > GENESIS_LEDGER_SEQ {
                if self.ledger_manager.is_initialized() {
                    // Fast path: clone from live ledger manager.
                    // Must resolve async merges first — structure-based restart_merges
                    // creates PendingMerge::Async handles, and BucketLevel::clone()
                    // drops unresolved async merges.
                    self.ledger_manager.resolve_pending_bucket_merges();
                    let bucket_list = self.ledger_manager.bucket_list().clone();
                    let hot_archive = self.ledger_manager.hot_archive_bucket_list()
                        .clone()
                        .unwrap_or_default();
                    let header = self.ledger_manager.current_header();
                    let network_id = NetworkId(self.network_id());

                    tracing::info!(
                        current_lcl = current,
                        target_ledger,
                        bucket_list_hash = %bucket_list.hash().to_hex(),
                        "Cloned bucket lists from ledger manager for replay-only catchup"
                    );

                    (Some(ExistingBucketState {
                        bucket_list,
                        hot_archive_bucket_list: hot_archive,
                        header,
                        network_id,
                    }), Some(current))
                } else {
                    // Slow path: rebuild from persisted HAS
                    match self.rebuild_bucket_lists_from_has().await {
                        Ok(state) => {
                            tracing::info!(
                                current_lcl = current,
                                target_ledger,
                                "Rebuilt bucket lists from persisted HAS for replay-only catchup (Case 1)"
                            );
                            (Some(state), Some(current))
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "Failed to rebuild bucket lists from HAS, falling back to full catchup"
                            );
                            (None, None)
                        }
                    }
                }
            } else {
                (None, None)
            };
        // Run catchup work
        let output = self
            .run_catchup_work(
                target_ledger,
                mode,
                progress.clone(),
                existing_state,
                override_lcl,
            )
            .await?;

        // Persist the HAS and LCL to DB after catchup.
        // The LedgerManager is already initialized inside the catchup pipeline,
        // so we read the current state from it.
        //
        // This is critical: if a second catchup triggers before any ledger close
        // happens (e.g., when LCL+1 is missing from the buffer), rebuild_bucket_lists_from_has()
        // will read the HAS from the database. Without this persistence, it would
        // read stale HAS from before the first catchup, producing wrong bucket list
        // hashes on replay.
        //
        // This matches stellar-core's CatchupWork.cpp which calls
        // setLastClosedLedger() (persisting both LCL and HAS) after bucket apply.
        {
            let final_header = self.ledger_manager.current_header();
            let bucket_list = self.ledger_manager.bucket_list();
            let hot_archive_guard = self.ledger_manager.hot_archive_bucket_list();
            let default_hot_archive = HotArchiveBucketList::default();
            let hot_archive_ref = hot_archive_guard.as_ref().unwrap_or(&default_hot_archive);
            let has = build_history_archive_state(
                final_header.ledger_seq,
                &bucket_list,
                Some(hot_archive_ref),
                Some(self.config.network.passphrase.clone()),
            )
            .map_err(|e| anyhow::anyhow!("Failed to build HAS after catchup: {}", e))?;
            let has_json = has
                .to_json()
                .map_err(|e| anyhow::anyhow!("Failed to serialize HAS after catchup: {}", e))?;
            let header_xdr = final_header
                .to_xdr(stellar_xdr::curr::Limits::none())
                .map_err(|e| anyhow::anyhow!("Failed to serialize header XDR after catchup: {}", e))?;

            self.db.transaction(|conn| {
                conn.store_ledger_header(&final_header, &header_xdr)?;
                conn.set_state(state_keys::HISTORY_ARCHIVE_STATE, &has_json)?;
                conn.set_last_closed_ledger(final_header.ledger_seq)?;
                Ok(())
            })?;

            tracing::info!(
                ledger_seq = final_header.ledger_seq,
                "Persisted HAS and LCL to DB after catchup"
            );
        }

        tracing::info!(
            ledger_seq = output.result.ledger_seq,
            "Ledger manager initialized from catchup"
        );

        progress.set_phase(crate::logging::CatchupPhase::Complete);
        progress.summary();

        // Trim buffered ledgers that are now stale (at or before the new LCL).
        // Keep ledgers AFTER the catchup target - they will be applied next.
        // This matches stellar-core's behavior in LedgerApplyManagerImpl::trimSyncingLedgers.
        {
            let mut buffer = self.syncing_ledgers.write().await;
            let old_count = buffer.len();
            let new_lcl = output.result.ledger_seq;
            // Keep ledgers > new_lcl (i.e., remove ledgers <= new_lcl)
            buffer.retain(|seq, _| *seq > new_lcl);
            let kept_count = buffer.len();
            let removed_count = old_count - kept_count;
            if removed_count > 0 || kept_count > 0 {
                tracing::info!(
                    old_count,
                    removed_count,
                    kept_count,
                    new_lcl,
                    first_buffered = buffer.keys().next(),
                    "Trimmed stale buffered ledgers after catchup, keeping future ledgers"
                );
            }
        }

        // Clear bucket manager cache to release memory after catchup.
        // The bucket files are still on disk if needed, but we don't need to
        // keep them in RAM. With frequent catchups, this cache can grow unbounded.
        let cache_size_before = self.bucket_manager.cache_size();
        self.bucket_manager.clear_cache();

        tracing::debug!(
            cache_size_before,
            "Cleared bucket manager cache after catchup"
        );

        // Garbage collect bucket files no longer referenced after catchup.
        // Matches stellar-core's cleanupStaleFiles() in assumeState().
        // Run on the blocking thread pool: resolve_pending_bucket_merges() inside
        // cleanup_stale_bucket_files() uses block_in_place() to wait for in-flight
        // async merges, which would freeze the tokio event loop if run inline.
        {
            let lm = self.ledger_manager.clone();
            let bm = self.bucket_manager.clone();
            let _ = tokio::task::spawn_blocking(move || {
                lm.resolve_pending_bucket_merges();

                let hashes = lm.all_referenced_bucket_hashes();
                match bm.retain_buckets(&hashes) {
                    Ok(deleted) => {
                        if deleted > 0 {
                            tracing::info!(deleted, "Cleaned up stale bucket files");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to cleanup stale bucket files");
                    }
                }

                // Clean up merge temp files left by previous process runs.
                let empty_refs = std::collections::HashSet::new();
                if let Ok(deleted) = bm.cleanup_unreferenced_files(&empty_refs) {
                    if deleted > 0 {
                        tracing::info!(
                            deleted,
                            "Cleaned up merge temp files from previous runs"
                        );
                    }
                }
            })
            .await;
        }

        // Trim herder/scp_driver caches to release memory after catchup, but
        // PRESERVE data for slots > new_lcl that will be needed for buffered ledgers.
        // This is critical: during catchup, we receive EXTERNALIZE envelopes and
        // cache their tx_sets. After catchup, we need those tx_sets to apply the
        // buffered ledgers. If we clear them, peers may have already evicted those
        // old tx_sets, causing "DontHave" responses and sync failures.
        let new_lcl = output.result.ledger_seq;
        self.herder.trim_scp_driver_caches(new_lcl as u64);
        self.herder.trim_fetching_caches(new_lcl as u64);

        // Clear pending envelopes for slots <= new_lcl - they are stale after catchup.
        // Note: we keep pending envelopes for slots > new_lcl as they may still be
        // waiting for tx_sets that we just preserved above.
        self.herder.clear_pending_envelopes();

        // On Linux, ask glibc to return freed memory to the OS.
        // This helps prevent RSS from appearing to grow unboundedly after catchups,
        // even though Rust has freed the memory internally.
        #[cfg(target_os = "linux")]
        {
            // SAFETY: malloc_trim is a standard glibc function that's safe to call.
            // It returns memory to the OS and is commonly used after large deallocations.
            unsafe {
                let trimmed = libc::malloc_trim(0);
                tracing::info!(
                    trimmed,
                    "Called malloc_trim after catchup to return memory to OS"
                );
            }
        }

        // Reset the tx set exhausted flag after catchup - fresh start
        self.tx_set_all_peers_exhausted
            .store(false, Ordering::SeqCst);

        // Update cache with the ledger we caught up to (it's a checkpoint)
        {
            let mut cache = self.cached_archive_checkpoint.write().await;
            *cache = Some((output.result.ledger_seq, Instant::now()));
        }

        // Drain all sequential buffered ledgers before returning.
        // This matches stellar-core's behavior: CatchupWork creates
        // ApplyBufferedLedgersWork which applies all sequential buffered
        // ledgers before CatchupWork returns WORK_SUCCESS.
        // Without this, we'd transition to Synced with a gap (the next
        // sequential ledger is buffered but not applied), and the stuck
        // timeout would trigger an unnecessary re-catchup.
        let drained = self.drain_buffered_ledgers_sync().await;
        if drained > 0 {
            let new_ledger = self.get_current_ledger().await.unwrap_or(0);
            tracing::info!(
                drained,
                new_ledger,
                "Drained buffered ledgers as part of catchup completion"
            );
        }

        // Record catchup completion time for cooldown AFTER draining buffered
        // ledgers. This ensures the cooldown window starts after all post-catchup
        // work is complete, preventing maybe_start_buffered_catchup() from
        // triggering a second catchup while the node is still stabilizing.
        *self.last_catchup_completed_at.write().await = Some(Instant::now());

        let final_ledger = self.get_current_ledger().await.unwrap_or(output.result.ledger_seq);
        Ok(CatchupResult {
            ledger_seq: final_ledger,
            ledger_hash: output.result.ledger_hash,
            buckets_applied: output.result.buckets_downloaded,
            ledgers_replayed: output.result.ledgers_applied,
        })
    }

    /// Get the latest checkpoint from history archives, using a cache to avoid repeated network calls.
    /// The cache is valid for ARCHIVE_CHECKPOINT_CACHE_SECS.
    async fn get_cached_archive_checkpoint(&self) -> anyhow::Result<u32> {
        // Check cache first
        {
            let cache = self.cached_archive_checkpoint.read().await;
            if let Some((checkpoint, queried_at)) = *cache {
                if queried_at.elapsed().as_secs() < ARCHIVE_CHECKPOINT_CACHE_SECS {
                    tracing::debug!(
                        checkpoint,
                        age_secs = queried_at.elapsed().as_secs(),
                        "Using cached archive checkpoint"
                    );
                    return Ok(checkpoint);
                }
            }
        }

        // Cache miss or expired, query archive
        let checkpoint = self.get_latest_checkpoint().await?;

        // Update cache
        {
            let mut cache = self.cached_archive_checkpoint.write().await;
            *cache = Some((checkpoint, Instant::now()));
        }

        Ok(checkpoint)
    }

    /// Get the latest checkpoint from history archives.
    async fn get_latest_checkpoint(&self) -> anyhow::Result<u32> {
        tracing::info!("Querying history archives for latest checkpoint");

        // Try each configured archive to get the current ledger
        for archive_config in &self.config.history.archives {
            match HistoryArchive::new(&archive_config.url) {
                Ok(archive) => {
                    match archive.get_current_ledger().await {
                        Ok(ledger) => {
                            tracing::info!(
                                ledger,
                                archive = %archive_config.url,
                                "Got current ledger from archive"
                            );
                            // Round down to the latest completed checkpoint
                            let checkpoint =
                                henyey_history::checkpoint::latest_checkpoint_before_or_at(
                                    ledger,
                                )
                                .ok_or_else(|| {
                                    anyhow::anyhow!("No checkpoint available for ledger {}", ledger)
                                })?;
                            return Ok(checkpoint);
                        }
                        Err(e) => {
                            tracing::warn!(
                                archive = %archive_config.url,
                                error = %e,
                                "Failed to get current ledger from archive"
                            );
                            continue;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        archive = %archive_config.url,
                        error = %e,
                        "Failed to create archive client"
                    );
                    continue;
                }
            }
        }

        Err(anyhow::anyhow!(
            "Failed to get current ledger from any archive"
        ))
    }

    /// Run the catchup work using the real CatchupManager.
    async fn run_catchup_work(
        &self,
        target_ledger: u32,
        mode: CatchupMode,
        progress: Arc<CatchupProgress>,
        existing_state: Option<ExistingBucketState>,
        override_lcl: Option<u32>,
    ) -> anyhow::Result<CatchupOutput> {
        use crate::logging::CatchupPhase;

        // Phase 1: Create history archives from config
        progress.set_phase(CatchupPhase::DownloadingState);
        tracing::info!(target_ledger, "Downloading history archive state");

        let archives: Vec<HistoryArchive> = self
            .config
            .history
            .archives
            .iter()
            .filter(|a| a.get_enabled)
            .filter_map(|a| match HistoryArchive::new(&a.url) {
                Ok(archive) => Some(archive),
                Err(e) => {
                    tracing::warn!(url = %a.url, error = %e, "Failed to create archive");
                    None
                }
            })
            .collect();

        if archives.is_empty() {
            return Err(anyhow::anyhow!("No history archives available"));
        }

        tracing::info!(
            archive_count = archives.len(),
            "Created history archive clients"
        );

        let checkpoint_seq = latest_checkpoint_before_or_at(target_ledger).ok_or_else(|| {
            anyhow::anyhow!("target ledger {} is before first checkpoint", target_ledger)
        })?;

        let archives_arc: Vec<Arc<HistoryArchive>> = archives.into_iter().map(Arc::new).collect();

        // Only use historywork for Minimal mode WITHOUT existing bucket state.
        // When we have existing bucket state (Case 1: replay from LCL), skip historywork
        // entirely — it would unnecessarily download all buckets when we only need
        // transaction history for replay.
        let checkpoint_data = if mode == CatchupMode::Minimal && existing_state.is_none() {
            if let Some(primary) = archives_arc.first() {
                match self
                    .download_checkpoint_with_historywork(Arc::clone(primary), checkpoint_seq)
                    .await
                {
                    Ok(data) => {
                        tracing::info!(checkpoint_seq, "Using historywork for checkpoint downloads");
                        Some(data)
                    }
                    Err(err) => {
                        tracing::warn!(
                            checkpoint_seq,
                            error = %err,
                            "Historywork download failed, falling back to direct catchup"
                        );
                        None
                    }
                }
            } else {
                None
            }
        } else {
            tracing::info!(
                ?mode,
                "Using mode-aware catchup (historywork only supported for Minimal mode)"
            );
            None
        };

        // Create CatchupManager using Arc references
        let mut catchup_manager = CatchupManager::new_with_arcs(
            archives_arc,
            self.bucket_manager.clone(),
            Arc::new(self.db.clone()),
        );
        catchup_manager.set_network_passphrase(self.config.network.passphrase.clone());

        // Wire up meta streaming for catchup replay.
        // When --metadata-output-stream is configured, replayed ledgers
        // stream their LedgerCloseMeta to the pipe. This is required for
        // stellar-rpc's bounded replay mode (`catchup --metadata-output-stream fd:3`).
        //
        // We temporarily move the MetaStreamManager into a shared Arc so the
        // callback closure can use it. After catchup completes, we move it back.
        let shared_meta_stream = {
            let mut guard = self.meta_stream.lock().unwrap();
            guard
                .take()
                .map(|stream| Arc::new(std::sync::Mutex::new(stream)))
        };
        if let Some(ref stream_arc) = shared_meta_stream {
            let stream_for_callback = Arc::clone(stream_arc);
            catchup_manager.set_meta_callback(Box::new(move |meta| {
                let mut guard = stream_for_callback.lock().unwrap();
                if let Err(e) = guard.emit_meta(&meta) {
                    tracing::error!(error = %e, "Fatal: metadata stream write failed during catchup replay");
                    std::process::abort();
                }
            }));
        }

        // Run catchup
        progress.set_phase(CatchupPhase::DownloadingBuckets);

        // Get current LCL for mode calculation.
        // When an override is provided (e.g., after rebuild_bucket_lists_from_has
        // which resets the ledger manager), use it directly.
        // Otherwise, query the ledger manager.
        let lcl = if let Some(lcl_override) = override_lcl {
            lcl_override
        } else {
            match self.get_current_ledger().await {
                Ok(seq) if seq >= GENESIS_LEDGER_SEQ => seq,
                _ => GENESIS_LEDGER_SEQ,
            }
        };

        let output = match checkpoint_data {
            Some(data) => {
                // With checkpoint data, use direct method (minimal mode behavior)
                catchup_manager
                    .catchup_to_ledger_with_checkpoint_data(target_ledger, data, &self.ledger_manager)
                    .await
            }
            None => {
                // Use mode-aware catchup
                catchup_manager
                    .catchup_to_ledger_with_mode(target_ledger, mode, lcl, existing_state, &self.ledger_manager)
                    .await
            }
        }
        .map_err(|e| anyhow::anyhow!("Catchup failed: {}", e))?;

        // Drop the catchup manager (and its meta callback) before restoring
        // the MetaStreamManager. The callback holds an Arc clone.
        drop(catchup_manager);

        // Restore the MetaStreamManager back to self after catchup completes.
        if let Some(stream_arc) = shared_meta_stream {
            match Arc::try_unwrap(stream_arc) {
                Ok(mutex) => {
                    let stream = mutex.into_inner().unwrap();
                    *self.meta_stream.lock().unwrap() = Some(stream);
                }
                Err(_arc) => {
                    // Shouldn't happen: the callback was dropped with catchup_manager.
                    tracing::warn!(
                        "MetaStreamManager Arc still shared after catchup; \
                         meta stream will not be available for live mode"
                    );
                }
            }
        }

        // Update progress with bucket count
        progress.set_total_buckets(output.result.buckets_downloaded);
        for _ in 0..output.result.buckets_downloaded {
            progress.bucket_downloaded();
        }

        // Update ledger progress
        progress.set_phase(CatchupPhase::ReplayingLedgers);
        for _ in 0..output.result.ledgers_applied {
            progress.ledger_applied();
        }

        // Verify
        progress.set_phase(CatchupPhase::Verifying);
        tracing::info!("Verifying catchup state");

        Ok(output)
    }

    async fn download_checkpoint_with_historywork(
        &self,
        archive: Arc<HistoryArchive>,
        checkpoint_seq: u32,
    ) -> anyhow::Result<CheckpointData> {
        let state = Arc::new(tokio::sync::Mutex::new(HistoryWorkState::default()));
        let mut scheduler = WorkScheduler::new(WorkSchedulerConfig {
            max_concurrency: 16, // Match stellar-core MAX_CONCURRENT_SUBPROCESSES
            retry_delay: Duration::from_millis(200),
            event_tx: None,
        });
        let bucket_dir = self.bucket_manager.bucket_dir().to_path_buf();
        let builder = HistoryWorkBuilder::new(
            archive,
            checkpoint_seq,
            Arc::clone(&state),
            bucket_dir,
        );
        let ids = builder.register(&mut scheduler);

        let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
        let state_monitor = Arc::clone(&state);
        let monitor = tokio::spawn(async move {
            let mut last_stage = None;
            let mut last_message = String::new();
            let mut interval = tokio::time::interval(Duration::from_millis(250));
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let progress = get_progress(&state_monitor).await;
                        if progress.stage != last_stage || progress.message != last_message {
                            last_stage = progress.stage.clone();
                            last_message = progress.message.clone();
                            if let Some(stage) = progress.stage {
                                tracing::info!(stage = ?stage, message = %progress.message, "Historywork progress");
                            }
                        }
                    }
                    _ = stop_rx.changed() => {
                        if *stop_rx.borrow() {
                            break;
                        }
                    }
                }
            }
        });

        scheduler.run_until_done().await;

        let _ = stop_tx.send(true);
        let _ = monitor.await;

        let work_ids = [
            ids.has,
            ids.buckets,
            ids.headers,
            ids.transactions,
            ids.tx_results,
            ids.scp_history,
        ];
        for id in work_ids {
            match scheduler.state(id) {
                Some(WorkState::Success) => {}
                state => {
                    return Err(anyhow::anyhow!(
                        "historywork failed; work {} ended in {:?}",
                        id,
                        state
                    ));
                }
            }
        }

        build_checkpoint_data(&state).await
    }

    /// Start caching messages during catchup using the stored weak reference.
    /// This can be called from `&self` methods unlike `start_catchup_message_caching`.
    ///
    /// Returns a JoinHandle that can be aborted when catchup completes.
    pub(super) async fn start_catchup_message_caching_from_self(
        &self,
    ) -> Option<tokio::task::JoinHandle<()>> {
        tracing::info!("Attempting to start catchup message caching from self_arc");
        let weak = self.self_arc.read().await;
        let app = match weak.upgrade() {
            Some(arc) => {
                tracing::info!("Successfully upgraded self_arc weak reference");
                arc
            }
            None => {
                tracing::warn!("Failed to upgrade self_arc weak reference for message caching");
                return None;
            }
        };
        drop(weak); // Release the read lock before calling async method
        let handle = app.start_catchup_message_caching().await;
        if handle.is_some() {
            tracing::info!("Started catchup message caching task from self_arc");
        } else {
            tracing::warn!("Failed to start catchup message caching task (overlay not available?)");
        }
        handle
    }

    /// Start caching messages during catchup.
    ///
    /// Returns a JoinHandle that can be aborted when catchup completes.
    /// This method starts a background task that caches GeneralizedTxSets
    /// and requests tx_sets for EXTERNALIZE messages during catchup.
    /// Uses a dedicated mpsc channel (via subscribe_catchup) that never drops
    /// messages, unlike the broadcast channel which overflows during high traffic.
    pub async fn start_catchup_message_caching(
        self: &Arc<Self>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if let Some(overlay) = self.overlay().await {
            let message_rx = overlay.subscribe_catchup();
            let app = Arc::clone(self);
            Some(tokio::spawn(async move {
                app.cache_messages_during_catchup_impl(message_rx).await;
            }))
        } else {
            None
        }
    }

    /// Cache messages during catchup to bridge the gap between catchup and live consensus.
    ///
    /// This runs in a background task during catchup:
    /// 1. Caching GeneralizedTxSets received from peers
    /// 2. Processing EXTERNALIZE messages to request their tx_sets
    ///
    /// Uses a dedicated mpsc channel that never drops messages, ensuring no
    /// EXTERNALIZE or GeneralizedTxSet messages are lost during catchup.
    async fn cache_messages_during_catchup_impl(
        &self,
        mut message_rx: tokio::sync::mpsc::UnboundedReceiver<OverlayMessage>,
    ) {
        use stellar_xdr::curr::{
            GeneralizedTransactionSet, Limits, ScpStatementPledges, TransactionPhase,
            TxSetComponent, WriteXdr,
        };
        use std::collections::HashSet;

        let mut cached_tx_sets = 0u32;
        let mut requested_tx_sets = 0u32;
        let mut recorded_externalized = 0u32;
        let mut rejected_externalized = 0u32;
        // Track tx_sets we've already broadcast requests for to avoid spamming all peers
        let mut requested_hashes: HashSet<Hash256> = HashSet::new();

        while let Some(msg) = message_rx.recv().await {
            match msg.message {
                StellarMessage::GeneralizedTxSet(gen_tx_set) => {
                    // Compute hash as SHA-256 of XDR-encoded GeneralizedTransactionSet
                    let xdr_bytes =
                        match gen_tx_set.to_xdr(stellar_xdr::curr::Limits::none()) {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                tracing::warn!(error = %e, "Failed to encode GeneralizedTxSet to XDR");
                                continue;
                            }
                        };
                    let hash = henyey_common::Hash256::hash(&xdr_bytes);

                    // Extract transactions from the GeneralizedTxSet
                    let prev_hash = match &gen_tx_set {
                        GeneralizedTransactionSet::V1(v1) => {
                            henyey_common::Hash256::from_bytes(
                                v1.previous_ledger_hash.0,
                            )
                        }
                    };

                    let transactions: Vec<stellar_xdr::curr::TransactionEnvelope> =
                        match &gen_tx_set {
                            GeneralizedTransactionSet::V1(v1) => v1
                                .phases
                                .iter()
                                .flat_map(|phase| match phase {
                                    TransactionPhase::V0(components) => components
                                        .iter()
                                        .flat_map(|component| match component {
                                            TxSetComponent::TxsetCompTxsMaybeDiscountedFee(
                                                comp,
                                            ) => comp.txs.to_vec(),
                                        })
                                        .collect::<Vec<_>>(),
                                    TransactionPhase::V1(parallel) => parallel
                                        .execution_stages
                                        .iter()
                                        .flat_map(|stage| {
                                            stage
                                                .0
                                                .iter()
                                                .flat_map(|cluster| cluster.0.to_vec())
                                        })
                                        .collect(),
                                })
                                .collect(),
                        };

                    let tx_set = henyey_herder::TransactionSet::with_generalized(
                        prev_hash,
                        hash,
                        transactions,
                        gen_tx_set,
                    );

                    // Cache it in herder (this will be available after catchup)
                    self.herder.cache_tx_set(tx_set);
                    cached_tx_sets += 1;

                    tracing::debug!(
                        cached_tx_sets,
                        hash = %hash,
                        "Cached tx_set during catchup"
                    );
                }

                StellarMessage::ScpMessage(envelope) => {
                    // For EXTERNALIZE messages, extract tx_set_hash, record, and request
                    if let ScpStatementPledges::Externalize(ext) =
                        &envelope.statement.pledges
                    {
                        let slot = envelope.statement.slot_index;
                        let value = ext.commit.value.clone();

                        // Parse the StellarValue to validate before recording
                        let sv = match StellarValue::from_xdr(&ext.commit.value.0, Limits::none()) {
                            Ok(sv) => sv,
                            Err(e) => {
                                tracing::debug!(
                                    slot,
                                    error = %e,
                                    "Rejecting EXTERNALIZE during catchup: failed to parse StellarValue"
                                );
                                rejected_externalized += 1;
                                continue;
                            }
                        };

                        // Validate close-time: reject messages with stale or future close times.
                        // This prevents accepting EXTERNALIZE from pre-reset eras or other networks.
                        let lcl_close_time = self.ledger_manager.current_header().scp_value.close_time.0;
                        let scp_driver = self.herder.scp_driver();
                        if !scp_driver.check_close_time(slot, lcl_close_time, sv.close_time.0) {
                            tracing::debug!(
                                slot,
                                close_time = sv.close_time.0,
                                lcl_close_time,
                                "Rejecting EXTERNALIZE during catchup: close-time validation failed"
                            );
                            rejected_externalized += 1;
                            continue;
                        }

                        // Validate slot range: reject slots impossibly far from LCL.
                        // Use the same LEDGER_VALIDITY_BRACKET (100) as the herder.
                        let lcl_seq = self.ledger_manager.current_ledger_seq() as u64;
                        const LEDGER_VALIDITY_BRACKET: u64 = 100;
                        if slot > lcl_seq + LEDGER_VALIDITY_BRACKET {
                            tracing::debug!(
                                slot,
                                lcl_seq,
                                max = lcl_seq + LEDGER_VALIDITY_BRACKET,
                                "Rejecting EXTERNALIZE during catchup: slot outside validity bracket"
                            );
                            rejected_externalized += 1;
                            continue;
                        }

                        // Verify envelope signature to prevent accepting forged messages
                        if let Err(e) = scp_driver.verify_envelope(&envelope) {
                            tracing::debug!(
                                slot,
                                error = %e,
                                "Rejecting EXTERNALIZE during catchup: invalid signature"
                            );
                            rejected_externalized += 1;
                            continue;
                        }

                        // All validations passed - record this externalized slot
                        scp_driver.record_externalized(slot, value);
                        recorded_externalized += 1;
                        tracing::debug!(
                            slot,
                            "Recorded externalized slot during catchup"
                        );

                        let tx_set_hash = Hash256::from_bytes(sv.tx_set_hash.0);

                        // Check if we already have this tx_set or already broadcast a request
                        if !self.herder.has_tx_set(&tx_set_hash)
                            && !requested_hashes.contains(&tx_set_hash)
                        {
                            // Register as pending and send GetTxSet request
                            self.herder
                                .scp_driver()
                                .request_tx_set(tx_set_hash, slot);

                            // Track that we've requested this hash to avoid duplicate broadcasts
                            requested_hashes.insert(tx_set_hash);

                            // Broadcast GetTxSet request to ALL peers, not just the sender.
                            // This is critical for bridging the gap after catchup: by the time
                            // catchup completes, older tx_sets may be evicted from the sender's
                            // cache. By requesting from all peers, we maximize our chances of
                            // getting the tx_set before any single peer evicts it.
                            let overlay = self.overlay().await;
                            if let Some(overlay) = overlay {
                                match overlay.request_tx_set(&tx_set_hash.0).await {
                                    Ok(peer_count) => {
                                        requested_tx_sets += 1;
                                        tracing::debug!(
                                            slot,
                                            hash = %tx_set_hash,
                                            peer_count,
                                            "Broadcast tx_set request to all peers during catchup"
                                        );
                                    }
                                    Err(e) => {
                                        tracing::debug!(
                                            slot,
                                            error = %e,
                                            "Failed to broadcast tx_set request during catchup"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }

                _ => {
                    // Ignore other message types during catchup
                }
            }
        }

        tracing::info!(
            cached_tx_sets,
            requested_tx_sets,
            recorded_externalized,
            rejected_externalized,
            "Finished caching messages during catchup"
        );
    }

    pub(super) async fn maybe_start_buffered_catchup(&self) {
        // Fatal-failure guard (spec §13.3): block further catchup after a
        // verification/integrity failure.
        if self.catchup_fatal_failure.load(Ordering::SeqCst) {
            return;
        }

        // Early cooldown check: if we recently completed or skipped catchup,
        // skip re-evaluating. This prevents log spam and avoids re-triggering
        // catchup while the node is still stabilizing after a catchup cycle.
        // 10 seconds gives enough time for SCP messages to arrive and fill
        // small gaps after catchup + buffered ledger drain.
        const EVALUATION_COOLDOWN_SECS: u64 = 10;
        let cooldown_elapsed = self
            .last_catchup_completed_at
            .read()
            .await
            .map(|t| t.elapsed().as_secs());
        let recently_skipped = cooldown_elapsed
            .is_some_and(|s| s < EVALUATION_COOLDOWN_SECS);
        if recently_skipped {
            tracing::debug!(
                cooldown_elapsed = ?cooldown_elapsed,
                "maybe_start_buffered_catchup: skipped due to cooldown"
            );
            return;
        }

        let current_ledger = match self.get_current_ledger().await {
            Ok(seq) => seq,
            Err(_) => return,
        };

        // Guard: if the node is essentially caught up (gap ≤ TX_SET_REQUEST_WINDOW),
        // do NOT trigger catchup. Stale tx_set requests from prior EXTERNALIZE
        // messages can set tx_set_all_peers_exhausted, but when the gap is small
        // the correct action is to wait for fresh EXTERNALIZE — not to catchup.
        // Without this guard, the node enters an infinite loop:
        //   catchup → rapid close → stale tx_set timeout → all_peers_exhausted
        //   → catchup → repeat
        let latest_externalized = self.herder.latest_externalized_slot().unwrap_or(0);
        let gap = latest_externalized.saturating_sub(current_ledger as u64);
        if gap <= TX_SET_REQUEST_WINDOW {
            // Clear stale state that might trigger unnecessary catchup
            if self.tx_set_all_peers_exhausted.load(Ordering::SeqCst) {
                tracing::info!(
                    current_ledger,
                    latest_externalized,
                    gap,
                    "maybe_start_buffered_catchup: essentially caught up, \
                     clearing tx_set_all_peers_exhausted and stale state"
                );
                self.tx_set_all_peers_exhausted
                    .store(false, Ordering::SeqCst);
                self.tx_set_dont_have.write().await.clear();
                self.tx_set_last_request.write().await.clear();
                self.tx_set_exhausted_warned.write().await.clear();
                self.herder.clear_pending_tx_sets();
            }
            return;
        }

        let (first_buffered, last_buffered) = {
            let mut buffer = self.syncing_ledgers.write().await;
            let pre_trim_count = buffer.len();
            let pre_trim_first = buffer.keys().next().copied();
            let pre_trim_last = buffer.keys().next_back().copied();
            Self::trim_syncing_ledgers(&mut buffer, current_ledger);

            // When all peers have reported DontHave for tx_sets, evict buffered
            // entries starting from current_ledger+1 that have no tx_set. These
            // slots were externalized (we have the SCP value) but the tx_set data
            // has been evicted from all peers' caches and will never arrive.
            // Removing them creates a proper gap so the catchup target logic can
            // compute a valid target (e.g., the next checkpoint boundary).
            // Without this, the buffer looks like [N+1(no tx_set), N+25(tx_set), ...]
            // and first_buffered = N+1 causes catchup target computation to fail
            // (it thinks the gap is only 1 ledger).
            if self.tx_set_all_peers_exhausted.load(Ordering::SeqCst) {
                let mut evicted = 0u32;
                let start = current_ledger.saturating_add(1);
                // Evict consecutive entries from the front that lack tx_sets.
                // Stop at the first entry that HAS a tx_set — those are still
                // usable once we catch up past the gap.
                for seq in start.. {
                    match buffer.get(&seq) {
                        Some(info) if info.tx_set.is_none() => {
                            buffer.remove(&seq);
                            evicted += 1;
                        }
                        _ => break, // gap in buffer or entry has tx_set
                    }
                }
                if evicted > 0 {
                    tracing::info!(
                        current_ledger,
                        evicted,
                        "Evicted buffered entries with permanently unavailable tx_sets"
                    );
                }
            }

            let post_trim_count = buffer.len();
            let post_first = buffer.keys().next().copied();
            let post_last = buffer.keys().next_back().copied();
            tracing::debug!(
                current_ledger,
                pre_trim_count,
                pre_trim_first,
                pre_trim_last,
                post_trim_count,
                post_first,
                post_last,
                "maybe_start_buffered_catchup: buffer state"
            );

            match (post_first, post_last) {
                (Some(first), Some(last)) => (first, last),
                _ => {
                    tracing::debug!(
                        current_ledger,
                        pre_trim_count,
                        pre_trim_first,
                        pre_trim_last,
                        "maybe_start_buffered_catchup: empty buffer after trim/evict, returning"
                    );
                    return;
                }
            }
        };

        tracing::debug!(
            current_ledger,
            first_buffered,
            last_buffered,
            is_checkpoint_boundary = Self::is_first_ledger_in_checkpoint(first_buffered),
            gap = first_buffered.saturating_sub(current_ledger),
            "maybe_start_buffered_catchup: evaluating"
        );

        // Check if sequential ledger has tx set available
        let sequential_with_tx_set = if first_buffered == current_ledger + 1 {
            let buffer = self.syncing_ledgers.read().await;
            buffer
                .get(&first_buffered)
                .is_some_and(|info| info.tx_set.is_some())
        } else {
            false
        };

        if sequential_with_tx_set {
            // Tx set is available, let try_apply_buffered_ledgers() handle it.
            // DON'T reset stuck state here - there's a race condition where the tx_set
            // might have arrived after try_apply_buffered_ledgers() checked but before
            // this check. The stuck state will naturally become invalid when current_ledger
            // advances (the match condition state.current_ledger == current_ledger will fail).
            tracing::debug!(
                current_ledger,
                first_buffered,
                "Sequential ledger tx set available; skipping buffered catchup"
            );
            return;
        }

        // Calculate gap and determine catchup strategy.
        //
        // stellar-core only triggers immediate catchup when the first buffered
        // ledger sits at a checkpoint boundary AND there is at least one more
        // buffered ledger after it. The gap *size* alone is not a trigger — a
        // gap slightly larger than CHECKPOINT_FREQUENCY is expected right after
        // the initial catchup because the network advances while catchup runs.
        // Triggering on gap size alone caused unnecessary second catchup cycles
        // (see: "Buffered gap exceeds checkpoint; starting catchup" log spam).
        // First buffered is checkpoint boundary AND we have multiple buffered ledgers.
        // This matches stellar-core: catchup to first_buffered - 1.
        let can_trigger_immediate =
            Self::is_first_ledger_in_checkpoint(first_buffered) && first_buffered < last_buffered;

        tracing::debug!(
            can_trigger_immediate,
            first_buffered,
            last_buffered,
            is_checkpoint = Self::is_first_ledger_in_checkpoint(first_buffered),
            "maybe_start_buffered_catchup: can_trigger_immediate decision"
        );

        // If we can't trigger immediate catchup, check if we should wait for trigger
        // or if we're stuck and need timeout-based catchup
        if !can_trigger_immediate {
            let (required_first, trigger) = if Self::is_first_ledger_in_checkpoint(first_buffered) {
                (first_buffered, first_buffered.saturating_add(1))
            } else {
                let required_first = Self::first_ledger_in_checkpoint(first_buffered)
                    .saturating_add(CHECKPOINT_FREQUENCY);
                (required_first, required_first.saturating_add(1))
            };

            // Check if we have the trigger ledger
            if last_buffered >= trigger {
                // We have enough buffered ledgers - proceed to catchup below
            } else {
                // We're waiting for trigger - apply consensus stuck timeout
                // This handles the case where we have a gap but can't reach the trigger
                let now = Instant::now();
                let action = {
                    let mut stuck_state = self.consensus_stuck_state.write().await;
                    match stuck_state.as_mut() {
                        // Match on current_ledger only. first_buffered can change as
                        // stale EXTERNALIZE messages from SCP state requests create
                        // new syncing_ledgers entries with lower slot numbers. Matching
                        // on both caused the stuck timer to reset every time
                        // first_buffered shifted, preventing catchup from ever
                        // triggering (Problem 9).
                        Some(state)
                            if state.current_ledger == current_ledger =>
                        {
                            // Update first_buffered to track the current value
                            state.first_buffered = first_buffered;
                            let elapsed = state.stuck_start.elapsed().as_secs();
                            let since_recovery = state.last_recovery_attempt.elapsed().as_secs();

                            // These signals help determine the stuck timeout when NOT
                            // recently caught up. When all peers report DontHave or
                            // requests have been waiting too long, use a faster timeout.
                            let all_peers_exhausted =
                                self.tx_set_all_peers_exhausted.load(Ordering::SeqCst);
                            let has_stale_requests = self
                                .herder
                                .has_stale_pending_tx_set(TX_SET_UNAVAILABLE_TIMEOUT_SECS);
                            let recovery_failed = state.recovery_attempts >= 2;

                            // Cooldown: don't trigger catchup if we recently completed
                            // catchup. stellar-core does NOT have a stuck timeout
                            // that triggers catchup — it only triggers catchup when
                            // checkpoint boundary conditions are met (handled above by
                            // can_trigger_immediate). When recently caught up, only do
                            // recovery (re-request SCP state) to fill gaps.
                            let recently_caught_up = self
                                .last_catchup_completed_at
                                .read()
                                .await
                                .is_some_and(|t| t.elapsed().as_secs() < POST_CATCHUP_RECOVERY_WINDOW_SECS);

                            // When recently caught up, prefer recovery over catchup.
                            // The next checkpoint won't be published to archives for
                            // ~5 min, so archive-based catchup will fail trying to
                            // download unpublished checkpoint data. However, if
                            // recovery has been attempted multiple times without
                            // progress, the missing slots have likely been evicted
                            // from peers' caches and recovery will never succeed.
                            // In that case, fall through to catchup.
                            if recently_caught_up {
                                if state.recovery_attempts >= MAX_POST_CATCHUP_RECOVERY_ATTEMPTS {
                                    // Recovery is futile — same gap persists after
                                    // multiple attempts. Peers don't have the missing
                                    // EXTERNALIZE messages (they only cache ~12 recent
                                    // slots). Trigger catchup instead of waiting the
                                    // full POST_CATCHUP_RECOVERY_WINDOW.
                                    tracing::warn!(
                                        current_ledger,
                                        first_buffered,
                                        last_buffered,
                                        elapsed_secs = elapsed,
                                        recovery_attempts = state.recovery_attempts,
                                        "Post-catchup recovery exhausted; \
                                         missing slots unrecoverable via SCP. \
                                         Triggering catchup."
                                    );
                                    state.catchup_triggered = true;
                                    ConsensusStuckAction::TriggerCatchup
                                } else if since_recovery >= OUT_OF_SYNC_RECOVERY_TIMER_SECS {
                                    state.last_recovery_attempt = now;
                                    state.recovery_attempts += 1;
                                    tracing::info!(
                                        current_ledger,
                                        first_buffered,
                                        elapsed_secs = elapsed,
                                        recovery_attempts = state.recovery_attempts,
                                        max_recovery_attempts = MAX_POST_CATCHUP_RECOVERY_ATTEMPTS,
                                        "Attempting out-of-sync recovery (post-catchup gap)"
                                    );
                                    ConsensusStuckAction::AttemptRecovery
                                } else {
                                    tracing::debug!(
                                        current_ledger,
                                        first_buffered,
                                        elapsed_secs = elapsed,
                                        "Waiting for SCP to fill post-catchup gap"
                                    );
                                    ConsensusStuckAction::Wait
                                }
                            } else {
                                // Not recently caught up — use stuck timeout logic.
                                let use_fast_timeout =
                                    all_peers_exhausted || has_stale_requests || recovery_failed;
                                let effective_timeout = if use_fast_timeout {
                                    TX_SET_UNAVAILABLE_TIMEOUT_SECS
                                } else {
                                    CONSENSUS_STUCK_TIMEOUT_SECS
                                };

                                if state.catchup_triggered {
                                    tracing::debug!(
                                        current_ledger,
                                        first_buffered,
                                        elapsed_secs = elapsed,
                                        "Catchup already triggered, waiting for progress"
                                    );
                                    ConsensusStuckAction::Wait
                                } else if elapsed >= effective_timeout {
                                    tracing::warn!(
                                        current_ledger,
                                        first_buffered,
                                        last_buffered,
                                        required_first,
                                        trigger,
                                        elapsed_secs = elapsed,
                                        recovery_attempts = state.recovery_attempts,
                                        all_peers_exhausted,
                                        has_stale_requests,
                                        recovery_failed,
                                        effective_timeout,
                                        "Buffered catchup stuck timeout; triggering catchup"
                                    );
                                    state.catchup_triggered = true;
                                    self.tx_set_all_peers_exhausted
                                        .store(false, Ordering::SeqCst);
                                    self.tx_set_exhausted_warned.write().await.clear();
                                    ConsensusStuckAction::TriggerCatchup
                                } else if since_recovery >= OUT_OF_SYNC_RECOVERY_TIMER_SECS {
                                    state.last_recovery_attempt = now;
                                    state.recovery_attempts += 1;
                                    tracing::info!(
                                        current_ledger,
                                        first_buffered,
                                        elapsed_secs = elapsed,
                                        recovery_attempts = state.recovery_attempts,
                                        timeout_secs = CONSENSUS_STUCK_TIMEOUT_SECS,
                                        "Attempting out-of-sync recovery (buffered gap)"
                                    );
                                    ConsensusStuckAction::AttemptRecovery
                                } else {
                                    tracing::debug!(
                                        current_ledger,
                                        first_buffered,
                                        last_buffered,
                                        required_first,
                                        trigger,
                                        elapsed_secs = elapsed,
                                        "Waiting for buffered catchup trigger ledger"
                                    );
                                    ConsensusStuckAction::Wait
                                }
                            }
                        }
                        _ => {
                            tracing::info!(
                                current_ledger,
                                first_buffered,
                                last_buffered,
                                required_first,
                                trigger,
                                "Buffered gap detected; starting recovery timer"
                            );
                            *stuck_state = Some(ConsensusStuckState {
                                current_ledger,
                                first_buffered,
                                stuck_start: now,
                                last_recovery_attempt: now,
                                recovery_attempts: 0,
                                catchup_triggered: false,
                            });
                            ConsensusStuckAction::AttemptRecovery
                        }
                    }
                };

                match action {
                    ConsensusStuckAction::Wait => return,
                    ConsensusStuckAction::AttemptRecovery => {
                        self.out_of_sync_recovery(current_ledger).await;
                        return;
                    }
                    ConsensusStuckAction::TriggerCatchup => {
                        // Fall through to catchup below
                    }
                }
            }
        }

        // Determine catchup target
        tracing::debug!(
            current_ledger,
            first_buffered,
            last_buffered,
            "maybe_start_buffered_catchup: computing catchup target"
        );
        let target = Self::buffered_catchup_target(current_ledger, first_buffered, last_buffered);
        let target = match target {
            Some(t) => Some(t),
            None => {
                // Fallback: use timeout-based target if buffered_catchup_target returns None
                // but we've decided to catchup due to timeout
                Self::compute_catchup_target_for_timeout(
                    last_buffered,
                    first_buffered,
                    current_ledger,
                )
            }
        };

        // If we still don't have a target, catch up to the latest checkpoint from archive.
        // This handles the case where we're stuck with a gap we can't bridge via buffered messages.
        let use_current_target = target.is_none();
        let target = match target {
            Some(t) => t,
            None => {
                tracing::info!(
                    current_ledger,
                    first_buffered,
                    last_buffered,
                    "No buffered catchup target; catching up to latest checkpoint from archive"
                );
                // We'll use CatchupTarget::Current below
                0
            }
        };

        if self.catchup_in_progress.swap(true, Ordering::SeqCst) {
            tracing::info!("Buffered catchup already in progress");
            return;
        }

        // Skip the target validation if we're using CatchupTarget::Current
        if !use_current_target && (target == 0 || target <= current_ledger) {
            self.catchup_in_progress.store(false, Ordering::SeqCst);
            return;
        }

        // When using CatchupTarget::Current, check if the archive has a newer checkpoint.
        // Use the cached checkpoint to avoid repeated network calls that block the main loop.
        if use_current_target && is_checkpoint_ledger(current_ledger) {
            match self.get_cached_archive_checkpoint().await {
                Ok(latest_checkpoint) => {
                    if latest_checkpoint <= current_ledger {
                        // This is expected behavior after catchup - archive hasn't published
                        // the next checkpoint yet. Use debug level to avoid log spam.
                        tracing::debug!(
                            current_ledger,
                            latest_checkpoint,
                            first_buffered,
                            "Skipping catchup: archive has no newer checkpoint"
                        );
                        // DON'T reset tx_set tracking here - we're not completing catchup,
                        // just waiting for the next checkpoint. Resetting tracking would
                        // clear pending requests and prevent responses from being matched.
                        // Record skip time for cooldown to prevent repeated archive queries.
                        // This uses the same cooldown mechanism as catchup completion.
                        *self.last_catchup_completed_at.write().await = Some(Instant::now());
                        self.catchup_in_progress.store(false, Ordering::SeqCst);
                        return;
                    }
                    tracing::info!(
                        current_ledger,
                        latest_checkpoint,
                        first_buffered,
                        "Archive has newer checkpoint, proceeding with catchup"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        current_ledger,
                        error = %e,
                        "Failed to query archive for latest checkpoint, skipping catchup"
                    );
                    // Record skip time for cooldown to prevent repeated archive queries.
                    *self.last_catchup_completed_at.write().await = Some(Instant::now());
                    self.catchup_in_progress.store(false, Ordering::SeqCst);
                    return;
                }
            }
        }

        tracing::info!(
            current_ledger,
            target,
            first_buffered,
            last_buffered,
            use_current_target,
            "Starting buffered catchup"
        );

        // Start caching messages during catchup to capture tx_sets for gap ledgers
        let catchup_message_handle = self.start_catchup_message_caching_from_self().await;

        let catchup_target = if use_current_target {
            CatchupTarget::Current
        } else {
            CatchupTarget::Ledger(target)
        };
        let catchup_result = self.catchup(catchup_target).await;

        // Stop the catchup message caching task
        if let Some(handle) = catchup_message_handle {
            handle.abort();
            tracing::debug!("Stopped catchup message caching task (buffered catchup)");
        }

        self.catchup_in_progress.store(false, Ordering::SeqCst);

        self.handle_catchup_result(catchup_result, true, "Buffered").await;
    }

    /// Process the result of a catchup operation: update state, bootstrap herder,
    /// and apply buffered ledgers. Shared by buffered and externalized catchup paths.
    pub(super) async fn handle_catchup_result(
        &self,
        catchup_result: anyhow::Result<CatchupResult>,
        reset_stuck_state: bool,
        label: &str,
    ) {
        match catchup_result {
            Ok(result) => {
                let catchup_did_work = result.buckets_applied > 0 || result.ledgers_replayed > 0;

                if catchup_did_work {
                    if reset_stuck_state {
                        *self.consensus_stuck_state.write().await = None;
                    }
                    *self.current_ledger.write().await = result.ledger_seq;
                    *self.last_processed_slot.write().await = result.ledger_seq as u64;
                    self.clear_tx_advert_history(result.ledger_seq).await;
                    self.herder.bootstrap(result.ledger_seq);
                    self.herder.purge_slots_below(result.ledger_seq as u64);
                    let cleaned = self
                        .herder
                        .cleanup_old_pending_tx_sets(result.ledger_seq as u64 + 1);
                    if cleaned > 0 {
                        tracing::info!(
                            cleaned,
                            "Dropped stale pending tx set requests after catchup"
                        );
                    }
                    self.reset_tx_set_tracking_after_catchup().await;

                    // Clear stale syncing_ledgers entries above the catchup target.
                    // These were created during pre-catchup SCP fast-forwarding and
                    // have tx_set: None because peers had already evicted the tx_sets
                    // for those slots (too old at the time). After catchup brings
                    // current_ledger up to the target, these slots are now recent and
                    // peers SHOULD have their tx_sets. Clearing forces fresh
                    // process_externalized_slots() calls that will re-create entries
                    // via check_ledger_close() with proper tx_set lookups.
                    {
                        let mut buffer = self.syncing_ledgers.write().await;
                        let stale_count = buffer.len();
                        buffer.retain(|&seq, _| seq <= result.ledger_seq);
                        let removed = stale_count - buffer.len();
                        if removed > 0 {
                            tracing::info!(
                                removed,
                                catchup_ledger = result.ledger_seq,
                                "Cleared stale syncing_ledgers entries above catchup target"
                            );
                        }
                    }

                    self.restore_operational_state().await;

                    // Refresh bucket snapshots so the query server sees
                    // the state restored by catchup.
                    self.update_bucket_snapshot();

                    tracing::info!(ledger_seq = result.ledger_seq, "{} catchup complete", label);
                    let latest_ext = self.herder.latest_externalized_slot().unwrap_or(0);
                    let pending_count = self.herder.get_pending_tx_sets().len();
                    let buffer_count = self.syncing_ledgers.read().await.len();
                    tracing::debug!(
                        latest_externalized = latest_ext,
                        last_processed = result.ledger_seq,
                        pending_tx_sets = pending_count,
                        buffered_ledgers = buffer_count,
                        tx_set_cache_size = self.herder.scp_driver().tx_set_cache_size(),
                        "Post-catchup state before try_apply_buffered_ledgers"
                    );
                    self.try_apply_buffered_ledgers().await;

                    // After rapid buffered ledger closes, the broadcast channel
                    // likely overflowed, and process_externalized_slots() may
                    // have created syncing_ledgers entries with tx_set: None
                    // (because fetch responses hadn't been processed yet).
                    // Clear these stale entries and reset tracking so the main
                    // event loop can repopulate them cleanly when it drains
                    // the SCP and fetch_response channels.
                    {
                        let current_ledger = *self.current_ledger.read().await;
                        let mut buffer = self.syncing_ledgers.write().await;
                        let stale_count = buffer.len();
                        buffer.retain(|&seq, _| seq <= current_ledger);
                        let removed = stale_count - buffer.len();
                        if removed > 0 {
                            tracing::info!(
                                removed,
                                current_ledger,
                                "Cleared stale syncing_ledgers entries after buffered close"
                            );
                        }
                    }

                    // Reset last_processed_slot to current_ledger so the main
                    // loop's process_externalized_slots() re-evaluates the gap
                    // from current_ledger+1.  Previously this was set to
                    // latest_ext which skipped all intermediate slots, creating
                    // an unbridgeable gap between current_ledger and
                    // last_processed_slot — no ledger closes could happen and
                    // the node fell into infinite sync recovery loops.
                    //
                    // With last_processed_slot = current_ledger, the main loop
                    // will iterate slots current_ledger+1..=latest_ext:
                    //  - If gap > TX_SET_REQUEST_WINDOW: triggers another
                    //    externalized catchup (line 618 in ledger_close.rs)
                    //  - If gap <= TX_SET_REQUEST_WINDOW: fetches tx_sets from
                    //    peers (recent enough to still be cached)
                    {
                        let current_ledger = *self.current_ledger.read().await;
                        let latest_ext = self.herder.latest_externalized_slot()
                            .unwrap_or(current_ledger as u64);
                        *self.last_processed_slot.write().await = current_ledger as u64;
                        tracing::info!(
                            latest_ext,
                            current_ledger,
                            "Reset last_processed_slot to current_ledger after catchup"
                        );
                    }

                    // Reset tx_set tracking state (same as rapid close handler)
                    // so the main loop can make fresh requests.
                    *self.last_externalized_at.write().await = Instant::now();
                    self.tx_set_all_peers_exhausted.store(false, Ordering::SeqCst);
                    self.tx_set_dont_have.write().await.clear();
                    self.tx_set_last_request.write().await.clear();
                    self.tx_set_exhausted_warned.write().await.clear();
                    *self.consensus_stuck_state.write().await = None;

                    // Do NOT request SCP state from peers after catchup.
                    // That brings in EXTERNALIZE messages for recent slots
                    // whose tx_sets peers have already evicted from their
                    // caches (~60s window).  Processing those creates
                    // syncing_ledgers entries with tx_set: None, triggering
                    // the timeout→exhausted→catchup loop.  Instead, rely on
                    // the dedicated SCP channel which delivers the NEXT fresh
                    // EXTERNALIZE (with a fetchable tx_set) within ~5 seconds.
                } else {
                    tracing::info!(
                        ledger_seq = result.ledger_seq,
                        "{} catchup skipped (already at target); preserving tx_set tracking",
                        label
                    );
                    // Restore operational state even when catchup was a no-op,
                    // since catchup() unconditionally sets CatchingUp on entry.
                    self.restore_operational_state().await;
                }
                *self.last_catchup_completed_at.write().await = Some(Instant::now());
            }
            Err(err) => {
                // Check if this is a fatal catchup failure (verification/integrity
                // error indicating local state corruption).  Per spec §13.3, once
                // a fatal failure is detected, further catchup attempts must be
                // blocked — they will keep failing and waste resources.
                let is_fatal = err
                    .downcast_ref::<henyey_history::HistoryError>()
                    .is_some_and(|e| e.is_fatal_catchup_failure());
                if is_fatal {
                    tracing::error!(
                        error = %err,
                        "{} catchup FATAL: verification/integrity failure — \
                         local state may be corrupt.  Further catchup attempts \
                         will be blocked.  Manual intervention required \
                         (wipe state or restore from known-good snapshot).",
                        label,
                    );
                    self.catchup_fatal_failure.store(true, Ordering::SeqCst);
                } else {
                    tracing::error!(error = %err, "{} catchup failed", label);
                }
                // Restore operational state so the node can continue
                // participating in consensus. Without this, the node stays
                // permanently stuck in CatchingUp after a failed catchup
                // (e.g., archive checkpoint not yet published).
                self.restore_operational_state().await;
                // Apply cooldown after failed catchup to prevent rapid-fire retries.
                // Without this, a failed catchup (e.g., archive checkpoint not yet
                // published) would re-trigger immediately on the next tick because
                // the stuck state's recovery_attempts are already exhausted.
                *self.last_catchup_completed_at.write().await = Some(Instant::now());
                // Reset the stuck state so the recovery/timeout cycle re-arms.
                // This provides natural backoff: 10s cooldown + 3 recovery attempts
                // (30s) + catchup retry = ~40s per cycle while waiting for the
                // archive to publish the next checkpoint.
                if reset_stuck_state {
                    if let Some(state) = self.consensus_stuck_state.write().await.as_mut() {
                        state.catchup_triggered = false;
                        state.recovery_attempts = 0;
                        state.last_recovery_attempt = Instant::now();
                    }
                }
            }
        }
    }

    pub(super) async fn maybe_start_externalized_catchup(&self, latest_externalized: u64) {
        // Fatal-failure guard (spec §13.3): block further catchup after a
        // verification/integrity failure.
        if self.catchup_fatal_failure.load(Ordering::SeqCst) {
            return;
        }

        let current_ledger = match self.get_current_ledger().await {
            Ok(seq) => seq,
            Err(_) => return,
        };
        if latest_externalized <= current_ledger as u64 {
            return;
        }
        let gap = latest_externalized.saturating_sub(current_ledger as u64);
        if gap <= TX_SET_REQUEST_WINDOW {
            return;
        }

        // Cooldown: don't retry immediately after a catchup attempt.
        // Failed catchups (e.g., archive checkpoint not yet published)
        // would otherwise trigger rapid-fire retries because
        // process_externalized_slots() re-evaluates the gap on every
        // tick.  10 seconds gives the archive time to publish and
        // avoids wasting resources on repeated download failures.
        const CATCHUP_RETRY_COOLDOWN_SECS: u64 = 10;
        let cooldown_elapsed = self
            .last_catchup_completed_at
            .read()
            .await
            .map(|t| t.elapsed().as_secs());
        if cooldown_elapsed.is_some_and(|s| s < CATCHUP_RETRY_COOLDOWN_SECS) {
            tracing::debug!(
                cooldown_elapsed = ?cooldown_elapsed,
                gap,
                "Externalized catchup skipped due to cooldown"
            );
            return;
        }

        if self.catchup_in_progress.swap(true, Ordering::SeqCst) {
            tracing::info!("Externalized catchup already in progress");
            return;
        }

        let target = latest_externalized.saturating_sub(TX_SET_REQUEST_WINDOW) as u32;
        if target == 0 || target <= current_ledger {
            self.catchup_in_progress.store(false, Ordering::SeqCst);
            return;
        }

        // Skip catchup when any required checkpoint data hasn't been published yet.
        // The archive organizes data into 64-ledger checkpoints.  A replay from
        // current_ledger+1 to target needs all checkpoints covering that range.
        // If the checkpoint containing the TARGET hasn't been published, the
        // download will fail with 404s, blocking the event loop.  The stuck
        // state / escalation logic will eventually use CatchupTarget::Current
        // which queries the archive for what's actually available.
        let target_checkpoint = checkpoint_containing(target);
        if target_checkpoint > latest_externalized as u32 {
            let first_replay = current_ledger as u64 + 1;
            let have_next_externalize = self.herder.get_externalized(first_replay).is_some();
            tracing::info!(
                current_ledger,
                target,
                target_checkpoint,
                latest_externalized,
                have_next_externalize,
                "Skipping archive catchup: target checkpoint not yet published"
            );
            self.catchup_in_progress.store(false, Ordering::SeqCst);
            // Set cooldown to avoid rapid re-evaluation.
            *self.last_catchup_completed_at.write().await = Some(Instant::now());
            return;
        }

        tracing::info!(
            current_ledger,
            latest_externalized,
            target,
            "Starting externalized catchup"
        );

        // Start caching messages during catchup to capture tx_sets for gap ledgers
        let catchup_message_handle = self.start_catchup_message_caching_from_self().await;

        self.set_phase(14); // 14 = catchup_running
        let catchup_result = self.catchup(CatchupTarget::Ledger(target)).await;
        self.set_phase(11); // 11 = back in externalized_catchup

        // Stop the catchup message caching task
        if let Some(handle) = catchup_message_handle {
            handle.abort();
            tracing::debug!("Stopped catchup message caching task (externalized catchup)");
        }

        self.catchup_in_progress.store(false, Ordering::SeqCst);

        self.handle_catchup_result(catchup_result, false, "Externalized").await;
    }

    pub(super) fn buffered_catchup_target(
        current_ledger: u32,
        first_buffered: u32,
        last_buffered: u32,
    ) -> Option<u32> {
        if first_buffered <= current_ledger + 1 {
            return None;
        }

        let gap = first_buffered.saturating_sub(current_ledger);
        if gap >= CHECKPOINT_FREQUENCY {
            // When the gap is large enough to span a checkpoint boundary, target
            // the latest checkpoint before first_buffered. This ensures we catch
            // up to a known-good checkpoint state from the archive rather than
            // trying to replay a large number of ledgers.
            let target =
                latest_checkpoint_before_or_at(first_buffered.saturating_sub(1)).unwrap_or(0);
            return if target == 0 { None } else { Some(target) };
        }

        let required_first = if Self::is_first_ledger_in_checkpoint(first_buffered) {
            first_buffered
        } else {
            Self::first_ledger_in_checkpoint(first_buffered).saturating_add(CHECKPOINT_FREQUENCY)
        };
        let trigger = required_first.saturating_add(1);
        if last_buffered < trigger {
            return None;
        }
        let target = required_first.saturating_sub(1);
        if target == 0 {
            None
        } else {
            Some(target)
        }
    }

    /// Compute a catchup target when we're stuck waiting for buffered ledgers.
    /// This targets the checkpoint boundary that will allow us to apply buffered ledgers.
    /// Returns None if no published checkpoint is ahead of current_ledger, meaning
    /// the caller should either wait or query the archive for the latest checkpoint.
    pub(super) fn compute_catchup_target_for_timeout(
        last_buffered: u32,
        first_buffered: u32,
        current_ledger: u32,
    ) -> Option<u32> {
        // We need to catch up to a point that lets us make progress.
        // The best target is just before first_buffered, so we can then apply the buffered ledgers.

        // Find the checkpoint that contains first_buffered
        let first_buffered_checkpoint_start = Self::first_ledger_in_checkpoint(first_buffered);

        // Target should be the last ledger of the checkpoint BEFORE the one containing first_buffered
        // This is checkpoint_start - 1
        let target = if first_buffered_checkpoint_start > 0 {
            first_buffered_checkpoint_start.saturating_sub(1)
        } else {
            // first_buffered is in the first checkpoint, target first_buffered - 1
            first_buffered.saturating_sub(1)
        };

        // If target is not better than current_ledger, try targeting last_buffered's checkpoint
        if target <= current_ledger {
            let last_checkpoint_start = Self::first_ledger_in_checkpoint(last_buffered);
            let alt_target = last_checkpoint_start.saturating_sub(1);

            if alt_target > current_ledger {
                return Some(alt_target);
            }

            // No checkpoint target ahead of current_ledger.
            // For tiny gaps (e.g., LCL=922751, first_buffered=922753), target
            // first_buffered - 1 directly. This produces a Case 1 replay that
            // bridges the gap (e.g., replay 1 ledger from 922751 to 922752),
            // then the buffer starting at 922753 can drain.
            let direct_target = first_buffered.saturating_sub(1);
            if direct_target > current_ledger {
                return Some(direct_target);
            }

            // Truly no target ahead. Return None so the caller falls through
            // to CatchupTarget::Current, which queries the archive for the
            // latest published checkpoint.
            return None;
        }

        Some(target)
    }

    /// Reset tx_set tracking after catchup to give pending tx_sets a fresh chance.
    ///
    /// After catchup, the node's current_ledger has jumped significantly.
    /// Pending tx_set requests that were "DontHave" before catchup may now
    /// be available from peers (since those slots are now current, not future).
    /// Clearing the tracking allows fresh requests to all peers.
    async fn reset_tx_set_tracking_after_catchup(&self) {
        let mut dont_have = self.tx_set_dont_have.write().await;
        let cleared_dont_have = dont_have.len();
        dont_have.clear();
        drop(dont_have);

        let mut last_request = self.tx_set_last_request.write().await;
        let cleared_last_request = last_request.len();
        last_request.clear();
        drop(last_request);

        if cleared_dont_have > 0 || cleared_last_request > 0 {
            tracing::info!(
                cleared_dont_have,
                cleared_last_request,
                "Reset tx_set tracking after catchup"
            );
        }
    }
}
