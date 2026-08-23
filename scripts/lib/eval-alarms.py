#!/usr/bin/env python3
"""
Evaluate monitor-tick alarms from a TOML catalog against Prometheus scrape data.

Usage:
    eval-alarms.py --catalog PATH --current PATH [--prev PATH] --state-dir PATH

Inputs (env vars):
    PREV_PROM_INVALID   true/false (default: false)
    PREV_SCRAPE_AGE_SECONDS  wall-clock age of the prev.prom/snapshot baseline,
                        in seconds (default/absent/-1: unknown ⇒ not stale).
                        When >= GAP_STALE_THRESHOLD_SECONDS the baseline is
                        "gap-stale" (see #3246): the monitor LOOP stalled across
                        a long wall-clock gap while the validator PROCESS
                        survived, so the same-PID baseline is hours old and the
                        counter-delta would false-fire as an acute burst.
    GAP_STALE_THRESHOLD_SECONDS  gap-stale threshold (default: 3600 ≈ 3× tick).
    MIN_EVAL_WINDOW_SECONDS  symmetric LOWER bound to gap-stale (default: 600).
                        When the prev baseline is YOUNGER than this, a duplicate/
                        back-to-back tick (e.g. the watchdog firing into a
                        still-running tick, #3757) sampled too short an interval
                        to carry a meaningful cross-tick delta; the counter-family
                        evaluators return a NON-destructive skip that PRESERVES
                        the streak/ratio/dynamic snapshot. Age -1/absent ⇒ unknown
                        ⇒ NOT too-fresh (fail-safe).
    WARMUP_TICKS_REMAINING  0/1/2 (default: 0)
    FRESH_START         yes/no (default: no)
    CRASH_RECOVERY      yes/no (default: no)
    UPTIME_SECONDS      integer (default: 9999)
    MONITOR_MODE        validator/watcher (default: validator)
    PID                 process PID (required for counter-ratio/counter-streak)
    START_TICKS         /proc/PID/stat field 22 (required for counter-ratio/counter-streak)

Outputs:
    stdout: JSON (schema_version=1) with alarms array + aggregate lines
    stderr: per-alarm telemetry (# alarm=NAME metric=METRIC series_matched=N state=STATE)
    Side effects: writes updated snapshot files in --state-dir
                  (suppressed by --no-snapshot-write for a read-only dry-run)

Exit codes:
    0 = success
    1 = fatal error (invalid TOML, missing required args)
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import time
from pathlib import Path

try:
    import tomllib
except ImportError:
    import tomli as tomllib  # type: ignore[no-redef]

SCHEMA_VERSION = 1

# Sentinel skip_reason for the symmetric "too-fresh" lower-bound guard (#3757):
# a duplicate / back-to-back tick sampled an inter-scrape interval shorter than
# MIN_EVAL_WINDOW_SECONDS. The cross-tick counter-family evaluators return this
# as a NON-destructive skip (no snapshot write). It is the lower-bound mirror of
# the gap_stale upper bound: gap_stale suppresses a too-OLD baseline (loop
# stalled), too-fresh suppresses a too-YOUNG one (watchdog duplicate fired into
# a still-running tick).
SKIP_INTERVAL_TOO_SHORT = "interval too short"

# Skip reasons that must NOT trigger a counter-snapshot reset (#3758). These are
# monitoring-side caller errors (e.g. an abbreviated tick that failed to export
# PID/START_TICKS) that carry ZERO information about the node — treating them as
# a coverage gap and clearing the baseline destroys sustained-breach evidence
# (a live 22-tick breach_streak was erased this way). #3279's in-function guard
# already preserves the baseline for this case; maybe_reset_counter_snapshot
# honors the same contract via this set. Deny-list (not allow-list) because the
# reset-worthy reasons carry dynamic text (`gap-stale (prev age 5.0h)`,
# `low volume (delta=… < …)`, `missing <metric>{suffix}`), making an exact
# allow-list impractical and a prefix allow-list fragile. Any FUTURE
# monitoring-side caller-error skip reason carrying no node info must be added
# here.
#
# SKIP_INTERVAL_TOO_SHORT (#3757) belongs here for the same reason: the
# evaluator already returned WITHOUT a snapshot write, and this membership stops
# the centralized reset from wiping the streak/ratio/dynamic state.
PRESERVE_BASELINE_SKIP_REASONS = {"missing process identity", SKIP_INTERVAL_TOO_SHORT}

# When True (set by --no-snapshot-write in main()), write_snapshot() becomes a
# no-op so the evaluator runs as a TRUE read-only dry-run: it evaluates against
# the existing on-disk snapshots and emits identical JSON/telemetry, but
# persists nothing. This is the single chokepoint for ALL stateful writes
# (counter_streak_snapshot, ratio_snapshot, counter_dynamic_snapshot,
# gauge_persistence, and the maybe_reset_counter_snapshot reset paths), so
# gating it here suppresses every side effect at once — letting a diagnostic or
# repeat invocation within a tick re-evaluate without consuming the delta.
_NO_SNAPSHOT_WRITE = False

VALID_KINDS = {
    "gauge", "gauge-ratio", "counter", "counter-dynamic",
    "counter-ratio", "histogram-p99", "histogram-bucket-rate", "counter-streak",
}
VALID_SEVERITIES = {"SYNC", "ACTION", "WARN", "NONC"}
VALID_GATES = {"warmup-2-ticks", "synced-only", "uptime-min-15m", "validator-only"}
VALID_OPS = {">", "<", ">=", "<=", "!=", "=="}


# ── Prometheus parsing ───────────────────────────────────────────────────────

def parse_prom(path: Path | None) -> dict[str, list[tuple[dict[str, str], float]]]:
    """Parse a Prometheus text exposition file.

    Returns {metric_name: [(labels_dict, value), ...]} where metric_name
    is the base name (without labels). Labels are parsed from {k="v",...}.
    """
    if path is None or not path.exists() or path.stat().st_size == 0:
        return {}

    metrics: dict[str, list[tuple[dict[str, str], float]]] = {}
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        # Parse: metric_name{label="val",...} value [timestamp]
        # or:   metric_name value [timestamp]
        m = re.match(
            r'^([a-zA-Z_:][a-zA-Z0-9_:]*)'
            r'(?:\{([^}]*)\})?\s+'
            r'([0-9eE.+\-]+(?:NaN|Inf)?)',
            line,
        )
        if not m:
            continue
        name = m.group(1)
        labels_str = m.group(2) or ""
        try:
            value = float(m.group(3))
        except ValueError:
            continue

        labels: dict[str, str] = {}
        if labels_str:
            for pair in re.findall(r'([a-zA-Z_][a-zA-Z0-9_]*)="([^"]*)"', labels_str):
                labels[pair[0]] = pair[1]

        metrics.setdefault(name, []).append((labels, value))
    return metrics


def extract_value(
    metrics: dict[str, list[tuple[dict[str, str], float]]],
    metric_name: str,
    extraction: str,
    labels: list[dict[str, str]] | None = None,
) -> float | None:
    """Extract a single numeric value from parsed metrics.

    Returns None if the metric/label combination is not found.
    """
    # Handle metric names with inline label selectors like
    # 'henyey_scp_post_verify_total{reason="accepted"}'
    inline_labels: dict[str, str] = {}
    m = re.match(r'^([^{]+)\{(.+)\}$', metric_name)
    if m:
        metric_name = m.group(1)
        for pair in re.findall(r'([a-zA-Z_][a-zA-Z0-9_]*)="([^"]*)"', m.group(2)):
            inline_labels[pair[0]] = pair[1]

    series = metrics.get(metric_name, [])
    if not series:
        return None

    if extraction == "form1" and not inline_labels:
        # Scalar — expect exactly one series without labels (or first match)
        for lbl, val in series:
            if not lbl:
                return val
        # Fallback: return first series if no unlabeled one
        return series[0][1] if series else None

    if extraction == "form2" or inline_labels:
        # Single labeled series
        target_labels = dict(inline_labels)
        if labels:
            for l in labels:
                target_labels[l["key"]] = l["value"]
        for lbl, val in series:
            if all(lbl.get(k) == v for k, v in target_labels.items()):
                return val
        return None

    if extraction == "form3":
        # Sum of all matching series (for metric_sum with labels)
        total = 0.0
        matched = 0
        for lbl, val in series:
            total += val
            matched += 1
        return total if matched > 0 else None

    if extraction == "form2-sum-all":
        # Sum of all labeled series for a metric
        total = 0.0
        matched = 0
        for lbl, val in series:
            total += val
            matched += 1
        return total if matched > 0 else None

    return None


def extract_sum(
    metrics: dict[str, list[tuple[dict[str, str], float]]],
    metric_names: list[str],
    extraction: str,
) -> float | None:
    """Extract and sum values from multiple metrics."""
    total = 0.0
    for name in metric_names:
        val = extract_value(metrics, name, extraction)
        if val is None:
            return None
        total += val
    return total


def count_series(
    metrics: dict[str, list[tuple[dict[str, str], float]]],
    metric_name: str,
) -> int:
    """Count how many series match a metric name (stripping inline labels)."""
    m = re.match(r'^([^{]+)', metric_name)
    base = m.group(1) if m else metric_name
    return len(metrics.get(base, []))


def telemetry_metric(alarm: dict, kind: str) -> str:
    """Return the representative metric name for telemetry output.

    Each alarm kind stores its metric under different catalog keys.
    This mirrors the evaluator's own lookup order so that the telemetry
    line reflects the metric that was actually evaluated.
    """
    if kind in ("histogram-p99", "histogram-bucket-rate"):
        m = alarm.get("metric", "")
        return f"{m}_bucket" if m else ""
    if kind == "gauge-ratio":
        return alarm.get("numerator_metric", "")
    if kind == "counter-ratio":
        if alarm.get("numerator_sum"):
            return alarm["numerator_sum"][0]
        if alarm.get("numerator"):
            return alarm["numerator"]
        if alarm.get("denominator_sum"):
            return alarm["denominator_sum"][0]
        return alarm.get("denominator", "")
    if kind == "counter-streak":
        return alarm.get("metric", "")
    if kind in ("counter", "counter-dynamic"):
        if alarm.get("metric_sum"):
            return alarm["metric_sum"][0]
        return alarm.get("metric", "")
    return alarm.get("metric", "")


def default_extra_values(alarm: dict, kind: str) -> dict:
    """Return default extra_values for a given alarm kind.

    Ensures that all template placeholders in details/filing_title are
    resolved even on skip, baseline, and early-return paths.
    """
    if kind == "histogram-p99":
        return {"p99_value": None, "mean_value": None}
    if kind == "histogram-bucket-rate":
        return {"rate_value": None}
    if kind == "counter-ratio":
        return {
            "streak": 0,
            "streak_threshold": alarm.get("streak_threshold", 3),
            "ratio_threshold": alarm.get("ratio_threshold", 0),
        }
    if kind == "counter-dynamic":
        return {"prior_delta": 0}
    if kind == "counter-streak":
        return {"streak": 0, "streak_threshold": alarm.get("streak_threshold", 3)}
    return {}


# ── Gate evaluation ──────────────────────────────────────────────────────────

def gates_pass(
    gates: list[str],
    warmup_remaining: int,
    fresh_start: bool,
    crash_recovery: bool,
    uptime: int,
    monitor_mode: str,
) -> tuple[bool, str | None]:
    """Check if all gates pass. Returns (pass, skip_reason)."""
    for gate in gates:
        if gate == "validator-only" and monitor_mode == "watcher":
            return False, "watcher mode (validator-only alarm)"
        if gate == "warmup-2-ticks" and warmup_remaining > 0:
            return False, f"warmup ({warmup_remaining} ticks remaining)"
        if gate == "synced-only":
            if uptime < 900 or crash_recovery or fresh_start:
                return False, "not synced (synced-only gate)"
        if gate == "uptime-min-15m" and uptime < 900:
            return False, "uptime < 15m"
    return True, None


# ── Comparison operators ─────────────────────────────────────────────────────

def compare(value: float, op: str, threshold: float) -> bool:
    """Apply comparison operator."""
    if op == ">":
        return value > threshold
    if op == "<":
        return value < threshold
    if op == ">=":
        return value >= threshold
    if op == "<=":
        return value <= threshold
    if op == "!=":
        return value != threshold
    if op == "==":
        return value == threshold
    return False


# ── Snapshot management ──────────────────────────────────────────────────────

def read_snapshot(path: Path) -> dict[str, str]:
    """Read a key=value snapshot file."""
    result: dict[str, str] = {}
    if not path.exists():
        return result
    for line in path.read_text().splitlines():
        line = line.strip()
        if "=" in line:
            k, v = line.split("=", 1)
            result[k.strip()] = v.strip()
    return result


def write_snapshot(path: Path, data: dict[str, str]) -> None:
    """Write a key=value snapshot file atomically via rename.

    No-op when _NO_SNAPSHOT_WRITE is set (--no-snapshot-write): the in-memory
    snapshot/persistence dicts the callers built are still updated locally —
    harmless for this single-shot process since nothing reads them after exit —
    but nothing is persisted, so the on-disk delta is not consumed. A future
    in-process loop that re-reads state would need to revisit this.
    """
    if _NO_SNAPSHOT_WRITE:
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    lines = [f"{k}={v}" for k, v in data.items()]
    tmp = path.with_suffix(".tmp")
    tmp.write_text("\n".join(lines) + "\n")
    tmp.rename(path)


# ── Alarm cooldown (dedup) file ──────────────────────────────────────────────
#
# `anomaly_cooldown.json` is the monitor's alarm-dedup state. Historically it
# was written by a hand-performed JSON edit in the tick procedure (#3762) — a
# step that failed silently whenever an agent skipped it, most often on an
# alarm's *first* fire (no prior entry to pattern-match against). These helpers
# move the whole read/suppress/write lifecycle into the one tool that already
# knows `cooldown_key`, `cooldown_seconds`, and which alarms are firing.
#
# The file has a historically MIXED schema: some values are dicts
# ({"last_fired": <int>, "cooldown_seconds": <int>}), others are bare ints.
# read_cooldown_file normalizes bare ints to dict form on read; write_cooldown_file
# always emits normalized dict form, one-time-migrating the bare ints.

def _normalize_cooldown_entry(value: object) -> dict:
    """Normalize one cooldown value to dict form.

    Bare int/float `X` → {"last_fired": int(X)}; a dict is passed through
    (unknown sub-keys preserved). Anything else yields {} (dropped last_fired),
    which the callers treat as "no prior fire".
    """
    if isinstance(value, dict):
        entry = dict(value)
        if "last_fired" in entry:
            try:
                entry["last_fired"] = int(entry["last_fired"])
            except (TypeError, ValueError):
                pass
        return entry
    if isinstance(value, bool):
        # bool is an int subclass — treat as no usable timestamp.
        return {}
    if isinstance(value, (int, float)):
        return {"last_fired": int(value)}
    return {}


def read_cooldown_file(path: Path) -> dict[str, dict]:
    """Read the alarm-cooldown dedup file, normalizing the mixed schema.

    Missing/empty/malformed file → {} (fail toward "no prior fires", i.e. the
    next fire is reported so nothing is silently suppressed). Every returned
    value is a dict; unknown keys are preserved verbatim.
    """
    if not path.exists():
        return {}
    try:
        raw = json.loads(path.read_text())
    except (json.JSONDecodeError, OSError):
        return {}
    if not isinstance(raw, dict):
        return {}
    return {k: _normalize_cooldown_entry(v) for k, v in raw.items()}


def write_cooldown_file(path: Path, data: dict[str, dict]) -> None:
    """Write the alarm-cooldown dedup file atomically via rename.

    Always emits normalized dict form (bare ints one-time-migrated). No-op when
    _NO_SNAPSHOT_WRITE is set (--no-snapshot-write), mirroring write_snapshot so
    a read-only diagnostic re-run never mutates the dedup window. The temp file
    is created in the target's OWN directory so the rename stays atomic and no
    unrelated directory (e.g. cwd) is dirtied.
    """
    if _NO_SNAPSHOT_WRITE:
        return
    normalized = {k: _normalize_cooldown_entry(v) for k, v in data.items()}
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.parent / (path.name + ".tmp")
    tmp.write_text(json.dumps(normalized, indent=2, sort_keys=True) + "\n")
    tmp.rename(path)


def apply_cooldowns(results: list[dict], cooldown_data: dict[str, dict],
                    now: int) -> None:
    """Suppress in-window firing metrics-family alarms; persist fresh fires.

    Scoped to `contributes_to == "metrics"` (Critic A): the ratio/streak
    families are rendered by their own lines in render_aggregate and have no
    `cooldown` branch, so flipping them would mislabel a suppressed alarm as
    `ok`. Mutates `results` and `cooldown_data` in place.

    For each firing metrics-family alarm, keyed by its `cooldown_key`:
      - if a prior `last_fired` is within `cooldown_seconds` (strict `<`), it is
        a duplicate the agent already filed this window: suppress it — set
        state="cooldown", record `cooldown_remaining_seconds`, and do NOT bump
        `last_fired`. The `now - last_fired == cooldown_seconds` boundary is
        OUTSIDE the window and re-fires.
      - otherwise it is a genuine fire the agent will file this tick: record
        `last_fired = now` (and the catalog `cooldown_seconds`). Recording at
        detection — not after filing — means a crash between eval and filing
        marks it fired-but-unfiled for one window (self-heals next window);
        strictly better than today's fail-open duplicate storm (#3762).
    """
    for r in results:
        if r.get("contributes_to") != "metrics":
            continue
        if r.get("state") != "firing":
            continue
        key = r["cooldown_key"]
        cooldown_seconds = r["cooldown_seconds"]
        entry = cooldown_data.get(key)
        last_fired = entry.get("last_fired") if entry else None
        if last_fired is not None and (now - last_fired) < cooldown_seconds:
            r["state"] = "cooldown"
            r["cooldown_remaining_seconds"] = cooldown_seconds - (now - last_fired)
        else:
            cooldown_data[key] = {"last_fired": now, "cooldown_seconds": cooldown_seconds}


# ── Alarm evaluation ────────────────────────────────────────────────────────

def interpolate(template: str, values: dict[str, object]) -> str:
    """Interpolate {key} placeholders in a template string."""
    result = template
    for k, v in values.items():
        result = result.replace(f"{{{k}}}", str(v))
    return result


def make_result(
    alarm: dict,
    state: str,
    value: float | None = None,
    threshold: float | None = None,
    skip_reason: str | None = None,
    for_ticks_elapsed: int = 0,
    extra_values: dict | None = None,
) -> dict:
    """Create a result dict for one alarm."""
    values = {"value": value or 0, "threshold": threshold or 0}
    if extra_values:
        values.update(extra_values)

    kind = alarm["kind"]
    if kind in ("counter-ratio", ):
        contributes_to = "metrics_ratio"
    elif kind == "counter-streak":
        contributes_to = "recovery_stalled"
    else:
        contributes_to = "metrics"

    result = {
        "name": alarm["name"],
        "state": state,
        "severity": alarm.get("severity", "") if state == "firing" else "",
        "value": value,
        "threshold": threshold,
        "summary": interpolate(alarm.get("summary", ""), values),
        "details": interpolate(alarm.get("details", ""), values),
        "cooldown_key": alarm.get("cooldown_key", alarm["name"]),
        "cooldown_seconds": alarm.get("cooldown_seconds", 3600),
        "filing_title": interpolate(alarm.get("filing_title", ""), values),
        "filing_search": alarm.get("filing_search", ""),
        "notes": alarm.get("notes", ""),
        "for_ticks_elapsed": for_ticks_elapsed,
        "skip_reason": skip_reason,
        "contributes_to": contributes_to,
        # Surface the post-restart marker from extra_values so the renderer can
        # distinguish a baseline-reset absolute fire (#3198/#3206) from a
        # cross-tick burst. maybe_post_restart_fire is the sole setter; it lives
        # only inside extra_values, so without this it never reaches the
        # renderer and a post-restart fire is mislabeled (burst) (#3274).
        "post_restart": bool(extra_values and extra_values.get("post_restart")),
    }

    # Safety net: warn about unresolved template placeholders.
    # Pattern matches {word} but not Prometheus label syntax like {reason="..."}.
    for field in ("details", "summary", "filing_title"):
        val = result[field]
        if val and re.search(r'\{[a-z_]+\}', val):
            print(
                f"WARNING: unresolved placeholder in {alarm['name']}.{field}: {val}",
                file=sys.stderr,
            )

    return result


def maybe_reset_gauge_persistence(
    alarm: dict, kind: str, state: str, persistence_state: dict,
):
    """Reset gauge persistence on non-evaluable ticks to break consecutive chains.

    Invariant: for persistent gauge-family alarms (for_ticks > 1), any tick
    producing state="skipped" resets the persistence counter to 0. This prevents
    stale pre-restart (or pre-gate) persistence from causing immediate firing
    when the alarm becomes evaluable again.
    """
    if state == "skipped" and kind in ("gauge", "gauge-ratio") and alarm.get("for_ticks", 1) > 1:
        persistence_state[f"gauge_persist_{alarm['name']}"] = "0"


def maybe_reset_counter_snapshot(
    alarm: dict, kind: str, state: str, state_dir: Path,
    skip_reason: str | None = None,
):
    """Reset counter snapshot state on non-evaluable ticks.

    Invariant: for counter-family alarms, any tick producing state="skipped"
    resets stateful snapshot keys to prevent stale pre-skip state from
    carrying over:
    - counter-dynamic: deletes prior_delta (a delta, not cumulative — stale after gap)
    - counter-ratio: zeros streak only (baselines are cumulative and may be
      freshly updated by the evaluator on low-volume skips)
    - counter-streak: clears entire snapshot to force baseline re-collection
      on resume (eval_counter_streak defaults missing counter_value to 0,
      so partial deletion would cause the full counter value to appear as
      a single-tick delta)

    Does NOT fire on "collecting_baseline" — that state means the evaluator
    wrote fresh baseline data that must be preserved for the next tick.

    Exemption (#3758): a `missing process identity` skip is a monitoring-side
    caller bug (an abbreviated tick that didn't export PID/START_TICKS) — it
    carries ZERO information about the node. #3279's guard eval_counter_streak
    deliberately treats it as "establish nothing, decide nothing, preserve the
    baseline untouched" and returns before touching the snapshot. This hook
    honors that same contract: for any reason in PRESERVE_BASELINE_SKIP_REASONS
    we early-return before any write, so the caller-side cleanup no longer
    destroys the baseline from the outside (a 22-tick breach_streak was erased
    live this way). Any FUTURE monitoring-side caller-error skip reason that
    carries no node information must be added to that set.
    """
    if state != "skipped":
        return
    if skip_reason in PRESERVE_BASELINE_SKIP_REASONS:
        return
    name = alarm["name"]

    if kind == "counter-dynamic":
        snapshot_path = state_dir / "counter_dynamic_snapshot"
        snapshot = read_snapshot(snapshot_path)
        key = f"prior_delta_{name}"
        if key in snapshot:
            del snapshot[key]
            write_snapshot(snapshot_path, snapshot)

    elif kind == "counter-ratio":
        snapshot_path = state_dir / "ratio_snapshot"
        snapshot = read_snapshot(snapshot_path)
        streak_key = f"{name}_streak"
        if streak_key in snapshot and snapshot[streak_key] != "0":
            snapshot[streak_key] = "0"
            write_snapshot(snapshot_path, snapshot)

    elif kind == "counter-streak":
        snapshot_file = alarm.get("snapshot_file", "counter_streak_snapshot")
        snapshot_path = state_dir / snapshot_file
        snapshot = read_snapshot(snapshot_path)
        if snapshot:
            # Clear the entire snapshot to force a full "collecting_baseline"
            # tick on resume. We cannot just delete counter_value because
            # eval_counter_streak defaults missing counter_value to 0, which
            # would make the entire current counter value appear as a delta.
            # Clearing the snapshot triggers the `if not snapshot:` baseline
            # collection path.
            write_snapshot(snapshot_path, {})


def eval_gauge(
    alarm: dict,
    current: dict,
    persistence_state: dict,
    prev_prom_invalid: bool,
) -> dict:
    """Evaluate a gauge alarm."""
    metric = alarm["metric"]
    extraction = alarm.get("extraction", "form1")
    labels = alarm.get("labels", [])

    val = extract_value(current, metric, extraction, labels)
    if val is None:
        return make_result(alarm, "skipped", skip_reason="metric not found")

    op = alarm["op"]
    threshold = alarm["threshold"]
    for_ticks = alarm.get("for_ticks", 1)
    breaching = compare(val, op, threshold)

    # Optional companion-metric guard (#3752): a tick counts as breaching only
    # when the primary condition AND a second (guard) condition both hold. Used
    # by jemalloc-frag-high so a high fragmentation ratio only fires when
    # resident memory is *also* genuinely elevated — a bounded, oscillating
    # frag% on a long-uptime node is informational, not actionable. If the
    # guard metric is absent from this scrape we cannot confirm the guard
    # condition, so we fail toward NOT firing. Folding the guard into
    # `breaching` reuses the persistence/reset logic below verbatim: an unmet
    # guard resets the streak and reports `ok`, exactly like a non-breaching
    # primary. Alarms without guard_metric are unaffected (byte-for-byte).
    guard_metric = alarm.get("guard_metric")
    if guard_metric is not None:
        guard_extraction = alarm.get("guard_extraction", "form1")
        guard_val = extract_value(current, guard_metric, guard_extraction)
        if guard_val is None:
            guard_breaching = False
        else:
            guard_breaching = compare(guard_val, alarm["guard_op"], alarm["guard_threshold"])
        breaching = breaching and guard_breaching

    if for_ticks <= 1:
        if breaching:
            return make_result(alarm, "firing", value=val, threshold=threshold, for_ticks_elapsed=1)
        return make_result(alarm, "ok", value=val, threshold=threshold)

    # Persistence guard
    key = f"gauge_persist_{alarm['name']}"
    prev_count = int(persistence_state.get(key, "0"))

    if prev_prom_invalid:
        # Reset persistence counter
        persistence_state[key] = "0"
        if breaching:
            persistence_state[key] = "1"
            return make_result(alarm, "breach", value=val, threshold=threshold, for_ticks_elapsed=1)
        return make_result(alarm, "ok", value=val, threshold=threshold)

    if breaching:
        new_count = prev_count + 1
        persistence_state[key] = str(new_count)
        if new_count >= for_ticks:
            return make_result(alarm, "firing", value=val, threshold=threshold, for_ticks_elapsed=new_count)
        return make_result(alarm, "breach", value=val, threshold=threshold, for_ticks_elapsed=new_count)
    else:
        persistence_state[key] = "0"
        return make_result(alarm, "ok", value=val, threshold=threshold)


def eval_gauge_ratio(
    alarm: dict,
    current: dict,
    persistence_state: dict,
    prev_prom_invalid: bool,
) -> dict:
    """Evaluate a gauge-ratio alarm."""
    num_metric = alarm["numerator_metric"]
    den_metric = alarm["denominator_metric"]
    num_extraction = alarm.get("numerator_extraction", "form1")
    den_extraction = alarm.get("denominator_extraction", "form1")

    num_val = extract_value(current, num_metric, num_extraction)
    if num_val is None:
        return make_result(alarm, "skipped", skip_reason="numerator metric not found")

    den_val = extract_value(current, den_metric, den_extraction)
    if den_val is None:
        absent = alarm.get("absent_denominator", "skip")
        if absent == "skip":
            return make_result(alarm, "skipped", skip_reason="denominator absent")
        return make_result(alarm, "skipped", skip_reason="denominator missing (error)")

    if den_val == 0:
        return make_result(alarm, "skipped", skip_reason="zero denominator")

    ratio = num_val / den_val
    op = alarm["op"]
    threshold = alarm["threshold"]
    breaching = compare(ratio, op, threshold)

    for_ticks = alarm.get("for_ticks", 1)
    if for_ticks <= 1:
        if breaching:
            return make_result(alarm, "firing", value=round(ratio, 4), threshold=threshold, for_ticks_elapsed=1)
        return make_result(alarm, "ok", value=round(ratio, 4), threshold=threshold)

    # Persistence guard (same logic as gauge)
    key = f"gauge_persist_{alarm['name']}"
    prev_count = int(persistence_state.get(key, "0"))

    if prev_prom_invalid:
        persistence_state[key] = "0"
        if breaching:
            persistence_state[key] = "1"
            return make_result(alarm, "breach", value=round(ratio, 4), threshold=threshold, for_ticks_elapsed=1)
        return make_result(alarm, "ok", value=round(ratio, 4), threshold=threshold)

    if breaching:
        new_count = prev_count + 1
        persistence_state[key] = str(new_count)
        if new_count >= for_ticks:
            return make_result(alarm, "firing", value=round(ratio, 4), threshold=threshold, for_ticks_elapsed=new_count)
        return make_result(alarm, "breach", value=round(ratio, 4), threshold=threshold, for_ticks_elapsed=new_count)
    else:
        persistence_state[key] = "0"
        return make_result(alarm, "ok", value=round(ratio, 4), threshold=threshold)


def gap_stale_reason(age_hours: float) -> str:
    """Skip reason for a gap-stale prev.prom baseline (#3246)."""
    return f"gap-stale (prev age {age_hours}h)"


def compute_too_fresh(prev_scrape_age: int, min_eval_window: int) -> bool:
    """Symmetric lower-bound mirror of gap_stale (#3757).

    True when the prev baseline is YOUNGER than MIN_EVAL_WINDOW_SECONDS — i.e. a
    duplicate / back-to-back tick (e.g. the watchdog firing into a still-running
    interactive tick) sampled an inter-scrape interval too short to carry a
    meaningful cross-tick delta. Such a tick would otherwise reset a breach
    streak to 0 (delta=0) or advance it (delta>=1), burning alarm cooldowns and
    double-advancing streak/ratio snapshots.

    Half-open window [0, min): age 0 (same instant) IS too-fresh, exactly the
    window is NOT. Age -1/absent ⇒ unknown ⇒ NOT too-fresh (fail-safe: never
    suppress on unknown age, exactly like gap_stale).
    """
    return 0 <= prev_scrape_age < min_eval_window


def eval_counter(
    alarm: dict,
    current: dict,
    prev: dict,
    prev_prom_invalid: bool,
    warmup_remaining: int,
    gap_stale: bool = False,
    gap_stale_age_hours: float = 0,
    too_fresh: bool = False,
) -> dict:
    """Evaluate a counter alarm."""
    if prev_prom_invalid:
        return make_result(alarm, "skipped", skip_reason="PREV_PROM_INVALID")
    # Gap-stale (#3246): prev.prom is same-PID but hours old, so cur-prev spans
    # the whole loop gap and would false-fire. prev.prom is recomputed from the
    # file each tick, so a plain SKIP is sufficient — no persisted baseline to
    # re-seed (contrast the snapshot families, which must re-baseline). Checked
    # AFTER prev_prom_invalid so a PID change is never double-handled.
    if gap_stale:
        return make_result(alarm, "skipped",
                           skip_reason=gap_stale_reason(gap_stale_age_hours))

    metric = alarm.get("metric")
    metric_sum_list = alarm.get("metric_sum")
    extraction = alarm.get("extraction", "form1")
    labels = alarm.get("labels", [])

    if metric_sum_list:
        cur_val = extract_sum(current, metric_sum_list, extraction)
        prev_val = extract_sum(prev, metric_sum_list, extraction)
    else:
        cur_val = extract_value(current, metric, extraction, labels)
        prev_val = extract_value(prev, metric, extraction, labels)

    if cur_val is None:
        return make_result(alarm, "skipped", skip_reason="metric not found")
    if prev_val is None:
        return make_result(alarm, "skipped", skip_reason="no previous data")

    # Warmup: skip if prev=0 (counter started at zero after restart)
    if warmup_remaining > 0 and prev_val == 0:
        return make_result(alarm, "skipped", skip_reason="warmup (prev=0)")

    # Too-fresh (#3757): a duplicate/back-to-back tick sampled an inter-scrape
    # interval below MIN_EVAL_WINDOW_SECONDS. prev.prom is recomputed each tick
    # (no persisted baseline), so a plain SKIP suffices — the next normal-cadence
    # tick spans the real interval. Checked after the metric/warmup guards and
    # before the delta so a too-short burst read is not mistaken for a fire.
    if too_fresh:
        return make_result(alarm, "skipped", skip_reason=SKIP_INTERVAL_TOO_SHORT)

    # Counter reset: if cur < prev, delta = cur
    if cur_val < prev_val:
        delta = cur_val
    else:
        delta = cur_val - prev_val

    op = alarm["op"]
    threshold = alarm["threshold"]
    breaching = compare(delta, op, threshold)

    if breaching:
        return make_result(alarm, "firing", value=delta, threshold=threshold, for_ticks_elapsed=1)
    return make_result(alarm, "ok", value=delta, threshold=threshold)


def eval_counter_dynamic(
    alarm: dict,
    current: dict,
    prev: dict,
    state_dir: Path,
    prev_prom_invalid: bool,
    warmup_remaining: int,
    gap_stale: bool = False,
    gap_stale_age_hours: float = 0,
    too_fresh: bool = False,
) -> dict:
    """Evaluate a counter-dynamic alarm (threshold = multiplier × prior delta)."""
    ev_default = default_extra_values(alarm, "counter-dynamic")

    if prev_prom_invalid:
        return make_result(alarm, "skipped", skip_reason="PREV_PROM_INVALID",
                           extra_values=ev_default)
    # Gap-stale (#3246): skip BEFORE the prior-delta snapshot write below — the
    # gap-spanning delta must not be stored as next tick's prior_delta, which
    # would poison the dynamic threshold. prev.prom is recomputed each tick, so
    # SKIP (not re-baseline) suffices. Checked after prev_prom_invalid so a PID
    # change is never double-handled.
    if gap_stale:
        return make_result(alarm, "skipped",
                           skip_reason=gap_stale_reason(gap_stale_age_hours),
                           extra_values=ev_default)

    metric_sum_list = alarm["metric_sum"]
    extraction = alarm.get("extraction", "form1")

    cur_val = extract_sum(current, metric_sum_list, extraction)
    prev_val = extract_sum(prev, metric_sum_list, extraction)

    if cur_val is None:
        return make_result(alarm, "skipped", skip_reason="metric not found",
                           extra_values=ev_default)
    if prev_val is None:
        return make_result(alarm, "skipped", skip_reason="no previous data",
                           extra_values=ev_default)

    if warmup_remaining > 0 and prev_val == 0:
        return make_result(alarm, "skipped", skip_reason="warmup (prev=0)",
                           extra_values=ev_default)

    # Too-fresh (#3757): skip BEFORE the prior-delta snapshot write below — a
    # duplicate tick's below-window delta must not be stored as next tick's
    # prior_delta, which would poison the dynamic threshold. The prior_delta
    # snapshot is left untouched, so the next normal tick compares against the
    # last real delta. Mirrors the gap_stale branch above.
    if too_fresh:
        return make_result(alarm, "skipped", skip_reason=SKIP_INTERVAL_TOO_SHORT,
                           extra_values=ev_default)

    # Counter reset
    delta = cur_val if cur_val < prev_val else cur_val - prev_val

    # Read prior delta from snapshot
    snapshot_path = state_dir / "counter_dynamic_snapshot"
    snapshot = read_snapshot(snapshot_path)
    prior_delta_key = f"prior_delta_{alarm['name']}"
    prior_delta_str = snapshot.get(prior_delta_key)

    # Store current delta for next tick
    snapshot[prior_delta_key] = str(int(delta))
    write_snapshot(snapshot_path, snapshot)

    if prior_delta_str is None:
        return make_result(alarm, "collecting_baseline", extra_values=ev_default)

    prior_delta = int(prior_delta_str)
    multiplier = alarm["multiplier"]
    min_absolute = alarm.get("min_absolute", 0)

    # Don't fire if prior delta is too small
    if prior_delta < min_absolute:
        return make_result(
            alarm, "ok", value=delta, threshold=multiplier * prior_delta,
            extra_values={"prior_delta": prior_delta},
        )

    # Don't fire if prior delta is 0
    if prior_delta == 0:
        return make_result(
            alarm, "ok", value=delta, threshold=0,
            extra_values={"prior_delta": prior_delta},
        )

    threshold = multiplier * prior_delta
    if delta >= threshold:
        return make_result(
            alarm, "firing", value=delta, threshold=threshold, for_ticks_elapsed=1,
            extra_values={"prior_delta": prior_delta},
        )
    return make_result(
        alarm, "ok", value=delta, threshold=threshold,
        extra_values={"prior_delta": prior_delta},
    )


def eval_histogram_p99(
    alarm: dict,
    current: dict,
    prev: dict,
    prev_prom_invalid: bool,
    gap_stale: bool = False,
    gap_stale_age_hours: float = 0,
    too_fresh: bool = False,
) -> dict:
    """Evaluate a histogram-p99 alarm with mean fallback."""
    ev_default = default_extra_values(alarm, "histogram-p99")

    if prev_prom_invalid:
        return make_result(alarm, "skipped", skip_reason="PREV_PROM_INVALID",
                           extra_values=ev_default)
    # Gap-stale (#3246): the count/sum/bucket deltas all span the loop gap, so
    # SKIP. prev.prom is recomputed each tick (no persisted baseline to reseed).
    # Checked after prev_prom_invalid so a PID change is never double-handled.
    if gap_stale:
        return make_result(alarm, "skipped",
                           skip_reason=gap_stale_reason(gap_stale_age_hours),
                           extra_values=ev_default)
    # Too-fresh (#3757): a below-window duplicate tick's count/sum/bucket deltas
    # span too short an interval to be meaningful. prev.prom is recomputed each
    # tick (no persisted baseline), so SKIP. Mirrors the gap_stale upper bound.
    if too_fresh:
        return make_result(alarm, "skipped", skip_reason=SKIP_INTERVAL_TOO_SHORT,
                           extra_values=ev_default)

    metric = alarm["metric"]
    min_count = alarm.get("min_count_delta", 20)

    # Check suffixes exist
    for suffix in ("_bucket", "_sum", "_count"):
        if not current.get(f"{metric}{suffix}"):
            return make_result(alarm, "skipped", skip_reason=f"missing {metric}{suffix}",
                               extra_values=ev_default)
    for suffix in ("_bucket", "_sum", "_count"):
        if not prev.get(f"{metric}{suffix}"):
            return make_result(alarm, "skipped", skip_reason=f"no previous {metric}{suffix}",
                               extra_values=ev_default)

    # Count delta
    cur_count = extract_value(current, f"{metric}_count", "form1")
    prev_count = extract_value(prev, f"{metric}_count", "form1")
    if cur_count is None or prev_count is None:
        return make_result(alarm, "skipped", skip_reason="missing count metric",
                           extra_values=ev_default)

    count_delta = cur_count - prev_count
    if count_delta < 0:
        return make_result(alarm, "skipped", skip_reason="counter reset (count)",
                           extra_values=ev_default)
    if count_delta < min_count:
        return make_result(alarm, "skipped", skip_reason=f"low volume (count_delta={int(count_delta)} < {min_count})",
                           extra_values=ev_default)

    # Mean fallback
    cur_sum = extract_value(current, f"{metric}_sum", "form1")
    prev_sum = extract_value(prev, f"{metric}_sum", "form1")
    mean_value = None
    if cur_sum is not None and prev_sum is not None:
        sum_delta = cur_sum - prev_sum
        if sum_delta >= 0 and count_delta > 0:
            mean_value = sum_delta / count_delta

    # P99 from buckets
    bucket_series_cur = current.get(f"{metric}_bucket", [])
    bucket_series_prev = prev.get(f"{metric}_bucket", [])

    # Build {le: delta} map
    bucket_deltas: dict[float, float] = {}
    cur_by_le: dict[float, float] = {}
    prev_by_le: dict[float, float] = {}
    for labels, val in bucket_series_cur:
        le = labels.get("le")
        if le is not None:
            try:
                cur_by_le[float(le)] = val
            except ValueError:
                if le == "+Inf":
                    cur_by_le[float("inf")] = val
    for labels, val in bucket_series_prev:
        le = labels.get("le")
        if le is not None:
            try:
                prev_by_le[float(le)] = val
            except ValueError:
                if le == "+Inf":
                    prev_by_le[float("inf")] = val

    for le in sorted(cur_by_le.keys()):
        cur_b = cur_by_le.get(le, 0)
        prev_b = prev_by_le.get(le, 0)
        d = cur_b - prev_b
        if d < 0:
            d = cur_b  # counter reset
        bucket_deltas[le] = d

    # Compute p99
    p99_value = None
    if bucket_deltas and count_delta > 0:
        target = 0.99 * count_delta
        cumulative = 0.0
        for le in sorted(bucket_deltas.keys()):
            cumulative += bucket_deltas[le]
            if cumulative >= target:
                p99_value = le
                break

    p99_threshold = alarm.get("p99_threshold", 0)
    mean_threshold = alarm.get("mean_threshold", 0)

    p99_breach = p99_value is not None and p99_value > p99_threshold
    mean_breach = mean_value is not None and mean_value > mean_threshold

    ev = {
        "p99_value": round(p99_value, 4) if p99_value is not None else None,
        "mean_value": round(mean_value, 4) if mean_value is not None else None,
    }

    if p99_breach or mean_breach:
        # Use the breached metric's value and threshold for display
        if p99_breach:
            display_value = p99_value
            display_threshold = p99_threshold
        else:
            display_value = mean_value
            display_threshold = mean_threshold
        return make_result(
            alarm, "firing", value=round(display_value, 4) if display_value else 0,
            threshold=display_threshold, for_ticks_elapsed=1, extra_values=ev,
        )
    return make_result(
        alarm, "ok",
        value=round(p99_value, 4) if p99_value is not None else 0,
        threshold=p99_threshold, extra_values=ev,
    )


def eval_histogram_bucket_rate(
    alarm: dict,
    current: dict,
    prev: dict,
    prev_prom_invalid: bool,
    gap_stale: bool = False,
    gap_stale_age_hours: float = 0,
    too_fresh: bool = False,
) -> dict:
    """Evaluate a histogram-bucket-rate alarm.

    Fires when the fraction of new observations exceeding a configured `le`
    boundary over the tick window is above `rate_threshold` (strict `>`). This
    keys on sustained window behavior — unlike the single-sample last-slot gauge
    alarm it complements (#3750). Reuses the same cumulative-bucket differencing
    as eval_histogram_p99: because Prometheus buckets are cumulative,
    `bucket_deltas[bucket_le]` is the count of new observations ≤ bucket_le in
    the window, so `over = count_delta - bucket_deltas[bucket_le]` and
    `rate = over / count_delta`.
    """
    ev_default = default_extra_values(alarm, "histogram-bucket-rate")

    if prev_prom_invalid:
        return make_result(alarm, "skipped", skip_reason="PREV_PROM_INVALID",
                           extra_values=ev_default)
    # Gap-stale (#3246): the count/bucket deltas span the loop gap, so SKIP.
    # prev.prom is recomputed each tick (no persisted baseline to reseed).
    # Checked after prev_prom_invalid so a PID change is never double-handled.
    if gap_stale:
        return make_result(alarm, "skipped",
                           skip_reason=gap_stale_reason(gap_stale_age_hours),
                           extra_values=ev_default)
    # Too-fresh (#3757): a below-window duplicate tick's count/bucket deltas span
    # too short an interval to be meaningful. prev.prom is recomputed each tick
    # (no persisted baseline), so SKIP. Mirrors the gap_stale upper bound, and
    # matches eval_histogram_p99 — this family is a prev.prom-differencing
    # evaluator, so it needs the lower bound just as much as the upper one.
    if too_fresh:
        return make_result(alarm, "skipped", skip_reason=SKIP_INTERVAL_TOO_SHORT,
                           extra_values=ev_default)

    metric = alarm["metric"]
    min_count = alarm.get("min_count_delta", 20)
    bucket_le = float(alarm["bucket_le"])
    rate_threshold = alarm["rate_threshold"]

    # Only _bucket and _count are needed for the over-threshold rate (no _sum).
    for suffix in ("_bucket", "_count"):
        if not current.get(f"{metric}{suffix}"):
            return make_result(alarm, "skipped", skip_reason=f"missing {metric}{suffix}",
                               extra_values=ev_default)
    for suffix in ("_bucket", "_count"):
        if not prev.get(f"{metric}{suffix}"):
            return make_result(alarm, "skipped", skip_reason=f"no previous {metric}{suffix}",
                               extra_values=ev_default)

    cur_count = extract_value(current, f"{metric}_count", "form1")
    prev_count = extract_value(prev, f"{metric}_count", "form1")
    if cur_count is None or prev_count is None:
        return make_result(alarm, "skipped", skip_reason="missing count metric",
                           extra_values=ev_default)

    count_delta = cur_count - prev_count
    if count_delta < 0:
        return make_result(alarm, "skipped", skip_reason="counter reset (count)",
                           extra_values=ev_default)
    if count_delta < min_count:
        return make_result(alarm, "skipped",
                           skip_reason=f"low volume (count_delta={int(count_delta)} < {min_count})",
                           extra_values=ev_default)

    # Build {le: delta} map (same cumulative-bucket differencing as p99).
    bucket_series_cur = current.get(f"{metric}_bucket", [])
    bucket_series_prev = prev.get(f"{metric}_bucket", [])
    cur_by_le: dict[float, float] = {}
    prev_by_le: dict[float, float] = {}
    for labels, val in bucket_series_cur:
        le = labels.get("le")
        if le is not None:
            try:
                cur_by_le[float(le)] = val
            except ValueError:
                if le == "+Inf":
                    cur_by_le[float("inf")] = val
    for labels, val in bucket_series_prev:
        le = labels.get("le")
        if le is not None:
            try:
                prev_by_le[float(le)] = val
            except ValueError:
                if le == "+Inf":
                    prev_by_le[float("inf")] = val

    bucket_deltas: dict[float, float] = {}
    for le in cur_by_le:
        d = cur_by_le[le] - prev_by_le.get(le, 0)
        if d < 0:
            d = cur_by_le[le]  # counter reset
        bucket_deltas[le] = d

    if bucket_le not in bucket_deltas:
        return make_result(alarm, "skipped",
                           skip_reason=f"bucket le={alarm['bucket_le']} not found",
                           extra_values=ev_default)

    # Cumulative bucket at bucket_le = observations ≤ bucket_le, so the
    # complement is the over-threshold count. Clamp before dividing so a
    # rebucket/reset differencing artifact can never yield a negative rate
    # (Critic A).
    over = count_delta - bucket_deltas[bucket_le]
    over = max(over, 0.0)
    # Defensive: `count_delta < min_count` already skips low-volume windows for
    # every shipped alarm (default min_count 20, this alarm 100), but a future
    # alarm configured with `min_count_delta = 0` and a zero-volume window would
    # reach here with count_delta == 0. Guard the division for symmetry with
    # `eval_histogram_p99`'s `count_delta > 0` checks (a zero-volume window has
    # no over-threshold slots, so rate is 0.0 and cannot fire).
    rate = over / count_delta if count_delta > 0 else 0.0

    ev = {"rate_value": round(rate, 4)}
    if rate > rate_threshold:
        return make_result(
            alarm, "firing", value=round(rate, 4), threshold=rate_threshold,
            for_ticks_elapsed=1, extra_values=ev,
        )
    return make_result(
        alarm, "ok", value=round(rate, 4), threshold=rate_threshold,
        extra_values=ev,
    )


def eval_counter_ratio(
    alarm: dict,
    current: dict,
    prev: dict,
    state_dir: Path,
    pid: str,
    start_ticks: str,
    fresh_start: bool,
    crash_recovery: bool,
    uptime: int,
    gap_stale: bool = False,
    too_fresh: bool = False,
) -> dict:
    """Evaluate a counter-ratio alarm with streak detection.

    Independent of PREV_PROM_INVALID — uses own PID/start_ticks in snapshot.
    """
    ev_default = default_extra_values(alarm, "counter-ratio")

    # Missing process identity guard (#3279): mirror eval_counter_streak. An
    # abbreviated tick that skips exporting PID/START_TICKS passes EMPTY strings.
    # An empty identity is NOT a valid distinct incarnation — treating it as one
    # invalidates the snapshot and rewrites the per-alarm baselines under the
    # empty identity (poison write). eval_counter_ratio does not call
    # maybe_post_restart_fire so it cannot post-restart-fire, but it must not
    # persist an empty identity either. Establish nothing, decide nothing: skip
    # BEFORE any snapshot/metric I/O so the prior valid baseline is preserved.
    if not pid or not start_ticks:
        return make_result(alarm, "skipped",
                           skip_reason="missing process identity",
                           extra_values=ev_default)

    # Global skip conditions for ratio checks
    ledger_age = extract_value(current, "stellar_ledger_age_current_seconds", "form1")
    if fresh_start:
        return make_result(alarm, "skipped", skip_reason="FRESH_START",
                           extra_values=ev_default)
    if ledger_age is not None and ledger_age > 30:
        return make_result(alarm, "skipped", skip_reason="ledger age > 30s",
                           extra_values=ev_default)
    if uptime < 600:
        return make_result(alarm, "skipped", skip_reason="uptime < 10m",
                           extra_values=ev_default)

    # Label validation for alarms with expected_labels
    expected_labels = alarm.get("expected_labels")
    if expected_labels:
        # Check that the expected label values exist in the current scrape
        base_metric = alarm.get("denominator", alarm.get("numerator", ""))
        if "{" in base_metric:
            base_metric = base_metric.split("{")[0]
        series = current.get(base_metric, [])
        if series:
            found_labels = set()
            for lbl, _ in series:
                reason_val = lbl.get("reason")
                if reason_val:
                    found_labels.add(reason_val)
            expected_set = set(expected_labels)
            if found_labels != expected_set:
                return make_result(alarm, "skipped", skip_reason="label set mismatch",
                                   extra_values=ev_default)

    snapshot_path = state_dir / "ratio_snapshot"
    snapshot = read_snapshot(snapshot_path)

    # Process identity check
    if snapshot:
        if snapshot.get("version") != "1":
            snapshot = {}
        elif snapshot.get("pid") != pid or snapshot.get("start_ticks") != start_ticks:
            snapshot = {}

    # Extract current values
    numerator_metric = alarm.get("numerator")
    numerator_sum = alarm.get("numerator_sum")
    denominator_metric = alarm.get("denominator")
    denominator_sum = alarm.get("denominator_sum")
    num_extraction = alarm.get("numerator_extraction", "form1")
    den_extraction = alarm.get("denominator_extraction", "form1")

    if numerator_sum:
        cur_num = 0.0
        for m in numerator_sum:
            v = extract_value(current, m, num_extraction)
            if v is None:
                return make_result(alarm, "skipped", skip_reason="missing numerator counter",
                                   extra_values=ev_default)
            cur_num += v
    elif numerator_metric:
        cur_num_v = extract_value(current, numerator_metric, num_extraction)
        if cur_num_v is None:
            if alarm.get("optional_counters"):
                return make_result(alarm, "skipped", skip_reason="missing counters",
                                   extra_values=ev_default)
            return make_result(alarm, "skipped", skip_reason="missing numerator counter",
                               extra_values=ev_default)
        cur_num = cur_num_v
    else:
        return make_result(alarm, "skipped", skip_reason="no numerator defined",
                           extra_values=ev_default)

    if denominator_sum:
        cur_den = 0.0
        for m in denominator_sum:
            v = extract_value(current, m, den_extraction)
            if v is None:
                return make_result(alarm, "skipped", skip_reason="missing denominator counter",
                                   extra_values=ev_default)
            cur_den += v
    elif denominator_metric:
        cur_den_v = extract_value(current, denominator_metric, den_extraction)
        if cur_den_v is None:
            if alarm.get("optional_counters"):
                return make_result(alarm, "skipped", skip_reason="missing counters",
                                   extra_values=ev_default)
            return make_result(alarm, "skipped", skip_reason="missing denominator counter",
                               extra_values=ev_default)
        cur_den = cur_den_v
    else:
        return make_result(alarm, "skipped", skip_reason="no denominator defined",
                           extra_values=ev_default)

    # Check for collecting baseline
    alarm_name = alarm["name"]
    prev_num_key = f"{alarm_name}_numerator"
    prev_den_key = f"{alarm_name}_denominator"
    streak_key = f"{alarm_name}_streak"

    if not snapshot or prev_num_key not in snapshot:
        # Collecting baseline — write current values
        snapshot["version"] = "1"
        snapshot["pid"] = pid
        snapshot["start_ticks"] = start_ticks
        snapshot[prev_num_key] = str(int(cur_num))
        snapshot[prev_den_key] = str(int(cur_den))
        snapshot[streak_key] = "0"
        write_snapshot(snapshot_path, snapshot)
        return make_result(alarm, "collecting_baseline", extra_values=ev_default)

    prev_num = int(snapshot[prev_num_key])
    prev_den = int(snapshot[prev_den_key])
    streak = int(snapshot.get(streak_key, "0"))

    # Counter reset check
    if cur_num < prev_num or cur_den < prev_den:
        snapshot[prev_num_key] = str(int(cur_num))
        snapshot[prev_den_key] = str(int(cur_den))
        snapshot[streak_key] = "0"
        write_snapshot(snapshot_path, snapshot)
        return make_result(alarm, "collecting_baseline", extra_values=ev_default)

    # Gap-stale (#3246): the snapshot baseline is SAME-PID (the identity check
    # above passed) but hours old — the monitor loop stalled across a long
    # wall-clock gap while the process survived. Unlike the prev.prom families
    # (which recompute their baseline from the file each tick and can simply
    # SKIP), this snapshot PERSISTS across ticks, so a plain skip would leave
    # the stale baseline and fire on the NEXT tick. RE-BASELINE instead: rewrite
    # the snapshot to current values and reset the streak to "0" — the exact
    # path used on a counter reset above. Sequenced after the identity check, so
    # a PID change (#3206) is handled by the snapshot reset there and never
    # double-handled here.
    if gap_stale:
        snapshot[prev_num_key] = str(int(cur_num))
        snapshot[prev_den_key] = str(int(cur_den))
        snapshot[streak_key] = "0"
        write_snapshot(snapshot_path, snapshot)
        return make_result(alarm, "collecting_baseline", extra_values=ev_default)

    # Too-fresh (#3757): a duplicate/back-to-back tick sampled an inter-scrape
    # interval below MIN_EVAL_WINDOW_SECONDS. Unlike gap_stale (which re-baselines
    # because the snapshot is stale), the baseline here is still VALID — the
    # duplicate must be a pure no-op. Return a NON-destructive skip WITHOUT
    # writing the snapshot, so the streak and cumulative baselines are preserved
    # untouched; the next normal-cadence tick spans the real interval. Sequenced
    # after the identity / counter-reset / gap_stale branches so a restart is
    # always handled first, and before any streak/baseline write below.
    if too_fresh:
        return make_result(alarm, "skipped", skip_reason=SKIP_INTERVAL_TOO_SHORT,
                           extra_values=ev_default)

    num_delta = cur_num - prev_num
    den_delta = cur_den - prev_den

    # Update snapshot
    snapshot[prev_num_key] = str(int(cur_num))
    snapshot[prev_den_key] = str(int(cur_den))

    # Min volume check
    min_volume = alarm.get("min_volume", 0)
    if den_delta < min_volume:
        snapshot[streak_key] = "0"
        write_snapshot(snapshot_path, snapshot)
        return make_result(alarm, "skipped", skip_reason=f"low volume (delta={int(den_delta)} < {min_volume})",
                           extra_values=ev_default)

    # Compute ratio
    if den_delta == 0:
        snapshot[streak_key] = "0"
        write_snapshot(snapshot_path, snapshot)
        return make_result(alarm, "ok", value=0, threshold=alarm["ratio_threshold"],
                           extra_values=ev_default)

    ratio = num_delta / den_delta
    ratio_op = alarm.get("ratio_op", ">")
    ratio_threshold = alarm["ratio_threshold"]
    streak_threshold = alarm.get("streak_threshold", 3)

    breaching = compare(ratio, ratio_op, ratio_threshold)

    if breaching:
        streak += 1
        snapshot[streak_key] = str(streak)
        write_snapshot(snapshot_path, snapshot)

        ev = {"streak": streak, "streak_threshold": streak_threshold, "ratio_threshold": ratio_threshold}
        if streak >= streak_threshold:
            return make_result(
                alarm, "firing", value=round(ratio, 4), threshold=ratio_threshold,
                for_ticks_elapsed=streak, extra_values=ev,
            )
        return make_result(
            alarm, "breach", value=round(ratio, 4), threshold=ratio_threshold,
            for_ticks_elapsed=streak, extra_values=ev,
        )
    else:
        snapshot[streak_key] = "0"
        write_snapshot(snapshot_path, snapshot)
        ev = {"streak": 0, "streak_threshold": streak_threshold, "ratio_threshold": ratio_threshold}
        return make_result(alarm, "ok", value=round(ratio, 4), threshold=ratio_threshold, extra_values=ev)


def maybe_post_restart_fire(alarm: dict, cur_val: float) -> dict | None:
    """Post-restart absolute-value check for counter-streak alarms (#3198).

    On a streak baseline reset (PID/start_ticks change, or a counter reset), the
    cross-tick delta machine cannot observe a stall that accrued during a node's
    startup/warmup window — the burst straddles the reset tick. This evaluates
    the absolute counter value that the reset would otherwise discard: if it
    meets `post_restart_absolute_threshold` (a positive value enables the check;
    0/absent disables it), return a "firing" result keyed on the ABSOLUTE value
    with a `post_restart` marker (streak/delta semantics are meaningless on a
    fresh incarnation). Otherwise return None so the caller falls through to
    "collecting_baseline".

    Callers MUST write the fresh baseline snapshot BEFORE invoking this, so the
    next tick's streak machine stays consistent regardless of the fire path.
    """
    threshold = alarm.get("post_restart_absolute_threshold", 0)
    if not threshold or threshold <= 0:
        return None
    if cur_val < threshold:
        return None
    abs_val = int(cur_val)
    return make_result(
        alarm, "firing", value=abs_val, threshold=threshold,
        extra_values={
            "post_restart": True,
            "value": abs_val,
            "threshold": threshold,
            "streak": 0,
            "streak_threshold": alarm.get("streak_threshold", 3),
        },
    )


def eval_counter_streak(
    alarm: dict,
    current: dict,
    state_dir: Path,
    pid: str,
    start_ticks: str,
    gap_stale: bool = False,
    too_fresh: bool = False,
    now: int | None = None,
) -> dict:
    """Evaluate a counter-streak alarm.

    Independent of PREV_PROM_INVALID — uses own PID/start_ticks in snapshot.

    The streak dwell is TIME-denominated, not tick-denominated (#3790): the
    streak-opening tick records `first_breach_ts`, and the alarm fires only once
    `streak >= streak_threshold` AND `now - first_breach_ts` has reached
    `streak_threshold * expected_interval_seconds`. This makes the confirmation
    gate independent of tick cadence — a natural interval split into two
    sub-window halves (each above the #3757 too-fresh floor, so that guard
    misses them) can no longer advance the streak to its firing threshold at 2x
    wall-clock speed. `now` defaults to int(time.time()); it is a parameter for
    deterministic tests.
    """
    if now is None:
        now = int(time.time())
    ev_default = default_extra_values(alarm, "counter-streak")

    # Missing process identity guard (#3279): an abbreviated tick that skips
    # exporting PID/START_TICKS passes EMPTY strings here. An empty identity is
    # NOT a valid distinct incarnation — treating it as one makes the PID-change
    # branch below write a fresh baseline under pid="" (poison write) and
    # post-restart-fire on the discarded absolute value, then the NEXT real-PID
    # tick reads the poisoned pid="" snapshot, re-enters the PID-change branch,
    # and false-fires (post-restart) despite a stable PID and frozen counter.
    # Establish nothing, decide nothing: skip BEFORE any snapshot/metric I/O so
    # the prior valid baseline is preserved untouched.
    if not pid or not start_ticks:
        return make_result(alarm, "skipped",
                           skip_reason="missing process identity",
                           extra_values=ev_default)

    metric = alarm["metric"]
    extraction = alarm.get("extraction", "form2")
    labels = alarm.get("labels", [])

    cur_val = extract_value(current, metric, extraction, labels)
    if cur_val is None:
        return make_result(alarm, "skipped", skip_reason="metric not found",
                           extra_values=ev_default)

    snapshot_file = alarm.get("snapshot_file", "counter_streak_snapshot")
    snapshot_path = state_dir / snapshot_file
    snapshot = read_snapshot(snapshot_path)

    # Process identity check
    if snapshot:
        if snapshot.get("version") != "1":
            snapshot = {}
        elif snapshot.get("pid") != pid or snapshot.get("start_ticks") != start_ticks:
            # Process identity changed (restart) — invalidate the streak and
            # re-collect baseline. The cross-tick delta machine is blind to a
            # stall that accrued during startup/warmup because the burst spans
            # the baseline-reset tick (see #3198). Write the fresh baseline
            # snapshot FIRST so the next tick's streak machine stays consistent,
            # THEN evaluate the discarded absolute value: if it already crosses
            # post_restart_absolute_threshold, fire WARN once on this reset tick.
            new_snapshot = {
                "version": "1",
                "pid": pid,
                "start_ticks": start_ticks,
                "counter_value": str(int(cur_val)),
                "breach_streak": "0",
            }
            write_snapshot(snapshot_path, new_snapshot)
            post_restart = maybe_post_restart_fire(alarm, cur_val)
            if post_restart is not None:
                return post_restart
            return make_result(alarm, "collecting_baseline",
                               extra_values=ev_default)

    if not snapshot:
        # First tick — collecting baseline
        new_snapshot = {
            "version": "1",
            "pid": pid,
            "start_ticks": start_ticks,
            "counter_value": str(int(cur_val)),
            "breach_streak": "0",
        }
        write_snapshot(snapshot_path, new_snapshot)
        return make_result(alarm, "collecting_baseline",
                           extra_values=ev_default)

    prev_counter = int(snapshot.get("counter_value", "0"))
    streak = int(snapshot.get("breach_streak", "0"))

    # Counter reset (cur_val < prev_counter): the counter regressed without a
    # PID change (e.g. metric re-registered). cur_val here is the POST-reset
    # absolute value, not a delta — re-collect baseline. Mirror the PID-change
    # branch: write the fresh baseline FIRST, then evaluate the discarded
    # absolute value for a post-restart fire (#3198). The fire here uses the
    # absolute value, so it is not double-counting a delta.
    if cur_val < prev_counter:
        new_snapshot = {
            "version": "1",
            "pid": pid,
            "start_ticks": start_ticks,
            "counter_value": str(int(cur_val)),
            "breach_streak": "0",
        }
        write_snapshot(snapshot_path, new_snapshot)
        post_restart = maybe_post_restart_fire(alarm, cur_val)
        if post_restart is not None:
            return post_restart
        return make_result(alarm, "collecting_baseline",
                           extra_values=ev_default)

    # Gap-stale (#3246): the snapshot baseline is SAME-PID (the identity check
    # above passed) but hours old — the monitor loop stalled across a long
    # wall-clock gap while the process survived. The headline tick-184
    # recovery-stalled false-fire lives here: counter jumped 0→703 over the gap,
    # delta >= burst_threshold would fire WARN as an acute burst. RE-BASELINE
    # instead: rewrite the snapshot to the current value and reset the streak to
    # "0" (mirrors the counter-reset branch above) — this snapshot PERSISTS
    # across ticks, so a plain skip would leave the stale baseline and fire on
    # the NEXT tick. Unlike the PID-change / counter-reset branches, do NOT
    # invoke maybe_post_restart_fire: gap-stale is not a restart — the counts
    # accrued across normal operation, not a startup burst, so the absolute
    # value must not acute-fire. Sequenced after the identity check, so a PID
    # change (#3206) is handled there and never double-handled here.
    if gap_stale:
        new_snapshot = {
            "version": "1",
            "pid": pid,
            "start_ticks": start_ticks,
            "counter_value": str(int(cur_val)),
            "breach_streak": "0",
        }
        write_snapshot(snapshot_path, new_snapshot)
        return make_result(alarm, "collecting_baseline",
                           extra_values=ev_default)

    # Too-fresh (#3757): a duplicate/back-to-back tick sampled an inter-scrape
    # interval below MIN_EVAL_WINDOW_SECONDS. The snapshot baseline is still
    # VALID (identity matched, no counter reset, not gap-stale) — the duplicate
    # must be a pure no-op. Return a NON-destructive skip WITHOUT writing the
    # snapshot, so counter_value and breach_streak are preserved untouched and
    # the next normal-cadence tick computes the delta over the real interval.
    # This is the headline harm from #3757: a watchdog duplicate would otherwise
    # advance (delta>=1) or reset (delta=0) the streak and burn the alarm
    # cooldown. Sequenced after the identity / counter-reset / gap_stale branches
    # (a restart is always handled first) and before the delta computation.
    if too_fresh:
        return make_result(alarm, "skipped", skip_reason=SKIP_INTERVAL_TOO_SHORT,
                           extra_values=ev_default)

    delta = int(cur_val) - prev_counter
    delta_threshold = alarm.get("delta_threshold", 1)
    streak_threshold = alarm.get("streak_threshold", 3)
    burst_threshold = alarm.get("burst_threshold", 10)
    # The DESIGN cadence the streak_threshold was calibrated against (#3790):
    # streak_threshold * expected_interval_seconds is the wall-clock dwell a
    # sustained breach must survive before firing. Deliberately the *design*
    # cadence (below the real ~44 min tick cadence), so the healthy sustained
    # case still fires on the same tick as before; only a compressed/duplicate
    # cadence that would advance the count at 2x wall-clock speed is held.
    expected_interval_seconds = alarm.get("expected_interval_seconds", 1200)

    # first_breach_ts anchors the streak to wall-clock time. It is set on the
    # streak-opening (0->1) tick and preserved verbatim as the streak advances.
    # A legacy snapshot carrying breach_streak>=1 but no first_breach_ts (written
    # before #3790) re-anchors to `now` here — a one-time conservative delay,
    # never a spurious fire off a stale unanchored streak. Every reset branch
    # above rewrites the snapshot WITHOUT this key, which clears it.
    prev_first_breach_ts_str = snapshot.get("first_breach_ts")

    def _anchor(prev_streak: int) -> int:
        """Resolve first_breach_ts for a breaching tick.

        Anchor to `now` when the streak is opening (prev_streak == 0) or when a
        legacy snapshot advanced a streak with no persisted anchor; otherwise
        preserve the existing anchor.
        """
        if prev_streak == 0 or prev_first_breach_ts_str is None:
            return now
        try:
            return int(prev_first_breach_ts_str)
        except ValueError:
            return now

    ev = {"streak": streak, "streak_threshold": streak_threshold}

    if delta >= burst_threshold:
        first_breach_ts = _anchor(streak)
        streak += 1
        new_snapshot = {
            "version": "1",
            "pid": pid,
            "start_ticks": start_ticks,
            "counter_value": str(int(cur_val)),
            "breach_streak": str(streak),
            "first_breach_ts": str(first_breach_ts),
        }
        write_snapshot(snapshot_path, new_snapshot)
        # Acute burst is a single-tick spike — fire immediately, ungated by dwell.
        return make_result(
            alarm, "firing", value=delta, threshold=burst_threshold,
            for_ticks_elapsed=streak, extra_values={"streak": streak, "streak_threshold": streak_threshold},
        )

    if delta >= delta_threshold:
        first_breach_ts = _anchor(streak)
        streak += 1
        new_snapshot = {
            "version": "1",
            "pid": pid,
            "start_ticks": start_ticks,
            "counter_value": str(int(cur_val)),
            "breach_streak": str(streak),
            "first_breach_ts": str(first_breach_ts),
        }
        write_snapshot(snapshot_path, new_snapshot)

        dwell_required = streak_threshold * expected_interval_seconds
        dwell_elapsed = now - first_breach_ts
        ev_streak = {
            "streak": streak,
            "streak_threshold": streak_threshold,
            "dwell_elapsed": dwell_elapsed,
            "dwell_required": dwell_required,
        }
        # Fire only when BOTH the count AND the wall-clock dwell are satisfied.
        # If the count is met but the dwell is not, keep dwelling (breach): the
        # snapshot (with first_breach_ts) is preserved because a "breach" return
        # never triggers maybe_reset_counter_snapshot (which only acts on
        # "skipped").
        if streak >= streak_threshold and dwell_elapsed >= dwell_required:
            return make_result(
                alarm, "firing", value=delta, threshold=delta_threshold,
                for_ticks_elapsed=streak, extra_values=ev_streak,
            )
        return make_result(
            alarm, "breach", value=delta, threshold=delta_threshold,
            for_ticks_elapsed=streak, extra_values=ev_streak,
        )

    # delta == 0 — reset the streak and clear the wall-clock anchor.
    new_snapshot = {
        "version": "1",
        "pid": pid,
        "start_ticks": start_ticks,
        "counter_value": str(int(cur_val)),
        "breach_streak": "0",
    }
    write_snapshot(snapshot_path, new_snapshot)
    return make_result(alarm, "ok", value=delta, threshold=delta_threshold, extra_values={"streak": 0, "streak_threshold": streak_threshold})


# ── Aggregate line rendering ────────────────────────────────────────────────

def render_aggregate(results: list[dict], watcher_mode: bool) -> dict:
    """Render aggregate status lines from alarm results."""
    # metrics line
    metrics_alarms = [r for r in results if r["contributes_to"] == "metrics"]
    firing = [r for r in metrics_alarms if r["state"] == "firing"]
    # Cooldown-suppressed alarms (#3762): a firing alarm still inside its dedup
    # window, flipped by apply_cooldowns. They are excluded from the firing
    # count so they neither flip the banner nor get filed (the agent only files
    # state=="firing"), and surfaced separately for visibility.
    cooldown = [r for r in metrics_alarms if r["state"] == "cooldown"]
    skipped = [r for r in metrics_alarms if r["state"] == "skipped"]
    total = len(metrics_alarms)
    metrics_line = f"metrics: {len(firing)}/{total} firing"
    if cooldown:
        metrics_line += f", {len(cooldown)} in cooldown"
    if skipped:
        skip_reasons = set(r.get("skip_reason", "") for r in skipped)
        metrics_line += f", {len(skipped)} skipped ({', '.join(r for r in skip_reasons if r)})"

    # metrics_ratio line
    ratio_alarms = [r for r in results if r["contributes_to"] == "metrics_ratio"]
    if not ratio_alarms or watcher_mode:
        metrics_ratio_line = None
    else:
        # Check for global skip (all ratio alarms skipped for same reason)
        all_skipped = all(r["state"] == "skipped" for r in ratio_alarms)
        all_baseline = all(r["state"] == "collecting_baseline" for r in ratio_alarms)
        if all_skipped:
            reasons = set(r.get("skip_reason", "") for r in ratio_alarms)
            metrics_ratio_line = f"metrics_ratio: skipped ({', '.join(r for r in reasons if r)})"
        elif all_baseline:
            metrics_ratio_line = "metrics_ratio: collecting baseline"
        else:
            parts = []
            name_map = {
                "scp-accept-rate-low": "scp",
                "apply-failure-ratio": "apply",
                "pending-too-old-ratio": "pending",
            }
            for r in ratio_alarms:
                short = name_map.get(r["name"], r["name"])
                if r["state"] == "firing":
                    parts.append(f"{short} WARNING {r['details']}")
                elif r["state"] == "breach":
                    parts.append(f"{short} breach ({r['details']})")
                elif r["state"] == "skipped":
                    parts.append(f"{short} skipped ({r.get('skip_reason', '')})")
                elif r["state"] == "collecting_baseline":
                    parts.append(f"{short} collecting baseline")
                else:
                    val = r.get("value", 0)
                    val_pct = f"{val:.0%}" if isinstance(val, float) and val < 1 else str(val)
                    parts.append(f"{short} ok ({val_pct})")
            metrics_ratio_line = f"metrics_ratio: {', '.join(parts)}"

    # recovery_stalled line
    stalled_alarms = [r for r in results if r["contributes_to"] == "recovery_stalled"]
    if not stalled_alarms or watcher_mode:
        recovery_stalled_line = None
    else:
        r = stalled_alarms[0]
        if r["state"] == "firing":
            if r.get("post_restart"):
                # Baseline-reset absolute fire (#3198/#3206): the value is an
                # absolute counter, not a cross-tick delta, so label it
                # (post-restart) per the documented form (monitor-tick SKILL.md
                # L1335). MUST precede the value>=10 burst check, since a
                # post-restart absolute (>= threshold 50) is always >= 10 (#3274).
                recovery_stalled_line = f"recovery_stalled: WARNING absolute={r['value']} (post-restart) — investigating"
            elif r.get("value", 0) >= 10:
                recovery_stalled_line = f"recovery_stalled: WARNING delta={r['value']} (burst) — investigating"
            else:
                streak = r.get("for_ticks_elapsed", 0)
                recovery_stalled_line = f"recovery_stalled: WARNING delta={r['value']} ({streak} ticks) — investigating"
        elif r["state"] == "breach":
            streak = r.get("for_ticks_elapsed", 0)
            recovery_stalled_line = f"recovery_stalled: breach (delta={r['value']}, streak {streak}/3)"
        elif r["state"] == "skipped":
            recovery_stalled_line = f"recovery_stalled: skipped ({r.get('skip_reason', '')})"
        elif r["state"] == "collecting_baseline":
            recovery_stalled_line = "recovery_stalled: collecting baseline"
        else:
            recovery_stalled_line = f"recovery_stalled: ok (delta={r.get('value', 0)})"

    return {
        "metrics_line": metrics_line,
        "metrics_ratio_line": metrics_ratio_line,
        "recovery_stalled_line": recovery_stalled_line,
    }


# ── Schema validation ───────────────────────────────────────────────────────

def validate_catalog(catalog: dict) -> list[str]:
    """Validate the TOML catalog schema. Returns list of errors."""
    errors: list[str] = []

    version = catalog.get("schema_version")
    if version != SCHEMA_VERSION:
        errors.append(f"Unknown schema_version: {version} (expected {SCHEMA_VERSION})")
        return errors

    alarms = catalog.get("alarm", [])
    names_seen: set[str] = set()
    cooldown_keys_seen: set[str] = set()

    for i, alarm in enumerate(alarms):
        name = alarm.get("name", f"<unnamed-{i}>")

        # Duplicate name check
        if name in names_seen:
            errors.append(f"Duplicate alarm name: {name}")
        names_seen.add(name)

        # Exempt validation
        exempt = alarm.get("exempt", False)
        exempt_reason = alarm.get("exempt_reason", "")
        if exempt and not exempt_reason:
            errors.append(f"{name}: exempt=true requires non-empty exempt_reason")
        if not exempt and exempt_reason:
            errors.append(f"{name}: exempt_reason without exempt=true")

        # baseline_version validation (optional, defaults to 1)
        baseline_version = alarm.get("baseline_version")
        if baseline_version is not None:
            if not isinstance(baseline_version, int) or isinstance(baseline_version, bool) or baseline_version < 1:
                errors.append(f"{name}: invalid baseline_version '{baseline_version}' (must be integer >= 1)")

        # semantic_change_date validation (optional, ISO 8601 UTC)
        semantic_change_date = alarm.get("semantic_change_date")
        if semantic_change_date is not None:
            import re
            if not isinstance(semantic_change_date, str) or not re.match(r'^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$', semantic_change_date):
                errors.append(f"{name}: invalid semantic_change_date '{semantic_change_date}' (must be YYYY-MM-DDTHH:MM:SSZ)")

        # silence_expected validation (optional, must be boolean)
        silence_expected = alarm.get("silence_expected")
        if silence_expected is not None:
            if not isinstance(silence_expected, bool):
                errors.append(f"{name}: invalid silence_expected '{silence_expected}' (must be boolean)")

        # Required fields
        kind = alarm.get("kind")
        if kind not in VALID_KINDS:
            errors.append(f"{name}: invalid kind '{kind}'")
            continue

        severity = alarm.get("severity")
        if severity not in VALID_SEVERITIES:
            errors.append(f"{name}: invalid severity '{severity}'")

        # Gate validation
        for gate in alarm.get("gates", []):
            if gate not in VALID_GATES:
                errors.append(f"{name}: invalid gate '{gate}'")

        # Duplicate cooldown_key check
        ck = alarm.get("cooldown_key", name)
        if not alarm.get("allow_duplicate_cooldown") and ck in cooldown_keys_seen:
            errors.append(f"{name}: duplicate cooldown_key '{ck}'")
        cooldown_keys_seen.add(ck)

        # Kind-specific validation
        if kind == "gauge":
            if "metric" not in alarm:
                errors.append(f"{name}: gauge requires 'metric'")
            if "op" not in alarm or alarm["op"] not in VALID_OPS:
                errors.append(f"{name}: gauge requires valid 'op'")
            if "threshold" not in alarm:
                errors.append(f"{name}: gauge requires 'threshold'")
            # Optional companion-metric guard (#3752): if any guard_* field is
            # present, the guard must be fully specified so it cannot be
            # silently ignored at evaluation time.
            if any(k in alarm for k in ("guard_metric", "guard_op", "guard_threshold", "guard_extraction")):
                if "guard_metric" not in alarm:
                    errors.append(f"{name}: gauge guard requires 'guard_metric'")
                if "guard_op" not in alarm or alarm["guard_op"] not in VALID_OPS:
                    errors.append(f"{name}: gauge guard requires valid 'guard_op'")
                if "guard_threshold" not in alarm:
                    errors.append(f"{name}: gauge guard requires 'guard_threshold'")
        elif kind == "gauge-ratio":
            for field in ("numerator_metric", "denominator_metric", "op", "threshold"):
                if field not in alarm:
                    errors.append(f"{name}: gauge-ratio requires '{field}'")
        elif kind == "counter":
            if "metric" not in alarm and "metric_sum" not in alarm:
                errors.append(f"{name}: counter requires 'metric' or 'metric_sum'")
            if "op" not in alarm:
                errors.append(f"{name}: counter requires 'op'")
            if "threshold" not in alarm:
                errors.append(f"{name}: counter requires 'threshold'")
        elif kind == "counter-dynamic":
            if "metric_sum" not in alarm:
                errors.append(f"{name}: counter-dynamic requires 'metric_sum'")
            if "multiplier" not in alarm:
                errors.append(f"{name}: counter-dynamic requires 'multiplier'")
        elif kind == "counter-ratio":
            if "numerator" not in alarm and "numerator_sum" not in alarm:
                errors.append(f"{name}: counter-ratio requires 'numerator' or 'numerator_sum'")
            if "denominator" not in alarm and "denominator_sum" not in alarm:
                errors.append(f"{name}: counter-ratio requires 'denominator' or 'denominator_sum'")
            if "ratio_threshold" not in alarm:
                errors.append(f"{name}: counter-ratio requires 'ratio_threshold'")
        elif kind == "histogram-p99":
            if "metric" not in alarm:
                errors.append(f"{name}: histogram-p99 requires 'metric'")
            if "p99_threshold" not in alarm:
                errors.append(f"{name}: histogram-p99 requires 'p99_threshold'")
        elif kind == "histogram-bucket-rate":
            if "metric" not in alarm:
                errors.append(f"{name}: histogram-bucket-rate requires 'metric'")
            if "bucket_le" not in alarm:
                errors.append(f"{name}: histogram-bucket-rate requires 'bucket_le'")
            if "rate_threshold" not in alarm:
                errors.append(f"{name}: histogram-bucket-rate requires 'rate_threshold'")
        elif kind == "counter-streak":
            if "metric" not in alarm:
                errors.append(f"{name}: counter-streak requires 'metric'")
            if "delta_threshold" not in alarm:
                errors.append(f"{name}: counter-streak requires 'delta_threshold'")
            if "streak_threshold" not in alarm:
                errors.append(f"{name}: counter-streak requires 'streak_threshold'")
            if "burst_threshold" not in alarm:
                errors.append(f"{name}: counter-streak requires 'burst_threshold'")

    return errors


# ── Main ─────────────────────────────────────────────────────────────────────

def main() -> int:
    parser = argparse.ArgumentParser(description="Evaluate monitor-tick alarms")
    parser.add_argument("--catalog", required=True, help="Path to metric-alarms.toml")
    parser.add_argument("--current", default=None, help="Path to current.prom")
    parser.add_argument("--prev", default=None, help="Path to prev.prom")
    parser.add_argument("--state-dir", default=None, help="Directory for snapshot files")
    parser.add_argument("--validate-only", action="store_true", help="Only validate schema, don't evaluate")
    parser.add_argument(
        "--no-snapshot-write",
        action="store_true",
        help="Evaluate without persisting any snapshot/state files (read-only "
             "dry-run for diagnostic/repeat invocations within a tick)",
    )
    parser.add_argument(
        "--cooldown-file",
        default=None,
        help="Path to the alarm-dedup JSON (anomaly_cooldown.json). When set, "
             "the evaluator suppresses metrics-family alarms still inside their "
             "cooldown window (state=cooldown) and persists last_fired for the "
             "ones it reports firing — removing the manual JSON edit (#3762).",
    )
    parser.add_argument(
        "--now",
        type=int,
        default=None,
        help="Unix timestamp used as 'now' for cooldown-window math "
             "(defaults to int(time.time())); primarily for deterministic tests.",
    )
    args = parser.parse_args()

    # Gate the single write_snapshot() chokepoint for the whole process.
    global _NO_SNAPSHOT_WRITE
    _NO_SNAPSHOT_WRITE = args.no_snapshot_write

    # Validate-only mode only needs --catalog
    if not args.validate_only:
        if not args.current or not args.state_dir:
            parser.error("--current and --state-dir are required unless --validate-only is set")

    # Read catalog
    catalog_path = Path(args.catalog)
    if not catalog_path.exists():
        print(f"ERROR: catalog not found: {catalog_path}", file=sys.stderr)
        return 1

    with open(catalog_path, "rb") as f:
        catalog = tomllib.load(f)

    # Validate schema
    errors = validate_catalog(catalog)
    if errors:
        for e in errors:
            print(f"SCHEMA ERROR: {e}", file=sys.stderr)
        return 1

    if args.validate_only:
        print(json.dumps({"schema_version": SCHEMA_VERSION, "valid": True, "alarm_count": len(catalog.get("alarm", []))}))
        return 0

    # Read env vars
    prev_prom_invalid = os.environ.get("PREV_PROM_INVALID", "false").lower() == "true"
    # Gap-staleness (#3246): the baseline (prev.prom for the file families,
    # the persisted PID/start_ticks snapshot for the snapshot families) is
    # SAME-PID-but-old when the monitor loop stalled across a long wall-clock
    # gap while the process survived. PID-change (#3206) is handled FIRST via
    # PREV_PROM_INVALID / the snapshot PID-mismatch reset; gap-stale is the
    # distinct same-PID case, sequenced AFTER (never double-handled). Age -1 /
    # absent ⇒ unknown ⇒ NOT stale (fail-safe; truly-missing identity is
    # already covered by PREV_PROM_INVALID).
    try:
        prev_scrape_age = int(os.environ.get("PREV_SCRAPE_AGE_SECONDS", "-1"))
    except ValueError:
        prev_scrape_age = -1
    try:
        gap_stale_threshold = int(os.environ.get("GAP_STALE_THRESHOLD_SECONDS", "3600"))
    except ValueError:
        gap_stale_threshold = 3600
    gap_stale = prev_scrape_age >= 0 and prev_scrape_age >= gap_stale_threshold
    gap_stale_age_hours = round(prev_scrape_age / 3600.0, 1) if gap_stale else 0
    # Too-fresh (#3757): the symmetric LOWER bound to gap_stale. When the prev
    # baseline is younger than MIN_EVAL_WINDOW_SECONDS, a duplicate/back-to-back
    # tick (e.g. the watchdog firing into a still-running interactive tick)
    # sampled too short an interval to carry a meaningful cross-tick delta. The
    # counter-family evaluators then return a NON-destructive skip that PRESERVES
    # the streak/ratio/dynamic snapshot instead of resetting or advancing it.
    try:
        min_eval_window = int(os.environ.get("MIN_EVAL_WINDOW_SECONDS", "600"))
    except ValueError:
        min_eval_window = 600
    too_fresh = compute_too_fresh(prev_scrape_age, min_eval_window)
    warmup_remaining = int(os.environ.get("WARMUP_TICKS_REMAINING", "0"))
    fresh_start = os.environ.get("FRESH_START", "no").lower() == "yes"
    crash_recovery = os.environ.get("CRASH_RECOVERY", "no").lower() == "yes"
    uptime = int(os.environ.get("UPTIME_SECONDS", "9999"))
    monitor_mode = os.environ.get("MONITOR_MODE", "validator")
    pid = os.environ.get("PID", "")
    start_ticks_val = os.environ.get("START_TICKS", "")

    # Loud caller-error warning (#3758): a validator-mode tick with no
    # PID/START_TICKS is a monitoring-side bug (an abbreviated/headless tick
    # that skipped the `export PID=... START_TICKS=...` preamble), not a node
    # state. It silently degrades the counter-streak/ratio families to
    # `skipped` — which reads as health in a tick report. Emit a single loud
    # stderr line so the run is unmistakably flagged. Non-breaking: no exit-code
    # change and no new alarm state (the stronger form — non-zero exit or a
    # distinct FAILED aggregate state — is deliberately deferred, see #3758).
    if monitor_mode == "validator" and (not pid or not start_ticks_val):
        print(
            "WARNING: validator-mode tick has missing process identity "
            "(PID/START_TICKS unset) — counter-streak/ratio alarms will be "
            "skipped, not evaluated. This is a monitoring-side caller bug, not "
            "node health. Export PID and START_TICKS before invoking the "
            "evaluator (see SKILL.md).",
            file=sys.stderr,
        )

    # Parse metrics
    current_path = Path(args.current)
    prev_path = Path(args.prev) if args.prev else None
    current = parse_prom(current_path)
    prev = parse_prom(prev_path)

    state_dir = Path(args.state_dir)
    # Reject a relative --state-dir (#3201). A relative path resolves against the
    # caller's cwd; when invoked from the repo root with e.g. `--state-dir metrics`
    # it drops state files (gauge_persistence, scrape_identity, ...) into the
    # tracked metrics/ dir, dirtying the working tree and hard-blocking the deploy
    # gate. Reject (not abspath) — abspath("metrics") from the repo root still
    # lands in the tree, so only rejection forces an absolute path under ~/data.
    if not state_dir.is_absolute():
        print(
            f"ERROR: --state-dir must be an absolute path, got relative: {args.state_dir}",
            file=sys.stderr,
        )
        return 1
    state_dir.mkdir(parents=True, exist_ok=True)

    # Persistence state for gauge for_ticks
    persist_path = state_dir / "gauge_persistence"
    persistence_state = read_snapshot(persist_path)

    alarms = catalog.get("alarm", [])
    results: list[dict] = []

    # Single wall-clock reference for the whole tick: the counter-streak
    # time-dwell gate (#3790) and the cooldown-window math below share it, so
    # both see the same `now`. `--now` overrides int(time.time()) for
    # deterministic tests. Nothing between here and either consumer mutates it.
    now = args.now if args.now is not None else int(time.time())

    for alarm in alarms:
        name = alarm["name"]
        kind = alarm["kind"]

        # --- Determine result (all branches fall through to post-processing) ---
        if alarm.get("exempt", False):
            reason = alarm.get("exempt_reason", "exempt")
            result = make_result(alarm, "skipped", skip_reason=f"exempt: {reason}",
                                 extra_values=default_extra_values(alarm, kind))
        else:
            gates = alarm.get("gates", [])
            passed, skip_reason = gates_pass(
                gates, warmup_remaining, fresh_start, crash_recovery, uptime, monitor_mode,
            )
            if not passed:
                result = make_result(alarm, "skipped", skip_reason=skip_reason,
                                     extra_values=default_extra_values(alarm, kind))
            elif kind == "gauge":
                result = eval_gauge(alarm, current, persistence_state, prev_prom_invalid)
            elif kind == "gauge-ratio":
                result = eval_gauge_ratio(alarm, current, persistence_state, prev_prom_invalid)
            elif kind == "counter":
                result = eval_counter(alarm, current, prev, prev_prom_invalid, warmup_remaining,
                                      gap_stale, gap_stale_age_hours, too_fresh)
            elif kind == "counter-dynamic":
                result = eval_counter_dynamic(alarm, current, prev, state_dir, prev_prom_invalid, warmup_remaining,
                                              gap_stale, gap_stale_age_hours, too_fresh)
            elif kind == "histogram-p99":
                result = eval_histogram_p99(alarm, current, prev, prev_prom_invalid,
                                            gap_stale, gap_stale_age_hours, too_fresh)
            elif kind == "histogram-bucket-rate":
                result = eval_histogram_bucket_rate(alarm, current, prev, prev_prom_invalid,
                                                    gap_stale, gap_stale_age_hours, too_fresh)
            elif kind == "counter-ratio":
                result = eval_counter_ratio(
                    alarm, current, prev, state_dir, pid, start_ticks_val,
                    fresh_start, crash_recovery, uptime, gap_stale, too_fresh,
                )
            elif kind == "counter-streak":
                result = eval_counter_streak(alarm, current, state_dir, pid, start_ticks_val,
                                             gap_stale, too_fresh, now=now)
            else:
                result = make_result(alarm, "skipped", skip_reason=f"unknown kind: {kind}")

        # --- Single converged post-processing ---
        results.append(result)
        maybe_reset_gauge_persistence(alarm, kind, result["state"], persistence_state)
        # Pass the skip reason so the cleanup hook can exempt monitoring-side
        # caller errors ("missing process identity", "interval too short") from
        # destroying the baseline — see PRESERVE_BASELINE_SKIP_REASONS / #3758.
        maybe_reset_counter_snapshot(
            alarm, kind, result["state"], state_dir, result.get("skip_reason"),
        )

        # Telemetry (unified format for all branches)
        metric = telemetry_metric(alarm, kind)
        n = count_series(current, metric) if metric else 0
        state = result["state"]
        if n == 0 and state != "skipped":
            print(f"# alarm={name} metric={metric} series_matched=0 state=ERROR_NO_SERIES", file=sys.stderr)
        else:
            print(f"# alarm={name} metric={metric} series_matched={n} state={state}", file=sys.stderr)

    # Save gauge persistence state
    write_snapshot(persist_path, persistence_state)

    # Alarm-cooldown lifecycle (#3762): when a dedup file is supplied, the tool
    # owns read/suppress/write so the manual JSON edit can be deleted. Absent
    # --cooldown-file, this is a no-op and behavior is byte-for-byte legacy.
    if args.cooldown_file:
        cooldown_path = Path(args.cooldown_file)
        cooldown_data = read_cooldown_file(cooldown_path)
        apply_cooldowns(results, cooldown_data, now)
        write_cooldown_file(cooldown_path, cooldown_data)

    watcher_mode = monitor_mode == "watcher"
    aggregate = render_aggregate(results, watcher_mode)

    output = {
        "schema_version": SCHEMA_VERSION,
        "alarms": results,
        "aggregate": aggregate,
        "watcher_mode": watcher_mode,
    }

    json.dump(output, sys.stdout, indent=2)
    print()  # trailing newline
    return 0


if __name__ == "__main__":
    sys.exit(main())
