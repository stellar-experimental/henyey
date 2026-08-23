#!/usr/bin/env python3
"""Regression + coverage tests for the time-denominated counter-streak dwell (#3790).

Follow-up from #3757. The too-fresh guard (#3757) fixes the destructive *reset*
direction (a sub-MIN_EVAL_WINDOW duplicate that samples delta=0 and zeros the
streak). It does NOT fix the *acceleration* direction ("vacuous dwell"): when a
natural ~38 min interval is split into two ~19 min halves that each independently
clear delta>=1, both exceed the 600s too-fresh floor, so breach_streak still
advances at 2x wall-clock speed and `streak >= streak_threshold` fires early.

Fix: make the streak dwell TIME-denominated rather than tick-denominated. Store
`first_breach_ts` on the streak-opening tick and require
`now - first_breach_ts >= streak_threshold * expected_interval_seconds` IN
ADDITION to the count before firing. This makes the confirmation gate
independent of tick cadence and survives a future cadence change.
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


def _make_alarm(name="recovery-stalled", **kwargs):
    """Create a minimal counter-streak alarm dict for testing."""
    alarm = {
        "name": name,
        "kind": "counter-streak",
        "metric": "m",
        "delta_threshold": 1,
        "streak_threshold": 3,
        "burst_threshold": 10,
        "severity": "WARN",
        "expected_interval_seconds": 1200,
    }
    alarm.update(kwargs)
    return alarm


def _seed(state_dir, *, counter_value, breach_streak, first_breach_ts=None,
          pid="P", start_ticks="T"):
    """Write a counter_streak_snapshot with the given fields."""
    snap = {
        "version": "1",
        "pid": pid,
        "start_ticks": start_ticks,
        "counter_value": str(counter_value),
        "breach_streak": str(breach_streak),
    }
    if first_breach_ts is not None:
        snap["first_breach_ts"] = str(first_breach_ts)
    write_snapshot(state_dir / "counter_streak_snapshot", snap)


# ── the headline regression: 2x acceleration must not fire early ──────────────

def test_split_interval_does_not_fire_early():
    """A ~38 min interval split into two ~19 min halves must NOT fire at streak 3.

    Red on main: no time gate — streak reaches 3 and returns "firing" at
    T0+2280 (~38 min), well before the intended dwell. Green after: the count is
    met but 2280 < 3*1200 = 3600, so the tick returns "breach" and keeps dwelling.
    """
    T0 = 1_000_000
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        alarm = _make_alarm()

        # Streak already opened at T0 (breach_streak=1, anchored at T0).
        _seed(state_dir, counter_value=100, breach_streak=1, first_breach_ts=T0)

        # First half boundary: delta>=1 at T0+1140 (~19 min) → streak 2.
        r1 = eval_counter_streak(alarm, {"m": [({}, 101.0)]}, state_dir,
                                 "P", "T", now=T0 + 1140)
        assert r1["state"] == "breach", f"streak 2 must be breach, got {r1['state']}"

        # Second half boundary: delta>=1 at T0+2280 (~38 min) → streak 3, but
        # 2280 < 3600 dwell → still breach, NOT firing.
        r2 = eval_counter_streak(alarm, {"m": [({}, 102.0)]}, state_dir,
                                 "P", "T", now=T0 + 2280)
        assert r2["state"] == "breach", \
            f"count met but dwell unmet must be breach, got {r2['state']}"
        snap = read_snapshot(state_dir / "counter_streak_snapshot")
        assert snap["breach_streak"] == "3", \
            f"streak still counts to 3, got {snap.get('breach_streak')!r}"
        assert snap["first_breach_ts"] == str(T0), \
            f"first_breach_ts must be preserved, got {snap.get('first_breach_ts')!r}"


def test_dwell_met_fires():
    """The gate is a DELAY, not a permanent suppression: once wall-clock dwell is
    satisfied, streak >= threshold fires."""
    T0 = 1_000_000
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        alarm = _make_alarm()
        _seed(state_dir, counter_value=100, breach_streak=2, first_breach_ts=T0)

        # streak → 3 at T0+3600: 3600 >= 3*1200 → fires.
        r = eval_counter_streak(alarm, {"m": [({}, 101.0)]}, state_dir,
                                "P", "T", now=T0 + 3600)
        assert r["state"] == "firing", \
            f"dwell met at streak 3 must fire, got {r['state']}"


# ── first_breach_ts lifecycle ─────────────────────────────────────────────────

def test_first_breach_ts_set_on_open_and_preserved():
    """first_breach_ts is set to `now` on the 0->1 opening tick and byte-preserved
    while the streak advances 1->2."""
    T0 = 2_000_000
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        alarm = _make_alarm()
        # No prior streak, no anchor.
        _seed(state_dir, counter_value=100, breach_streak=0)

        # 0 -> 1: opens the streak, anchors at T0.
        r1 = eval_counter_streak(alarm, {"m": [({}, 101.0)]}, state_dir,
                                 "P", "T", now=T0)
        assert r1["state"] == "breach", f"streak 1 must be breach, got {r1['state']}"
        snap1 = read_snapshot(state_dir / "counter_streak_snapshot")
        assert snap1["breach_streak"] == "1"
        assert snap1["first_breach_ts"] == str(T0), \
            f"first_breach_ts must be set on open, got {snap1.get('first_breach_ts')!r}"

        # 1 -> 2: anchor preserved verbatim (NOT re-set to the later now).
        r2 = eval_counter_streak(alarm, {"m": [({}, 102.0)]}, state_dir,
                                 "P", "T", now=T0 + 5000)
        assert r2["state"] == "breach"
        snap2 = read_snapshot(state_dir / "counter_streak_snapshot")
        assert snap2["breach_streak"] == "2"
        assert snap2["first_breach_ts"] == str(T0), \
            f"first_breach_ts must be preserved on advance, got {snap2.get('first_breach_ts')!r}"


def test_first_breach_ts_cleared_on_reset():
    """Every reset branch (delta==0, counter-reset, PID-change, gap-stale) drops
    both breach_streak and first_breach_ts."""
    T0 = 3_000_000

    # delta == 0
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        alarm = _make_alarm()
        _seed(state_dir, counter_value=100, breach_streak=2, first_breach_ts=T0)
        eval_counter_streak(alarm, {"m": [({}, 100.0)]}, state_dir,
                            "P", "T", now=T0 + 1200)
        snap = read_snapshot(state_dir / "counter_streak_snapshot")
        assert snap["breach_streak"] == "0", "delta==0 must zero the streak"
        assert "first_breach_ts" not in snap, \
            f"delta==0 must clear first_breach_ts, got {snap.get('first_breach_ts')!r}"

    # counter reset (cur < prev)
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        alarm = _make_alarm()
        _seed(state_dir, counter_value=100, breach_streak=2, first_breach_ts=T0)
        eval_counter_streak(alarm, {"m": [({}, 50.0)]}, state_dir,
                            "P", "T", now=T0 + 1200)
        snap = read_snapshot(state_dir / "counter_streak_snapshot")
        assert snap["breach_streak"] == "0"
        assert "first_breach_ts" not in snap, \
            "counter-reset must clear first_breach_ts"

    # PID change
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        alarm = _make_alarm()
        _seed(state_dir, counter_value=100, breach_streak=2, first_breach_ts=T0,
              pid="OLD")
        eval_counter_streak(alarm, {"m": [({}, 101.0)]}, state_dir,
                            "NEW", "T", now=T0 + 1200)
        snap = read_snapshot(state_dir / "counter_streak_snapshot")
        assert snap["breach_streak"] == "0"
        assert "first_breach_ts" not in snap, \
            "PID-change re-baseline must clear first_breach_ts"

    # gap-stale
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        alarm = _make_alarm()
        _seed(state_dir, counter_value=100, breach_streak=2, first_breach_ts=T0)
        eval_counter_streak(alarm, {"m": [({}, 101.0)]}, state_dir,
                            "P", "T", gap_stale=True, now=T0 + 1200)
        snap = read_snapshot(state_dir / "counter_streak_snapshot")
        assert snap["breach_streak"] == "0"
        assert "first_breach_ts" not in snap, \
            "gap-stale re-baseline must clear first_breach_ts"


def test_legacy_snapshot_without_first_breach_ts_reanchors():
    """A legacy snapshot with breach_streak>=1 but NO first_breach_ts must
    re-anchor to `now` on the next tick and NOT fire off the stale unanchored
    streak (one-time conservative delay, never a spurious fire)."""
    T0 = 4_000_000
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        alarm = _make_alarm()
        # Legacy: streak already at 2, but no anchor persisted.
        _seed(state_dir, counter_value=100, breach_streak=2)

        r = eval_counter_streak(alarm, {"m": [({}, 101.0)]}, state_dir,
                                "P", "T", now=T0)
        assert r["state"] == "breach", \
            f"unanchored streak must not fire on re-anchor tick, got {r['state']}"
        snap = read_snapshot(state_dir / "counter_streak_snapshot")
        assert snap["breach_streak"] == "3"
        assert snap["first_breach_ts"] == str(T0), \
            f"legacy streak must re-anchor first_breach_ts to now, got {snap.get('first_breach_ts')!r}"


def test_dwell_preserved_across_reset_hook():
    """A sub-dwell breach tick returns state="breach"; the centralized
    maybe_reset_counter_snapshot hook only mutates on state=="skipped", so a
    "breach" return preserves first_breach_ts end-to-end (Critic A invariant)."""
    T0 = 5_000_000
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        alarm = _make_alarm()
        _seed(state_dir, counter_value=100, breach_streak=1, first_breach_ts=T0)

        r = eval_counter_streak(alarm, {"m": [({}, 101.0)]}, state_dir,
                                "P", "T", now=T0 + 1140)
        assert r["state"] == "breach"

        # Drive the same post-processing hook main() calls after each result.
        maybe_reset_counter_snapshot(alarm, "counter-streak", r["state"],
                                     state_dir, r.get("skip_reason"))
        snap = read_snapshot(state_dir / "counter_streak_snapshot")
        assert snap["first_breach_ts"] == str(T0), \
            f"reset hook must preserve first_breach_ts on breach, got {snap.get('first_breach_ts')!r}"
        assert snap["breach_streak"] == "2", \
            f"reset hook must preserve breach_streak on breach, got {snap.get('breach_streak')!r}"


# ── burst path stays ungated ──────────────────────────────────────────────────

def test_burst_still_fires_immediately():
    """The acute burst path (delta >= burst_threshold) fires on a single tick
    regardless of dwell, but still initializes first_breach_ts."""
    T0 = 6_000_000
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        alarm = _make_alarm()
        _seed(state_dir, counter_value=100, breach_streak=0)

        # delta=15 >= burst_threshold=10 on the opening tick.
        r = eval_counter_streak(alarm, {"m": [({}, 115.0)]}, state_dir,
                                "P", "T", now=T0)
        assert r["state"] == "firing", \
            f"burst must fire immediately, got {r['state']}"
        snap = read_snapshot(state_dir / "counter_streak_snapshot")
        assert snap["first_breach_ts"] == str(T0), \
            f"burst must anchor first_breach_ts, got {snap.get('first_breach_ts')!r}"


def test_expected_interval_default():
    """With no expected_interval_seconds in the alarm, the 1200s default is used,
    so the dwell required for streak 3 is 3*1200 = 3600s."""
    T0 = 7_000_000
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        alarm = _make_alarm()
        del alarm["expected_interval_seconds"]  # exercise the default

        # streak 2 -> 3 just below 3600s dwell → breach.
        _seed(state_dir, counter_value=100, breach_streak=2, first_breach_ts=T0)
        r_below = eval_counter_streak(alarm, {"m": [({}, 101.0)]}, state_dir,
                                      "P", "T", now=T0 + 3599)
        assert r_below["state"] == "breach", \
            f"3599 < 3600 default dwell must be breach, got {r_below['state']}"

        # And exactly at 3600s → firing.
        _seed(state_dir, counter_value=100, breach_streak=2, first_breach_ts=T0)
        r_at = eval_counter_streak(alarm, {"m": [({}, 101.0)]}, state_dir,
                                   "P", "T", now=T0 + 3600)
        assert r_at["state"] == "firing", \
            f"3600 == default dwell must fire, got {r_at['state']}"


if __name__ == "__main__":
    import pytest
    sys.exit(pytest.main([__file__, "-v"]))
