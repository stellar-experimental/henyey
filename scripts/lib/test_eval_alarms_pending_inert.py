#!/usr/bin/env python3
"""Regression + coverage tests for pending-too-old-ratio reachability (#3826).

Three coupled fixes:
  1. The `synced-only` gate is removed from `pending-too-old-ratio` so the alarm
     is reachable — the denominator only accrues at/near sync, the exact regime
     `synced-only` suppressed.
  2. `eval_counter_ratio` tracks a persistent `{name}_zero_den_streak`; after
     INERT_ZERO_DEN_TICKS consecutive `den_delta == 0` ticks the skip_reason
     flips from the generic `low volume (…)` to `inert (denominator 0 for N
     ticks)`, and `render_aggregate` surfaces it as `<short> inert (…)`.
  3. `lint_gate_metric_contradictions` flags any alarm whose gate set excludes
     the regime its input metrics come from, so this dead-alarm shape is caught
     at authoring time.
"""

import importlib.util
import sys
import tempfile
from pathlib import Path

# eval-alarms.py uses a hyphen, so we need importlib.
_spec = importlib.util.spec_from_file_location(
    "eval_alarms",
    Path(__file__).parent / "eval-alarms.py",
)
_mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_mod)

read_snapshot = _mod.read_snapshot
write_snapshot = _mod.write_snapshot
eval_counter_ratio = _mod.eval_counter_ratio
render_aggregate = _mod.render_aggregate
gates_pass = _mod.gates_pass

try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib  # type: ignore[no-redef]

CATALOG_PATH = (
    Path(__file__).parent.parent.parent
    / ".claude" / "skills" / "shared" / "metric-alarms.toml"
)


# ── Helpers ──────────────────────────────────────────────────────────────────

def _load_catalog() -> dict:
    with open(CATALOG_PATH, "rb") as f:
        return tomllib.load(f)


def _find_alarm(catalog: dict, name: str) -> dict:
    for a in catalog.get("alarm", []):
        if a.get("name") == name:
            return a
    raise AssertionError(f"alarm {name!r} not found in catalog")


def _ratio_alarm(**kwargs) -> dict:
    alarm = {
        "name": "test-ratio",
        "kind": "counter-ratio",
        "numerator": "num",
        "denominator": "den",
        "ratio_op": ">",
        "ratio_threshold": 0.5,
        "min_volume": 100,
        "streak_threshold": 3,
        "severity": "WARN",
    }
    alarm.update(kwargs)
    return alarm


def _scrape(num: float, den: float, ledger_age: float = 5.0) -> dict:
    return {
        "num": [({}, num)],
        "den": [({}, den)],
        "stellar_ledger_age_current_seconds": [({}, ledger_age)],
    }


def _eval(alarm, current, state_dir, **kw):
    kwargs = dict(
        pid="123", start_ticks="456",
        fresh_start=False, crash_recovery=False, uptime=3600,
    )
    kwargs.update(kw)
    return eval_counter_ratio(
        alarm, current, {}, state_dir,
        kwargs.pop("pid"), kwargs.pop("start_ticks"),
        fresh_start=kwargs.pop("fresh_start"),
        crash_recovery=kwargs.pop("crash_recovery"),
        uptime=kwargs.pop("uptime"),
        **kwargs,
    )


# ── Fix 1: gate removed / reachable in synced regime ─────────────────────────

def test_pending_too_old_ratio_gate_removed():
    """The catalog's pending-too-old-ratio no longer carries `synced-only`.

    Fails on main: gates == ["synced-only", "validator-only"].
    """
    alarm = _find_alarm(_load_catalog(), "pending-too-old-ratio")
    assert alarm["gates"] == ["validator-only"], \
        f"expected [validator-only], got {alarm['gates']}"


def test_ratio_evaluates_in_synced_regime_after_gate_removal():
    """With the real catalog gate set, a synced-but-<15m node (uptime=700) is
    NOT gated out, and a two-tick eval reaches a real breach/ok/firing state.

    Fails on main: catalog gates include `synced-only`, so gates_pass at
    uptime=700 returns (False, "not synced (synced-only gate)").
    """
    gates = _find_alarm(_load_catalog(), "pending-too-old-ratio")["gates"]
    ok, reason = gates_pass(
        gates, warmup_remaining=0, fresh_start=False,
        crash_recovery=False, uptime=700, monitor_mode="validator",
    )
    assert ok is True, f"gate must pass at uptime=700, got ({ok}, {reason!r})"

    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        alarm = _ratio_alarm()
        # Tick 1 — collecting baseline.
        r1 = _eval(alarm, _scrape(0, 0), state_dir, uptime=700)
        assert r1["state"] == "collecting_baseline", r1
        # Tick 2 — breach: 90 too-old of 100 received = 0.9 > 0.5.
        r2 = _eval(alarm, _scrape(90, 100), state_dir, uptime=700)
        assert r2["state"] in ("breach", "firing", "ok"), \
            f"second tick must evaluate, got {r2['state']} ({r2.get('skip_reason')})"
        assert r2["state"] != "skipped", r2


# ── Fix 2: inert rendering ───────────────────────────────────────────────────

def test_zero_denominator_streak_renders_inert():
    """After INERT_ZERO_DEN_TICKS consecutive den_delta==0 ticks the skip_reason
    flips to `inert (…)` and render_aggregate surfaces `pending inert (…)`.

    Fails on main: always `low volume (delta=0 < 100)`; no inert concept.
    """
    n = _mod.INERT_ZERO_DEN_TICKS
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        alarm = _ratio_alarm(name="pending-too-old-ratio")
        # Baseline.
        _eval(alarm, _scrape(0, 0), state_dir)
        result = None
        for _ in range(n):
            result = _eval(alarm, _scrape(0, 0), state_dir)
        assert result["state"] == "skipped", result
        assert result["skip_reason"].startswith("inert (denominator 0 for"), \
            f"expected inert skip_reason, got {result['skip_reason']!r}"

        out = render_aggregate([result], watcher_mode=False)
        line = out["metrics_ratio_line"]
        assert "pending inert (denominator 0 for" in line, \
            f"expected inert render, got {line!r}"
        assert "pending skipped" not in line, line


def test_inert_threshold_boundary():
    """Streak N-1 is still `low volume`; streak N flips to `inert`."""
    n = _mod.INERT_ZERO_DEN_TICKS
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        alarm = _ratio_alarm()
        _eval(alarm, _scrape(0, 0), state_dir)  # baseline
        last = None
        for i in range(1, n):  # ticks 1..n-1
            last = _eval(alarm, _scrape(0, 0), state_dir)
        assert last["skip_reason"].startswith("low volume"), \
            f"streak {n - 1} must still be low volume, got {last['skip_reason']!r}"
        # tick n → inert
        final = _eval(alarm, _scrape(0, 0), state_dir)
        assert final["skip_reason"].startswith("inert (denominator 0 for"), \
            f"streak {n} must be inert, got {final['skip_reason']!r}"


def test_zero_denominator_streak_resets_on_volume():
    """A 0 < den_delta < min_volume tick resets the inert streak; a following
    den_delta==0 tick restarts the streak from 1 (not from where it was)."""
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        alarm = _ratio_alarm(name="pending-too-old-ratio")
        _eval(alarm, _scrape(0, 0), state_dir)  # baseline num=0 den=0
        # A few zero-den ticks accumulate the streak.
        for _ in range(3):
            _eval(alarm, _scrape(0, 0), state_dir)
        snap = read_snapshot(state_dir / "ratio_snapshot")
        assert snap["pending-too-old-ratio_zero_den_streak"] == "3", snap

        # Small-but-nonzero denominator activity: den 0 → 40 (delta 40 < 100).
        r = _eval(alarm, _scrape(0, 40), state_dir)
        assert r["skip_reason"].startswith("low volume"), r
        snap = read_snapshot(state_dir / "ratio_snapshot")
        assert snap["pending-too-old-ratio_zero_den_streak"] == "0", \
            f"streak must reset on volume, got {snap.get('pending-too-old-ratio_zero_den_streak')!r}"

        # Next zero-den tick (den stays 40) restarts the streak at 1.
        _eval(alarm, _scrape(0, 40), state_dir)
        snap = read_snapshot(state_dir / "ratio_snapshot")
        assert snap["pending-too-old-ratio_zero_den_streak"] == "1", snap


def test_zero_den_streak_resets_on_rebaseline():
    """A counter-reset re-baseline and a gap_stale re-baseline both zero the
    `{name}_zero_den_streak` key (Critic A item)."""
    # Counter reset (cur_den < prev_den).
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "ratio_snapshot"
        write_snapshot(snap_path, {
            "version": "1", "pid": "123", "start_ticks": "456",
            "myalarm_numerator": "10", "myalarm_denominator": "500",
            "myalarm_streak": "0", "myalarm_zero_den_streak": "15",
        })
        alarm = _ratio_alarm(name="myalarm")
        r = _eval(alarm, _scrape(10, 100), state_dir)  # den 500 → 100 = reset
        assert r["state"] == "collecting_baseline", r
        snap = read_snapshot(snap_path)
        assert snap["myalarm_zero_den_streak"] == "0", \
            f"counter reset must zero zero_den_streak, got {snap.get('myalarm_zero_den_streak')!r}"

    # gap_stale re-baseline.
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "ratio_snapshot"
        write_snapshot(snap_path, {
            "version": "1", "pid": "123", "start_ticks": "456",
            "myalarm_numerator": "10", "myalarm_denominator": "50",
            "myalarm_streak": "0", "myalarm_zero_den_streak": "15",
        })
        alarm = _ratio_alarm(name="myalarm")
        r = _eval(alarm, _scrape(20, 200), state_dir, gap_stale=True)
        assert r["state"] == "collecting_baseline", r
        snap = read_snapshot(snap_path)
        assert snap["myalarm_zero_den_streak"] == "0", \
            f"gap_stale must zero zero_den_streak, got {snap.get('myalarm_zero_den_streak')!r}"


# ── Fix 3: startup contradiction lint ────────────────────────────────────────

def test_gate_metric_lint_flags_synced_only_pending():
    """lint_gate_metric_contradictions flags a synced-only alarm whose
    denominator is a pending-family metric.

    Fails on main: function does not exist.
    """
    catalog = {"alarm": [{
        "name": "synthetic-pending",
        "kind": "counter-ratio",
        "gates": ["synced-only"],
        "numerator": "stellar_herder_pending_too_old_total",
        "denominator": "stellar_herder_pending_received_total",
    }]}
    warnings = _mod.lint_gate_metric_contradictions(catalog)
    assert warnings, "expected a contradiction warning, got none"
    assert any("synthetic-pending" in w and "synced-only" in w for w in warnings), \
        f"warning must name alarm + gate, got {warnings!r}"


def test_lint_no_warning_for_sound_alarm():
    """A synced-only alarm over a metric that accrues DURING sync is sound."""
    catalog = {"alarm": [{
        "name": "scp-accept-rate-low",
        "kind": "counter-ratio",
        "gates": ["synced-only"],
        "numerator": "henyey_scp_post_verify_total",
        "denominator": "henyey_scp_post_verify_total",
    }]}
    assert _mod.lint_gate_metric_contradictions(catalog) == [], \
        "sound synced-only alarm must not warn"


def test_live_catalog_lint_clean():
    """The real post-fix catalog has no gate/metric contradictions."""
    warnings = _mod.lint_gate_metric_contradictions(_load_catalog())
    assert warnings == [], f"live catalog must be lint-clean, got {warnings!r}"


# ── Run tests ─────────────────────────────────────────────────────────────────

if __name__ == "__main__":
    tests = [v for k, v in sorted(globals().items()) if k.startswith("test_")]
    passed = failed = 0
    for t in tests:
        try:
            t()
            passed += 1
            print(f"  PASS  {t.__name__}")
        except Exception as e:
            failed += 1
            print(f"  FAIL  {t.__name__}: {e}")
    print(f"\n{passed} passed, {failed} failed")
    sys.exit(1 if failed else 0)
