#!/usr/bin/env python3
"""Check that baseline_version is bumped when semantic alarm fields change.

Compares old vs new versions of metric-alarms.toml and emits GitHub Actions
annotations when semantic fields changed but baseline_version was not bumped.

Exit codes:
  0 — success (warnings are advisory, not failures)
  1 — hard error (parse failure, decreased version, duplicate names, unknown fields)
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib  # type: ignore[no-redef]

# ── Field classification ─────────────────────────────────────────────────────
# Every alarm field must be in exactly one set. Unknown fields → exit 1.

SEMANTIC_FIELDS = frozenset({
    "metric", "metric_sum", "kind", "extraction",
    "labels", "expected_labels", "expected_suffixes",
    "op", "threshold", "multiplier", "min_absolute",
    "gates", "for_ticks",
    "burst_threshold", "delta_threshold", "streak_threshold",
    "post_restart_absolute_threshold",
    "denominator", "denominator_metric", "denominator_sum",
    "denominator_extraction", "denominator_includes_numerator",
    "numerator", "numerator_metric", "numerator_sum", "numerator_extraction",
    "ratio_op", "ratio_threshold", "absent_denominator",
    "p99_threshold", "mean_threshold",
    "bucket_le", "rate_threshold",
    "min_volume", "min_count_delta",
    "snapshot_file", "optional_counters",
    "exempt", "exempt_reason", "allow_duplicate_cooldown",
    "guard_metric", "guard_op", "guard_threshold", "guard_extraction",
})

NON_SEMANTIC_FIELDS = frozenset({
    "name",
    "notes", "filing_title", "filing_search", "summary", "details",
    "severity", "cooldown_key", "cooldown_seconds",
    "baseline_version", "semantic_change_date",
    "silence_expected",
})

ALL_KNOWN_FIELDS = SEMANTIC_FIELDS | NON_SEMANTIC_FIELDS

# List fields that should be compared order-insensitively (sorted).
ORDER_INSENSITIVE_LISTS = frozenset({
    "gates", "expected_labels", "expected_suffixes",
    "metric_sum", "optional_counters", "denominator_sum", "numerator_sum",
})


def parse_toml(path: str) -> dict:
    """Parse a TOML file, returning empty dict for empty/missing files."""
    content = Path(path).read_bytes()
    if not content.strip():
        return {}
    return tomllib.loads(content.decode())


def build_alarm_map(alarms: list[dict]) -> tuple[dict[str, dict], list[str]]:
    """Build {name → alarm_dict} map. Returns (map, list of errors)."""
    result: dict[str, dict] = {}
    errors: list[str] = []
    for alarm in alarms:
        name = alarm.get("name")
        if not name:
            errors.append("Alarm entry missing 'name' field")
            continue
        if name in result:
            errors.append(f"Duplicate alarm name: '{name}'")
            continue
        result[name] = alarm
    return result, errors


def check_unknown_fields(alarms: list[dict]) -> list[str]:
    """Check for fields not in either classification set."""
    errors: list[str] = []
    for alarm in alarms:
        name = alarm.get("name", "<unnamed>")
        for field in alarm:
            if field not in ALL_KNOWN_FIELDS:
                errors.append(
                    f"Alarm '{name}': unknown field '{field}' — "
                    f"add it to SEMANTIC_FIELDS or NON_SEMANTIC_FIELDS in check-alarm-versions.py"
                )
    return errors


def normalize_value(field: str, value):
    """Normalize a field value for comparison."""
    if field == "labels" and isinstance(value, list):
        # labels is a list of {key, value} tables → sorted (key, value) tuples
        return tuple(sorted((d.get("key", ""), d.get("value", "")) for d in value))
    if field in ORDER_INSENSITIVE_LISTS and isinstance(value, list):
        return tuple(sorted(str(v) for v in value))
    return value


def get_semantic_snapshot(alarm: dict) -> dict:
    """Extract normalized semantic fields from an alarm."""
    snapshot = {}
    for field in SEMANTIC_FIELDS:
        if field in alarm:
            snapshot[field] = normalize_value(field, alarm[field])
    return snapshot


def annotation(level: str, msg: str, file: str = "", line: int = 0) -> str:
    """Format a GitHub Actions annotation."""
    loc = ""
    if file:
        loc += f" file={file}"
    if line:
        loc += f",line={line}"
    return f"::{level}{loc}::{msg}"


def check_alarm_versions(old_path: str, new_path: str) -> tuple[list[str], list[str], list[str], bool]:
    """Compare old and new alarm files.

    Returns (errors, warnings, notices, has_hard_errors).
    """
    errors: list[str] = []
    warnings: list[str] = []
    notices: list[str] = []

    # Parse files
    try:
        old_data = parse_toml(old_path)
    except Exception as e:
        return ([f"Failed to parse old file: {e}"], [], [], True)

    try:
        new_data = parse_toml(new_path)
    except Exception as e:
        return ([f"Failed to parse new file: {e}"], [], [], True)

    old_alarms = old_data.get("alarm", [])
    new_alarms = new_data.get("alarm", [])

    # Validate alarm structure (must be list of dicts, i.e. [[alarm]] not [alarm])
    for label, alarms in [("Old file", old_alarms), ("New file", new_alarms)]:
        if not isinstance(alarms, list):
            errors.append(f"{label}: 'alarm' must be an array of tables ([[alarm]]), got {type(alarms).__name__}")
            continue
        for i, entry in enumerate(alarms):
            if not isinstance(entry, dict):
                errors.append(f"{label}: alarm entry {i} is not a table (got {type(entry).__name__})")

    if errors:
        return (errors, warnings, notices, True)

    # Check for unknown fields in both old and new files
    for label, alarms in [("Old file", old_alarms), ("New file", new_alarms)]:
        unknown_errors = check_unknown_fields(alarms)
        errors.extend(f"{label}: {e}" for e in unknown_errors)

    # Build maps
    old_map, old_errs = build_alarm_map(old_alarms)
    new_map, new_errs = build_alarm_map(new_alarms)
    errors.extend(f"Old file: {e}" for e in old_errs)
    errors.extend(f"New file: {e}" for e in new_errs)

    if errors:
        return (errors, warnings, notices, True)

    alarm_file = ".claude/skills/shared/metric-alarms.toml"

    # Compare alarms present in both old and new
    for name in sorted(new_map.keys()):
        if name not in old_map:
            continue  # New alarm, no bump needed

        old_alarm = old_map[name]
        new_alarm = new_map[name]

        old_semantic = get_semantic_snapshot(old_alarm)
        new_semantic = get_semantic_snapshot(new_alarm)

        if old_semantic == new_semantic:
            continue  # No semantic change

        # Semantic fields changed — check baseline_version
        old_version = old_alarm.get("baseline_version", 1)
        new_version = new_alarm.get("baseline_version", 1)

        # Find changed fields for the message
        changed_fields = []
        all_fields = set(old_semantic.keys()) | set(new_semantic.keys())
        for field in sorted(all_fields):
            if old_semantic.get(field) != new_semantic.get(field):
                changed_fields.append(field)

        if new_version < old_version:
            errors.append(
                f"Alarm '{name}': baseline_version decreased "
                f"(old={old_version}, new={new_version}) — version must never decrease"
            )
            continue

        if new_version <= old_version:
            warnings.append(
                f"Alarm '{name}': semantic field(s) changed ({', '.join(changed_fields)}) "
                f"but baseline_version not bumped (old={old_version}, new={new_version})"
            )
        else:
            # Version was bumped — check semantic_change_date
            old_scd = old_alarm.get("semantic_change_date")
            new_scd = new_alarm.get("semantic_change_date")
            if new_scd is None or new_scd == old_scd:
                notices.append(
                    f"Alarm '{name}': baseline_version bumped to {new_version} "
                    f"but semantic_change_date not updated — consider setting it "
                    f"to the deployment date"
                )

    return (errors, warnings, notices, len(errors) > 0)


def main() -> int:
    if len(sys.argv) != 3:
        print(f"Usage: {sys.argv[0]} <old-file> <new-file>", file=sys.stderr)
        return 1

    old_path, new_path = sys.argv[1], sys.argv[2]

    errors, warnings, notices, has_hard_errors = check_alarm_versions(old_path, new_path)

    alarm_file = ".claude/skills/shared/metric-alarms.toml"

    # Emit annotations
    for msg in errors:
        print(annotation("error", msg, file=alarm_file))
    for msg in warnings:
        print(annotation("warning", msg, file=alarm_file))
    for msg in notices:
        print(annotation("notice", msg, file=alarm_file))

    # Summary
    print(f"\nAlarm version check: {len(errors)} error(s), {len(warnings)} warning(s), {len(notices)} notice(s)")

    # Set GitHub Actions output
    github_output = os.environ.get("GITHUB_OUTPUT")
    if github_output:
        with open(github_output, "a") as f:
            f.write(f"has_warnings={'true' if warnings else 'false'}\n")
            f.write(f"has_errors={'true' if errors else 'false'}\n")

    if has_hard_errors:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
