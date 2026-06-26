#!/usr/bin/env python3
"""Regression tests for inbound-auth banner severity calibration (issue #3653).

`inbound-auth-low` (threshold < 3) was severity WARN and flipped the monitor-tick
banner to WARNING on ~90% of ticks for a structurally-expected, firewall-limited,
consensus-non-impacting condition (alarm fatigue, #3653). The fix demotes the
chronic < 3 floor to NONC (reported/filed Non-critical, banner-neutral) and adds
a separate `inbound-auth-critical` (threshold < 1, WARN) so that genuine isolation
(zero authenticated inbound) still escalates and flips the banner.

These tests load the REAL catalog (.claude/skills/shared/metric-alarms.toml) and
evaluate the two alarms against synthetic gauge values, asserting:
  - inbound=2  → inbound-auth-low fires as NONC, inbound-auth-critical does NOT fire
  - inbound=0  → inbound-auth-low fires as NONC, inbound-auth-critical fires as WARN
  - inbound=3  → neither fires

Fail-before / pass-after: on origin/main, inbound-auth-low is WARN and
inbound-auth-critical does not exist → these tests fail.
"""

import importlib.util
from pathlib import Path

import pytest

# eval-alarms.py uses a hyphen, so we need importlib
_spec = importlib.util.spec_from_file_location(
    "eval_alarms",
    Path(__file__).parent / "eval-alarms.py",
)
_mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_mod)

eval_gauge = _mod.eval_gauge

# Locate the catalog relative to this test file (scripts/lib/ → repo root).
_REPO_ROOT = Path(__file__).resolve().parents[2]
_CATALOG = _REPO_ROOT / ".claude" / "skills" / "shared" / "metric-alarms.toml"

try:
    import tomllib  # py3.11+
except ModuleNotFoundError:  # pragma: no cover
    import tomli as tomllib  # type: ignore


def _load_alarm(name: str) -> dict:
    with open(_CATALOG, "rb") as f:
        catalog = tomllib.load(f)
    for a in catalog.get("alarm", []):
        if a.get("name") == name:
            return a
    raise AssertionError(f"alarm {name!r} not found in {_CATALOG}")


def _current(inbound: float) -> dict:
    """Build a parsed-prom dict with the inbound-authenticated gauge set."""
    return {"stellar_overlay_inbound_authenticated": [({}, inbound)]}


# ── inbound-auth-low: the benign chronic floor (< 3) is now NONC ─────────────

def test_inbound_auth_low_is_nonc_not_warn():
    """The chronic < 3 floor must NOT be WARN (would pin the banner; #3653)."""
    alarm = _load_alarm("inbound-auth-low")
    assert alarm["severity"] == "NONC", (
        f"inbound-auth-low must be NONC (banner-neutral), got {alarm['severity']!r}"
    )
    assert alarm["threshold"] == 3
    assert alarm["op"] == "<"


def test_inbound_auth_low_fires_nonc_at_two():
    """inbound=2 fires inbound-auth-low, and its firing severity is NONC."""
    alarm = _load_alarm("inbound-auth-low")
    r = eval_gauge(alarm, _current(2), {}, prev_prom_invalid=False)
    assert r["state"] == "firing", r
    assert r["severity"] == "NONC", r


def test_inbound_auth_low_ok_at_three():
    """inbound=3 does NOT fire (strict < 3)."""
    alarm = _load_alarm("inbound-auth-low")
    r = eval_gauge(alarm, _current(3), {}, prev_prom_invalid=False)
    assert r["state"] == "ok", r
    assert r["severity"] == "", r  # severity only set when firing


# ── inbound-auth-critical: genuine isolation (< 1) still escalates to WARN ───

def test_inbound_auth_critical_exists_and_is_warn():
    """Escalation tier exists, is WARN, threshold < 1."""
    alarm = _load_alarm("inbound-auth-critical")
    assert alarm["severity"] == "WARN", alarm
    assert alarm["threshold"] == 1
    assert alarm["op"] == "<"


def test_inbound_auth_critical_does_not_fire_at_two():
    """The benign < 3 floor (inbound=2) must NOT trip the WARN escalation."""
    alarm = _load_alarm("inbound-auth-critical")
    r = eval_gauge(alarm, _current(2), {}, prev_prom_invalid=False)
    assert r["state"] == "ok", r


def test_inbound_auth_critical_fires_warn_at_zero():
    """Zero authenticated inbound is genuine isolation → fires WARN."""
    alarm = _load_alarm("inbound-auth-critical")
    r = eval_gauge(alarm, _current(0), {}, prev_prom_invalid=False)
    assert r["state"] == "firing", r
    assert r["severity"] == "WARN", r


# ── Combined banner-contract check at zero inbound ───────────────────────────

def test_zero_inbound_low_nonc_critical_warn():
    """At inbound=0 the benign tier stays NONC and the escalation tier is WARN.

    The banner-contribution rule (SKILL.md, #3653) then keeps the NONC tier
    banner-neutral while the WARN tier flips the banner — so genuine isolation
    is never silenced by the demotion.
    """
    low = eval_gauge(_load_alarm("inbound-auth-low"), _current(0), {}, prev_prom_invalid=False)
    crit = eval_gauge(_load_alarm("inbound-auth-critical"), _current(0), {}, prev_prom_invalid=False)
    assert low["state"] == "firing" and low["severity"] == "NONC", low
    assert crit["state"] == "firing" and crit["severity"] == "WARN", crit


if __name__ == "__main__":
    raise SystemExit(pytest.main([__file__, "-v"]))
