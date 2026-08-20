#!/usr/bin/env python3
"""Regression tests for the optional companion-metric guard on gauge alarms (#3752).

`jemalloc-frag-high` false-fires on long-uptime nodes: `henyey_jemalloc_fragmentation_pct`
oscillates ~8 points on a 60s cycle (#3759) and grazes the flat 50% WARN threshold
while resident memory stays bounded/stable (~18-21 GB) — i.e. the frag ratio is high
but there is no real memory pressure, so the fire is not actionable.

The fix adds an optional `guard_metric`/`guard_op`/`guard_threshold` to `eval_gauge`:
a tick counts as breaching only if BOTH the primary condition AND the guard condition
hold. Applied to `jemalloc-frag-high` with a guard on `henyey_jemalloc_resident_bytes`
> 32 GiB, so the alarm only fires when resident memory is genuinely elevated.

These call `eval_gauge()` / `validate_catalog()` directly (import-by-path harness,
mirroring test_eval_alarms_histogram_rebucket.py).
"""

import importlib.util
from pathlib import Path

try:
    import tomllib
except ImportError:
    import tomli as tomllib  # type: ignore[no-redef]

# Import eval-alarms.py (hyphen in filename → import by path).
_spec = importlib.util.spec_from_file_location(
    "eval_alarms",
    Path(__file__).parent / "eval-alarms.py",
)
_mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_mod)

eval_gauge = _mod.eval_gauge
validate_catalog = _mod.validate_catalog

GIB = 1024 ** 3
GUARD_THRESHOLD = 34359738368  # 32 GiB

# jemalloc-frag-high-shaped alarm WITH the resident-memory guard.
GUARD_ALARM = {
    "name": "jemalloc-frag-high",
    "metric": "henyey_jemalloc_fragmentation_pct",
    "kind": "gauge",
    "extraction": "form1",
    "labels": [],
    "op": ">",
    "threshold": 50,
    "for_ticks": 2,
    "severity": "WARN",
    "summary": "Jemalloc fragmentation high (sustained)",
    "details": "fragmentation={value}% threshold={threshold}%",
    "guard_metric": "henyey_jemalloc_resident_bytes",
    "guard_op": ">",
    "guard_threshold": GUARD_THRESHOLD,
}

# Same alarm WITHOUT any guard fields (backward-compat / other gauge alarms).
NO_GUARD_ALARM = {k: v for k, v in GUARD_ALARM.items() if not k.startswith("guard_")}


def make_current(frag=None, resident=None):
    """Build a parsed-metrics dict: {metric_name: [(labels, value)]}."""
    current = {}
    if frag is not None:
        current["henyey_jemalloc_fragmentation_pct"] = [({}, float(frag))]
    if resident is not None:
        current["henyey_jemalloc_resident_bytes"] = [({}, float(resident))]
    return current


def run_two_ticks(alarm, current):
    """Evaluate the same scrape for two consecutive ticks (for_ticks=2)."""
    persistence = {}
    states = []
    for _ in range(2):
        result = eval_gauge(alarm, current, persistence, prev_prom_invalid=False)
        states.append(result["state"])
    return states


# ── Regression tests (fail on origin/main) ───────────────────────────────────

def test_guard_below_threshold_suppresses():
    """frag=55 (>50) but resident=20 GiB (< 32 GiB guard) → never fires.

    THE reported false-fire. On main, eval_gauge ignores the guard_* keys, so
    frag=55 for two ticks yields breach→firing; the guard must suppress it.
    """
    current = make_current(frag=55, resident=20 * GIB)
    states = run_two_ticks(GUARD_ALARM, current)
    assert states == ["ok", "ok"], states
    assert "firing" not in states
    assert "breach" not in states


def test_guard_above_threshold_fires():
    """frag=55 (>50) AND resident=36 GiB (> 32 GiB guard) → fires (actionable)."""
    current = make_current(frag=55, resident=36 * GIB)
    states = run_two_ticks(GUARD_ALARM, current)
    assert states == ["breach", "firing"], states


# ── New coverage ──────────────────────────────────────────────────────────────

def test_guard_absent_suppresses():
    """frag=55 but the guard metric is missing from the scrape → never fires.

    Guard cannot be confirmed → fail toward not-firing.
    """
    current = make_current(frag=55, resident=None)
    states = run_two_ticks(GUARD_ALARM, current)
    assert states == ["ok", "ok"], states


def test_primary_below_guard_above():
    """frag=40 (≤50) with resident=36 GiB → ok (primary not breaching)."""
    current = make_current(frag=40, resident=36 * GIB)
    states = run_two_ticks(GUARD_ALARM, current)
    assert states == ["ok", "ok"], states


def test_no_guard_backward_compat():
    """Alarm with no guard_* fields: frag=55 ×2 → fires, exactly as before."""
    current = make_current(frag=55, resident=None)
    states = run_two_ticks(NO_GUARD_ALARM, current)
    assert states == ["breach", "firing"], states


def test_guard_primary_absent_skipped():
    """Primary metric missing → skipped, regardless of guard (unchanged path)."""
    current = make_current(frag=None, resident=36 * GIB)
    persistence = {}
    result = eval_gauge(GUARD_ALARM, current, persistence, prev_prom_invalid=False)
    assert result["state"] == "skipped", result
    assert result["skip_reason"] == "metric not found"


# ── guard_extraction is honored (#3841) ────────────────────────────────────────
#
# The shipped jemalloc-frag-high alarm uses the default guard_extraction=form1,
# so the guard's `extraction` argument is only exercised implicitly. These tests
# pin that eval_gauge actually threads `guard_extraction` into extract_value.
#
# The discriminator: eval_gauge calls extract_value(current, guard_metric,
# guard_extraction) with NO label selector, so form1 (the default) returns the
# FIRST series while a sum extraction (form3) returns the TOTAL. A guard scrape
# with two labeled sub-series, each individually below the threshold but summing
# above it, therefore fires ONLY when the sum extraction is honored — and would
# NOT fire if guard_extraction were ignored and form1 silently used instead.

# jemalloc resident reported as two per-arena labeled sub-series: 20 GiB each,
# summing to 40 GiB (> the 32 GiB guard). No unlabeled series, so form1 falls
# back to the first sub-series (20 GiB, below the guard).
def make_current_split_guard(frag):
    return {
        "henyey_jemalloc_fragmentation_pct": [({}, float(frag))],
        "henyey_jemalloc_resident_bytes": [
            ({"arena": "0"}, float(20 * GIB)),
            ({"arena": "1"}, float(20 * GIB)),
        ],
    }


# Same alarm as GUARD_ALARM but the guard uses a sum extraction over the
# per-arena sub-series.
SUM_GUARD_ALARM = {**GUARD_ALARM, "guard_extraction": "form3"}


def test_guard_extraction_sum_honored():
    """Guard sub-series sum (40 GiB) > 32 GiB with guard_extraction=form3 → fires.

    Proves eval_gauge passes guard_extraction through to extract_value: the sum
    of the two 20-GiB sub-series crosses the guard only under a sum extraction.
    """
    current = make_current_split_guard(frag=55)
    states = run_two_ticks(SUM_GUARD_ALARM, current)
    assert states == ["breach", "firing"], states


def test_guard_extraction_default_form1_suppresses():
    """Control: identical scrape with the default (form1) guard extraction.

    form1 reads only the first 20-GiB sub-series (< 32 GiB guard) → suppressed.
    This is the counterfactual: the difference from the test above is *solely*
    the guard_extraction setting, so it pins that the setting is what's honored.
    """
    current = make_current_split_guard(frag=55)
    states = run_two_ticks(GUARD_ALARM, current)  # no guard_extraction → form1
    assert states == ["ok", "ok"], states


# ── validate_catalog ──────────────────────────────────────────────────────────

def _catalog(alarm_overrides):
    """Wrap a single alarm into a minimal valid catalog dict."""
    base = {
        "name": "some-alarm",
        "metric": "henyey_jemalloc_fragmentation_pct",
        "kind": "gauge",
        "op": ">",
        "threshold": 50,
        "severity": "WARN",
    }
    base.update(alarm_overrides)
    return {"schema_version": 1, "alarm": [base]}


def test_validate_partial_guard_rejected():
    """guard_metric present but guard_op / guard_threshold missing → error."""
    catalog = _catalog({"guard_metric": "henyey_jemalloc_resident_bytes"})
    errors = validate_catalog(catalog)
    assert any("guard_op" in e for e in errors), errors
    assert any("guard_threshold" in e for e in errors), errors


def test_validate_bad_guard_op_rejected():
    """guard_op not in VALID_OPS → error."""
    catalog = _catalog({
        "guard_metric": "henyey_jemalloc_resident_bytes",
        "guard_op": "><",
        "guard_threshold": GUARD_THRESHOLD,
    })
    errors = validate_catalog(catalog)
    assert any("guard_op" in e for e in errors), errors


def test_validate_complete_guard_accepted():
    """A fully-specified guard passes validation."""
    catalog = _catalog({
        "guard_metric": "henyey_jemalloc_resident_bytes",
        "guard_op": ">",
        "guard_threshold": GUARD_THRESHOLD,
    })
    errors = validate_catalog(catalog)
    assert errors == [], errors


# ── Shipped catalog ───────────────────────────────────────────────────────────

def _load_shipped_catalog():
    repo_root = Path(__file__).resolve().parents[2]
    path = repo_root / ".claude" / "skills" / "shared" / "metric-alarms.toml"
    with open(path, "rb") as f:
        return tomllib.load(f)


def _find_alarm(catalog, name):
    for a in catalog.get("alarm", []):
        if a.get("name") == name:
            return a
    return None


def test_catalog_frag_alarm_semantics():
    """The shipped jemalloc-frag-high alarm carries the resident guard."""
    catalog = _load_shipped_catalog()
    frag = _find_alarm(catalog, "jemalloc-frag-high")
    assert frag is not None, "jemalloc-frag-high not found in shipped catalog"
    assert frag["guard_metric"] == "henyey_jemalloc_resident_bytes", frag
    assert frag["guard_op"] == ">", frag
    assert isinstance(frag["guard_threshold"], (int, float)), frag
    # Primary semantics unchanged.
    assert frag["kind"] == "gauge"
    assert frag["op"] == ">"
    assert frag["threshold"] == 50
    assert frag["for_ticks"] == 2
    assert frag["severity"] == "WARN"


def test_shipped_catalog_validates():
    """The real catalog validates end-to-end (exercises the new guard branch)."""
    catalog = _load_shipped_catalog()
    errors = validate_catalog(catalog)
    assert errors == [], errors


def test_agents_catalog_matches_claude():
    """.agents/ catalog is byte-identical to the .claude/ copy."""
    repo_root = Path(__file__).resolve().parents[2]
    claude = (repo_root / ".claude" / "skills" / "shared" / "metric-alarms.toml").read_bytes()
    agents = (repo_root / ".agents" / "skills" / "shared" / "metric-alarms.toml").read_bytes()
    assert claude == agents, "catalog copies diverged"


if __name__ == "__main__":
    import sys
    fns = [v for k, v in sorted(globals().items()) if k.startswith("test_") and callable(v)]
    failures = 0
    for fn in fns:
        try:
            fn()
            print(f"PASS {fn.__name__}")
        except Exception as e:  # noqa: BLE001
            failures += 1
            print(f"FAIL {fn.__name__}: {e}")
    print(f"\n{len(fns) - failures}/{len(fns)} passed")
    sys.exit(1 if failures else 0)
