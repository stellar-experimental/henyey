//! Prometheus metrics facade for the henyey node.
//!
//! This module is the single source of truth for all metric names, types,
//! descriptions, and scrape-time refresh logic. It wraps the [`metrics`] crate
//! facade to provide:
//!
//! - Centralized metric name constants
//! - One-time HELP/TYPE registration via [`describe_metrics`]
//! - Pre-registration of labeled counter series at zero via [`register_label_series`]
//! - Scrape-time gauge refresh via [`refresh_gauges`]
//! - A once-per-process test helper via [`ensure_test_recorder`]

use std::sync::OnceLock;

use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

use crate::http::ServerState;

// ── Metric name constants ──────────────────────────────────────────────

// Stellar-compatible names (backward compatibility with existing dashboards).
pub const LEDGER_SEQUENCE: &str = "stellar_ledger_sequence";
pub const PEER_COUNT: &str = "stellar_peer_count";
pub const PENDING_TRANSACTIONS: &str = "stellar_pending_transactions";
pub const UPTIME_SECONDS: &str = "stellar_uptime_seconds";
pub const IS_VALIDATOR: &str = "stellar_is_validator";
pub const META_STREAM_BYTES_TOTAL: &str = "stellar_meta_stream_bytes_total";
pub const META_STREAM_WRITES_TOTAL: &str = "stellar_meta_stream_writes_total";

// Henyey-specific metrics.
pub const SCP_PREFILTER_REJECTS_TOTAL: &str = "henyey_scp_prefilter_rejects_total";
pub const SCP_POST_VERIFY_DROPS_TOTAL: &str = "henyey_scp_post_verify_drops_total";
pub const SCP_POST_VERIFY_TOTAL: &str = "henyey_scp_post_verify_total";
pub const SCP_VERIFY_INPUT_BACKLOG: &str = "henyey_scp_verify_input_backlog";
pub const SCP_VERIFY_OUTPUT_BACKLOG: &str = "henyey_scp_verify_output_backlog";
pub const SCP_VERIFIER_THREAD_STATE: &str = "henyey_scp_verifier_thread_state";
pub const SCP_VERIFY_LATENCY_US_SUM: &str = "henyey_scp_verify_latency_us_sum";
pub const SCP_VERIFY_LATENCY_US_COUNT: &str = "henyey_scp_verify_latency_us_count";
pub const OVERLAY_FETCH_CHANNEL_DEPTH: &str = "henyey_overlay_fetch_channel_depth";
pub const OVERLAY_FETCH_CHANNEL_DEPTH_MAX: &str = "henyey_overlay_fetch_channel_depth_max";
pub const POST_CATCHUP_HARD_RESET_TOTAL: &str = "henyey_post_catchup_hard_reset_total";

// SCP/herder counters (Phase 1 — expose already-tracked App-level counters).
pub const SCP_ENVELOPE_EMIT_TOTAL: &str = "stellar_scp_envelope_emit_total";
pub const SCP_ENVELOPE_RECEIVE_TOTAL: &str = "stellar_scp_envelope_receive_total";
pub const HERDER_LOST_SYNC_TOTAL: &str = "stellar_herder_lost_sync_total";
pub const HERDER_STATE: &str = "stellar_herder_state";
pub const HERDER_PENDING_ENVELOPES: &str = "stellar_herder_pending_envelopes";
pub const HERDER_CACHED_TX_SETS: &str = "stellar_herder_cached_tx_sets";
pub const HERDER_PENDING_RECEIVED_TOTAL: &str = "stellar_herder_pending_received_total";
pub const HERDER_PENDING_DUPLICATES_TOTAL: &str = "stellar_herder_pending_duplicates_total";
pub const HERDER_PENDING_TOO_OLD_TOTAL: &str = "stellar_herder_pending_too_old_total";
pub const HERDER_PENDING_EVICTED_TOTAL: &str = "stellar_herder_pending_evicted_total";

// Bucket merge counters (Phase 1 — live MergeCounters from BucketList).
pub const BUCKET_MERGE_COMPLETED_TOTAL: &str = "stellar_bucket_merge_completed_total";
pub const BUCKET_MERGE_TIME_US_TOTAL: &str = "stellar_bucket_merge_time_us_total";
pub const BUCKET_MERGE_NEW_LIVE_TOTAL: &str = "stellar_bucket_merge_new_live_total";
pub const BUCKET_MERGE_NEW_DEAD_TOTAL: &str = "stellar_bucket_merge_new_dead_total";
pub const BUCKET_MERGE_NEW_INIT_TOTAL: &str = "stellar_bucket_merge_new_init_total";
pub const BUCKET_MERGE_NEW_META_TOTAL: &str = "stellar_bucket_merge_new_meta_total";
pub const BUCKET_MERGE_SHADOWED_TOTAL: &str = "stellar_bucket_merge_shadowed_total";
pub const BUCKET_MERGE_ANNIHILATED_TOTAL: &str = "stellar_bucket_merge_annihilated_total";

// Overlay counters (Phase 1 — wired to OverlayMetrics in OverlayManager).
pub const OVERLAY_MESSAGE_READ_TOTAL: &str = "stellar_overlay_message_read_total";
pub const OVERLAY_MESSAGE_WRITE_TOTAL: &str = "stellar_overlay_message_write_total";
pub const OVERLAY_MESSAGE_BROADCAST_TOTAL: &str = "stellar_overlay_message_broadcast_total";
pub const OVERLAY_ERROR_READ_TOTAL: &str = "stellar_overlay_error_read_total";
pub const OVERLAY_ERROR_WRITE_TOTAL: &str = "stellar_overlay_error_write_total";
pub const OVERLAY_TIMEOUT_IDLE_TOTAL: &str = "stellar_overlay_timeout_idle_total";
pub const OVERLAY_TIMEOUT_STRAGGLER_TOTAL: &str = "stellar_overlay_timeout_straggler_total";

// Ledger close metrics (Phase 2 — per-ledger snapshot gauges).
pub const LEDGER_CLOSE_TIME_MS: &str = "stellar_ledger_close_time_ms";
pub const LEDGER_TX_COUNT: &str = "stellar_ledger_tx_count";
pub const LEDGER_OP_COUNT: &str = "stellar_ledger_op_count";
pub const LEDGER_TX_SUCCESS_COUNT: &str = "stellar_ledger_tx_success_count";
pub const LEDGER_TX_FAILED_COUNT: &str = "stellar_ledger_tx_failed_count";
pub const LEDGER_TOTAL_FEES: &str = "stellar_ledger_total_fees";
pub const LEDGER_ENTRIES_CREATED: &str = "stellar_ledger_entries_created";
pub const LEDGER_ENTRIES_UPDATED: &str = "stellar_ledger_entries_updated";
pub const LEDGER_ENTRIES_DELETED: &str = "stellar_ledger_entries_deleted";
pub const LEDGER_APPLY_US: &str = "stellar_ledger_apply_us";

// Herder tx queue metrics (Phase 2 — from TxQueueStats).
pub const HERDER_TX_QUEUE_ACCOUNTS: &str = "stellar_herder_tx_queue_accounts";
pub const HERDER_TX_QUEUE_BANNED: &str = "stellar_herder_tx_queue_banned";
pub const HERDER_TX_QUEUE_SEEN: &str = "stellar_herder_tx_queue_seen";

// Herder pending envelope metrics (Phase 2 — completing the set).
pub const HERDER_PENDING_ADDED_TOTAL: &str = "stellar_herder_pending_added_total";
pub const HERDER_PENDING_RELEASED_TOTAL: &str = "stellar_herder_pending_released_total";

// Clock drift metrics (Phase 2 — from CloseTimeDriftTracker).
pub const DRIFT_MIN_SECONDS: &str = "henyey_herder_drift_min_seconds";
pub const DRIFT_MAX_SECONDS: &str = "henyey_herder_drift_max_seconds";
pub const DRIFT_MEDIAN_SECONDS: &str = "henyey_herder_drift_median_seconds";
pub const DRIFT_P75_SECONDS: &str = "henyey_herder_drift_p75_seconds";
pub const DRIFT_SAMPLE_COUNT: &str = "henyey_herder_drift_sample_count";

// jemalloc allocator metrics — scrape-time gauges from AllocatorStats.
pub const JEMALLOC_ALLOCATED_BYTES: &str = "henyey_jemalloc_allocated_bytes";
pub const JEMALLOC_ACTIVE_BYTES: &str = "henyey_jemalloc_active_bytes";
pub const JEMALLOC_RESIDENT_BYTES: &str = "henyey_jemalloc_resident_bytes";
pub const JEMALLOC_MAPPED_BYTES: &str = "henyey_jemalloc_mapped_bytes";
pub const JEMALLOC_RETAINED_BYTES: &str = "henyey_jemalloc_retained_bytes";
pub const JEMALLOC_FRAGMENTATION_PCT: &str = "henyey_jemalloc_fragmentation_pct";

// Phase 3 — Ledger apply cumulative counters.
pub const LEDGER_APPLY_SUCCESS_TOTAL: &str = "stellar_ledger_apply_success_total";
pub const LEDGER_APPLY_FAILURE_TOTAL: &str = "stellar_ledger_apply_failure_total";
pub const LEDGER_APPLY_SOROBAN_SUCCESS_TOTAL: &str = "stellar_ledger_apply_soroban_success_total";
pub const LEDGER_APPLY_SOROBAN_FAILURE_TOTAL: &str = "stellar_ledger_apply_soroban_failure_total";
pub const LEDGER_APPLY_SOROBAN_MAX_CLUSTERS: &str = "stellar_ledger_apply_soroban_max_clusters";
pub const LEDGER_APPLY_SOROBAN_STAGES: &str = "stellar_ledger_apply_soroban_stages";
pub const LEDGER_AGE_CURRENT_SECONDS: &str = "stellar_ledger_age_current_seconds";

// Phase 3 — Soroban network config limits (gauges — snapshot values, not monotonic).
pub const SOROBAN_CONFIG_CONTRACT_MAX_RW_KEY_BYTE: &str =
    "stellar_soroban_config_contract_max_rw_key_byte";
pub const SOROBAN_CONFIG_CONTRACT_MAX_RW_DATA_BYTE: &str =
    "stellar_soroban_config_contract_max_rw_data_byte";
pub const SOROBAN_CONFIG_CONTRACT_MAX_RW_CODE_BYTE: &str =
    "stellar_soroban_config_contract_max_rw_code_byte";
pub const SOROBAN_CONFIG_TX_MAX_SIZE_BYTE: &str = "stellar_soroban_config_tx_max_size_byte";
pub const SOROBAN_CONFIG_TX_MAX_CPU_INSN: &str = "stellar_soroban_config_tx_max_cpu_insn";
pub const SOROBAN_CONFIG_TX_MAX_MEM_BYTE: &str = "stellar_soroban_config_tx_max_mem_byte";
pub const SOROBAN_CONFIG_TX_MAX_READ_ENTRY: &str = "stellar_soroban_config_tx_max_read_entry";
pub const SOROBAN_CONFIG_TX_MAX_READ_LEDGER_BYTE: &str =
    "stellar_soroban_config_tx_max_read_ledger_byte";
pub const SOROBAN_CONFIG_TX_MAX_WRITE_ENTRY: &str = "stellar_soroban_config_tx_max_write_entry";
pub const SOROBAN_CONFIG_TX_MAX_WRITE_LEDGER_BYTE: &str =
    "stellar_soroban_config_tx_max_write_ledger_byte";
pub const SOROBAN_CONFIG_TX_MAX_EMIT_EVENT_BYTE: &str =
    "stellar_soroban_config_tx_max_emit_event_byte";
pub const SOROBAN_CONFIG_LEDGER_MAX_TX_COUNT: &str = "stellar_soroban_config_ledger_max_tx_count";
pub const SOROBAN_CONFIG_LEDGER_MAX_CPU_INSN: &str = "stellar_soroban_config_ledger_max_cpu_insn";
pub const SOROBAN_CONFIG_LEDGER_MAX_TXS_SIZE_BYTE: &str =
    "stellar_soroban_config_ledger_max_txs_size_byte";
pub const SOROBAN_CONFIG_LEDGER_MAX_READ_ENTRY: &str =
    "stellar_soroban_config_ledger_max_read_entry";
pub const SOROBAN_CONFIG_LEDGER_MAX_READ_LEDGER_BYTE: &str =
    "stellar_soroban_config_ledger_max_read_ledger_byte";
pub const SOROBAN_CONFIG_LEDGER_MAX_WRITE_ENTRY: &str =
    "stellar_soroban_config_ledger_max_write_entry";
pub const SOROBAN_CONFIG_LEDGER_MAX_WRITE_LEDGER_BYTE: &str =
    "stellar_soroban_config_ledger_max_write_ledger_byte";
pub const SOROBAN_CONFIG_BUCKET_LIST_TARGET_SIZE_BYTE: &str =
    "stellar_soroban_config_bucket_list_target_size_byte";
pub const SOROBAN_CONFIG_FEE_WRITE_1KB: &str = "stellar_soroban_config_fee_write_1kb";

// Phase 3 — Henyey-specific observability.
pub const LEDGER_SOROBAN_EXEC_US: &str = "henyey_ledger_soroban_exec_us";
pub const LEDGER_CLASSIC_EXEC_US: &str = "henyey_ledger_classic_exec_us";
pub const LEDGER_BUCKET_CACHE_HIT_RATIO: &str = "henyey_bucket_cache_hit_ratio";
pub const LEDGER_SNAPSHOT_CACHE_HIT_RATIO: &str = "henyey_snapshot_cache_hit_ratio";
pub const LEDGER_SNAPSHOT_CACHE_FALLBACK_LOOKUPS: &str = "henyey_snapshot_cache_fallback_lookups";

// Phase 4 — Ledger close histogram.
pub const LEDGER_CLOSE_DURATION_SECONDS: &str = "stellar_ledger_close_duration_seconds";

// Phase 4 — Overlay connection breakdown.
pub const OVERLAY_INBOUND_AUTHENTICATED: &str = "stellar_overlay_inbound_authenticated";
pub const OVERLAY_OUTBOUND_AUTHENTICATED: &str = "stellar_overlay_outbound_authenticated";
pub const OVERLAY_INBOUND_PENDING: &str = "stellar_overlay_inbound_pending";
pub const OVERLAY_OUTBOUND_PENDING: &str = "stellar_overlay_outbound_pending";

// Phase 4 — Process health.
pub const PROCESS_OPEN_FDS: &str = "henyey_process_open_fds";
pub const PROCESS_MAX_FDS: &str = "henyey_process_max_fds";

// Phase 4 — Quorum health.
pub const QUORUM_AGREE: &str = "stellar_quorum_agree";
pub const QUORUM_MISSING: &str = "stellar_quorum_missing";
pub const QUORUM_DISAGREE: &str = "stellar_quorum_disagree";
pub const QUORUM_FAIL_AT: &str = "stellar_quorum_fail_at";

// Phase 4 — SCP timing.
pub const SCP_TIMING_EXTERNALIZED_SECONDS: &str = "stellar_scp_timing_externalized_seconds";
pub const SCP_TIMING_NOMINATED_SECONDS: &str = "stellar_scp_timing_nominated_seconds";

// Phase 5 — Per-phase ledger close histograms (LedgerClosePerf).
pub const CLOSE_BEGIN_SECONDS: &str = "henyey_ledger_close_begin_seconds";
pub const CLOSE_TX_EXEC_SECONDS: &str = "henyey_ledger_close_tx_exec_seconds";
pub const CLOSE_CLASSIC_EXEC_SECONDS: &str = "henyey_ledger_close_classic_exec_seconds";
pub const CLOSE_SOROBAN_EXEC_SECONDS: &str = "henyey_ledger_close_soroban_exec_seconds";
pub const CLOSE_COMMIT_SETUP_SECONDS: &str = "henyey_ledger_close_commit_setup_seconds";
pub const CLOSE_BUCKET_LOCK_WAIT_SECONDS: &str = "henyey_ledger_close_bucket_lock_wait_seconds";
pub const CLOSE_EVICTION_SECONDS: &str = "henyey_ledger_close_eviction_seconds";
pub const CLOSE_SOROBAN_STATE_SECONDS: &str = "henyey_ledger_close_soroban_state_seconds";
pub const CLOSE_BUCKET_ADD_SECONDS: &str = "henyey_ledger_close_bucket_add_seconds";
pub const CLOSE_HOT_ARCHIVE_SECONDS: &str = "henyey_ledger_close_hot_archive_seconds";
pub const CLOSE_HEADER_SECONDS: &str = "henyey_ledger_close_header_seconds";
pub const CLOSE_COMMIT_SECONDS: &str = "henyey_ledger_close_commit_seconds";
pub const CLOSE_META_SECONDS: &str = "henyey_ledger_close_meta_seconds";

// Phase 5 — Event-loop post-close PhaseTimer histograms.
pub const CLOSE_COMPLETE_JOIN_MATCH_SECONDS: &str = "henyey_close_complete_join_match_seconds";
pub const CLOSE_COMPLETE_META_EMIT_SECONDS: &str = "henyey_close_complete_meta_emit_seconds";
pub const CLOSE_COMPLETE_BUILD_PERSIST_INPUTS_SECONDS: &str =
    "henyey_close_complete_build_persist_inputs_seconds";
pub const CLOSE_COMPLETE_OVERLAY_BOOKKEEPING_SECONDS: &str =
    "henyey_close_complete_overlay_bookkeeping_seconds";
pub const CLOSE_COMPLETE_SPAWN_BLOCKING_SETUP_SECONDS: &str =
    "henyey_close_complete_spawn_blocking_setup_seconds";
pub const CLOSE_COMPLETE_TX_QUEUE_SECONDS: &str = "henyey_close_complete_tx_queue_seconds";
pub const CLOSE_COMPLETE_POST_CLOSE_BOOKKEEPING_SECONDS: &str =
    "henyey_close_complete_post_close_bookkeeping_seconds";

// Phase 6 — Sub-phase instrumentation inside tx_queue spawn_blocking.
pub const CLOSE_TX_QUEUE_PREP_SECONDS: &str = "henyey_close_tx_queue_prep_seconds";
pub const CLOSE_TX_QUEUE_LEDGER_CLOSED_SECONDS: &str =
    "henyey_close_tx_queue_ledger_closed_seconds";
pub const CLOSE_TX_QUEUE_SHIFT_UPDATE_SECONDS: &str = "henyey_close_tx_queue_shift_update_seconds";
pub const CLOSE_TX_QUEUE_SNAPSHOT_SECONDS: &str = "henyey_close_tx_queue_snapshot_seconds";
pub const CLOSE_TX_QUEUE_ENVELOPES_FETCH_SECONDS: &str =
    "henyey_close_tx_queue_envelopes_fetch_seconds";
pub const CLOSE_TX_QUEUE_SNAPSHOT_BUILD_SECONDS: &str =
    "henyey_close_tx_queue_snapshot_build_seconds";
pub const CLOSE_TX_QUEUE_INVALIDATION_SECONDS: &str = "henyey_close_tx_queue_invalidation_seconds";

// Phase 6 — Persist and close-cycle instrumentation.
pub const PERSIST_LEDGER_CLOSE_SECONDS: &str = "henyey_persist_ledger_close_seconds";
pub const CLOSE_CYCLE_SECONDS: &str = "henyey_close_cycle_seconds";

// Close-cycle decomposition metrics (#1909).
// close_cycle ≈ handle_complete + post_complete + inter_close_wait + dispatch_to_join
pub const CLOSE_HANDLE_COMPLETE_SECONDS: &str = "henyey_close_handle_complete_seconds";
pub const CLOSE_POST_COMPLETE_SECONDS: &str = "henyey_close_post_complete_seconds";
pub const CLOSE_DISPATCH_TO_JOIN_SECONDS: &str = "henyey_close_dispatch_to_join_seconds";
pub const PERSIST_DISPATCH_TO_JOIN_SECONDS: &str = "henyey_persist_dispatch_to_join_seconds";

// Phase 5 — Slot-to-close latency histogram.
pub const SLOT_TO_CLOSE_LATENCY_SECONDS: &str = "henyey_slot_to_close_latency_seconds";

// Phase 5 — Archive cache metrics.
pub const ARCHIVE_CACHE_FRESH_TOTAL: &str = "henyey_archive_cache_fresh_total";
pub const ARCHIVE_CACHE_STALE_TOTAL: &str = "henyey_archive_cache_stale_total";
pub const ARCHIVE_CACHE_COLD_TOTAL: &str = "henyey_archive_cache_cold_total";
pub const ARCHIVE_CACHE_REFRESH_SUCCESS_TOTAL: &str = "henyey_archive_cache_refresh_success_total";
pub const ARCHIVE_CACHE_REFRESH_ERROR_TOTAL: &str = "henyey_archive_cache_refresh_error_total";
pub const ARCHIVE_CACHE_REFRESH_TIMEOUT_TOTAL: &str = "henyey_archive_cache_refresh_timeout_total";
pub const ARCHIVE_CACHE_REFRESH_DURATION_SECONDS: &str =
    "henyey_archive_cache_refresh_duration_seconds";
pub const ARCHIVE_CACHE_AGE_SECONDS: &str = "henyey_archive_cache_age_seconds";
pub const ARCHIVE_CACHE_POPULATED: &str = "henyey_archive_cache_populated";

// Phase 5 — Recovery stalled tick counter.
pub const RECOVERY_STALLED_TICK_TOTAL: &str = "henyey_recovery_stalled_tick_total";

// ── Registration ───────────────────────────────────────────────────────

/// Register HELP/TYPE annotations for all metrics.
///
/// Must be called **after** the global recorder is installed, otherwise
/// the descriptions are silently dropped.
pub fn describe_metrics() {
    // Stellar-compatible gauges.
    describe_gauge!(LEDGER_SEQUENCE, "Current ledger sequence number");
    describe_gauge!(PEER_COUNT, "Number of connected peers");
    describe_gauge!(PENDING_TRANSACTIONS, "Number of pending transactions");
    describe_gauge!(UPTIME_SECONDS, "Node uptime in seconds");
    describe_gauge!(IS_VALIDATOR, "Whether this node is a validator");
    describe_counter!(
        META_STREAM_BYTES_TOTAL,
        "Total bytes written to metadata output stream"
    );
    describe_counter!(
        META_STREAM_WRITES_TOTAL,
        "Total frames written to metadata output stream"
    );

    // SCP verify pipeline.
    describe_counter!(
        SCP_PREFILTER_REJECTS_TOTAL,
        "SCP envelopes rejected by the event-loop pre-filter, by reason"
    );
    describe_counter!(
        SCP_POST_VERIFY_DROPS_TOTAL,
        "Envelopes dropped after verification (aggregate)"
    );
    describe_counter!(
        SCP_POST_VERIFY_TOTAL,
        "Envelopes processed by post-verify, by reason"
    );
    describe_gauge!(
        SCP_VERIFY_INPUT_BACKLOG,
        "Current depth of the SCP signature-verify input channel (event-loop sampled)"
    );
    describe_gauge!(
        SCP_VERIFY_OUTPUT_BACKLOG,
        "Current depth of the verified-envelope output channel (envelopes awaiting the event loop)"
    );
    describe_gauge!(
        SCP_VERIFIER_THREAD_STATE,
        "Worker thread state (0=Running, 1=Stopping, 2=Dead)"
    );
    // SCP verify latency — formerly exposed as a synthetic Prometheus
    // "summary" (`# TYPE henyey_scp_verify_latency_us summary` with _sum
    // and _count sub-metrics). The `metrics` crate has no native summary
    // type, so these are now separate gauges. Consumers computing average
    // latency as sum/count continue to work unchanged.
    describe_gauge!(
        SCP_VERIFY_LATENCY_US_SUM,
        "Enqueue-to-post-verify latency microseconds (cumulative sum)"
    );
    describe_gauge!(
        SCP_VERIFY_LATENCY_US_COUNT,
        "Enqueue-to-post-verify latency sample count"
    );

    // Overlay fetch channel.
    describe_gauge!(
        OVERLAY_FETCH_CHANNEL_DEPTH,
        "Current depth of the overlay fetch-response channel (event-loop sampled)"
    );
    describe_gauge!(
        OVERLAY_FETCH_CHANNEL_DEPTH_MAX,
        "Monotonic maximum depth of the overlay fetch-response channel since process start"
    );

    // Catchup.
    describe_counter!(
        POST_CATCHUP_HARD_RESET_TOTAL,
        "Total post-catchup hard resets performed"
    );

    // SCP/herder counters.
    describe_counter!(
        SCP_ENVELOPE_EMIT_TOTAL,
        "Total SCP envelopes emitted by the local node"
    );
    describe_counter!(
        SCP_ENVELOPE_RECEIVE_TOTAL,
        "Total SCP envelopes received from peers"
    );
    describe_counter!(
        HERDER_LOST_SYNC_TOTAL,
        "Total lost-sync events (transitions out of Tracking)"
    );
    describe_gauge!(
        HERDER_STATE,
        "Herder state (0=Booting, 1=Syncing, 2=Tracking)"
    );
    describe_gauge!(
        HERDER_PENDING_ENVELOPES,
        "SCP envelopes queued in pending pool"
    );
    describe_gauge!(
        HERDER_CACHED_TX_SETS,
        "Transaction sets cached for pending SCP slots"
    );
    describe_counter!(
        HERDER_PENDING_RECEIVED_TOTAL,
        "Total envelopes received by pending pool"
    );
    describe_counter!(
        HERDER_PENDING_DUPLICATES_TOTAL,
        "Envelopes rejected as duplicates by pending pool"
    );
    describe_counter!(
        HERDER_PENDING_TOO_OLD_TOTAL,
        "Envelopes rejected as too old by pending pool"
    );
    describe_counter!(
        HERDER_PENDING_EVICTED_TOTAL,
        "Envelopes evicted from pending pool"
    );

    // Bucket merge counters.
    describe_counter!(
        BUCKET_MERGE_COMPLETED_TOTAL,
        "Total bucket merges completed"
    );
    describe_counter!(
        BUCKET_MERGE_TIME_US_TOTAL,
        "Total bucket merge time in microseconds"
    );
    describe_counter!(
        BUCKET_MERGE_NEW_LIVE_TOTAL,
        "Total new LIVEENTRY entries written during merges"
    );
    describe_counter!(
        BUCKET_MERGE_NEW_DEAD_TOTAL,
        "Total new DEADENTRY entries written during merges"
    );
    describe_counter!(
        BUCKET_MERGE_NEW_INIT_TOTAL,
        "Total new INITENTRY entries written during merges"
    );
    describe_counter!(
        BUCKET_MERGE_NEW_META_TOTAL,
        "Total new METAENTRY entries written during merges"
    );
    describe_counter!(
        BUCKET_MERGE_SHADOWED_TOTAL,
        "Total entries shadowed during merges"
    );
    describe_counter!(
        BUCKET_MERGE_ANNIHILATED_TOTAL,
        "Total INIT+DEAD entry pairs annihilated during merges"
    );

    // Overlay counters.
    describe_counter!(
        OVERLAY_MESSAGE_READ_TOTAL,
        "Total overlay messages read from peers"
    );
    describe_counter!(
        OVERLAY_MESSAGE_WRITE_TOTAL,
        "Total overlay messages written to peers"
    );
    describe_counter!(
        OVERLAY_MESSAGE_BROADCAST_TOTAL,
        "Total overlay messages broadcast to all peers"
    );
    describe_counter!(OVERLAY_ERROR_READ_TOTAL, "Total overlay read errors");
    describe_counter!(OVERLAY_ERROR_WRITE_TOTAL, "Total overlay write errors");
    describe_counter!(
        OVERLAY_TIMEOUT_IDLE_TOTAL,
        "Total peers dropped due to idle timeout"
    );
    describe_counter!(
        OVERLAY_TIMEOUT_STRAGGLER_TOTAL,
        "Total peers dropped due to straggler timeout"
    );

    // Ledger close metrics.
    describe_gauge!(
        LEDGER_CLOSE_TIME_MS,
        "Wall-clock time of the most recent ledger close (milliseconds)"
    );
    describe_gauge!(LEDGER_TX_COUNT, "Transactions processed in the last close");
    describe_gauge!(LEDGER_OP_COUNT, "Operations executed in the last close");
    describe_gauge!(
        LEDGER_TX_SUCCESS_COUNT,
        "Successful transactions in the last close"
    );
    describe_gauge!(
        LEDGER_TX_FAILED_COUNT,
        "Failed transactions in the last close"
    );
    describe_gauge!(
        LEDGER_TOTAL_FEES,
        "Fees collected in the last close (stroops)"
    );
    describe_gauge!(
        LEDGER_ENTRIES_CREATED,
        "Ledger entries created in the last close"
    );
    describe_gauge!(
        LEDGER_ENTRIES_UPDATED,
        "Ledger entries updated in the last close"
    );
    describe_gauge!(
        LEDGER_ENTRIES_DELETED,
        "Ledger entries deleted in the last close"
    );
    describe_gauge!(LEDGER_APPLY_US, "Total ledger close time in microseconds");

    // Herder tx queue metrics.
    describe_gauge!(
        HERDER_TX_QUEUE_ACCOUNTS,
        "Accounts with pending transactions in the queue"
    );
    describe_gauge!(
        HERDER_TX_QUEUE_BANNED,
        "Currently banned transactions in the queue"
    );
    describe_gauge!(
        HERDER_TX_QUEUE_SEEN,
        "Deduplicated transaction hashes in the seen set"
    );

    // Herder pending envelope metrics.
    describe_counter!(
        HERDER_PENDING_ADDED_TOTAL,
        "Envelopes added to the pending pool"
    );
    describe_counter!(
        HERDER_PENDING_RELEASED_TOTAL,
        "Envelopes released from the pending pool"
    );

    // Clock drift metrics.
    describe_gauge!(
        DRIFT_MIN_SECONDS,
        "Minimum close time drift in the last completed window (seconds)"
    );
    describe_gauge!(
        DRIFT_MAX_SECONDS,
        "Maximum close time drift in the last completed window (seconds)"
    );
    describe_gauge!(
        DRIFT_MEDIAN_SECONDS,
        "Median close time drift in the last completed window (seconds)"
    );
    describe_gauge!(
        DRIFT_P75_SECONDS,
        "75th percentile close time drift in the last completed window (seconds)"
    );
    describe_gauge!(
        DRIFT_SAMPLE_COUNT,
        "Number of samples in the last completed drift window"
    );

    // jemalloc allocator metrics.
    describe_gauge!(
        JEMALLOC_ALLOCATED_BYTES,
        "Bytes allocated by the application (jemalloc stats.allocated)"
    );
    describe_gauge!(
        JEMALLOC_ACTIVE_BYTES,
        "Bytes in active pages (jemalloc stats.active)"
    );
    describe_gauge!(
        JEMALLOC_RESIDENT_BYTES,
        "Bytes resident in physical memory (jemalloc stats.resident)"
    );
    describe_gauge!(
        JEMALLOC_MAPPED_BYTES,
        "Total bytes mapped by the allocator (jemalloc stats.mapped)"
    );
    describe_gauge!(
        JEMALLOC_RETAINED_BYTES,
        "Bytes retained by the allocator (jemalloc stats.retained)"
    );
    describe_gauge!(
        JEMALLOC_FRAGMENTATION_PCT,
        "Allocator fragmentation: (resident - allocated) / allocated * 100"
    );

    // Phase 3: Ledger apply counters.
    describe_counter!(
        LEDGER_APPLY_SUCCESS_TOTAL,
        "Cumulative successful transaction applies"
    );
    describe_counter!(
        LEDGER_APPLY_FAILURE_TOTAL,
        "Cumulative failed transaction applies"
    );
    describe_counter!(
        LEDGER_APPLY_SOROBAN_SUCCESS_TOTAL,
        "Cumulative successful Soroban transaction applies"
    );
    describe_counter!(
        LEDGER_APPLY_SOROBAN_FAILURE_TOTAL,
        "Cumulative failed Soroban transaction applies"
    );
    describe_gauge!(
        LEDGER_APPLY_SOROBAN_MAX_CLUSTERS,
        "Max dependent tx clusters in the last Soroban ledger close"
    );
    describe_gauge!(
        LEDGER_APPLY_SOROBAN_STAGES,
        "Number of parallel stages in the last Soroban ledger close"
    );
    describe_gauge!(
        LEDGER_AGE_CURRENT_SECONDS,
        "Seconds since the last ledger close"
    );

    // Phase 3: Soroban config limits (gauges — snapshot values that go up/down).
    describe_gauge!(
        SOROBAN_CONFIG_CONTRACT_MAX_RW_KEY_BYTE,
        "Soroban config: max contract data key size in bytes"
    );
    describe_gauge!(
        SOROBAN_CONFIG_CONTRACT_MAX_RW_DATA_BYTE,
        "Soroban config: max contract data entry size in bytes"
    );
    describe_gauge!(
        SOROBAN_CONFIG_CONTRACT_MAX_RW_CODE_BYTE,
        "Soroban config: max contract code size in bytes"
    );
    describe_gauge!(
        SOROBAN_CONFIG_TX_MAX_SIZE_BYTE,
        "Soroban config: max transaction size in bytes"
    );
    describe_gauge!(
        SOROBAN_CONFIG_TX_MAX_CPU_INSN,
        "Soroban config: max CPU instructions per transaction"
    );
    describe_gauge!(
        SOROBAN_CONFIG_TX_MAX_MEM_BYTE,
        "Soroban config: max memory per transaction in bytes"
    );
    describe_gauge!(
        SOROBAN_CONFIG_TX_MAX_READ_ENTRY,
        "Soroban config: max read entries per transaction"
    );
    describe_gauge!(
        SOROBAN_CONFIG_TX_MAX_READ_LEDGER_BYTE,
        "Soroban config: max read bytes per transaction"
    );
    describe_gauge!(
        SOROBAN_CONFIG_TX_MAX_WRITE_ENTRY,
        "Soroban config: max write entries per transaction"
    );
    describe_gauge!(
        SOROBAN_CONFIG_TX_MAX_WRITE_LEDGER_BYTE,
        "Soroban config: max write bytes per transaction"
    );
    describe_gauge!(
        SOROBAN_CONFIG_TX_MAX_EMIT_EVENT_BYTE,
        "Soroban config: max event bytes per transaction"
    );
    describe_gauge!(
        SOROBAN_CONFIG_LEDGER_MAX_TX_COUNT,
        "Soroban config: max transactions per ledger"
    );
    describe_gauge!(
        SOROBAN_CONFIG_LEDGER_MAX_CPU_INSN,
        "Soroban config: max CPU instructions per ledger"
    );
    describe_gauge!(
        SOROBAN_CONFIG_LEDGER_MAX_TXS_SIZE_BYTE,
        "Soroban config: max total transaction size per ledger"
    );
    describe_gauge!(
        SOROBAN_CONFIG_LEDGER_MAX_READ_ENTRY,
        "Soroban config: max read entries per ledger"
    );
    describe_gauge!(
        SOROBAN_CONFIG_LEDGER_MAX_READ_LEDGER_BYTE,
        "Soroban config: max read bytes per ledger"
    );
    describe_gauge!(
        SOROBAN_CONFIG_LEDGER_MAX_WRITE_ENTRY,
        "Soroban config: max write entries per ledger"
    );
    describe_gauge!(
        SOROBAN_CONFIG_LEDGER_MAX_WRITE_LEDGER_BYTE,
        "Soroban config: max write bytes per ledger"
    );
    describe_gauge!(
        SOROBAN_CONFIG_BUCKET_LIST_TARGET_SIZE_BYTE,
        "Soroban config: bucket list target size in bytes"
    );
    describe_gauge!(
        SOROBAN_CONFIG_FEE_WRITE_1KB,
        "Soroban config: fee for 1KB write"
    );

    // Phase 3: Henyey-specific observability.
    describe_gauge!(
        LEDGER_SOROBAN_EXEC_US,
        "Soroban parallel phase execution time in microseconds (last close)"
    );
    describe_gauge!(
        LEDGER_CLASSIC_EXEC_US,
        "Classic phase execution time in microseconds (last close)"
    );
    describe_gauge!(
        LEDGER_BUCKET_CACHE_HIT_RATIO,
        "Per-bucket RandomEvictionCache hit ratio (0.0-1.0, Account entries only, last close)"
    );
    describe_gauge!(
        LEDGER_SNAPSHOT_CACHE_HIT_RATIO,
        "SnapshotHandle local cache hit ratio (0.0-1.0, last close)"
    );
    describe_gauge!(
        LEDGER_SNAPSHOT_CACHE_FALLBACK_LOOKUPS,
        "Lookups dispatched to fallback (not served by snapshot local caches, last close)"
    );

    // Phase 4: Ledger close histogram.
    describe_histogram!(
        LEDGER_CLOSE_DURATION_SECONDS,
        "Ledger close wall-clock duration in seconds"
    );

    // Phase 4: Overlay connection breakdown.
    describe_gauge!(
        OVERLAY_INBOUND_AUTHENTICATED,
        "Authenticated inbound peer connections"
    );
    describe_gauge!(
        OVERLAY_OUTBOUND_AUTHENTICATED,
        "Authenticated outbound peer connections"
    );
    describe_gauge!(
        OVERLAY_INBOUND_PENDING,
        "Pending inbound peer connections (handshaking)"
    );
    describe_gauge!(
        OVERLAY_OUTBOUND_PENDING,
        "Pending outbound peer connections (handshaking)"
    );

    // Phase 4: Process health.
    describe_gauge!(PROCESS_OPEN_FDS, "Open file descriptors");
    describe_gauge!(PROCESS_MAX_FDS, "Maximum file descriptors (RLIMIT_NOFILE)");

    // Phase 4: Quorum health.
    describe_gauge!(
        QUORUM_AGREE,
        "Quorum set nodes in agreement (confirming/externalized)"
    );
    describe_gauge!(QUORUM_MISSING, "Quorum set nodes not responding");
    describe_gauge!(QUORUM_DISAGREE, "Quorum set nodes disagreeing");
    describe_gauge!(QUORUM_FAIL_AT, "Nodes that can fail before quorum is lost");

    // Phase 4: SCP timing.
    describe_gauge!(
        SCP_TIMING_EXTERNALIZED_SECONDS,
        "Time from slot creation to externalize (seconds, last slot)"
    );
    describe_gauge!(
        SCP_TIMING_NOMINATED_SECONDS,
        "Time from first local nomination to externalize (seconds, last slot)"
    );

    // Phase 5: Per-phase close-duration histograms (LedgerClosePerf).
    describe_histogram!(
        CLOSE_BEGIN_SECONDS,
        "Ledger close begin_close phase (seconds)"
    );
    describe_histogram!(
        CLOSE_TX_EXEC_SECONDS,
        "Ledger close tx_exec phase (seconds)"
    );
    describe_histogram!(
        CLOSE_CLASSIC_EXEC_SECONDS,
        "Ledger close classic_exec sub-phase of tx_exec (seconds)"
    );
    describe_histogram!(
        CLOSE_SOROBAN_EXEC_SECONDS,
        "Ledger close soroban_exec sub-phase of tx_exec (seconds)"
    );
    describe_histogram!(
        CLOSE_COMMIT_SETUP_SECONDS,
        "Ledger close commit_setup phase (seconds)"
    );
    describe_histogram!(
        CLOSE_BUCKET_LOCK_WAIT_SECONDS,
        "Ledger close bucket_lock_wait phase (seconds)"
    );
    describe_histogram!(
        CLOSE_EVICTION_SECONDS,
        "Ledger close eviction phase (seconds)"
    );
    describe_histogram!(
        CLOSE_SOROBAN_STATE_SECONDS,
        "Ledger close soroban_state phase (seconds)"
    );
    describe_histogram!(
        CLOSE_BUCKET_ADD_SECONDS,
        "Ledger close bucket_add phase (seconds)"
    );
    describe_histogram!(
        CLOSE_HOT_ARCHIVE_SECONDS,
        "Ledger close hot_archive phase (seconds)"
    );
    describe_histogram!(CLOSE_HEADER_SECONDS, "Ledger close header phase (seconds)");
    describe_histogram!(
        CLOSE_COMMIT_SECONDS,
        "Ledger close commit_close phase (seconds)"
    );
    describe_histogram!(CLOSE_META_SECONDS, "Ledger close meta phase (seconds)");

    // Phase 5: Event-loop post-close PhaseTimer histograms.
    describe_histogram!(
        CLOSE_COMPLETE_JOIN_MATCH_SECONDS,
        "Close-complete join_match phase (seconds)"
    );
    describe_histogram!(
        CLOSE_COMPLETE_META_EMIT_SECONDS,
        "Close-complete meta_emit phase (seconds)"
    );
    describe_histogram!(
        CLOSE_COMPLETE_BUILD_PERSIST_INPUTS_SECONDS,
        "Close-complete build_persist_inputs phase (seconds)"
    );
    describe_histogram!(
        CLOSE_COMPLETE_OVERLAY_BOOKKEEPING_SECONDS,
        "Close-complete overlay_bookkeeping phase (seconds)"
    );
    describe_histogram!(
        CLOSE_COMPLETE_SPAWN_BLOCKING_SETUP_SECONDS,
        "Close-complete spawn_blocking_setup phase (seconds)"
    );
    describe_histogram!(
        CLOSE_COMPLETE_TX_QUEUE_SECONDS,
        "Close-complete tx_queue_background_wait phase (seconds)"
    );
    describe_histogram!(
        CLOSE_COMPLETE_POST_CLOSE_BOOKKEEPING_SECONDS,
        "Close-complete post_close_bookkeeping phase (seconds)"
    );

    // Phase 6: Sub-phase instrumentation inside tx_queue spawn_blocking.
    describe_histogram!(
        CLOSE_TX_QUEUE_PREP_SECONDS,
        "tx_queue spawn_blocking closure-local prep (Soroban limits, close-time math) (seconds)"
    );
    describe_histogram!(
        CLOSE_TX_QUEUE_LEDGER_CLOSED_SECONDS,
        "tx_queue spawn_blocking herder.ledger_closed + ban failed txs (seconds)"
    );
    describe_histogram!(
        CLOSE_TX_QUEUE_SHIFT_UPDATE_SECONDS,
        "tx_queue spawn_blocking validation context update + Soroban limits + shift (seconds)"
    );
    describe_histogram!(
        CLOSE_TX_QUEUE_SNAPSHOT_SECONDS,
        "tx_queue spawn_blocking pending_hashed_envelopes + snapshot build total (seconds)"
    );
    describe_histogram!(
        CLOSE_TX_QUEUE_ENVELOPES_FETCH_SECONDS,
        "tx_queue spawn_blocking pending_hashed_envelopes only (seconds)"
    );
    describe_histogram!(
        CLOSE_TX_QUEUE_SNAPSHOT_BUILD_SECONDS,
        "tx_queue spawn_blocking SnapshotValidationProviders::new only (seconds)"
    );
    describe_histogram!(
        CLOSE_TX_QUEUE_INVALIDATION_SECONDS,
        "tx_queue spawn_blocking get_invalid_hashed_tx_list + ban invalid (seconds)"
    );

    // Phase 6: Persist and close-cycle instrumentation.
    describe_histogram!(
        PERSIST_LEDGER_CLOSE_SECONDS,
        "PersistJob::LedgerClose wall-clock duration (seconds, excludes scheduling delay)"
    );
    describe_histogram!(
        CLOSE_CYCLE_SECONDS,
        "Wall-clock between consecutive deferred-pipeline close-complete events (seconds)"
    );

    // Close-cycle decomposition (#1909).
    describe_histogram!(
        CLOSE_HANDLE_COMPLETE_SECONDS,
        "Wall-clock of handle_close_complete including finalizer dispatch (seconds)"
    );
    describe_histogram!(
        CLOSE_POST_COMPLETE_SECONDS,
        "Wall-clock of post-close lifecycle work after handle_close_complete (seconds)"
    );
    describe_histogram!(
        CLOSE_DISPATCH_TO_JOIN_SECONDS,
        "Wall-clock from spawn_blocking dispatch to event-loop join observation (seconds)"
    );
    describe_histogram!(
        PERSIST_DISPATCH_TO_JOIN_SECONDS,
        "Wall-clock from persist spawn_blocking dispatch to event-loop join observation (seconds)"
    );

    // Phase 5: Slot-to-close latency.
    describe_histogram!(
        SLOT_TO_CLOSE_LATENCY_SECONDS,
        "Wall-clock from first SCP activity to close-complete (seconds)"
    );

    // Phase 5: Archive cache metrics.
    describe_counter!(
        ARCHIVE_CACHE_FRESH_TOTAL,
        "Archive cache get_cached() Fresh results"
    );
    describe_counter!(
        ARCHIVE_CACHE_STALE_TOTAL,
        "Archive cache get_cached() Stale results"
    );
    describe_counter!(
        ARCHIVE_CACHE_COLD_TOTAL,
        "Archive cache get_cached() Cold results"
    );
    describe_counter!(
        ARCHIVE_CACHE_REFRESH_SUCCESS_TOTAL,
        "Archive cache background refresh successes"
    );
    describe_counter!(
        ARCHIVE_CACHE_REFRESH_ERROR_TOTAL,
        "Archive cache background refresh errors"
    );
    describe_counter!(
        ARCHIVE_CACHE_REFRESH_TIMEOUT_TOTAL,
        "Archive cache background refresh timeouts"
    );
    describe_histogram!(
        ARCHIVE_CACHE_REFRESH_DURATION_SECONDS,
        "Archive cache background refresh duration (seconds, including timeouts)"
    );
    describe_gauge!(
        ARCHIVE_CACHE_AGE_SECONDS,
        "Age of cached archive checkpoint (seconds, 0 when cold)"
    );
    describe_gauge!(
        ARCHIVE_CACHE_POPULATED,
        "Whether the archive checkpoint cache has a value (1=populated, 0=cold)"
    );

    // Phase 5: Recovery stalled tick counter.
    describe_counter!(
        RECOVERY_STALLED_TICK_TOTAL,
        "Recovery stalled ticks by reason"
    );
}

/// Pre-register all metrics with initial values so that every metric appears
/// on the very first scrape with its HELP/TYPE annotations.
///
/// Must be called **after** [`describe_metrics`].
pub fn register_label_series() {
    use henyey_herder::scp_verify::{PostVerifyReason, PreFilterRejectReason};

    // Initialize all gauges to zero so they appear in output.
    gauge!(LEDGER_SEQUENCE).set(0.0);
    gauge!(PEER_COUNT).set(0.0);
    gauge!(PENDING_TRANSACTIONS).set(0.0);
    gauge!(UPTIME_SECONDS).set(0.0);
    gauge!(IS_VALIDATOR).set(0.0);
    // Note: meta stream counters are intentionally NOT pre-registered —
    // they only appear when the meta stream is active (matching current behavior).

    // SCP verify pipeline gauges.
    gauge!(SCP_VERIFY_INPUT_BACKLOG).set(0.0);
    gauge!(SCP_VERIFY_OUTPUT_BACKLOG).set(0.0);
    gauge!(SCP_VERIFIER_THREAD_STATE).set(0.0);
    gauge!(SCP_VERIFY_LATENCY_US_SUM).set(0.0);
    gauge!(SCP_VERIFY_LATENCY_US_COUNT).set(0.0);

    // Overlay fetch channel gauges.
    gauge!(OVERLAY_FETCH_CHANNEL_DEPTH).set(0.0);
    gauge!(OVERLAY_FETCH_CHANNEL_DEPTH_MAX).set(0.0);

    // Counters.
    counter!(SCP_POST_VERIFY_DROPS_TOTAL).absolute(0);
    counter!(POST_CATCHUP_HARD_RESET_TOTAL).absolute(0);

    // SCP/herder counters.
    counter!(SCP_ENVELOPE_EMIT_TOTAL).absolute(0);
    counter!(SCP_ENVELOPE_RECEIVE_TOTAL).absolute(0);
    counter!(HERDER_LOST_SYNC_TOTAL).absolute(0);
    gauge!(HERDER_STATE).set(0.0);
    gauge!(HERDER_PENDING_ENVELOPES).set(0.0);
    gauge!(HERDER_CACHED_TX_SETS).set(0.0);
    counter!(HERDER_PENDING_RECEIVED_TOTAL).absolute(0);
    counter!(HERDER_PENDING_DUPLICATES_TOTAL).absolute(0);
    counter!(HERDER_PENDING_TOO_OLD_TOTAL).absolute(0);
    counter!(HERDER_PENDING_EVICTED_TOTAL).absolute(0);

    // Bucket merge counters.
    counter!(BUCKET_MERGE_COMPLETED_TOTAL).absolute(0);
    counter!(BUCKET_MERGE_TIME_US_TOTAL).absolute(0);
    counter!(BUCKET_MERGE_NEW_LIVE_TOTAL).absolute(0);
    counter!(BUCKET_MERGE_NEW_DEAD_TOTAL).absolute(0);
    counter!(BUCKET_MERGE_NEW_INIT_TOTAL).absolute(0);
    counter!(BUCKET_MERGE_NEW_META_TOTAL).absolute(0);
    counter!(BUCKET_MERGE_SHADOWED_TOTAL).absolute(0);
    counter!(BUCKET_MERGE_ANNIHILATED_TOTAL).absolute(0);

    // Overlay counters.
    counter!(OVERLAY_MESSAGE_READ_TOTAL).absolute(0);
    counter!(OVERLAY_MESSAGE_WRITE_TOTAL).absolute(0);
    counter!(OVERLAY_MESSAGE_BROADCAST_TOTAL).absolute(0);
    counter!(OVERLAY_ERROR_READ_TOTAL).absolute(0);
    counter!(OVERLAY_ERROR_WRITE_TOTAL).absolute(0);
    counter!(OVERLAY_TIMEOUT_IDLE_TOTAL).absolute(0);
    counter!(OVERLAY_TIMEOUT_STRAGGLER_TOTAL).absolute(0);

    // Ledger close gauges (Phase 2).
    gauge!(LEDGER_CLOSE_TIME_MS).set(0.0);
    gauge!(LEDGER_TX_COUNT).set(0.0);
    gauge!(LEDGER_OP_COUNT).set(0.0);
    gauge!(LEDGER_TX_SUCCESS_COUNT).set(0.0);
    gauge!(LEDGER_TX_FAILED_COUNT).set(0.0);
    gauge!(LEDGER_TOTAL_FEES).set(0.0);
    gauge!(LEDGER_ENTRIES_CREATED).set(0.0);
    gauge!(LEDGER_ENTRIES_UPDATED).set(0.0);
    gauge!(LEDGER_ENTRIES_DELETED).set(0.0);
    gauge!(LEDGER_APPLY_US).set(0.0);

    // Herder tx queue gauges (Phase 2).
    gauge!(HERDER_TX_QUEUE_ACCOUNTS).set(0.0);
    gauge!(HERDER_TX_QUEUE_BANNED).set(0.0);
    gauge!(HERDER_TX_QUEUE_SEEN).set(0.0);

    // Herder pending envelope counters (Phase 2).
    counter!(HERDER_PENDING_ADDED_TOTAL).absolute(0);
    counter!(HERDER_PENDING_RELEASED_TOTAL).absolute(0);

    // Clock drift gauges (Phase 2).
    gauge!(DRIFT_MIN_SECONDS).set(0.0);
    gauge!(DRIFT_MAX_SECONDS).set(0.0);
    gauge!(DRIFT_MEDIAN_SECONDS).set(0.0);
    gauge!(DRIFT_P75_SECONDS).set(0.0);
    gauge!(DRIFT_SAMPLE_COUNT).set(0.0);

    // jemalloc allocator gauges (zero when jemalloc is not enabled).
    gauge!(JEMALLOC_ALLOCATED_BYTES).set(0.0);
    gauge!(JEMALLOC_ACTIVE_BYTES).set(0.0);
    gauge!(JEMALLOC_RESIDENT_BYTES).set(0.0);
    gauge!(JEMALLOC_MAPPED_BYTES).set(0.0);
    gauge!(JEMALLOC_RETAINED_BYTES).set(0.0);
    gauge!(JEMALLOC_FRAGMENTATION_PCT).set(0.0);

    // Labeled counters — all reason labels pre-registered at zero.
    for reason in PreFilterRejectReason::ALL {
        counter!(SCP_PREFILTER_REJECTS_TOTAL, "reason" => reason.label()).absolute(0);
    }
    for reason in PostVerifyReason::ALL {
        counter!(SCP_POST_VERIFY_TOTAL, "reason" => reason.label()).absolute(0);
    }

    // Phase 3: Ledger apply counters.
    counter!(LEDGER_APPLY_SUCCESS_TOTAL).absolute(0);
    counter!(LEDGER_APPLY_FAILURE_TOTAL).absolute(0);
    counter!(LEDGER_APPLY_SOROBAN_SUCCESS_TOTAL).absolute(0);
    counter!(LEDGER_APPLY_SOROBAN_FAILURE_TOTAL).absolute(0);
    gauge!(LEDGER_APPLY_SOROBAN_MAX_CLUSTERS).set(0.0);
    gauge!(LEDGER_APPLY_SOROBAN_STAGES).set(0.0);
    gauge!(LEDGER_AGE_CURRENT_SECONDS).set(0.0);

    // Phase 3: Soroban config (pre-registered at zero for first-scrape visibility).
    gauge!(SOROBAN_CONFIG_CONTRACT_MAX_RW_KEY_BYTE).set(0.0);
    gauge!(SOROBAN_CONFIG_CONTRACT_MAX_RW_DATA_BYTE).set(0.0);
    gauge!(SOROBAN_CONFIG_CONTRACT_MAX_RW_CODE_BYTE).set(0.0);
    gauge!(SOROBAN_CONFIG_TX_MAX_SIZE_BYTE).set(0.0);
    gauge!(SOROBAN_CONFIG_TX_MAX_CPU_INSN).set(0.0);
    gauge!(SOROBAN_CONFIG_TX_MAX_MEM_BYTE).set(0.0);
    gauge!(SOROBAN_CONFIG_TX_MAX_READ_ENTRY).set(0.0);
    gauge!(SOROBAN_CONFIG_TX_MAX_READ_LEDGER_BYTE).set(0.0);
    gauge!(SOROBAN_CONFIG_TX_MAX_WRITE_ENTRY).set(0.0);
    gauge!(SOROBAN_CONFIG_TX_MAX_WRITE_LEDGER_BYTE).set(0.0);
    gauge!(SOROBAN_CONFIG_TX_MAX_EMIT_EVENT_BYTE).set(0.0);
    gauge!(SOROBAN_CONFIG_LEDGER_MAX_TX_COUNT).set(0.0);
    gauge!(SOROBAN_CONFIG_LEDGER_MAX_CPU_INSN).set(0.0);
    gauge!(SOROBAN_CONFIG_LEDGER_MAX_TXS_SIZE_BYTE).set(0.0);
    gauge!(SOROBAN_CONFIG_LEDGER_MAX_READ_ENTRY).set(0.0);
    gauge!(SOROBAN_CONFIG_LEDGER_MAX_READ_LEDGER_BYTE).set(0.0);
    gauge!(SOROBAN_CONFIG_LEDGER_MAX_WRITE_ENTRY).set(0.0);
    gauge!(SOROBAN_CONFIG_LEDGER_MAX_WRITE_LEDGER_BYTE).set(0.0);
    gauge!(SOROBAN_CONFIG_BUCKET_LIST_TARGET_SIZE_BYTE).set(0.0);
    gauge!(SOROBAN_CONFIG_FEE_WRITE_1KB).set(0.0);

    // Phase 3: Henyey-specific gauges.
    gauge!(LEDGER_SOROBAN_EXEC_US).set(0.0);
    gauge!(LEDGER_CLASSIC_EXEC_US).set(0.0);
    gauge!(LEDGER_BUCKET_CACHE_HIT_RATIO).set(0.0);
    gauge!(LEDGER_SNAPSHOT_CACHE_HIT_RATIO).set(0.0);
    gauge!(LEDGER_SNAPSHOT_CACHE_FALLBACK_LOOKUPS).set(0.0);

    // Phase 4: Overlay connection breakdown.
    gauge!(OVERLAY_INBOUND_AUTHENTICATED).set(0.0);
    gauge!(OVERLAY_OUTBOUND_AUTHENTICATED).set(0.0);
    gauge!(OVERLAY_INBOUND_PENDING).set(0.0);
    gauge!(OVERLAY_OUTBOUND_PENDING).set(0.0);

    // Phase 4: Process health.
    gauge!(PROCESS_OPEN_FDS).set(0.0);
    gauge!(PROCESS_MAX_FDS).set(0.0);

    // Phase 4: Quorum health.
    gauge!(QUORUM_AGREE).set(0.0);
    gauge!(QUORUM_MISSING).set(0.0);
    gauge!(QUORUM_DISAGREE).set(0.0);
    gauge!(QUORUM_FAIL_AT).set(0.0);

    // Phase 4: SCP timing.
    gauge!(SCP_TIMING_EXTERNALIZED_SECONDS).set(0.0);
    gauge!(SCP_TIMING_NOMINATED_SECONDS).set(0.0);
    // Note: histogram (LEDGER_CLOSE_DURATION_SECONDS) is not pre-registered —
    // it is recorded in the ledger close path, not at scrape time.

    // Phase 5: Archive cache counters and gauges.
    counter!(ARCHIVE_CACHE_FRESH_TOTAL).absolute(0);
    counter!(ARCHIVE_CACHE_STALE_TOTAL).absolute(0);
    counter!(ARCHIVE_CACHE_COLD_TOTAL).absolute(0);
    counter!(ARCHIVE_CACHE_REFRESH_SUCCESS_TOTAL).absolute(0);
    counter!(ARCHIVE_CACHE_REFRESH_ERROR_TOTAL).absolute(0);
    counter!(ARCHIVE_CACHE_REFRESH_TIMEOUT_TOTAL).absolute(0);
    gauge!(ARCHIVE_CACHE_AGE_SECONDS).set(0.0);
    gauge!(ARCHIVE_CACHE_POPULATED).set(0.0);

    // Phase 5: Recovery stalled tick — pre-register all reason labels at zero.
    for reason in &[
        "backoff_active",
        "forcing_catchup_not_behind",
        "forcing_catchup_behind",
    ] {
        counter!(RECOVERY_STALLED_TICK_TOTAL, "reason" => *reason).absolute(0);
    }
    // Note: Phase 5 histograms (CLOSE_*_SECONDS, SLOT_TO_CLOSE_LATENCY_SECONDS,
    // ARCHIVE_CACHE_REFRESH_DURATION_SECONDS) are not pre-registered — they are
    // recorded in the ledger close / archive refresh path, not at scrape time.
}

// ── Scrape-time refresh ────────────────────────────────────────────────

/// Update scrape-time gauges and absolute counters from current App state.
///
/// Called by the `/metrics` handler immediately before `handle.render()`.
pub(crate) async fn refresh_gauges(state: &ServerState) {
    let uptime = state.start_time.elapsed().as_secs();
    let ledger_seq = state.app.ledger_info().ledger_seq;
    let peer_count = state.app.peer_count().await;
    let pending_txs = state.app.pending_transaction_count() as u64;
    let snap = state.app.metrics_snapshot();

    // Stellar-compatible gauges.
    gauge!(LEDGER_SEQUENCE).set(ledger_seq as f64);
    gauge!(PEER_COUNT).set(peer_count as f64);
    gauge!(PENDING_TRANSACTIONS).set(pending_txs as f64);
    gauge!(UPTIME_SECONDS).set(uptime as f64);
    gauge!(IS_VALIDATOR).set(if snap.is_validator { 1.0 } else { 0.0 });

    // Meta stream counters — only set when active (matching current conditional behavior).
    if snap.meta_stream_bytes_total > 0 || snap.meta_stream_writes_total > 0 {
        counter!(META_STREAM_BYTES_TOTAL).absolute(snap.meta_stream_bytes_total);
        counter!(META_STREAM_WRITES_TOTAL).absolute(snap.meta_stream_writes_total);
    }

    // SCP verify pipeline — absolute counters.
    let sv = &snap.scp_verify;
    for (reason, &count) in sv.prefilter_counters.iter() {
        counter!(SCP_PREFILTER_REJECTS_TOTAL, "reason" => reason.label()).absolute(count);
    }
    counter!(SCP_POST_VERIFY_DROPS_TOTAL).absolute(sv.post_verify_drops);
    for (reason, &count) in sv.pv_counters.iter() {
        counter!(SCP_POST_VERIFY_TOTAL, "reason" => reason.label()).absolute(count);
    }

    // SCP verify pipeline — gauges.
    gauge!(SCP_VERIFY_INPUT_BACKLOG).set(sv.verify_input_backlog as f64);
    gauge!(SCP_VERIFY_OUTPUT_BACKLOG).set(sv.verify_output_backlog as f64);
    gauge!(SCP_VERIFIER_THREAD_STATE).set(sv.verifier_thread_state as f64);
    gauge!(SCP_VERIFY_LATENCY_US_SUM).set(sv.verify_latency_us_sum as f64);
    gauge!(SCP_VERIFY_LATENCY_US_COUNT).set(sv.verify_latency_count as f64);

    // Overlay fetch channel.
    gauge!(OVERLAY_FETCH_CHANNEL_DEPTH).set(snap.overlay_fetch_channel.depth as f64);
    gauge!(OVERLAY_FETCH_CHANNEL_DEPTH_MAX).set(snap.overlay_fetch_channel.depth_max as f64);

    // Catchup.
    counter!(POST_CATCHUP_HARD_RESET_TOTAL).absolute(snap.post_catchup_hard_reset_total);

    // SCP/herder counters.
    let (scp_sent, scp_received) = state.app.scp_envelope_counters();
    counter!(SCP_ENVELOPE_EMIT_TOTAL).absolute(scp_sent);
    counter!(SCP_ENVELOPE_RECEIVE_TOTAL).absolute(scp_received);
    counter!(HERDER_LOST_SYNC_TOTAL).absolute(state.app.lost_sync_count());

    let herder = state.app.herder_stats();
    let herder_state_val = match herder.state {
        henyey_herder::HerderState::Booting => 0.0,
        henyey_herder::HerderState::Syncing => 1.0,
        henyey_herder::HerderState::Tracking => 2.0,
    };
    gauge!(HERDER_STATE).set(herder_state_val);
    gauge!(HERDER_PENDING_ENVELOPES).set(herder.pending_envelopes as f64);
    gauge!(HERDER_CACHED_TX_SETS).set(herder.cached_tx_sets as f64);

    let pstats = &herder.pending_envelope_stats;
    counter!(HERDER_PENDING_RECEIVED_TOTAL).absolute(pstats.received);
    counter!(HERDER_PENDING_DUPLICATES_TOTAL).absolute(pstats.duplicates);
    counter!(HERDER_PENDING_TOO_OLD_TOTAL).absolute(pstats.too_old);
    counter!(HERDER_PENDING_EVICTED_TOTAL).absolute(pstats.evicted);

    // Bucket merge counters.
    let mc = state.app.merge_counters_snapshot();
    counter!(BUCKET_MERGE_COMPLETED_TOTAL).absolute(mc.merges_completed);
    counter!(BUCKET_MERGE_TIME_US_TOTAL).absolute(mc.merge_time_us);
    counter!(BUCKET_MERGE_NEW_LIVE_TOTAL).absolute(mc.new_live_entries);
    counter!(BUCKET_MERGE_NEW_DEAD_TOTAL).absolute(mc.new_dead_entries);
    counter!(BUCKET_MERGE_NEW_INIT_TOTAL).absolute(mc.new_init_entries);
    counter!(BUCKET_MERGE_NEW_META_TOTAL).absolute(mc.new_meta_entries);
    counter!(BUCKET_MERGE_SHADOWED_TOTAL).absolute(mc.old_entries_shadowed);
    counter!(BUCKET_MERGE_ANNIHILATED_TOTAL).absolute(mc.entries_annihilated);

    // Overlay counters (if overlay is running).
    if let Some(ov) = state.app.overlay_metrics_snapshot().await {
        counter!(OVERLAY_MESSAGE_READ_TOTAL).absolute(ov.messages_read);
        counter!(OVERLAY_MESSAGE_WRITE_TOTAL).absolute(ov.messages_written);
        counter!(OVERLAY_MESSAGE_BROADCAST_TOTAL).absolute(ov.messages_broadcast);
        counter!(OVERLAY_ERROR_READ_TOTAL).absolute(ov.errors_read);
        counter!(OVERLAY_ERROR_WRITE_TOTAL).absolute(ov.errors_write);
        counter!(OVERLAY_TIMEOUT_IDLE_TOTAL).absolute(ov.timeouts_idle);
        counter!(OVERLAY_TIMEOUT_STRAGGLER_TOTAL).absolute(ov.timeouts_straggler);
    }

    // Ledger close stats (Phase 2).
    let lcs = state.app.last_close_stats();
    gauge!(LEDGER_CLOSE_TIME_MS).set(lcs.close_time_ms as f64);
    gauge!(LEDGER_TX_COUNT).set(lcs.tx_count as f64);
    gauge!(LEDGER_OP_COUNT).set(lcs.op_count as f64);
    gauge!(LEDGER_TX_SUCCESS_COUNT).set(lcs.tx_success_count as f64);
    gauge!(LEDGER_TX_FAILED_COUNT).set(lcs.tx_failed_count as f64);
    gauge!(LEDGER_TOTAL_FEES).set(lcs.total_fees as f64);
    gauge!(LEDGER_ENTRIES_CREATED).set(lcs.entries_created as f64);
    gauge!(LEDGER_ENTRIES_UPDATED).set(lcs.entries_updated as f64);
    gauge!(LEDGER_ENTRIES_DELETED).set(lcs.entries_deleted as f64);

    // Ledger apply timing (Phase 2).
    if let Some(perf) = state.app.last_close_perf() {
        gauge!(LEDGER_APPLY_US).set(perf.total_us as f64);
    }

    // Herder tx queue stats (Phase 2).
    let tq = &herder.tx_queue_stats;
    gauge!(HERDER_TX_QUEUE_ACCOUNTS).set(tq.account_count as f64);
    gauge!(HERDER_TX_QUEUE_BANNED).set(tq.banned_count as f64);
    gauge!(HERDER_TX_QUEUE_SEEN).set(tq.seen_count as f64);

    // Herder pending envelope stats — added + released (Phase 2).
    counter!(HERDER_PENDING_ADDED_TOTAL).absolute(pstats.added);
    counter!(HERDER_PENDING_RELEASED_TOTAL).absolute(pstats.released);

    // Clock drift (Phase 2) — always write, zeros when no completed window.
    if let Some(ds) = state.app.drift_stats() {
        gauge!(DRIFT_MIN_SECONDS).set(ds.min as f64);
        gauge!(DRIFT_MAX_SECONDS).set(ds.max as f64);
        gauge!(DRIFT_MEDIAN_SECONDS).set(ds.median as f64);
        gauge!(DRIFT_P75_SECONDS).set(ds.p75 as f64);
        gauge!(DRIFT_SAMPLE_COUNT).set(ds.sample_count as f64);
    } else {
        gauge!(DRIFT_MIN_SECONDS).set(0.0);
        gauge!(DRIFT_MAX_SECONDS).set(0.0);
        gauge!(DRIFT_MEDIAN_SECONDS).set(0.0);
        gauge!(DRIFT_P75_SECONDS).set(0.0);
        gauge!(DRIFT_SAMPLE_COUNT).set(0.0);
    }

    // jemalloc allocator stats — always available, zeros when jemalloc is not enabled.
    let alloc = henyey_ledger::memory_report::AllocatorStats::capture();
    gauge!(JEMALLOC_ALLOCATED_BYTES).set(alloc.allocated as f64);
    gauge!(JEMALLOC_ACTIVE_BYTES).set(alloc.active as f64);
    gauge!(JEMALLOC_RESIDENT_BYTES).set(alloc.resident as f64);
    gauge!(JEMALLOC_MAPPED_BYTES).set(alloc.mapped as f64);
    gauge!(JEMALLOC_RETAINED_BYTES).set(alloc.retained as f64);
    if alloc.allocated > 0 {
        let frag =
            (alloc.resident as f64 - alloc.allocated as f64) / alloc.allocated as f64 * 100.0;
        gauge!(JEMALLOC_FRAGMENTATION_PCT).set(frag);
    }

    // Phase 3: Ledger apply cumulative counters.
    counter!(LEDGER_APPLY_SUCCESS_TOTAL).absolute(snap.cumulative_apply_success);
    counter!(LEDGER_APPLY_FAILURE_TOTAL).absolute(snap.cumulative_apply_failure);
    counter!(LEDGER_APPLY_SOROBAN_SUCCESS_TOTAL).absolute(snap.cumulative_soroban_success);
    counter!(LEDGER_APPLY_SOROBAN_FAILURE_TOTAL).absolute(snap.cumulative_soroban_failure);
    gauge!(LEDGER_APPLY_SOROBAN_MAX_CLUSTERS).set(snap.soroban_max_cluster_count as f64);
    gauge!(LEDGER_APPLY_SOROBAN_STAGES).set(snap.soroban_stage_count as f64);

    // Phase 3: Ledger age from header close_time.
    let ledger_info = state.app.ledger_info();
    if ledger_info.close_time > 0 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let age = now.saturating_sub(ledger_info.close_time);
        gauge!(LEDGER_AGE_CURRENT_SECONDS).set(age as f64);
    }

    // Phase 3: Soroban config limits (gauges — snapshot values).
    if let Some(info) = state.app.soroban_network_info() {
        gauge!(SOROBAN_CONFIG_CONTRACT_MAX_RW_KEY_BYTE).set(info.max_contract_data_key_size as f64);
        gauge!(SOROBAN_CONFIG_CONTRACT_MAX_RW_DATA_BYTE)
            .set(info.max_contract_data_entry_size as f64);
        gauge!(SOROBAN_CONFIG_CONTRACT_MAX_RW_CODE_BYTE).set(info.max_contract_size as f64);
        gauge!(SOROBAN_CONFIG_TX_MAX_SIZE_BYTE).set(info.tx_max_size_bytes as f64);
        gauge!(SOROBAN_CONFIG_TX_MAX_CPU_INSN).set(info.tx_max_instructions.max(0) as f64);
        gauge!(SOROBAN_CONFIG_TX_MAX_MEM_BYTE).set(info.tx_memory_limit as f64);
        gauge!(SOROBAN_CONFIG_TX_MAX_READ_ENTRY).set(info.tx_max_read_ledger_entries as f64);
        gauge!(SOROBAN_CONFIG_TX_MAX_READ_LEDGER_BYTE).set(info.tx_max_read_bytes as f64);
        gauge!(SOROBAN_CONFIG_TX_MAX_WRITE_ENTRY).set(info.tx_max_write_ledger_entries as f64);
        gauge!(SOROBAN_CONFIG_TX_MAX_WRITE_LEDGER_BYTE).set(info.tx_max_write_bytes as f64);
        gauge!(SOROBAN_CONFIG_TX_MAX_EMIT_EVENT_BYTE)
            .set(info.tx_max_contract_events_size_bytes as f64);
        gauge!(SOROBAN_CONFIG_LEDGER_MAX_TX_COUNT).set(info.ledger_max_tx_count as f64);
        gauge!(SOROBAN_CONFIG_LEDGER_MAX_CPU_INSN).set(info.ledger_max_instructions.max(0) as f64);
        gauge!(SOROBAN_CONFIG_LEDGER_MAX_TXS_SIZE_BYTE).set(info.ledger_max_tx_size_bytes as f64);
        gauge!(SOROBAN_CONFIG_LEDGER_MAX_READ_ENTRY)
            .set(info.ledger_max_read_ledger_entries as f64);
        gauge!(SOROBAN_CONFIG_LEDGER_MAX_READ_LEDGER_BYTE).set(info.ledger_max_read_bytes as f64);
        gauge!(SOROBAN_CONFIG_LEDGER_MAX_WRITE_ENTRY)
            .set(info.ledger_max_write_ledger_entries as f64);
        gauge!(SOROBAN_CONFIG_LEDGER_MAX_WRITE_LEDGER_BYTE).set(info.ledger_max_write_bytes as f64);
        gauge!(SOROBAN_CONFIG_BUCKET_LIST_TARGET_SIZE_BYTE)
            .set(info.state_target_size_bytes.max(0) as f64);
        gauge!(SOROBAN_CONFIG_FEE_WRITE_1KB).set(info.fee_write_1kb.max(0) as f64);
    }

    // Phase 3: Henyey-specific observability.
    gauge!(LEDGER_SOROBAN_EXEC_US).set(snap.soroban_exec_us as f64);
    gauge!(LEDGER_CLASSIC_EXEC_US).set(snap.classic_exec_us as f64);
    gauge!(LEDGER_BUCKET_CACHE_HIT_RATIO).set(snap.bucket_cache_hit_ratio);
    gauge!(LEDGER_SNAPSHOT_CACHE_HIT_RATIO).set(snap.snapshot_cache_hit_ratio);
    gauge!(LEDGER_SNAPSHOT_CACHE_FALLBACK_LOOKUPS).set(snap.snapshot_cache_fallback_lookups as f64);

    // Phase 4: Overlay connection breakdown.
    if let Some(breakdown) = state.app.overlay_connection_breakdown().await {
        gauge!(OVERLAY_INBOUND_AUTHENTICATED).set(breakdown.inbound_authenticated as f64);
        gauge!(OVERLAY_OUTBOUND_AUTHENTICATED).set(breakdown.outbound_authenticated as f64);
        gauge!(OVERLAY_INBOUND_PENDING).set(breakdown.inbound_pending as f64);
        gauge!(OVERLAY_OUTBOUND_PENDING).set(breakdown.outbound_pending as f64);
    } else {
        gauge!(OVERLAY_INBOUND_AUTHENTICATED).set(0.0);
        gauge!(OVERLAY_OUTBOUND_AUTHENTICATED).set(0.0);
        gauge!(OVERLAY_INBOUND_PENDING).set(0.0);
        gauge!(OVERLAY_OUTBOUND_PENDING).set(0.0);
    }

    // Phase 4: Process health (Linux-only).
    if let Some(fds) = process_open_fds() {
        gauge!(PROCESS_OPEN_FDS).set(fds as f64);
    }
    if let Some(max) = process_max_fds() {
        gauge!(PROCESS_MAX_FDS).set(max as f64);
    }

    // Phase 4: Quorum health.
    if let Some(qh) = state.app.quorum_health() {
        gauge!(QUORUM_AGREE).set(qh.agree as f64);
        gauge!(QUORUM_MISSING).set(qh.missing as f64);
        gauge!(QUORUM_DISAGREE).set(qh.disagree as f64);
        gauge!(QUORUM_FAIL_AT).set(qh.fail_at as f64);
    } else {
        gauge!(QUORUM_AGREE).set(0.0);
        gauge!(QUORUM_MISSING).set(0.0);
        gauge!(QUORUM_DISAGREE).set(0.0);
        gauge!(QUORUM_FAIL_AT).set(0.0);
    }

    // Phase 4: SCP timing.
    if let Some(timing) = state.app.scp_timing() {
        if let Some(ext_secs) = timing.externalize_duration_secs {
            gauge!(SCP_TIMING_EXTERNALIZED_SECONDS).set(ext_secs);
        }
        if let Some(nom_secs) = timing.nomination_duration_secs {
            gauge!(SCP_TIMING_NOMINATED_SECONDS).set(nom_secs);
        } else {
            gauge!(SCP_TIMING_NOMINATED_SECONDS).set(0.0);
        }
    } else {
        // No timing available (e.g., after catchup cleared it). Reset gauges.
        gauge!(SCP_TIMING_EXTERNALIZED_SECONDS).set(0.0);
        gauge!(SCP_TIMING_NOMINATED_SECONDS).set(0.0);
    }

    // Phase 5: Archive cache absolute counters.
    counter!(ARCHIVE_CACHE_FRESH_TOTAL).absolute(snap.archive_cache_fresh);
    counter!(ARCHIVE_CACHE_STALE_TOTAL).absolute(snap.archive_cache_stale);
    counter!(ARCHIVE_CACHE_COLD_TOTAL).absolute(snap.archive_cache_cold);
    counter!(ARCHIVE_CACHE_REFRESH_SUCCESS_TOTAL).absolute(snap.archive_cache_refresh_success);
    counter!(ARCHIVE_CACHE_REFRESH_ERROR_TOTAL).absolute(snap.archive_cache_refresh_error);
    counter!(ARCHIVE_CACHE_REFRESH_TIMEOUT_TOTAL).absolute(snap.archive_cache_refresh_timeout);

    // Phase 5: Archive cache gauges.
    gauge!(ARCHIVE_CACHE_AGE_SECONDS).set(snap.archive_cache_age_secs);
    gauge!(ARCHIVE_CACHE_POPULATED).set(if snap.archive_cache_populated {
        1.0
    } else {
        0.0
    });
}

// ── Process health helpers ──────────────────────────────────────────────

/// Returns the number of open file descriptors, or `None` on non-Linux
/// or if `/proc/self/fd` is inaccessible.
#[cfg(target_os = "linux")]
fn process_open_fds() -> Option<u64> {
    // Note: reading /proc/self/fd itself opens an FD, so count may be off by
    // one, but this is standard practice and the error is negligible.
    std::fs::read_dir("/proc/self/fd")
        .ok()
        .map(|d| d.count() as u64)
}

#[cfg(not(target_os = "linux"))]
fn process_open_fds() -> Option<u64> {
    None
}

/// Returns the soft RLIMIT_NOFILE limit, or `None` on non-Linux.
#[cfg(target_os = "linux")]
fn process_max_fds() -> Option<u64> {
    let mut rlim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes into a valid rlimit struct.
    let ret = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) };
    if ret == 0 && rlim.rlim_cur != libc::RLIM_INFINITY {
        Some(rlim.rlim_cur)
    } else if ret == 0 {
        // RLIM_INFINITY — don't export a misleading number
        None
    } else {
        None
    }
}

#[cfg(not(target_os = "linux"))]
fn process_max_fds() -> Option<u64> {
    None
}

// ── Test helper ────────────────────────────────────────────────────────

/// Install the metrics recorder once per test process and return a handle.
///
/// The `metrics` global recorder is one-shot per process. Since Rust tests
/// run in parallel within the same process, this helper ensures the recorder
/// is installed exactly once. Subsequent calls return the same handle.
pub fn ensure_test_recorder() -> &'static PrometheusHandle {
    static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
    HANDLE.get_or_init(|| {
        PrometheusBuilder::new()
            .set_buckets(&[0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 30.0])
            .expect("valid histogram buckets")
            .install_recorder()
            .expect("metrics recorder should install successfully")
    })
}

/// Install the production metrics recorder.
///
/// Returns the `PrometheusHandle` for use by the `/metrics` endpoint.
///
/// This is safe to call from `main()` before constructing `RunOptions`, or
/// from `run_node()` as a fallback when no handle was provided. If a
/// recorder is already installed (e.g. by a library consumer), this panics
/// — callers who need tolerance for that should pre-install and pass the
/// handle via `RunOptions::prometheus_handle`.
pub fn install_recorder() -> PrometheusHandle {
    let handle = PrometheusBuilder::new()
        .set_buckets(&[0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 30.0])
        .expect("valid histogram buckets")
        .set_buckets_for_metric(
            Matcher::Full(LEDGER_CLOSE_DURATION_SECONDS.to_string()),
            &[0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0],
        )
        .expect("valid ledger close histogram buckets")
        .install_recorder()
        .expect("metrics recorder should install successfully");
    describe_metrics();
    register_label_series();
    handle
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_describe_metrics_registers_help_lines() {
        let handle = ensure_test_recorder();
        describe_metrics();
        register_label_series();
        let output = handle.render();

        // Verify HELP lines are present for key metrics.
        assert!(
            output.contains("# HELP stellar_ledger_sequence"),
            "missing HELP for ledger_sequence"
        );
        assert!(
            output.contains("# HELP stellar_uptime_seconds"),
            "missing HELP for uptime_seconds"
        );
        assert!(
            output.contains("# HELP henyey_scp_prefilter_rejects_total"),
            "missing HELP for prefilter_rejects_total"
        );
        assert!(
            output.contains("# HELP henyey_scp_post_verify_total"),
            "missing HELP for post_verify_total"
        );
        assert!(
            output.contains("# HELP henyey_post_catchup_hard_reset_total"),
            "missing HELP for hard_reset_total"
        );

        // Phase 1: SCP/herder.
        assert!(
            output.contains("# HELP stellar_scp_envelope_emit_total"),
            "missing HELP for scp_envelope_emit_total"
        );
        assert!(
            output.contains("# HELP stellar_herder_state"),
            "missing HELP for herder_state"
        );
        assert!(
            output.contains("# HELP stellar_herder_pending_envelopes"),
            "missing HELP for herder_pending_envelopes"
        );

        // Phase 1: Bucket merge.
        assert!(
            output.contains("# HELP stellar_bucket_merge_completed_total"),
            "missing HELP for bucket_merge_completed_total"
        );
        assert!(
            output.contains("# HELP stellar_bucket_merge_shadowed_total"),
            "missing HELP for bucket_merge_shadowed_total"
        );

        // Phase 1: Overlay.
        assert!(
            output.contains("# HELP stellar_overlay_message_read_total"),
            "missing HELP for overlay_message_read_total"
        );
        assert!(
            output.contains("# HELP stellar_overlay_timeout_idle_total"),
            "missing HELP for overlay_timeout_idle_total"
        );
    }

    #[test]
    fn test_type_annotations_present() {
        let handle = ensure_test_recorder();
        describe_metrics();
        register_label_series();
        let output = handle.render();

        // Gauges must have TYPE gauge.
        assert!(
            output.contains("# TYPE stellar_ledger_sequence gauge"),
            "missing TYPE gauge for ledger_sequence"
        );
        assert!(
            output.contains("# TYPE stellar_peer_count gauge"),
            "missing TYPE gauge for peer_count"
        );
        assert!(
            output.contains("# TYPE henyey_scp_verify_latency_us_sum gauge"),
            "missing TYPE gauge for latency_us_sum"
        );
        assert!(
            output.contains("# TYPE henyey_scp_verify_latency_us_count gauge"),
            "missing TYPE gauge for latency_us_count"
        );

        // Counters must have TYPE counter.
        assert!(
            output.contains("# TYPE stellar_uptime_seconds gauge"),
            "missing TYPE gauge for uptime_seconds"
        );
        assert!(
            output.contains("# TYPE henyey_scp_prefilter_rejects_total counter"),
            "missing TYPE counter for prefilter_rejects_total"
        );
        assert!(
            output.contains("# TYPE henyey_post_catchup_hard_reset_total counter"),
            "missing TYPE counter for hard_reset_total"
        );

        // Phase 1: SCP/herder types.
        assert!(
            output.contains("# TYPE stellar_scp_envelope_emit_total counter"),
            "missing TYPE counter for scp_envelope_emit_total"
        );
        assert!(
            output.contains("# TYPE stellar_herder_state gauge"),
            "missing TYPE gauge for herder_state"
        );
        assert!(
            output.contains("# TYPE stellar_herder_pending_received_total counter"),
            "missing TYPE counter for herder_pending_received_total"
        );

        // Phase 1: Bucket merge types.
        assert!(
            output.contains("# TYPE stellar_bucket_merge_completed_total counter"),
            "missing TYPE counter for bucket_merge_completed_total"
        );
        assert!(
            output.contains("# TYPE stellar_bucket_merge_annihilated_total counter"),
            "missing TYPE counter for bucket_merge_annihilated_total"
        );

        // Phase 1: Overlay types.
        assert!(
            output.contains("# TYPE stellar_overlay_message_read_total counter"),
            "missing TYPE counter for overlay_message_read_total"
        );
        assert!(
            output.contains("# TYPE stellar_overlay_error_write_total counter"),
            "missing TYPE counter for overlay_error_write_total"
        );
    }

    #[test]
    fn test_labeled_series_present_at_zero() {
        let handle = ensure_test_recorder();
        describe_metrics();
        register_label_series();
        let output = handle.render();

        // All prefilter reason labels should appear.
        use henyey_herder::scp_verify::PreFilterRejectReason;
        for reason in PreFilterRejectReason::ALL {
            let label = format!(
                "henyey_scp_prefilter_rejects_total{{reason=\"{}\"}}",
                reason.label()
            );
            assert!(
                output.contains(&label),
                "missing pre-registered label for reason={}; output:\n{}",
                reason.label(),
                output
            );
        }

        // All post-verify reason labels should appear.
        use henyey_herder::scp_verify::PostVerifyReason;
        for reason in PostVerifyReason::ALL {
            let label = format!(
                "henyey_scp_post_verify_total{{reason=\"{}\"}}",
                reason.label()
            );
            assert!(
                output.contains(&label),
                "missing pre-registered label for reason={}; output:\n{}",
                reason.label(),
                output
            );
        }
    }

    #[test]
    fn test_recorder_install_is_idempotent() {
        // Both calls should return the same handle without panicking.
        let h1 = ensure_test_recorder();
        let h2 = ensure_test_recorder();
        // They should be the same reference (OnceLock returns the same value).
        assert!(std::ptr::eq(h1, h2));
    }

    #[test]
    fn test_removed_overlay_metrics_absent() {
        let handle = ensure_test_recorder();
        describe_metrics();
        register_label_series();
        let output = handle.render();

        // These overlay metrics were intentionally removed because they lack
        // production producers. Verify they don't silently reappear.
        let removed = [
            "stellar_overlay_message_drop_total",
            "stellar_overlay_byte_read_total",
            "stellar_overlay_byte_write_total",
            "stellar_overlay_pending_peers",
            "stellar_overlay_authenticated_peers",
            "stellar_overlay_flood_demanded_total",
            "stellar_overlay_flood_fulfilled_total",
            "stellar_overlay_demand_timeout_total",
            "stellar_overlay_connection_latency_us_sum",
            "stellar_overlay_connection_latency_us_count",
            "stellar_overlay_tx_pull_latency_us_sum",
            "stellar_overlay_tx_pull_latency_us_count",
        ];
        for name in &removed {
            assert!(
                !output.contains(&format!("# HELP {}", name)),
                "metric {} should not be registered (no production producer)",
                name
            );
        }
    }

    #[test]
    fn test_phase2_ledger_metrics_present() {
        let handle = ensure_test_recorder();
        describe_metrics();
        register_label_series();
        let output = handle.render();

        // Ledger close gauges.
        let ledger_metrics = [
            LEDGER_CLOSE_TIME_MS,
            LEDGER_TX_COUNT,
            LEDGER_OP_COUNT,
            LEDGER_TX_SUCCESS_COUNT,
            LEDGER_TX_FAILED_COUNT,
            LEDGER_TOTAL_FEES,
            LEDGER_ENTRIES_CREATED,
            LEDGER_ENTRIES_UPDATED,
            LEDGER_ENTRIES_DELETED,
            LEDGER_APPLY_US,
        ];
        for name in &ledger_metrics {
            assert!(
                output.contains(&format!("# TYPE {} gauge", name)),
                "missing TYPE gauge for {}",
                name
            );
            assert!(
                output.contains(&format!("# HELP {}", name)),
                "missing HELP for {}",
                name
            );
        }
    }

    #[test]
    fn test_phase2_herder_metrics_present() {
        let handle = ensure_test_recorder();
        describe_metrics();
        register_label_series();
        let output = handle.render();

        // Tx queue gauges.
        let queue_gauges = [
            HERDER_TX_QUEUE_ACCOUNTS,
            HERDER_TX_QUEUE_BANNED,
            HERDER_TX_QUEUE_SEEN,
        ];
        for name in &queue_gauges {
            assert!(
                output.contains(&format!("# TYPE {} gauge", name)),
                "missing TYPE gauge for {}",
                name
            );
        }

        // Pending envelope counters.
        let pending_counters = [HERDER_PENDING_ADDED_TOTAL, HERDER_PENDING_RELEASED_TOTAL];
        for name in &pending_counters {
            assert!(
                output.contains(&format!("# TYPE {} counter", name)),
                "missing TYPE counter for {}",
                name
            );
        }
    }

    #[test]
    fn test_phase2_drift_metrics_present() {
        let handle = ensure_test_recorder();
        describe_metrics();
        register_label_series();
        let output = handle.render();

        let drift_metrics = [
            DRIFT_MIN_SECONDS,
            DRIFT_MAX_SECONDS,
            DRIFT_MEDIAN_SECONDS,
            DRIFT_P75_SECONDS,
            DRIFT_SAMPLE_COUNT,
        ];
        for name in &drift_metrics {
            assert!(
                output.contains(&format!("# TYPE {} gauge", name)),
                "missing TYPE gauge for {}",
                name
            );
            // All drift gauges should be pre-registered at 0.
            assert!(
                output.contains(&format!("{} 0", name)),
                "drift metric {} should be pre-registered at 0",
                name
            );
        }
    }

    #[test]
    fn test_phase2_ledger_metrics_pre_registered_at_zero() {
        let handle = ensure_test_recorder();
        describe_metrics();
        register_label_series();
        let output = handle.render();

        // Before any close, all ledger gauges should be 0.
        let zero_metrics = [
            LEDGER_CLOSE_TIME_MS,
            LEDGER_TX_COUNT,
            LEDGER_OP_COUNT,
            LEDGER_TX_SUCCESS_COUNT,
            LEDGER_TX_FAILED_COUNT,
            LEDGER_TOTAL_FEES,
            LEDGER_ENTRIES_CREATED,
            LEDGER_ENTRIES_UPDATED,
            LEDGER_ENTRIES_DELETED,
            LEDGER_APPLY_US,
        ];
        for name in &zero_metrics {
            assert!(
                output.contains(&format!("{} 0", name)),
                "ledger metric {} should be pre-registered at 0; output:\n{}",
                name,
                output
            );
        }
    }

    #[test]
    fn test_jemalloc_metrics_present() {
        let handle = ensure_test_recorder();
        describe_metrics();
        register_label_series();
        let output = handle.render();

        let jemalloc_metrics = [
            JEMALLOC_ALLOCATED_BYTES,
            JEMALLOC_ACTIVE_BYTES,
            JEMALLOC_RESIDENT_BYTES,
            JEMALLOC_MAPPED_BYTES,
            JEMALLOC_RETAINED_BYTES,
            JEMALLOC_FRAGMENTATION_PCT,
        ];
        for name in &jemalloc_metrics {
            assert!(
                output.contains(&format!("# TYPE {} gauge", name)),
                "missing TYPE gauge for {}",
                name
            );
            assert!(
                output.contains(&format!("# HELP {}", name)),
                "missing HELP for {}",
                name
            );
            // Pre-registered at 0.
            assert!(
                output.contains(&format!("{} 0", name)),
                "jemalloc metric {} should be pre-registered at 0",
                name
            );
        }
    }

    #[test]
    fn test_phase3_ledger_apply_counters_present() {
        let handle = ensure_test_recorder();
        describe_metrics();
        register_label_series();
        let output = handle.render();

        // True cumulative counters.
        let counter_metrics = [
            LEDGER_APPLY_SUCCESS_TOTAL,
            LEDGER_APPLY_FAILURE_TOTAL,
            LEDGER_APPLY_SOROBAN_SUCCESS_TOTAL,
            LEDGER_APPLY_SOROBAN_FAILURE_TOTAL,
        ];
        for name in &counter_metrics {
            assert!(
                output.contains(&format!("# TYPE {} counter", name)),
                "missing TYPE counter for {}",
                name
            );
            assert!(
                output.contains(&format!("# HELP {}", name)),
                "missing HELP for {}",
                name
            );
            // Pre-registered at 0.
            assert!(
                output.contains(&format!("{} 0", name)),
                "phase3 counter {} should be pre-registered at 0",
                name
            );
        }

        // Snapshot gauges (not monotonic — fixed from counter to gauge).
        let gauge_metrics = [
            LEDGER_APPLY_SOROBAN_MAX_CLUSTERS,
            LEDGER_APPLY_SOROBAN_STAGES,
            LEDGER_AGE_CURRENT_SECONDS,
        ];
        for name in &gauge_metrics {
            assert!(
                output.contains(&format!("# TYPE {} gauge", name)),
                "missing TYPE gauge for {}",
                name
            );
            assert!(
                output.contains(&format!("# HELP {}", name)),
                "missing HELP for {}",
                name
            );
            assert!(
                output.contains(&format!("{} 0", name)),
                "phase3 gauge {} should be pre-registered at 0",
                name
            );
        }
    }

    #[test]
    fn test_phase3_henyey_specific_gauges_present() {
        let handle = ensure_test_recorder();
        describe_metrics();
        register_label_series();
        let output = handle.render();

        let gauge_metrics = [
            LEDGER_SOROBAN_EXEC_US,
            LEDGER_CLASSIC_EXEC_US,
            LEDGER_BUCKET_CACHE_HIT_RATIO,
            LEDGER_SNAPSHOT_CACHE_HIT_RATIO,
            LEDGER_SNAPSHOT_CACHE_FALLBACK_LOOKUPS,
        ];
        for name in &gauge_metrics {
            assert!(
                output.contains(&format!("# TYPE {} gauge", name)),
                "missing TYPE gauge for {}",
                name
            );
            assert!(
                output.contains(&format!("# HELP {}", name)),
                "missing HELP for {}",
                name
            );
            assert!(
                output.contains(&format!("{} 0", name)),
                "phase3 gauge {} should be pre-registered at 0",
                name
            );
        }
    }

    #[test]
    fn test_phase3_soroban_config_metrics_described() {
        let handle = ensure_test_recorder();
        describe_metrics();
        register_label_series();
        let output = handle.render();

        let config_metrics = [
            SOROBAN_CONFIG_CONTRACT_MAX_RW_KEY_BYTE,
            SOROBAN_CONFIG_CONTRACT_MAX_RW_DATA_BYTE,
            SOROBAN_CONFIG_CONTRACT_MAX_RW_CODE_BYTE,
            SOROBAN_CONFIG_TX_MAX_SIZE_BYTE,
            SOROBAN_CONFIG_TX_MAX_CPU_INSN,
            SOROBAN_CONFIG_TX_MAX_MEM_BYTE,
            SOROBAN_CONFIG_TX_MAX_READ_ENTRY,
            SOROBAN_CONFIG_TX_MAX_READ_LEDGER_BYTE,
            SOROBAN_CONFIG_TX_MAX_WRITE_ENTRY,
            SOROBAN_CONFIG_TX_MAX_WRITE_LEDGER_BYTE,
            SOROBAN_CONFIG_TX_MAX_EMIT_EVENT_BYTE,
            SOROBAN_CONFIG_LEDGER_MAX_TX_COUNT,
            SOROBAN_CONFIG_LEDGER_MAX_CPU_INSN,
            SOROBAN_CONFIG_LEDGER_MAX_TXS_SIZE_BYTE,
            SOROBAN_CONFIG_LEDGER_MAX_READ_ENTRY,
            SOROBAN_CONFIG_LEDGER_MAX_READ_LEDGER_BYTE,
            SOROBAN_CONFIG_LEDGER_MAX_WRITE_ENTRY,
            SOROBAN_CONFIG_LEDGER_MAX_WRITE_LEDGER_BYTE,
            SOROBAN_CONFIG_BUCKET_LIST_TARGET_SIZE_BYTE,
            SOROBAN_CONFIG_FEE_WRITE_1KB,
        ];
        for name in &config_metrics {
            assert!(
                output.contains(&format!("# TYPE {} gauge", name)),
                "missing TYPE gauge for soroban config metric {}",
                name
            );
            assert!(
                output.contains(&format!("# HELP {}", name)),
                "missing HELP for soroban config metric {}",
                name
            );
        }
    }
}
