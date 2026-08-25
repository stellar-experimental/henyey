#!/usr/bin/env python3
"""Regression tests for the recovery-stalled family-union re-key (issue #3824).

Check 12b (`recovery-stalled` counter-streak) historically observed a single
`reason` series (`forcing_catchup_behind`) of the 8-label
`henyey_recovery_stalled_tick_total` family. During an at-tip stall the node
takes the `forcing_catchup_not_behind` branch by construction, so the one label
the alarm watched was exactly the branch that could not move — a real recovery
episode incremented two UNCOVERED labels and the tick reported `ok (delta=0)`.

The fix re-keys the delta/streak/burst trigger onto the SUM of all `reason`
series (`extraction = "form2-sum-all"`), while scoping the post-restart absolute
guard to a single historically-calibrated label via `post_restart_absolute_label`
(so a summed warmup value of ~113 does not false-fire the absolute check tuned
for `forcing_catchup_behind` alone). A per-reason `reason_breakdown` is attached
on breach/firing so a summed fire still names the moving labels.
"""

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
eval_counter_streak = _mod.eval_counter_streak
render_aggregate = _mod.render_aggregate
validate_catalog = _mod.validate_catalog


def _make_alarm(name="recovery-stalled", **kwargs):
    alarm = {"name": name, "kind": "counter-streak"}
    alarm.update(kwargs)
    return alarm


def _family(behind, not_behind, peer_scp):
    """Build a current-metrics dict for the recovery family with 3 reasons set."""
    return {
        "henyey_recovery_stalled_tick_total": [
            ({"reason": "forcing_catchup_behind"}, float(behind)),
            ({"reason": "forcing_catchup_not_behind"}, float(not_behind)),
            ({"reason": "near_tip_peer_scp_recovery"}, float(peer_scp)),
        ]
    }


# ── post-restart absolute guard is scoped to a single label, not the sum ──────

def test_post_restart_absolute_uses_label_not_sum():
    """On a baseline reset, the post-restart absolute guard must evaluate only
    the `post_restart_absolute_label` series, NOT the family sum.

    forcing_catchup_behind == 40 (< 50 threshold) while the family sums to 120
    (> 50). The correct behaviour is `collecting_baseline` (no post-restart
    fire), because the historically-calibrated absolute signal (#3197/#3198) is
    the single label, not the aggregate.

    Fails on origin/main: `post_restart_absolute_label` is unhandled, so under a
    `form2-sum-all` extraction the aggregate 120 >= 50 fires as post-restart.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "recovery_family_streak_snapshot"
        # Existing baseline under a DIFFERENT pid → next eval is a baseline reset.
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "111",
            "start_ticks": "100",
            "counter_value": "0",
            "breach_streak": "0",
        })

        alarm = _make_alarm(
            metric="henyey_recovery_stalled_tick_total",
            extraction="form2-sum-all",
            delta_threshold=1, streak_threshold=3, burst_threshold=10,
            post_restart_absolute_threshold=50,
            post_restart_absolute_label="forcing_catchup_behind",
            snapshot_file="recovery_family_streak_snapshot",
            severity="WARN",
        )
        # behind=40 (<50), family sum = 40 + 50 + 30 = 120 (>50).
        current = _family(behind=40, not_behind=50, peer_scp=30)

        result = eval_counter_streak(
            alarm, current, state_dir, "222", "200", prev=None,
        )

        assert result["state"] == "collecting_baseline", (
            "post-restart guard must use the single label (40 < 50), not the "
            f"family sum (120 >= 50); got state={result['state']}"
        )


def test_post_restart_absolute_label_fires_on_label_value():
    """Complementary case: when the scoped label itself crosses the threshold,
    the post-restart absolute fire still triggers (guard not disabled)."""
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "recovery_family_streak_snapshot"
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "111",
            "start_ticks": "100",
            "counter_value": "0",
            "breach_streak": "0",
        })

        alarm = _make_alarm(
            metric="henyey_recovery_stalled_tick_total",
            extraction="form2-sum-all",
            delta_threshold=1, streak_threshold=3, burst_threshold=10,
            post_restart_absolute_threshold=50,
            post_restart_absolute_label="forcing_catchup_behind",
            snapshot_file="recovery_family_streak_snapshot",
            severity="WARN",
        )
        # behind=63 (>= 50) → post-restart fire on the scoped label.
        current = _family(behind=63, not_behind=1, peer_scp=49)

        result = eval_counter_streak(
            alarm, current, state_dir, "222", "200", prev=None,
        )

        assert result["state"] == "firing", (
            f"scoped label 63 >= 50 must post-restart fire, got {result['state']}"
        )
        assert result.get("post_restart") is True
        # The fresh baseline snapshot stores the SUM (the streak machine's unit).
        snap = read_snapshot(snap_path)
        assert snap["counter_value"] == "113", (
            f"baseline must snapshot the family sum 113, got {snap['counter_value']}"
        )


def test_snapshot_file_rename_rebaselines_without_post_restart_fire():
    """Baseline migration via snapshot_file rename (#3222 lever): when only the
    OLD snapshot filename is present, the first post-migration tick re-collects
    a fresh baseline (`collecting_baseline`) — NOT a post-restart fire — even
    when the scoped label already exceeds the absolute threshold.

    The empty-snapshot first-tick branch does not invoke the post-restart path,
    so renaming the file avoids the spurious post-restart fire a version bump
    would cause on a long-running process.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        # Only the OLD filename exists; the new path is absent.
        old_path = state_dir / "counter_streak_snapshot"
        write_snapshot(old_path, {
            "version": "1",
            "pid": "222",
            "start_ticks": "200",
            "counter_value": "500",
            "breach_streak": "3",
        })

        alarm = _make_alarm(
            metric="henyey_recovery_stalled_tick_total",
            extraction="form2-sum-all",
            delta_threshold=1, streak_threshold=3, burst_threshold=10,
            post_restart_absolute_threshold=50,
            post_restart_absolute_label="forcing_catchup_behind",
            snapshot_file="recovery_family_streak_snapshot",
            severity="WARN",
        )
        # Same pid/start_ticks as the old snapshot; scoped label >= 50.
        current = _family(behind=63, not_behind=1, peer_scp=50)

        result = eval_counter_streak(
            alarm, current, state_dir, "222", "200", prev=None,
        )

        assert result["state"] == "collecting_baseline", (
            "rename must re-baseline cleanly (empty new-path snapshot), not "
            f"post-restart fire; got {result['state']}"
        )
        # New-path baseline written with the family sum.
        snap = read_snapshot(state_dir / "recovery_family_streak_snapshot")
        assert snap["counter_value"] == "114"


# ── the trigger fires on the family sum (the branch that actually moves) ───────

def test_at_tip_stall_fires_on_family_sum():
    """An at-tip stall increments `not_behind` + `near_tip_peer_scp_recovery`
    while `forcing_catchup_behind` barely moves. Summing the family makes the
    burst trigger observe the branch that moves: delta 161 >= burst 10 → fire.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "recovery_family_streak_snapshot"
        # Baseline sum = 63 + 1 + 50 = 114.
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "222",
            "start_ticks": "200",
            "counter_value": "114",
            "breach_streak": "0",
        })

        alarm = _make_alarm(
            metric="henyey_recovery_stalled_tick_total",
            extraction="form2-sum-all",
            delta_threshold=1, streak_threshold=3, burst_threshold=10,
            post_restart_absolute_threshold=50,
            post_restart_absolute_label="forcing_catchup_behind",
            snapshot_file="recovery_family_streak_snapshot",
            severity="WARN",
        )
        prev = _family(behind=63, not_behind=1, peer_scp=50)
        # not_behind 1→81, peer_scp 50→130, behind 63→64 → sum 275, delta 161.
        current = _family(behind=64, not_behind=81, peer_scp=130)

        result = eval_counter_streak(
            alarm, current, state_dir, "222", "200", prev=prev,
        )

        assert result["state"] == "firing", (
            f"family sum delta 161 >= burst 10 must fire, got {result['state']}"
        )
        assert result["value"] == 161, f"expected delta 161, got {result['value']}"


def test_reason_breakdown_names_moving_labels():
    """On a burst fire, the result carries `reason_breakdown` naming the moved
    reasons with their deltas and omitting flat ones."""
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        snap_path = state_dir / "recovery_family_streak_snapshot"
        write_snapshot(snap_path, {
            "version": "1",
            "pid": "222",
            "start_ticks": "200",
            "counter_value": "114",
            "breach_streak": "0",
        })

        alarm = _make_alarm(
            metric="henyey_recovery_stalled_tick_total",
            extraction="form2-sum-all",
            delta_threshold=1, streak_threshold=3, burst_threshold=10,
            post_restart_absolute_threshold=50,
            post_restart_absolute_label="forcing_catchup_behind",
            snapshot_file="recovery_family_streak_snapshot",
            severity="WARN",
        )
        prev = _family(behind=63, not_behind=1, peer_scp=50)
        current = _family(behind=63, not_behind=81, peer_scp=130)

        result = eval_counter_streak(
            alarm, current, state_dir, "222", "200", prev=prev,
        )

        assert result["state"] == "firing"
        breakdown = result.get("reason_breakdown")
        assert breakdown, f"expected reason_breakdown, got {breakdown!r}"
        moved = {b["reason"]: b["delta"] for b in breakdown}
        assert moved.get("forcing_catchup_not_behind") == 80, moved
        assert moved.get("near_tip_peer_scp_recovery") == 80, moved
        # forcing_catchup_behind was flat (63→63) — omitted.
        assert "forcing_catchup_behind" not in moved, moved


def test_render_breakdown_appended_to_line():
    """render_aggregate appends the per-reason breakdown to the recovery_stalled
    line so a summed fire names the moving labels."""
    r = {
        "contributes_to": "recovery_stalled",
        "state": "firing",
        "value": 160,
        "post_restart": False,
        "reason_breakdown": [
            {"reason": "forcing_catchup_not_behind", "delta": 80},
            {"reason": "near_tip_peer_scp_recovery", "delta": 80},
        ],
    }
    out = render_aggregate([r], watcher_mode=False)
    line = out["recovery_stalled_line"]
    assert "delta=160" in line and "(burst)" in line, line
    assert "forcing_catchup_not_behind+80" in line, line
    assert "near_tip_peer_scp_recovery+80" in line, line


# ── catalog validation ────────────────────────────────────────────────────────

def test_validate_catalog_rejects_non_string_post_restart_label():
    """A non-string post_restart_absolute_label is a schema error."""
    catalog = {
        "schema_version": _mod.SCHEMA_VERSION,
        "alarm": [{
            "name": "recovery-stalled",
            "kind": "counter-streak",
            "metric": "henyey_recovery_stalled_tick_total",
            "severity": "WARN",
            "delta_threshold": 1,
            "streak_threshold": 3,
            "burst_threshold": 10,
            "post_restart_absolute_label": 123,  # not a string
        }],
    }
    errors = validate_catalog(catalog)
    assert any("post_restart_absolute_label" in e for e in errors), (
        f"expected a post_restart_absolute_label type error, got {errors}"
    )


def test_validate_catalog_accepts_string_post_restart_label():
    catalog = {
        "schema_version": _mod.SCHEMA_VERSION,
        "alarm": [{
            "name": "recovery-stalled",
            "kind": "counter-streak",
            "metric": "henyey_recovery_stalled_tick_total",
            "severity": "WARN",
            "delta_threshold": 1,
            "streak_threshold": 3,
            "burst_threshold": 10,
            "post_restart_absolute_label": "forcing_catchup_behind",
        }],
    }
    errors = validate_catalog(catalog)
    assert not any("post_restart_absolute_label" in e for e in errors), errors


if __name__ == "__main__":
    import sys
    fns = [v for k, v in sorted(globals().items()) if k.startswith("test_") and callable(v)]
    failed = 0
    for fn in fns:
        try:
            fn()
            print(f"ok - {fn.__name__}")
        except Exception as e:  # noqa: BLE001
            failed += 1
            print(f"not ok - {fn.__name__}: {e}")
    sys.exit(1 if failed else 0)
