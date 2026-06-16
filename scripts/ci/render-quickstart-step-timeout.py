#!/usr/bin/env python3
# render-quickstart-step-timeout.py — correct-by-construction renderer for the
# per-shard `timeout-minutes` of the "Run probes through wrapper" step in
# .github/workflows/quickstart.yml (#3286).
#
# WHY THIS EXISTS
# ---------------
# The whole #3286 diagnostic chain hinges on the testnet/core,horizon shard's
# "Run probes through wrapper" step carrying a TIGHT step-level
# `timeout-minutes` (25). A step-level timeout marks the STEP failed (so the
# `if: failure()` "Upload diagnostics" step runs and the watchdog dump is
# uploaded); the 45-min JOB wall-clock is a *cancel* (subsequent steps do NOT
# reliably run). The wiring is:
#
#     timeout-minutes: ${{ matrix.step_timeout_minutes || 360 }}
#
# and ONLY the testnet matrix entry sets `step_timeout_minutes: 25`. A previous
# attempt put the key on the wrong testnet entry / could not prove it rendered,
# so a hung CI run was the only "test" — flaky and slow. This script REMOVES
# that dependency: it parses the workflow and EVALUATES the expression exactly
# as GitHub Actions does (substitute the per-shard matrix value, then apply the
# `||` short-circuit with GitHub's falsy rules: null / false / 0 / '' / NaN are
# falsy, everything else returns the left operand). The harness asserts on the
# rendered number per shard — no CI hang required.
#
# OUTPUT: one line per matrix shard of the `test` job:
#     <network>|<enable>|<rendered_timeout_minutes>
# Exit 0 on success; non-zero with a diagnostic on any structural problem
# (missing job/step/key, unparseable expression) so the harness fails loudly
# rather than silently passing on a workflow that drifted.

import re
import sys

import yaml

WORKFLOW = sys.argv[1] if len(sys.argv) > 1 else ".github/workflows/quickstart.yml"
STEP_NAME = "Run probes through wrapper"


def fail(msg):
    print(f"ERROR: {msg}", file=sys.stderr)
    sys.exit(1)


def gha_truthy(value):
    """GitHub Actions falsy set: null, false, 0, '', NaN. Everything else truthy.

    Mirrors the `||` operator semantics used in expression contexts. An UNSET
    matrix key surfaces here as None (null) and is falsy, so `X || 360` -> 360.
    A set integer 25 is truthy, so `25 || 360` -> 25.
    """
    if value is None:
        return False
    if isinstance(value, bool):
        return value
    if isinstance(value, (int, float)):
        # NaN is falsy; 0 is falsy.
        if value != value:  # NaN
            return False
        return value != 0
    if isinstance(value, str):
        return value != ""
    # Lists/dicts and other objects are truthy in GHA expression context.
    return True


def eval_timeout_expr(expr, matrix_entry):
    """Evaluate a `${{ ... }}` timeout-minutes expression for one matrix entry.

    Supports exactly the form this workflow uses:
        ${{ matrix.<key> || <int-default> }}
    and the degenerate forms `${{ matrix.<key> }}` and a bare integer (a shard
    that sets timeout-minutes to a literal). Anything else is a structural error
    — we refuse to guess, so an unrecognized expression fails the assertion
    loudly instead of rendering a wrong number.
    """
    # Bare integer literal (no expression).
    if isinstance(expr, int):
        return expr
    s = str(expr).strip()
    m = re.fullmatch(r"\$\{\{\s*(.*?)\s*\}\}", s)
    if not m:
        # A literal numeric string?
        if re.fullmatch(r"\d+", s):
            return int(s)
        fail(f"unrecognized timeout-minutes value (not ${{{{ }}}} expr or int): {expr!r}")
    inner = m.group(1)

    # Split on the top-level `||` (only form we support).
    parts = [p.strip() for p in re.split(r"\|\|", inner)]
    for part in parts:
        mm = re.fullmatch(r"matrix\.([A-Za-z0-9_]+)", part)
        if mm:
            key = mm.group(1)
            val = matrix_entry.get(key)
            if gha_truthy(val):
                if not isinstance(val, (int, float)):
                    fail(
                        f"matrix.{key}={val!r} is truthy but not numeric — "
                        f"timeout-minutes must render to an integer"
                    )
                return int(val)
            # falsy -> fall through to the next `||` operand
            continue
        if re.fullmatch(r"\d+", part):
            # Literal default operand; reached only when all prior operands
            # were falsy.
            return int(part)
        fail(f"unsupported operand in timeout-minutes expression: {part!r} (full: {expr!r})")
    fail(f"timeout-minutes expression had no truthy operand: {expr!r}")


def main():
    with open(WORKFLOW) as fh:
        doc = yaml.safe_load(fh)

    jobs = doc.get("jobs") or {}
    test_job = jobs.get("test")
    if test_job is None:
        fail("workflow has no `test` job")

    strategy = test_job.get("strategy") or {}
    matrix = strategy.get("matrix") or {}
    include = matrix.get("include")
    if not include:
        fail("`test` job matrix has no `include` list")

    steps = test_job.get("steps") or []
    step = next((st for st in steps if st.get("name") == STEP_NAME), None)
    if step is None:
        fail(f"`test` job has no step named {STEP_NAME!r}")
    if "timeout-minutes" not in step:
        fail(f"step {STEP_NAME!r} has no `timeout-minutes` key")
    expr = step["timeout-minutes"]

    for entry in include:
        network = entry.get("network", "")
        enable = entry.get("enable", "")
        rendered = eval_timeout_expr(expr, entry)
        print(f"{network}|{enable}|{rendered}")


if __name__ == "__main__":
    main()
