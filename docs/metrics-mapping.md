# Metrics Mapping: Henyey ↔ Stellar-Core

This document maps henyey's Prometheus metrics to their stellar-core counterparts
and documents henyey-only metrics. All constants are defined in
`crates/app/src/metrics.rs`.

**Dashboards:** [`docs/henyey-monitoring.json`](henyey-monitoring.json) (21-panel overview,
10603 equivalent) | [`docs/henyey-monitoring-full.json`](henyey-monitoring-full.json) (full
drill-down, 10334 equivalent).

## 1. Exact Stellar-Core Equivalents

Metrics with the `stellar_` prefix that directly mirror stellar-core Medida counters/timers/gauges.

| Henyey constant | Prometheus name | Type | Stellar-core source |
|---|---|---|---|
| `LEDGER_SEQUENCE` | `stellar_ledger_sequence` | gauge | `LedgerManagerImpl::mLedgerNum` (`LedgerManager.h`) |
| `PEER_COUNT` | `stellar_peer_count` | gauge | `OverlayManagerImpl::mPeerCount` (`OverlayManager.cpp`) |
| `PENDING_TRANSACTIONS` | `stellar_pending_transactions` | gauge | `HerderImpl::mTransactionQueueSize` (`Herder.cpp`) |
| `UPTIME_SECONDS` | `stellar_uptime_seconds` | gauge | Process uptime |
| `IS_VALIDATOR` | `stellar_is_validator` | gauge | Node validator status |
| `STARTED_ON` | `stellar_started_on` | gauge | Unix epoch of process start (`mStartedOn = time(nullptr)`, `ApplicationImpl.cpp:87`) |
| `LEDGER_VERSION` | `stellar_ledger_version` | gauge | Current ledger header `ledgerVersion` (`LedgerHeaderHistoryEntry.header.ledgerVersion`) |
| `PROTOCOL_VERSION` | `stellar_protocol_version` | gauge | Configured max protocol version (`getConfig().LEDGER_PROTOCOL_VERSION`, `ApplicationImpl.cpp:469`) |
| `LEDGER_TX_COUNT` | `stellar_ledger_tx_count` | gauge | `LedgerCloseData.mStats.mTxCount` |
| `LEDGER_OP_COUNT` | `stellar_ledger_op_count` | gauge | `LedgerCloseData.mStats.mOpCount` |
| `LEDGER_TX_SUCCESS_COUNT` | `stellar_ledger_tx_success_count` | gauge | `LedgerCloseData.mStats.mTxSuccessCount` |
| `LEDGER_TX_FAILED_COUNT` | `stellar_ledger_tx_failed_count` | gauge | `LedgerCloseData.mStats.mTxFailedCount` |
| `LEDGER_TOTAL_FEES` | `stellar_ledger_total_fees` | gauge | `LedgerCloseData.mStats.mTotalFees` |
| `LEDGER_ENTRIES_CREATED` | `stellar_ledger_entries_created` | gauge | `LedgerCloseData.mStats.mNewEntries` |
| `LEDGER_ENTRIES_UPDATED` | `stellar_ledger_entries_updated` | gauge | `LedgerCloseData.mStats.mUpdatedEntries` |
| `LEDGER_ENTRIES_DELETED` | `stellar_ledger_entries_deleted` | gauge | `LedgerCloseData.mStats.mDeletedEntries` |
| `LEDGER_APPLY_US` | `stellar_ledger_apply_us` | gauge | `LedgerManagerImpl::closeLedger` total μs (`perf.total_us`) |
| `LEDGER_AGE_CURRENT_SECONDS` | `stellar_ledger_age_current_seconds` | gauge | Seconds since last close |
| `HERDER_STATE` | `stellar_herder_state` | gauge | `HerderImpl::mState` |
| `HERDER_PENDING_ENVELOPES` | `stellar_herder_pending_envelopes` | gauge | `PendingEnvelopes` count |
| `HERDER_CACHED_TX_SETS` | `stellar_herder_cached_tx_sets` | gauge | Cached transaction set count |
| `HERDER_TX_QUEUE_ACCOUNTS` | `stellar_herder_tx_queue_accounts` | gauge | `TransactionQueue::mAccountStates` size |
| `HERDER_TX_QUEUE_BANNED` | `stellar_herder_tx_queue_banned` | gauge | `TransactionQueue::mBannedTransactions` size |
| `HERDER_TX_QUEUE_SEEN` | `stellar_herder_tx_queue_seen` | gauge | `TransactionQueue::mKnownTxHashes` size |
| `HERDER_PENDING_TXS_AGE0` | `stellar_herder_pending_txs_age0` | gauge | Pending txs at age 0 (current slot) |
| `HERDER_PENDING_TXS_AGE1` | `stellar_herder_pending_txs_age1` | gauge | Pending txs at age 1 |
| `HERDER_PENDING_TXS_AGE2` | `stellar_herder_pending_txs_age2` | gauge | Pending txs at age 2 |
| `HERDER_PENDING_TXS_AGE3` | `stellar_herder_pending_txs_age3` | gauge | Pending txs at age 3+ |
| `HERDER_LOST_SYNC_TOTAL` | `stellar_herder_lost_sync_total` | counter | `HerderImpl::mLostSync` |
| `HERDER_PENDING_RECEIVED_TOTAL` | `stellar_herder_pending_received_total` | counter | Envelopes received |
| `HERDER_PENDING_DUPLICATES_TOTAL` | `stellar_herder_pending_duplicates_total` | counter | Duplicate envelopes |
| `HERDER_PENDING_TOO_OLD_TOTAL` | `stellar_herder_pending_too_old_total` | counter | Envelopes rejected as too old |
| `HERDER_PENDING_EVICTED_TOTAL` | `stellar_herder_pending_evicted_total` | counter | Evicted pending envelopes |
| `HERDER_PENDING_ADDED_TOTAL` | `stellar_herder_pending_added_total` | counter | Envelopes added to pending |
| `HERDER_PENDING_RELEASED_TOTAL` | `stellar_herder_pending_released_total` | counter | Envelopes released from pending |
| `SCP_ENVELOPE_EMIT_TOTAL` | `stellar_scp_envelope_emit_total` | counter | `SCPDriver::mEnvelopeEmit` |
| `SCP_ENVELOPE_RECEIVE_TOTAL` | `stellar_scp_envelope_receive_total` | counter | `SCPDriver::mEnvelopeReceive` |
| `SCP_ENVELOPE_SIGN_TOTAL` | `stellar_scp_envelope_sign_total` | counter | `SCPDriver::mEnvelopeSign` — counts successful envelope signings. |
| `SCP_ENVELOPE_VALIDSIG_TOTAL` | `stellar_scp_envelope_validsig_total` | counter | `SCPDriver::mEnvelopeValidSig` — signature verifications that pass. |
| `SCP_ENVELOPE_INVALIDSIG_TOTAL` | `stellar_scp_envelope_invalidsig_total` | counter | `SCPDriver::mEnvelopeInvalidSig` — signature verifications that fail. |
| `SCP_VALUE_VALID_TOTAL` | `stellar_scp_value_valid_total` | counter | `SCPDriver::mValueValid` — value validations that succeed. |
| `SCP_VALUE_INVALID_TOTAL` | `stellar_scp_value_invalid_total` | counter | `SCPDriver::mValueInvalid` — value validations that fail (includes FullyValidated deferred path). |
| `SCP_NOMINATION_COMBINECANDIDATES_TOTAL` | `stellar_scp_nomination_combinecandidates_total` | counter | `SCPDriver::mCombinedCandidates` — number of candidates passed to combine_candidates. |
| `SCP_TIMEOUT_NOMINATE_TOTAL` | `stellar_scp_timeout_nominate_total` | counter | `SCPDriver::mNominateTimeout` — fires of the nomination timer. |
| `SCP_TIMEOUT_PREPARE_TOTAL` | `stellar_scp_timeout_prepare_total` | counter | `SCPDriver::mPrepareTimeout` — fires of the ballot timer. |
| `SCP_PHASE` | `stellar_scp_phase` | gauge | `HerderImpl::mSCPPhase` — tracking slot ballot phase (0=unknown, 1=prepare, 2=confirm, 3=externalize). |
| `SCP_MEMORY_CUMULATIVE_STATEMENTS` | `stellar_scp_memory_cumulative_statements` | gauge | `SCP::mCumulativeStatements` — total statements stored across all slots. |
| `OVERLAY_MESSAGE_READ_TOTAL` | `stellar_overlay_message_read_total` | counter | `OverlayManagerImpl::mMessageRead` |
| `OVERLAY_MESSAGE_WRITE_TOTAL` | `stellar_overlay_message_write_total` | counter | `OverlayManagerImpl::mMessageWrite` |
| `OVERLAY_MESSAGE_BROADCAST_TOTAL` | `stellar_overlay_message_broadcast_total` | counter | `OverlayManagerImpl::mMessagesBroadcast` |
| `OVERLAY_ERROR_READ_TOTAL` | `stellar_overlay_error_read_total` | counter | `OverlayManagerImpl::mErrorRead` |
| `OVERLAY_ERROR_WRITE_TOTAL` | `stellar_overlay_error_write_total` | counter | `OverlayManagerImpl::mErrorWrite` |
| `OVERLAY_TIMEOUT_IDLE_TOTAL` | `stellar_overlay_timeout_idle_total` | counter | `OverlayManagerImpl::mTimeoutIdle` |
| `OVERLAY_TIMEOUT_STRAGGLER_TOTAL` | `stellar_overlay_timeout_straggler_total` | counter | `OverlayManagerImpl::mTimeoutStraggler` |
| `OVERLAY_INBOUND_AUTHENTICATED` | `stellar_overlay_inbound_authenticated` | gauge | Authenticated inbound peer count |
| `OVERLAY_OUTBOUND_AUTHENTICATED` | `stellar_overlay_outbound_authenticated` | gauge | Authenticated outbound peer count |
| `OVERLAY_INBOUND_PENDING` | `stellar_overlay_inbound_pending` | gauge | Pending inbound connections |
| `OVERLAY_OUTBOUND_PENDING` | `stellar_overlay_outbound_pending` | gauge | Pending outbound connections |
| `BUCKET_MERGE_COMPLETED_TOTAL` | `stellar_bucket_merge_completed_total` | counter | `BucketManager::mMergesCompleted` |
| `BUCKET_MERGE_TIME_US_TOTAL` | `stellar_bucket_merge_time_us_total` | counter | Cumulative merge time μs |
| `BUCKET_MERGE_NEW_LIVE_TOTAL` | `stellar_bucket_merge_new_live_total` | counter | Live entries produced by merges |
| `BUCKET_MERGE_NEW_DEAD_TOTAL` | `stellar_bucket_merge_new_dead_total` | counter | Dead entries produced by merges |
| `BUCKET_MERGE_NEW_INIT_TOTAL` | `stellar_bucket_merge_new_init_total` | counter | Init entries produced by merges |
| `BUCKET_MERGE_NEW_META_TOTAL` | `stellar_bucket_merge_new_meta_total` | counter | Meta entries produced by merges |
| `BUCKET_MERGE_SHADOWED_TOTAL` | `stellar_bucket_merge_shadowed_total` | counter | Entries shadowed during merges |
| `BUCKET_MERGE_ANNIHILATED_TOTAL` | `stellar_bucket_merge_annihilated_total` | counter | Entries annihilated during merges |
| `META_STREAM_BYTES_TOTAL` | `stellar_meta_stream_bytes_total` | counter | Bytes written to meta stream |
| `META_STREAM_WRITES_TOTAL` | `stellar_meta_stream_writes_total` | counter | Write operations to meta stream |
| `QUORUM_AGREE` | `stellar_quorum_agree` | gauge | Nodes agreeing with latest slot |
| `QUORUM_MISSING` | `stellar_quorum_missing` | gauge | Nodes missing from latest slot |
| `QUORUM_DISAGREE` | `stellar_quorum_disagree` | gauge | Nodes disagreeing in latest slot |
| `QUORUM_FAIL_AT` | `stellar_quorum_fail_at` | gauge | Peers that can fail before quorum loss (via `find_closest_v_blocking`, excludes self) |
| `QUORUM_DELAYED` | `stellar_quorum_delayed` | gauge | Quorum set nodes lagging behind the local node (subset of agree) |
| `LEDGER_INVARIANT_FAILURE_TOTAL` | `stellar_ledger_invariant_failure_total` | counter | `InvariantManagerImpl::handleInvariantFailure` |
| `LEDGER_TRANSACTION_INTERNAL_ERROR_TOTAL` | `stellar_ledger_transaction_internal_error_total` | counter | `TransactionFrame::apply` txINTERNAL_ERROR path |

## 2. Derived / Approximate Equivalents

Metrics inspired by stellar-core but with different type, unit, or granularity.

| Henyey constant | Prometheus name | Type | Notes |
|---|---|---|---|
| `LEDGER_CLOSE_DURATION_SECONDS` | `stellar_ledger_close_duration_seconds` | histogram | Derived from `stats.close_time_ms`. Histogram (seconds) vs stellar-core's gauge (ms). |
| `SCP_TIMING_EXTERNALIZED_SECONDS` | `stellar_scp_timing_externalized_seconds` | histogram | SCP externalization latency — histogram in henyey vs timer in stellar-core. |
| `SCP_TIMING_NOMINATED_SECONDS` | `stellar_scp_timing_nominated_seconds` | gauge | SCP nomination phase duration (nomination start → ballot protocol start). Matches stellar-core's `mNominateToPrepare`. |
| `LEDGER_APPLY_SUCCESS_TOTAL` | `stellar_ledger_apply_success_total` | counter | Cumulative successful tx applies — monotonic counter vs per-close gauge in stellar-core. |
| `LEDGER_APPLY_FAILURE_TOTAL` | `stellar_ledger_apply_failure_total` | counter | Cumulative failed tx applies. |
| `LEDGER_APPLY_SOROBAN_SUCCESS_TOTAL` | `stellar_ledger_apply_soroban_success_total` | counter | Cumulative successful Soroban tx applies. |
| `LEDGER_APPLY_SOROBAN_FAILURE_TOTAL` | `stellar_ledger_apply_soroban_failure_total` | counter | Cumulative failed Soroban tx applies. |
| `LEDGER_APPLY_SOROBAN_STAGES` | `stellar_ledger_apply_soroban_stages` | gauge | Parallel execution stage count (last close). |
| `LEDGER_APPLY_SOROBAN_MAX_CLUSTERS` | `stellar_ledger_apply_soroban_max_clusters` | gauge | Max cluster count across stages (last close). |
| `SOROBAN_CONFIG_*` | `stellar_soroban_config_*` | gauge | Soroban network configuration parameters (tx limits, ledger limits, fees). 20 metrics total. |
| `LEDGER_AGE_CLOSED_SECONDS` | `stellar_ledger_age_closed_seconds` | histogram | Same semantics as stellar-core (wall-clock age at close), expanded buckets (1-60s vs core's 5/7/10/20s). |
| `LEDGER_OPERATION_APPLY_SECONDS` | `stellar_ledger_operation_apply_seconds` | histogram | Broader scope than core: full op cycle (load+exec+invariant) vs core's exec-only. |
| `LEDGER_TRANSACTION_APPLY_SECONDS` | `stellar_ledger_transaction_apply_seconds` | histogram | Same scope as core: ops+meta, excludes validation/fees/footprint. |
| `LEDGER_MEMORY_QUEUED_LEDGERS` | `stellar_ledger_memory_queued_ledgers` | gauge | Measures `syncing_ledgers` depth (externalized slots waiting to close). |
| `LEDGER_CATCHUP_DURATION_SECONDS` | `stellar_ledger_catchup_duration_seconds` | histogram | Wall-clock catchup duration. Records successful completions only. |

## 3. Henyey-Only Metrics

Metrics unique to henyey with no stellar-core counterpart.

### Ledger Close Phase Histograms

Fine-grained breakdown of ledger close phases, all in seconds.

| Constant | Prometheus name | Description |
|---|---|---|
| `CLOSE_BEGIN_SECONDS` | `henyey_ledger_close_begin_seconds` | Begin-close phase (setup, fee computation) |
| `CLOSE_TX_EXEC_SECONDS` | `henyey_ledger_close_tx_exec_seconds` | Full transaction execution phase |
| `CLOSE_CLASSIC_EXEC_SECONDS` | `henyey_ledger_close_classic_exec_seconds` | Classic transaction execution |
| `CLOSE_SOROBAN_EXEC_SECONDS` | `henyey_ledger_close_soroban_exec_seconds` | Soroban contract execution |
| `CLOSE_COMMIT_SETUP_SECONDS` | `henyey_ledger_close_commit_setup_seconds` | Commit setup phase |
| `CLOSE_BUCKET_LOCK_WAIT_SECONDS` | `henyey_ledger_close_bucket_lock_wait_seconds` | Bucket lock acquisition wait |
| `CLOSE_EVICTION_SECONDS` | `henyey_ledger_close_eviction_seconds` | State eviction phase |
| `CLOSE_SOROBAN_STATE_SECONDS` | `henyey_ledger_close_soroban_state_seconds` | Soroban state archival phase |
| `CLOSE_BUCKET_ADD_SECONDS` | `henyey_ledger_close_bucket_add_seconds` | Bucket list addition |
| `CLOSE_HOT_ARCHIVE_SECONDS` | `henyey_ledger_close_hot_archive_seconds` | Hot archive update |
| `CLOSE_HEADER_SECONDS` | `henyey_ledger_close_header_seconds` | Header computation |
| `CLOSE_COMMIT_SECONDS` | `henyey_ledger_close_commit_seconds` | Database commit |
| `CLOSE_META_SECONDS` | `henyey_ledger_close_meta_seconds` | Meta emission |

### Post-Close Event-Loop Histograms

| Constant | Prometheus name | Description |
|---|---|---|
| `CLOSE_COMPLETE_JOIN_MATCH_SECONDS` | `henyey_ledger_close_complete_join_match_seconds` | spawn_blocking join + result matching |
| `CLOSE_COMPLETE_META_EMIT_SECONDS` | `henyey_ledger_close_complete_meta_emit_seconds` | Meta stream emission |
| `CLOSE_COMPLETE_BUILD_PERSIST_INPUTS_SECONDS` | `henyey_ledger_close_complete_build_persist_inputs_seconds` | Persist input preparation |
| `CLOSE_COMPLETE_OVERLAY_BOOKKEEPING_SECONDS` | `henyey_ledger_close_complete_overlay_bookkeeping_seconds` | Overlay state updates |
| `CLOSE_COMPLETE_SPAWN_BLOCKING_SETUP_SECONDS` | `henyey_ledger_close_complete_spawn_blocking_setup_seconds` | spawn_blocking setup |
| `CLOSE_COMPLETE_TX_QUEUE_SECONDS` | `henyey_ledger_close_complete_tx_queue_seconds` | Transaction queue bookkeeping |
| `CLOSE_COMPLETE_POST_CLOSE_BOOKKEEPING_SECONDS` | `henyey_ledger_close_complete_post_close_bookkeeping_seconds` | Post-close lifecycle work |

### Transaction Queue Sub-Phase Histograms

| Constant | Prometheus name | Description |
|---|---|---|
| `CLOSE_TX_QUEUE_PREP_SECONDS` | `henyey_ledger_close_tx_queue_prep_seconds` | Queue update preparation |
| `CLOSE_TX_QUEUE_LEDGER_CLOSED_SECONDS` | `henyey_ledger_close_tx_queue_ledger_closed_seconds` | Ledger-closed notification processing |
| `CLOSE_TX_QUEUE_SHIFT_UPDATE_SECONDS` | `henyey_ledger_close_tx_queue_shift_update_seconds` | Sequence number shift updates |
| `CLOSE_TX_QUEUE_SNAPSHOT_SECONDS` | `henyey_ledger_close_tx_queue_snapshot_seconds` | Queue snapshot creation |
| `CLOSE_TX_QUEUE_ENVELOPES_FETCH_SECONDS` | `henyey_ledger_close_tx_queue_envelopes_fetch_seconds` | Envelope fetch for nomination |
| `CLOSE_TX_QUEUE_SNAPSHOT_BUILD_SECONDS` | `henyey_ledger_close_tx_queue_snapshot_build_seconds` | Snapshot build phase |
| `CLOSE_TX_QUEUE_INVALIDATION_SECONDS` | `henyey_ledger_close_tx_queue_invalidation_seconds` | Tx invalidation processing |

### Close-Cycle Decomposition

| Constant | Prometheus name | Description |
|---|---|---|
| `CLOSE_CYCLE_SECONDS` | `henyey_ledger_close_cycle_seconds` | Full close cycle (close-to-close) |
| `CLOSE_HANDLE_COMPLETE_SECONDS` | `henyey_ledger_close_handle_complete_seconds` | Close-complete handler duration |
| `CLOSE_POST_COMPLETE_SECONDS` | `henyey_ledger_close_post_complete_seconds` | Post-complete lifecycle work |
| `CLOSE_DISPATCH_TO_JOIN_SECONDS` | `henyey_ledger_close_dispatch_to_join_seconds` | Close spawn_blocking dispatch-to-join |
| `PERSIST_DISPATCH_TO_JOIN_SECONDS` | `henyey_ledger_persist_dispatch_to_join_seconds` | Persist spawn_blocking dispatch-to-join |
| `PERSIST_LEDGER_CLOSE_SECONDS` | `henyey_ledger_persist_close_seconds` | Ledger persist (DB write) duration |
| `SLOT_TO_CLOSE_LATENCY_SECONDS` | `henyey_ledger_slot_to_close_latency_seconds` | Slot externalization to close-start latency |

### Cache Metrics

| Constant | Prometheus name | Description |
|---|---|---|
| `LEDGER_BUCKET_CACHE_HIT_RATIO` | `henyey_ledger_bucket_cache_hit_ratio` | Per-bucket RandomEvictionCache hit ratio (0.0–1.0, last close) |
| `LEDGER_SNAPSHOT_CACHE_HIT_RATIO` | `henyey_ledger_snapshot_cache_hit_ratio` | SnapshotHandle local cache hit ratio (0.0–1.0, last close) |
| `LEDGER_SNAPSHOT_CACHE_FALLBACK_LOOKUPS` | `henyey_ledger_snapshot_cache_fallback_lookups` | Lookups dispatched to fallback (last close) |

### SCP Verification Pipeline

| Constant | Prometheus name | Description |
|---|---|---|
| `SCP_PREFILTER_REJECTS_TOTAL` | `henyey_scp_prefilter_rejects_total` | Envelopes rejected by prefilter (labeled by `reason`) |
| `SCP_POST_VERIFY_DROPS_TOTAL` | `henyey_scp_post_verify_drops_total` | Envelopes dropped post-verification |
| `SCP_POST_VERIFY_TOTAL` | `henyey_scp_post_verify_total` | Envelopes processed post-verification (labeled by `reason`) |
| `SCP_VERIFY_INPUT_BACKLOG` | `henyey_scp_verify_input_backlog` | Verification input queue depth |
| `SCP_VERIFY_OUTPUT_BACKLOG` | `henyey_scp_verify_output_backlog` | Verification output queue depth |
| `SCP_VERIFIER_THREAD_STATE` | `henyey_scp_verifier_thread_state` | Verifier thread state |
| `SCP_VERIFY_LATENCY_US_SUM` | `henyey_scp_verify_latency_us_sum` | Cumulative verification latency (μs) |
| `SCP_VERIFY_LATENCY_US_COUNT` | `henyey_scp_verify_latency_us_count` | Verification count |

### Clock Drift

| Constant | Prometheus name | Description |
|---|---|---|
| `DRIFT_MIN_SECONDS` | `henyey_herder_drift_min_seconds` | Minimum observed clock drift |
| `DRIFT_MAX_SECONDS` | `henyey_herder_drift_max_seconds` | Maximum observed clock drift |
| `DRIFT_MEDIAN_SECONDS` | `henyey_herder_drift_median_seconds` | Median clock drift |
| `DRIFT_P75_SECONDS` | `henyey_herder_drift_p75_seconds` | 75th percentile clock drift |
| `DRIFT_SAMPLE_COUNT` | `henyey_herder_drift_sample_count` | Drift sample count in window |

### Memory (jemalloc)

| Constant | Prometheus name | Description |
|---|---|---|
| `JEMALLOC_ALLOCATED_BYTES` | `henyey_jemalloc_allocated_bytes` | Allocated bytes |
| `JEMALLOC_ACTIVE_BYTES` | `henyey_jemalloc_active_bytes` | Active bytes |
| `JEMALLOC_RESIDENT_BYTES` | `henyey_jemalloc_resident_bytes` | Resident set size |
| `JEMALLOC_MAPPED_BYTES` | `henyey_jemalloc_mapped_bytes` | Mapped bytes |
| `JEMALLOC_RETAINED_BYTES` | `henyey_jemalloc_retained_bytes` | Retained bytes |
| `JEMALLOC_FRAGMENTATION_PCT` | `henyey_jemalloc_fragmentation_pct` | Fragmentation percentage |

### Process Health

| Constant | Prometheus name | Description |
|---|---|---|
| `PROCESS_OPEN_FDS` | `henyey_process_open_fds` | Open file descriptors |
| `PROCESS_MAX_FDS` | `henyey_process_max_fds` | Max file descriptor limit |

### Overlay Internals

| Constant | Prometheus name | Description |
|---|---|---|
| `OVERLAY_FETCH_CHANNEL_DEPTH` | `henyey_overlay_fetch_channel_depth` | Fetch-response channel depth |
| `OVERLAY_FETCH_CHANNEL_DEPTH_MAX` | `henyey_overlay_fetch_channel_depth_max` | Max observed channel depth |

### Archive Cache

| Constant | Prometheus name | Description |
|---|---|---|
| `ARCHIVE_CACHE_FRESH_TOTAL` | `henyey_archive_cache_fresh_total` | Fresh archive cache lookups |
| `ARCHIVE_CACHE_STALE_TOTAL` | `henyey_archive_cache_stale_total` | Stale cache lookups triggering refresh |
| `ARCHIVE_CACHE_COLD_TOTAL` | `henyey_archive_cache_cold_total` | Cold (miss) lookups |
| `ARCHIVE_CACHE_REFRESH_SUCCESS_TOTAL` | `henyey_archive_cache_refresh_success_total` | Successful refresh operations |
| `ARCHIVE_CACHE_REFRESH_ERROR_TOTAL` | `henyey_archive_cache_refresh_error_total` | Failed refresh operations |
| `ARCHIVE_CACHE_REFRESH_TIMEOUT_TOTAL` | `henyey_archive_cache_refresh_timeout_total` | Timed-out refresh operations |
| `ARCHIVE_CACHE_REFRESH_DURATION_SECONDS` | `henyey_archive_cache_refresh_duration_seconds` | Refresh duration histogram |
| `ARCHIVE_CACHE_AGE_SECONDS` | `henyey_archive_cache_age_seconds` | Age of cached archive state |
| `ARCHIVE_CACHE_POPULATED` | `henyey_archive_cache_populated` | Whether cache is populated (0/1) |

### Recovery

| Constant | Prometheus name | Description |
|---|---|---|
| `POST_CATCHUP_HARD_RESET_TOTAL` | `henyey_post_catchup_hard_reset_total` | Hard resets during post-catchup recovery |
| `RECOVERY_STALLED_TICK_TOTAL` | `henyey_recovery_stalled_tick_total` | Stalled recovery tick count |
| `RECOVERY_TX_SET_STUCK_SECONDS` | `henyey_recovery_tx_set_stuck_seconds` | Time stuck waiting for tx set |

## 4. Removed Metrics

The following metrics were removed as redundant (see issue #1927):

| Former metric | Replaced by | Reason |
|---|---|---|
| `henyey_ledger_soroban_exec_us` | `henyey_ledger_close_soroban_exec_seconds` (histogram) | Same source data; histogram provides distribution. |
| `henyey_ledger_classic_exec_us` | `henyey_ledger_close_classic_exec_seconds` (histogram) | Same source data; histogram provides distribution. |
| `stellar_ledger_close_time_ms` | `stellar_ledger_close_duration_seconds` (histogram) | Same source data (`stats.close_time_ms`); histogram provides distribution. |

## 5. Not Applicable Stellar-Core Metrics

The following stellar-core metrics have no henyey equivalent and are intentionally omitted.
They reference concepts or architecture that don't exist in henyey.

| Stellar-core metric | Reason not applicable |
|---|---|
| `stellar_core_quorum_transitive_intersection` | Stellar-core's periodic transitive quorum intersection check is not implemented in henyey. |
| `stellar_core_quorum_transitive_last_check_ledger` | Same as above — no transitive quorum check. |
| `stellar_core_bucket_memory_shared` | Stellar-core's in-memory bucket sharing model differs from henyey's bucket cache architecture. |
| `stellar_core_app_post_on_main_thread_delay_seconds` | Stellar-core's queue-backed main thread; henyey uses tokio async runtime instead. |
| `stellar_core_app_post_on_background_thread_delay_seconds` | Same as above — no background thread queue. |
| `stellar_core_crypto_verify_hit` / `_total` | Stellar-core's signature verification cache; henyey has no signature cache (SCP verify pipeline uses `henyey_scp_verify_*` metrics). |
| `stellar_core_quorum_transitive_critical` | Transitive quorum critical node analysis not implemented in henyey. |
| `stellar_core_quorum_transitive_node_count` | Transitive quorum node count not implemented. |
| `stellar_core_database_{operation}_seconds` (73 metrics) | Per-SOCI-statement timings (e.g., `select_account`, `insert_ledgerheader`, `commit`). Henyey uses rusqlite directly without per-statement instrumentation. |
| `stellar_core_overlay_{message_type}_seconds` (24 metrics) | Per-message-type overlay wire timing. Henyey does not expose per-message-type duration metrics; deferred pending operator demand. |
| `stellar_core_app_post_on_main_thread_with_delay_delay_seconds` | Same as `app_post_on_main_thread_delay` — tokio async runtime. |
