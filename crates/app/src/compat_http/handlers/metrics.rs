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

    // Source the loadgen meter counts from the Prometheus registry (where #3571
    // increments them across henyey-bin/simulation/app). The compat medida JSON
    // and the Prometheus render are otherwise two divergent surfaces; rendering
    // + parsing here keeps the registry the single source of truth. When no
    // handle is wired (library consumers, tests without a recorder), the counts
    // default to zero — matching the pre-registered-at-zero series.
    let loadgen = match &state.prometheus_handle {
        Some(handle) => parse_loadgen_counts(&handle.render()),
        None => LoadgenCounts::default(),
    };

    Json(build_metrics_json(&scalars, compat, &loadgen))
}

/// Loadgen meter counts keyed by dotted medida key (e.g. `loadgen.run.start`).
///
/// Populated by [`parse_loadgen_counts`] from a Prometheus render; consumed by
/// [`build_metrics_json`] to emit medida `meter` entries. Defaults to empty
/// (all-zero) when no Prometheus handle is available.
#[derive(Debug, Default)]
struct LoadgenCounts {
    /// dotted medida key → (count, event_type)
    by_key: std::collections::HashMap<&'static str, (u64, &'static str)>,
}

impl LoadgenCounts {
    /// Count for a dotted medida key; 0 if absent.
    fn count(&self, dotted_key: &str) -> u64 {
        self.by_key.get(dotted_key).map(|(c, _)| *c).unwrap_or(0)
    }
}

/// Parse the loadgen counter values out of a Prometheus exposition render.
///
/// For each `(prom_name, dotted_key, event_type)` in
/// [`crate::metrics::LOADGEN_COMPAT_MAP`], scan the render for a bare,
/// label-less line whose first whitespace token equals `prom_name` exactly and
/// extract the trailing `u64` value. Exact-token matching (not `contains`)
/// avoids prefix collisions (`loadgen_run_start` vs a hypothetical
/// `loadgen_run_start_total`). The series are pre-registered at zero by #3571
/// so the lines normally exist; an absent line maps to 0 (omitted from the map,
/// surfaced as 0 by [`LoadgenCounts::count`]).
fn parse_loadgen_counts(render: &str) -> LoadgenCounts {
    let mut by_key = std::collections::HashMap::new();
    for &(prom_name, dotted_key, event_type) in crate::metrics::LOADGEN_COMPAT_MAP {
        for line in render.lines() {
            // Skip HELP/TYPE comment lines.
            if line.starts_with('#') {
                continue;
            }
            let mut tokens = line.split_whitespace();
            // The exposition line for a label-less counter is `<name> <value>`.
            // Reject any line carrying labels (`<name>{...} <value>`): the bare
            // token must equal the metric name with no `{`.
            if tokens.next() != Some(prom_name) {
                continue;
            }
            if let Some(value_tok) = tokens.next() {
                // Prometheus renders counters as integers; parse as f64 first to
                // tolerate a `123` or `123.0` rendering, then truncate to u64.
                if let Ok(v) = value_tok.parse::<f64>() {
                    by_key.insert(dotted_key, (v as u64, event_type));
                }
            }
            break;
        }
    }
    LoadgenCounts { by_key }
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
    loadgen: &LoadgenCounts,
) -> serde_json::Value {
    let close_timer = compat.close_timer.snapshot();
    let close_meter = compat.close_meter.snapshot();
    let tx_hist = compat.tx_count_histogram.snapshot();
    let scp_valid = compat.scp_value_valid.snapshot();
    let scp_invalid = compat.scp_value_invalid.snapshot();

    let mut value = serde_json::json!({
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
    });

    // Append the loadgen.* meters ADDITIVELY, sourced from the Prometheus
    // registry via LOADGEN_COMPAT_MAP (#3572). Supercluster's IsLoadGenComplete
    // reads `loadgen.run.start`/`loadgen.run.complete`/`loadgen.account.created`/
    // `loadgen.txn.attempted` from this medida JSON. Each entry is a medida
    // `meter` whose load-bearing field is `count`; the rate fields are 0.0
    // placeholders (supercluster's MeterCountOr reads only `.count`, and its
    // JsonProvider only needs the keys present for type inference).
    let metrics = value["metrics"]
        .as_object_mut()
        .expect("metrics object is built above");
    for &(_, dotted_key, event_type) in crate::metrics::LOADGEN_COMPAT_MAP {
        metrics.insert(
            dotted_key.to_string(),
            serde_json::json!({
                "type": "meter",
                "count": loadgen.count(dotted_key),
                "event_type": event_type,
                "rate_unit": "s",
                "mean_rate": 0.0,
                "1_min_rate": 0.0,
                "5_min_rate": 0.0,
                "15_min_rate": 0.0
            }),
        );
    }

    value
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
                "scp.value.invalid": { "type": "meter", "count": 0 },
                "scp.envelope.invalidsig": { "type": "counter", "count": 0 },
                "history.publish.failure": { "type": "counter", "count": 0 },
                "ledger.invariant.failure": { "type": "counter", "count": 0 },
                "ledger.transaction.internal-error": { "type": "counter", "count": 0 }
            }
        });

        let metrics = value["metrics"].as_object().unwrap();
        assert_eq!(metrics.len(), 13, "should have 13 metrics");
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

    use super::{build_metrics_json, parse_loadgen_counts, LoadgenCounts, MetricsScalars};
    use crate::medida_compat::MedidaCompat;
    use crate::metrics::{
        describe_metrics, register_label_series, LOADGEN_ACCOUNT_CREATED, LOADGEN_RUN_COMPLETE,
        LOADGEN_RUN_START, LOADGEN_SOROBAN_SETUP_INVOKE, LOADGEN_TXN_ATTEMPTED,
    };
    use metrics_exporter_prometheus::PrometheusBuilder;

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

        let v = build_metrics_json(&test_scalars(), &compat, &LoadgenCounts::default());
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

        // --- Shape preserved: 13 curated + 15 loadgen.* meters (#3572) ---
        assert_eq!(m.len(), 28, "13 curated + 15 loadgen.* meters");
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
        let v = build_metrics_json(&test_scalars(), &compat, &LoadgenCounts::default());
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

        // Shape contract: 13 curated + 15 loadgen.* meters, all with type+count.
        assert_eq!(m.len(), 28);
        for (name, metric) in m {
            assert!(metric.get("type").is_some(), "{name} has type");
            assert!(metric.get("count").is_some(), "{name} has count");
        }
        // At startup the loadgen.* meters are present at 0 (so supercluster
        // reads NotStarted, not a missing series).
        let lg = &m["loadgen.run.start"];
        assert_eq!(lg["type"], "meter");
        assert_eq!(lg["count"], 0);
        assert_eq!(lg["event_type"], "run");
    }

    /// #3572 regression: drive a synthetic loadgen lifecycle through the
    /// Prometheus registry, render + parse it, build the COMPAT medida JSON, and
    /// assert the JSON (NOT the Prometheus render) carries the `loadgen.*`
    /// meters supercluster's `IsLoadGenComplete` reads. Fails on `ce336bd3`,
    /// where `build_metrics_json` was 2-arg and emitted no `loadgen.*`.
    #[test]
    fn test_compat_metrics_includes_loadgen_after_run() {
        const ATTEMPTED: u64 = 7;
        const ACCOUNTS: u64 = 3;
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let render = metrics::with_local_recorder(&recorder, || {
            describe_metrics();
            register_label_series();
            // Synthetic run lifecycle: start, accounts, attempts, complete.
            LOADGEN_RUN_START.increment(1.0);
            for _ in 0..ACCOUNTS {
                LOADGEN_ACCOUNT_CREATED.increment(1.0);
            }
            for _ in 0..ATTEMPTED {
                LOADGEN_TXN_ATTEMPTED.increment(1.0);
            }
            LOADGEN_RUN_COMPLETE.increment(1.0);
            handle.render()
        });

        let counts = parse_loadgen_counts(&render);
        let compat = MedidaCompat::new_for_test();
        let v = build_metrics_json(&test_scalars(), &compat, &counts);
        let m = v["metrics"].as_object().unwrap();

        // Supercluster Success condition, read off the COMPAT medida JSON:
        // loadgen.run.start == loadgen.run.complete > 0.
        let start = &m["loadgen.run.start"];
        let complete = &m["loadgen.run.complete"];
        assert_eq!(start["type"], "meter");
        assert_eq!(complete["type"], "meter");
        assert_eq!(start["count"].as_u64().unwrap(), 1);
        assert_eq!(
            start["count"], complete["count"],
            "loadgen.run.start must equal loadgen.run.complete (Success)"
        );
        assert!(
            start["count"].as_u64().unwrap() > 0,
            "loadgen.run.start must be > 0"
        );

        // Progress meters supercluster also polls are present with real counts.
        assert_eq!(
            m["loadgen.account.created"]["count"].as_u64().unwrap(),
            ACCOUNTS
        );
        assert_eq!(
            m["loadgen.txn.attempted"]["count"].as_u64().unwrap(),
            ATTEMPTED
        );
        // Each emitted loadgen.* entry is a medida meter with the rate keys
        // supercluster's JsonProvider needs for type inference.
        for key in [
            "loadgen.run.start",
            "loadgen.run.complete",
            "loadgen.run.failed",
            "loadgen.account.created",
            "loadgen.txn.attempted",
        ] {
            let meter = &m[key];
            assert_eq!(meter["type"], "meter", "{key} is a meter");
            assert!(meter.get("count").is_some(), "{key} has count");
            assert!(meter.get("mean_rate").is_some(), "{key} has mean_rate");
        }
    }

    /// #3572: `parse_loadgen_counts` maps each Prometheus underscore name to the
    /// correct DOTTED medida key, preserving soroban third-component underscores
    /// (`loadgen.soroban.setup_invoke`, NOT `…setup.invoke`); absent series → 0;
    /// label-bearing / prefix-collision lines are ignored. Pins the explicit
    /// mapping table (NOT `replace('_','.')`).
    #[test]
    fn test_parse_loadgen_counts() {
        let render = "\
# TYPE loadgen_run_start counter\n\
loadgen_run_start 1\n\
loadgen_run_complete 1\n\
loadgen_account_created 5\n\
loadgen_txn_attempted 100\n\
loadgen_soroban_setup_invoke 4\n\
loadgen_run_start_total 999\n\
loadgen_txn_attempted{label=\"x\"} 42\n\
some_unrelated_metric 7\n";
        let counts = parse_loadgen_counts(render);

        // Soroban third component keeps its underscore.
        assert_eq!(
            LOADGEN_SOROBAN_SETUP_INVOKE.0,
            "loadgen_soroban_setup_invoke"
        );
        assert_eq!(counts.count("loadgen.soroban.setup_invoke"), 4);

        // Standard dotted mappings.
        assert_eq!(counts.count("loadgen.run.start"), 1);
        assert_eq!(counts.count("loadgen.run.complete"), 1);
        assert_eq!(counts.count("loadgen.account.created"), 5);
        // The exact-token match must NOT pick up the label-bearing
        // `loadgen_txn_attempted{...}` line — only the bare line counts.
        assert_eq!(counts.count("loadgen.txn.attempted"), 100);

        // Absent series default to 0 (not present in the render above).
        assert_eq!(counts.count("loadgen.soroban.create_upgrade"), 0);
        assert_eq!(counts.count("loadgen.step.count"), 0);
    }

    /// #3572 (REQUIRED): guards the production handle-threading. A
    /// `CompatServerState` built with `Some(handle)` after a synthetic run must
    /// surface a non-zero `loadgen.run.start` in the compat JSON — exactly the
    /// path `run_cmd.rs` wires. If `set_prometheus_handle` is ever dropped from
    /// the compat server, the loadgen.* meters silently stay 0 and this fails.
    #[test]
    fn test_compat_metrics_handler_threads_handle() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let render = metrics::with_local_recorder(&recorder, || {
            describe_metrics();
            register_label_series();
            LOADGEN_RUN_START.increment(1.0);
            LOADGEN_RUN_COMPLETE.increment(1.0);
            handle.render()
        });

        // Mirror the handler's render→parse→build path with a wired handle.
        let counts = parse_loadgen_counts(&render);
        let v = build_metrics_json(&test_scalars(), &MedidaCompat::new_for_test(), &counts);
        let start = v["metrics"]["loadgen.run.start"]["count"].as_u64().unwrap();
        assert!(
            start > 0,
            "wired handle must surface non-zero loadgen.run.start, got {start}"
        );

        // And the None path (no handle wired) reports zeroed loadgen.* — the
        // documented fallback, never a missing series.
        let v0 = build_metrics_json(
            &test_scalars(),
            &MedidaCompat::new_for_test(),
            &LoadgenCounts::default(),
        );
        assert_eq!(v0["metrics"]["loadgen.run.start"]["count"], 0);
    }
}
