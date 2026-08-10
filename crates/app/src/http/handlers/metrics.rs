//! Handler for /metrics endpoint (Prometheus format).

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse};

use super::super::ServerState;
use crate::metrics::refresh_gauges;

pub(crate) async fn metrics_handler(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    refresh_gauges(&state).await;
    let body = match &state.prometheus_handle {
        Some(handle) => sort_prometheus_families(&handle.render()),
        None => "# metrics recorder not installed\n".to_string(),
    };
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4")],
        body,
    )
}

/// Normalise `metrics-exporter-prometheus`'s rendered scrape into a byte-stable
/// form for a fixed metric set.
///
/// `PrometheusHandle::render()` walks freshly-allocated, randomly-seeded
/// `HashMap`s, so the *order* of families — and of label-set lines within a
/// family — varies both across restarts and within a single process's lifetime
/// (each `render()` builds new maps with a new `RandomState`). The series set,
/// names, labels, values and types are all deterministic; only line order
/// churns. That churn silently breaks positional consumers of the scrape (line
/// diffs, `awk '{print $2}'` one-liners, archived-snapshot diffs). Prometheus
/// itself does not care about ordering, so this is purely for consumer sanity.
///
/// The theoretical root fix is an ordering knob in the exporter (or sorting
/// inside its recorder); `metrics-exporter-prometheus` 0.18 exposes neither, and
/// an upstream PR / fork was deliberately declined as disproportionate for a
/// low-severity observability nit. Post-processing the rendered text here is the
/// chosen fix — do not re-litigate this without new upstream support.
///
/// Transform:
/// - Family blocks (`# HELP`/`# TYPE` header lines, then metric lines, then a
///   blank separator) are **stable**-sorted by family name.
/// - Within each `counter`/`gauge` block, metric lines are stable-sorted by the
///   whole line. This is value-independent: for two distinct series identities
///   the byte comparison always resolves inside the identity (they differ
///   before the value, or one identity is a strict prefix of the other and the
///   shorter side compares `' '` against a non-space identity byte).
/// - `histogram`/`summary` blocks keep their emitted line order — buckets are
///   emitted in increasing `le` with `+Inf` last, then `_sum`, then `_count`,
///   which lexicographic sorting would scramble (`+Inf` sorts before `0`). That
///   order is already deterministic within a process.
///
/// The line multiset is preserved exactly (nothing dropped, duplicated, or
/// synthesised), output framing matches the exporter's, and the function is
/// idempotent — a byte-for-byte no-op on already-sorted input.
fn sort_prometheus_families(rendered: &str) -> String {
    struct Block {
        name: String,
        header: Vec<String>,
        metrics: Vec<String>,
        sortable: bool,
        has_metric: bool,
    }

    /// The family name of a `# HELP <name> …` / `# TYPE <name> <type>` line is
    /// the third whitespace token.
    fn meta_family(line: &str) -> &str {
        line.split_whitespace().nth(2).unwrap_or("")
    }

    let mut preamble: Vec<&str> = Vec::new();
    let mut blocks: Vec<Block> = Vec::new();
    let mut cur: Option<Block> = None;
    let mut started = false;

    for line in rendered.split('\n') {
        if line.is_empty() {
            // Blank line: closes the current block (and is the exporter's family
            // separator / trailing terminator). Framing is re-synthesised on
            // emit, so blank lines are not carried through.
            if let Some(b) = cur.take() {
                blocks.push(b);
            }
            continue;
        }

        let is_meta = line.starts_with("# HELP ") || line.starts_with("# TYPE ");

        if !started && !is_meta {
            // Anything before the first family block is preamble, emitted first
            // and verbatim (empty for all real scrapes; the recorder-not-
            // installed body reaches this path).
            preamble.push(line);
            continue;
        }

        if is_meta {
            let name = meta_family(line);
            // A meta line opens a new block if its family name differs from the
            // current block's, or if it follows a metric line (a same-named
            // block that has already emitted metrics).
            let open_new = match &cur {
                None => true,
                Some(b) => b.name != name || b.has_metric,
            };
            if open_new {
                if let Some(b) = cur.take() {
                    blocks.push(b);
                }
                cur = Some(Block {
                    name: name.to_string(),
                    header: Vec::new(),
                    metrics: Vec::new(),
                    sortable: false,
                    has_metric: false,
                });
                started = true;
            }
            let b = cur.as_mut().expect("current block is set");
            if line.starts_with("# TYPE ") {
                let ty = line.split_whitespace().nth(3).unwrap_or("");
                b.sortable = ty == "counter" || ty == "gauge";
            }
            b.header.push(line.to_string());
        } else {
            // Non-empty, non-meta line inside a block: a metric line (or any
            // unrecognised line), carried through verbatim.
            match cur.as_mut() {
                Some(b) => {
                    b.metrics.push(line.to_string());
                    b.has_metric = true;
                }
                None => preamble.push(line),
            }
        }
    }
    if let Some(b) = cur.take() {
        blocks.push(b);
    }

    // Stable sort so any same-name blocks retain relative order.
    blocks.sort_by(|a, b| a.name.cmp(&b.name));
    for b in &mut blocks {
        if b.sortable {
            // Stable, whole-line sort — value-independent (see doc comment).
            b.metrics.sort();
        }
    }

    let mut out = String::new();
    for line in &preamble {
        out.push_str(line);
        out.push('\n');
    }
    for b in &blocks {
        for line in &b.header {
            out.push_str(line);
            out.push('\n');
        }
        for line in &b.metrics {
            out.push_str(line);
            out.push('\n');
        }
        // Canonical family separator / terminator, matching the exporter.
        out.push('\n');
    }
    out
}

/// The ordered sequence of family names in a rendered scrape, one per `# TYPE`
/// line. Used by tests to assert family ordering.
#[cfg(test)]
fn family_sequence(rendered: &str) -> Vec<&str> {
    rendered
        .lines()
        .filter(|l| l.starts_with("# TYPE "))
        .filter_map(|l| l.split_whitespace().nth(2))
        .collect()
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
