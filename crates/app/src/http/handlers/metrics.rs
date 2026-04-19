//! Handler for /metrics endpoint (Prometheus format).

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse};

use super::super::ServerState;
use crate::metrics::refresh_gauges;

pub(crate) async fn metrics_handler(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    refresh_gauges(&state).await;
    let body = match &state.prometheus_handle {
        Some(handle) => handle.render(),
        None => "# metrics recorder not installed\n".to_string(),
    };
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4")],
        body,
    )
}

#[cfg(test)]
mod tests {
    use crate::metrics;

    /// Ensure the metrics handler returns valid Prometheus text when a recorder
    /// is installed and `refresh_gauges` has been called.
    #[test]
    fn test_describe_metrics_and_label_series() {
        let handle = metrics::ensure_test_recorder();
        metrics::describe_metrics();
        metrics::register_label_series();
        let output = handle.render();

        // HELP lines for key metrics.
        assert!(
            output.contains("# HELP stellar_ledger_sequence"),
            "missing HELP for ledger_sequence"
        );
        assert!(
            output.contains("# HELP henyey_scp_prefilter_rejects_total"),
            "missing HELP for prefilter_rejects_total"
        );
        assert!(
            output.contains("# HELP henyey_post_catchup_hard_reset_total"),
            "missing HELP for hard_reset_total"
        );

        // TYPE lines must be correct.
        assert!(
            output.contains("# TYPE stellar_ledger_sequence gauge"),
            "wrong TYPE for ledger_sequence"
        );
        assert!(
            output.contains("# TYPE henyey_scp_prefilter_rejects_total counter"),
            "wrong TYPE for prefilter_rejects_total"
        );

        // All prefilter reason labels present.
        use henyey_herder::scp_verify::PreFilterRejectReason;
        for reason in PreFilterRejectReason::ALL {
            let label = format!(
                "henyey_scp_prefilter_rejects_total{{reason=\"{}\"}}",
                reason.label()
            );
            assert!(
                output.contains(&label),
                "missing counter for reason={}; got:\n{output}",
                reason.label()
            );
        }

        // All post-verify reason labels present.
        use henyey_herder::scp_verify::PostVerifyReason;
        for reason in PostVerifyReason::ALL {
            let label = format!(
                "henyey_scp_post_verify_total{{reason=\"{}\"}}",
                reason.label()
            );
            assert!(
                output.contains(&label),
                "missing counter for reason={}; got:\n{output}",
                reason.label()
            );
        }
    }
}
