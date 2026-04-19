//! Handler for /metrics endpoint (Prometheus format).

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse};

use crate::app::AppInfo;

use super::super::types::MetricsResponse;
use super::super::ServerState;

pub(crate) async fn metrics_handler(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    let app_state = state.app.state().await;
    let uptime = state.start_time.elapsed().as_secs();
    let ledger_seq = state.app.ledger_info().ledger_seq;
    let peer_count = state.app.peer_snapshots().await.len();
    let pending_transactions = state.app.pending_transaction_count() as u64;
    let app_info = state.app.info();

    let metrics = MetricsResponse {
        ledger_seq,
        peer_count,
        pending_transactions,
        uptime_seconds: uptime,
        state: format!("{}", app_state),
        is_validator: app_info.is_validator,
    };

    let prometheus_text = render_prometheus_text(&metrics, &app_info);

    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4")],
        prometheus_text,
    )
}

/// Render the Prometheus text-format body for `/metrics`.
///
/// Extracted as a pure function so the format can be unit-tested without
/// bringing up a full `App`/`ServerState` stack.
pub(crate) fn render_prometheus_text(metrics: &MetricsResponse, app_info: &AppInfo) -> String {
    let mut prometheus_text = format!(
        "# HELP stellar_ledger_sequence Current ledger sequence number\n\
         # TYPE stellar_ledger_sequence gauge\n\
         stellar_ledger_sequence {}\n\
         # HELP stellar_peer_count Number of connected peers\n\
         # TYPE stellar_peer_count gauge\n\
         stellar_peer_count {}\n\
         # HELP stellar_pending_transactions Number of pending transactions\n\
         # TYPE stellar_pending_transactions gauge\n\
         stellar_pending_transactions {}\n\
         # HELP stellar_uptime_seconds Node uptime in seconds\n\
         # TYPE stellar_uptime_seconds counter\n\
         stellar_uptime_seconds {}\n\
         # HELP stellar_is_validator Whether this node is a validator\n\
         # TYPE stellar_is_validator gauge\n\
         stellar_is_validator {}\n",
        metrics.ledger_seq,
        metrics.peer_count,
        metrics.pending_transactions,
        metrics.uptime_seconds,
        if metrics.is_validator { 1 } else { 0 }
    );

    // Add meta stream metrics if active
    if app_info.meta_stream_bytes_total > 0 || app_info.meta_stream_writes_total > 0 {
        prometheus_text.push_str(&format!(
            "# HELP stellar_meta_stream_bytes_total Total bytes written to metadata output stream\n\
             # TYPE stellar_meta_stream_bytes_total counter\n\
             stellar_meta_stream_bytes_total {}\n\
             # HELP stellar_meta_stream_writes_total Total frames written to metadata output stream\n\
             # TYPE stellar_meta_stream_writes_total counter\n\
             stellar_meta_stream_writes_total {}\n",
            app_info.meta_stream_bytes_total, app_info.meta_stream_writes_total
        ));
    }

    // SCP signature-verify pipeline metrics (issue #1734 Phase B).
    let sv = &app_info.scp_verify;
    prometheus_text.push_str(&format!(
        "# HELP henyey_scp_prefilter_rejects_total SCP envelopes rejected by the event-loop pre-filter, by reason\n\
         # TYPE henyey_scp_prefilter_rejects_total counter\n\
         henyey_scp_prefilter_rejects_total{{reason=\"cannot_receive\"}} {}\n\
         henyey_scp_prefilter_rejects_total{{reason=\"close_time\"}} {}\n\
         henyey_scp_prefilter_rejects_total{{reason=\"range\"}} {}\n\
         # HELP henyey_scp_post_verify_drops_total Envelopes dropped after verification (aggregate)\n\
         # TYPE henyey_scp_post_verify_drops_total counter\n\
         henyey_scp_post_verify_drops_total {}\n",
        sv.prefilter_reject_cannot_receive,
        sv.prefilter_reject_close_time,
        sv.prefilter_reject_range,
        sv.post_verify_drops,
    ));
    // Per-reason post-verify counters — driven from PostVerifyReason::ALL (issue #1792).
    {
        use std::fmt::Write;
        prometheus_text.push_str(
            "# HELP henyey_scp_post_verify_total Envelopes processed by post-verify, by reason\n\
             # TYPE henyey_scp_post_verify_total counter\n",
        );
        for (reason, count) in sv.pv_counters.iter() {
            write!(
                prometheus_text,
                "henyey_scp_post_verify_total{{reason=\"{}\"}} {}\n",
                reason.label(),
                count,
            )
            .unwrap();
        }
    }
    prometheus_text.push_str(&format!(
        "# HELP henyey_scp_verify_input_backlog Current depth of the SCP signature-verify input channel (event-loop sampled)\n\
         # TYPE henyey_scp_verify_input_backlog gauge\n\
         henyey_scp_verify_input_backlog {}\n\
         # HELP henyey_scp_verify_output_backlog Current depth of the verified-envelope output channel (envelopes awaiting the event loop)\n\
         # TYPE henyey_scp_verify_output_backlog gauge\n\
         henyey_scp_verify_output_backlog {}\n\
         # HELP henyey_scp_verifier_thread_state Worker thread state (0=Running, 1=Stopping, 2=Dead)\n\
         # TYPE henyey_scp_verifier_thread_state gauge\n\
         henyey_scp_verifier_thread_state {}\n\
         # HELP henyey_scp_verify_latency_us Enqueue-to-post-verify latency (sum + count; average = sum/count)\n\
         # TYPE henyey_scp_verify_latency_us summary\n\
         henyey_scp_verify_latency_us_sum {}\n\
         henyey_scp_verify_latency_us_count {}\n",
        sv.verify_input_backlog,
        sv.verify_output_backlog,
        sv.verifier_thread_state,
        sv.verify_latency_us_sum,
        sv.verify_latency_count,
    ));

    // Overlay fetch-response channel depth gauges (issue #1741).
    let ofc = &app_info.overlay_fetch_channel;
    prometheus_text.push_str(&format!(
        "# HELP henyey_overlay_fetch_channel_depth Current depth of the overlay fetch-response channel (event-loop sampled)\n\
         # TYPE henyey_overlay_fetch_channel_depth gauge\n\
         henyey_overlay_fetch_channel_depth {}\n\
         # HELP henyey_overlay_fetch_channel_depth_max Monotonic maximum depth of the overlay fetch-response channel since process start\n\
         # TYPE henyey_overlay_fetch_channel_depth_max gauge\n\
         henyey_overlay_fetch_channel_depth_max {}\n",
        ofc.depth, ofc.depth_max,
    ));

    prometheus_text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AppInfo, OverlayFetchChannelMetrics, ScpVerifyMetrics};

    fn dummy_app_info() -> AppInfo {
        AppInfo {
            version: String::new(),
            commit_hash: String::new(),
            build_timestamp: String::new(),
            node_name: String::new(),
            public_key: String::new(),
            network_passphrase: String::new(),
            is_validator: false,
            database_path: std::path::PathBuf::new(),
            meta_stream_bytes_total: 0,
            meta_stream_writes_total: 0,
            scp_verify: ScpVerifyMetrics::default(),
            overlay_fetch_channel: OverlayFetchChannelMetrics::default(),
        }
    }

    fn dummy_metrics() -> MetricsResponse {
        MetricsResponse {
            ledger_seq: 0,
            peer_count: 0,
            pending_transactions: 0,
            uptime_seconds: 0,
            state: String::new(),
            is_validator: false,
        }
    }

    /// Issue #1741 regression: `/metrics` must expose both the current-depth
    /// and monotonic-max gauges for the overlay fetch-response channel so
    /// unbounded growth under a wedged app loop is operator-visible.
    #[test]
    fn metrics_endpoint_exposes_fetch_channel_depth() {
        let mut app_info = dummy_app_info();
        app_info.overlay_fetch_channel = OverlayFetchChannelMetrics {
            depth: 17,
            depth_max: 42,
        };
        let body = render_prometheus_text(&dummy_metrics(), &app_info);
        assert!(
            body.contains("# HELP henyey_overlay_fetch_channel_depth "),
            "missing HELP line for fetch_channel_depth"
        );
        assert!(
            body.contains("# TYPE henyey_overlay_fetch_channel_depth gauge"),
            "missing TYPE line for fetch_channel_depth"
        );
        assert!(
            body.contains("\nhenyey_overlay_fetch_channel_depth 17\n"),
            "sample value for fetch_channel_depth not emitted; got:\n{}",
            body
        );
        assert!(
            body.contains("# HELP henyey_overlay_fetch_channel_depth_max "),
            "missing HELP line for fetch_channel_depth_max"
        );
        assert!(
            body.contains("# TYPE henyey_overlay_fetch_channel_depth_max gauge"),
            "missing TYPE line for fetch_channel_depth_max"
        );
        assert!(
            body.contains("\nhenyey_overlay_fetch_channel_depth_max 42\n"),
            "sample value for fetch_channel_depth_max not emitted; got:\n{}",
            body
        );
    }

    /// Issue #1733: `/metrics` must expose per-reason post-verify counters
    /// as labeled `henyey_scp_post_verify_total{reason="..."}` lines.
    #[test]
    fn metrics_endpoint_exposes_per_reason_post_verify_counters() {
        use henyey_herder::scp_verify::PostVerifyReason;

        let mut app_info = dummy_app_info();
        app_info.scp_verify.pv_counters[PostVerifyReason::GateDriftRange] = 3;
        app_info.scp_verify.pv_counters[PostVerifyReason::Accepted] = 42;
        app_info.scp_verify.pv_counters[PostVerifyReason::PanicVerdict] = 1;
        let body = render_prometheus_text(&dummy_metrics(), &app_info);
        assert!(
            body.contains("# HELP henyey_scp_post_verify_total "),
            "missing HELP line for post_verify_total"
        );
        assert!(
            body.contains("# TYPE henyey_scp_post_verify_total counter"),
            "missing TYPE line for post_verify_total"
        );
        assert!(
            body.contains("henyey_scp_post_verify_total{reason=\"drift_range\"} 3"),
            "drift_range counter not rendered correctly; got:\n{}",
            body
        );
        assert!(
            body.contains("henyey_scp_post_verify_total{reason=\"accepted\"} 42"),
            "accepted counter not rendered correctly; got:\n{}",
            body
        );
        assert!(
            body.contains("henyey_scp_post_verify_total{reason=\"panic\"} 1"),
            "panic counter not rendered correctly; got:\n{}",
            body
        );
        // Verify all reason labels are present — driven from PostVerifyReason::ALL
        // so adding a variant automatically extends this check (issue #1792).
        for reason in PostVerifyReason::ALL {
            let label = format!(
                "henyey_scp_post_verify_total{{reason=\"{}\"}}",
                reason.label()
            );
            assert!(
                body.contains(&label),
                "missing counter for reason={}; got:\n{body}",
                reason.label()
            );
        }
    }
}
