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

    use super::{family_sequence, sort_prometheus_families};

    /// A family sequence is sorted iff each name is <= the next.
    fn is_sorted(seq: &[&str]) -> bool {
        seq.windows(2).all(|w| w[0] <= w[1])
    }

    /// Test 1 — families and label-set lines are reordered to a byte-stable form.
    ///
    /// The fixture is intentionally hostile: a gauge family precedes an
    /// alphabetically-earlier counter family; label sets appear in reverse
    /// identity order with values that sort *opposite* to their identities (so
    /// a value-based sort would produce a different, wrong answer); and one
    /// family has a `# TYPE` line with no `# HELP`. Asserted against the exact
    /// expected bytes.
    #[test]
    fn test_sort_prometheus_families_orders_families_and_label_sets() {
        let input = "\
# HELP z_gauge help z
# TYPE z_gauge gauge
z_gauge{k=\"b\"} 1
z_gauge{k=\"a\"} 2

# HELP a_counter help a
# TYPE a_counter counter
a_counter 5

# TYPE n_notype gauge
n_notype{id=\"2\"} 10
n_notype{id=\"1\"} 20

";
        let expected = "\
# HELP a_counter help a
# TYPE a_counter counter
a_counter 5

# TYPE n_notype gauge
n_notype{id=\"1\"} 20
n_notype{id=\"2\"} 10

# HELP z_gauge help z
# TYPE z_gauge gauge
z_gauge{k=\"a\"} 2
z_gauge{k=\"b\"} 1

";
        assert_eq!(sort_prometheus_families(input), expected);
    }

    /// Test 2 — the real rendered catalog comes out with a sorted family
    /// sequence after normalisation, and (validity guard) is *not* sorted
    /// before it.
    ///
    /// Guard (a) fails deterministically and seed-independently on
    /// `origin/main`: the exporter emits all counters, then all gauges, then
    /// all histograms, and the pre-registered catalog contains a gauge
    /// (`henyey_archive_cache_populated`) whose name sorts after a counter
    /// (`stellar_bucket_merge_annihilated_total`), so the family sequence can
    /// never be sorted for any `RandomState` seed.
    ///
    /// If guard (a) ever fires, the exporter now emits sorted output — delete
    /// `sort_prometheus_families`, its call site, and this test.
    #[test]
    fn test_rendered_catalog_families_are_sorted() {
        let handle = metrics::ensure_test_recorder();
        metrics::describe_metrics();
        metrics::register_label_series();
        let raw = handle.render();

        // (a) validity guard: raw output is NOT sorted.
        assert!(
            !is_sorted(&family_sequence(&raw)),
            "exporter unexpectedly emitted sorted families — see doc comment"
        );

        // (b) the fix: normalised output IS sorted.
        let sorted = sort_prometheus_families(&raw);
        assert!(
            is_sorted(&family_sequence(&sorted)),
            "sort_prometheus_families did not produce a sorted family sequence"
        );
    }

    /// Test 3 — histogram bucket / sum / count order is preserved verbatim.
    ///
    /// Buckets are emitted in increasing `le` with `+Inf` last, then `_sum`,
    /// then `_count`. Lexicographic sorting would hoist `+Inf` to the front and
    /// scramble numeric bucket order; the fix must leave histogram blocks
    /// untouched.
    #[test]
    fn test_sort_prometheus_families_preserves_histogram_bucket_order() {
        let input = "\
# HELP h_lat help
# TYPE h_lat histogram
h_lat_bucket{le=\"0.001\"} 1
h_lat_bucket{le=\"0.01\"} 5
h_lat_bucket{le=\"+Inf\"} 9
h_lat_sum 3.2
h_lat_count 9

";
        // Single family, already canonically framed → byte-for-byte identical.
        assert_eq!(sort_prometheus_families(input), input);
    }

    /// Test 4 — the transform never drops, duplicates, or synthesises a metric
    /// line: the output line multiset equals the input line multiset. Includes
    /// a `# HELP`/`# TYPE`-only family with zero metric lines.
    #[test]
    fn test_sort_prometheus_families_preserves_all_lines() {
        let input = "\
# HELP z_gauge help
# TYPE z_gauge gauge
z_gauge 7

# HELP m_empty help
# TYPE m_empty gauge

# HELP a_counter help
# TYPE a_counter counter
a_counter{x=\"2\"} 1
a_counter{x=\"1\"} 2

";
        let out = sort_prometheus_families(input);
        let mut in_lines: Vec<&str> = input.lines().filter(|l| !l.is_empty()).collect();
        let mut out_lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        in_lines.sort_unstable();
        out_lines.sort_unstable();
        assert_eq!(in_lines, out_lines, "line multiset changed");
    }

    /// Test 5 — idempotence: sorting twice equals sorting once, and sorting
    /// already-sorted input is a byte-for-byte no-op. The second input does
    /// NOT end in a blank line, pinning the re-emitted framing.
    #[test]
    fn test_sort_prometheus_families_is_idempotent() {
        let unsorted = "\
# TYPE b_gauge gauge
b_gauge{k=\"2\"} 1
b_gauge{k=\"1\"} 2

# TYPE a_counter counter
a_counter 5

";
        let once = sort_prometheus_families(unsorted);
        let twice = sort_prometheus_families(&once);
        assert_eq!(once, twice, "not idempotent");

        // Input with no trailing blank line still normalises to canonical
        // framing (trailing blank line), and re-sorting is then a no-op.
        let no_trailing_blank = "# TYPE a_counter counter\na_counter 5";
        let normalised = sort_prometheus_families(no_trailing_blank);
        assert_eq!(normalised, "# TYPE a_counter counter\na_counter 5\n\n");
        assert_eq!(sort_prometheus_families(&normalised), normalised);
    }

    /// Test 6 — non-family input round-trips through the preamble path
    /// unchanged: the empty string, and the recorder-not-installed body.
    #[test]
    fn test_sort_prometheus_families_passes_through_non_family_input() {
        assert_eq!(sort_prometheus_families(""), "");
        assert_eq!(
            sort_prometheus_families("# metrics recorder not installed\n"),
            "# metrics recorder not installed\n"
        );
    }
}
