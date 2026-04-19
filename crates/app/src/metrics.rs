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

use metrics::{counter, describe_counter, describe_gauge, gauge};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

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
    describe_counter!(UPTIME_SECONDS, "Node uptime in seconds");
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
    counter!(UPTIME_SECONDS).absolute(0);
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

    // Labeled counters — all reason labels pre-registered at zero.
    for reason in PreFilterRejectReason::ALL {
        counter!(SCP_PREFILTER_REJECTS_TOTAL, "reason" => reason.label()).absolute(0);
    }
    for reason in PostVerifyReason::ALL {
        counter!(SCP_POST_VERIFY_TOTAL, "reason" => reason.label()).absolute(0);
    }
}

// ── Scrape-time refresh ────────────────────────────────────────────────

/// Update scrape-time gauges and absolute counters from current App state.
///
/// Called by the `/metrics` handler immediately before `handle.render()`.
pub(crate) async fn refresh_gauges(state: &ServerState) {
    let _app_state = state.app.state().await;
    let uptime = state.start_time.elapsed().as_secs();
    let ledger_seq = state.app.ledger_info().ledger_seq;
    let peer_count = state.app.peer_snapshots().await.len();
    let pending_txs = state.app.pending_transaction_count() as u64;
    let app_info = state.app.info();

    // Stellar-compatible gauges.
    gauge!(LEDGER_SEQUENCE).set(ledger_seq as f64);
    gauge!(PEER_COUNT).set(peer_count as f64);
    gauge!(PENDING_TRANSACTIONS).set(pending_txs as f64);
    counter!(UPTIME_SECONDS).absolute(uptime);
    gauge!(IS_VALIDATOR).set(if app_info.is_validator { 1.0 } else { 0.0 });

    // Meta stream counters — only set when active (matching current conditional behavior).
    if app_info.meta_stream_bytes_total > 0 || app_info.meta_stream_writes_total > 0 {
        counter!(META_STREAM_BYTES_TOTAL).absolute(app_info.meta_stream_bytes_total);
        counter!(META_STREAM_WRITES_TOTAL).absolute(app_info.meta_stream_writes_total);
    }

    // SCP verify pipeline — absolute counters.
    let sv = &app_info.scp_verify;
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
    gauge!(OVERLAY_FETCH_CHANNEL_DEPTH).set(app_info.overlay_fetch_channel.depth as f64);
    gauge!(OVERLAY_FETCH_CHANNEL_DEPTH_MAX).set(app_info.overlay_fetch_channel.depth_max as f64);

    // Catchup.
    counter!(POST_CATCHUP_HARD_RESET_TOTAL).absolute(app_info.post_catchup_hard_reset_total);
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
            output.contains("# TYPE stellar_uptime_seconds counter"),
            "missing TYPE counter for uptime_seconds"
        );
        assert!(
            output.contains("# TYPE henyey_scp_prefilter_rejects_total counter"),
            "missing TYPE counter for prefilter_rejects_total"
        );
        assert!(
            output.contains("# TYPE henyey_post_catchup_hard_reset_total counter"),
            "missing TYPE counter for hard_reset_total"
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
}
