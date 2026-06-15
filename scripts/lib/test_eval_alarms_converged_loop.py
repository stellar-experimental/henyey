#!/usr/bin/env python3
"""Regression tests for the converged main-loop post-processing (issue #2618).

Tests verify that after refactoring the main loop to a single converged
result path, all skip branches (exempt, gate-skip) properly reset stateful
evaluator state and emit unified telemetry.
"""

import io
import json
import os
import sys
import tempfile
from pathlib import Path
from unittest.mock import patch

try:
    import tomli_w
except ImportError:
    tomli_w = None

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
maybe_reset_gauge_persistence = _mod.maybe_reset_gauge_persistence
maybe_reset_counter_snapshot = _mod.maybe_reset_counter_snapshot
main = _mod.main


def _write_toml(path: Path, data: dict):
    """Write a TOML file from a dict, using tomli_w if available, else manual format."""
    if tomli_w is not None:
        with open(path, "wb") as f:
            tomli_w.dump(data, f)
        return
    # Manual TOML serialization for alarm catalogs
    lines = []
    # Top-level keys first
    for k, v in data.items():
        if k == "alarm":
            continue
        if isinstance(v, int):
            lines.append(f'{k} = {v}')
        elif isinstance(v, str):
            lines.append(f'{k} = "{v}"')
    for alarm in data.get("alarm", []):
        lines.append("[[alarm]]")
        for k, v in alarm.items():
            if isinstance(v, bool):
                lines.append(f'{k} = {"true" if v else "false"}')
            elif isinstance(v, str):
                lines.append(f'{k} = "{v}"')
            elif isinstance(v, int):
                lines.append(f'{k} = {v}')
            elif isinstance(v, float):
                lines.append(f'{k} = {v}')
            elif isinstance(v, list):
                items = ", ".join(f'"{x}"' if isinstance(x, str) else str(x) for x in v)
                lines.append(f'{k} = [{items}]')
        lines.append("")
    path.write_text("\n".join(lines) + "\n")


# ── Helpers ──────────────────────────────────────────────────────────────────

def _write_prom(path: Path, lines: list[str]):
    """Write a .prom file with the given metric lines."""
    path.write_text("\n".join(lines) + "\n")


def _run_main(catalog: dict, current_lines: list[str], prev_lines: list[str] | None = None,
              env_overrides: dict | None = None, state_dir: Path | None = None,
              pre_gauge_persistence: dict | None = None,
              pre_counter_dynamic_snapshot: dict | None = None):
    """Run main() with the given catalog and metrics, return (json_output, stderr_output, state_dir)."""
    with tempfile.TemporaryDirectory() as d:
        d = Path(d)
        _state_dir = state_dir or (d / "state")
        _state_dir.mkdir(parents=True, exist_ok=True)

        # Write catalog
        catalog_path = d / "catalog.toml"
        _write_toml(catalog_path, catalog)

        # Write prom files
        current_path = d / "current.prom"
        _write_prom(current_path, current_lines)

        prev_path = None
        if prev_lines is not None:
            prev_path = d / "prev.prom"
            _write_prom(prev_path, prev_lines)

        # Pre-seed state files
        if pre_gauge_persistence:
            write_snapshot(_state_dir / "gauge_persistence", pre_gauge_persistence)
        if pre_counter_dynamic_snapshot:
            write_snapshot(_state_dir / "counter_dynamic_snapshot", pre_counter_dynamic_snapshot)

        # Build argv
        argv = [
            "eval-alarms",
            "--catalog", str(catalog_path),
            "--current", str(current_path),
            "--state-dir", str(_state_dir),
        ]
        if prev_path:
            argv.extend(["--prev", str(prev_path)])

        # Set env
        env = {
            "PREV_PROM_INVALID": "false",
            "WARMUP_TICKS_REMAINING": "0",
            "FRESH_START": "no",
            "CRASH_RECOVERY": "no",
            "UPTIME_SECONDS": "9999",
            "MONITOR_MODE": "validator",
            "PID": "123",
            "START_TICKS": "456",
        }
        if env_overrides:
            env.update(env_overrides)

        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()

        with patch.object(sys, "argv", argv), \
             patch.object(sys, "stdout", stdout_buf), \
             patch.object(sys, "stderr", stderr_buf), \
             patch.dict(os.environ, env, clear=False):
            rc = main()

        assert rc == 0, f"main() returned {rc}, stderr: {stderr_buf.getvalue()}"

        output = json.loads(stdout_buf.getvalue())
        stderr_text = stderr_buf.getvalue()

        return output, stderr_text, _state_dir


# ── Test: exempt alarm resets gauge persistence ──────────────────────────────

def test_exempt_resets_gauge_persistence():
    """An exempt gauge alarm with for_ticks > 1 resets persistence via converged block."""
    catalog = {
        "schema_version": 1,
        "alarm": [{
            "name": "test-gauge",
            "kind": "gauge",
            "severity": "WARN",
            "metric": "some_gauge",
            "op": ">",
            "threshold": 100,
            "for_ticks": 3,
            "exempt": True,
            "exempt_reason": "maintenance window",
        }]
    }

    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d) / "state"
        state_dir.mkdir(parents=True, exist_ok=True)

        # Pre-seed gauge persistence (simulating prior ticks had accumulated)
        write_snapshot(state_dir / "gauge_persistence", {
            "gauge_persist_test-gauge": "2",
        })

        output, stderr, _ = _run_main(
            catalog,
            current_lines=["some_gauge 150"],
            state_dir=state_dir,
        )

        # Verify the alarm was skipped
        assert output["alarms"][0]["state"] == "skipped"
        assert "exempt" in output["alarms"][0]["skip_reason"]

        # Verify gauge persistence was reset
        persist = read_snapshot(state_dir / "gauge_persistence")
        assert persist.get("gauge_persist_test-gauge") == "0", \
            f"Expected gauge persistence reset to 0, got {persist}"


# ── Test: exempt alarm resets counter-dynamic snapshot ───────────────────────

def test_exempt_resets_counter_dynamic_snapshot():
    """An exempt counter-dynamic alarm with pre-existing prior_delta resets it."""
    catalog = {
        "schema_version": 1,
        "alarm": [{
            "name": "test-cdyn",
            "kind": "counter-dynamic",
            "severity": "WARN",
            "metric_sum": ["some_counter_total"],
            "multiplier": 3.0,
            "exempt": True,
            "exempt_reason": "testing",
        }]
    }

    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d) / "state"
        state_dir.mkdir(parents=True, exist_ok=True)

        # Pre-seed counter dynamic snapshot
        write_snapshot(state_dir / "counter_dynamic_snapshot", {
            "prior_delta_test-cdyn": "42",
            "other_key": "keep",
        })

        output, stderr, _ = _run_main(
            catalog,
            current_lines=["some_counter_total 100"],
            state_dir=state_dir,
        )

        assert output["alarms"][0]["state"] == "skipped"

        # Verify prior_delta was cleared but other keys preserved
        snap = read_snapshot(state_dir / "counter_dynamic_snapshot")
        assert "prior_delta_test-cdyn" not in snap, \
            f"Expected prior_delta cleared, got {snap}"
        assert snap.get("other_key") == "keep"


# ── Test: exempt precedence over gate-skip ───────────────────────────────────

def test_exempt_precedence_over_gates():
    """An alarm with exempt=true AND failing gates should skip as exempt, not gate-skip."""
    catalog = {
        "schema_version": 1,
        "alarm": [{
            "name": "test-prec",
            "kind": "gauge",
            "severity": "WARN",
            "metric": "some_metric",
            "op": ">",
            "threshold": 10,
            "exempt": True,
            "exempt_reason": "maintenance",
            "gates": ["warmup-2-ticks"],
        }]
    }

    output, stderr, _ = _run_main(
        catalog,
        current_lines=["some_metric 50"],
        env_overrides={"WARMUP_TICKS_REMAINING": "5"},
    )

    alarm_result = output["alarms"][0]
    assert alarm_result["state"] == "skipped"
    assert alarm_result["skip_reason"].startswith("exempt:"), \
        f"Expected skip_reason to start with 'exempt:', got '{alarm_result['skip_reason']}'"


# ── Test: gate-skip resets counter snapshot ──────────────────────────────────

def test_gate_skip_resets_counter_dynamic_snapshot():
    """A gate-skipped counter-dynamic alarm resets prior_delta via converged block."""
    catalog = {
        "schema_version": 1,
        "alarm": [{
            "name": "test-gate-cdyn",
            "kind": "counter-dynamic",
            "severity": "WARN",
            "metric_sum": ["some_counter_total"],
            "multiplier": 3.0,
            "gates": ["warmup-2-ticks"],
        }]
    }

    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d) / "state"
        state_dir.mkdir(parents=True, exist_ok=True)

        # Pre-seed snapshot
        write_snapshot(state_dir / "counter_dynamic_snapshot", {
            "prior_delta_test-gate-cdyn": "99",
        })

        output, stderr, _ = _run_main(
            catalog,
            current_lines=["some_counter_total 100"],
            env_overrides={"WARMUP_TICKS_REMAINING": "3"},
            state_dir=state_dir,
        )

        assert output["alarms"][0]["state"] == "skipped"
        assert "warmup" in output["alarms"][0]["skip_reason"]

        # Verify prior_delta was cleared
        snap = read_snapshot(state_dir / "counter_dynamic_snapshot")
        assert "prior_delta_test-gate-cdyn" not in snap, \
            f"Expected prior_delta cleared on gate-skip, got {snap}"


# ── Test: gate-skip resets gauge persistence ─────────────────────────────────

def test_gate_skip_resets_gauge_persistence():
    """A gate-skipped gauge alarm with for_ticks > 1 resets persistence."""
    catalog = {
        "schema_version": 1,
        "alarm": [{
            "name": "test-gate-gauge",
            "kind": "gauge",
            "severity": "WARN",
            "metric": "some_gauge",
            "op": ">",
            "threshold": 100,
            "for_ticks": 3,
            "gates": ["warmup-2-ticks"],
        }]
    }

    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d) / "state"
        state_dir.mkdir(parents=True, exist_ok=True)

        write_snapshot(state_dir / "gauge_persistence", {
            "gauge_persist_test-gate-gauge": "2",
        })

        output, stderr, _ = _run_main(
            catalog,
            current_lines=["some_gauge 150"],
            env_overrides={"WARMUP_TICKS_REMAINING": "3"},
            state_dir=state_dir,
        )

        assert output["alarms"][0]["state"] == "skipped"

        persist = read_snapshot(state_dir / "gauge_persistence")
        assert persist.get("gauge_persist_test-gate-gauge") == "0", \
            f"Expected gauge persistence reset to 0, got {persist}"


# ── Test: exempt stderr uses unified telemetry format ────────────────────────

def test_exempt_stderr_unified_format():
    """Exempt alarm stderr telemetry uses the documented unified format, not the old reason=exempt format."""
    catalog = {
        "schema_version": 1,
        "alarm": [{
            "name": "test-fmt",
            "kind": "gauge",
            "severity": "WARN",
            "metric": "some_gauge",
            "op": ">",
            "threshold": 100,
            "exempt": True,
            "exempt_reason": "testing format",
        }]
    }

    output, stderr, _ = _run_main(
        catalog,
        current_lines=["some_gauge 50"],
    )

    # Should use unified format: # alarm=test-fmt metric=some_gauge series_matched=N state=skipped
    lines = [l for l in stderr.strip().split("\n") if "alarm=test-fmt" in l]
    assert len(lines) == 1, f"Expected exactly one telemetry line for test-fmt, got {lines}"
    line = lines[0]
    assert "metric=" in line, f"Missing 'metric=' in telemetry: {line}"
    assert "series_matched=" in line, f"Missing 'series_matched=' in telemetry: {line}"
    assert "state=skipped" in line, f"Missing 'state=skipped' in telemetry: {line}"
    # Must NOT contain old format
    assert "reason=exempt" not in line, f"Old format 'reason=exempt' found in telemetry: {line}"


# ── Test: exempt stderr does NOT produce ERROR_NO_SERIES when metric absent ──

def test_exempt_no_error_no_series_when_metric_absent():
    """Exempt alarm with absent metric should emit unified format with state=skipped, NOT ERROR_NO_SERIES."""
    catalog = {
        "schema_version": 1,
        "alarm": [{
            "name": "test-absent",
            "kind": "gauge",
            "severity": "WARN",
            "metric": "nonexistent_metric",
            "op": ">",
            "threshold": 100,
            "exempt": True,
            "exempt_reason": "testing",
        }]
    }

    output, stderr, _ = _run_main(
        catalog,
        current_lines=["other_metric 50"],
    )

    lines = [l for l in stderr.strip().split("\n") if "alarm=test-absent" in l]
    assert len(lines) == 1
    line = lines[0]
    # Full unified contract with exact values
    assert "metric=nonexistent_metric" in line, f"Wrong metric in telemetry: {line}"
    assert "series_matched=0" in line, f"Expected series_matched=0 for absent metric: {line}"
    assert "state=skipped" in line, f"Expected state=skipped, got: {line}"
    assert "ERROR_NO_SERIES" not in line, f"Unexpected ERROR_NO_SERIES for exempt+absent: {line}"


# ── Test: gate-skip stderr uses unified telemetry format ─────────────────────

def test_gate_skip_stderr_unified_format():
    """Gate-skipped alarm stderr telemetry uses the documented unified format."""
    catalog = {
        "schema_version": 1,
        "alarm": [{
            "name": "test-gate-fmt",
            "kind": "gauge",
            "severity": "WARN",
            "metric": "some_gauge",
            "op": ">",
            "threshold": 100,
            "gates": ["warmup-2-ticks"],
        }]
    }

    output, stderr, _ = _run_main(
        catalog,
        current_lines=["some_gauge 150"],
        env_overrides={"WARMUP_TICKS_REMAINING": "3"},
    )

    lines = [l for l in stderr.strip().split("\n") if "alarm=test-gate-fmt" in l]
    assert len(lines) == 1, f"Expected exactly one telemetry line, got {lines}"
    line = lines[0]
    # Exact values: metric matches alarm config, series_matched=1 (metric present in scrape)
    assert "metric=some_gauge" in line, f"Wrong metric in telemetry: {line}"
    assert "series_matched=1" in line, f"Expected series_matched=1: {line}"
    assert "state=skipped" in line, f"Missing 'state=skipped' in telemetry: {line}"


# ── Test: scp-accept-rate-low not skipped on the real 15-label fixtures (#3281) ─

def test_scp_accept_rate_low_not_label_set_mismatch_on_real_fixtures():
    """#3281/#3278: scp-accept-rate-low must NOT skip with "label set mismatch".

    Runs the REAL alarm catalog (.claude/skills/shared/metric-alarms.toml) against
    the REAL healthy eval-alarms fixtures. Before the label-set sync, the catalog
    `expected_labels` was 14 while the live binary (and the fixtures) emit 15
    post-verify series including `drift_manual_close`; the evaluator's exact
    set-equality check then skipped the alarm every tick with skip_reason
    "label set mismatch", silently disabling SCP accept-rate monitoring.

    This is the observable-impact guard for #3281: once the catalog and both
    fixtures carry all 15 labels, the alarm must EVALUATE — accepted states are
    `collecting_baseline` (single-invocation baseline) or `ok`, and skip_reason
    must not be a label-set mismatch. We deliberately accept `collecting_baseline`
    in addition to `ok`: a single eval against the two fixtures resets the
    counter-snapshot identity, so the evaluator legitimately reports
    `collecting_baseline` rather than a converged ratio.
    """
    repo_root = Path(__file__).resolve().parents[2]
    catalog_path = repo_root / ".claude" / "skills" / "shared" / "metric-alarms.toml"
    fixture_dir = repo_root / "scripts" / "fixtures" / "eval-alarms"
    current_path = fixture_dir / "healthy-current.prom"
    prev_path = fixture_dir / "healthy-prev.prom"

    assert catalog_path.is_file(), f"missing catalog: {catalog_path}"
    assert current_path.is_file(), f"missing fixture: {current_path}"
    assert prev_path.is_file(), f"missing fixture: {prev_path}"

    with tempfile.TemporaryDirectory() as d:
        state_dir = Path(d) / "state"
        state_dir.mkdir(parents=True, exist_ok=True)

        argv = [
            "eval-alarms",
            "--catalog", str(catalog_path),
            "--current", str(current_path),
            "--prev", str(prev_path),
            "--state-dir", str(state_dir),
        ]
        env = {
            "PREV_PROM_INVALID": "false",
            "WARMUP_TICKS_REMAINING": "0",
            "FRESH_START": "no",
            "CRASH_RECOVERY": "no",
            "UPTIME_SECONDS": "9999",
            "MONITOR_MODE": "validator",
            "PID": "123",
            "START_TICKS": "456",
        }

        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()
        with patch.object(sys, "argv", argv), \
             patch.object(sys, "stdout", stdout_buf), \
             patch.object(sys, "stderr", stderr_buf), \
             patch.dict(os.environ, env, clear=False):
            rc = main()

        assert rc == 0, f"main() returned {rc}, stderr: {stderr_buf.getvalue()}"
        output = json.loads(stdout_buf.getvalue())

    results = [a for a in output.get("alarms", []) if a.get("name") == "scp-accept-rate-low"]
    assert len(results) == 1, f"expected exactly one scp-accept-rate-low result, got {results}"
    result = results[0]

    skip_reason = result.get("skip_reason")
    assert skip_reason != "label set mismatch", (
        f"scp-accept-rate-low skipped with label-set mismatch — the catalog "
        f"expected_labels and the fixtures disagree on the post-verify label set: "
        f"{result}"
    )
    assert result.get("state") in ("collecting_baseline", "ok"), (
        f"scp-accept-rate-low did not evaluate (state should be collecting_baseline "
        f"or ok, skip_reason should be falsy): state={result.get('state')!r} "
        f"skip_reason={skip_reason!r}"
    )


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
