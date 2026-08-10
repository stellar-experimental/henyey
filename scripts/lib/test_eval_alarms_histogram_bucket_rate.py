#!/usr/bin/env python3
"""Tests for the histogram-bucket-rate alarm kind (#3750).

Covers the new `eval_histogram_bucket_rate()` evaluator and the shipped
`scp-externalize-slow-rate` catalog alarm. The evaluator fires when the fraction
of histogram observations exceeding a configured `le` boundary over the tick
window exceeds `rate_threshold` (strict `>`). It is a defense-in-depth
complement to the existing single-sample `scp-externalize-slow` gauge alarm,
which is left unchanged.

Mirrors test_eval_alarms_histogram_rebucket.py's import-by-path harness.
"""

import importlib.util
from pathlib import Path

# Import eval-alarms.py (uses hyphen in filename)
_spec = importlib.util.spec_from_file_location(
    "eval_alarms",
    Path(__file__).parent / "eval-alarms.py",
)
_mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_mod)

eval_histogram_bucket_rate = _mod.eval_histogram_bucket_rate
validate_catalog = _mod.validate_catalog

# SCP_TIMING_BUCKETS from crates/app/src/metrics.rs (the 10.0 boundary is what
# the scp-externalize-slow-rate alarm keys on).
SCP_TIMING_BUCKETS = [
    0.1, 0.25, 0.5, 1.0, 2.0, 3.0, 4.0, 5.0, 7.0, 10.0, 15.0, 20.0, 30.0,
    float("inf"),
]

METRIC = "stellar_scp_timing_externalized_hist_seconds"

ALARM = {
    "name": "scp-externalize-slow-rate",
    "metric": METRIC,
    "kind": "histogram-bucket-rate",
    "bucket_le": 10,
    "rate_threshold": 0.02,
    "min_count_delta": 100,
    "severity": "WARN",
    "details": "over_10s_rate={rate_value} threshold={threshold}",
}


def _le_label(le: float) -> str:
    return "+Inf" if le == float("inf") else str(le)


def make_histogram_data(count: int, n_over: int, buckets=None):
    """Build (current, prev) prom data where `n_over` of `count` new observations
    exceed the 10s boundary.

    Buckets are cumulative (Prometheus semantics). The `count - n_over`
    under-threshold observations sit at 5.0s (≤ every le ≥ 5.0); the `n_over`
    over-threshold observations sit at 12.0s (≤ every le ≥ 15.0). So the
    cumulative count at le=10 is exactly the number of under-threshold slots,
    and over = count - bucket[10] = n_over. Prev is all-zero (fresh baseline).
    """
    if buckets is None:
        buckets = SCP_TIMING_BUCKETS
    n_under = count - n_over

    bucket_series_cur = []
    for le in buckets:
        if le < 5.0:
            cumulative = 0
        elif le < 15.0:
            cumulative = n_under
        else:
            cumulative = n_under + n_over
        bucket_series_cur.append(({"le": _le_label(le)}, cumulative))

    current = {
        f"{METRIC}_bucket": bucket_series_cur,
        f"{METRIC}_count": [([], count)],
        f"{METRIC}_sum": [([], 5.0 * n_under + 12.0 * n_over)],
    }
    bucket_series_prev = [({"le": _le_label(le)}, 0) for le in buckets]
    prev = {
        f"{METRIC}_bucket": bucket_series_prev,
        f"{METRIC}_count": [([], 0)],
        f"{METRIC}_sum": [([], 0)],
    }
    return current, prev


def test_rate_above_threshold_fires():
    """4% of slots > 10s (> threshold 2%) → fires."""
    current, prev = make_histogram_data(count=1000, n_over=40)
    result = eval_histogram_bucket_rate(ALARM, current, prev, prev_prom_invalid=False)
    assert result["state"] == "firing", f"Expected 'firing', got '{result['state']}'"


def test_rate_below_threshold_does_not_fire():
    """1% of slots > 10s (< threshold 2%) → ok."""
    current, prev = make_histogram_data(count=1000, n_over=10)
    result = eval_histogram_bucket_rate(ALARM, current, prev, prev_prom_invalid=False)
    assert result["state"] == "ok", f"Expected 'ok', got '{result['state']}'"


def test_rate_at_threshold_does_not_fire():
    """Exactly 2% of slots > 10s (strict >) → ok."""
    current, prev = make_histogram_data(count=1000, n_over=20)
    result = eval_histogram_bucket_rate(ALARM, current, prev, prev_prom_invalid=False)
    assert result["state"] == "ok", f"Expected 'ok' (strict >), got '{result['state']}'"


def test_low_volume_skips():
    """count_delta < min_count_delta → skipped."""
    current, prev = make_histogram_data(count=50, n_over=40)
    result = eval_histogram_bucket_rate(ALARM, current, prev, prev_prom_invalid=False)
    assert result["state"] == "skipped", f"Expected 'skipped', got '{result['state']}'"
    assert "low volume" in (result.get("skip_reason") or "")


def test_prev_prom_invalid_skips():
    """PREV_PROM_INVALID=true → skipped."""
    current, prev = make_histogram_data(count=1000, n_over=40)
    result = eval_histogram_bucket_rate(ALARM, current, prev, prev_prom_invalid=True)
    assert result["state"] == "skipped", f"Expected 'skipped', got '{result['state']}'"


def test_gap_stale_skips():
    """gap_stale=True → skipped (deltas span the loop gap)."""
    current, prev = make_histogram_data(count=1000, n_over=40)
    result = eval_histogram_bucket_rate(
        ALARM, current, prev, prev_prom_invalid=False, gap_stale=True,
        gap_stale_age_hours=5.0,
    )
    assert result["state"] == "skipped", f"Expected 'skipped', got '{result['state']}'"


def test_counter_reset_skips():
    """cur_count < prev_count → skipped (counter reset)."""
    current, prev = make_histogram_data(count=1000, n_over=40)
    # Swap so current count is below prev count.
    current[f"{METRIC}_count"] = [([], 500)]
    prev[f"{METRIC}_count"] = [([], 1000)]
    result = eval_histogram_bucket_rate(ALARM, current, prev, prev_prom_invalid=False)
    assert result["state"] == "skipped", f"Expected 'skipped', got '{result['state']}'"


def test_missing_bucket_le_skips():
    """Configured bucket_le absent from the bucket set → skipped."""
    current, prev = make_histogram_data(count=1000, n_over=40)
    alarm = dict(ALARM)
    alarm["bucket_le"] = 99  # not a real boundary
    result = eval_histogram_bucket_rate(alarm, current, prev, prev_prom_invalid=False)
    assert result["state"] == "skipped", f"Expected 'skipped', got '{result['state']}'"


def test_negative_over_clamped():
    """Degenerate scrape where bucket[10] delta > count_delta → over clamps to 0, ok."""
    current, prev = make_histogram_data(count=1000, n_over=40)
    # Force the le=10 cumulative above the total count (differencing artifact).
    bumped = []
    for labels, val in current[f"{METRIC}_bucket"]:
        if labels.get("le") in ("10.0", "10"):
            bumped.append((labels, 1200))
        else:
            bumped.append((labels, val))
    current[f"{METRIC}_bucket"] = bumped
    result = eval_histogram_bucket_rate(ALARM, current, prev, prev_prom_invalid=False)
    assert result["state"] == "ok", f"Expected 'ok' (clamped), got '{result['state']}'"
    assert result["value"] == 0, f"Expected clamped rate 0, got {result['value']}"


def test_zero_volume_no_division_error():
    """Defensive: an alarm configured with min_count_delta=0 and a zero-volume
    window must not raise ZeroDivisionError on `over / count_delta`; the window
    has no observations, so rate is 0.0 and the alarm stays ok."""
    zero_min_alarm = dict(ALARM)
    zero_min_alarm["min_count_delta"] = 0
    current, prev = make_histogram_data(count=0, n_over=0)
    result = eval_histogram_bucket_rate(
        zero_min_alarm, current, prev, prev_prom_invalid=False
    )
    assert result["state"] == "ok", f"Expected 'ok', got '{result['state']}'"
    assert result["value"] == 0, f"Expected rate 0, got {result['value']}"


def _load_catalog():
    try:
        import tomllib
    except ImportError:
        import tomli as tomllib
    catalog_path = (
        Path(__file__).parent.parent.parent
        / ".claude" / "skills" / "shared" / "metric-alarms.toml"
    )
    with open(catalog_path, "rb") as f:
        return tomllib.load(f)


def test_catalog_alarm_semantics():
    """Shipped catalog has scp-externalize-slow-rate and leaves the gauge alarm intact."""
    catalog = _load_catalog()
    rate_alarm = None
    gauge_alarm = None
    for a in catalog.get("alarm", []):
        if a["name"] == "scp-externalize-slow-rate":
            rate_alarm = a
        if a["name"] == "scp-externalize-slow":
            gauge_alarm = a

    assert rate_alarm is not None, "scp-externalize-slow-rate not found in catalog"
    assert rate_alarm["kind"] == "histogram-bucket-rate"
    assert rate_alarm["metric"] == METRIC
    assert rate_alarm["bucket_le"] == 10
    assert rate_alarm["rate_threshold"] == 0.02
    assert rate_alarm["min_count_delta"] == 100

    assert gauge_alarm is not None, "scp-externalize-slow (gauge) must remain"
    assert gauge_alarm["kind"] == "gauge"
    assert gauge_alarm["threshold"] == 10
    assert gauge_alarm.get("for_ticks", 1) == 1
    # The two alarms must fire independently (distinct cooldown keys).
    assert rate_alarm.get("cooldown_key") != gauge_alarm.get("cooldown_key")


def test_catalog_validates():
    """validate_catalog() returns no errors for the shipped catalog."""
    catalog = _load_catalog()
    errors = validate_catalog(catalog)
    assert errors == [], f"Expected no schema errors, got: {errors}"


if __name__ == "__main__":
    tests = [
        test_rate_above_threshold_fires,
        test_rate_below_threshold_does_not_fire,
        test_rate_at_threshold_does_not_fire,
        test_low_volume_skips,
        test_prev_prom_invalid_skips,
        test_gap_stale_skips,
        test_counter_reset_skips,
        test_missing_bucket_le_skips,
        test_negative_over_clamped,
        test_zero_volume_no_division_error,
        test_catalog_alarm_semantics,
        test_catalog_validates,
    ]

    passed = 0
    failed = 0
    for test in tests:
        try:
            test()
            passed += 1
            print(f"  PASS: {test.__name__}")
        except AssertionError as e:
            failed += 1
            print(f"  FAIL: {test.__name__}: {e}")

    print(f"\n{passed}/{passed + failed} tests passed")
    if failed:
        raise SystemExit(1)
