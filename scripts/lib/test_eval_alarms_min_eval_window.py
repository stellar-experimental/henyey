#!/usr/bin/env python3
"""Regression tests for the symmetric "too-fresh" lower-bound guard (#3757).

A duplicate / back-to-back monitor-tick (e.g. the watchdog firing into a
still-running interactive tick) can sample an inter-scrape interval shorter than
MIN_EVAL_WINDOW_SECONDS. Without a lower bound, such a tick either resets a
breach streak to 0 (delta=0) or advances it (delta>=1), letting the duplicate
burn alarm cooldowns and double-advance streak/ratio snapshots.

The too-fresh guard makes the cross-tick counter-family evaluators return a
NON-destructive skip (skip_reason == SKIP_INTERVAL_TOO_SHORT, no snapshot
write), and maybe_reset_counter_snapshot early-returns on that sentinel so the
snapshot is PRESERVED. This mirrors the existing gap_stale UPPER bound.
"""

import sys
import tempfile
from pathlib import Path

# eval-alarms.py uses a hyphen, so we need importlib
import importlib.util

_spec = importlib.util.spec_from_file_location(
    "eval_alarms",
    Path(__file__).parent / "eval-alarms.py",
)
_mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_mod)

read_snapshot = _mod.read_snapshot
write_snapshot = _mod.write_snapshot
maybe_reset_counter_snapshot = _mod.maybe_reset_counter_snapshot
eval_counter_streak = _mod.eval_counter_streak
eval_counter_ratio = _mod.eval_counter_ratio
compute_too_fresh = _mod.compute_too_fresh
SKIP_INTERVAL_TOO_SHORT = _mod.SKIP_INTERVAL_TOO_SHORT


def _make_alarm(name, kind="counter-streak", **kwargs):
    """Create a minimal alarm dict for testing."""
    alarm = {"name": name, "kind": kind}
    alarm.update(kwargs)
    return alarm


# ── derivation ────────────────────────────────────────────────────────────────

def test_unknown_age_not_too_fresh():
    """Age -1/absent ⇒ unknown ⇒ NOT too-fresh (fail-safe, mirrors gap_stale).

    The half-open window [0, min) means: age 0 IS too-fresh, exactly the window
    is NOT, a normal (>= window) interval is NOT, and unknown (-1) is NOT.
    """
    assert compute_too_fresh(-1, 600) is False, "unknown age must NOT be too-fresh"
    assert compute_too_fresh(2640, 600) is False, "normal ~44m interval is not too-fresh"
    assert compute_too_fresh(120, 600) is True, "a 2m duplicate interval is too-fresh"
    assert compute_too_fresh(600, 600) is False, "exactly the window is not too-fresh"
    assert compute_too_fresh(0, 600) is True, "age 0 (same instant) is too-fresh"


# ── counter-streak ────────────────────────────────────────────────────────────

def test_too_fresh_preserves_breach_streak_on_zero_delta():
    """A too-fresh duplicate tick with delta=0 must PRESERVE breach_streak, not
    zero it — and the centralized reset must preserve it too.

    Red on main: eval_counter_streak has no too_fresh param (TypeError); even
    ignoring that, delta=0 rewrites breach_streak to "0" and maybe_reset then
    clears the whole snapshot. Green after: the evaluator returns a non-destructive
    skip and maybe_reset early-returns on the sentinel.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "counter_streak_snapshot"
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "P",
            "start_ticks": "T",
            "counter_value": "3642",
            "breach_streak": "14",
        })
        alarm = _make_alarm("recovery-stalled", kind="counter-streak",
                            metric="m", delta_threshold=1, streak_threshold=3,
                            burst_threshold=10, severity="WARN")
        # delta=0: current counter equals the snapshot baseline.
        current = {"m": [({}, 3642.0)]}

        result = eval_counter_streak(alarm, current, state_dir, "P", "T",
                                     too_fresh=True)

        assert result["state"] == "skipped", \
            f"too-fresh tick must skip, got {result['state']}"
        assert result["skip_reason"] == SKIP_INTERVAL_TOO_SHORT, \
            f"expected too-short sentinel, got {result['skip_reason']!r}"
        snap = read_snapshot(snap_path)
        assert snap["breach_streak"] == "14", \
            f"breach_streak must be preserved by the evaluator, got {snap.get('breach_streak')!r}"
        assert snap["counter_value"] == "3642", \
            f"counter_value must be preserved, got {snap.get('counter_value')!r}"

        # The centralized reset must also preserve it (early-return on sentinel).
        maybe_reset_counter_snapshot(alarm, "counter-streak", result["state"],
                                     state_dir, result.get("skip_reason"))
        snap2 = read_snapshot(snap_path)
        assert snap2["breach_streak"] == "14", \
            f"maybe_reset must preserve breach_streak on too-fresh, got {snap2.get('breach_streak')!r}"
        assert snap2["counter_value"] == "3642", \
            f"maybe_reset must preserve counter_value, got {snap2.get('counter_value')!r}"


def test_too_fresh_no_false_advance():
    """A too-fresh duplicate with delta>=threshold must NOT advance breach_streak.

    Red on main: delta=5 >= delta_threshold advances the streak to 3 (fires).
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "counter_streak_snapshot"
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "P",
            "start_ticks": "T",
            "counter_value": "100",
            "breach_streak": "2",
        })
        alarm = _make_alarm("recovery-stalled", kind="counter-streak",
                            metric="m", delta_threshold=1, streak_threshold=3,
                            burst_threshold=10, severity="WARN")
        # delta=5 (>= delta_threshold) would normally advance the streak to 3.
        current = {"m": [({}, 105.0)]}

        result = eval_counter_streak(alarm, current, state_dir, "P", "T",
                                     too_fresh=True)

        assert result["state"] == "skipped", \
            f"too-fresh tick must skip, got {result['state']}"
        snap = read_snapshot(snap_path)
        assert snap["breach_streak"] == "2", \
            f"breach_streak must not advance on too-fresh, got {snap.get('breach_streak')!r}"
        assert snap["counter_value"] == "100", \
            f"counter_value must be preserved (delta not consumed), got {snap.get('counter_value')!r}"


def test_too_fresh_default_off_streak():
    """Default too_fresh=False leaves normal evaluation intact: delta>=threshold
    still advances the streak (normal path unregressed)."""
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "counter_streak_snapshot"
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "P",
            "start_ticks": "T",
            "counter_value": "100",
            "breach_streak": "2",
        })
        alarm = _make_alarm("recovery-stalled", kind="counter-streak",
                            metric="m", delta_threshold=1, streak_threshold=3,
                            burst_threshold=10, severity="WARN")
        current = {"m": [({}, 105.0)]}

        # No too_fresh kwarg → default False → normal advance.
        result = eval_counter_streak(alarm, current, state_dir, "P", "T")

        assert result["state"] in ("breach", "firing"), \
            f"normal tick must advance, got {result['state']}"
        snap = read_snapshot(snap_path)
        assert snap["breach_streak"] == "3", \
            f"breach_streak must advance normally when not too-fresh, got {snap.get('breach_streak')!r}"


# ── counter-ratio ─────────────────────────────────────────────────────────────

def test_too_fresh_ratio_preserves_streak():
    """A too-fresh duplicate must PRESERVE the counter-ratio streak and baselines.

    Red on main: without the guard the ratio 1.0 > 0.5 breaches, advancing the
    streak to 3 and rewriting the baselines to the duplicate's values.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "ratio_snapshot"
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "P",
            "start_ticks": "T",
            "myalarm_streak": "2",
            "myalarm_numerator": "100",
            "myalarm_denominator": "500",
        })
        alarm = _make_alarm("myalarm", kind="counter-ratio",
                            numerator="num", denominator="den",
                            ratio_threshold=0.5, ratio_op=">",
                            streak_threshold=3, min_volume=1)
        # Would breach (ratio 1.0 > 0.5) and advance streak to 3 without the guard.
        current = {"num": [({}, 200.0)], "den": [({}, 600.0)]}

        result = eval_counter_ratio(
            alarm, current, {}, state_dir, "P", "T",
            fresh_start=False, crash_recovery=False, uptime=3600,
            too_fresh=True,
        )

        assert result["state"] == "skipped", \
            f"too-fresh ratio tick must skip, got {result['state']}"
        assert result["skip_reason"] == SKIP_INTERVAL_TOO_SHORT, \
            f"expected too-short sentinel, got {result['skip_reason']!r}"
        snap = read_snapshot(snap_path)
        assert snap["myalarm_streak"] == "2", \
            f"ratio streak must be preserved, got {snap.get('myalarm_streak')!r}"
        assert snap["myalarm_numerator"] == "100", \
            f"numerator baseline must be preserved, got {snap.get('myalarm_numerator')!r}"
        assert snap["myalarm_denominator"] == "500", \
            f"denominator baseline must be preserved, got {snap.get('myalarm_denominator')!r}"

        # Centralized reset must also preserve the streak (early-return on sentinel).
        maybe_reset_counter_snapshot(alarm, "counter-ratio", result["state"],
                                     state_dir, result.get("skip_reason"))
        snap2 = read_snapshot(snap_path)
        assert snap2["myalarm_streak"] == "2", \
            f"maybe_reset must preserve ratio streak on too-fresh, got {snap2.get('myalarm_streak')!r}"


def test_too_fresh_ratio_default_off():
    """Default too_fresh=False leaves counter-ratio evaluation intact: a breach
    still advances the streak."""
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "ratio_snapshot"
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "P",
            "start_ticks": "T",
            "myalarm_streak": "2",
            "myalarm_numerator": "100",
            "myalarm_denominator": "500",
        })
        alarm = _make_alarm("myalarm", kind="counter-ratio",
                            numerator="num", denominator="den",
                            ratio_threshold=0.5, ratio_op=">",
                            streak_threshold=3, min_volume=1)
        current = {"num": [({}, 200.0)], "den": [({}, 600.0)]}

        # No too_fresh kwarg → default False → normal breach advance.
        result = eval_counter_ratio(
            alarm, current, {}, state_dir, "P", "T",
            fresh_start=False, crash_recovery=False, uptime=3600,
        )

        assert result["state"] in ("breach", "firing"), \
            f"normal ratio tick must advance, got {result['state']}"
        snap = read_snapshot(snap_path)
        assert snap["myalarm_streak"] == "3", \
            f"ratio streak must advance normally when not too-fresh, got {snap.get('myalarm_streak')!r}"


# ── Run tests ─────────────────────────────────────────────────────────────────

if __name__ == "__main__":
    tests = [v for k, v in sorted(globals().items()) if k.startswith("test_")]
    passed = 0
    failed = 0
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
