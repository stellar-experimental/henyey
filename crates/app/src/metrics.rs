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

use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

use crate::http::ServerState;

// ── Typed metric wrappers ──────────────────────────────────────────────
//
// These newtypes make it a compile-time error to emit a counter metric
// through a gauge API or vice versa. Each wrapper exposes only the methods
// appropriate for its Prometheus type.

/// A gauge metric — supports `set`, `increment`, `decrement`.
#[derive(Clone, Copy, Debug)]
pub struct GaugeMetric(pub &'static str);

/// A counter metric — supports `absolute` and `increment`.
#[derive(Clone, Copy, Debug)]
pub struct CounterMetric(pub &'static str);

/// A histogram metric — supports `record`.
#[derive(Clone, Copy, Debug)]
pub struct HistogramMetric(pub &'static str);

/// A labeled counter metric with a fixed label key.
/// Supports `absolute(label_value, n)` and `increment(label_value, n)`.
#[derive(Clone, Copy, Debug)]
pub struct LabeledCounterMetric {
    pub name: &'static str,
    pub key: &'static str,
}

impl GaugeMetric {
    pub fn set(&self, value: f64) {
        gauge!(self.0).set(value);
    }
    pub fn increment(&self, value: f64) {
        gauge!(self.0).increment(value);
    }
    pub fn decrement(&self, value: f64) {
        gauge!(self.0).decrement(value);
    }
    pub const fn name(&self) -> &'static str {
        self.0
    }
}

impl CounterMetric {
    pub fn absolute(&self, value: u64) {
        counter!(self.0).absolute(value);
    }
    pub fn increment(&self, value: u64) {
        counter!(self.0).increment(value);
    }
    pub const fn name(&self) -> &'static str {
        self.0
    }
}

impl HistogramMetric {
    pub fn record(&self, value: f64) {
        metrics::histogram!(self.0).record(value);
    }
    pub const fn name(&self) -> &'static str {
        self.0
    }
}

impl LabeledCounterMetric {
    pub fn absolute(&self, label_value: &'static str, value: u64) {
        counter!(self.name, self.key => label_value).absolute(value);
    }
    pub fn increment(&self, label_value: &'static str, value: u64) {
        counter!(self.name, self.key => label_value).increment(value);
    }
    pub const fn name(&self) -> &'static str {
        self.name
    }
}

// Trait impls for ergonomic string access.
impl AsRef<str> for GaugeMetric {
    fn as_ref(&self) -> &str {
        self.0
    }
}
impl AsRef<str> for CounterMetric {
    fn as_ref(&self) -> &str {
        self.0
    }
}
impl AsRef<str> for HistogramMetric {
    fn as_ref(&self) -> &str {
        self.0
    }
}
impl AsRef<str> for LabeledCounterMetric {
    fn as_ref(&self) -> &str {
        self.name
    }
}

impl PartialEq<&str> for GaugeMetric {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}
impl PartialEq<&str> for CounterMetric {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}
impl PartialEq<&str> for HistogramMetric {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}
impl PartialEq<&str> for LabeledCounterMetric {
    fn eq(&self, other: &&str) -> bool {
        self.name == *other
    }
}

impl std::fmt::Display for GaugeMetric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::fmt::Display for CounterMetric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::fmt::Display for HistogramMetric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::fmt::Display for LabeledCounterMetric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name)
    }
}

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
        // ── 1. Public typed constants ──────────────────────────────────
        $( pub const $g_name: GaugeMetric = GaugeMetric($g_prom); )*
        $( pub const $gnp_name: GaugeMetric = GaugeMetric($gnp_prom); )*
        $( pub const $c_name: CounterMetric = CounterMetric($c_prom); )*
        $( pub const $cnp_name: CounterMetric = CounterMetric($cnp_prom); )*
        $( pub const $lce_name: LabeledCounterMetric = LabeledCounterMetric { name: $lce_prom, key: $lce_key }; )*
        $( pub const $lcl_name: LabeledCounterMetric = LabeledCounterMetric { name: $lcl_prom, key: $lcl_key }; )*
        $( pub const $h_name: HistogramMetric = HistogramMetric($h_prom); )*

        // ── 2. describe_metrics() ──────────────────────────────────────
        /// Register HELP/TYPE annotations for all metrics.
        ///
        /// Must be called **after** the global recorder is installed.
        pub fn describe_metrics() {
            $( describe_gauge!($g_prom, $g_help); )*
            $( describe_gauge!($gnp_prom, $gnp_help); )*
            $( describe_counter!($c_prom, $c_help); )*
            $( describe_counter!($cnp_prom, $cnp_help); )*
            $( describe_counter!($lce_prom, $lce_help); )*
            $( describe_counter!($lcl_prom, $lcl_help); )*
            $( describe_histogram!($h_prom, $h_help); )*
        }

        // ── 3. register_label_series() ─────────────────────────────────
        /// Pre-register all metrics with initial values so that every metric
        /// appears on the very first scrape with its HELP/TYPE annotations.
        ///
        /// Must be called **after** [`describe_metrics`].
        pub fn register_label_series() {
            // Gauges — zero-init.
            $( gauge!($g_prom).set(0.0); )*
            // gauges_no_prereg: intentionally skipped.
            // Counters — zero-init.
            $( counter!($c_prom).absolute(0); )*
            // counters_no_prereg: intentionally skipped.
            // Labeled counters (enum-backed) — iterate ALL variants.
            $( for v in <$lce_enum>::ALL {
                counter!($lce_prom, $lce_key => v.label()).absolute(0);
            } )*
            // Labeled counters (literal-backed).
            $( for v in &[ $( $lcl_val ),+ ] {
                counter!($lcl_prom, $lcl_key => *v).absolute(0);
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
        SCP_VERIFY_INPUT_BACKLOG_PEAK = "henyey_scp_verify_input_backlog_peak"
            => "Monotonic high-water mark of verifier input backlog (worker sampled)";

        // SCP persist-worker backlog gauges (#3796). The emit hot path spawns
        // one detached persist worker per envelope; these expose the in-flight
        // count that #3796 §2 had to measure with a bespoke /proc sampler.
        SCP_PENDING_PERSISTS = "henyey_scp_pending_persists"
            => "SCP persist worker threads currently in flight (deferred SQLite writes)";
        SCP_PENDING_PERSISTS_PEAK = "henyey_scp_pending_persists_peak"
            => "Process-lifetime high-water mark of in-flight SCP persist workers";
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
            => "Authenticated inbound peer connections (instantaneous net, currently-authenticated count; trend stellar_overlay_inbound_authenticated_total instead)";
        OVERLAY_OUTBOUND_AUTHENTICATED = "stellar_overlay_outbound_authenticated"
            => "Authenticated outbound peer connections";
        OVERLAY_INBOUND_PENDING = "stellar_overlay_inbound_pending"
            => "Pending inbound peer connections (handshaking)";
        OVERLAY_OUTBOUND_PENDING = "stellar_overlay_outbound_pending"
            => "Pending outbound peer connections (handshaking)";

        // Stage F.2: FloodGate known entries gauge (issue #2244).
        OVERLAY_MEMORY_FLOOD_KNOWN = "stellar_overlay_memory_flood_known"
            => "Current FloodGate known entries count";

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

        // Stage B: Ledger pipeline depth.
        LEDGER_MEMORY_QUEUED_LEDGERS = "stellar_ledger_memory_queued_ledgers"
            => "Externalized ledger slots buffered and waiting to close";

        // Stage C: SCP phase and memory gauges (issue #2233).
        SCP_PHASE = "stellar_scp_phase"
            => "Current SCP ballot phase of the tracking slot (0=unknown, 1=prepare, 2=confirm, 3=externalize)";
        SCP_MEMORY_CUMULATIVE_STATEMENTS = "stellar_scp_memory_cumulative_statements"
            => "Total SCP statements currently held in memory (decreases after slot purging)";

        // Startup/catchup peak RSS (issue #3226). Set once when the startup
        // sampler stops, before the event loop's first ledger close. Covers the
        // restore + catchup window that the `% 64` ledger-close report misses.
        STARTUP_PEAK_ANON_RSS_MB = "henyey_startup_peak_anon_rss_mb"
            => "Peak anonymous RSS (MB) observed during startup restore + catchup";

        // ── Loadgen meters (#3569, reset on clearmetrics #3630) ────────
        //
        // Parity with stellar-core's medida loadgen meters
        // (src/simulation/LoadGenerator.cpp + main/ApplicationImpl.cpp:1156).
        // Supercluster's `IsLoadGenComplete` polls `loadgen_run_start ==
        // loadgen_run_complete` to detect henyey-driven loadgen completion, so
        // these must render with the EXACT bare-`loadgen_` names core's medida
        // exporter emits (no `stellar_`/`henyey_` prefix, no dot→underscore
        // munging — the literal string IS the exposition name). Pre-registered
        // at zero so supercluster reads NotStarted (start==0) before a run
        // rather than a missing series.
        //
        // These are GAUGES, not counters: stellar-core's `clearMetrics`
        // (`MetricResetter`, ApplicationImpl.cpp:1375) truly resets the medida
        // meters between Supercluster max-TPS / min-block-time binary-search
        // steps, and the Prometheus exporter only renders a true `0` for a value
        // it can `set` (gauge `swap`) — a monotonic counter's `absolute(0)` is
        // `fetch_max(0)`, a no-op that leaves the accumulated value (#3630).
        // Supercluster reads the numeric value via the compat `/metrics` JSON
        // (`LOADGEN_COMPAT_MAP` → `count`), which is type-agnostic, so the
        // counter→gauge exposition change is invisible to the consumer.
        LOADGEN_RUN_START = "loadgen_run_start"
            => "Loadgen runs started (parity: ApplicationImpl::generateLoad)";
        LOADGEN_RUN_COMPLETE = "loadgen_run_complete"
            => "Loadgen runs completed successfully (parity: LoadGenerator mLoadgenComplete)";
        LOADGEN_RUN_FAILED = "loadgen_run_failed"
            => "Loadgen runs that failed or were stopped (parity: LoadGenerator mLoadgenFail)";
        LOADGEN_ACCOUNT_CREATED = "loadgen_account_created"
            => "Loadgen accounts created (parity: registered-at-zero; core v27 marks this test-only)";
        LOADGEN_TXN_ATTEMPTED = "loadgen_txn_attempted"
            => "Loadgen transactions attempted (parity: LoadGenerator mTxnAttempted)";
        LOADGEN_TXN_REJECTED = "loadgen_txn_rejected"
            => "Loadgen transactions rejected by the tx queue (parity: LoadGenerator mTxnRejected)";
        LOADGEN_TXN_BYTES = "loadgen_txn_bytes"
            => "Loadgen transaction bytes submitted (parity: LoadGenerator mTxnBytes)";
        LOADGEN_PAYMENT_SUBMITTED = "loadgen_payment_submitted"
            => "Loadgen native payment operations submitted (parity: LoadGenerator mNativePayment, by op count)";
        LOADGEN_PAYMENT_BYTES = "loadgen_payment_bytes"
            => "Loadgen native payment transaction bytes (parity: LoadGenerator mNativePaymentBytes)";
        LOADGEN_SOROBAN_UPLOAD = "loadgen_soroban_upload"
            => "Loadgen Soroban upload transactions (parity: LoadGenerator mSorobanUploadTxs)";
        LOADGEN_SOROBAN_INVOKE = "loadgen_soroban_invoke"
            => "Loadgen Soroban invoke transactions (parity: LoadGenerator mSorobanInvokeTxs)";
        LOADGEN_SOROBAN_SETUP_INVOKE = "loadgen_soroban_setup_invoke"
            => "Loadgen Soroban setup-invoke transactions (parity: LoadGenerator mSorobanSetupInvokeTxs)";
        LOADGEN_SOROBAN_SETUP_UPGRADE = "loadgen_soroban_setup_upgrade"
            => "Loadgen Soroban setup-upgrade transactions (parity: LoadGenerator mSorobanSetupUpgradeTxs)";
        LOADGEN_SOROBAN_CREATE_UPGRADE = "loadgen_soroban_create_upgrade"
            => "Loadgen Soroban create-upgrade transactions (parity: LoadGenerator mSorobanCreateUpgradeTxs)";
        LOADGEN_STEP_COUNT = "loadgen_step_count"
            => "Loadgen generate_load loop iterations (parity: LoadGenerator mStepMeter)";
    }

    gauges_no_prereg {
        // Phase 4: Quorum transitive intersection — absent until first publishable check.
        QUORUM_TRANSITIVE_INTERSECTION = "stellar_quorum_transitive_intersection"
            => "Whether the network enjoys transitive quorum intersection (1=yes, 0=no, absent until first publishable check)";

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
        RECOVERY_STALL_ONSET_TOTAL = "henyey_recovery_stall_onset_total"
            => "Recovery stall onset events (one per episode)";
        SCP_SCHEDULED_DEDUP_TOTAL = "henyey_scp_scheduled_dedup_total"
            => "SCP envelopes rejected by in-flight scheduled dedup";
        SCP_STATE_REQUESTS_SENT_TOTAL = "henyey_scp_state_requests_sent_total"
            => "Total GetScpState (SCP state) re-request attempts sent to peers \
                (one per request attempt, including attempts where 0 peers were \
                reachable); see issue #3270";
        DB_BUSY_RETRY_TOTAL = "henyey_db_busy_retry_total"
            => "Total bounded retries of a consensus-critical persist DB commit \
                after a transient SQLITE_BUSY/LOCKED, before escalating to a \
                clean recoverable shutdown; see issue #3640";
        EVENT_LOOP_STALL_ERROR_TOTAL = "henyey_event_loop_stall_error_total"
            => "Event-loop stalls of >=30s, counted loop-side exactly once per \
                stall when the loop resumes (issue #3795). Complete by \
                construction — unlike the phase-locked watchdog sampler, which \
                deterministically misses recovering stalls in a fixed blind \
                band. Do NOT sum with the sampler's repeated WATCHDOG log lines.";

        // SCP/herder counters.
        SCP_ENVELOPE_EMIT_TOTAL = "stellar_scp_envelope_emit_total"
            => "Total SCP envelopes emitted by the local node";
        SCP_ENVELOPE_RECEIVE_TOTAL = "stellar_scp_envelope_receive_total"
            => "Total SCP envelopes accepted past in-flight dedup \
                (parity: HerderImpl.cpp:810 mEnvelopeReceive)";
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
        HERDER_ARB_TX_SEEN = "stellar_herder_arb_tx_seen_total"
            => "Total arbitrage (looping path-payment) transactions evaluated for broadcast";
        HERDER_ARB_TX_DROPPED = "stellar_herder_arb_tx_dropped_total"
            => "Total arbitrage transactions dropped by flood damping";

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

        // State-archival eviction counters. Names mirror stellar-core's medida
        // keys {"state-archival","eviction",*} (BucketUtils.cpp:171-181).
        EVICTION_ENTRIES_EVICTED_TOTAL = "state_archival_eviction_entries_evicted"
            => "Total Soroban state entries evicted from the live bucket list";
        EVICTION_BYTES_SCANNED_TOTAL = "state_archival_eviction_bytes_scanned"
            => "Total bytes scanned by the eviction scan";
        EVICTION_INCOMPLETE_SCAN_TOTAL = "state_archival_eviction_incomplete_scan"
            => "Count of buckets too large to fully scan within their update period";
        EVICTION_PERIOD = "state_archival_eviction_period"
            => "Ledger span of the most recently completed eviction cycle";
        EVICTION_AGE = "state_archival_eviction_age"
            => "Average age (ledgers) of entries evicted in the last eviction cycle";

        // Overlay counters.
        OVERLAY_MESSAGE_READ_TOTAL = "stellar_overlay_message_read_total"
            => "Total overlay messages read from peers";
        OVERLAY_MESSAGE_WRITE_TOTAL = "stellar_overlay_message_write_total"
            => "Total overlay messages written to peers";
        OVERLAY_MESSAGE_BROADCAST_TOTAL = "stellar_overlay_message_broadcast_total"
            => "Total overlay messages broadcast to all peers";
        OVERLAY_FETCH_MESSAGES_DROPPED_TOTAL = "henyey_overlay_fetch_messages_dropped_total"
            => "Total fetch-response/-request messages dropped on the bounded fetch intake channel when full (#3661)";
        OVERLAY_CATCHUP_MESSAGES_DROPPED_TOTAL = "henyey_overlay_catchup_messages_dropped_total"
            => "Total catchup-cache fan-out messages dropped on the bounded catchup channel when full (#3661)";
        OVERLAY_ERROR_READ_TOTAL = "stellar_overlay_error_read_total"
            => "Total overlay read errors";
        OVERLAY_ERROR_WRITE_TOTAL = "stellar_overlay_error_write_total"
            => "Total overlay write errors";
        OVERLAY_TIMEOUT_IDLE_TOTAL = "stellar_overlay_timeout_idle_total"
            => "Total peers dropped due to idle timeout";
        OVERLAY_TIMEOUT_STRAGGLER_TOTAL = "stellar_overlay_timeout_straggler_total"
            => "Total peers dropped due to straggler timeout";

        // Stage F.1: Overlay byte / async I/O counters (issue #2236).
        OVERLAY_BYTE_READ_TOTAL = "stellar_overlay_byte_read_total"
            => "Total bytes read from peers (XDR-encoded AuthenticatedMessage size, excluding 4-byte length header)";
        OVERLAY_BYTE_WRITE_TOTAL = "stellar_overlay_byte_write_total"
            => "Total bytes written to peers (XDR-encoded AuthenticatedMessage size, excluding 4-byte length header)";
        OVERLAY_ASYNC_READ_TOTAL = "stellar_overlay_async_read_total"
            => "Total successful recv I/O operations";
        OVERLAY_ASYNC_WRITE_TOTAL = "stellar_overlay_async_write_total"
            => "Total successful send I/O operations";

        // Tx-demand timeout: a tx demand went unfulfilled past the retry delay
        // and was re-issued to a peer. Mirrors stellar-core's
        // `overlay.demand.timeout`. Recorded by string in
        // `app::tx_flooding::run_tx_demands`; cataloged here for HELP/TYPE +
        // pre-registration (same pattern as the tx-pull-latency histogram).
        OVERLAY_DEMAND_TIMEOUT_TOTAL = "stellar_overlay_demand_timeout_total"
            => "Total tx demands re-issued after a prior demand timed out";

        // [maxtps_diag] Fulfiller-side demand outcomes (why a demand we receive
        // is/ isn't answered). Recorded by string in app::tx_flooding.
        OVERLAY_DEMAND_FULFILLED_TOTAL = "stellar_overlay_demand_fulfilled_total"
            => "Total demanded txs we answered (sent the tx)";
        OVERLAY_DEMAND_UNFULFILLED_DROPPED_TOTAL = "stellar_overlay_demand_unfulfilled_dropped_total"
            => "Total demanded txs not answered because the peer outbound channel was full";
        OVERLAY_DEMAND_UNFULFILLED_BANNED_TOTAL = "stellar_overlay_demand_unfulfilled_banned_total"
            => "Total demanded txs not answered because the tx is banned";
        OVERLAY_DEMAND_UNFULFILLED_UNKNOWN_TOTAL = "stellar_overlay_demand_unfulfilled_unknown_total"
            => "Total demanded txs not answered because the hash is not in our queue";
        // [maxtps_diag] Flood messages (TX) dropped by the lossy broadcast
        // receive channel when the event loop falls behind under load.
        OVERLAY_BROADCAST_LAGGED_DROPPED_TOTAL = "stellar_overlay_broadcast_lagged_dropped_total"
            => "Total flood messages dropped due to broadcast-channel lag on receive";

        // Stage F.1: Overlay connection lifecycle counters (issue #2236).
        OVERLAY_INBOUND_ATTEMPT_TOTAL = "stellar_overlay_inbound_attempt_total"
            => "Total inbound connection accepts (TCP listener.accept() Ok)";
        OVERLAY_INBOUND_ESTABLISH_TOTAL = "stellar_overlay_inbound_establish_total"
            => "Total inbound peers fully established (registered after handshake)";
        OVERLAY_INBOUND_DROP_TOTAL = "stellar_overlay_inbound_drop_total"
            => "Total inbound peer disconnections (run_peer_loop returned); equals remote + local";
        // #3422 diagnostic: segment inbound drops by initiator so "peers churn
        // out" (remote RST/FIN) is visibly distinct from "henyey drops peers".
        // remote + local == the inbound_drop total by construction.
        OVERLAY_INBOUND_DROP_REMOTE_TOTAL = "stellar_overlay_inbound_drop_remote_total"
            => "Inbound drops initiated by the remote peer (FIN / RST / recv-error)";
        OVERLAY_INBOUND_DROP_LOCAL_TOTAL = "stellar_overlay_inbound_drop_local_total"
            => "Inbound drops initiated by henyey (timeout / send-error / protocol / shutdown)";
        OVERLAY_INBOUND_REJECT_TOTAL = "stellar_overlay_inbound_reject_total"
            => "Total inbound connections rejected before establishment";
        // #3419 diagnostic: monotonic count of inbound peers that reached
        // authenticated state. Climbing while the `inbound_authenticated` gauge
        // stays near 0 means "authenticated then churned", not "never authenticated".
        OVERLAY_INBOUND_AUTHENTICATED_TOTAL = "stellar_overlay_inbound_authenticated_total"
            => "Total inbound peers that transitioned to authenticated (monotonic)";
        OVERLAY_OUTBOUND_ATTEMPT_TOTAL = "stellar_overlay_outbound_attempt_total"
            => "Total outbound connection attempts (dial initiated)";
        OVERLAY_OUTBOUND_ESTABLISH_TOTAL = "stellar_overlay_outbound_establish_total"
            => "Total outbound peers fully established (registered after handshake)";
        OVERLAY_OUTBOUND_DROP_TOTAL = "stellar_overlay_outbound_drop_total"
            => "Total outbound peer disconnections (run_peer_loop returned)";
        OVERLAY_OUTBOUND_REJECT_TOTAL = "stellar_overlay_outbound_reject_total"
            => "Total outbound connections rejected before establishment";

        // Stage F.2: Overlay flood / fetch / item-fetcher counters (issue #2244).
        OVERLAY_FLOOD_BROADCAST_TOTAL = "stellar_overlay_flood_broadcast_total"
            => "Total flood messages broadcast (per-recipient deliveries)";
        OVERLAY_FLOOD_DUPLICATE_RECV_TOTAL = "stellar_overlay_flood_duplicate_recv_total"
            => "Total duplicate flood messages received";
        OVERLAY_FLOOD_UNIQUE_RECV_TOTAL = "stellar_overlay_flood_unique_recv_total"
            => "Total unique flood messages received";
        // Issue #2648: SCP-specific flood counters.
        OVERLAY_SCP_FLOOD_UNIQUE_RECV_TOTAL = "henyey_overlay_scp_flood_unique_recv_total"
            => "Total unique SCP flood messages received";
        OVERLAY_SCP_FLOOD_DUPLICATE_RECV_TOTAL = "henyey_overlay_scp_flood_duplicate_recv_total"
            => "Total duplicate SCP flood messages received";
        OVERLAY_FETCH_DUPLICATE_RECV_TOTAL = "stellar_overlay_fetch_duplicate_recv_total"
            => "Total duplicate/unsolicited fetch responses received";
        OVERLAY_FETCH_UNIQUE_RECV_TOTAL = "stellar_overlay_fetch_unique_recv_total"
            => "Total unique/solicited fetch responses received";
        OVERLAY_ITEM_FETCHER_NEXT_PEER_TOTAL = "stellar_overlay_item_fetcher_next_peer_total"
            => "Total item fetcher next-peer selections";

        // Issue #2725: Flood messages shed when node is not synced.
        OVERLAY_FLOOD_SHED_UNSYNCED_TOTAL = "henyey_overlay_flood_shed_unsynced_total"
            => "Total flood messages dropped because node is not synced";

        // Issue #2621 B1: Per-type outbound queue drops.
        OVERLAY_OUTBOUND_QUEUE_DROP_SCP_TOTAL = "stellar_overlay_outbound_queue_drop_scp_total"
            => "Outbound SCP messages dropped (queue trim)";
        OVERLAY_OUTBOUND_QUEUE_DROP_TX_TOTAL = "stellar_overlay_outbound_queue_drop_tx_total"
            => "Outbound transaction messages dropped (queue trim)";
        OVERLAY_OUTBOUND_QUEUE_DROP_ADVERT_TOTAL = "stellar_overlay_outbound_queue_drop_advert_total"
            => "Outbound flood advert tx-hashes dropped (queue trim)";
        OVERLAY_OUTBOUND_QUEUE_DROP_DEMAND_TOTAL = "stellar_overlay_outbound_queue_drop_demand_total"
            => "Outbound flood demand tx-hashes dropped (queue trim)";

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

        // Stage B: Invariant and transaction error counters.
        LEDGER_INVARIANT_FAILURE_TOTAL = "stellar_ledger_invariant_failure_total"
            => "Total invariant check failures";
        LEDGER_TRANSACTION_INTERNAL_ERROR_TOTAL = "stellar_ledger_transaction_internal_error_total"
            => "Total transactions resulting in txINTERNAL_ERROR";

        // Stage C: SCP event counters (issue #2233).
        SCP_TIMEOUT_NOMINATE_TOTAL = "stellar_scp_timeout_nominate_total"
            => "Total SCP nomination timeout fires";
        SCP_TIMEOUT_PREPARE_TOTAL = "stellar_scp_timeout_prepare_total"
            => "Total SCP ballot (prepare) timeout fires";
        SCP_ENVELOPE_VALIDSIG_TOTAL = "stellar_scp_envelope_validsig_total"
            => "Total SCP envelopes with valid signature";
        SCP_ENVELOPE_INVALIDSIG_TOTAL = "stellar_scp_envelope_invalidsig_total"
            => "Total SCP envelopes with invalid signature";
        SCP_ENVELOPE_SIGN_TOTAL = "stellar_scp_envelope_sign_total"
            => "Total SCP envelopes signed locally";
        // SCP persist-worker spawn counters (#3796). `attempted - spawn_failed`
        // is the number of persist worker threads actually spawned.
        SCP_PERSIST_ATTEMPTED_TOTAL = "henyey_scp_persist_attempted_total"
            => "Total SCP persist worker spawns attempted (counted before the spawn)";
        SCP_PERSIST_SPAWN_FAILED_TOTAL = "henyey_scp_persist_spawn_failed_total"
            => "Total SCP persist worker spawns that failed to launch (e.g. EAGAIN)";
        SCP_VALUE_VALID_TOTAL = "stellar_scp_value_valid_total"
            => "Total SCP value validations returning valid (includes MaybeValid and MaybeValidDeferred)";
        SCP_VALUE_INVALID_TOTAL = "stellar_scp_value_invalid_total"
            => "Total SCP value validations returning invalid";
        SCP_NOMINATION_COMBINECANDIDATES_TOTAL = "stellar_scp_nomination_combinecandidates_total"
            => "Total candidate values passed to combineCandidates";

        // Parity gates from #2302 — observability for the new is_applying
        // and stale-slot skip paths.
        CONSENSUS_TRIGGER_SKIPPED_APPLYING_TOTAL =
            "henyey_consensus_trigger_skipped_applying_total"
            => "Total try_trigger_consensus invocations skipped because a ledger \
                close was in progress (parity with stellar-core HerderImpl.cpp:1440-1447)";
        CONSENSUS_TRIGGER_SKIPPED_STALE_TOTAL =
            "henyey_consensus_trigger_skipped_stale_total"
            => "Total trigger_next_ledger invocations that returned SkippedStale \
                because LCL advanced during build_nomination_value (parity with \
                stellar-core HerderImpl.cpp:1550-1562)";
        NOMINATION_TIMEOUT_SKIPPED_STALE_TOTAL =
            "henyey_nomination_timeout_skipped_stale_total"
            => "Total handle_nomination_timeout invocations that returned \
                SkippedStale because LCL advanced during build/drain";
        CONSENSUS_TRIGGER_TIMER_FIRES_TOTAL =
            "henyey_consensus_trigger_timer_fires_total"
            => "Total event-driven consensus-trigger timer firings (#2702); each \
                drives one try_trigger_consensus attempt, mirroring stellar-core's \
                mTriggerTimer → triggerNextLedger";

        // Stage E: History archive lifecycle counters (10334 dashboard).
        // All count terminal outcomes; retries within an operation are not counted.
        HISTORY_PUBLISH_SUCCESS_TOTAL = "stellar_history_publish_success_total"
            => "Checkpoint publishes that completed successfully";
        HISTORY_PUBLISH_FAILURE_TOTAL = "stellar_history_publish_failure_total"
            => "Checkpoint publishes that returned an error (panics terminate the process and are not counted)";
        HISTORY_BUCKET_APPLY_SUCCESS_TOTAL = "stellar_history_bucket_apply_success_total"
            => "Bucket-apply pipelines (HAS → restore → init ledger manager) that succeeded";
        HISTORY_BUCKET_APPLY_FAILURE_TOTAL = "stellar_history_bucket_apply_failure_total"
            => "Bucket-apply pipelines that failed";
        HISTORY_APPLY_LEDGER_CHAIN_SUCCESS_TOTAL = "stellar_history_apply_ledger_chain_success_total"
            => "Download-verify-replay ledger pipelines that completed (only counted when replay_count > 0)";
        HISTORY_APPLY_LEDGER_CHAIN_FAILURE_TOTAL = "stellar_history_apply_ledger_chain_failure_total"
            => "Download-verify-replay ledger pipelines that exhausted retries";
        HISTORY_DOWNLOAD_BUCKET_SUCCESS_TOTAL = "stellar_history_download_bucket_success_total"
            => "Per-bucket-file downloads that succeeded (terminal outcome, after archive rotation)";
        HISTORY_DOWNLOAD_BUCKET_FAILURE_TOTAL = "stellar_history_download_bucket_failure_total"
            => "Per-bucket-file downloads that failed across all archives";
        HISTORY_VERIFY_BUCKET_SUCCESS_TOTAL = "stellar_history_verify_bucket_success_total"
            => "Per-bucket hash verifications that passed";
        HISTORY_VERIFY_BUCKET_FAILURE_TOTAL = "stellar_history_verify_bucket_failure_total"
            => "Per-bucket hash verifications that detected a mismatch";
        HISTORY_DOWNLOAD_LEDGER_SUCCESS_TOTAL = "stellar_history_download_ledger_success_total"
            => "Checkpoint ledger-data downloads (headers+txs+results) that succeeded";
        HISTORY_DOWNLOAD_LEDGER_FAILURE_TOTAL = "stellar_history_download_ledger_failure_total"
            => "Checkpoint ledger-data downloads that failed across all archives";
        HISTORY_VERIFY_LEDGER_CHAIN_SUCCESS_TOTAL = "stellar_history_verify_ledger_chain_success_total"
            => "Per-attempt verify_downloaded_data calls that passed (chain + tx + result hashes)";
        HISTORY_VERIFY_LEDGER_CHAIN_FAILURE_TOTAL = "stellar_history_verify_ledger_chain_failure_total"
            => "Per-attempt verify_downloaded_data calls that failed";
        HISTORY_DOWNLOAD_HAS_SUCCESS_TOTAL = "stellar_history_download_history_archive_state_success_total"
            => "History Archive State downloads that succeeded (and passed verify_has where applicable)";
        HISTORY_DOWNLOAD_HAS_FAILURE_TOTAL = "stellar_history_download_history_archive_state_failure_total"
            => "History Archive State downloads or verifications that failed";
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
        OVERLAY_SEND_TOTAL = "stellar_overlay_send_total"
            => "Overlay messages sent by type (success-only, post-wire-send)",
            "type", henyey_overlay::OverlayMessageKind;
    }

    labeled_counters_literal {
        RECOVERY_STALLED_TICK_TOTAL = "henyey_recovery_stalled_tick_total"
            => "Recovery stalled ticks by reason",
            "reason", ["backoff_active", "forcing_catchup_not_behind", "forcing_catchup_behind",
                       "at_tip_no_scp_hard_reset", "archive_behind_peer_ahead_hard_reset",
                       "hard_reset_suppressed_archive_behind", "near_tip_peer_scp_recovery",
                       "near_tip_peer_scp_widen", "near_tip_gap1_suppressed"];
    }

    histograms {
        // Event-loop stall distribution (issue #3795).
        EVENT_LOOP_STALL_SECONDS = "henyey_event_loop_stall_seconds"
            => "Event-loop inter-tick stall duration (seconds), recorded \
                loop-side exactly once per stall when the loop resumes. \
                TRUNCATED at >=15s by design (a partial distribution — shorter \
                gaps are not recorded), and counts each stall EXACTLY ONCE, \
                whereas the watchdog sampler's WARN/ERROR log lines fire \
                repeatedly during one stall; the two must not be summed. See \
                issue #3795.";

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
        CLOSE_SOROBAN_STATE_DATA_ARC_COUNT = "henyey_ledger_close_soroban_state_data_arc_refs"
            => "Max per-shard Arc strong_count for contract_data_entries at start of soroban_state phase";
        CLOSE_SOROBAN_STATE_CODE_ARC_COUNT = "henyey_ledger_close_soroban_state_code_arc_refs"
            => "Max per-shard Arc strong_count for contract_code_entries at start of soroban_state phase";
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

        // Stage B: Ledger age, catchup, and per-tx/op apply histograms.
        LEDGER_AGE_CLOSED_SECONDS = "stellar_ledger_age_closed_seconds"
            => "Wall-clock age of ledger at close (seconds since previous close)";
        LEDGER_CATCHUP_DURATION_SECONDS = "stellar_ledger_catchup_duration_seconds"
            => "Catchup wall-clock duration in seconds";
        LEDGER_OPERATION_APPLY_SECONDS = "stellar_ledger_operation_apply_seconds"
            => "Per-operation apply cycle duration in seconds";
        LEDGER_TRANSACTION_APPLY_SECONDS = "stellar_ledger_transaction_apply_seconds"
            => "Per-transaction apply duration in seconds (ops + meta, excludes validation/fees)";

        // Stage E: History publish duration histogram.
        // Custom buckets installed in `install_recorder()` because publishes
        // typically take 30–50 s — well past the 30 s ceiling of the default
        // bucket schedule.
        HISTORY_PUBLISH_TIME_SECONDS = "stellar_history_publish_time_seconds"
            => "Wall-clock duration of a single checkpoint publish (seconds)";

        // Issue #2621 B3: SCP timing histograms (event-site recording in herder).
        SCP_TIMING_EXTERNALIZED_HIST_SECONDS = "stellar_scp_timing_externalized_hist_seconds"
            => "Time from slot creation to externalize (seconds, histogram across slots)";
        SCP_TIMING_NOMINATED_HIST_SECONDS = "stellar_scp_timing_nominated_hist_seconds"
            => "Time from first nomination to ballot protocol start (seconds, histogram)";
        SCP_TIMING_FIRST_TO_SELF_EXTERNALIZE_HIST_SECONDS = "stellar_scp_timing_first_to_self_externalize_hist_seconds"
            => "Time from first observed EXTERNALIZE to self-externalize (seconds, histogram)";

        // Issue #2648: SCP receive-to-relay latency histogram.
        SCP_RECEIVE_TO_RELAY_SECONDS = "henyey_scp_receive_to_relay_seconds"
            => "Time from SCP envelope arrival at overlay to broadcast relay (seconds)";

        // Issue #2621 B4: Peer ping round-trip time histogram (event-site recording in overlay).
        OVERLAY_CONNECTION_LATENCY_SECONDS = "stellar_overlay_connection_latency_seconds"
            => "Peer ping round-trip time (seconds)";

        // Issue #2632: Overlay recv/queue/throttle/pull metrics.
        OVERLAY_RECV_MESSAGE_SECONDS = "stellar_overlay_recv_message_seconds"
            => "Per-message-type recv processing time (seconds)";
        OVERLAY_OUTBOUND_QUEUE_DELAY_SECONDS = "stellar_overlay_outbound_queue_delay_seconds"
            => "Outbound queue delay from emplacement to send (seconds)";
        OVERLAY_FLOOD_THROTTLE_SECONDS = "stellar_overlay_flood_throttle_seconds"
            => "Outbound flood throttle duration (seconds)";
        OVERLAY_TX_PULL_LATENCY_SECONDS = "stellar_overlay_tx_pull_latency_seconds"
            => "Global tx demand-to-fulfillment latency (seconds)";
        OVERLAY_PEER_TX_PULL_LATENCY_SECONDS = "stellar_overlay_peer_tx_pull_latency_seconds"
            => "Per-peer tx demand-to-fulfillment latency (seconds)";
    }
}

// ── Loadgen Prometheus → medida-compat mapping (#3572) ─────────────────
//
// The loadgen meters render in the Prometheus exposition with UNDERSCORE
// names (`loadgen_run_start`, …). The stellar-core compat `/metrics` endpoint
// — the medida JSON that supercluster's `IsLoadGenComplete` reads — needs the
// DOTTED medida key names (`loadgen.run.start`, …). This table is the single
// source of truth for that translation, consumed by BOTH the compat-handler's
// `parse_loadgen_counts` (Prometheus name → count) and its JSON builder
// (dotted key + event_type). Keep it beside the loadgen catalog so the two
// surfaces can never re-drift.
//
// IMPORTANT: this is NOT `replace('_', '.')`. The soroban third components keep
// their underscore (`loadgen.soroban.setup_invoke`, not `…setup.invoke`),
// matching stellar-core's medida keys verbatim. Verified against
// `src/simulation/LoadGenerator.cpp:114-1483` and `loadgen.run.start` ←
// `main/ApplicationImpl.cpp:1156`.
//
// Entries: (Prometheus exposition name, dotted medida key, medida event_type).
pub(crate) const LOADGEN_COMPAT_MAP: &[(&str, &str, &str)] = &[
    (LOADGEN_RUN_START.0, "loadgen.run.start", "run"),
    (LOADGEN_RUN_COMPLETE.0, "loadgen.run.complete", "run"),
    (LOADGEN_RUN_FAILED.0, "loadgen.run.failed", "run"),
    (
        LOADGEN_ACCOUNT_CREATED.0,
        "loadgen.account.created",
        "account",
    ),
    (LOADGEN_TXN_ATTEMPTED.0, "loadgen.txn.attempted", "txn"),
    (LOADGEN_TXN_REJECTED.0, "loadgen.txn.rejected", "txn"),
    (LOADGEN_TXN_BYTES.0, "loadgen.txn.bytes", "txn"),
    (
        LOADGEN_PAYMENT_SUBMITTED.0,
        "loadgen.payment.submitted",
        "op",
    ),
    (LOADGEN_PAYMENT_BYTES.0, "loadgen.payment.bytes", "txn"),
    (LOADGEN_SOROBAN_UPLOAD.0, "loadgen.soroban.upload", "txn"),
    (LOADGEN_SOROBAN_INVOKE.0, "loadgen.soroban.invoke", "txn"),
    (
        LOADGEN_SOROBAN_SETUP_INVOKE.0,
        "loadgen.soroban.setup_invoke",
        "txn",
    ),
    (
        LOADGEN_SOROBAN_SETUP_UPGRADE.0,
        "loadgen.soroban.setup_upgrade",
        "txn",
    ),
    (
        LOADGEN_SOROBAN_CREATE_UPGRADE.0,
        "loadgen.soroban.create_upgrade",
        "txn",
    ),
    (LOADGEN_STEP_COUNT.0, "loadgen.step.count", "step"),
];

/// The full loadgen meter catalog, in catalog order.
///
/// Single source of truth for the set of meters reset by [`reset_loadgen_meters`]
/// on `clearmetrics` (#3630). Must stay in sync with the `loadgen_*` entries in
/// the catalog and with [`LOADGEN_COMPAT_MAP`].
pub const LOADGEN_METERS: &[GaugeMetric] = &[
    LOADGEN_RUN_START,
    LOADGEN_RUN_COMPLETE,
    LOADGEN_RUN_FAILED,
    LOADGEN_ACCOUNT_CREATED,
    LOADGEN_TXN_ATTEMPTED,
    LOADGEN_TXN_REJECTED,
    LOADGEN_TXN_BYTES,
    LOADGEN_PAYMENT_SUBMITTED,
    LOADGEN_PAYMENT_BYTES,
    LOADGEN_SOROBAN_UPLOAD,
    LOADGEN_SOROBAN_INVOKE,
    LOADGEN_SOROBAN_SETUP_INVOKE,
    LOADGEN_SOROBAN_SETUP_UPGRADE,
    LOADGEN_SOROBAN_CREATE_UPGRADE,
    LOADGEN_STEP_COUNT,
];

// ── Scrape-time refresh ────────────────────────────────────────────────

/// Update scrape-time gauges and absolute counters from current App state.
///
/// Called by the `/metrics` handler immediately before `handle.render()`.
pub(crate) async fn refresh_gauges(state: &ServerState) {
    let uptime = state.start_time.elapsed().as_secs();
    let ledger_seq = state.app.ledger_info().ledger_seq;
    let peer_count = state.app.peer_count().await;
    let pending_txs = state.app.pending_transaction_count() as u64;
    let queued_ledgers = state.app.syncing_ledgers_count().await;
    let snap = state.app.metrics_snapshot();

    // Stellar-compatible gauges.
    LEDGER_SEQUENCE.set(ledger_seq as f64);
    PEER_COUNT.set(peer_count as f64);
    PENDING_TRANSACTIONS.set(pending_txs as f64);
    UPTIME_SECONDS.set(uptime as f64);
    IS_VALIDATOR.set(if snap.is_validator { 1.0 } else { 0.0 });

    // Stage B: Memory queued ledgers.
    LEDGER_MEMORY_QUEUED_LEDGERS.set(queued_ledgers as f64);

    // Meta stream counters — only set when active (matching current conditional behavior).
    if snap.meta_stream_bytes_total > 0 || snap.meta_stream_writes_total > 0 {
        META_STREAM_BYTES_TOTAL.absolute(snap.meta_stream_bytes_total);
        META_STREAM_WRITES_TOTAL.absolute(snap.meta_stream_writes_total);
    }

    // SCP verify pipeline — absolute counters.
    let sv = &snap.scp_verify;
    for (reason, &count) in sv.prefilter_counters.iter() {
        SCP_PREFILTER_REJECTS_TOTAL.absolute(reason.label(), count);
    }
    SCP_POST_VERIFY_DROPS_TOTAL.absolute(sv.post_verify_drops);
    for (reason, &count) in sv.pv_counters.iter() {
        SCP_POST_VERIFY_TOTAL.absolute(reason.label(), count);
    }

    // SCP verify pipeline — gauges.
    SCP_VERIFY_INPUT_BACKLOG.set(sv.verify_input_backlog as f64);
    SCP_VERIFY_INPUT_BACKLOG_PEAK.set(sv.verify_input_backlog_peak as f64);
    SCP_VERIFY_OUTPUT_BACKLOG.set(sv.verify_output_backlog as f64);
    SCP_VERIFIER_THREAD_STATE.set(sv.verifier_thread_state as f64);
    SCP_VERIFY_LATENCY_US_SUM.set(sv.verify_latency_us_sum as f64);
    SCP_VERIFY_LATENCY_US_COUNT.set(sv.verify_latency_count as f64);
    SCP_SCHEDULED_DEDUP_TOTAL.absolute(sv.scheduled_dedup_count);

    // Overlay fetch channel.
    OVERLAY_FETCH_CHANNEL_DEPTH.set(snap.overlay_fetch_channel.depth as f64);
    OVERLAY_FETCH_CHANNEL_DEPTH_MAX.set(snap.overlay_fetch_channel.depth_max as f64);

    // Catchup.
    POST_CATCHUP_HARD_RESET_TOTAL.absolute(snap.post_catchup_hard_reset_total);

    // SCP/herder counters.
    let (scp_sent, scp_received) = state.app.scp_envelope_counters();
    SCP_ENVELOPE_EMIT_TOTAL.absolute(scp_sent);
    SCP_ENVELOPE_RECEIVE_TOTAL.absolute(scp_received);
    HERDER_LOST_SYNC_TOTAL.absolute(state.app.lost_sync_count());

    let herder = state.app.herder_stats();
    let herder_state_val = match herder.state {
        henyey_herder::HerderState::Booting => 0.0,
        henyey_herder::HerderState::Syncing => 1.0,
        henyey_herder::HerderState::Tracking => 2.0,
    };
    HERDER_STATE.set(herder_state_val);
    HERDER_PENDING_ENVELOPES.set(herder.pending_envelopes as f64);
    HERDER_CACHED_TX_SETS.set(herder.cached_tx_sets as f64);

    let pstats = &herder.pending_envelope_stats;
    HERDER_PENDING_RECEIVED_TOTAL.absolute(pstats.received);
    HERDER_PENDING_DUPLICATES_TOTAL.absolute(pstats.duplicates);
    HERDER_PENDING_TOO_OLD_TOTAL.absolute(pstats.too_old);
    HERDER_PENDING_EVICTED_TOTAL.absolute(pstats.evicted);

    // Bucket merge counters.
    let mc = state.app.merge_counters_snapshot();
    BUCKET_MERGE_COMPLETED_TOTAL.absolute(mc.merges_completed);
    BUCKET_MERGE_TIME_US_TOTAL.absolute(mc.merge_time_us);
    BUCKET_MERGE_NEW_LIVE_TOTAL.absolute(mc.new_live_entries);
    BUCKET_MERGE_NEW_DEAD_TOTAL.absolute(mc.new_dead_entries);
    BUCKET_MERGE_NEW_INIT_TOTAL.absolute(mc.new_init_entries);
    BUCKET_MERGE_NEW_META_TOTAL.absolute(mc.new_meta_entries);
    BUCKET_MERGE_SHADOWED_TOTAL.absolute(mc.old_entries_shadowed);
    BUCKET_MERGE_ANNIHILATED_TOTAL.absolute(mc.entries_annihilated);

    // State-archival eviction counters.
    let ec = state.app.eviction_counters_snapshot();
    EVICTION_ENTRIES_EVICTED_TOTAL.absolute(ec.entries_evicted);
    EVICTION_BYTES_SCANNED_TOTAL.absolute(ec.bytes_scanned);
    EVICTION_INCOMPLETE_SCAN_TOTAL.absolute(ec.incomplete_bucket_scans);
    // period + age are last-cycle SET-semantics gauges; `.absolute(...)`
    // faithfully overwrites (matching core's medida Counter::set_count).
    EVICTION_PERIOD.absolute(ec.eviction_cycle_period);
    EVICTION_AGE.absolute(ec.average_evicted_entry_age);

    // Overlay counters (if overlay is running).
    if let Some(ov) = state.app.overlay_metrics_snapshot().await {
        OVERLAY_MESSAGE_READ_TOTAL.absolute(ov.messages_read);
        OVERLAY_MESSAGE_WRITE_TOTAL.absolute(ov.messages_written);
        OVERLAY_MESSAGE_BROADCAST_TOTAL.absolute(ov.messages_broadcast);
        OVERLAY_FETCH_MESSAGES_DROPPED_TOTAL.absolute(ov.fetch_messages_dropped);
        OVERLAY_CATCHUP_MESSAGES_DROPPED_TOTAL.absolute(ov.catchup_messages_dropped);
        OVERLAY_ERROR_READ_TOTAL.absolute(ov.errors_read);
        OVERLAY_ERROR_WRITE_TOTAL.absolute(ov.errors_write);
        OVERLAY_TIMEOUT_IDLE_TOTAL.absolute(ov.timeouts_idle);
        OVERLAY_TIMEOUT_STRAGGLER_TOTAL.absolute(ov.timeouts_straggler);

        // Stage F.1: byte / async I/O counters (issue #2236).
        OVERLAY_BYTE_READ_TOTAL.absolute(ov.bytes_read);
        OVERLAY_BYTE_WRITE_TOTAL.absolute(ov.bytes_written);
        OVERLAY_ASYNC_READ_TOTAL.absolute(ov.async_read);
        OVERLAY_ASYNC_WRITE_TOTAL.absolute(ov.async_write);

        // Stage F.1: connection lifecycle counters (issue #2236).
        OVERLAY_INBOUND_ATTEMPT_TOTAL.absolute(ov.inbound_attempt);
        OVERLAY_INBOUND_ESTABLISH_TOTAL.absolute(ov.inbound_establish);
        OVERLAY_INBOUND_DROP_TOTAL.absolute(ov.inbound_drop);
        OVERLAY_INBOUND_DROP_REMOTE_TOTAL.absolute(ov.inbound_drop_remote);
        OVERLAY_INBOUND_DROP_LOCAL_TOTAL.absolute(ov.inbound_drop_local);
        OVERLAY_INBOUND_REJECT_TOTAL.absolute(ov.inbound_reject);
        OVERLAY_INBOUND_AUTHENTICATED_TOTAL.absolute(ov.inbound_authenticated_total);
        OVERLAY_OUTBOUND_ATTEMPT_TOTAL.absolute(ov.outbound_attempt);
        OVERLAY_OUTBOUND_ESTABLISH_TOTAL.absolute(ov.outbound_establish);
        OVERLAY_OUTBOUND_DROP_TOTAL.absolute(ov.outbound_drop);
        OVERLAY_OUTBOUND_REJECT_TOTAL.absolute(ov.outbound_reject);

        // Stage F.2: flood / fetch / item-fetcher counters (issue #2244).
        OVERLAY_FLOOD_BROADCAST_TOTAL.absolute(ov.flood_broadcast);
        OVERLAY_FLOOD_DUPLICATE_RECV_TOTAL.absolute(ov.flood_duplicate_recv);
        OVERLAY_FLOOD_UNIQUE_RECV_TOTAL.absolute(ov.flood_unique_recv);
        // Issue #2648: SCP-specific flood counters.
        OVERLAY_SCP_FLOOD_UNIQUE_RECV_TOTAL.absolute(ov.scp_flood_unique_recv);
        OVERLAY_SCP_FLOOD_DUPLICATE_RECV_TOTAL.absolute(ov.scp_flood_duplicate_recv);
        OVERLAY_FETCH_DUPLICATE_RECV_TOTAL.absolute(ov.fetch_duplicate_recv);
        OVERLAY_FETCH_UNIQUE_RECV_TOTAL.absolute(ov.fetch_unique_recv);
        OVERLAY_ITEM_FETCHER_NEXT_PEER_TOTAL.absolute(ov.item_fetcher_next_peer);
        OVERLAY_FLOOD_SHED_UNSYNCED_TOTAL.absolute(ov.flood_shed_unsynced);
        OVERLAY_MEMORY_FLOOD_KNOWN.set(ov.flood_known_count as f64);

        // Stage F.3: per-message-type send counter (issue #2245).
        for kind in henyey_overlay::OverlayMessageKind::ALL {
            OVERLAY_SEND_TOTAL.absolute(kind.label(), ov.send_by_type[kind as usize]);
        }
    } else {
        // Ensure gauge is zeroed if overlay stops.
        OVERLAY_MEMORY_FLOOD_KNOWN.set(0.0);
    }

    // Ledger close stats (Phase 2).
    let lcs = state.app.last_close_stats();
    LEDGER_TX_COUNT.set(lcs.tx_count as f64);
    LEDGER_OP_COUNT.set(lcs.op_count as f64);
    LEDGER_TX_SUCCESS_COUNT.set(lcs.tx_success_count as f64);
    LEDGER_TX_FAILED_COUNT.set(lcs.tx_failed_count as f64);
    LEDGER_TOTAL_FEES.set(lcs.total_fees as f64);
    LEDGER_ENTRIES_CREATED.set(lcs.entries_created as f64);
    LEDGER_ENTRIES_UPDATED.set(lcs.entries_updated as f64);
    LEDGER_ENTRIES_DELETED.set(lcs.entries_deleted as f64);

    // Ledger apply timing (Phase 2).
    if let Some(perf) = state.app.last_close_perf() {
        LEDGER_APPLY_US.set(perf.total_us as f64);
    }

    // Herder tx queue stats (Phase 2).
    let tq = &herder.tx_queue_stats;
    HERDER_TX_QUEUE_ACCOUNTS.set(tq.account_count as f64);
    HERDER_TX_QUEUE_BANNED.set(tq.banned_count as f64);
    HERDER_TX_QUEUE_SEEN.set(tq.seen_count as f64);
    HERDER_ARB_TX_SEEN.absolute(tq.arb_tx_seen);
    HERDER_ARB_TX_DROPPED.absolute(tq.arb_tx_dropped);

    // Herder pending-tx age gauges (Stage D).
    HERDER_PENDING_TXS_AGE0.set(tq.pending_txs_age[0] as f64);
    HERDER_PENDING_TXS_AGE1.set(tq.pending_txs_age[1] as f64);
    HERDER_PENDING_TXS_AGE2.set(tq.pending_txs_age[2] as f64);
    HERDER_PENDING_TXS_AGE3.set(tq.pending_txs_age[3] as f64);

    // Herder pending envelope stats — added + released (Phase 2).
    HERDER_PENDING_ADDED_TOTAL.absolute(pstats.added);
    HERDER_PENDING_RELEASED_TOTAL.absolute(pstats.released);

    // Clock drift (Phase 2) — always write, zeros when no completed window.
    if let Some(ds) = state.app.drift_stats() {
        DRIFT_MIN_SECONDS.set(ds.min as f64);
        DRIFT_MAX_SECONDS.set(ds.max as f64);
        DRIFT_MEDIAN_SECONDS.set(ds.median as f64);
        DRIFT_P75_SECONDS.set(ds.p75 as f64);
        DRIFT_SAMPLE_COUNT.set(ds.sample_count as f64);
    } else {
        DRIFT_MIN_SECONDS.set(0.0);
        DRIFT_MAX_SECONDS.set(0.0);
        DRIFT_MEDIAN_SECONDS.set(0.0);
        DRIFT_P75_SECONDS.set(0.0);
        DRIFT_SAMPLE_COUNT.set(0.0);
    }

    // jemalloc allocator stats — always available, zeros when jemalloc is not enabled.
    let alloc = henyey_ledger::memory_report::AllocatorStats::capture();
    JEMALLOC_ALLOCATED_BYTES.set(alloc.allocated as f64);
    JEMALLOC_ACTIVE_BYTES.set(alloc.active as f64);
    JEMALLOC_RESIDENT_BYTES.set(alloc.resident as f64);
    JEMALLOC_MAPPED_BYTES.set(alloc.mapped as f64);
    JEMALLOC_RETAINED_BYTES.set(alloc.retained as f64);
    if alloc.allocated > 0 {
        let frag =
            (alloc.resident as f64 - alloc.allocated as f64) / alloc.allocated as f64 * 100.0;
        JEMALLOC_FRAGMENTATION_PCT.set(frag);
    }

    // Phase 3: Ledger apply cumulative counters.
    LEDGER_APPLY_SUCCESS_TOTAL.absolute(snap.cumulative_apply_success);
    LEDGER_APPLY_FAILURE_TOTAL.absolute(snap.cumulative_apply_failure);
    LEDGER_APPLY_SOROBAN_SUCCESS_TOTAL.absolute(snap.cumulative_soroban_success);
    LEDGER_APPLY_SOROBAN_FAILURE_TOTAL.absolute(snap.cumulative_soroban_failure);
    LEDGER_APPLY_SOROBAN_MAX_CLUSTERS.set(snap.soroban_max_cluster_count as f64);
    LEDGER_APPLY_SOROBAN_STAGES.set(snap.soroban_stage_count as f64);

    // Phase 3: Ledger age from header close_time.
    let ledger_info = state.app.ledger_info();
    if ledger_info.close_time > 0 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let age = now.saturating_sub(ledger_info.close_time);
        LEDGER_AGE_CURRENT_SECONDS.set(age as f64);
    }

    // Stage A (10334 dashboard): Health Summary & Version gauges.
    STARTED_ON.set(state.started_on_epoch);
    LEDGER_VERSION.set(ledger_info.protocol_version as f64);
    PROTOCOL_VERSION.set(state.app.config().network.max_protocol_version as f64);

    // Phase 3: Soroban config limits (gauges — snapshot values).
    if let Some(info) = state.app.soroban_network_info() {
        SOROBAN_CONFIG_CONTRACT_MAX_RW_KEY_BYTE.set(info.max_contract_data_key_size as f64);
        SOROBAN_CONFIG_CONTRACT_MAX_RW_DATA_BYTE.set(info.max_contract_data_entry_size as f64);
        SOROBAN_CONFIG_CONTRACT_MAX_RW_CODE_BYTE.set(info.max_contract_size as f64);
        SOROBAN_CONFIG_TX_MAX_SIZE_BYTE.set(info.tx_max_size_bytes as f64);
        SOROBAN_CONFIG_TX_MAX_CPU_INSN.set(info.tx_max_instructions.max(0) as f64);
        SOROBAN_CONFIG_TX_MAX_MEM_BYTE.set(info.tx_memory_limit as f64);
        SOROBAN_CONFIG_TX_MAX_READ_ENTRY.set(info.tx_max_read_ledger_entries as f64);
        SOROBAN_CONFIG_TX_MAX_READ_LEDGER_BYTE.set(info.tx_max_read_bytes as f64);
        SOROBAN_CONFIG_TX_MAX_WRITE_ENTRY.set(info.tx_max_write_ledger_entries as f64);
        SOROBAN_CONFIG_TX_MAX_WRITE_LEDGER_BYTE.set(info.tx_max_write_bytes as f64);
        SOROBAN_CONFIG_TX_MAX_EMIT_EVENT_BYTE.set(info.tx_max_contract_events_size_bytes as f64);
        SOROBAN_CONFIG_LEDGER_MAX_TX_COUNT.set(info.ledger_max_tx_count as f64);
        SOROBAN_CONFIG_LEDGER_MAX_CPU_INSN.set(info.ledger_max_instructions.max(0) as f64);
        SOROBAN_CONFIG_LEDGER_MAX_TXS_SIZE_BYTE.set(info.ledger_max_tx_size_bytes as f64);
        SOROBAN_CONFIG_LEDGER_MAX_READ_ENTRY.set(info.ledger_max_read_ledger_entries as f64);
        SOROBAN_CONFIG_LEDGER_MAX_READ_LEDGER_BYTE.set(info.ledger_max_read_bytes as f64);
        SOROBAN_CONFIG_LEDGER_MAX_WRITE_ENTRY.set(info.ledger_max_write_ledger_entries as f64);
        SOROBAN_CONFIG_LEDGER_MAX_WRITE_LEDGER_BYTE.set(info.ledger_max_write_bytes as f64);
        SOROBAN_CONFIG_BUCKET_LIST_TARGET_SIZE_BYTE.set(info.state_target_size_bytes.max(0) as f64);
        SOROBAN_CONFIG_FEE_WRITE_1KB.set(info.fee_write_1kb.max(0) as f64);
    }

    // Phase 3: Henyey-specific observability.
    LEDGER_BUCKET_CACHE_HIT_RATIO.set(snap.bucket_cache_hit_ratio);
    LEDGER_SNAPSHOT_CACHE_HIT_RATIO.set(snap.snapshot_cache_hit_ratio);
    LEDGER_SNAPSHOT_CACHE_FALLBACK_LOOKUPS.set(snap.snapshot_cache_fallback_lookups as f64);

    // Phase 4: Overlay connection breakdown.
    if let Some(breakdown) = state.app.overlay_connection_breakdown().await {
        OVERLAY_INBOUND_AUTHENTICATED.set(breakdown.inbound_authenticated as f64);
        OVERLAY_OUTBOUND_AUTHENTICATED.set(breakdown.outbound_authenticated as f64);
        OVERLAY_INBOUND_PENDING.set(breakdown.inbound_pending as f64);
        OVERLAY_OUTBOUND_PENDING.set(breakdown.outbound_pending as f64);
    } else {
        OVERLAY_INBOUND_AUTHENTICATED.set(0.0);
        OVERLAY_OUTBOUND_AUTHENTICATED.set(0.0);
        OVERLAY_INBOUND_PENDING.set(0.0);
        OVERLAY_OUTBOUND_PENDING.set(0.0);
    }

    // Phase 4: Process health (Linux-only).
    if let Some(fds) = process_open_fds() {
        PROCESS_OPEN_FDS.set(fds as f64);
    }
    if let Some(max) = process_max_fds() {
        PROCESS_MAX_FDS.set(max as f64);
    }

    // Phase 4: Quorum health.
    if let Some(qh) = state.app.quorum_health() {
        QUORUM_AGREE.set(qh.agree as f64);
        QUORUM_MISSING.set(qh.missing as f64);
        QUORUM_DISAGREE.set(qh.disagree as f64);
        QUORUM_FAIL_AT.set(qh.fail_at as f64);
        QUORUM_DELAYED.set(qh.delayed as f64);
    } else {
        QUORUM_AGREE.set(0.0);
        QUORUM_MISSING.set(0.0);
        QUORUM_DISAGREE.set(0.0);
        QUORUM_FAIL_AT.set(0.0);
        QUORUM_DELAYED.set(0.0);
    }

    // Phase 4: Quorum transitive intersection.
    // Only set when a publishable result exists (gauges_no_prereg keeps it
    // absent until the first intersecting check completes).
    if let Some(intersection) = state.app.quorum_intersection_publishable() {
        QUORUM_TRANSITIVE_INTERSECTION.set(if intersection { 1.0 } else { 0.0 });
    }

    // Phase 4: SCP timing.
    if let Some(timing) = state.app.scp_timing() {
        if let Some(ext_secs) = timing.externalize_duration_secs {
            SCP_TIMING_EXTERNALIZED_SECONDS.set(ext_secs);
        }
        if let Some(nom_secs) = timing.nomination_duration_secs {
            SCP_TIMING_NOMINATED_SECONDS.set(nom_secs);
        } else {
            SCP_TIMING_NOMINATED_SECONDS.set(0.0);
        }
        if let Some(lag_secs) = timing.first_to_self_externalize_secs {
            SCP_TIMING_FIRST_TO_SELF_EXTERNALIZE_SECONDS.set(lag_secs);
        } else {
            SCP_TIMING_FIRST_TO_SELF_EXTERNALIZE_SECONDS.set(0.0);
        }
    } else {
        // No timing available (e.g., after catchup cleared it). Reset gauges.
        SCP_TIMING_EXTERNALIZED_SECONDS.set(0.0);
        SCP_TIMING_NOMINATED_SECONDS.set(0.0);
        SCP_TIMING_FIRST_TO_SELF_EXTERNALIZE_SECONDS.set(0.0);
    }

    // Phase 5: Archive cache absolute counters.
    ARCHIVE_CACHE_FRESH_TOTAL.absolute(snap.archive_cache_fresh);
    ARCHIVE_CACHE_STALE_TOTAL.absolute(snap.archive_cache_stale);
    ARCHIVE_CACHE_COLD_TOTAL.absolute(snap.archive_cache_cold);
    ARCHIVE_CACHE_REFRESH_SUCCESS_TOTAL.absolute(snap.archive_cache_refresh_success);
    ARCHIVE_CACHE_REFRESH_ERROR_TOTAL.absolute(snap.archive_cache_refresh_error);
    ARCHIVE_CACHE_REFRESH_TIMEOUT_TOTAL.absolute(snap.archive_cache_refresh_timeout);

    // Phase 5: Archive cache gauges.
    ARCHIVE_CACHE_AGE_SECONDS.set(snap.archive_cache_age_secs);
    ARCHIVE_CACHE_POPULATED.set(if snap.archive_cache_populated {
        1.0
    } else {
        0.0
    });

    // Phase 6: TxSet exhaustion stuck gauge.
    let exhausted_since = state.app.tx_set_exhausted_since_offset();
    if exhausted_since > 0 {
        let stuck_secs = uptime.saturating_sub(exhausted_since);
        RECOVERY_TX_SET_STUCK_SECONDS.set(stuck_secs as f64);
    } else {
        RECOVERY_TX_SET_STUCK_SECONDS.set(0.0);
    }

    // Stage C: SCP metrics (issue #2233).
    SCP_PHASE.set(snap.scp_phase as f64);
    SCP_MEMORY_CUMULATIVE_STATEMENTS.set(snap.scp_cumulative_statements as f64);
    SCP_TIMEOUT_NOMINATE_TOTAL.absolute(snap.nomination_timeout_fires);
    SCP_TIMEOUT_PREPARE_TOTAL.absolute(snap.ballot_timeout_fires);
    CONSENSUS_TRIGGER_SKIPPED_APPLYING_TOTAL.absolute(snap.consensus_trigger_skipped_applying);
    CONSENSUS_TRIGGER_SKIPPED_STALE_TOTAL.absolute(snap.consensus_trigger_skipped_stale);
    NOMINATION_TIMEOUT_SKIPPED_STALE_TOTAL.absolute(snap.nomination_timeout_skipped_stale);
    CONSENSUS_TRIGGER_TIMER_FIRES_TOTAL.absolute(snap.consensus_trigger_timer_fires);
    SCP_ENVELOPE_VALIDSIG_TOTAL.absolute(snap.scp.envelope_validsig_total);
    SCP_ENVELOPE_INVALIDSIG_TOTAL.absolute(snap.scp.envelope_invalidsig_total);
    SCP_ENVELOPE_SIGN_TOTAL.absolute(snap.scp.envelope_sign_total);
    SCP_VALUE_VALID_TOTAL.absolute(snap.scp.value_valid_total);
    SCP_VALUE_INVALID_TOTAL.absolute(snap.scp.value_invalid_total);
    SCP_NOMINATION_COMBINECANDIDATES_TOTAL.absolute(snap.scp.combine_candidates_total);

    // SCP persist-worker backlog (#3796). Live + peak gauges plus monotonic
    // attempt/failure counters (pushed with `.absolute` = fetch_max, matching
    // the other cumulative SCP counters — both values are monotonic).
    SCP_PENDING_PERSISTS.set(snap.scp_persist.pending as f64);
    SCP_PENDING_PERSISTS_PEAK.set(snap.scp_persist.pending_peak as f64);
    SCP_PERSIST_ATTEMPTED_TOTAL.absolute(snap.scp_persist.attempted_total);
    SCP_PERSIST_SPAWN_FAILED_TOTAL.absolute(snap.scp_persist.spawn_failed_total);

    // Feed the medida-compat scp meters (`scp.value.valid`/`scp.value.invalid`)
    // for the stellar-core-compatible /metrics endpoint by delta-marking the
    // cumulative totals on this periodic refresh, so marks are spread over time
    // for a truer EWMA shape than dumping the whole backlog at scrape (#3296).
    crate::medida_compat::medida_compat()
        .feed_scp(snap.scp.value_valid_total, snap.scp.value_invalid_total);
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

// ── Histogram bucket schedules ─────────────────────────────────────────

/// Default histogram buckets for general-purpose timing metrics.
const DEFAULT_BUCKETS: &[f64] = &[0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 30.0];

/// Buckets for close-cadence histograms (close_cycle, slot_to_close, etc.).
/// Dense in the 4–8 s range where mainnet 5 s slot observations concentrate,
/// avoiding the misleading 25 s interpolation gap of the default schedule.
const CLOSE_CADENCE_BUCKETS: &[f64] = &[
    0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 3.0, 4.0, 4.5, 5.0, 5.5, 6.0, 7.0, 8.0, 10.0, 15.0, 20.0, 30.0,
    60.0,
];

/// Buckets for SCP timing histograms (externalize, nominate, first-to-self lag).
const SCP_TIMING_BUCKETS: &[f64] = &[
    0.1, 0.25, 0.5, 1.0, 2.0, 3.0, 4.0, 5.0, 7.0, 10.0, 15.0, 20.0, 30.0,
];

/// Buckets for peer ping round-trip time.
const PING_RTT_BUCKETS: &[f64] = &[0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0];

/// Buckets for SCP receive-to-relay latency (issue #2648).
/// Dense in the 10ms–2s range where relay latency concentrates.
const SCP_RELAY_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0,
];

/// Arc strong_count bucket boundaries for soroban_state diagnostics.
const SOROBAN_ARC_COUNT_BUCKETS: &[f64] = &[1.0, 2.0, 3.0, 5.0, 10.0];

/// Configure all histogram bucket overrides on a `PrometheusBuilder`.
///
/// Shared between `install_recorder()` and `ensure_test_recorder()` so that
/// production and test bucket schedules cannot drift.
fn configure_histogram_buckets(builder: PrometheusBuilder) -> PrometheusBuilder {
    builder
        .set_buckets(DEFAULT_BUCKETS)
        .expect("valid default histogram buckets")
        // Close-cadence histograms — dense around the 5 s slot interval.
        .set_buckets_for_metric(
            Matcher::Full(CLOSE_CYCLE_SECONDS.to_string()),
            CLOSE_CADENCE_BUCKETS,
        )
        .expect("valid close_cycle histogram buckets")
        .set_buckets_for_metric(
            Matcher::Full(SLOT_TO_CLOSE_LATENCY_SECONDS.to_string()),
            CLOSE_CADENCE_BUCKETS,
        )
        .expect("valid slot_to_close histogram buckets")
        .set_buckets_for_metric(
            Matcher::Full(CLOSE_HANDLE_COMPLETE_SECONDS.to_string()),
            CLOSE_CADENCE_BUCKETS,
        )
        .expect("valid close_handle_complete histogram buckets")
        .set_buckets_for_metric(
            Matcher::Full(CLOSE_POST_COMPLETE_SECONDS.to_string()),
            CLOSE_CADENCE_BUCKETS,
        )
        .expect("valid close_post_complete histogram buckets")
        .set_buckets_for_metric(
            Matcher::Full(CLOSE_DISPATCH_TO_JOIN_SECONDS.to_string()),
            CLOSE_CADENCE_BUCKETS,
        )
        .expect("valid close_dispatch_to_join histogram buckets")
        .set_buckets_for_metric(
            Matcher::Full(PERSIST_DISPATCH_TO_JOIN_SECONDS.to_string()),
            CLOSE_CADENCE_BUCKETS,
        )
        .expect("valid persist_dispatch_to_join histogram buckets")
        // Existing per-metric overrides.
        .set_buckets_for_metric(
            Matcher::Full(LEDGER_CLOSE_DURATION_SECONDS.to_string()),
            &[0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0],
        )
        .expect("valid ledger close histogram buckets")
        .set_buckets_for_metric(
            Matcher::Full(LEDGER_AGE_CLOSED_SECONDS.to_string()),
            &[1.0, 2.0, 3.0, 5.0, 7.0, 10.0, 15.0, 20.0, 30.0, 60.0],
        )
        .expect("valid ledger age closed histogram buckets")
        .set_buckets_for_metric(
            Matcher::Full(LEDGER_CATCHUP_DURATION_SECONDS.to_string()),
            &[1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0],
        )
        .expect("valid catchup duration histogram buckets")
        .set_buckets_for_metric(
            Matcher::Full(HISTORY_PUBLISH_TIME_SECONDS.to_string()),
            &[1.0, 5.0, 10.0, 20.0, 30.0, 45.0, 60.0, 90.0, 120.0, 300.0],
        )
        .expect("valid history publish histogram buckets")
        // SCP timing histograms — slot-cadence durations.
        .set_buckets_for_metric(
            Matcher::Full(SCP_TIMING_EXTERNALIZED_HIST_SECONDS.to_string()),
            SCP_TIMING_BUCKETS,
        )
        .expect("valid scp externalized histogram buckets")
        .set_buckets_for_metric(
            Matcher::Full(SCP_TIMING_NOMINATED_HIST_SECONDS.to_string()),
            SCP_TIMING_BUCKETS,
        )
        .expect("valid scp nominated histogram buckets")
        .set_buckets_for_metric(
            Matcher::Full(SCP_TIMING_FIRST_TO_SELF_EXTERNALIZE_HIST_SECONDS.to_string()),
            SCP_TIMING_BUCKETS,
        )
        .expect("valid scp first_to_self histogram buckets")
        // Ping RTT histogram.
        .set_buckets_for_metric(
            Matcher::Full(OVERLAY_CONNECTION_LATENCY_SECONDS.to_string()),
            PING_RTT_BUCKETS,
        )
        .expect("valid ping RTT histogram buckets")
        // SCP receive-to-relay histogram (issue #2648).
        .set_buckets_for_metric(
            Matcher::Full(SCP_RECEIVE_TO_RELAY_SECONDS.to_string()),
            SCP_RELAY_BUCKETS,
        )
        .expect("valid scp relay histogram buckets")
        // Soroban state Arc strong_count histograms (issue #2810).
        .set_buckets_for_metric(
            Matcher::Full(CLOSE_SOROBAN_STATE_DATA_ARC_COUNT.to_string()),
            SOROBAN_ARC_COUNT_BUCKETS,
        )
        .expect("valid soroban state data arc count histogram buckets")
        .set_buckets_for_metric(
            Matcher::Full(CLOSE_SOROBAN_STATE_CODE_ARC_COUNT.to_string()),
            SOROBAN_ARC_COUNT_BUCKETS,
        )
        .expect("valid soroban state code arc count histogram buckets")
}

// ── Test helper ────────────────────────────────────────────────────────

/// Install the metrics recorder once per test process and return a handle.
///
/// The `metrics` global recorder is one-shot per process. Since Rust tests
/// run in parallel within the same process, this helper ensures the recorder
/// is installed exactly once. Subsequent calls return the same handle.
#[cfg(test)]
pub(crate) fn ensure_test_recorder() -> &'static PrometheusHandle {
    use std::sync::OnceLock;
    static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
    HANDLE.get_or_init(|| {
        configure_histogram_buckets(PrometheusBuilder::new())
            .install_recorder()
            .expect("metrics recorder should install successfully")
    })
}

/// Install the production metrics recorder.
///
/// Returns the `PrometheusHandle` for use by the `/metrics` endpoint.
pub fn install_recorder() -> PrometheusHandle {
    let handle = configure_histogram_buckets(PrometheusBuilder::new())
        .install_recorder()
        .expect("metrics recorder should install successfully");
    describe_metrics();
    register_label_series();
    handle
}

/// Reset the loadgen-domain meters to zero (#3630).
///
/// stellar-core's `ApplicationImpl::clearMetrics` runs a `MetricResetter` over
/// the requested domain, and medida meters genuinely reset to 0. Supercluster's
/// max-TPS / min-block-time binary search calls `clearmetrics` before every step
/// and then reads the loadgen meters per run, so without a real reset the
/// `loadgen_txn_attempted` progress counter accumulates across steps and a stale
/// `loadgen_run_failed` from a prior step makes every subsequent
/// `IsLoadGenComplete` read Failure — converging the search wrong.
///
/// Invoked from the `clearmetrics` HTTP path only (see `App::clear_metrics`).
pub fn reset_loadgen_meters() {
    for m in LOADGEN_METERS {
        // Gauge `set` is a true `swap` (renders the exact value), unlike a
        // counter's `absolute` = `fetch_max`, which cannot lower a non-zero
        // counter and was the #3630 no-op.
        m.set(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalog_arrays::*;
    use metrics_exporter_prometheus::PrometheusRecorder;
    use std::collections::HashSet;

    /// Build the union of all known metric names from the catalog.
    fn known_metric_names() -> HashSet<&'static str> {
        let mut known = HashSet::new();
        known.extend(ALL_GAUGE_NAMES.iter().copied());
        known.extend(ALL_COUNTER_NAMES.iter().copied());
        known.extend(ALL_HISTOGRAM_NAMES.iter().copied());
        known
    }

    /// Regex matching PromQL identifier tokens.
    fn metric_token_regex() -> regex::Regex {
        regex::Regex::new(r"[a-zA-Z_:][a-zA-Z0-9_:]*").unwrap()
    }

    /// Build a pristine local recorder for tests that assert absolute zero values.
    ///
    /// `with_local_recorder` is thread-local — this helper is only correct for
    /// synchronous `#[test]` functions. Do NOT use in `#[tokio::test]` or any
    /// async context where work may span threads.
    ///
    /// Uses a bare `PrometheusBuilder` (no custom histogram buckets) because
    /// these tests only inspect counter/gauge values, never histogram boundaries.
    fn fresh_local_recorder() -> (PrometheusRecorder, PrometheusHandle) {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        (recorder, handle)
    }

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
        let (recorder, handle) = fresh_local_recorder();
        metrics::with_local_recorder(&recorder, || {
            describe_metrics();
            register_label_series();
        });
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
        let (recorder, handle) = fresh_local_recorder();
        metrics::with_local_recorder(&recorder, || {
            describe_metrics();
            register_label_series();
        });
        let output = handle.render();

        for name in ALL_PREREGISTERED_COUNTER_NAMES {
            assert!(
                output.contains(&format!("{} 0", name)),
                "counter {} should be pre-registered at 0",
                name
            );
        }
    }

    /// The SCP persist-worker series (#3796) are present in the catalog: two
    /// gauges and two counters, all unlabeled and pre-registered at 0, with
    /// HELP/TYPE emitted after `describe_metrics()` + `register_label_series()`.
    #[test]
    fn test_scp_persist_metrics_in_catalog() {
        for name in [
            "henyey_scp_pending_persists",
            "henyey_scp_pending_persists_peak",
        ] {
            assert!(
                ALL_GAUGE_NAMES.contains(&name),
                "gauge {} missing from ALL_GAUGE_NAMES",
                name
            );
            assert!(
                ALL_PREREGISTERED_GAUGE_NAMES.contains(&name),
                "gauge {} should be pre-registered",
                name
            );
        }
        for name in [
            "henyey_scp_persist_attempted_total",
            "henyey_scp_persist_spawn_failed_total",
        ] {
            assert!(
                ALL_COUNTER_NAMES.contains(&name),
                "counter {} missing from ALL_COUNTER_NAMES",
                name
            );
            assert!(
                ALL_PREREGISTERED_COUNTER_NAMES.contains(&name),
                "counter {} should be pre-registered",
                name
            );
        }

        let (recorder, handle) = fresh_local_recorder();
        metrics::with_local_recorder(&recorder, || {
            describe_metrics();
            register_label_series();
        });
        let output = handle.render();
        assert!(
            output.contains("# TYPE henyey_scp_pending_persists gauge"),
            "pending gauge TYPE missing"
        );
        assert!(
            output.contains("# TYPE henyey_scp_persist_attempted_total counter"),
            "attempted counter TYPE missing"
        );
        for name in [
            "henyey_scp_pending_persists",
            "henyey_scp_pending_persists_peak",
            "henyey_scp_persist_attempted_total",
            "henyey_scp_persist_spawn_failed_total",
        ] {
            assert!(
                output.contains(&format!("{} 0", name)),
                "{} should be pre-registered at 0",
                name
            );
        }
    }

    /// The five state-archival eviction counters (#3168) are present in the
    /// catalog under the expected Prometheus names. The names mirror
    /// stellar-core's medida keys {"state-archival","eviction",*}.
    #[test]
    fn test_eviction_counters_in_catalog() {
        for name in [
            "state_archival_eviction_entries_evicted",
            "state_archival_eviction_bytes_scanned",
            "state_archival_eviction_incomplete_scan",
            "state_archival_eviction_period",
            "state_archival_eviction_age",
        ] {
            assert!(
                ALL_COUNTER_NAMES.contains(&name),
                "eviction counter {} missing from catalog",
                name
            );
            assert!(
                ALL_PREREGISTERED_COUNTER_NAMES.contains(&name),
                "eviction counter {} should be pre-registered",
                name
            );
        }
    }

    /// The five eviction counters are pre-registered at 0 and carry HELP/TYPE.
    #[test]
    fn test_eviction_counters_preregistered_at_zero() {
        let (recorder, handle) = fresh_local_recorder();
        metrics::with_local_recorder(&recorder, || {
            describe_metrics();
            register_label_series();
        });
        let output = handle.render();

        for name in [
            "state_archival_eviction_entries_evicted",
            "state_archival_eviction_bytes_scanned",
            "state_archival_eviction_incomplete_scan",
            "state_archival_eviction_period",
            "state_archival_eviction_age",
        ] {
            assert!(
                output.contains(&format!("{} 0", name)),
                "eviction counter {} should be pre-registered at 0",
                name
            );
            assert!(
                output.contains(&format!("# TYPE {} counter", name)),
                "eviction counter {} should have TYPE counter",
                name
            );
        }
    }

    /// #3270: the GetScpState re-request counter is pre-registered, carries
    /// HELP/TYPE, and increments (scrape-able from the first scrape).
    #[test]
    fn test_scp_state_requests_counter_in_catalog() {
        const NAME: &str = "henyey_scp_state_requests_sent_total";
        assert!(
            ALL_COUNTER_NAMES.contains(&NAME),
            "{} missing from catalog",
            NAME
        );
        assert!(
            ALL_PREREGISTERED_COUNTER_NAMES.contains(&NAME),
            "{} should be pre-registered",
            NAME
        );
        // The typed constant points at the expected Prometheus name.
        assert_eq!(SCP_STATE_REQUESTS_SENT_TOTAL, NAME);
    }

    /// #3270: the counter is pre-registered at 0 with TYPE counter, and after
    /// an increment renders the incremented value in the scrape.
    #[test]
    fn test_scp_state_requests_counter_increments() {
        const NAME: &str = "henyey_scp_state_requests_sent_total";
        let (recorder, handle) = fresh_local_recorder();
        metrics::with_local_recorder(&recorder, || {
            describe_metrics();
            register_label_series();

            // First scrape: pre-registered at 0 with TYPE counter.
            let before = handle.render();
            assert!(
                before.contains(&format!("{} 0", NAME)),
                "{} should be pre-registered at 0",
                NAME
            );
            assert!(
                before.contains(&format!("# TYPE {} counter", NAME)),
                "{} should have TYPE counter",
                NAME
            );

            // Increment via the typed metric API and confirm the scrape moves.
            SCP_STATE_REQUESTS_SENT_TOTAL.increment(1);
            SCP_STATE_REQUESTS_SENT_TOTAL.increment(1);
            let after = handle.render();
            assert!(
                after.contains(&format!("{} 2", NAME)),
                "{} should render 2 after two increments, got:\n{}",
                NAME,
                after
            );
        });
    }

    /// #3569: every loadgen meter is in the catalog and pre-registered at zero,
    /// with the exact bare-`loadgen_` Prometheus name (no prefix, no munging) so
    /// supercluster's `IsLoadGenComplete` reads match core's medida exporter.
    #[test]
    fn test_loadgen_meters_in_catalog() {
        // (typed constant, expected literal Prometheus name) pairs.
        let expected: &[(GaugeMetric, &str)] = &[
            (LOADGEN_RUN_START, "loadgen_run_start"),
            (LOADGEN_RUN_COMPLETE, "loadgen_run_complete"),
            (LOADGEN_RUN_FAILED, "loadgen_run_failed"),
            (LOADGEN_ACCOUNT_CREATED, "loadgen_account_created"),
            (LOADGEN_TXN_ATTEMPTED, "loadgen_txn_attempted"),
            (LOADGEN_TXN_REJECTED, "loadgen_txn_rejected"),
            (LOADGEN_TXN_BYTES, "loadgen_txn_bytes"),
            (LOADGEN_PAYMENT_SUBMITTED, "loadgen_payment_submitted"),
            (LOADGEN_PAYMENT_BYTES, "loadgen_payment_bytes"),
            (LOADGEN_SOROBAN_UPLOAD, "loadgen_soroban_upload"),
            (LOADGEN_SOROBAN_INVOKE, "loadgen_soroban_invoke"),
            (LOADGEN_SOROBAN_SETUP_INVOKE, "loadgen_soroban_setup_invoke"),
            (
                LOADGEN_SOROBAN_SETUP_UPGRADE,
                "loadgen_soroban_setup_upgrade",
            ),
            (
                LOADGEN_SOROBAN_CREATE_UPGRADE,
                "loadgen_soroban_create_upgrade",
            ),
            (LOADGEN_STEP_COUNT, "loadgen_step_count"),
        ];

        for (metric, name) in expected {
            // The literal name must carry NO domain prefix (parity: core's
            // medida keys are bare `loadgen.*`).
            assert!(
                !name.starts_with("stellar_") && !name.starts_with("henyey_"),
                "loadgen meter {} must be a bare name (no domain prefix)",
                name
            );
            // Typed constant equals the expected literal.
            assert_eq!(*metric, *name, "constant {:?} != {}", metric, name);
            // Present in the full catalog AND pre-registered at zero so the
            // series exists on the first scrape.
            assert!(
                ALL_GAUGE_NAMES.contains(name),
                "{} missing from ALL_GAUGE_NAMES (loadgen meters are resettable \
                 gauges, #3630)",
                name
            );
            assert!(
                ALL_PREREGISTERED_GAUGE_NAMES.contains(name),
                "{} should be pre-registered (present-at-zero)",
                name
            );
        }
    }

    /// #3569: the loadgen run lifecycle renders the supercluster Success
    /// condition. After a run, `loadgen_run_start == loadgen_run_complete` and
    /// `loadgen_txn_attempted` reflects the attempt count, exactly as
    /// supercluster's `IsLoadGenComplete` / `LogLoadGenProgressTowards` read.
    #[test]
    fn test_loadgen_run_lifecycle_renders() {
        const N: u64 = 7;
        let (recorder, handle) = fresh_local_recorder();
        metrics::with_local_recorder(&recorder, || {
            describe_metrics();
            register_label_series();

            // First scrape: run.start is pre-registered at 0 → supercluster
            // reads NotStarted, not a missing series.
            let before = handle.render();
            assert!(
                before.contains("loadgen_run_start 0"),
                "loadgen_run_start should be pre-registered at 0, got:\n{}",
                before
            );
            assert!(
                before.contains("# TYPE loadgen_run_start gauge"),
                "loadgen_run_start should have TYPE gauge (resettable on clearmetrics, #3630)"
            );

            // Drive a run lifecycle: start, N attempts, then complete, plus one
            // mark of each soroban.* meter and step.count.
            LOADGEN_RUN_START.increment(1.0);
            for _ in 0..N {
                LOADGEN_TXN_ATTEMPTED.increment(1.0);
            }
            LOADGEN_SOROBAN_UPLOAD.increment(1.0);
            LOADGEN_SOROBAN_INVOKE.increment(1.0);
            LOADGEN_SOROBAN_SETUP_INVOKE.increment(1.0);
            LOADGEN_SOROBAN_SETUP_UPGRADE.increment(1.0);
            LOADGEN_SOROBAN_CREATE_UPGRADE.increment(1.0);
            LOADGEN_STEP_COUNT.increment(1.0);
            LOADGEN_RUN_COMPLETE.increment(1.0);

            let after = handle.render();
            // Supercluster Success: run.start == run.complete (both 1).
            assert!(
                after.contains("loadgen_run_start 1"),
                "loadgen_run_start should be 1, got:\n{}",
                after
            );
            assert!(
                after.contains("loadgen_run_complete 1"),
                "loadgen_run_complete should be 1 (start==complete = Success), got:\n{}",
                after
            );
            assert!(
                after.contains(&format!("loadgen_txn_attempted {}", N)),
                "loadgen_txn_attempted should be {}, got:\n{}",
                N,
                after
            );
        });
    }

    /// #3630 regression: `clearmetrics` must drive the loadgen meters back to a
    /// rendered `0`, mirroring stellar-core's `MetricResetter`. This asserts the
    /// RENDERED Prometheus value (not just the setter), because the prior fix
    /// (`CounterMetric::absolute(0)` = `AtomicU64::fetch_max(0)`) was a no-op on
    /// any non-zero counter and still rendered the accumulated value.
    #[test]
    fn test_clear_loadgen_meters_resets_to_zero_rendered() {
        const N: u64 = 42;
        let (recorder, handle) = fresh_local_recorder();
        metrics::with_local_recorder(&recorder, || {
            describe_metrics();
            register_label_series();

            // Accumulate a non-zero value on every loadgen meter — the exact
            // scenario #3630 describes after a Supercluster step.
            for _ in 0..N {
                LOADGEN_TXN_ATTEMPTED.increment(1.0);
            }
            for m in LOADGEN_METERS {
                m.increment(1.0);
            }

            let before = handle.render();
            assert!(
                before.contains(&format!("loadgen_txn_attempted {}", N + 1)),
                "precondition: loadgen_txn_attempted should be {}, got:\n{}",
                N + 1,
                before
            );

            // The clearmetrics reset path.
            reset_loadgen_meters();

            // Every loadgen meter must render exactly 0 afterwards. Match the
            // full exposition line (`<name> 0`) so a leftover `<name> 43` fails.
            let after = handle.render();
            for &(prom_name, _, _) in LOADGEN_COMPAT_MAP {
                let zero_line = format!("{} 0", prom_name);
                let rendered = after
                    .lines()
                    .find(|l| l.starts_with(prom_name) && !l.starts_with('#'))
                    .unwrap_or("<missing>");
                assert_eq!(
                    rendered, zero_line,
                    "{} should render `{}` after reset_loadgen_meters(), got line `{}`\nfull:\n{}",
                    prom_name, zero_line, rendered, after
                );
            }
            // Spot-check the meter from #3630 explicitly.
            assert!(
                after.contains("loadgen_txn_attempted 0"),
                "loadgen_txn_attempted must be 0 after clearmetrics, got:\n{}",
                after
            );
        });
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
            "hard_reset_suppressed_archive_behind",
            "near_tip_gap1_suppressed",
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
    fn test_quorum_transitive_intersection_absent_until_set() {
        // Verify gauges_no_prereg semantics: the metric value is absent
        // from scrape output after describe+register, and only appears
        // after an explicit set().
        let (recorder, handle) = fresh_local_recorder();
        metrics::with_local_recorder(&recorder, || {
            describe_metrics();
            register_label_series();

            let output = handle.render();
            // The metric value should NOT appear (no pre-registered zero).
            assert!(
                !output.contains("stellar_quorum_transitive_intersection"),
                "gauges_no_prereg metric should not appear before first set()"
            );

            // Set to 1 (intersection holds).
            QUORUM_TRANSITIVE_INTERSECTION.set(1.0);
            let output = handle.render();
            assert!(
                output.contains("stellar_quorum_transitive_intersection 1"),
                "metric should be 1 after set(1.0)"
            );

            // Set to 0 (split detected).
            QUORUM_TRANSITIVE_INTERSECTION.set(0.0);
            let output = handle.render();
            assert!(
                output.contains("stellar_quorum_transitive_intersection 0"),
                "metric should be 0 after set(0.0)"
            );
        });
    }

    #[test]
    fn test_removed_overlay_metrics_absent() {
        let handle = ensure_test_recorder();
        describe_metrics();
        register_label_series();
        let output = handle.render();

        // Note: stellar_overlay_byte_read_total and stellar_overlay_byte_write_total
        // were re-added in Stage F.1 (issue #2236) with proper instrumentation in the
        // peer send/recv paths. They are no longer absent.
        let removed = [
            "stellar_overlay_message_drop_total",
            "stellar_overlay_pending_peers",
            "stellar_overlay_authenticated_peers",
            "stellar_overlay_flood_demanded_total",
            "stellar_overlay_flood_fulfilled_total",
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

        // demand-timeout now HAS a producer (app::tx_flooding::run_tx_demands)
        // and is cataloged, so it must be registered/described.
        assert!(
            output.contains("# HELP stellar_overlay_demand_timeout_total"),
            "stellar_overlay_demand_timeout_total should be registered now that it \
             has a producer"
        );
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
    fn test_stage_f1_overlay_byte_async_counters_in_catalog() {
        // Stage F.1 (issue #2236): byte / async I/O counters and connection
        // lifecycle counters must all be in the pre-registered counter catalog
        // so they appear at 0 on the very first scrape.
        let counter_names: HashSet<&str> = ALL_COUNTER_NAMES.iter().copied().collect();
        for expected in &[
            "stellar_overlay_byte_read_total",
            "stellar_overlay_byte_write_total",
            "stellar_overlay_async_read_total",
            "stellar_overlay_async_write_total",
            "stellar_overlay_inbound_attempt_total",
            "stellar_overlay_inbound_establish_total",
            "stellar_overlay_inbound_drop_total",
            "stellar_overlay_inbound_drop_remote_total",
            "stellar_overlay_inbound_drop_local_total",
            "stellar_overlay_inbound_reject_total",
            "stellar_overlay_outbound_attempt_total",
            "stellar_overlay_outbound_establish_total",
            "stellar_overlay_outbound_drop_total",
            "stellar_overlay_outbound_reject_total",
        ] {
            assert!(
                counter_names.contains(expected),
                "Stage F.1 counter {} missing from catalog",
                expected
            );
        }
    }

    #[test]
    fn test_stage_f1_overlay_counters_preregistered_at_zero() {
        // Stage F.1 counters must be visible on the very first scrape with a
        // 0 value so dashboard panels render even before any peer traffic.
        let (recorder, handle) = fresh_local_recorder();
        metrics::with_local_recorder(&recorder, || {
            describe_metrics();
            register_label_series();
        });
        let output = handle.render();

        for name in &[
            "stellar_overlay_byte_read_total",
            "stellar_overlay_byte_write_total",
            "stellar_overlay_async_read_total",
            "stellar_overlay_async_write_total",
            "stellar_overlay_inbound_attempt_total",
            "stellar_overlay_inbound_establish_total",
            "stellar_overlay_inbound_drop_total",
            "stellar_overlay_inbound_drop_remote_total",
            "stellar_overlay_inbound_drop_local_total",
            "stellar_overlay_inbound_reject_total",
            "stellar_overlay_outbound_attempt_total",
            "stellar_overlay_outbound_establish_total",
            "stellar_overlay_outbound_drop_total",
            "stellar_overlay_outbound_reject_total",
        ] {
            assert!(
                output.contains(&format!("{} 0", name)),
                "Stage F.1 counter {} should be pre-registered at 0",
                name
            );
        }
    }

    #[test]
    fn test_stage_f2_overlay_flood_fetch_counters_in_catalog() {
        // Stage F.2 (issue #2244): flood / fetch / item-fetcher counters
        // must all be in the pre-registered counter catalog.
        let counter_names: HashSet<&str> = ALL_COUNTER_NAMES.iter().copied().collect();
        let gauge_names: HashSet<&str> = ALL_GAUGE_NAMES.iter().copied().collect();
        for expected in &[
            "stellar_overlay_flood_broadcast_total",
            "stellar_overlay_flood_duplicate_recv_total",
            "stellar_overlay_flood_unique_recv_total",
            "stellar_overlay_fetch_duplicate_recv_total",
            "stellar_overlay_fetch_unique_recv_total",
            "stellar_overlay_item_fetcher_next_peer_total",
        ] {
            assert!(
                counter_names.contains(expected),
                "Stage F.2 counter {} missing from catalog",
                expected
            );
        }
        assert!(
            gauge_names.contains("stellar_overlay_memory_flood_known"),
            "Stage F.2 gauge stellar_overlay_memory_flood_known missing from catalog"
        );
    }

    #[test]
    fn test_stage_f2_overlay_counters_preregistered_at_zero() {
        // Stage F.2 counters must be visible on the very first scrape with a
        // 0 value so dashboard panels render even before any peer traffic.
        let (recorder, handle) = fresh_local_recorder();
        metrics::with_local_recorder(&recorder, || {
            describe_metrics();
            register_label_series();
        });
        let output = handle.render();

        for name in &[
            "stellar_overlay_flood_broadcast_total",
            "stellar_overlay_flood_duplicate_recv_total",
            "stellar_overlay_flood_unique_recv_total",
            "stellar_overlay_fetch_duplicate_recv_total",
            "stellar_overlay_fetch_unique_recv_total",
            "stellar_overlay_item_fetcher_next_peer_total",
        ] {
            assert!(
                output.contains(&format!("{} 0", name)),
                "Stage F.2 counter {} should be pre-registered at 0",
                name
            );
        }
        assert!(
            output.contains("stellar_overlay_memory_flood_known"),
            "Stage F.2 gauge stellar_overlay_memory_flood_known should be pre-registered"
        );
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

    /// Issue #3226: the startup peak-anon-RSS gauge is in the gauge catalog and
    /// pre-registered at 0 (so dashboards render it before the sampler stops).
    /// Mirrors the #3168 eviction-catalog test pattern.
    #[test]
    fn test_startup_peak_gauge_in_catalog() {
        let gauge_names: HashSet<&str> = ALL_GAUGE_NAMES.iter().copied().collect();
        assert!(
            gauge_names.contains("henyey_startup_peak_anon_rss_mb"),
            "henyey_startup_peak_anon_rss_mb missing from gauge catalog"
        );
        let prereg: HashSet<&str> = ALL_PREREGISTERED_GAUGE_NAMES.iter().copied().collect();
        assert!(
            prereg.contains("henyey_startup_peak_anon_rss_mb"),
            "henyey_startup_peak_anon_rss_mb must be pre-registered at 0"
        );
    }

    #[test]
    fn test_startup_peak_gauge_preregistered_at_zero() {
        let handle = ensure_test_recorder();
        describe_metrics();
        register_label_series();
        let output = handle.render();
        assert!(
            output.contains("henyey_startup_peak_anon_rss_mb"),
            "henyey_startup_peak_anon_rss_mb not found in rendered metrics"
        );
    }

    /// Issue #3759: the three live process-RSS gauges are in the gauge catalog,
    /// pre-registered at 0, and rendered by the recorder. These give the metrics
    /// archive a retroactive view of the 60 s / 2.76 GB RSS sawtooth that the
    /// allocator-view (`henyey_jemalloc_resident_bytes`) and one-shot startup
    /// peak gauges cannot capture. Mirrors the #3226 startup-peak-gauge pattern.
    #[test]
    fn test_process_rss_gauges_in_catalog() {
        let gauge_names: HashSet<&str> = ALL_GAUGE_NAMES.iter().copied().collect();
        let prereg: HashSet<&str> = ALL_PREREGISTERED_GAUGE_NAMES.iter().copied().collect();
        for name in [
            "henyey_process_resident_memory_bytes",
            "henyey_process_anon_resident_memory_bytes",
            "henyey_process_file_resident_memory_bytes",
        ] {
            assert!(
                gauge_names.contains(name),
                "{name} missing from gauge catalog"
            );
            assert!(
                prereg.contains(name),
                "{name} must be pre-registered at 0"
            );
        }
    }

    #[test]
    fn test_process_rss_gauges_rendered() {
        let handle = ensure_test_recorder();
        describe_metrics();
        register_label_series();
        let output = handle.render();
        for name in [
            "henyey_process_resident_memory_bytes",
            "henyey_process_anon_resident_memory_bytes",
            "henyey_process_file_resident_memory_bytes",
        ] {
            assert!(
                output.contains(name),
                "{name} not found in rendered metrics"
            );
        }
    }

    /// Stage E: 16 history counters and 1 histogram are present in the
    /// catalog with the exact wire names referenced from the `henyey-history`
    /// and `henyey-historywork` crates (which use string literals, not
    /// constants, to avoid a cross-crate dep on `henyey-app`).
    ///
    /// If any name here drifts away from the literal strings in those crates,
    /// dashboards silently break. Keep this list in lockstep.
    #[test]
    fn test_stage_e_history_metrics_in_catalog() {
        let counter_names: HashSet<&str> = ALL_COUNTER_NAMES.iter().copied().collect();
        let histogram_names: HashSet<&str> = ALL_HISTOGRAM_NAMES.iter().copied().collect();

        let expected_counters = [
            "stellar_history_publish_success_total",
            "stellar_history_publish_failure_total",
            "stellar_history_bucket_apply_success_total",
            "stellar_history_bucket_apply_failure_total",
            "stellar_history_apply_ledger_chain_success_total",
            "stellar_history_apply_ledger_chain_failure_total",
            "stellar_history_download_bucket_success_total",
            "stellar_history_download_bucket_failure_total",
            "stellar_history_verify_bucket_success_total",
            "stellar_history_verify_bucket_failure_total",
            "stellar_history_download_ledger_success_total",
            "stellar_history_download_ledger_failure_total",
            "stellar_history_verify_ledger_chain_success_total",
            "stellar_history_verify_ledger_chain_failure_total",
            "stellar_history_download_history_archive_state_success_total",
            "stellar_history_download_history_archive_state_failure_total",
        ];
        for name in &expected_counters {
            assert!(
                counter_names.contains(name),
                "Stage E counter `{name}` missing from catalog — dashboards and \
                 history-crate string literals must match catalog wire names",
            );
        }

        assert!(
            histogram_names.contains("stellar_history_publish_time_seconds"),
            "Stage E publish histogram missing from catalog",
        );
    }

    /// Stage E: `stellar_history_publish_*` constants exposed from the catalog
    /// match the wire names used in the `crates/history/` and
    /// `crates/historywork/` instrumentation. We re-assert here so a refactor
    /// that renames the constant but forgets to update the literal (or vice
    /// versa) fails this single test instead of silently breaking dashboards.
    #[test]
    fn test_stage_e_history_constant_string_stability() {
        assert_eq!(
            super::HISTORY_PUBLISH_SUCCESS_TOTAL,
            "stellar_history_publish_success_total"
        );
        assert_eq!(
            super::HISTORY_PUBLISH_FAILURE_TOTAL,
            "stellar_history_publish_failure_total"
        );
        assert_eq!(
            super::HISTORY_PUBLISH_TIME_SECONDS,
            "stellar_history_publish_time_seconds"
        );
        assert_eq!(
            super::HISTORY_BUCKET_APPLY_SUCCESS_TOTAL,
            "stellar_history_bucket_apply_success_total"
        );
        assert_eq!(
            super::HISTORY_BUCKET_APPLY_FAILURE_TOTAL,
            "stellar_history_bucket_apply_failure_total"
        );
        assert_eq!(
            super::HISTORY_APPLY_LEDGER_CHAIN_SUCCESS_TOTAL,
            "stellar_history_apply_ledger_chain_success_total"
        );
        assert_eq!(
            super::HISTORY_APPLY_LEDGER_CHAIN_FAILURE_TOTAL,
            "stellar_history_apply_ledger_chain_failure_total"
        );
        assert_eq!(
            super::HISTORY_DOWNLOAD_BUCKET_SUCCESS_TOTAL,
            "stellar_history_download_bucket_success_total"
        );
        assert_eq!(
            super::HISTORY_DOWNLOAD_BUCKET_FAILURE_TOTAL,
            "stellar_history_download_bucket_failure_total"
        );
        assert_eq!(
            super::HISTORY_VERIFY_BUCKET_SUCCESS_TOTAL,
            "stellar_history_verify_bucket_success_total"
        );
        assert_eq!(
            super::HISTORY_VERIFY_BUCKET_FAILURE_TOTAL,
            "stellar_history_verify_bucket_failure_total"
        );
        assert_eq!(
            super::HISTORY_DOWNLOAD_LEDGER_SUCCESS_TOTAL,
            "stellar_history_download_ledger_success_total"
        );
        assert_eq!(
            super::HISTORY_DOWNLOAD_LEDGER_FAILURE_TOTAL,
            "stellar_history_download_ledger_failure_total"
        );
        assert_eq!(
            super::HISTORY_VERIFY_LEDGER_CHAIN_SUCCESS_TOTAL,
            "stellar_history_verify_ledger_chain_success_total"
        );
        assert_eq!(
            super::HISTORY_VERIFY_LEDGER_CHAIN_FAILURE_TOTAL,
            "stellar_history_verify_ledger_chain_failure_total"
        );
        assert_eq!(
            super::HISTORY_DOWNLOAD_HAS_SUCCESS_TOTAL,
            "stellar_history_download_history_archive_state_success_total"
        );
        assert_eq!(
            super::HISTORY_DOWNLOAD_HAS_FAILURE_TOTAL,
            "stellar_history_download_history_archive_state_failure_total"
        );
    }

    /// Stage E: histogram custom bucket schedule (1s … 5min) is in effect for
    /// `stellar_history_publish_time_seconds`. The global test recorder uses
    /// the default schedule, so we install a *local* recorder configured with
    /// the same `set_buckets_for_metric` call as `install_recorder()`,
    /// record one sample, and assert the rendered output contains the
    /// expected `le=` boundaries. This catches regressions where
    /// `set_buckets_for_metric(HISTORY_PUBLISH_TIME_SECONDS, ...)` is removed
    /// or its boundary list is changed.
    #[test]
    fn test_stage_e_publish_histogram_custom_buckets_rendered() {
        // The exact bucket schedule used in `install_recorder()` for
        // `stellar_history_publish_time_seconds`.
        const EXPECTED: &[f64] = &[1.0, 5.0, 10.0, 20.0, 30.0, 45.0, 60.0, 90.0, 120.0, 300.0];

        // First: invariants on the constant itself — strictly increasing,
        // bracketing the publish norm (30–50 s), reaching 5 min.
        for w in EXPECTED.windows(2) {
            assert!(
                w[0] < w[1],
                "publish histogram buckets must be strictly increasing"
            );
        }
        assert!(
            EXPECTED.first().copied().unwrap_or(0.0) <= 1.0,
            "publish histogram should resolve sub-5s publishes"
        );
        assert!(
            EXPECTED.last().copied().unwrap_or(0.0) >= 120.0,
            "publish histogram must reach beyond typical 30–50s norm"
        );

        // Second: build a local recorder configured the same way
        // `install_recorder()` configures the global one, record one sample,
        // and verify the rendered output advertises every `le=` boundary.
        // This guarantees the production code actually installs the schedule.
        let recorder = PrometheusBuilder::new()
            .set_buckets(&[0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 30.0])
            .expect("valid default histogram buckets")
            .set_buckets_for_metric(
                Matcher::Full(HISTORY_PUBLISH_TIME_SECONDS.to_string()),
                EXPECTED,
            )
            .expect("valid history publish histogram buckets")
            .build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            HISTORY_PUBLISH_TIME_SECONDS.record(42.0);
        });

        let output = handle.render();
        for boundary in EXPECTED {
            // `le="1"` (integer-valued) and `le="5"` etc. are rendered without
            // a trailing `.0`, while non-integers would render as e.g.
            // `le="0.5"`. All EXPECTED values are integer-valued, so render
            // them with `{:.0}` and assert exact substring presence.
            let needle = format!(
                "{}_bucket{{le=\"{:.0}\"}}",
                HISTORY_PUBLISH_TIME_SECONDS, boundary
            );
            assert!(
                output.contains(&needle),
                "rendered output missing expected bucket boundary `{}`. Output:\n{}",
                needle,
                output
            );
        }
    }

    /// Regression test for #2626: verify close-cadence histograms render with
    /// custom `CLOSE_CADENCE_BUCKETS` boundaries, not the defaults. Covers all
    /// six close-cadence metrics.
    #[test]
    fn test_close_cadence_histogram_custom_buckets_rendered() {
        const EXPECTED: &[f64] = &[
            0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 3.0, 4.0, 4.5, 5.0, 5.5, 6.0, 7.0, 8.0, 10.0, 15.0,
            20.0, 30.0, 60.0,
        ];

        // Invariant: strictly increasing.
        for w in EXPECTED.windows(2) {
            assert!(
                w[0] < w[1],
                "close-cadence buckets must be strictly increasing"
            );
        }
        // Brackets the 5 s slot interval.
        assert!(EXPECTED.first().copied().unwrap_or(f64::MAX) <= 0.5);
        assert!(EXPECTED.last().copied().unwrap_or(0.0) >= 30.0);

        let all_close_cadence_metrics: &[HistogramMetric] = &[
            CLOSE_CYCLE_SECONDS,
            SLOT_TO_CLOSE_LATENCY_SECONDS,
            CLOSE_HANDLE_COMPLETE_SECONDS,
            CLOSE_POST_COMPLETE_SECONDS,
            CLOSE_DISPATCH_TO_JOIN_SECONDS,
            PERSIST_DISPATCH_TO_JOIN_SECONDS,
        ];

        // Build a local recorder with the same bucket config as production.
        let recorder = configure_histogram_buckets(PrometheusBuilder::new()).build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            for metric in all_close_cadence_metrics {
                metric.record(5.1);
            }
        });

        let output = handle.render();

        /// Format a bucket boundary the way Prometheus renders it:
        /// integers as `"5"`, non-integers as `"4.5"`.
        fn format_le(v: f64) -> String {
            if v.fract() == 0.0 {
                format!("{:.0}", v)
            } else {
                format!("{}", v)
            }
        }

        for metric in all_close_cadence_metrics {
            let name = metric.name();
            // Positive: every custom boundary is present.
            for &boundary in EXPECTED {
                let needle = format!("{}_bucket{{le=\"{}\"}}", name, format_le(boundary));
                assert!(
                    output.contains(&needle),
                    "missing custom bucket boundary `{}` for metric `{}`.\nOutput:\n{}",
                    needle,
                    name,
                    output,
                );
            }
            // Negative: default-only boundaries must be absent.
            for absent in &["0.001", "0.005"] {
                let absent_needle = format!("{}_bucket{{le=\"{}\"}}", name, absent);
                assert!(
                    !output.contains(&absent_needle),
                    "default-only boundary `{}` should NOT appear for metric `{}`",
                    absent_needle,
                    name,
                );
            }
        }
    }

    /// Regression test for #2350: metrics declared in the wrong catalog block
    /// (gauges vs counters) produce duplicate `# TYPE` lines in Prometheus
    /// exposition. Verify that the three formerly-misclassified metrics each
    /// have exactly one `# TYPE` line and it says `counter`.
    #[test]
    fn test_no_duplicate_type_lines() {
        let (recorder, handle) = fresh_local_recorder();
        metrics::with_local_recorder(&recorder, || {
            describe_metrics();
            register_label_series();
            // Simulate runtime emission (as refresh_gauges does).
            SCP_SCHEDULED_DEDUP_TOTAL.absolute(42);
            HERDER_ARB_TX_SEEN.absolute(100);
            HERDER_ARB_TX_DROPPED.absolute(50);
        });
        let output = handle.render();

        for name in &[
            SCP_SCHEDULED_DEDUP_TOTAL,
            HERDER_ARB_TX_SEEN,
            HERDER_ARB_TX_DROPPED,
        ] {
            let type_counter = format!("# TYPE {} counter", name);
            let type_gauge = format!("# TYPE {} gauge", name);

            let counter_count = output.matches(&type_counter).count();
            let gauge_count = output.matches(&type_gauge).count();

            assert_eq!(
                counter_count, 1,
                "expected exactly one `# TYPE {name} counter` line, found {counter_count}.\n\
                 Output:\n{output}"
            );
            assert_eq!(
                gauge_count, 0,
                "expected zero `# TYPE {name} gauge` lines, found {gauge_count}.\n\
                 Output:\n{output}"
            );
        }
    }

    // NOTE: The former `test_refresh_gauges_type_consistency` test has been
    // removed. It manually verified that counter constants were only used with
    // counter!() and gauge constants only with gauge!(). That invariant is now
    // enforced at compile time by the typed wrapper structs (GaugeMetric,
    // CounterMetric, HistogramMetric) — a type mismatch is a compile error.

    // ── Alert rules YAML validation ───────────────────────────────────────

    mod alert_rules_validation {
        use super::*;
        use std::path::PathBuf;

        /// Load and parse the alert rules YAML file.
        fn load_alert_yaml() -> serde_yaml::Value {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../metrics/alerts/henyey-slo-alerts.yaml");
            let contents = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
            serde_yaml::from_str(&contents)
                .unwrap_or_else(|e| panic!("failed to parse YAML {}: {e}", path.display()))
        }

        /// Collect all rule objects from groups[*].rules[*].
        fn collect_rules(yaml: &serde_yaml::Value) -> Vec<&serde_yaml::Value> {
            let groups = yaml["groups"]
                .as_sequence()
                .expect("YAML must have a 'groups' array");
            let mut rules = Vec::new();
            for group in groups {
                if let Some(group_rules) = group["rules"].as_sequence() {
                    rules.extend(group_rules.iter());
                }
            }
            assert!(!rules.is_empty(), "no rules found in alert YAML");
            rules
        }

        #[test]
        fn test_alert_yaml_parses() {
            let yaml = load_alert_yaml();
            assert!(
                yaml["groups"].as_sequence().is_some(),
                "parsed YAML must contain a 'groups' array"
            );
        }

        #[test]
        fn test_alert_uids_unique() {
            let yaml = load_alert_yaml();
            let rules = collect_rules(&yaml);
            let mut seen = HashSet::new();
            let mut duplicates = Vec::new();
            for rule in &rules {
                let uid = rule["uid"].as_str().unwrap_or_else(|| {
                    panic!("rule missing 'uid' or uid is not a string: {rule:?}")
                });
                if !seen.insert(uid) {
                    duplicates.push(uid.to_string());
                }
            }
            assert!(
                duplicates.is_empty(),
                "duplicate alert UIDs found: {duplicates:?}"
            );
        }

        #[test]
        fn test_alert_metrics_exist() {
            let yaml = load_alert_yaml();
            let rules = collect_rules(&yaml);

            let known = known_metric_names();
            let ident_re = metric_token_regex();

            let mut unknown = Vec::new();
            for rule in &rules {
                let uid = rule["uid"].as_str().unwrap_or("<no-uid>");
                let data = match rule["data"].as_sequence() {
                    Some(d) => d,
                    None => continue,
                };
                for entry in data {
                    // Skip Grafana expression nodes (threshold, reduce, etc.).
                    if entry["datasourceUid"].as_str() == Some("__expr__") {
                        continue;
                    }
                    let expr = match entry["model"]["expr"].as_str() {
                        Some(e) => e,
                        None => {
                            panic!(
                                "rule '{uid}': non-__expr__ data entry missing model.expr string"
                            );
                        }
                    };
                    for cap in ident_re.find_iter(expr) {
                        let token = cap.as_str();
                        if (token.starts_with("stellar_") || token.starts_with("henyey_"))
                            && !known.contains(token)
                        {
                            unknown.push(format!("{uid}: {token}"));
                        }
                    }
                }
            }
            assert!(
                unknown.is_empty(),
                "alert rules reference unknown metrics:\n  {}",
                unknown.join("\n  ")
            );
        }

        #[test]
        fn test_alert_rule_count_matches_readme() {
            let yaml = load_alert_yaml();
            let rules = collect_rules(&yaml);
            let yaml_count = rules.len();

            let readme_path =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../metrics/alerts/README.md");
            let readme = std::fs::read_to_string(&readme_path)
                .unwrap_or_else(|e| panic!("failed to read {}: {e}", readme_path.display()));

            let marker_re = regex::Regex::new(r"rule_count:\s*(\d+)").unwrap();
            let matches: Vec<_> = marker_re.captures_iter(&readme).collect();
            assert!(
                !matches.is_empty(),
                "no <!-- rule_count: N --> marker found in README"
            );
            assert_eq!(
                matches.len(),
                1,
                "expected exactly one rule_count marker in README, found {}",
                matches.len()
            );
            let readme_count: usize = matches[0][1].parse().unwrap();
            assert_eq!(
                yaml_count, readme_count,
                "YAML has {yaml_count} rules but README marker says {readme_count}"
            );
        }
    }

    // ── Dashboard JSON validation ─────────────────────────────────────────

    mod dashboard_json_validation {
        use super::*;
        use std::path::PathBuf;

        /// stellar-core metrics referenced in the vs-core comparison dashboard.
        /// These are emitted by stellar-core's Prometheus exporter, not by henyey.
        const VS_CORE_EXTERNAL_METRICS: &[&str] = &[
            "stellar_core_bucket_snap_merge_seconds",
            "stellar_core_ledger_age",
            "stellar_core_ledger_ledger_close_seconds",
            "stellar_core_ledger_operation_count",
            "stellar_core_ledger_transaction_apply_seconds",
            "stellar_core_ledger_transaction_count",
            "stellar_core_overlay_connection_authenticated",
            "stellar_core_overlay_inbound_live",
            "stellar_core_quorum_agree",
            "stellar_core_quorum_disagree",
            "stellar_core_quorum_fail_at",
            "stellar_core_quorum_missing",
            "stellar_core_scp_envelope_receive",
            "stellar_core_scp_envelope_sign",
            "stellar_core_scp_timing_externalized_seconds",
            "stellar_core_scp_timing_first_to_self_externalize_lag_seconds",
            "stellar_core_scp_timing_nominated_seconds",
            "stellar_core_started_on",
            "stellar_core_synced",
        ];

        /// All dashboard JSON files to validate.
        const DASHBOARD_FILES: &[&str] = &[
            "henyey-dashboard.json",
            "henyey-vs-core-dashboard.json",
            "henyey-monitoring-full.json",
            "henyey-monitoring.json",
        ];

        fn load_dashboard_json(filename: &str) -> serde_json::Value {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../metrics")
                .join(filename);
            let contents = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
            serde_json::from_str(&contents)
                .unwrap_or_else(|e| panic!("failed to parse JSON {}: {e}", path.display()))
        }

        /// Strip histogram sub-series suffixes to get the base name stored in
        /// the catalog. Does NOT strip `_total` — counters are cataloged with it.
        fn normalize_histogram_suffix(name: &str) -> &str {
            for suffix in &["_bucket", "_sum", "_count"] {
                if let Some(base) = name.strip_suffix(suffix) {
                    return base;
                }
            }
            name
        }

        /// Recursively collect `(panel_title, expr)` from nested panels.
        fn collect_panel_exprs(panels: &serde_json::Value) -> Vec<(String, String)> {
            let mut result = Vec::new();
            let panels = match panels.as_array() {
                Some(p) => p,
                None => return result,
            };
            for panel in panels {
                let title = panel["title"].as_str().unwrap_or("<untitled>").to_string();

                // Collect exprs from targets.
                if let Some(targets) = panel["targets"].as_array() {
                    for target in targets {
                        if let Some(expr) = target["expr"].as_str() {
                            if !expr.is_empty() {
                                result.push((title.clone(), expr.to_string()));
                            }
                        }
                    }
                }

                // Recurse into nested panels (row panels, collapsed panels).
                if panel["panels"].is_array() {
                    result.extend(collect_panel_exprs(&panel["panels"]));
                }
            }
            result
        }

        /// Collect `(var_name, query_string)` from dashboard template variables.
        fn collect_templating_queries(dashboard: &serde_json::Value) -> Vec<(String, String)> {
            let mut result = Vec::new();
            let list = match dashboard["templating"]["list"].as_array() {
                Some(l) => l,
                None => return result,
            };
            for item in list {
                let name = item["name"].as_str().unwrap_or("<unnamed>").to_string();
                let query = &item["query"];
                let query_str = if let Some(s) = query.as_str() {
                    s.to_string()
                } else if let Some(s) = query["query"].as_str() {
                    s.to_string()
                } else {
                    continue;
                };
                if !query_str.is_empty() {
                    result.push((name, query_str));
                }
            }
            result
        }

        /// Check whether a token at the given position in `expr` is a Grafana
        /// variable reference (preceded by `$`).
        fn is_grafana_variable(expr: &str, match_start: usize) -> bool {
            match_start > 0 && expr.as_bytes()[match_start - 1] == b'$'
        }

        /// Validate metric references in a single expression. Unknown metrics
        /// are appended to `unknown` as `"<context>: <metric_name>"`.
        fn validate_expr_tokens(
            expr: &str,
            context: &str,
            ident_re: &regex::Regex,
            known: &HashSet<&str>,
            external: &HashSet<&str>,
            unknown: &mut Vec<String>,
        ) {
            for cap in ident_re.find_iter(expr) {
                let token = cap.as_str();
                if !(token.starts_with("stellar_") || token.starts_with("henyey_")) {
                    continue;
                }
                if is_grafana_variable(expr, cap.start()) {
                    continue;
                }
                let normalized = normalize_histogram_suffix(token);
                if known.contains(token)
                    || known.contains(normalized)
                    || external.contains(token)
                    || external.contains(normalized)
                {
                    continue;
                }
                unknown.push(format!("{context}: {token}"));
            }
        }

        #[test]
        fn test_dashboard_json_parses() {
            for filename in DASHBOARD_FILES {
                let dashboard = load_dashboard_json(filename);
                assert!(
                    dashboard["panels"].is_array(),
                    "{filename}: parsed JSON must contain a 'panels' array"
                );
            }
        }

        #[test]
        fn test_dashboard_metrics_exist() {
            let known = known_metric_names();
            let ident_re = metric_token_regex();

            let vs_core_external: HashSet<&str> =
                VS_CORE_EXTERNAL_METRICS.iter().copied().collect();
            let empty_external: HashSet<&str> = HashSet::new();

            let mut unknown = Vec::new();

            for filename in DASHBOARD_FILES {
                let dashboard = load_dashboard_json(filename);

                let external = if *filename == "henyey-vs-core-dashboard.json" {
                    &vs_core_external
                } else {
                    &empty_external
                };

                // Validate panel target expressions.
                for (title, expr) in collect_panel_exprs(&dashboard["panels"]) {
                    let ctx = format!("{filename} panel '{title}'");
                    validate_expr_tokens(&expr, &ctx, &ident_re, &known, external, &mut unknown);
                }

                // Validate template variable queries.
                for (var_name, query) in collect_templating_queries(&dashboard) {
                    let ctx = format!("{filename} template var '{var_name}'");
                    validate_expr_tokens(&query, &ctx, &ident_re, &known, external, &mut unknown);
                }
            }

            assert!(
                unknown.is_empty(),
                "dashboard JSON files reference unknown metrics:\n  {}",
                unknown.join("\n  ")
            );
        }
    }

    /// Verify soroban state Arc-count histograms render with custom [1, 2, 3, 5, 10] buckets.
    #[test]
    fn test_soroban_state_arc_count_histograms_custom_buckets_rendered() {
        let recorder = configure_histogram_buckets(PrometheusBuilder::new()).build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            CLOSE_SOROBAN_STATE_DATA_ARC_COUNT.record(2.0);
            CLOSE_SOROBAN_STATE_CODE_ARC_COUNT.record(1.0);
        });

        let output = handle.render();

        for metric in &[
            CLOSE_SOROBAN_STATE_DATA_ARC_COUNT,
            CLOSE_SOROBAN_STATE_CODE_ARC_COUNT,
        ] {
            let name = metric.name();
            for boundary in &["1", "2", "3", "5", "10"] {
                let needle = format!("{}_bucket{{le=\"{}\"}}", name, boundary);
                assert!(
                    output.contains(&needle),
                    "missing custom bucket boundary `{}` for metric `{}`.\nOutput:\n{}",
                    needle,
                    name,
                    output,
                );
            }
            // Default-only boundaries must be absent.
            for absent in &["0.001", "0.005", "0.01", "0.05", "0.1", "0.5", "30"] {
                let absent_needle = format!("{}_bucket{{le=\"{}\"}}", name, absent);
                assert!(
                    !output.contains(&absent_needle),
                    "default-only boundary `{}` should NOT appear for metric `{}`",
                    absent_needle,
                    name,
                );
            }
        }
    }
}
