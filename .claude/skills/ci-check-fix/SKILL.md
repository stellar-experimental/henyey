---
name: ci-check-fix
description: One tick of a CI watchdog — check main-branch GHA status; spawn an agent to fix if failing and no investigation is in flight.
argument-hint: "(no args; invoke via /loop 10m /ci-check-fix)"
---

# ci-check-fix

One tick of a CI watchdog. Checks main-branch GHA status. If all green,
does nothing. If any job is failing and no previously-spawned agent is
still investigating, spawns an agent to investigate and fix.

Biases toward action: when in doubt, spawn.

## Usage

Invoke once to check now, and on a 10-minute cron to keep checking:

```
/loop 10m /ci-check-fix
```

The `/loop` skill schedules the recurring cron and runs this skill
immediately. Stop with `CronDelete <id>` (the `/loop` confirmation
includes the cron ID).

## Scope

- **Branch**: `main` only. PR-branch failures are the PR author's
  problem.
- **Workflows**: all workflows on `main`, with Quickstart prioritized.
  When multiple workflows fail, Quickstart is triaged first.
- **Lookback**: most recent completed run per distinct workflow name,
  from the last 6 hours.

Anything older than 6h is assumed stale and ignored.

## State

The skill persists state at `~/data/ci-check-fix/state.json`.
Ensure the directory exists before writing
(`mkdir -p ~/data/ci-check-fix/`).

Schema:

```json
{
  "active_task_id": "bk7gxvein | null",
  "active_task_since": "2026-04-18T22:00:00Z | null",
  "active_task_signature": "quickstart/testnet | ci/test_foo | null",
  "consecutive_futile_ticks": 0,
  "last_tick_at": "2026-04-18T22:30:00Z"
}
```

- `active_task_id` is the Agent-tool task ID of the currently-
  investigating agent (when one is running). Null when no agent is in
  flight.
- `active_task_signature` is a short tag of what the agent was spawned
  to fix — used for logging, not decision-making.
- `consecutive_futile_ticks` counts ticks where failures remain AND no
  new agent was spawned AND no agent is running. Reset on green or on
  any spawn.

## Decision algorithm

Each invocation:

### 1. Load state

```bash
STATE=~/data/ci-check-fix/state.json
mkdir -p ~/data/ci-check-fix
if [ ! -f "$STATE" ]; then
  echo '{"active_task_id":null,"active_task_since":null,"active_task_signature":null,"consecutive_futile_ticks":0,"last_tick_at":null}' > "$STATE"
fi
```

Parse fields.

### 2. Resolve active agent

If `active_task_id` is set, check liveness via `TaskList`:
- If the task is still `in_progress` (or `queued`): an agent is
  investigating. Log one line (`ci-check-fix: agent <id> still
  running (<signature>)`), update `last_tick_at`, exit. Do not
  increment the futile-ticks counter; counting runs without an owner
  is the whole point.
- If the task has completed, errored, or is missing: clear
  `active_task_id` / `active_task_since` / `active_task_signature` in
  state and fall through to the next step.

### 3. Snapshot CI on main

For each distinct workflow name, take the most recent completed run
from the last 6h. Quickstart matters most, so check it first.

```bash
# Most recent completed runs across all workflows on main in the last 6h
SIX_H_AGO=$(date -u -d '6 hours ago' +%Y-%m-%dT%H:%M:%SZ)
gh run list --branch main --limit 30 \
  --json databaseId,status,conclusion,name,createdAt,headSha \
  --jq --arg since "$SIX_H_AGO" \
    '[ .[] | select(.status=="completed" and .createdAt > $since) ]'
```

Deduplicate by workflow `name`, keep the newest per name.

### 4. Job-level failure check (critical)

For each of the selected runs, check **job-level** conclusions —
run-level `success` can still hide per-job failures on workflows with
`continue-on-error`:

```bash
gh run view <run-id> --json jobs \
  --jq '.jobs[] | select(.conclusion=="failure") | .name'
```

If zero jobs failed across every selected run → CI is green.

### 5. Act on the outcome

#### 5a. All green

- Reset `consecutive_futile_ticks` to 0.
- Clear any stale agent fields (already done in step 2 if the agent
  completed).
- Update `last_tick_at`.
- Log one line (`ci-check-fix: all green on <sha>`). Exit.

#### 5b. Failures exist, no active agent

First, a **not-ready gate**: before spawning anything, check whether
the failure has already been tracked by an open issue labeled
`not-ready`. `/plan-do-review`'s Step 1 applies this label when its
readiness triage concludes the issue is blocked (e.g., awaiting an
operator decision). Re-spawning agents against a `not-ready` issue
just repeats the same triage and noise.

```bash
# Any open issue that already matches this failure signature AND is
# marked not-ready?
BLOCKED=$(gh issue list --state open --label not-ready \
  --limit 20 --json number,title,updatedAt \
  --jq '.[] | select(.title | test("testnet|local/rpc|quickstart"; "i")) | .number' \
  | head -1)
```

(Refine the `test()` regex to match the actual failing workflow +
shard signature the skill has computed — not a literal string.)

- If a `not-ready` issue matches the current failure signature:
  - Do NOT spawn an agent. The operator has outstanding input on
    that issue; spawning would only re-apply `not-ready` and waste a
    round.
  - Do NOT increment `consecutive_futile_ticks` — the absence of a
    spawn here is a deliberate skip, not a failed attempt. The loop
    will still hit the 6-tick stop via normal ticks that either
    detect different failures or where the issue eventually changes
    state.
  - Log `ci-check-fix: blocked on not-ready issue #<N> (<signature>) — skipping spawn`.
  - Update `last_tick_at`, exit.
- Otherwise, proceed:

- Increment `consecutive_futile_ticks`.
- **If `consecutive_futile_ticks` >= 6**: stop the loop. The previous
  agents have evidently been returning without closing the failure
  across six consecutive ticks (60 minutes). Take no further action.
  Log a clear line indicating the skill is giving up:
  ```
  ci-check-fix: stopping loop — 6 consecutive ticks with
  unresolved failure. Last agents returned but the failure recurs.
  Operator intervention required. Consider stopping the cron with
  CronDelete <id>.
  ```
  Do NOT call `ScheduleWakeup` or leave a cron active — if the
  invoker was `/loop 10m`, explicitly note they should run
  `CronDelete`.
- **Else**: spawn a new agent (see §6). Update state with the new
  `active_task_id`, `active_task_since=now`, `active_task_signature=<tag>`.

Bias-to-action note: once failures are detected and no agent is in
flight, spawn on the same tick that detected them — do not wait for
the next cycle.

### 6. Spawning the investigator

Two branches, depending on which workflow is failing. If multiple
workflows fail simultaneously, prefer Quickstart first and file the
others next tick.

**Invariant across both branches**: every spawned agent MUST end with
a `/plan-do-review <N>` invocation against the issue that tracks the
failure — whether `<N>` was an existing open issue the agent
discovered, or a new issue the agent just filed. Simply commenting on
an existing issue is NOT acceptable — commenting produces notifications
without driving a fix. `/plan-do-review` is the only mechanism that
converges on an executable plan and lands code.

#### 6a. Quickstart failed

Spawn an Agent that triages the failure, finds or files a tracking
issue, and runs `/plan-do-review` on it:

```
Agent(
  description="ci-check-fix: investigate Quickstart failure on main",
  subagent_type="general-purpose",
  model="sonnet",
  run_in_background=true,
  prompt="""
You are spawned by the /ci-check-fix skill to investigate and fix the
most recent Quickstart failure on main.

Context:
- Failing run: <run-url>
- Failing shards: <comma-separated shard names from step 4>
- HEAD sha: <sha>

Your task runs in two phases:

PHASE 1 — Triage via /quickstart-fix (stages 1-3)
Invoke /quickstart-fix (no args — it picks the most recent failing
run automatically). Follow stages 1, 2, and 3 of the skill to:
- identify the failing shard(s) and signature,
- collect CI evidence (log excerpts from the shard artifact),
- attempt local reproduction.

PHASE 2 — Drive the fix via /plan-do-review (mandatory)
After triage, always end with /plan-do-review against a tracking
issue, regardless of what /quickstart-fix's stage-4 triage would
conclude. Specifically:

1. Search for an existing open GitHub issue that tracks this exact
   failure signature — grep the open-issues list for the shard name,
   the signature keywords, and any obvious file:line candidates:
     gh issue list --state open --limit 50 --json number,title
   Look for an issue whose title/body matches the symptom.
2. If exactly one matching open issue exists, use its number as `N`.
3. If zero matching issues exist, file a new one using /quickstart-fix
   stage-5b's issue body template (Symptom, Evidence, Repro steps,
   Suspected root cause, Candidate sites, Related run URL). Capture
   the returned issue number as `N`.
4. If multiple matching issues exist, pick the most recently updated
   one and note the others in a comment on `N` for the reviewer to
   consolidate.
5. Invoke /plan-do-review `N`. Let it run end-to-end (adversarial
   critique, implementation, review-fix iteration, landing).
6. Return when /plan-do-review completes.

You are self-driving. Do not ask the operator for approval at any
decision point. If /plan-do-review itself concludes the issue is
'not ready' or blocked, return with that status — do not leave the
issue in a half-baked state.

Notes:
- Do NOT skip /plan-do-review because the issue already exists. A
  fresh /plan-do-review invocation reruns the adversarial-critic loop
  with the latest evidence; that's the whole point.
- Do NOT just comment on the issue. ci-check-fix's prior tick may have
  added a comment; that comment did not drive a fix. This tick owes a
  /plan-do-review run.
"""
)
```

Set `active_task_signature` to `quickstart/<first failing shard>`.

#### 6b. A non-Quickstart workflow failed

Same phase-1/phase-2 structure, but phase 1 is a direct log-pull
instead of /quickstart-fix:

```
Agent(
  description="ci-check-fix: investigate <workflow> failure on main",
  subagent_type="general-purpose",
  model="sonnet",
  run_in_background=true,
  prompt="""
You are spawned by the /ci-check-fix skill to investigate and fix a
non-Quickstart CI failure on main.

Context:
- Workflow: <workflow-name>
- Failing run: <run-url>
- Failing jobs: <list of job names>
- HEAD sha: <sha>

PHASE 1 — Triage
Pull failure logs via
  `gh run view <run-id> --log-failed 2>&1 | tail -200`
and identify the specific failure signature (panic, assertion,
timeout, build error, test name).

PHASE 2 — Drive the fix via /plan-do-review (mandatory)
Always end with a /plan-do-review run on the tracking issue:

1. Search for an existing open issue matching this failure:
     gh issue list --state open --limit 50 --json number,title
2. If exactly one matches, use its number as `N`.
3. If zero match, file a new issue titled:
     "<workflow> failed with <signature> on <sha-short>"
   Body must include: Symptom, Evidence (log excerpt), Repro steps
   (exact cargo command or test name), Suspected root cause (if any),
   Candidate sites (file:line), Link to the run. Format per
   .claude/skills/plan-do-review/SKILL.md expectations. Capture the
   returned issue number as `N`.
4. If multiple match, pick the most recently updated; note the others
   in a comment on `N`.
5. Invoke /plan-do-review `N`. Let it run end-to-end.
6. Return when /plan-do-review completes.

You are self-driving. Do not ask the operator for approval at any
decision point. If /plan-do-review concludes the issue is 'not
ready', return with that status.

Do NOT skip /plan-do-review because the issue already exists — rerun
it with the latest evidence so the adversarial-critic loop gets a
fresh pass. Do NOT just comment on the issue and stop.
"""
)
```

Set `active_task_signature` to `<workflow>/<first failing job>`.

### 7. Persist state

Write the updated state back to
`~/data/ci-check-fix/state.json`. Update `last_tick_at`
unconditionally.

## Output

The skill prints exactly one status line per tick, so the operator
can scan the conversation log quickly. Format examples:

- `ci-check-fix: all green on 601718a0`
- `ci-check-fix: agent bk7gxvein still running (quickstart/testnet)`
- `ci-check-fix: spawned agent bmq3wsqg6 for quickstart/local_rpc; run <url>`
- `ci-check-fix: spawned agent b7t5annij to file issue + /plan-do-review for ci/test_foo`
- `ci-check-fix: futile tick 4/6 — failures persist (quickstart/testnet), no agent in flight — spawning`
- `ci-check-fix: STOPPING — 6 futile ticks; run CronDelete <id>`

No other chatter. Inline investigation writes go to the spawned
agent's conversation, not this one.

## Guidelines

- **The skill does not make code changes itself.** It only observes
  and delegates. Ownership of the fix lives with the spawned agent.
- **Do not file duplicate issues.** Spawned agents already check for
  existing issues by signature. ci-check-fix itself never files
  issues — only the spawned agent does.
- **Do not bundle workflows.** One tick can spawn at most one agent.
  If both Quickstart and another workflow fail, Quickstart wins this
  tick; the other is handled next tick (assuming the first agent
  finishes).
- **Never run multiple investigator agents at once.** The
  decision-tree guards against this via `active_task_id` in state.
  This avoids dueling commits and issue floods.
- **Futile-tick counter semantics.** Increments every tick where
  failures exist and no agent is currently in flight — i.e., a prior
  agent ran, returned, and the failure still persists so this tick
  spawns another. Reset to 0 when CI is green. 6 consecutive such
  ticks (~60 min of spawned agents returning without closing the
  failure) = the skill is ineffective against this failure; stop and
  notify the operator to intervene.
- **State file is authoritative.** If the file is deleted while the
  loop is running, the next tick starts with a fresh slate and might
  double-spawn; this is a known limitation, acceptable given the
  `TaskList` liveness check is the real safety net.

## Interaction with other skills

- `/quickstart-fix` — invoked by the spawned agent for Quickstart
  failures. Same decision tree, adversarial-review flow.
- `/plan-do-review` — invoked by the spawned agent for non-Quickstart
  failures.
- `/monitor-loop` — has its own CI-fix inline workflow for the
  mainnet node's health. If both skills run concurrently, they may
  both detect the same CI failure and spawn agents. State files are
  separate, so deduplication via `active_task_id` won't cross skills.
  Operator should run one or the other, not both, against the same
  repo.
