#!/usr/bin/env python3
"""Regression + unit tests for mechanized alarm-cooldown persistence (issue #3762).

Before this change, `anomaly_cooldown.json` (the monitor's alarm-dedup state)
was written by a hand-performed JSON edit in the tick procedure — a step that
failed silently whenever an agent skipped it, most often on an alarm's *first*
fire. These tests pin the mechanized behavior: `eval-alarms.py --cooldown-file`
now owns the read/suppress/write lifecycle for the metrics family.

Covers:
  - firing alarm persists `last_fired` to the cooldown file,
  - an in-window firing alarm is suppressed to state="cooldown" (not re-filed),
  - a past-window firing alarm re-fires and bumps `last_fired`,
  - the mixed bare-int / dict schema is normalized on write,
  - `--no-snapshot-write` leaves the cooldown file untouched,
  - ratio/streak (non-metrics) families are never flipped to cooldown,
  - unit-level normalization + the `now - last_fired == cooldown_seconds`
    boundary (not suppressed).
"""

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

# eval-alarms.py uses a hyphen, so we need importlib.
import importlib.util

_spec = importlib.util.spec_from_file_location(
    "eval_alarms",
    Path(__file__).parent / "eval-alarms.py",
)
_mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_mod)

EVAL_SCRIPT = Path(__file__).parent / "eval-alarms.py"


# ── Fixtures ─────────────────────────────────────────────────────────────────

_GAUGE_CATALOG = """\
schema_version = 1

[[alarm]]
name = "test-gauge"
metric = "test_metric"
kind = "gauge"
extraction = "form1"
labels = []
op = ">"
threshold = 10
for_ticks = 1
severity = "WARN"
gates = []
cooldown_key = "test_metric"
cooldown_seconds = 3600
filing_title = "metrics: test_metric — {value} > {threshold}"
filing_search = "metrics: test_metric"
summary = "test gauge firing"
details = "value={value} threshold={threshold}"
notes = ""
"""

# A firing gauge (value 15 > threshold 10) alongside a firing counter-streak so
# we can assert the cooldown pass is scoped to the metrics family only.
_STREAK_CATALOG = """\
schema_version = 1

[[alarm]]
name = "test-gauge"
metric = "test_metric"
kind = "gauge"
extraction = "form1"
labels = []
op = ">"
threshold = 10
for_ticks = 1
severity = "WARN"
gates = []
cooldown_key = "test_metric"
cooldown_seconds = 3600
filing_title = "metrics: test_metric"
filing_search = "metrics: test_metric"
summary = "test gauge firing"
details = "value={value}"
notes = ""

[[alarm]]
name = "test-streak"
metric = "streak_metric"
kind = "counter-streak"
delta_threshold = 1
streak_threshold = 3
burst_threshold = 10
severity = "WARN"
gates = []
cooldown_key = "streak_metric"
cooldown_seconds = 3600
filing_title = "recovery_stalled: streak_metric"
filing_search = "recovery_stalled: streak_metric"
summary = "streak firing"
details = "delta={value}"
notes = ""
"""


def _run_eval(state_dir, catalog_text, current_text, cooldown_path=None,
              now=None, extra_args=None, extra_env=None):
    """Run eval-alarms.py as a subprocess; return (proc, parsed_json_or_None)."""
    catalog = state_dir / "catalog.toml"
    catalog.write_text(catalog_text)
    current = state_dir / "current.prom"
    current.write_text(current_text)

    env = dict(os.environ)
    env.update({
        "MONITOR_MODE": "validator",
        "FRESH_START": "no",
        "CRASH_RECOVERY": "no",
        "WARMUP_TICKS_REMAINING": "0",
        "UPTIME_SECONDS": "999999",
        "PREV_PROM_INVALID": "false",
        "PID": "1512116",
        "START_TICKS": "983181529",
    })
    if extra_env:
        env.update(extra_env)

    cmd = [sys.executable, str(EVAL_SCRIPT),
           "--catalog", str(catalog),
           "--current", str(current),
           "--state-dir", str(state_dir)]
    if cooldown_path is not None:
        cmd += ["--cooldown-file", str(cooldown_path)]
    if now is not None:
        cmd += ["--now", str(now)]
    if extra_args:
        cmd += extra_args

    proc = subprocess.run(cmd, capture_output=True, text=True, env=env)
    parsed = None
    if proc.stdout.strip():
        try:
            parsed = json.loads(proc.stdout)
        except json.JSONDecodeError:
            parsed = None
    return proc, parsed


def _alarm(parsed, name):
    for a in parsed["alarms"]:
        if a["name"] == name:
            return a
    raise AssertionError(f"alarm {name!r} not in output")


# ── Full-subprocess regression tests ─────────────────────────────────────────

def test_firing_alarm_persists_last_fired():
    """A firing gauge with an empty cooldown file writes last_fired for its key.

    Fails on main: --cooldown-file is unsupported, so the file is never written.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        cooldown = state_dir / "anomaly_cooldown.json"
        now = 1_800_000_000
        proc, parsed = _run_eval(state_dir, _GAUGE_CATALOG, "test_metric 15\n",
                                 cooldown_path=cooldown, now=now)
        assert proc.returncode == 0, f"eval failed: {proc.stderr}"
        assert _alarm(parsed, "test-gauge")["state"] == "firing"
        assert cooldown.exists(), "cooldown file must be written"
        data = json.loads(cooldown.read_text())
        assert "test_metric" in data, f"missing cooldown key, got {data}"
        assert data["test_metric"]["last_fired"] == now
        assert data["test_metric"]["cooldown_seconds"] == 3600


def test_in_window_alarm_suppressed_to_cooldown():
    """A firing alarm still inside its window is suppressed to state=cooldown,
    excluded from the firing count, and its last_fired is NOT bumped.

    Fails on main: no suppression path exists.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        cooldown = state_dir / "anomaly_cooldown.json"
        now = 1_800_000_000
        seeded = now - 60
        cooldown.write_text(json.dumps(
            {"test_metric": {"last_fired": seeded, "cooldown_seconds": 3600}}))
        proc, parsed = _run_eval(state_dir, _GAUGE_CATALOG, "test_metric 15\n",
                                 cooldown_path=cooldown, now=now)
        assert proc.returncode == 0, f"eval failed: {proc.stderr}"
        a = _alarm(parsed, "test-gauge")
        assert a["state"] == "cooldown", f"expected cooldown, got {a['state']}"
        assert "0/1 firing" in parsed["aggregate"]["metrics_line"], \
            parsed["aggregate"]["metrics_line"]
        assert "in cooldown" in parsed["aggregate"]["metrics_line"]
        data = json.loads(cooldown.read_text())
        assert data["test_metric"]["last_fired"] == seeded, \
            "last_fired must not be bumped while suppressed"


def test_past_window_alarm_refires_and_updates():
    """A firing alarm past its window re-fires and bumps last_fired to now.

    Fails on main: no write path.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        cooldown = state_dir / "anomaly_cooldown.json"
        now = 1_800_000_000
        cooldown.write_text(json.dumps(
            {"test_metric": {"last_fired": now - 100_000, "cooldown_seconds": 3600}}))
        proc, parsed = _run_eval(state_dir, _GAUGE_CATALOG, "test_metric 15\n",
                                 cooldown_path=cooldown, now=now)
        assert proc.returncode == 0, f"eval failed: {proc.stderr}"
        assert _alarm(parsed, "test-gauge")["state"] == "firing"
        data = json.loads(cooldown.read_text())
        assert data["test_metric"]["last_fired"] == now, "last_fired must be bumped"


def test_mixed_schema_normalized():
    """A mixed bare-int + dict cooldown file is normalized to dict form on
    write, and an unrelated key is preserved.

    Fails on main: the file is never opened.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        cooldown = state_dir / "anomaly_cooldown.json"
        now = 1_800_000_000
        cooldown.write_text(json.dumps({
            "test_metric": now - 100_000,                       # bare int → refires
            "overlay_error_total": 1_780_859_737,               # bare int, untouched
            "other_alarm": {"last_fired": 12345, "cooldown_seconds": 3600},
        }))
        proc, parsed = _run_eval(state_dir, _GAUGE_CATALOG, "test_metric 15\n",
                                 cooldown_path=cooldown, now=now)
        assert proc.returncode == 0, f"eval failed: {proc.stderr}"
        data = json.loads(cooldown.read_text())
        # All values normalized to dict form.
        for k, v in data.items():
            assert isinstance(v, dict), f"{k} not normalized: {v!r}"
            assert "last_fired" in v, f"{k} missing last_fired: {v!r}"
        # Unrelated keys preserved.
        assert data["overlay_error_total"]["last_fired"] == 1_780_859_737
        assert data["other_alarm"]["last_fired"] == 12345
        # The firing key bumped to now.
        assert data["test_metric"]["last_fired"] == now


def test_no_snapshot_write_leaves_file_untouched():
    """--no-snapshot-write must leave the cooldown file byte-identical.

    Fails on main: --cooldown-file / --no-snapshot-write cooldown gating absent.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        cooldown = state_dir / "anomaly_cooldown.json"
        now = 1_800_000_000
        original = json.dumps(
            {"test_metric": {"last_fired": now - 100_000, "cooldown_seconds": 3600}})
        cooldown.write_text(original)
        before = cooldown.read_bytes()
        proc, parsed = _run_eval(state_dir, _GAUGE_CATALOG, "test_metric 15\n",
                                 cooldown_path=cooldown, now=now,
                                 extra_args=["--no-snapshot-write"])
        assert proc.returncode == 0, f"eval failed: {proc.stderr}"
        assert cooldown.read_bytes() == before, "cooldown file must be untouched"


def test_ratio_streak_alarm_not_touched():
    """A firing counter-streak (non-metrics family) keeps state=firing and is
    NOT written to the cooldown file — the pass is scoped to metrics only.

    Guards the Critic-A scope restriction. Fails on main: no cooldown pass.
    """
    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d)
        cooldown = state_dir / "anomaly_cooldown.json"
        now = 1_800_000_000
        current = "test_metric 15\nstreak_metric 500\n"
        proc, parsed = _run_eval(state_dir, _STREAK_CATALOG, current,
                                 cooldown_path=cooldown, now=now)
        assert proc.returncode == 0, f"eval failed: {proc.stderr}"
        streak = _alarm(parsed, "test-streak")
        # counter-streak on its first observation collects a baseline; whatever
        # state it lands in, it must never be flipped to "cooldown".
        assert streak["state"] != "cooldown", \
            f"streak family must not be suppressed, got {streak['state']}"
        if cooldown.exists():
            data = json.loads(cooldown.read_text())
            assert "streak_metric" not in data, \
                "streak family must not be persisted to the cooldown file"


# ── Unit tests (importlib) ───────────────────────────────────────────────────

def _result(name, state, cooldown_key, cooldown_seconds=3600,
            contributes_to="metrics"):
    return {
        "name": name,
        "state": state,
        "cooldown_key": cooldown_key,
        "cooldown_seconds": cooldown_seconds,
        "contributes_to": contributes_to,
    }


def test_read_cooldown_file_normalizes():
    """read_cooldown_file wraps bare ints and passes dicts through; missing
    file returns {}."""
    with tempfile.TemporaryDirectory() as d:
        path = Path(d) / "cd.json"
        assert _mod.read_cooldown_file(path) == {}
        path.write_text(json.dumps({
            "a": 100,
            "b": {"last_fired": 200, "cooldown_seconds": 3600},
        }))
        data = _mod.read_cooldown_file(path)
        assert data["a"] == {"last_fired": 100}
        assert data["b"] == {"last_fired": 200, "cooldown_seconds": 3600}


def test_write_cooldown_file_emits_dict_form():
    """write_cooldown_file normalizes any bare int to dict form on disk."""
    with tempfile.TemporaryDirectory() as d:
        path = Path(d) / "cd.json"
        _mod.write_cooldown_file(path, {"a": 100, "b": {"last_fired": 200}})
        data = json.loads(path.read_text())
        assert data["a"] == {"last_fired": 100}
        assert data["b"] == {"last_fired": 200}


def test_apply_cooldowns_boundary_not_suppressed():
    """now - last_fired == cooldown_seconds is exactly at the edge and must
    re-fire (strict < window)."""
    now = 1_000_000
    data = {"k": {"last_fired": now - 3600, "cooldown_seconds": 3600}}
    results = [_result("g", "firing", "k", 3600)]
    _mod.apply_cooldowns(results, data, now)
    assert results[0]["state"] == "firing", "boundary must re-fire"
    assert data["k"]["last_fired"] == now


def test_apply_cooldowns_inside_window_suppressed():
    """Inside the window: suppress to cooldown, keep last_fired."""
    now = 1_000_000
    data = {"k": {"last_fired": now - 10, "cooldown_seconds": 3600}}
    results = [_result("g", "firing", "k", 3600)]
    _mod.apply_cooldowns(results, data, now)
    assert results[0]["state"] == "cooldown"
    assert results[0]["cooldown_remaining_seconds"] == 3590
    assert data["k"]["last_fired"] == now - 10


def test_apply_cooldowns_scoped_to_metrics():
    """Non-metrics families are never suppressed and never persisted."""
    now = 1_000_000
    data = {}
    results = [_result("s", "firing", "k", 3600, contributes_to="recovery_stalled")]
    _mod.apply_cooldowns(results, data, now)
    assert results[0]["state"] == "firing"
    assert "k" not in data


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
