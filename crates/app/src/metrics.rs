//! Prometheus metrics facade for the henyey node.
//!
//! This module is the single source of truth for all metric names, types,
//! descriptions, and scrape-time refresh logic. It wraps the [`metrics`] crate
//! facade to provide:
//!
//! - Centralized metric name constants via [`metric_catalog!`]
//! - One-time HELP/TYPE registration via [`describe_metrics`]
//! - Pre-registration of all series at zero via [`register_label_series`]
//! - Scrape-time gauge refresh via [`refresh_gauges`]
//! - A once-per-process test helper via [`ensure_test_recorder`]
//!
//! Adding a new metric requires a single entry in the [`metric_catalog!`]
//! invocation below. The macro generates the public constant, description,
//! pre-registration, and test inventory from that one declaration.

use std::sync::OnceLock;

use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

use crate::http::ServerState;

// ── Declarative metric catalog ─────────────────────────────────────────
//
// The `metric_catalog!` macro accepts metrics grouped by kind. Each entry
// declares the Rust constant name, Prometheus metric name, and HELP text.
//
// Supported kinds:
//   gauges          — pre-registered at 0 by default
//   gauges_no_prereg — described only (not pre-registered at startup)
//   counters        — pre-registered at 0 by default
//   counters_no_prereg — described only (not pre-registered)
//   labeled_counters_enum    — pre-registered for each enum label variant
//   labeled_counters_literal — pre-registered for each literal label value
//   histograms               — described only (never pre-registered)

macro_rules! metric_catalog {
    (
        gauges {
            $( $g_name:ident = $g_prom:expr => $g_help:expr ; )*
        }
        gauges_no_prereg {
            $( $gnp_name:ident = $gnp_prom:expr => $gnp_help:expr ; )*
        }
        counters {
            $( $c_name:ident = $c_prom:expr => $c_help:expr ; )*
        }
        counters_no_prereg {
            $( $cnp_name:ident = $cnp_prom:expr => $cnp_help:expr ; )*
        }
        labeled_counters_enum {
            $( $lce_name:ident = $lce_prom:expr => $lce_help:expr,
                $lce_key:expr, $lce_enum:ty ; )*
        }
        labeled_counters_literal {
            $( $lcl_name:ident = $lcl_prom:expr => $lcl_help:expr,
                $lcl_key:expr, [ $( $lcl_val:expr ),+ $(,)? ] ; )*
        }
        histograms {
            $( $h_name:ident = $h_prom:expr => $h_help:expr ; )*
        }
    ) => {
        // ── 1. Public constants ────────────────────────────────────────
        $( pub const $g_name: &str = $g_prom; )*
        $( pub const $gnp_name: &str = $gnp_prom; )*
        $( pub const $c_name: &str = $c_prom; )*
        $( pub const $cnp_name: &str = $cnp_prom; )*
        $( pub const $lce_name: &str = $lce_prom; )*
        $( pub const $lcl_name: &str = $lcl_prom; )*
        $( pub const $h_name: &str = $h_prom; )*

        // ── 2. describe_metrics() ──────────────────────────────────────
        /// Register HELP/TYPE annotations for all metrics.
        ///
        /// Must be called **after** the global recorder is installed.
        pub fn describe_metrics() {
            $( describe_gauge!($g_name, $g_help); )*
            $( describe_gauge!($gnp_name, $gnp_help); )*
            $( describe_counter!($c_name, $c_help); )*
            $( describe_counter!($cnp_name, $cnp_help); )*
            $( describe_counter!($lce_name, $lce_help); )*
            $( describe_counter!($lcl_name, $lcl_help); )*
            $( describe_histogram!($h_name, $h_help); )*
        }

        // ── 3. register_label_series() ─────────────────────────────────
        /// Pre-register all metrics with initial values so that every metric
        /// appears on the very first scrape with its HELP/TYPE annotations.
        ///
        /// Must be called **after** [`describe_metrics`].
        pub fn register_label_series() {
            // Gauges — zero-init.
            $( gauge!($g_name).set(0.0); )*
            // gauges_no_prereg: intentionally skipped.
            // Counters — zero-init.
            $( counter!($c_name).absolute(0); )*
            // counters_no_prereg: intentionally skipped.
            // Labeled counters (enum-backed) — iterate ALL variants.
            $( for v in <$lce_enum>::ALL {
                counter!($lce_name, $lce_key => v.label()).absolute(0);
            } )*
            // Labeled counters (literal-backed).
            $( for v in &[ $( $lcl_val ),+ ] {
                counter!($lcl_name, $lcl_key => *v).absolute(0);
            } )*
            // Histograms: never pre-registered.
        }

        // ── 4. Test arrays ─────────────────────────────────────────────
        #[cfg(test)]
        pub(crate) mod catalog_arrays {
            /// All gauge metric names (including no_preregister).
            pub const ALL_GAUGE_NAMES: &[&str] = &[
                $( $g_prom, )*
                $( $gnp_prom, )*
            ];
            /// All counter metric names (including labeled and no_preregister).
            pub const ALL_COUNTER_NAMES: &[&str] = &[
                $( $c_prom, )*
                $( $cnp_prom, )*
                $( $lce_prom, )*
                $( $lcl_prom, )*
            ];
            /// All histogram metric names.
            pub const ALL_HISTOGRAM_NAMES: &[&str] = &[
                $( $h_prom, )*
            ];
            /// Gauges that are pre-registered at zero (excludes no_preregister).
            pub const ALL_PREREGISTERED_GAUGE_NAMES: &[&str] = &[
                $( $g_prom, )*
            ];
            /// Counters that are pre-registered at zero (unlabeled only, excludes no_preregister).
            pub const ALL_PREREGISTERED_COUNTER_NAMES: &[&str] = &[
                $( $c_prom, )*
            ];
        }
    };
}

// ── Metric catalog ─────────────────────────────────────────────────────

metric_catalog! {
    gauges {
        // Stellar-compatible gauges.
        LEDGER_SEQUENCE = "stellar_ledger_sequence"
            => "Current ledger sequence number";
        PEER_COUNT = "stellar_peer_count"
            => "Number of connected peers";
        PENDING_TRANSACTIONS = "stellar_pending_transactions"
            => "Number of pending transactions";
        UPTIME_SECONDS = "stellar_uptime_seconds"
            => "Node uptime in seconds";
        IS_VALIDATOR = "stellar_is_validator"
            => "Whether this node is a validator";

        // Stage A: Health Summary & Version gauges (10334 dashboard).
        STARTED_ON = "stellar_started_on"
            => "Unix epoch timestamp of process start";
        LEDGER_VERSION = "stellar_ledger_version"
            => "Current ledger header protocol version";
        PROTOCOL_VERSION = "stellar_protocol_version"
            => "Max supported protocol version (configured)";

        // SCP verify pipeline gauges.
        SCP_VERIFY_INPUT_BACKLOG = "henyey_scp_verify_input_backlog"
            => "Current depth of the SCP signature-verify input channel (event-loop sampled)";
        SCP_VERIFY_OUTPUT_BACKLOG = "henyey_scp_verify_output_backlog"
            => "Current depth of the verified-envelope output channel (envelopes awaiting the event loop)";
        SCP_VERIFIER_THREAD_STATE = "henyey_scp_verifier_thread_state"
            => "Worker thread state (0=Running, 1=Stopping, 2=Dead)";
        // SCP verify latency — formerly exposed as a synthetic Prometheus
        // "summary". The `metrics` crate has no native summary type, so
        // these are now separate gauges.
        SCP_VERIFY_LATENCY_US_SUM = "henyey_scp_verify_latency_us_sum"
            => "Enqueue-to-post-verify latency microseconds (cumulative sum)";
        SCP_VERIFY_LATENCY_US_COUNT = "henyey_scp_verify_latency_us_count"
            => "Enqueue-to-post-verify latency sample count";

        // Overlay fetch channel.
        OVERLAY_FETCH_CHANNEL_DEPTH = "henyey_overlay_fetch_channel_depth"
            => "Current depth of the overlay fetch-response channel (event-loop sampled)";
        OVERLAY_FETCH_CHANNEL_DEPTH_MAX = "henyey_overlay_fetch_channel_depth_max"
            => "Monotonic maximum depth of the overlay fetch-response channel since process start";

        // SCP/herder gauges.
        HERDER_STATE = "stellar_herder_state"
            => "Herder state (0=Booting, 1=Syncing, 2=Tracking)";
        HERDER_PENDING_ENVELOPES = "stellar_herder_pending_envelopes"
            => "SCP envelopes queued in pending pool";
        HERDER_CACHED_TX_SETS = "stellar_herder_cached_tx_sets"
            => "Transaction sets cached for pending SCP slots";

        // Ledger close gauges (Phase 2).
        LEDGER_TX_COUNT = "stellar_ledger_tx_count"
            => "Transactions processed in the last close";
        LEDGER_OP_COUNT = "stellar_ledger_op_count"
            => "Operations executed in the last close";
        LEDGER_TX_SUCCESS_COUNT = "stellar_ledger_tx_success_count"
            => "Successful transactions in the last close";
        LEDGER_TX_FAILED_COUNT = "stellar_ledger_tx_failed_count"
            => "Failed transactions in the last close";
        LEDGER_TOTAL_FEES = "stellar_ledger_total_fees"
            => "Fees collected in the last close (stroops)";
        LEDGER_ENTRIES_CREATED = "stellar_ledger_entries_created"
            => "Ledger entries created in the last close";
        LEDGER_ENTRIES_UPDATED = "stellar_ledger_entries_updated"
            => "Ledger entries updated in the last close";
        LEDGER_ENTRIES_DELETED = "stellar_ledger_entries_deleted"
            => "Ledger entries deleted in the last close";
        LEDGER_APPLY_US = "stellar_ledger_apply_us"
            => "Total ledger close time in microseconds";

        // Herder tx queue gauges (Phase 2).
        HERDER_TX_QUEUE_ACCOUNTS = "stellar_herder_tx_queue_accounts"
            => "Accounts with pending transactions in the queue";
        HERDER_TX_QUEUE_BANNED = "stellar_herder_tx_queue_banned"
            => "Currently banned transactions in the queue";
        HERDER_TX_QUEUE_SEEN = "stellar_herder_tx_queue_seen"
            => "Deduplicated transaction hashes in the seen set";
        HERDER_ARB_TX_SEEN = "stellar_herder_arb_tx_seen_total"
            => "Total arbitrage (looping path-payment) transactions evaluated for broadcast";
        HERDER_ARB_TX_DROPPED = "stellar_herder_arb_tx_dropped_total"
            => "Total arbitrage transactions dropped by flood damping";

        // Herder pending-tx age gauges (Stage D).
        HERDER_PENDING_TXS_AGE0 = "stellar_herder_pending_txs_age0"
            => "Pending transactions at age 0 (current slot)";
        HERDER_PENDING_TXS_AGE1 = "stellar_herder_pending_txs_age1"
            => "Pending transactions at age 1";
        HERDER_PENDING_TXS_AGE2 = "stellar_herder_pending_txs_age2"
            => "Pending transactions at age 2";
        HERDER_PENDING_TXS_AGE3 = "stellar_herder_pending_txs_age3"
            => "Pending transactions at age 3+";

        // Clock drift gauges (Phase 2).
        DRIFT_MIN_SECONDS = "henyey_herder_drift_min_seconds"
            => "Minimum close time drift in the last completed window (seconds)";
        DRIFT_MAX_SECONDS = "henyey_herder_drift_max_seconds"
            => "Maximum close time drift in the last completed window (seconds)";
        DRIFT_MEDIAN_SECONDS = "henyey_herder_drift_median_seconds"
            => "Median close time drift in the last completed window (seconds)";
        DRIFT_P75_SECONDS = "henyey_herder_drift_p75_seconds"
            => "75th percentile close time drift in the last completed window (seconds)";
        DRIFT_SAMPLE_COUNT = "henyey_herder_drift_sample_count"
            => "Number of samples in the last completed drift window";

        // jemalloc allocator gauges.
        JEMALLOC_ALLOCATED_BYTES = "henyey_jemalloc_allocated_bytes"
            => "Bytes allocated by the application (jemalloc stats.allocated)";
        JEMALLOC_ACTIVE_BYTES = "henyey_jemalloc_active_bytes"
            => "Bytes in active pages (jemalloc stats.active)";
        JEMALLOC_RESIDENT_BYTES = "henyey_jemalloc_resident_bytes"
            => "Bytes resident in physical memory (jemalloc stats.resident)";
        JEMALLOC_MAPPED_BYTES = "henyey_jemalloc_mapped_bytes"
            => "Total bytes mapped by the allocator (jemalloc stats.mapped)";
        JEMALLOC_RETAINED_BYTES = "henyey_jemalloc_retained_bytes"
            => "Bytes retained by the allocator (jemalloc stats.retained)";
        JEMALLOC_FRAGMENTATION_PCT = "henyey_jemalloc_fragmentation_pct"
            => "Allocator fragmentation: (resident - allocated) / allocated * 100";

        // Phase 3: Ledger apply snapshot gauges.
        LEDGER_APPLY_SOROBAN_MAX_CLUSTERS = "stellar_ledger_apply_soroban_max_clusters"
            => "Max dependent tx clusters in the last Soroban ledger close";
        LEDGER_APPLY_SOROBAN_STAGES = "stellar_ledger_apply_soroban_stages"
            => "Number of parallel stages in the last Soroban ledger close";
        LEDGER_AGE_CURRENT_SECONDS = "stellar_ledger_age_current_seconds"
            => "Seconds since the last ledger close";

        // Phase 3: Soroban network config limits.
        SOROBAN_CONFIG_CONTRACT_MAX_RW_KEY_BYTE = "stellar_soroban_config_contract_max_rw_key_byte"
            => "Soroban config: max contract data key size in bytes";
        SOROBAN_CONFIG_CONTRACT_MAX_RW_DATA_BYTE = "stellar_soroban_config_contract_max_rw_data_byte"
            => "Soroban config: max contract data entry size in bytes";
        SOROBAN_CONFIG_CONTRACT_MAX_RW_CODE_BYTE = "stellar_soroban_config_contract_max_rw_code_byte"
            => "Soroban config: max contract code size in bytes";
        SOROBAN_CONFIG_TX_MAX_SIZE_BYTE = "stellar_soroban_config_tx_max_size_byte"
            => "Soroban config: max transaction size in bytes";
        SOROBAN_CONFIG_TX_MAX_CPU_INSN = "stellar_soroban_config_tx_max_cpu_insn"
            => "Soroban config: max CPU instructions per transaction";
        SOROBAN_CONFIG_TX_MAX_MEM_BYTE = "stellar_soroban_config_tx_max_mem_byte"
            => "Soroban config: max memory per transaction in bytes";
        SOROBAN_CONFIG_TX_MAX_READ_ENTRY = "stellar_soroban_config_tx_max_read_entry"
            => "Soroban config: max read entries per transaction";
        SOROBAN_CONFIG_TX_MAX_READ_LEDGER_BYTE = "stellar_soroban_config_tx_max_read_ledger_byte"
            => "Soroban config: max read bytes per transaction";
        SOROBAN_CONFIG_TX_MAX_WRITE_ENTRY = "stellar_soroban_config_tx_max_write_entry"
            => "Soroban config: max write entries per transaction";
        SOROBAN_CONFIG_TX_MAX_WRITE_LEDGER_BYTE = "stellar_soroban_config_tx_max_write_ledger_byte"
            => "Soroban config: max write bytes per transaction";
        SOROBAN_CONFIG_TX_MAX_EMIT_EVENT_BYTE = "stellar_soroban_config_tx_max_emit_event_byte"
            => "Soroban config: max event bytes per transaction";
        SOROBAN_CONFIG_LEDGER_MAX_TX_COUNT = "stellar_soroban_config_ledger_max_tx_count"
            => "Soroban config: max transactions per ledger";
        SOROBAN_CONFIG_LEDGER_MAX_CPU_INSN = "stellar_soroban_config_ledger_max_cpu_insn"
            => "Soroban config: max CPU instructions per ledger";
        SOROBAN_CONFIG_LEDGER_MAX_TXS_SIZE_BYTE = "stellar_soroban_config_ledger_max_txs_size_byte"
            => "Soroban config: max total transaction size per ledger";
        SOROBAN_CONFIG_LEDGER_MAX_READ_ENTRY = "stellar_soroban_config_ledger_max_read_entry"
            => "Soroban config: max read entries per ledger";
        SOROBAN_CONFIG_LEDGER_MAX_READ_LEDGER_BYTE = "stellar_soroban_config_ledger_max_read_ledger_byte"
            => "Soroban config: max read bytes per ledger";
        SOROBAN_CONFIG_LEDGER_MAX_WRITE_ENTRY = "stellar_soroban_config_ledger_max_write_entry"
            => "Soroban config: max write entries per ledger";
        SOROBAN_CONFIG_LEDGER_MAX_WRITE_LEDGER_BYTE = "stellar_soroban_config_ledger_max_write_ledger_byte"
            => "Soroban config: max write bytes per ledger";
        SOROBAN_CONFIG_BUCKET_LIST_TARGET_SIZE_BYTE = "stellar_soroban_config_bucket_list_target_size_byte"
            => "Soroban config: bucket list target size in bytes";
        SOROBAN_CONFIG_FEE_WRITE_1KB = "stellar_soroban_config_fee_write_1kb"
            => "Soroban config: fee for 1KB write";

        // Phase 3: Henyey-specific observability.
        LEDGER_BUCKET_CACHE_HIT_RATIO = "henyey_ledger_bucket_cache_hit_ratio"
            => "Per-bucket RandomEvictionCache hit ratio (0.0-1.0, Account entries only, last close)";
        LEDGER_SNAPSHOT_CACHE_HIT_RATIO = "henyey_ledger_snapshot_cache_hit_ratio"
            => "SnapshotHandle local cache hit ratio (0.0-1.0, last close)";
        LEDGER_SNAPSHOT_CACHE_FALLBACK_LOOKUPS = "henyey_ledger_snapshot_cache_fallback_lookups"
            => "Lookups dispatched to fallback (not served by snapshot local caches, last close)";

        // Phase 4: Overlay connection breakdown.
        OVERLAY_INBOUND_AUTHENTICATED = "stellar_overlay_inbound_authenticated"
            => "Authenticated inbound peer connections";
        OVERLAY_OUTBOUND_AUTHENTICATED = "stellar_overlay_outbound_authenticated"
            => "Authenticated outbound peer connections";
        OVERLAY_INBOUND_PENDING = "stellar_overlay_inbound_pending"
            => "Pending inbound peer connections (handshaking)";
        OVERLAY_OUTBOUND_PENDING = "stellar_overlay_outbound_pending"
            => "Pending outbound peer connections (handshaking)";

        // Phase 4: Process health.
        PROCESS_OPEN_FDS = "henyey_process_open_fds"
            => "Open file descriptors";
        PROCESS_MAX_FDS = "henyey_process_max_fds"
            => "Maximum file descriptors (RLIMIT_NOFILE)";

        // Phase 4: Quorum health.
        QUORUM_AGREE = "stellar_quorum_agree"
            => "Quorum set nodes in agreement (confirming/externalized)";
        QUORUM_MISSING = "stellar_quorum_missing"
            => "Quorum set nodes not responding";
        QUORUM_DISAGREE = "stellar_quorum_disagree"
            => "Quorum set nodes disagreeing";
        QUORUM_FAIL_AT = "stellar_quorum_fail_at"
            => "Peers that can fail before quorum is lost (via find_closest_v_blocking, excludes self)";
        QUORUM_DELAYED = "stellar_quorum_delayed"
            => "Quorum set nodes lagging behind the local node (subset of agree)";

        // Phase 4: SCP timing.
        SCP_TIMING_EXTERNALIZED_SECONDS = "stellar_scp_timing_externalized_seconds"
            => "Time from slot creation to externalize (seconds, last slot)";
        SCP_TIMING_NOMINATED_SECONDS = "stellar_scp_timing_nominated_seconds"
            => "Time from first local nomination to ballot protocol start (seconds, last slot, matches stellar-core mNominateToPrepare)";
        SCP_TIMING_FIRST_TO_SELF_EXTERNALIZE_SECONDS = "stellar_scp_timing_first_to_self_externalize_seconds"
            => "Time from first observed EXTERNALIZE to self-externalize (seconds, last slot)";

        // Phase 5: Archive cache gauges.
        ARCHIVE_CACHE_AGE_SECONDS = "henyey_archive_cache_age_seconds"
            => "Age of cached archive checkpoint (seconds, 0 when cold)";
        ARCHIVE_CACHE_POPULATED = "henyey_archive_cache_populated"
            => "Whether the archive checkpoint cache has a value (1=populated, 0=cold)";
    }

    gauges_no_prereg {
        // Phase 6: TxSet exhaustion stuck gauge — set conditionally in refresh_gauges.
        RECOVERY_TX_SET_STUCK_SECONDS = "henyey_recovery_tx_set_stuck_seconds"
            => "Seconds since all peers exhausted for pending tx_set hashes (0 = not stuck)";
    }

    counters {
        // SCP verify pipeline.
        SCP_POST_VERIFY_DROPS_TOTAL = "henyey_scp_post_verify_drops_total"
            => "Envelopes dropped after verification (aggregate)";
        POST_CATCHUP_HARD_RESET_TOTAL = "henyey_post_catchup_hard_reset_total"
            => "Total post-catchup hard resets performed";

        // SCP/herder counters.
        SCP_ENVELOPE_EMIT_TOTAL = "stellar_scp_envelope_emit_total"
            => "Total SCP envelopes emitted by the local node";
        SCP_ENVELOPE_RECEIVE_TOTAL = "stellar_scp_envelope_receive_total"
            => "Total SCP envelopes received from peers";
        HERDER_LOST_SYNC_TOTAL = "stellar_herder_lost_sync_total"
            => "Total lost-sync events (transitions out of Tracking)";
        HERDER_PENDING_RECEIVED_TOTAL = "stellar_herder_pending_received_total"
            => "Total envelopes received by pending pool";
        HERDER_PENDING_DUPLICATES_TOTAL = "stellar_herder_pending_duplicates_total"
            => "Envelopes rejected as duplicates by pending pool";
        HERDER_PENDING_TOO_OLD_TOTAL = "stellar_herder_pending_too_old_total"
            => "Envelopes rejected as too old by pending pool";
        HERDER_PENDING_EVICTED_TOTAL = "stellar_herder_pending_evicted_total"
            => "Envelopes evicted from pending pool";

        // Bucket merge counters.
        BUCKET_MERGE_COMPLETED_TOTAL = "stellar_bucket_merge_completed_total"
            => "Total bucket merges completed";
        BUCKET_MERGE_TIME_US_TOTAL = "stellar_bucket_merge_time_us_total"
            => "Total bucket merge time in microseconds";
        BUCKET_MERGE_NEW_LIVE_TOTAL = "stellar_bucket_merge_new_live_total"
            => "Total new LIVEENTRY entries written during merges";
        BUCKET_MERGE_NEW_DEAD_TOTAL = "stellar_bucket_merge_new_dead_total"
            => "Total new DEADENTRY entries written during merges";
        BUCKET_MERGE_NEW_INIT_TOTAL = "stellar_bucket_merge_new_init_total"
            => "Total new INITENTRY entries written during merges";
        BUCKET_MERGE_NEW_META_TOTAL = "stellar_bucket_merge_new_meta_total"
            => "Total new METAENTRY entries written during merges";
        BUCKET_MERGE_SHADOWED_TOTAL = "stellar_bucket_merge_shadowed_total"
            => "Total entries shadowed during merges";
        BUCKET_MERGE_ANNIHILATED_TOTAL = "stellar_bucket_merge_annihilated_total"
            => "Total INIT+DEAD entry pairs annihilated during merges";

        // Overlay counters.
        OVERLAY_MESSAGE_READ_TOTAL = "stellar_overlay_message_read_total"
            => "Total overlay messages read from peers";
        OVERLAY_MESSAGE_WRITE_TOTAL = "stellar_overlay_message_write_total"
            => "Total overlay messages written to peers";
        OVERLAY_MESSAGE_BROADCAST_TOTAL = "stellar_overlay_message_broadcast_total"
            => "Total overlay messages broadcast to all peers";
        OVERLAY_ERROR_READ_TOTAL = "stellar_overlay_error_read_total"
            => "Total overlay read errors";
        OVERLAY_ERROR_WRITE_TOTAL = "stellar_overlay_error_write_total"
            => "Total overlay write errors";
        OVERLAY_TIMEOUT_IDLE_TOTAL = "stellar_overlay_timeout_idle_total"
            => "Total peers dropped due to idle timeout";
        OVERLAY_TIMEOUT_STRAGGLER_TOTAL = "stellar_overlay_timeout_straggler_total"
            => "Total peers dropped due to straggler timeout";

        // Herder pending envelope counters (Phase 2).
        HERDER_PENDING_ADDED_TOTAL = "stellar_herder_pending_added_total"
            => "Envelopes added to the pending pool";
        HERDER_PENDING_RELEASED_TOTAL = "stellar_herder_pending_released_total"
            => "Envelopes released from the pending pool";

        // Phase 3: Ledger apply cumulative counters.
        LEDGER_APPLY_SUCCESS_TOTAL = "stellar_ledger_apply_success_total"
            => "Cumulative successful transaction applies";
        LEDGER_APPLY_FAILURE_TOTAL = "stellar_ledger_apply_failure_total"
            => "Cumulative failed transaction applies";
        LEDGER_APPLY_SOROBAN_SUCCESS_TOTAL = "stellar_ledger_apply_soroban_success_total"
            => "Cumulative successful Soroban transaction applies";
        LEDGER_APPLY_SOROBAN_FAILURE_TOTAL = "stellar_ledger_apply_soroban_failure_total"
            => "Cumulative failed Soroban transaction applies";

        // Phase 5: Archive cache counters.
        ARCHIVE_CACHE_FRESH_TOTAL = "henyey_archive_cache_fresh_total"
            => "Archive cache get_cached() Fresh results";
        ARCHIVE_CACHE_STALE_TOTAL = "henyey_archive_cache_stale_total"
            => "Archive cache get_cached() Stale results";
        ARCHIVE_CACHE_COLD_TOTAL = "henyey_archive_cache_cold_total"
            => "Archive cache get_cached() Cold results";
        ARCHIVE_CACHE_REFRESH_SUCCESS_TOTAL = "henyey_archive_cache_refresh_success_total"
            => "Archive cache background refresh successes";
        ARCHIVE_CACHE_REFRESH_ERROR_TOTAL = "henyey_archive_cache_refresh_error_total"
            => "Archive cache background refresh errors";
        ARCHIVE_CACHE_REFRESH_TIMEOUT_TOTAL = "henyey_archive_cache_refresh_timeout_total"
            => "Archive cache background refresh timeouts";
    }

    counters_no_prereg {
        // Meta stream counters — only appear when the meta stream is active.
        META_STREAM_BYTES_TOTAL = "stellar_meta_stream_bytes_total"
            => "Total bytes written to metadata output stream";
        META_STREAM_WRITES_TOTAL = "stellar_meta_stream_writes_total"
            => "Total frames written to metadata output stream";
    }

    labeled_counters_enum {
        SCP_PREFILTER_REJECTS_TOTAL = "henyey_scp_prefilter_rejects_total"
            => "SCP envelopes rejected by the event-loop pre-filter, by reason",
            "reason", henyey_herder::scp_verify::PreFilterRejectReason;
        SCP_POST_VERIFY_TOTAL = "henyey_scp_post_verify_total"
            => "Envelopes processed by post-verify, by reason",
            "reason", henyey_herder::scp_verify::PostVerifyReason;
    }

    labeled_counters_literal {
        RECOVERY_STALLED_TICK_TOTAL = "henyey_recovery_stalled_tick_total"
            => "Recovery stalled ticks by reason",
            "reason", ["backoff_active", "forcing_catchup_not_behind", "forcing_catchup_behind"];
    }

    histograms {
        // Phase 4: Ledger close histogram.
        LEDGER_CLOSE_DURATION_SECONDS = "stellar_ledger_close_duration_seconds"
            => "Ledger close wall-clock duration in seconds";

        // Phase 5: Per-phase close-duration histograms (LedgerClosePerf).
        CLOSE_BEGIN_SECONDS = "henyey_ledger_close_begin_seconds"
            => "Ledger close begin_close phase (seconds)";
        CLOSE_TX_EXEC_SECONDS = "henyey_ledger_close_tx_exec_seconds"
            => "Ledger close tx_exec phase (seconds)";
        CLOSE_CLASSIC_EXEC_SECONDS = "henyey_ledger_close_classic_exec_seconds"
            => "Ledger close classic_exec sub-phase of tx_exec (seconds)";
        CLOSE_SOROBAN_EXEC_SECONDS = "henyey_ledger_close_soroban_exec_seconds"
            => "Ledger close soroban_exec sub-phase of tx_exec (seconds)";
        CLOSE_COMMIT_SETUP_SECONDS = "henyey_ledger_close_commit_setup_seconds"
            => "Ledger close commit_setup phase (seconds)";
        CLOSE_BUCKET_LOCK_WAIT_SECONDS = "henyey_ledger_close_bucket_lock_wait_seconds"
            => "Ledger close bucket_lock_wait phase (seconds)";
        CLOSE_EVICTION_SECONDS = "henyey_ledger_close_eviction_seconds"
            => "Ledger close eviction phase (seconds)";
        CLOSE_SOROBAN_STATE_SECONDS = "henyey_ledger_close_soroban_state_seconds"
            => "Ledger close soroban_state phase (seconds)";
        CLOSE_BUCKET_ADD_SECONDS = "henyey_ledger_close_bucket_add_seconds"
            => "Ledger close bucket_add phase (seconds)";
        CLOSE_HOT_ARCHIVE_SECONDS = "henyey_ledger_close_hot_archive_seconds"
            => "Ledger close hot_archive phase (seconds)";
        CLOSE_HEADER_SECONDS = "henyey_ledger_close_header_seconds"
            => "Ledger close header phase (seconds)";
        CLOSE_COMMIT_SECONDS = "henyey_ledger_close_commit_seconds"
            => "Ledger close commit_close phase (seconds)";
        CLOSE_META_SECONDS = "henyey_ledger_close_meta_seconds"
            => "Ledger close meta phase (seconds)";

        // Phase 5: Event-loop post-close PhaseTimer histograms.
        CLOSE_COMPLETE_JOIN_MATCH_SECONDS = "henyey_ledger_close_complete_join_match_seconds"
            => "Close-complete join_match phase (seconds)";
        CLOSE_COMPLETE_META_EMIT_SECONDS = "henyey_ledger_close_complete_meta_emit_seconds"
            => "Close-complete meta_emit phase (seconds)";
        CLOSE_COMPLETE_BUILD_PERSIST_INPUTS_SECONDS = "henyey_ledger_close_complete_build_persist_inputs_seconds"
            => "Close-complete build_persist_inputs phase (seconds)";
        CLOSE_COMPLETE_OVERLAY_BOOKKEEPING_SECONDS = "henyey_ledger_close_complete_overlay_bookkeeping_seconds"
            => "Close-complete overlay_bookkeeping phase (seconds)";
        CLOSE_COMPLETE_SPAWN_BLOCKING_SETUP_SECONDS = "henyey_ledger_close_complete_spawn_blocking_setup_seconds"
            => "Close-complete spawn_blocking_setup phase (seconds)";
        CLOSE_COMPLETE_TX_QUEUE_SECONDS = "henyey_ledger_close_complete_tx_queue_seconds"
            => "Close-complete tx_queue_background_wait phase (seconds)";
        CLOSE_COMPLETE_POST_CLOSE_BOOKKEEPING_SECONDS = "henyey_ledger_close_complete_post_close_bookkeeping_seconds"
            => "Close-complete post_close_bookkeeping phase (seconds)";

        // Phase 6: Sub-phase instrumentation inside tx_queue spawn_blocking.
        CLOSE_TX_QUEUE_PREP_SECONDS = "henyey_ledger_close_tx_queue_prep_seconds"
            => "tx_queue spawn_blocking closure-local prep (Soroban limits, close-time math) (seconds)";
        CLOSE_TX_QUEUE_LEDGER_CLOSED_SECONDS = "henyey_ledger_close_tx_queue_ledger_closed_seconds"
            => "tx_queue spawn_blocking herder.ledger_closed + ban failed txs (seconds)";
        CLOSE_TX_QUEUE_SHIFT_UPDATE_SECONDS = "henyey_ledger_close_tx_queue_shift_update_seconds"
            => "tx_queue spawn_blocking validation context update + Soroban limits + shift (seconds)";
        CLOSE_TX_QUEUE_SNAPSHOT_SECONDS = "henyey_ledger_close_tx_queue_snapshot_seconds"
            => "tx_queue spawn_blocking pending_hashed_envelopes + snapshot build total (seconds)";
        CLOSE_TX_QUEUE_ENVELOPES_FETCH_SECONDS = "henyey_ledger_close_tx_queue_envelopes_fetch_seconds"
            => "tx_queue spawn_blocking pending_hashed_envelopes only (seconds)";
        CLOSE_TX_QUEUE_SNAPSHOT_BUILD_SECONDS = "henyey_ledger_close_tx_queue_snapshot_build_seconds"
            => "tx_queue spawn_blocking SnapshotValidationProviders::new only (seconds)";
        CLOSE_TX_QUEUE_INVALIDATION_SECONDS = "henyey_ledger_close_tx_queue_invalidation_seconds"
            => "tx_queue spawn_blocking get_invalid_hashed_tx_list + ban invalid (seconds)";

        // Phase 6: Persist and close-cycle instrumentation.
        PERSIST_LEDGER_CLOSE_SECONDS = "henyey_ledger_persist_close_seconds"
            => "PersistJob::LedgerClose wall-clock duration (seconds, excludes scheduling delay)";
        CLOSE_CYCLE_SECONDS = "henyey_ledger_close_cycle_seconds"
            => "Wall-clock between consecutive deferred-pipeline close-complete events (seconds)";

        // Close-cycle decomposition (#1909).
        CLOSE_HANDLE_COMPLETE_SECONDS = "henyey_ledger_close_handle_complete_seconds"
            => "Wall-clock of handle_close_complete including finalizer dispatch (seconds)";
        CLOSE_POST_COMPLETE_SECONDS = "henyey_ledger_close_post_complete_seconds"
            => "Wall-clock of post-close lifecycle work after handle_close_complete (seconds)";
        CLOSE_DISPATCH_TO_JOIN_SECONDS = "henyey_ledger_close_dispatch_to_join_seconds"
            => "Wall-clock from spawn_blocking dispatch to event-loop join observation (seconds)";
        PERSIST_DISPATCH_TO_JOIN_SECONDS = "henyey_ledger_persist_dispatch_to_join_seconds"
            => "Wall-clock from persist spawn_blocking dispatch to event-loop join observation (seconds)";

        // Phase 5: Slot-to-close latency.
        SLOT_TO_CLOSE_LATENCY_SECONDS = "henyey_ledger_slot_to_close_latency_seconds"
            => "Wall-clock from first SCP activity to close-complete (seconds)";

        // Phase 5: Archive cache histogram.
        ARCHIVE_CACHE_REFRESH_DURATION_SECONDS = "henyey_archive_cache_refresh_duration_seconds"
            => "Archive cache background refresh duration (seconds, including timeouts)";
    }
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
    counter!(HERDER_ARB_TX_SEEN).absolute(tq.arb_tx_seen);
    counter!(HERDER_ARB_TX_DROPPED).absolute(tq.arb_tx_dropped);

    // Herder pending-tx age gauges (Stage D).
    gauge!(HERDER_PENDING_TXS_AGE0).set(tq.pending_txs_age[0] as f64);
    gauge!(HERDER_PENDING_TXS_AGE1).set(tq.pending_txs_age[1] as f64);
    gauge!(HERDER_PENDING_TXS_AGE2).set(tq.pending_txs_age[2] as f64);
    gauge!(HERDER_PENDING_TXS_AGE3).set(tq.pending_txs_age[3] as f64);

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

    // Stage A (10334 dashboard): Health Summary & Version gauges.
    gauge!(STARTED_ON).set(state.started_on_epoch);
    gauge!(LEDGER_VERSION).set(ledger_info.protocol_version as f64);
    gauge!(PROTOCOL_VERSION).set(state.app.config().network.max_protocol_version as f64);

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
        gauge!(QUORUM_DELAYED).set(qh.delayed as f64);
    } else {
        gauge!(QUORUM_AGREE).set(0.0);
        gauge!(QUORUM_MISSING).set(0.0);
        gauge!(QUORUM_DISAGREE).set(0.0);
        gauge!(QUORUM_FAIL_AT).set(0.0);
        gauge!(QUORUM_DELAYED).set(0.0);
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
        if let Some(lag_secs) = timing.first_to_self_externalize_secs {
            gauge!(SCP_TIMING_FIRST_TO_SELF_EXTERNALIZE_SECONDS).set(lag_secs);
        } else {
            gauge!(SCP_TIMING_FIRST_TO_SELF_EXTERNALIZE_SECONDS).set(0.0);
        }
    } else {
        // No timing available (e.g., after catchup cleared it). Reset gauges.
        gauge!(SCP_TIMING_EXTERNALIZED_SECONDS).set(0.0);
        gauge!(SCP_TIMING_NOMINATED_SECONDS).set(0.0);
        gauge!(SCP_TIMING_FIRST_TO_SELF_EXTERNALIZE_SECONDS).set(0.0);
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

    // Phase 6: TxSet exhaustion stuck gauge.
    let exhausted_since = state.app.tx_set_exhausted_since_offset();
    if exhausted_since > 0 {
        let stuck_secs = uptime.saturating_sub(exhausted_since);
        gauge!(RECOVERY_TX_SET_STUCK_SECONDS).set(stuck_secs as f64);
    } else {
        gauge!(RECOVERY_TX_SET_STUCK_SECONDS).set(0.0);
    }
}

// ── Process health helpers ──────────────────────────────────────────────

/// Returns the number of open file descriptors, or `None` on non-Linux
/// or if `/proc/self/fd` is inaccessible.
#[cfg(target_os = "linux")]
fn process_open_fds() -> Option<u64> {
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
    use catalog_arrays::*;
    use std::collections::HashSet;

    #[test]
    fn test_all_gauges_described_and_typed() {
        let handle = ensure_test_recorder();
        describe_metrics();
        register_label_series();
        let output = handle.render();

        // Pre-registered gauges have visible HELP/TYPE because they have a value.
        for name in ALL_PREREGISTERED_GAUGE_NAMES {
            assert!(
                output.contains(&format!("# HELP {}", name)),
                "missing HELP for gauge {}",
                name
            );
            assert!(
                output.contains(&format!("# TYPE {} gauge", name)),
                "missing TYPE gauge for {}",
                name
            );
        }
    }

    #[test]
    fn test_all_counters_described_and_typed() {
        let handle = ensure_test_recorder();
        describe_metrics();
        register_label_series();
        let output = handle.render();

        // Pre-registered counters (unlabeled) have visible HELP/TYPE.
        for name in ALL_PREREGISTERED_COUNTER_NAMES {
            assert!(
                output.contains(&format!("# HELP {}", name)),
                "missing HELP for counter {}",
                name
            );
            assert!(
                output.contains(&format!("# TYPE {} counter", name)),
                "missing TYPE counter for {}",
                name
            );
        }
    }

    #[test]
    fn test_preregistered_gauges_at_zero() {
        let handle = ensure_test_recorder();
        describe_metrics();
        register_label_series();
        let output = handle.render();

        for name in ALL_PREREGISTERED_GAUGE_NAMES {
            assert!(
                output.contains(&format!("{} 0", name)),
                "gauge {} should be pre-registered at 0",
                name
            );
        }
    }

    #[test]
    fn test_preregistered_counters_at_zero() {
        let handle = ensure_test_recorder();
        describe_metrics();
        register_label_series();
        let output = handle.render();

        for name in ALL_PREREGISTERED_COUNTER_NAMES {
            assert!(
                output.contains(&format!("{} 0", name)),
                "counter {} should be pre-registered at 0",
                name
            );
        }
    }

    #[test]
    fn test_all_histograms_described_and_recorded() {
        let handle = ensure_test_recorder();
        describe_metrics();

        // Record one sample for each histogram so HELP/TYPE appears.
        for name in ALL_HISTOGRAM_NAMES {
            metrics::histogram!(*name).record(0.025);
        }

        let output = handle.render();

        for name in ALL_HISTOGRAM_NAMES {
            assert!(
                output.contains(&format!("# HELP {}", name)),
                "missing HELP for histogram {}",
                name
            );
            assert!(
                output.contains(&format!("# TYPE {} histogram", name)),
                "missing TYPE histogram for {}",
                name
            );
            assert!(
                output.contains(&format!("{}_bucket{{", name)),
                "no _bucket line for histogram {}",
                name
            );
            assert!(
                output.contains(&format!("{}_count", name)),
                "no _count line for histogram {}",
                name
            );
            assert!(
                output.contains(&format!("{}_sum", name)),
                "no _sum line for histogram {}",
                name
            );
        }
    }

    #[test]
    fn test_labeled_series_present_at_zero() {
        let handle = ensure_test_recorder();
        describe_metrics();
        register_label_series();
        let output = handle.render();

        // PreFilterRejectReason labels.
        use henyey_herder::scp_verify::PreFilterRejectReason;
        for reason in PreFilterRejectReason::ALL {
            let label = format!(
                "henyey_scp_prefilter_rejects_total{{reason=\"{}\"}}",
                reason.label()
            );
            assert!(
                output.contains(&label),
                "missing pre-registered label for reason={}",
                reason.label()
            );
        }

        // PostVerifyReason labels.
        use henyey_herder::scp_verify::PostVerifyReason;
        for reason in PostVerifyReason::ALL {
            let label = format!(
                "henyey_scp_post_verify_total{{reason=\"{}\"}}",
                reason.label()
            );
            assert!(
                output.contains(&label),
                "missing pre-registered label for reason={}",
                reason.label()
            );
        }

        // RECOVERY_STALLED_TICK_TOTAL literal labels.
        for reason in &[
            "backoff_active",
            "forcing_catchup_not_behind",
            "forcing_catchup_behind",
        ] {
            let label = format!(
                "henyey_recovery_stalled_tick_total{{reason=\"{}\"}}",
                reason
            );
            assert!(
                output.contains(&label),
                "missing pre-registered label for recovery_stalled reason={}",
                reason
            );
        }
    }

    #[test]
    fn test_recorder_install_is_idempotent() {
        let h1 = ensure_test_recorder();
        let h2 = ensure_test_recorder();
        assert!(std::ptr::eq(h1, h2));
    }

    #[test]
    fn test_removed_overlay_metrics_absent() {
        let handle = ensure_test_recorder();
        describe_metrics();
        register_label_series();
        let output = handle.render();

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
    fn test_removed_redundant_gauge_metrics_absent() {
        let handle = ensure_test_recorder();
        describe_metrics();
        register_label_series();
        let output = handle.render();

        let removed = [
            "henyey_ledger_soroban_exec_us",
            "henyey_ledger_classic_exec_us",
            "stellar_ledger_close_time_ms",
        ];
        for name in &removed {
            assert!(
                !output.contains(&format!("# HELP {}", name)),
                "metric {} should not be registered (redundant with histogram)",
                name
            );
        }
    }

    #[test]
    fn test_metric_names_unique() {
        let mut seen = HashSet::new();
        let all_names = ALL_GAUGE_NAMES
            .iter()
            .chain(ALL_COUNTER_NAMES.iter())
            .chain(ALL_HISTOGRAM_NAMES.iter());
        for name in all_names {
            assert!(seen.insert(name), "duplicate metric name: {}", name);
        }
    }

    #[test]
    fn test_stage_a_dashboard_gauges_in_catalog() {
        // Stage A (10334 dashboard): verify the 3 new gauges are registered.
        let gauge_names: HashSet<&str> = ALL_GAUGE_NAMES.iter().copied().collect();
        assert!(
            gauge_names.contains("stellar_started_on"),
            "stellar_started_on missing from gauge catalog"
        );
        assert!(
            gauge_names.contains("stellar_ledger_version"),
            "stellar_ledger_version missing from gauge catalog"
        );
        assert!(
            gauge_names.contains("stellar_protocol_version"),
            "stellar_protocol_version missing from gauge catalog"
        );
    }

    #[test]
    fn test_stage_a_gauges_preregistered_at_zero() {
        let handle = ensure_test_recorder();
        describe_metrics();
        register_label_series();
        let output = handle.render();

        // All 3 Stage A gauges should appear in the rendered output after pre-registration.
        assert!(
            output.contains("stellar_started_on"),
            "stellar_started_on not found in rendered metrics"
        );
        assert!(
            output.contains("stellar_ledger_version"),
            "stellar_ledger_version not found in rendered metrics"
        );
        assert!(
            output.contains("stellar_protocol_version"),
            "stellar_protocol_version not found in rendered metrics"
        );
    }
}
