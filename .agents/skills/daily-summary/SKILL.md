---
name: daily-summary
description: Post a daily henyey mainnet validator + project summary to GitHub Discussions on stellar-experimental/henyey. Intended to be invoked once per day at 09:00 ET (13:00 UTC) by cron. Reports validator state, deploys, incidents, issues activity, watch items, and tick aggregates over the last 24h.
argument-hint: ""
---

# Daily Summary

One invocation = one Discussion post on `stellar-experimental/henyey`.

## Preconditions

1. Discussions must be enabled on the repo with a usable category. The
   skill resolves repository + category IDs at runtime via GraphQL — no
   IDs are hardcoded — so renaming or swapping the category is fine as
   long as one named below exists.
2. `gh auth status` must be valid for the runtime user.
3. The runtime user's GitHub token must have **Discussions: Write** scope.
   GitHub does not expose this on the `Repository` type, so the only
   reliable test is to attempt `createDiscussion` itself: surface a
   `FORBIDDEN`/`Discussions: Write` error as a fatal precondition failure
   and instruct the operator to regenerate the fine-grained PAT with the
   missing scope (the project's historical PAT has lacked it). Do not
   silently retry — the post will fail every time until the token is
   updated.
4. `/home/tomer/data/monitor-loop.env` must exist (same env as
   `monitor-tick`). The skill loads it to find `MONITOR_SESSION_ID`,
   `MONITOR_ADMIN_PORT`, `MONITOR_RPC_PORT`.

If any precondition fails, bail out with a clear ERROR and do NOT post.

## Resolve repo + category

```bash
set -a
source /home/tomer/data/monitor-loop.env
set +a

# Preferred category names, in order. First match wins.
CATEGORY_CANDIDATES=("Daily reports" "Operations" "Announcements" "General")

resolved=$(gh api graphql -f query='
  query {
    repository(owner:"stellar-experimental", name:"henyey") {
      id
      hasDiscussionsEnabled
      discussionCategories(first:25) { nodes { id name } }
    }
  }')

REPO_ID=$(printf '%s' "$resolved" | python3 -c '
import sys, json
print(json.load(sys.stdin)["data"]["repository"]["id"])')

CATEGORY_ID=$(printf '%s' "$resolved" | python3 -c '
import sys, json
data = json.load(sys.stdin)["data"]["repository"]
cats = {n["name"]: n["id"] for n in data["discussionCategories"]["nodes"]}
for cand in '"${CATEGORY_CANDIDATES[@]@Q}".split():
    if cand in cats: print(cats[cand]); break
' )
```

If `REPO_ID` or `CATEGORY_ID` is empty, bail out — Discussions are not
configured. Otherwise continue.

## Compose the body

Build the markdown body in `/tmp/daily-summary-body.md`. All sections
below are required even if a section is empty (write `_(none)_`).

### 1. Validator section

Pull live state once at the top of the run:

```bash
INFO=$(curl -s -m 5 "http://localhost:$MONITOR_ADMIN_PORT/info")
HEALTH=$(curl -s -m 5 -X POST "http://localhost:$MONITOR_RPC_PORT" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}')
METRICS=$(curl -s -m 5 "http://localhost:$MONITOR_ADMIN_PORT/metrics")
source "$(git rev-parse --show-toplevel)/scripts/lib/monitor-decisions.sh"
PID=$(_find_session_process "$HOME/data" "/proc" "$MONITOR_SESSION_ID")
if [ -n "$PID" ]; then
  UPTIME_SEC=$(ps -o etimes= -p "$PID" | tr -d ' ')
  RSS_KB=$(ps -o rss= -p "$PID" | tr -d ' ')
else
  UPTIME_SEC=""
  RSS_KB=""
fi
```

Extract:
- `state` (Validating / Catching Up / etc.) from `INFO`
- `latestLedger - oldestLedger` from `HEALTH`, plus `age` and `status`
- `quorum.qset.{agree,missing,fail_at}` from `INFO` (nested under `qset`,
  not at the top level of the quorum block; the quorum block also has
  `node` and `transitive` children)
- `stellar_scp_timing_first_to_self_externalize_seconds`,
  `stellar_scp_timing_externalized_seconds`,
  `stellar_scp_timing_nominated_seconds` from `METRICS` — these are scalar
  gauges (current value), not histograms; just read the scalar
- `henyey_jemalloc_fragmentation_pct` (current value)
- `henyey_jemalloc_allocated_bytes` (heap-allocated, current)
- `henyey_startup_peak_anon_rss_mb` (startup/catchup peak anon RSS, MB —
  issue #3226/#3227; absent until startup completes). Read the attributed
  `phase` from the `startup_peak_anon_rss_mb=<N> phase=<phase>` log summary line.
- Last `memory_report=true` (or `Memory report summary`) line for `heap_components_mb` trajectory if available

Build (uptime, deploys, RSS-GB, frag-pct) and emit:

```markdown
## Validator

- Build: `<sha>` (uptime <Hh Mm>; <N> deploys in last 24h, ~<X>m avg/build)
- State: <icon> <state> (`age=<s>s`, `agree=<n>/missing=<n>`, `lag_ms=<n>`)
- SCP propagation (`first_to_self_externalize_seconds`): <ms>ms avg
- SCP slot-cycle (`externalized_seconds`): <s>s avg
- Memory: RSS <G>G, frag <pct>% (<heap-components-trajectory>)
- Startup peak: <G>G (phase=<phase>) — _(omit if `henyey_startup_peak_anon_rss_mb` absent / node still starting up)_
- Disk: data <pct>% (<used>G of <total>G), session <size>G, mainnet <size>G
```

Icon: 🟢 Validating, 🟡 Catching Up, 🔴 anything else.

### 2. Deploys section

```bash
SINCE_ISO=$(date -u -d '24 hours ago' +%Y-%m-%dT%H:%M:%SZ)
git log --since="$SINCE_ISO" --pretty=format:'%h %s' origin/main
```

For each commit, also list the touched crates (top-level `crates/<name>/`
prefixes from `git show --stat --format=`). Cross-check against the
session's `monitor.log.preredeploy-*` rotation timestamps to confirm
the commit was actually deployed (not just landed on main between two
deploy ticks). Mark a watch-sentinel column:

- `silent` — no warnings between this deploy and the next
- `fired and recovered` — at least one tick reported WARNING / ACTION
  but the validator stayed up
- `rolled back` — preredeploy rotation followed by a `.crashed-*` /
  `.frozen-*` rotation within an hour

```markdown
## Deploys (<N>)

- `<sha>` <subject> <(linked PR if any)>
  - Files touched: <crates/...>
  - Watch sentinel: <silent | fired and recovered | rolled back>
```

If `N=0`, still print the heading and `_(none)_`.

### 3. Incidents section

An incident is any one of:
- The validator process died (a `monitor.log.crashed-*` or
  `monitor.log.frozen-*` rotation appeared in the last 24h).
- A `urgent`-labeled GH issue was filed in the last 24h.
- A `MONITOR ACTION` (deploy-driven restart) **with** an associated
  warning or sync-failure in the same tick.

For each:

```markdown
- <icon> <one-line headline>
  - Window: <start UTC> → <end UTC> (<Hh Mm> total)
  - Symptom: <hash mismatch | wedge | OOM | …>
  - Root cause: <one sentence>
  - Resolved by: <commit-sha> (<linked PR/issue>)
```

Icon: 🔴 still-open or unresolved, 🟢 resolved.

### 4. Issues activity

Three sub-lists. Use `gh issue list`:

```bash
DAY=$(date -u +%Y-%m-%d)
gh issue list --repo stellar-experimental/henyey \
  --search "created:>=$(date -u -d '24 hours ago' +%Y-%m-%d)" \
  --state all --limit 50 --json number,title,labels,state
```

Bucket into:
- **Filed today** — created in last 24h:
  `gh issue list --search "created:>=$(date -u -d '24 hours ago' +%Y-%m-%d)" --state all`
- **Closed today** — closed in last 24h:
  `gh issue list --search "closed:>=$(date -u -d '24 hours ago' +%Y-%m-%d)" --state closed`
- **Still open** — long-running open issues (filed > 24h ago, currently open):
  `gh issue list --search "is:open created:<$(date -u -d '24 hours ago' +%Y-%m-%d)" --state open`

For each line, prefix with severity indicator. **Precedence:** if both
`urgent` and `not-ready` labels are present, render as `urgent` (🔴).

- `urgent` → 🔴 (red dot)
- `not-ready` → ⚫ (grey dot)
- *(no label)* → ⚪ (white circle)

```markdown
## Issues activity

Filed today (<N>):
- 🔴 #<n> <title>
- ⚪ #<n> <title>

Closed today (<N>):
- #<n> <title> (resolved by `<sha>`)

Still open (<N>):
- ⚪ #<n> <title> — last activity <date>
```

### 5. Watch items

Multi-tick non-incident concerns. Source: the `watch` array of
yesterday's tick-history.jsonl entries. Aggregate the same key across
ticks; report current value + 24h delta + linked issue if any.

Include `startup_peak_mb=` watch entries (emitted by monitor-tick check 7 from
`henyey_startup_peak_anon_rss_mb`) so peak creep across restarts is visible — a
peak that climbs run-over-run signals the cold-catchup transient is growing.

```bash
HIST=/home/tomer/data/$MONITOR_SESSION_ID/tick-history.jsonl
python3 - "$HIST" <<'PY'
# … aggregate and print watch lines …
PY
```

Format:

```markdown
## Watch items

- `pruning_gap`: 2451 → 2640 (+189 in 24h), tracked by #1989
- `<other>`: <trajectory>, <linked issue>
```

If the array is empty for the window, print `_(none)_`.

### 6. Tick aggregates

The `tick-history.jsonl` schema evolved when monitor-tick was refactored
to the TOML/evaluator catalog. Older rows (pre-refactor) use a top-level
`status` field with values OK/ACTION/WARNING/OFFLINE; newer rows use
`tick` (same values) and include an `evaluator` block with the alarm
aggregate (`metrics`, `ratio`, `recovery_stalled` strings). The
aggregator below reads either field and is forward-compatible.

```bash
python3 - "$HIST" <<'PY'
import sys, json, re
from collections import Counter
from datetime import datetime, timedelta, timezone

cutoff = datetime.now(timezone.utc) - timedelta(hours=24)
outcomes = Counter()
self_reflect = 0
firing_per_tick = []          # per-tick "N firing" counts from evaluator
ratio_breaches = 0
recovery_stall_events = 0
ratio_skipped = 0
total = 0

with open(sys.argv[1]) as f:
    for ln in f:
        try: row = json.loads(ln)
        except Exception: continue
        try:
            ts = datetime.fromisoformat(row["ts"].replace("Z","+00:00"))
        except Exception: continue
        if ts < cutoff: continue
        total += 1

        # Outcome: prefer new "tick" field, fall back to legacy "status"
        outcome = row.get("tick") or row.get("status") or ""
        if outcome in ("OK","ACTION","WARNING","OFFLINE","DEPLOY"):
            outcomes[outcome] += 1

        # Legacy self-reflect tracking (post-refactor rows omit this)
        if row.get("self_reflect","clean") != "clean":
            self_reflect += 1

        # Evaluator aggregate (new rows only)
        ev = row.get("evaluator", {})
        if ev:
            m = re.search(r"(\d+)/\d+ firing", ev.get("metrics",""))
            if m: firing_per_tick.append(int(m.group(1)))
            r = ev.get("ratio","")
            if "WARNING" in r or "breach" in r: ratio_breaches += 1
            if "skipped" in r: ratio_skipped += 1
            rs = ev.get("recovery_stalled","")
            if "delta=" in rs and "delta=0" not in rs:
                recovery_stall_events += 1

print(json.dumps({
    "total": total,
    "outcomes": dict(outcomes),
    "self_reflect": self_reflect,
    "firing_alarms_total": sum(firing_per_tick),
    "ticks_with_firing": sum(1 for n in firing_per_tick if n > 0),
    "ratio_breaches": ratio_breaches,
    "ratio_skipped": ratio_skipped,
    "recovery_stall_events": recovery_stall_events,
}, indent=2))
PY
```

```markdown
## Tick aggregates (last 24h)

- Cron: `<id>` (every 20m) — <expected> scheduled, <actual> fired
- Outcomes: <N> OK / <N> ACTION / <N> WARNING / <N> OFFLINE / <N> DEPLOY
- Alarm-firings: <ticks-with-firing> ticks with ≥1 firing alarm
  (<firing-alarms-total> total alarm-firings across the window)
- metrics_ratio: <ratio-breaches> ticks reported breach, <ratio-skipped> skipped
- recovery_stalled: <events> non-zero-delta ticks
- Self-reflection events: <N> (legacy field; 0 expected on post-refactor rows)
- Skill commits today: <N> (<sha list>)
```

Skill commits = `git log --since=24h --pretty=format:%h -- .Codex/skills/ scripts/lib/eval-alarms.* .Codex/skills/shared/`.

Cron `id` = `gh ... ` not applicable; use the active CronList id resolved
once at the top of the run (call CronList and pick the entry whose prompt
starts with `Check the henyey mainnet monitor log`).

### 7. Open questions

Optional. Only emit if the assistant has an open decision-point for the
operator (e.g. "watch item drifting past tolerance — retune monitor or
escalate?"). Otherwise omit the section entirely.

## Post the discussion

```bash
TITLE="henyey mainnet daily — $(date -u +%Y-%m-%d)"
BODY=$(cat /tmp/daily-summary-body.md)

URL=$(gh api graphql -f query='
  mutation($r:ID!,$c:ID!,$t:String!,$b:String!) {
    createDiscussion(input:{repositoryId:$r, categoryId:$c, title:$t, body:$b}) {
      discussion { url }
    }
  }' \
  -F r="$REPO_ID" -F c="$CATEGORY_ID" -F t="$TITLE" -F b="$BODY" \
  --jq '.data.createDiscussion.discussion.url')

echo "Posted: $URL"
```

Always pass the body via `-F b="$BODY"` (variable substitution by the gh
CLI), not heredoc-into-`-f` — `-f` does not handle multi-line markdown
cleanly. The `-F` form treats the value as a string variable for the
GraphQL mutation, which preserves newlines and backticks. Same lesson
as #1975 issue body formatting (commit `68c9efb2`).

## Output

Print the URL of the created discussion. If posting fails (GraphQL
returned an error, or the network call failed), print the full error
response and exit non-zero — cron will surface this as a failure.

## Schedule

This skill is intended to fire once per day at **13:00 UTC** (= 09:00
EDT, 08:00 EST — one-hour winter drift accepted). Schedule via:

```
CronCreate cron="0 13 * * *" prompt="/daily-summary" recurring=true
```

Do not co-schedule with monitor-tick — they are independent loops with
no ordering dependency.
