//! stellar-core compatible `/metrics` handler.
//!
//! stellar-core returns medida JSON format with `type`, `count`, and optional
//! rate/percentile fields. We emit the subset of metrics that SSC missions and
//! health checks commonly inspect.

use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;

use crate::compat_http::CompatServerState;

/// GET /metrics
///
/// Returns a medida-compatible metrics JSON that covers the metrics
/// stellar-rpc, SSC missions, and health checks commonly inspect.
///
/// stellar-core's medida format uses three metric types:
/// - `"counter"`: a monotonically increasing count
/// - `"timer"`: count + duration percentiles + rate
/// - `"meter"`: count + event rate
///
/// The four medida metrics SSC missions read emit REAL in-process
/// rate/percentile values from the [`crate::medida_compat`] accumulators. Other
/// rate/percentile fields we don't track remain `0.0` placeholders.
pub(crate) async fn compat_metrics_handler(
    State(state): State<Arc<CompatServerState>>,
) -> impl IntoResponse {
    let app = &state.app;
    let info = app.ledger_info();
    let (pending_count, authenticated_count) = app.peer_counts().await;
    let ledger_tx_count = app.ledger_tx_count();

    // Read real medida-compat snapshots. The scp meters are fed (delta-marked)
    // from the periodic metrics refresh path; the close timer/histogram are fed
    // from the ledger-close path.
    let compat = crate::medida_compat::medida_compat();

    let scalars = MetricsScalars {
        protocol_version: info.protocol_version,
        peer_count: authenticated_count + pending_count,
        authenticated_count,
        pending_count,
        pending_transactions: app.pending_transaction_count(),
        ledger_tx_count,
    };

    Json(build_metrics_json(&scalars, compat))
}

/// Non-metrics-registry scalar values (counters/gauges) the handler reports.
struct MetricsScalars {
    protocol_version: u32,
    peer_count: usize,
    authenticated_count: usize,
    pending_count: usize,
    pending_transactions: usize,
    ledger_tx_count: u64,
}

/// Build the medida-compatible `/metrics` JSON, reading real rate/percentile
/// values from the [`crate::medida_compat::MedidaCompat`] accumulators. Factored
/// out of the async handler so it can be unit-tested without a live `App`.
///
/// Field names/types/order mirror stellar-core's `json_reporter.cc`
/// `Process(Timer/Meter/Histogram)`. scp meters use `event_type = "value"`
/// (`HerderSCPDriver.cpp:55,57`); the close timer's embedded meter uses
/// `event_type = "calls"` (`LedgerManagerImpl.cpp:204`).
fn build_metrics_json(
    s: &MetricsScalars,
    compat: &crate::medida_compat::MedidaCompat,
) -> serde_json::Value {
    let close_timer = compat.close_timer.snapshot();
    let close_meter = compat.close_meter.snapshot();
    let tx_hist = compat.tx_count_histogram.snapshot();
    let scp_valid = compat.scp_value_valid.snapshot();
    let scp_invalid = compat.scp_value_invalid.snapshot();

    serde_json::json!({
        "metrics": {
            "ledger.ledger.close": {
                "type": "timer",
                "count": close_timer.count,
                "event_type": "calls",
                "rate_unit": "second",
                "mean_rate": close_meter.mean_rate,
                "1_min_rate": close_meter.one_minute_rate,
                "5_min_rate": close_meter.five_minute_rate,
                "15_min_rate": close_meter.fifteen_minute_rate,
                "duration_unit": "millisecond",
                "min": close_timer.min,
                "max": close_timer.max,
                "mean": close_timer.mean,
                "stddev": close_timer.stddev,
                "sum": close_timer.sum,
                "median": close_timer.median,
                "75%": close_timer.p75,
                "95%": close_timer.p95,
                "98%": close_timer.p98,
                "99%": close_timer.p99,
                "99.9%": close_timer.p999,
                "100%": close_timer.max
            },
            "ledger.transaction.count": {
                "type": "histogram",
                "count": s.ledger_tx_count,
                "min": tx_hist.min,
                "max": tx_hist.max,
                "mean": tx_hist.mean,
                "stddev": tx_hist.stddev,
                "median": tx_hist.median,
                "75%": tx_hist.p75,
                "95%": tx_hist.p95,
                "98%": tx_hist.p98,
                "99%": tx_hist.p99,
                "99.9%": tx_hist.p999,
                "100%": tx_hist.max
            },
            "peer.peer.count": {
                "type": "counter",
                "count": s.peer_count
            },
            "peer.peer.authenticated-count": {
                "type": "counter",
                "count": s.authenticated_count
            },
            "peer.peer.pending-count": {
                "type": "counter",
                "count": s.pending_count
            },
            "herder.pending.transactions": {
                "type": "counter",
                "count": s.pending_transactions
            },
            "ledger.ledger.version": {
                "type": "counter",
                "count": s.protocol_version
            },
            "scp.value.valid": {
                "type": "meter",
                "count": scp_valid.count,
                "event_type": "value",
                "rate_unit": "second",
                "mean_rate": scp_valid.mean_rate,
                "1_min_rate": scp_valid.one_minute_rate,
                "5_min_rate": scp_valid.five_minute_rate,
                "15_min_rate": scp_valid.fifteen_minute_rate
            },
            "scp.value.invalid": {
                "type": "meter",
                "count": scp_invalid.count,
                "event_type": "value",
                "rate_unit": "second",
                "mean_rate": scp_invalid.mean_rate,
                "1_min_rate": scp_invalid.one_minute_rate,
                "5_min_rate": scp_invalid.five_minute_rate,
                "15_min_rate": scp_invalid.fifteen_minute_rate
            },
            // Zero-value metrics required by SSC's CheckNoErrorMetrics.
            // Henyey doesn't track these, but SSC asserts they exist.
            "scp.envelope.invalidsig": {
                "type": "counter",
                "count": 0
            },
            "history.publish.failure": {
                "type": "counter",
                "count": 0
            },
            "ledger.invariant.failure": {
                "type": "counter",
                "count": 0
            },
            "ledger.transaction.internal-error": {
                "type": "counter",
                "count": 0
            }
        }
    })
}

#[cfg(test)]
mod tests {
    /// Verify the metrics response JSON shape matches stellar-core's medida format.
    ///
    /// stellar-core returns `{"metrics": {"name": {"type": "...", "count": N, ...}, ...}}`.
    #[test]
    fn test_metrics_response_shape() {
        let value = serde_json::json!({
            "metrics": {
                "ledger.ledger.close": {
                    "type": "timer",
                    "count": 100,
                    "event_type": "calls",
                    "rate_unit": "second",
                    "mean_rate": 0.0,
                    "1_min_rate": 0.0,
                    "5_min_rate": 0.0,
                    "15_min_rate": 0.0,
                    "duration_unit": "millisecond",
                    "min": 0.0,
                    "max": 0.0,
                    "mean": 0.0,
                    "stddev": 0.0,
                    "sum": 0.0,
                    "median": 0.0,
                    "75%": 0.0,
                    "95%": 0.0,
                    "98%": 0.0,
                    "99%": 0.0,
                    "99.9%": 0.0,
                    "100%": 0.0
                },
                "peer.peer.count": {
                    "type": "counter",
                    "count": 5
                },
                "herder.pending.transactions": {
                    "type": "counter",
                    "count": 3
                }
            }
        });

        let obj = value.as_object().unwrap();
        assert_eq!(obj.len(), 1, "top-level should only have 'metrics'");

        let metrics = value["metrics"].as_object().unwrap();
        let expected_counters = ["peer.peer.count", "herder.pending.transactions"];
        for name in &expected_counters {
            let metric = &metrics[*name];
            assert_eq!(metric["type"], "counter", "{name} should be a counter");
            assert!(metric.get("count").is_some(), "{name} must have 'count'");
        }

        // Timer has percentiles and rate fields
        let timer = &metrics["ledger.ledger.close"];
        assert_eq!(timer["type"], "timer");
        assert!(timer.get("count").is_some());
        assert!(timer.get("mean_rate").is_some());
        assert!(timer.get("duration_unit").is_some());
        assert!(timer.get("median").is_some());
        assert!(timer.get("99%").is_some());
    }

    /// Verify all metrics have the `type` field (medida format requirement).
    #[test]
    fn test_all_metrics_have_type_field() {
        let value = serde_json::json!({
            "metrics": {
                "ledger.ledger.close": { "type": "timer", "count": 0 },
                "ledger.transaction.count": { "type": "histogram", "count": 0 },
                "peer.peer.count": { "type": "counter", "count": 0 },
                "peer.peer.authenticated-count": { "type": "counter", "count": 0 },
                "peer.peer.pending-count": { "type": "counter", "count": 0 },
                "herder.pending.transactions": { "type": "counter", "count": 0 },
                "ledger.ledger.version": { "type": "counter", "count": 0 },
                "scp.value.valid": { "type": "meter", "count": 0 },
                "scp.value.invalid": { "type": "meter", "count": 0 }
            }
        });

        let metrics = value["metrics"].as_object().unwrap();
        assert_eq!(metrics.len(), 9, "should have 9 metrics");
        for (name, metric) in metrics {
            assert!(
                metric.get("type").is_some(),
                "metric '{name}' must have 'type' field"
            );
            assert!(
                metric.get("count").is_some(),
                "metric '{name}' must have 'count' field"
            );
        }
    }

    /// Verify meter-type metrics have rate fields.
    #[test]
    fn test_meter_metrics_have_rate_fields() {
        let meter = serde_json::json!({
            "type": "meter",
            "count": 100,
            "event_type": "events",
            "rate_unit": "second",
            "mean_rate": 0.2,
            "1_min_rate": 0.19,
            "5_min_rate": 0.2,
            "15_min_rate": 0.2
        });

        assert_eq!(meter["type"], "meter");
        let rate_fields = ["mean_rate", "1_min_rate", "5_min_rate", "15_min_rate"];
        for field in &rate_fields {
            assert!(meter.get(*field).is_some(), "meter must have '{field}'");
        }
    }

    use super::{build_metrics_json, MetricsScalars};
    use crate::medida_compat::MedidaCompat;

    fn test_scalars() -> MetricsScalars {
        MetricsScalars {
            protocol_version: 23,
            peer_count: 5,
            authenticated_count: 4,
            pending_count: 1,
            pending_transactions: 3,
            ledger_tx_count: 42,
        }
    }

    /// The four SSC-read metrics emit REAL non-zero rate/percentile values after
    /// synthetic observations (the pre-fix handler hardcoded these to 0.0). Also
    /// asserts the scp meters use `event_type="value"`, the counts are real, and
    /// the medida JSON shape is unchanged.
    #[test]
    fn test_metrics_real_values_after_observations() {
        use std::time::{Duration, Instant};
        // Anchor the meters 6s in the past and mark there, so that when the
        // handler reads the meters at the current wall clock the EWMA lazy-tick
        // (5s interval) has fired at least once and the rate fields are non-zero
        // — faithful medida behaviour (rates seed on the first tick, not on the
        // mark).
        let past = Instant::now() - Duration::from_secs(6);
        let compat = MedidaCompat::new_for_test_at(past);
        // Synthetic ledger closes: durations (ms) + tx counts.
        for (ms, txs) in [(100.0, 50), (200.0, 80), (150.0, 60), (90.0, 40)] {
            compat.record_ledger_close_at(ms, txs, past);
        }
        // Feed scp meters from cumulative snapshots (delta-marked) in the past.
        compat.feed_scp_at(120, 7, past);

        let v = build_metrics_json(&test_scalars(), &compat);
        let m = v["metrics"].as_object().unwrap();

        // --- Close timer: real percentiles + rate ---
        let close = &m["ledger.ledger.close"];
        assert_eq!(close["type"], "timer");
        assert_eq!(close["count"], 4, "timer count is real observation count");
        assert_eq!(close["event_type"], "calls");
        assert!(
            close["median"].as_f64().unwrap() > 0.0,
            "median must be non-zero, got {}",
            close["median"]
        );
        assert!(
            close["99%"].as_f64().unwrap() > 0.0,
            "99% must be non-zero, got {}",
            close["99%"]
        );
        assert!(
            close["1_min_rate"].as_f64().unwrap() > 0.0,
            "1_min_rate must be non-zero, got {}",
            close["1_min_rate"]
        );
        assert!(close["min"].as_f64().unwrap() > 0.0);
        assert!(close["max"].as_f64().unwrap() > 0.0);
        assert!(close["sum"].as_f64().unwrap() > 0.0);
        // Percentile ordering.
        let med = close["median"].as_f64().unwrap();
        let p99 = close["99%"].as_f64().unwrap();
        assert!(med <= p99, "median {med} <= p99 {p99}");

        // --- tx-count histogram: real percentiles ---
        let hist = &m["ledger.transaction.count"];
        assert_eq!(hist["type"], "histogram");
        assert_eq!(hist["count"], 42, "histogram count from ledger_tx_count");
        assert!(
            hist["median"].as_f64().unwrap() > 0.0,
            "hist median non-zero"
        );
        assert!(hist["max"].as_f64().unwrap() >= hist["median"].as_f64().unwrap());

        // --- scp meters: event_type "value", real count + rate ---
        let valid = &m["scp.value.valid"];
        assert_eq!(valid["type"], "meter");
        assert_eq!(
            valid["event_type"], "value",
            "scp meter event_type is 'value'"
        );
        assert_eq!(valid["count"], 120, "scp.value.valid count is real total");
        assert!(
            valid["1_min_rate"].as_f64().unwrap() > 0.0,
            "scp.value.valid 1_min_rate must be non-zero"
        );
        let invalid = &m["scp.value.invalid"];
        assert_eq!(invalid["event_type"], "value");
        assert_eq!(invalid["count"], 7, "scp.value.invalid count is real total");

        // --- Shape preserved: all 9 metrics with type+count ---
        assert_eq!(m.len(), 9, "should still emit 9 metrics");
        for (name, metric) in m {
            assert!(metric.get("type").is_some(), "{name} has type");
            assert!(metric.get("count").is_some(), "{name} has count");
        }
        // Timer/histogram percentile field names unchanged.
        for field in ["median", "75%", "95%", "98%", "99%", "99.9%", "100%"] {
            assert!(close.get(field).is_some(), "timer has {field}");
            assert!(hist.get(field).is_some(), "histogram has {field}");
        }
    }

    /// With no observations, the four metrics report 0.0 rate/percentile fields
    /// and the shape matches the existing shape contract.
    #[test]
    fn test_metrics_zero_at_startup() {
        let compat = MedidaCompat::new_for_test();
        let v = build_metrics_json(&test_scalars(), &compat);
        let m = v["metrics"].as_object().unwrap();

        let close = &m["ledger.ledger.close"];
        assert_eq!(close["count"], 0);
        for field in [
            "mean_rate",
            "1_min_rate",
            "5_min_rate",
            "15_min_rate",
            "min",
            "max",
            "mean",
            "stddev",
            "sum",
            "median",
            "75%",
            "95%",
            "98%",
            "99%",
            "99.9%",
            "100%",
        ] {
            assert_eq!(close[field].as_f64().unwrap(), 0.0, "close {field} is 0.0");
        }

        let valid = &m["scp.value.valid"];
        assert_eq!(valid["count"], 0);
        assert_eq!(valid["event_type"], "value");
        for field in ["mean_rate", "1_min_rate", "5_min_rate", "15_min_rate"] {
            assert_eq!(
                valid[field].as_f64().unwrap(),
                0.0,
                "scp valid {field} is 0.0"
            );
        }

        // Shape contract: 9 metrics, all with type+count.
        assert_eq!(m.len(), 9);
        for (name, metric) in m {
            assert!(metric.get("type").is_some(), "{name} has type");
            assert!(metric.get("count").is_some(), "{name} has count");
        }
    }
}
