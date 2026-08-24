#!/usr/bin/env python3
"""Regression tests for stale counter snapshot state carryover (issue #2617).

Tests verify that maybe_reset_counter_snapshot() correctly resets stateful
snapshot keys on "skipped" ticks and preserves them on non-skipped ticks.
Also tests the eval_counter_dynamic baseline state change from "skipped"
to "collecting_baseline".
"""

import os
import subprocess
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
eval_counter_dynamic = _mod.eval_counter_dynamic
eval_counter_streak = _mod.eval_counter_streak
eval_counter_ratio = _mod.eval_counter_ratio
render_aggregate = _mod.render_aggregate


# ── Helpers ──────────────────────────────────────────────────────────────────

def _make_alarm(name, kind="counter-dynamic", **kwargs):
    """Create a minimal alarm dict for testing."""
    alarm = {"name": name, "kind": kind}
    alarm.update(kwargs)
    return alarm


# ── counter-dynamic tests ────────────────────────────────────────────────────

def test_counter_dynamic_skip_resets_prior_delta():
    """Skipped state deletes prior_delta key from counter_dynamic_snapshot."""
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "counter_dynamic_snapshot"
        write_snapshot(snap_path, {"prior_delta_spike-alarm": "42", "other_key": "1"})

        alarm = _make_alarm("spike-alarm", kind="counter-dynamic")
        maybe_reset_counter_snapshot(alarm, "counter-dynamic", "skipped", state_dir)

        snap = read_snapshot(snap_path)
        assert "prior_delta_spike-alarm" not in snap, f"prior_delta should be deleted, got {snap}"
        assert snap["other_key"] == "1", "other keys should be preserved"


def test_counter_dynamic_collecting_baseline_preserves():
    """collecting_baseline state does NOT delete prior_delta."""
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "counter_dynamic_snapshot"
        write_snapshot(snap_path, {"prior_delta_spike-alarm": "42"})

        alarm = _make_alarm("spike-alarm", kind="counter-dynamic")
        maybe_reset_counter_snapshot(alarm, "counter-dynamic", "collecting_baseline", state_dir)

        snap = read_snapshot(snap_path)
        assert snap["prior_delta_spike-alarm"] == "42", "prior_delta should be preserved"


def test_counter_dynamic_ok_preserves():
    """ok state does NOT delete prior_delta."""
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "counter_dynamic_snapshot"
        write_snapshot(snap_path, {"prior_delta_spike-alarm": "42"})

        alarm = _make_alarm("spike-alarm", kind="counter-dynamic")
        maybe_reset_counter_snapshot(alarm, "counter-dynamic", "ok", state_dir)

        snap = read_snapshot(snap_path)
        assert snap["prior_delta_spike-alarm"] == "42", "prior_delta should be preserved"


def test_counter_dynamic_no_snapshot_file_no_error():
    """Skipped state with no existing snapshot file does not error."""
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        alarm = _make_alarm("spike-alarm", kind="counter-dynamic")
        # Should not raise
        maybe_reset_counter_snapshot(alarm, "counter-dynamic", "skipped", state_dir)


# ── counter-ratio tests ──────────────────────────────────────────────────────

def test_counter_ratio_skip_resets_streak_only():
    """Skipped state zeros streak but preserves baselines."""
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "ratio_snapshot"
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "123",
            "start_ticks": "456",
            "myalarm_streak": "3",
            "myalarm_numerator": "100",
            "myalarm_denominator": "500",
            "other_alarm_streak": "2",
        })

        alarm = _make_alarm("myalarm", kind="counter-ratio")
        maybe_reset_counter_snapshot(alarm, "counter-ratio", "skipped", state_dir)

        snap = read_snapshot(snap_path)
        assert snap["myalarm_streak"] == "0", f"streak should be 0, got {snap['myalarm_streak']}"
        assert snap["myalarm_numerator"] == "100", "numerator baseline should be preserved"
        assert snap["myalarm_denominator"] == "500", "denominator baseline should be preserved"
        # Other alarm's data should be preserved
        assert snap["other_alarm_streak"] == "2", "other alarm streak should be preserved"
        assert snap["version"] == "1", "version should be preserved"


def test_counter_ratio_no_reset_on_breach():
    """breach state does NOT reset streak or baselines."""
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "ratio_snapshot"
        write_snapshot(snap_path, {
            "myalarm_streak": "2",
            "myalarm_numerator": "100",
            "myalarm_denominator": "500",
        })

        alarm = _make_alarm("myalarm", kind="counter-ratio")
        maybe_reset_counter_snapshot(alarm, "counter-ratio", "breach", state_dir)

        snap = read_snapshot(snap_path)
        assert snap["myalarm_streak"] == "2", "streak should be preserved on breach"
        assert snap["myalarm_numerator"] == "100", "numerator should be preserved on breach"


# ── counter-streak tests ─────────────────────────────────────────────────────

def test_counter_streak_skip_clears_snapshot():
    """Skipped state clears the entire counter-streak snapshot to force
    baseline re-collection on resume.
    
    eval_counter_streak defaults missing counter_value to 0, so partial
    deletion would make the full counter value appear as a delta.
    Clearing the entire snapshot triggers the 'if not snapshot:' baseline
    collection path on resume.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "counter_streak_snapshot"
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "123",
            "start_ticks": "456",
            "counter_value": "100",
            "breach_streak": "5",
        })

        alarm = _make_alarm("stalled", kind="counter-streak")
        maybe_reset_counter_snapshot(alarm, "counter-streak", "skipped", state_dir)

        snap = read_snapshot(snap_path)
        assert len(snap) == 0, f"snapshot should be empty, got {snap}"


def test_counter_streak_custom_snapshot_file():
    """Alarm with custom snapshot_file uses the correct file."""
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        sub = state_dir / "metrics"
        sub.mkdir()
        snap_path = sub / "counter_streak_snapshot"
        write_snapshot(snap_path, {
            "version": "1",
            "breach_streak": "3",
            "counter_value": "50",
        })

        alarm = _make_alarm("stalled", kind="counter-streak",
                            snapshot_file="metrics/counter_streak_snapshot")
        maybe_reset_counter_snapshot(alarm, "counter-streak", "skipped", state_dir)

        snap = read_snapshot(snap_path)
        assert len(snap) == 0, "snapshot should be cleared"


def test_counter_streak_no_reset_on_ok():
    """ok state does NOT reset breach_streak or counter_value."""
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "counter_streak_snapshot"
        write_snapshot(snap_path, {
            "breach_streak": "2",
            "counter_value": "100",
        })

        alarm = _make_alarm("stalled", kind="counter-streak")
        maybe_reset_counter_snapshot(alarm, "counter-streak", "ok", state_dir)

        snap = read_snapshot(snap_path)
        assert snap["breach_streak"] == "2", "breach_streak should be preserved on ok"
        assert snap["counter_value"] == "100", "counter_value should be preserved on ok"


# ── eval_counter_dynamic state change test ────────────────────────────────────

def test_eval_counter_dynamic_baseline_uses_collecting_baseline():
    """eval_counter_dynamic returns 'collecting_baseline' (not 'skipped') when no prior delta."""
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        alarm = _make_alarm(
            "overlay-timeouts-spike",
            kind="counter-dynamic",
            metric_sum=["stellar_overlay_timeout_idle_total"],
            extraction="form1",
            multiplier=5,
            min_absolute=5,
            severity="WARN",
        )
        # Current has the metric, prev has the metric, so delta computes fine.
        # But no prior_delta in snapshot → collecting baseline.
        current = {"stellar_overlay_timeout_idle_total": [({}, 10.0)]}
        prev = {"stellar_overlay_timeout_idle_total": [({}, 5.0)]}

        result = eval_counter_dynamic(
            alarm, current, prev, state_dir,
            prev_prom_invalid=False, warmup_remaining=0,
        )
        assert result["state"] == "collecting_baseline", \
            f"Expected 'collecting_baseline', got '{result['state']}'"


def test_eval_counter_dynamic_skip_still_skipped():
    """eval_counter_dynamic still returns 'skipped' for actual skip paths."""
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        alarm = _make_alarm(
            "overlay-timeouts-spike",
            kind="counter-dynamic",
            metric_sum=["stellar_overlay_timeout_idle_total"],
            extraction="form1",
            multiplier=5,
        )
        # Metric not found in current → skipped
        current = {}
        prev = {"stellar_overlay_timeout_idle_total": [({}, 5.0)]}

        result = eval_counter_dynamic(
            alarm, current, prev, state_dir,
            prev_prom_invalid=False, warmup_remaining=0,
        )
        assert result["state"] == "skipped", \
            f"Expected 'skipped', got '{result['state']}'"


# ── End-to-end test ──────────────────────────────────────────────────────────

def test_end_to_end_counter_ratio_skip_no_false_fire():
    """After a skip gap, counter-ratio streak restarts from 0.

    Scenario:
    1. Accumulate streak=2 in ratio_snapshot
    2. Call maybe_reset on a "skipped" result
    3. Verify streak is 0 and baselines are cleared
    4. This means when the alarm resumes, it will collect baseline first,
       then start counting breaches from 0.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "ratio_snapshot"
        # Simulate accumulated state: 2 consecutive breaches, with baselines
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "123",
            "start_ticks": "456",
            "scp-accept-rate-low_streak": "2",
            "scp-accept-rate-low_numerator": "90",
            "scp-accept-rate-low_denominator": "100",
        })

        alarm = _make_alarm("scp-accept-rate-low", kind="counter-ratio")

        # A skip gap occurs (e.g., FRESH_START)
        maybe_reset_counter_snapshot(alarm, "counter-ratio", "skipped", state_dir)

        snap = read_snapshot(snap_path)
        assert snap["scp-accept-rate-low_streak"] == "0", \
            "streak should restart from 0 after skip gap"
        assert snap["scp-accept-rate-low_numerator"] == "90", \
            "numerator baseline should be preserved (cumulative counter)"
        assert snap["scp-accept-rate-low_denominator"] == "100", \
            "denominator baseline should be preserved (cumulative counter)"


def test_counter_ratio_low_volume_preserves_baselines():
    """Low-volume skip in eval_counter_ratio updates baselines; centralized
    reset must NOT clobber them. Only streak should be zeroed (which the
    evaluator already did)."""
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "ratio_snapshot"
        # Simulate state after a low-volume skip: evaluator updated baselines
        # to new values and reset streak to "0", then returned "skipped"
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "123",
            "start_ticks": "456",
            "myalarm_streak": "0",  # already reset by evaluator
            "myalarm_numerator": "200",  # freshly updated baseline
            "myalarm_denominator": "1000",  # freshly updated baseline
        })

        alarm = _make_alarm("myalarm", kind="counter-ratio")
        # Centralized reset fires because state == "skipped"
        maybe_reset_counter_snapshot(alarm, "counter-ratio", "skipped", state_dir)

        snap = read_snapshot(snap_path)
        assert snap["myalarm_streak"] == "0", "streak should remain 0"
        assert snap["myalarm_numerator"] == "200", \
            "numerator baseline should be preserved after low-volume skip"
        assert snap["myalarm_denominator"] == "1000", \
            "denominator baseline should be preserved after low-volume skip"


def test_counter_streak_skip_prevents_false_burst():
    """After skip reset, eval_counter_streak should re-collect baseline instead
    of computing a delta from a stale or zero counter_value."""
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "counter_streak_snapshot"
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "123",
            "start_ticks": "456",
            "counter_value": "500",
            "breach_streak": "3",
        })

        alarm = _make_alarm("stalled", kind="counter-streak")
        maybe_reset_counter_snapshot(alarm, "counter-streak", "skipped", state_dir)

        # Snapshot should be empty, forcing baseline re-collection
        snap = read_snapshot(snap_path)
        assert len(snap) == 0, \
            "snapshot should be empty to force baseline re-collection"


def test_counter_streak_resume_after_skip_collects_baseline():
    """End-to-end: after skip clears snapshot, eval_counter_streak returns
    collecting_baseline on the next tick instead of computing a delta."""
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "counter_streak_snapshot"

        # Simulate state before skip: accumulated some breach streak
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "123",
            "start_ticks": "456",
            "counter_value": "100",
            "breach_streak": "2",
        })

        alarm = _make_alarm("recovery-stalled", kind="counter-streak",
                            delta_threshold=1, streak_threshold=3,
                            burst_threshold=10)

        # Skip occurs → clear snapshot
        maybe_reset_counter_snapshot(alarm, "counter-streak", "skipped", state_dir)

        # Resume: metric is available at value 500 (big jump from 100)
        current = {"recovery-stalled-metric": [({}, 500.0)]}
        alarm["metric"] = "recovery-stalled-metric"

        result = eval_counter_streak(alarm, current, state_dir, "123", "456")

        # Should collect baseline, NOT fire or breach on the 400 delta
        assert result["state"] == "collecting_baseline", \
            f"Expected collecting_baseline after skip, got {result['state']}"


# ── post-restart absolute fire tests (issue #3274) ───────────────────────────

def test_post_restart_fire_sets_marker_on_result():
    """A baseline-reset (PID-change) tick whose absolute counter value crosses
    post_restart_absolute_threshold fires, and the firing result must carry the
    `post_restart` marker so the renderer can label it `(post-restart)`.

    Fails on origin/main: maybe_post_restart_fire sets `post_restart` only inside
    extra_values, and make_result never surfaces it onto the result dict.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "counter_streak_snapshot"
        # Existing baseline under a DIFFERENT pid → next eval is a baseline reset.
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "111",
            "start_ticks": "100",
            "counter_value": "0",
            "breach_streak": "0",
        })

        alarm = _make_alarm("recovery-stalled", kind="counter-streak",
                            metric="recovery-stalled-metric",
                            delta_threshold=1, streak_threshold=3,
                            burst_threshold=10,
                            post_restart_absolute_threshold=50,
                            severity="WARN")
        current = {"recovery-stalled-metric": [({}, 736.0)]}

        # New pid "222" != snapshot pid "111" → baseline-reset branch.
        result = eval_counter_streak(alarm, current, state_dir, "222", "200")

        assert result["state"] == "firing", \
            f"Expected firing on post-restart absolute fire, got {result['state']}"
        assert result["value"] == 736, \
            f"Expected absolute value 736, got {result['value']}"
        assert result.get("post_restart") is True, \
            f"Result must surface post_restart=True, got {result.get('post_restart')!r}"

        # Contract: the fresh baseline (new pid, breach_streak=0) is written
        # BEFORE the fire, so the next tick's streak machine stays consistent.
        snap = read_snapshot(snap_path)
        assert snap["pid"] == "222", "fresh baseline pid must be written before fire"
        assert snap["breach_streak"] == "0", "fresh baseline streak must be 0"


def test_render_post_restart_absolute_not_burst():
    """render_aggregate labels a post_restart firing result with the documented
    absolute=N (post-restart) form, NOT delta=N (burst).

    Fails on origin/main: the renderer only sees value=736 >= 10 and emits
    `(burst)` because it never inspects the post_restart marker.
    """
    r = {
        "contributes_to": "recovery_stalled",
        "state": "firing",
        "value": 736,
        "post_restart": True,
    }
    out = render_aggregate([r], watcher_mode=False)
    line = out["recovery_stalled_line"]
    assert line == "recovery_stalled: WARNING absolute=736 (post-restart) — investigating", \
        f"Expected post-restart absolute form, got: {line!r}"
    assert "(burst)" not in line, f"Must not render (burst) for post-restart fire, got: {line!r}"


def test_render_same_pid_burst_still_burst():
    """Over-relabel guard: a genuine same-PID burst (value >= 10, no post_restart
    marker) must STILL render `(burst)`. The post-restart relabel must not swallow
    real burst detection.

    Passes before AND after the fix — proves the relabel is conditional on the
    post_restart marker, not on value alone.
    """
    r = {
        "contributes_to": "recovery_stalled",
        "state": "firing",
        "value": 15,
    }
    out = render_aggregate([r], watcher_mode=False)
    line = out["recovery_stalled_line"]
    assert line == "recovery_stalled: WARNING delta=15 (burst) — investigating", \
        f"Expected burst form for same-PID burst, got: {line!r}"
    assert "post-restart" not in line, \
        f"Same-PID burst must not be relabeled post-restart, got: {line!r}"


def test_post_restart_below_threshold_does_not_fire():
    """Boundary: a baseline-reset tick whose absolute value is BELOW
    post_restart_absolute_threshold (cur_val < 50) must NOT fire as post-restart;
    it falls through to collecting_baseline.

    Passes before and after — guards the lower boundary of the post-restart check.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "counter_streak_snapshot"
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "111",
            "start_ticks": "100",
            "counter_value": "0",
            "breach_streak": "0",
        })

        alarm = _make_alarm("recovery-stalled", kind="counter-streak",
                            metric="recovery-stalled-metric",
                            delta_threshold=1, streak_threshold=3,
                            burst_threshold=10,
                            post_restart_absolute_threshold=50,
                            severity="WARN")
        # Absolute value 49 < threshold 50 → no post-restart fire.
        current = {"recovery-stalled-metric": [({}, 49.0)]}

        result = eval_counter_streak(alarm, current, state_dir, "222", "200")

        assert result["state"] == "collecting_baseline", \
            f"Below-threshold reset must collect baseline, got {result['state']}"
        assert not result.get("post_restart"), \
            f"Below-threshold reset must not set post_restart, got {result.get('post_restart')!r}"


# ── cold-catchup carveout tests (issue #3816) ────────────────────────────────

def test_post_restart_fire_suppressed_on_cold_catchup():
    """A baseline-reset (PID-change) tick whose absolute counter crosses
    post_restart_absolute_threshold must NOT fire post-restart when the node
    just completed a from-scratch (HAS-restore) catchup this incarnation —
    signalled by stellar_history_bucket_apply_success_total > 0. Being behind
    for minutes across ~10^5 ledgers is the point of a cold catchup, not a stall.

    Fails on origin/main: eval_counter_streak has no `fresh_start` kwarg
    (TypeError) and no cold-catchup gate, so the PID-change branch fires
    post_restart at absolute=63.
    Passes after: the cold-catchup gate returns collecting_baseline.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "counter_streak_snapshot"
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "111",
            "start_ticks": "100",
            "counter_value": "0",
            "breach_streak": "0",
        })

        alarm = _make_alarm("recovery-stalled", kind="counter-streak",
                            metric="recovery-stalled-metric",
                            delta_threshold=1, streak_threshold=3,
                            burst_threshold=10,
                            post_restart_absolute_threshold=50,
                            severity="WARN")
        # cur_val 63 >= threshold 50, AND a cold-catchup bucket-apply happened
        # this incarnation (labeled per-archive series, value >= 1).
        current = {
            "recovery-stalled-metric": [({}, 63.0)],
            "stellar_history_bucket_apply_success_total": [({"archive": "sdf"}, 1.0)],
        }

        result = eval_counter_streak(alarm, current, state_dir, "222", "200",
                                     fresh_start=False)

        assert result["state"] == "collecting_baseline", \
            f"Cold catchup must suppress the post-restart fire, got {result['state']}"
        assert not result.get("post_restart"), \
            f"Cold catchup must not set post_restart, got {result.get('post_restart')!r}"
        assert result.get("cold_catchup_exemption"), \
            f"Cold-catchup suppression must set the exemption marker, got {result.get('cold_catchup_exemption')!r}"

        # Fresh baseline still written before the (suppressed) fire check.
        snap = read_snapshot(snap_path)
        assert snap["pid"] == "222", "fresh baseline pid must be written"
        assert snap["breach_streak"] == "0", "fresh baseline streak must be 0"


def test_render_cold_catchup_exemption_line():
    """render_aggregate labels a collecting_baseline result carrying the
    cold_catchup_exemption marker with the documented exemption suffix.

    Fails on origin/main: the renderer has no such branch and emits the plain
    `recovery_stalled: collecting baseline` line.
    """
    r = {
        "contributes_to": "recovery_stalled",
        "state": "collecting_baseline",
        "cold_catchup_exemption": True,
    }
    out = render_aggregate([r], watcher_mode=False)
    line = out["recovery_stalled_line"]
    assert line == "recovery_stalled: collecting baseline (cold-catchup exemption)", \
        f"Expected cold-catchup exemption form, got: {line!r}"


def test_post_restart_fire_suppressed_on_fresh_start():
    """The fresh_start OR arm: even without the bucket-apply metric on this tick,
    a FRESH_START=yes (state-wipe) tick must suppress the post-restart fire.

    Fails on origin/main: no `fresh_start` kwarg (TypeError) and no gate.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "counter_streak_snapshot"
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "111",
            "start_ticks": "100",
            "counter_value": "0",
            "breach_streak": "0",
        })

        alarm = _make_alarm("recovery-stalled", kind="counter-streak",
                            metric="recovery-stalled-metric",
                            delta_threshold=1, streak_threshold=3,
                            burst_threshold=10,
                            post_restart_absolute_threshold=50,
                            severity="WARN")
        # No bucket-apply series present; fresh_start=True is the only signal.
        current = {"recovery-stalled-metric": [({}, 63.0)]}

        result = eval_counter_streak(alarm, current, state_dir, "222", "200",
                                     fresh_start=True)

        assert result["state"] == "collecting_baseline", \
            f"FRESH_START must suppress the post-restart fire, got {result['state']}"
        assert not result.get("post_restart"), \
            f"FRESH_START must not set post_restart, got {result.get('post_restart')!r}"
        assert result.get("cold_catchup_exemption"), \
            f"FRESH_START suppression must set the exemption marker, got {result.get('cold_catchup_exemption')!r}"


def test_post_restart_fire_still_fires_warm_restart():
    """Guard for #3197/#3198: a warm near-tip restart (no bucket-apply series,
    fresh_start=False) must STILL fire post-restart at absolute >= threshold.
    The carveout must be conditional on the cold-catchup signal, not swallow the
    warm-restart stall detection the check exists to provide.

    Passes before AND after the fix (with the new default kwarg), proving the
    carveout is conditional.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "counter_streak_snapshot"
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "111",
            "start_ticks": "100",
            "counter_value": "0",
            "breach_streak": "0",
        })

        alarm = _make_alarm("recovery-stalled", kind="counter-streak",
                            metric="recovery-stalled-metric",
                            delta_threshold=1, streak_threshold=3,
                            burst_threshold=10,
                            post_restart_absolute_threshold=50,
                            severity="WARN")
        # No bucket-apply series (warm restart, pure ledger-chain replay).
        current = {"recovery-stalled-metric": [({}, 63.0)]}

        result = eval_counter_streak(alarm, current, state_dir, "222", "200",
                                     fresh_start=False)

        assert result["state"] == "firing", \
            f"Warm restart must still fire post-restart, got {result['state']}"
        assert result.get("post_restart") is True, \
            f"Warm restart must set post_restart, got {result.get('post_restart')!r}"
        assert not result.get("cold_catchup_exemption"), \
            f"Warm restart must not set the exemption marker, got {result.get('cold_catchup_exemption')!r}"


# ── missing-process-identity guard tests (issue #3279) ───────────────────────

def test_missing_identity_does_not_poison_snapshot_or_fire():
    """An identity-less tick (empty PID/START_TICKS) must NOT fire post-restart
    and must NOT overwrite the prior valid baseline with an empty-identity one.

    Fails on origin/main: the empty pid "" != snapshot pid "1731959" takes the
    PID-change branch, writes a fresh baseline with pid="" (poison write), then
    maybe_post_restart_fire fires because cur_val (64) >= threshold (50). After
    the fix, the early identity guard returns skipped before any snapshot I/O.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "counter_streak_snapshot"
        # Prior snapshot has a VALID identity and counter.
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "1731959",
            "start_ticks": "802654858",
            "counter_value": "64",
            "breach_streak": "0",
        })

        alarm = _make_alarm("recovery-stalled", kind="counter-streak",
                            metric="recovery-stalled-metric",
                            delta_threshold=1, streak_threshold=3,
                            burst_threshold=10,
                            post_restart_absolute_threshold=50,
                            severity="WARN")
        current = {"recovery-stalled-metric": [({}, 64.0)]}

        # Identity-less tick: empty pid AND empty start_ticks.
        result = eval_counter_streak(alarm, current, state_dir, "", "")

        assert result["state"] == "skipped", \
            f"Identity-less tick must be skipped, got {result['state']}"
        assert result["skip_reason"] == "missing process identity", \
            f"Expected 'missing process identity', got {result['skip_reason']!r}"
        assert not result.get("post_restart"), \
            f"Identity-less tick must NOT post-restart fire, got {result.get('post_restart')!r}"

        # The prior valid baseline must be preserved verbatim (no poison write).
        snap = read_snapshot(snap_path)
        assert snap["pid"] == "1731959", \
            f"prior baseline pid must be preserved, got {snap.get('pid')!r}"
        assert snap["counter_value"] == "64", \
            f"prior counter_value must be preserved, got {snap.get('counter_value')!r}"


def test_empty_identity_snapshot_then_real_tick_no_fire():
    """End-to-end two-tick sequence on a healthy node: an identity-less tick
    (empty PID/START_TICKS) THEN a real-PID tick with a frozen counter (delta=0)
    must report ok, NOT a post-restart fire.

    Fails on origin/main: tick 1 poisons the snapshot to pid="" and itself fires
    post_restart; tick 2's real pid "1731959" != poisoned "" re-enters the
    PID-change branch and false-fires post_restart=True value=64. After the fix
    tick 1 is a skipped no-op that preserves the prior VALID baseline, so tick 2
    sees a stable PID and delta=0 → ok.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "counter_streak_snapshot"
        # Prior valid baseline established by an earlier healthy tick.
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "1731959",
            "start_ticks": "802654858",
            "counter_value": "64",
            "breach_streak": "0",
        })

        alarm = _make_alarm("recovery-stalled", kind="counter-streak",
                            metric="recovery-stalled-metric",
                            delta_threshold=1, streak_threshold=3,
                            burst_threshold=10,
                            post_restart_absolute_threshold=50,
                            severity="WARN")
        # Frozen counter at the same value 64 across both ticks (delta=0).
        current = {"recovery-stalled-metric": [({}, 64.0)]}

        # Tick 1: identity-less (abbreviated tick) — must not poison or fire.
        t1 = eval_counter_streak(alarm, current, state_dir, "", "")
        assert t1["state"] == "skipped", \
            f"Tick 1 (identity-less) must be skipped, got {t1['state']}"
        assert not t1.get("post_restart"), \
            f"Tick 1 must not post-restart fire, got {t1.get('post_restart')!r}"

        # Tick 2: real PID, frozen counter — must report ok, not fire.
        t2 = eval_counter_streak(alarm, current, state_dir, "1731959", "802654858")
        assert t2["state"] == "ok", \
            f"Tick 2 (real PID, frozen counter) must report ok, got {t2['state']}"
        assert not t2.get("post_restart"), \
            f"Tick 2 must not post-restart fire, got {t2.get('post_restart')!r}"


def test_counter_ratio_missing_identity_skips_without_write():
    """eval_counter_ratio with an empty process identity over a valid prior
    ratio_snapshot returns skipped (missing process identity) and leaves the
    snapshot untouched (no pid="" write).

    Fails on origin/main: the empty identity differs from the snapshot pid,
    so the snapshot is invalidated and baselines are rewritten under the empty
    identity. After the fix the early guard returns before the snapshot read.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "ratio_snapshot"
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "1731959",
            "start_ticks": "802654858",
            "myalarm_streak": "2",
            "myalarm_numerator": "90",
            "myalarm_denominator": "100",
        })

        alarm = _make_alarm("myalarm", kind="counter-ratio",
                            numerator="num-metric", denominator="den-metric")
        current = {"num-metric": [({}, 95.0)], "den-metric": [({}, 110.0)]}

        # Identity-less tick: empty pid AND empty start_ticks; uptime large so
        # the uptime<600 skip does not pre-empt the identity guard under test.
        result = eval_counter_ratio(
            alarm, current, {}, state_dir, "", "",
            fresh_start=False, crash_recovery=False, uptime=3600,
        )

        assert result["state"] == "skipped", \
            f"Identity-less ratio tick must be skipped, got {result['state']}"
        assert result["skip_reason"] == "missing process identity", \
            f"Expected 'missing process identity', got {result['skip_reason']!r}"

        # Prior baseline must be preserved verbatim (no empty-identity write).
        snap = read_snapshot(snap_path)
        assert snap["pid"] == "1731959", \
            f"prior ratio baseline pid must be preserved, got {snap.get('pid')!r}"
        assert snap["myalarm_numerator"] == "90", \
            f"prior numerator baseline must be preserved, got {snap.get('myalarm_numerator')!r}"


def test_genuine_restart_still_fires_post_restart():
    """Guard for #3198: a GENUINE restart (real, differing non-empty PID) with
    cur_val >= post_restart_absolute_threshold STILL fires post_restart.

    Passes before and after — the identity guard only short-circuits when the
    identity is EMPTY, never on a real differing identity.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "counter_streak_snapshot"
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "111",
            "start_ticks": "100",
            "counter_value": "0",
            "breach_streak": "0",
        })

        alarm = _make_alarm("recovery-stalled", kind="counter-streak",
                            metric="recovery-stalled-metric",
                            delta_threshold=1, streak_threshold=3,
                            burst_threshold=10,
                            post_restart_absolute_threshold=50,
                            severity="WARN")
        current = {"recovery-stalled-metric": [({}, 736.0)]}

        # Real differing identity "222"/"200" → genuine restart.
        result = eval_counter_streak(alarm, current, state_dir, "222", "200")

        assert result["state"] == "firing", \
            f"Genuine restart must still fire, got {result['state']}"
        assert result["value"] == 736, \
            f"Expected absolute value 736, got {result['value']}"
        assert result.get("post_restart") is True, \
            f"Genuine restart must set post_restart=True, got {result.get('post_restart')!r}"


def test_same_pid_frozen_counter_reports_ok():
    """Guard: a same-PID tick with a frozen counter (delta=0) reports ok — a
    frozen counter on a healthy node is the OPPOSITE of a stall and must not fire.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "counter_streak_snapshot"
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "1731959",
            "start_ticks": "802654858",
            "counter_value": "64",
            "breach_streak": "0",
        })

        alarm = _make_alarm("recovery-stalled", kind="counter-streak",
                            metric="recovery-stalled-metric",
                            delta_threshold=1, streak_threshold=3,
                            burst_threshold=10,
                            post_restart_absolute_threshold=50,
                            severity="WARN")
        current = {"recovery-stalled-metric": [({}, 64.0)]}

        # Same real identity, delta=0.
        result = eval_counter_streak(alarm, current, state_dir, "1731959", "802654858")

        assert result["state"] == "ok", \
            f"Same-PID frozen counter must report ok, got {result['state']}"
        assert not result.get("post_restart"), \
            f"Same-PID frozen counter must not post-restart fire, got {result.get('post_restart')!r}"


# ── missing-process-identity preservation tests (issue #3758) ────────────────

def test_counter_streak_missing_identity_preserves_snapshot():
    """A `missing process identity` skip (#3279) must NOT destroy the
    counter-streak baseline — the guard in eval_counter_streak preserves it
    in-function, so the caller-side cleanup hook must not delete it either.

    Fails on origin/main two ways: (1) the 5-arg call raises TypeError because
    maybe_reset_counter_snapshot has no skip_reason parameter there; and
    (2) semantically, main's reason-blind reset clears the snapshot to {} on
    any skipped counter-streak, erasing the whole breach_streak history.
    Passes after: the identity-less reason is exempted before any write, so the
    snapshot is byte-identical.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "counter_streak_snapshot"
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "1512116",
            "start_ticks": "983181529",
            "counter_value": "3413",
            "breach_streak": "22",
        })
        before = snap_path.read_bytes()

        alarm = _make_alarm("recovery-stalled", kind="counter-streak",
                            metric="recovery-stalled-metric")
        # Identity-less evaluation → skipped, skip_reason="missing process identity".
        result = eval_counter_streak(alarm, {}, state_dir, pid="", start_ticks="")
        assert result["state"] == "skipped"
        assert result["skip_reason"] == "missing process identity"

        # Exactly as the fixed call site (eval-alarms.py:1644) invokes it.
        maybe_reset_counter_snapshot(
            alarm, "counter-streak", result["state"], state_dir,
            result.get("skip_reason"),
        )

        assert snap_path.read_bytes() == before, \
            "identity-less skip must leave the snapshot byte-identical"
        snap = read_snapshot(snap_path)
        assert snap["breach_streak"] == "22", \
            f"breach_streak must be preserved, got {snap.get('breach_streak')!r}"


def test_counter_ratio_missing_identity_preserves_streak():
    """A `missing process identity` skip must not zero the counter-ratio
    `<name>_streak` sub-field either, bringing the sibling family fully in line
    with #3279 (previously it merely "degraded gracefully" by zeroing the streak
    while keeping baselines).

    Fails on origin/main: 5-arg call raises TypeError; and semantically the
    reason-blind reset zeros myalarm_streak. Passes after: exempted before write.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "ratio_snapshot"
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "123",
            "start_ticks": "456",
            "myalarm_streak": "4",
            "myalarm_numerator": "100",
            "myalarm_denominator": "500",
        })

        alarm = _make_alarm("myalarm", kind="counter-ratio")
        maybe_reset_counter_snapshot(
            alarm, "counter-ratio", "skipped", state_dir,
            "missing process identity",
        )

        snap = read_snapshot(snap_path)
        assert snap["myalarm_streak"] == "4", \
            f"streak must be preserved on identity-less skip, got {snap['myalarm_streak']}"
        assert snap["myalarm_numerator"] == "100", "baselines must be preserved"
        assert snap["myalarm_denominator"] == "500", "baselines must be preserved"


def test_counter_streak_gap_stale_still_clears():
    """Control: a genuine coverage-gap skip (gap-stale) MUST still clear the
    counter-streak snapshot. This guards the invariant the hook exists for and
    proves the #3758 fix is a narrow exemption, not a blanket disable.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "counter_streak_snapshot"
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "123",
            "start_ticks": "456",
            "counter_value": "3413",
            "breach_streak": "22",
        })

        alarm = _make_alarm("recovery-stalled", kind="counter-streak")
        maybe_reset_counter_snapshot(
            alarm, "counter-streak", "skipped", state_dir,
            "gap-stale (prev age 5.0h)",
        )

        snap = read_snapshot(snap_path)
        assert len(snap) == 0, \
            f"gap-stale skip must still clear the snapshot, got {snap}"


def test_missing_identity_warning_emitted():
    """An identity-less validator-mode evaluator run must emit a loud stderr
    warning (the issue's "silent, reads as health" harm), and must NOT emit it
    when process identity is present.

    Asserts on a stable substring only (not exact wording) to avoid brittleness.
    Fails on origin/main: no such warning exists.
    """
    eval_script = Path(__file__).parent / "eval-alarms.py"
    WARN_SUBSTR = "missing process identity"

    def run(env_extra):
        with tempfile.TemporaryDirectory() as d:
            state_dir = Path(d)
            catalog = state_dir / "catalog.toml"
            catalog.write_text("schema_version = 1\n")
            current = state_dir / "current.prom"
            current.write_text("# no metrics\n")

            env = dict(os.environ)
            env.pop("PID", None)
            env.pop("START_TICKS", None)
            env.update({
                "MONITOR_MODE": "validator",
                "FRESH_START": "no",
                "CRASH_RECOVERY": "no",
                "WARMUP_TICKS_REMAINING": "0",
                "UPTIME_SECONDS": "999999",
                "PREV_PROM_INVALID": "false",
            })
            env.update(env_extra)

            proc = subprocess.run(
                [sys.executable, str(eval_script),
                 "--catalog", str(catalog),
                 "--current", str(current),
                 "--state-dir", str(state_dir)],
                capture_output=True, text=True, env=env,
            )
            return proc.stderr

    # Identity-less validator tick → warning fires.
    stderr_missing = run({})
    assert WARN_SUBSTR in stderr_missing, \
        f"expected identity warning in stderr, got: {stderr_missing!r}"

    # Identity present → warning must NOT fire.
    stderr_present = run({"PID": "1512116", "START_TICKS": "983181529"})
    assert WARN_SUBSTR not in stderr_present, \
        f"warning must not fire when identity is present, got: {stderr_present!r}"


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
