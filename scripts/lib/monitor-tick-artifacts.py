#!/usr/bin/env python3
"""Single-writer helper for monitor-tick per-tick artifacts.

This module is the SOLE constructor of every per-tick artifact the monitor
emits: the `tick-history.jsonl` row, the archive `metadata.env` sidecar, the
`scrape_identity` file, and the `counter_streak_snapshot` file. The
monitor-tick SKILL calls these subcommands instead of hand-rolling heredocs,
so every artifact is conformant *by construction* (see issue #3791; follow-up
to #3757's schema-fragmentation census).

Why a single writer: `tick-history.jsonl` had fragmented into 100+ distinct
key-signatures because each tick path (interactive vs headless) hand-rolled its
own row, and `warnings` accumulated 571 spellings for ~17 real conditions. The
fix is to make one program own the schema, the `ts` format, and the closed
`warnings`/`actions` vocabulary.

Subcommands
-----------
- emit-row              build + self-check + print one canonical JSONL row
- validate-row          self-check a row read from stdin/arg (never raises)
- write-metadata        print archive `metadata.env` (ARCHIVE_VERSION=2)
- write-scrape-identity print `scrape_identity` file body
- write-counter-streak  print `counter_streak_snapshot` file body

Design invariants
-----------------
- `validate-row` is a self-check of the *freshly built* row immediately before
  append. It is NEVER a gate over the historical corpus, so it uses a
  required-subset + per-field-type contract (never set-equality): diagnostic
  supersets pass.
- `ts` is checked `isinstance(ts, str) and TS_RE.match(ts)` in that order, so a
  null/int `ts` is rejected without raising `TypeError` (issue #3791 comment 1
  §3).
- `warnings`/`actions` are `list[str]` over a closed vocabulary. The warnings
  vocabulary is the alarm `name`s in metric-alarms.toml UNION the
  `[warning_vocabulary].non_alarm` set declared in the same TOML (one registry).
  Unknown tokens map to the registered `other` fallback so there is always a
  compliant input.
- Embedded measurements are promoted out of the token into typed sibling keys
  (`low-disk 90%` -> `warnings:["low-disk"], disk_free_pct:90`).
- On self-check failure `emit-row` fails loud (non-zero exit, stderr note,
  nothing on stdout) but never aborts the caller: the SKILL routes the rejected
  row to a `tick-history.rejected.jsonl` sidecar and continues the tick.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from datetime import datetime, timezone
from pathlib import Path

try:
    import tomllib
except ImportError:  # Python < 3.11
    import tomli as tomllib  # type: ignore[no-redef]

# ── Schema constants ──────────────────────────────────────────────────────────

# Artifact-schema version for the archive metadata.env sidecar. BUMP THIS
# whenever the metadata.env field set or the tick-history row schema changes so
# downstream readers/replayers can branch on it. Was an inert `1` on every one
# of the 52 historical shapes (issue #3791); now it is a meaningful, owned
# constant.
ARCHIVE_VERSION = 2

# The 9 fields every canonical row MUST carry. Corrected from the issue body's
# original "canonical 9": drop `tick` (the dominant/headless writer never emits
# it — 57% of history lacks it) and add `self_reflect` (present in the modal
# signature). See #3791 comment 1.
REQUIRED_FIELDS = (
    "ts",
    "status",
    "ledger",
    "build",
    "deploys",
    "warnings",
    "actions",
    "self_reflect",
    "watch",
)

STATUS_VALUES = {"OK", "WARNING", "ACTION", "OFFLINE"}

# Type-guard-before-regex: match ONLY after confirming str. See #3791 comment 1 §3.
TS_RE = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$")

# Fallback token for any warning/action outside the closed vocabulary. There is
# always a compliant input, so the append-time check can safely fail loudly.
OTHER = "other"

# Measurement promotion: warning token base -> typed sibling row key. When a
# warning token carries an embedded number (`low-disk 90%`) the number moves to
# the sibling key and the token collapses to its bare condition name.
MEASUREMENT_KEYS = {
    "low-disk": "disk_free_pct",
    "high-memory": "rss_pct",
    "jemalloc-frag-high": "frag_pct",
    "frag": "frag_pct",
}

# Closed vocabulary for `actions`. Unlike warnings this is a small fixed set of
# verbs (not user-extensible via TOML), so it lives here. `filed-#<N>` carries a
# dynamic issue number and normalizes to the `filed` base for the vocab check
# while the full token is preserved in the row.
ACTION_VOCAB = {
    "restart",
    "deploy",
    "filed",
    "session-wiped-recovery",
    "session-wiped-process-alive",
    "session-wiped-rebuild-failed",
    "mainnet-data-wiped",
    OTHER,
}

_MEASUREMENT_RE = re.compile(r"^\s*(?P<base>.*?)[\s:=]+(?P<num>\d+(?:\.\d+)?)\s*%?\s*$")


# ── Catalog / vocabulary loading ──────────────────────────────────────────────


def _default_catalog_path() -> Path:
    # scripts/lib/monitor-tick-artifacts.py -> repo root is parents[2].
    return (
        Path(__file__).resolve().parents[2]
        / ".claude"
        / "skills"
        / "shared"
        / "metric-alarms.toml"
    )


def load_warning_vocab(catalog_path: Path | None = None) -> set[str]:
    """Closed warnings vocabulary = alarm names ∪ non_alarm set ∪ {other}."""
    path = catalog_path or _default_catalog_path()
    with open(path, "rb") as fh:
        catalog = tomllib.load(fh)
    alarm_names = {a["name"] for a in catalog.get("alarm", []) if "name" in a}
    wv = catalog.get("warning_vocabulary", {})
    non_alarm = set(wv.get("non_alarm", []))
    return alarm_names | non_alarm | {OTHER}


# ── Token normalization ───────────────────────────────────────────────────────


def promote_warning(token: str) -> tuple[str, str | None, object]:
    """Split an embedded measurement out of a warning token.

    Returns (base_token, sibling_key_or_None, value_or_None). `low-disk 90%`
    -> ("low-disk", "disk_free_pct", 90). A plain token returns (token, None,
    None).
    """
    m = _MEASUREMENT_RE.match(token)
    if not m:
        return token.strip(), None, None
    base = m.group("base").strip()
    if not base:
        return token.strip(), None, None
    num = m.group("num")
    value: object = float(num) if "." in num else int(num)
    return base, MEASUREMENT_KEYS.get(base), value


def action_base(token: str) -> str:
    """Normalize an action token to its vocabulary base (`filed-#123` -> `filed`)."""
    t = token.strip()
    if re.match(r"^filed-#", t):
        return "filed"
    return t


def _split_csv(raw: str | None) -> list[str]:
    if not raw:
        return []
    return [tok.strip() for tok in raw.split(",") if tok.strip()]


# ── Row construction + validation ─────────────────────────────────────────────


def build_row(args: argparse.Namespace, vocab: set[str]) -> dict:
    """Construct the canonical row from typed args (conformant by construction)."""
    ts = args.ts or datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    warnings: list[str] = []
    siblings: dict[str, object] = {}
    for raw in _split_csv(args.warnings):
        base, sib_key, value = promote_warning(raw)
        token = base if base in vocab else OTHER
        warnings.append(token)
        if sib_key is not None and value is not None:
            siblings[sib_key] = value

    actions: list[str] = []
    for raw in _split_csv(args.actions):
        actions.append(raw if action_base(raw) in ACTION_VOCAB else OTHER)

    row: dict = {
        "ts": ts,
        "status": args.status,
        "ledger": args.ledger,
        "build": args.build,
        "deploys": args.deploys,
        "warnings": warnings,
        "actions": actions,
        "self_reflect": args.self_reflect,
        "watch": _split_csv(args.watch),
    }
    # Promoted measurements become typed sibling keys (row is a superset;
    # validate-row accepts supersets by design).
    row.update(siblings)
    return row


def validate_row(row: object, vocab: set[str]) -> list[str]:
    """Return a list of violation strings; empty list == valid.

    Contract: required-subset + per-field type, NEVER set-equality. Diagnostic
    supersets (extra keys) are accepted.
    """
    errors: list[str] = []

    if not isinstance(row, dict):
        return [f"row is not a JSON object (got {type(row).__name__})"]

    missing = [f for f in REQUIRED_FIELDS if f not in row]
    if missing:
        errors.append(f"missing required fields: {sorted(missing)}")

    # ts: type-guard BEFORE regex so a null/int ts is rejected, not a TypeError.
    if "ts" in row:
        ts = row["ts"]
        if not (isinstance(ts, str) and TS_RE.match(ts)):
            errors.append(f"ts is not an ISO-8601 UTC string: {ts!r}")

    if "status" in row:
        st = row["status"]
        if not isinstance(st, str) or st not in STATUS_VALUES:
            errors.append(f"status not in {sorted(STATUS_VALUES)}: {st!r}")

    if "ledger" in row:
        lg = row["ledger"]
        if isinstance(lg, bool) or not isinstance(lg, int):
            errors.append(f"ledger is not an int: {lg!r}")

    if "build" in row and not isinstance(row["build"], str):
        errors.append(f"build is not a str: {row['build']!r}")

    if "deploys" in row:
        dp = row["deploys"]
        if isinstance(dp, bool) or not isinstance(dp, int) or dp not in (0, 1):
            errors.append(f"deploys is not 0/1: {dp!r}")

    if "self_reflect" in row and not isinstance(row["self_reflect"], str):
        errors.append(f"self_reflect is not a str: {row['self_reflect']!r}")

    for field in ("warnings", "actions", "watch"):
        if field not in row:
            continue
        val = row[field]
        if not isinstance(val, list) or not all(isinstance(x, str) for x in val):
            errors.append(f"{field} is not list[str]: {val!r}")
            continue
        if field == "warnings":
            unknown = [w for w in val if w not in vocab]
            if unknown:
                errors.append(f"warnings outside vocabulary: {unknown}")
        elif field == "actions":
            unknown = [a for a in val if action_base(a) not in ACTION_VOCAB]
            if unknown:
                errors.append(f"actions outside vocabulary: {unknown}")

    return errors


# ── Subcommand handlers ───────────────────────────────────────────────────────


def _catalog_arg(args: argparse.Namespace) -> Path | None:
    return Path(args.catalog) if getattr(args, "catalog", None) else None


def cmd_emit_row(args: argparse.Namespace) -> int:
    vocab = load_warning_vocab(_catalog_arg(args))
    row = build_row(args, vocab)
    errors = validate_row(row, vocab)
    if errors:
        # Fail loud but non-fatal: nothing on stdout, so `emit-row >> "$HIST"`
        # appends nothing. The SKILL routes the row to the reject sidecar.
        sys.stderr.write(
            "monitor-tick-artifacts: emit-row self-check failed: "
            + "; ".join(errors)
            + "\n"
        )
        sys.stderr.write(json.dumps(row) + "\n")
        return 3
    sys.stdout.write(json.dumps(row) + "\n")
    return 0


def cmd_validate_row(args: argparse.Namespace) -> int:
    if args.row is not None:
        raw = args.row
    elif args.file is not None:
        raw = Path(args.file).read_text()
    else:
        raw = sys.stdin.read()

    try:
        row = json.loads(raw)
    except (json.JSONDecodeError, ValueError) as exc:
        sys.stderr.write(f"monitor-tick-artifacts: row is not valid JSON: {exc}\n")
        return 1

    vocab = load_warning_vocab(_catalog_arg(args))
    errors = validate_row(row, vocab)
    if errors:
        sys.stderr.write(
            "monitor-tick-artifacts: validate-row failed: " + "; ".join(errors) + "\n"
        )
        return 1
    return 0


def cmd_write_metadata(args: argparse.Namespace) -> int:
    lines = [
        f"ARCHIVE_VERSION={ARCHIVE_VERSION}",
        f"TICK_SKIPPED={args.tick_skipped}",
        f"PREV_PROM_INVALID={args.prev_prom_invalid}",
        f"PREV_SCRAPE_AGE_SECONDS={args.prev_scrape_age_seconds}",
        f"WARMUP_TICKS_REMAINING={args.warmup_ticks_remaining}",
        f"FRESH_START={args.fresh_start}",
        f"CRASH_RECOVERY={args.crash_recovery}",
        f"UPTIME_SECONDS={args.uptime_seconds}",
        f"MONITOR_MODE={args.monitor_mode}",
        f"PID={args.pid}",
        f"START_TICKS={args.start_ticks}",
    ]
    sys.stdout.write("\n".join(lines) + "\n")
    return 0


def cmd_write_scrape_identity(args: argparse.Namespace) -> int:
    ts = args.ts or datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    sys.stdout.write(
        f"version=1\npid={args.pid}\nstart_ticks={args.start_ticks}\ntimestamp={ts}\n"
    )
    return 0


def cmd_write_counter_streak(args: argparse.Namespace) -> int:
    sys.stdout.write(
        "version=1\n"
        f"pid={args.pid}\n"
        f"start_ticks={args.start_ticks}\n"
        f"counter_value={args.counter_value}\n"
        f"breach_streak={args.breach_streak}\n"
    )
    return 0


# ── CLI ───────────────────────────────────────────────────────────────────────


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="monitor-tick-artifacts.py",
        description="Single-writer helper for monitor-tick per-tick artifacts.",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    def add_catalog(p: argparse.ArgumentParser) -> None:
        p.add_argument(
            "--catalog",
            default=None,
            help="Path to metric-alarms.toml (defaults to the in-repo catalog).",
        )

    p_emit = sub.add_parser("emit-row", help="Build + self-check + print one JSONL row.")
    p_emit.add_argument("--status", required=True)
    p_emit.add_argument("--ledger", required=True, type=int)
    p_emit.add_argument("--build", required=True)
    p_emit.add_argument("--deploys", type=int, default=0)
    p_emit.add_argument("--self-reflect", dest="self_reflect", default="clean")
    p_emit.add_argument("--warnings", default="", help="Comma-separated warning tokens.")
    p_emit.add_argument("--actions", default="", help="Comma-separated action tokens.")
    p_emit.add_argument("--watch", default="", help="Comma-separated key=value items.")
    p_emit.add_argument("--ts", default=None, help="Override ts (defaults to now, UTC).")
    add_catalog(p_emit)
    p_emit.set_defaults(func=cmd_emit_row)

    p_val = sub.add_parser("validate-row", help="Self-check a row (never raises).")
    p_val.add_argument("--row", default=None, help="Row JSON as a string.")
    p_val.add_argument("--file", default=None, help="Path to a file holding row JSON.")
    add_catalog(p_val)
    p_val.set_defaults(func=cmd_validate_row)

    p_meta = sub.add_parser("write-metadata", help="Print archive metadata.env.")
    p_meta.add_argument("--tick-skipped", dest="tick_skipped", default="false")
    p_meta.add_argument("--prev-prom-invalid", dest="prev_prom_invalid", default="false")
    p_meta.add_argument(
        "--prev-scrape-age-seconds", dest="prev_scrape_age_seconds", default="-1"
    )
    p_meta.add_argument(
        "--warmup-ticks-remaining", dest="warmup_ticks_remaining", default="0"
    )
    p_meta.add_argument("--fresh-start", dest="fresh_start", default="no")
    p_meta.add_argument("--crash-recovery", dest="crash_recovery", default="no")
    p_meta.add_argument("--uptime-seconds", dest="uptime_seconds", default="0")
    p_meta.add_argument("--monitor-mode", dest="monitor_mode", default="validator")
    p_meta.add_argument("--pid", default="")
    p_meta.add_argument("--start-ticks", dest="start_ticks", default="")
    p_meta.set_defaults(func=cmd_write_metadata)

    p_id = sub.add_parser("write-scrape-identity", help="Print scrape_identity body.")
    p_id.add_argument("--pid", required=True)
    p_id.add_argument("--start-ticks", dest="start_ticks", required=True)
    p_id.add_argument("--ts", default=None, help="Override timestamp (defaults to now).")
    p_id.set_defaults(func=cmd_write_scrape_identity)

    p_cs = sub.add_parser("write-counter-streak", help="Print counter_streak_snapshot.")
    p_cs.add_argument("--pid", required=True)
    p_cs.add_argument("--start-ticks", dest="start_ticks", required=True)
    p_cs.add_argument("--counter-value", dest="counter_value", required=True)
    p_cs.add_argument("--breach-streak", dest="breach_streak", required=True)
    p_cs.set_defaults(func=cmd_write_counter_streak)

    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
