---
name: project-loop
description: |
  Continuous always-on orchestrator for the henyey project pipeline. Each pass
  queries the board, central-picks up to N actionable issues by priority, and
  fans out parallel foreground specialist sub-agents (triage/plan/do/review-pr)
  with the right Claude model per stage. Owns concurrency, conflict-avoidance,
  CI pipelining, sibling-nit consolidation, and a self-reflection pass that files
  pipeline-improvement issues. Replaces scripts/project-tick-loop.sh. Use when
  the operator asks to "run the loop", "process the board continuously", or
  "keep the pipeline going".
argument-hint: "[--no-reflect] [--max-parallel=N]"
---

# /project-loop — continuous pipeline orchestrator

You are the single, always-on orchestrator for the henyey project management pipeline. You do **not** plan, implement, or review work yourself — you **pick** work centrally and **fan out** specialist sub-agents that do it. You manage the whole pipeline continuously: pick → dispatch → collect → merge → reflect → repeat, backing off when the board is empty and waking on new work.

This skill replaces `scripts/project-tick-loop.sh`. Unlike the old copilot multi-loop driver, there is **exactly one orchestrator** (you) and **no GitHub locking** — concurrency is owned in-process via central picking and an **in-memory in-flight set**. You are the mutual-exclusion mechanism; the design assumes you never run two copies of yourself.

It reuses `/project-tick`'s board query (Step 1), actionability filter (Step 2 + Step 2b CI-pending skip), pick ordering (Step 3), and per-stage dispatch (Step 5). Read that skill for the GraphQL query and the CI-rollup classifier — this skill does not re-paste them; it references them by step number.

## Configuration

- **`N` = max parallel sub-agents = 3** (override with `--max-parallel=N`). Never dispatch more than `N` specialists concurrently.
- **`KEEP_RECENT_DONE` = 10** — the loop keeps at most this many closed items in the `done` section, archiving the rest each pass (Step F2). Override with `--keep-recent-done=N`; set `--no-archive` to disable the trim entirely.
- **`--no-reflect`** — skip the self-reflection / self-improvement pass entirely (operator off-switch).
- Repo / project / field IDs: see `/project-tick` "Project board" section.

## In-memory state (lives for the whole session — NOT persisted to disk or GitHub)

- **`IN_FLIGHT`** — set of issue numbers currently dispatched to a sub-agent. Picks dedupe against this; it is cleared per-issue when that issue's sub-agent returns.
- **`AREA_LOCKS`** — set of "areas" (crate / script / skill-dir) currently being modified by an in-flight `/do` or `/review-pr`. Used for conflict-avoidance (below). Released when the owning sub-agent returns.
- **`CI_WAITERS`** — PRs whose CI is still pending (issue → PR mapping). Re-checked with a cheap `gh` call each pass (Step C); not a separate background process and not a dispatch slot.
- **`ANOMALY_LOG`** — the per-session anomaly log (append-only via the helper below; see "Self-reflection").
- **`PASS_HISTORY`** — recent picks/dispatches/outcomes, fed to the meta-reviewer.
- **`BACKOFF`** — current idle backoff interval (escalating).

## Main loop

**Loop init (once, before the first pass):** source the anomaly-log helper so its functions (`anomaly_log_append` / `anomaly_log_dump` / `anomaly_log_clear`) are in scope for the whole session:

```bash
source scripts/lib/pipeline-anomaly-log.sh
```

Repeat forever:

### Step A — Sync new repo issues onto the board

Before querying, make sure freshly filed repo issues are tracked on project #2 (otherwise the picker never sees them). List open repo issues, diff against the board item set from the last board query, and add any untracked ones:

```bash
# Open repo issues not yet on the project board → add them. `item-add` adds the
# item with an EMPTY Status field, which the picker (Step C) never sees — so we
# immediately set Status to `backlog` (idempotent adds-then-sets) so /triage can
# move it through the pipeline. Best-effort; skip on error.
for N in $(gh issue list --repo stellar-experimental/henyey --state open \
            --json number --jq '.[].number'); do
  case " $TRACKED_ISSUE_NUMBERS " in
    *" $N "*) : ;;                       # already on the board
    *) gh project item-add 2 --owner stellar-experimental \
         --url "https://github.com/stellar-experimental/henyey/issues/$N" \
         2>/dev/null \
         && bash .github/skills/shared/scripts/move-issue-status.sh "$N" backlog \
              2>/dev/null || true ;;
  esac
done
```

`TRACKED_ISSUE_NUMBERS` is the set of `content.number` from the previous pass's board query (Step B). On the first pass, run Step B once first, then sync. If you are unsure whether sync is safe (rate limits, permissions), skip it and just pick from issues already on the board — triage of a missing issue will surface late but the pipeline still functions.

### Step B — Query the board

Run `/project-tick` Step 1 (the paginated GraphQL query). Cache the returned nodes and the set of `content.number` as `TRACKED_ISSUE_NUMBERS` for the next pass's Step A.

### Step C — Filter to actionable items

Apply `/project-tick` Step 2 (OPEN, status ∈ {backlog, ready-for-planning, ready-for-doing, in-review}, **assignees empty**) and Step 2b (skip in-review items whose CI is still pending — they will be re-evaluated next pass; their CI is already being polled in `CI_WAITERS` if a `/do` opened them). Then drop any issue already in `IN_FLIGHT`.

### Step D — Central pick (up to N, conflict-avoiding)

Sort the actionable list by `/project-tick` Step 3 ordering, which **leads with the urgent override**: any OPEN issue labeled `urgent` sorts to the **very top**, ahead of all state-priority tiers (an `urgent` `backlog` item **outranks** a non-urgent `in-review` item); then state priority `in-review` > `ready-for-doing` > `ready-for-planning` > `backlog`; then label priority; then age. An `urgent` issue still in untriaged `backlog` is therefore picked **first** and dispatched to `/triage` immediately on the next pass — never deferred behind lower-priority work. Triage urgents immediately — first, **not a triage bypass**: urgents still flow through `/triage` via the existing `backlog → haiku /triage` dispatch in Step E; they simply go first. Then walk the sorted list and select up to `N − |IN_FLIGHT|` issues, applying **conflict-avoidance**:

- For each candidate, estimate the **area** it will touch:
  - `in-review` / `ready-for-doing`: the crate(s) / script(s) / skill-dir(s) named in the linked PR's changed files (for in-review) or the plan's "Files to change" section (for ready-for-doing). When unknown, treat the issue's primary label/title area as the lock.
  - `ready-for-planning` / `backlog`: planning and triage do not modify code, so they take **no area lock** (always parallelizable).
- **Skip** a candidate whose area overlaps any area in `AREA_LOCKS` or any other already-picked candidate this pass — serialize same-area work. Pick the **next non-conflicting** issue instead. The skipped issue is re-evaluated next pass once the conflicting work lands.
- When you pick an issue that will modify code, add its area(s) to `AREA_LOCKS` and its number to `IN_FLIGHT`.

If nothing is pickable → go to **Step G (idle)**.

### Step E — Fan out (parallel foreground sub-agents)

Dispatch the picked issues **in parallel** — emit all the Agent/Task tool calls **in a single message** (up to `N` concurrent), each `subagent_type: general-purpose`, `run_in_background: false`, with the **per-stage `model` set explicitly on the Agent call** exactly as in `/project-tick` Step 5:

| Status | `model` | Sub-agent prompt |
|---|---|---|
| `backlog` | `haiku` | `Run /triage <ISSUE> and report the final board state transition.` |
| `ready-for-planning` | `opus` (or `sonnet` if the issue carries **both** the `self-improvement` and `trivial` labels) | `Run /plan <ISSUE> and report the final board state transition.` |
| `ready-for-doing` | `opus` (or `sonnet` if the issue carries **both** the `self-improvement` and `trivial` labels) | `Run /do <ISSUE> and report the final board state transition.` |
| `in-review` | `opus` (or `sonnet` if the linked PR is **docs-only** — see the pre-check below) | `Run /review-pr <ISSUE> and report the final board state transition.` |

**Model-tiering pre-check for `in-review`.** Before dispatching `/review-pr`, do a cheap orchestrator-level check of the linked PR's changed files and select `sonnet` when **every** changed path is docs-only — the same class `/review-pr` treats as `kind: docs` (docs/refactor/test-only get the lighter "existing tests still pass" review, not the full feature test-coverage gate). This selection happens entirely at the dispatcher; the `/review-pr` specialist is unchanged.

```bash
# Docs-only iff EVERY changed path matches *.md, docs/**, or README* (case-insensitive
# on the basename). Any non-matching path (a *.rs, *.toml, *.sh, workflow, etc.) → opus.
mapfile -t FILES < <(gh pr view "$PR_NUM" --repo stellar-experimental/henyey --json files --jq '.files[].path')
DOCS_ONLY=1
for f in "${FILES[@]}"; do
  case "$f" in
    *.md|docs/*|README|README.*|*/README|*/README.*) ;;   # docs-only path, keep scanning
    *) DOCS_ONLY=0; break ;;                                # any code/config path → not docs-only
  esac
done
[ "${#FILES[@]}" -gt 0 ] && [ "$DOCS_ONLY" -eq 1 ] && MODEL=sonnet || MODEL=opus
```

Same rationale as `/project-tick`: haiku for triage, opus for plan/code/review — **except** trivial self-filed pipeline work (an issue carrying **both** `self-improvement` and `trivial`, the low-risk class Step H's reflection pass files) dispatches `/plan` and `/do` on `sonnet`, and a docs-only PR (every changed file `*.md` / `docs/**` / `README*`) gets a `sonnet` `/review-pr`; anything touching real crate code / parity-sensitive logic / config still gets `opus`. Cross-model diversity is now provided by the **adversarial refute pass (run in-agent)** inside `/plan` and `/review-pr`, not a second model family. Under this single-level dispatch model the specialists run their adversarial lenses (the `/plan` critics + refute, the `/review-pr` reviewers + refute) as **in-agent multi-lens passes** — the supported mode — because a dispatched specialist cannot itself spawn nested sub-agents. Option 1 (the orchestrator spawning the adversarial lenses as sibling sub-agents for true OS-level independence) is recorded as a documented **future upgrade hook**, to be wired here should a nested-spawn capability later become available.

All sub-agents run in the **foreground** (`run_in_background: false`) so you block on the batch and collect their reported state transitions. Sub-agent build artifacts (worktrees, cargo-targets) must stay under `~/data/<session>/` — the worktree-contract helper (`scripts/lib/agent-worktree-contract.sh`) enforces this; if a sub-agent reports a workspace that escaped `~/data`, record an anomaly (below).

### Step F — Collect results & post-dispatch handling

When the batch returns, for each sub-agent:

- Clear its issue from `IN_FLIGHT` and release its `AREA_LOCKS` area(s).
- Record the outcome in `PASS_HISTORY`.
- Detect anomalies (see "Self-reflection" for the signal list) and append them to the anomaly log.

Then handle these special cases before the next pass:

- **A `/do` opened a PR (new `in-review` item):** do **not** block a slot waiting on its CI. **Pipeline it** — record the PR in `CI_WAITERS` (issue → PR mapping) and immediately continue picking other work; the PR is excluded from picks until its CI resolves (Step C/Step 2b filters it out while pending). **There is no separate background poller.** The continuous main-loop structure *is* the polling mechanism: on each subsequent pass, Step C re-checks every `CI_WAITERS` entry with a single cheap call and either clears it (CI settled → the in-review item becomes actionable and Step D picks it for `/review-pr`) or leaves it pending for the next pass.

  ```bash
  # Cheap per-pass CI re-check for a CI_WAITERS entry (PR $PR_NUM, issue $ISSUE).
  # Run inline during Step C — NOT as a detached background job. Returns 0 (settled)
  # or 1 (still pending); the orchestrator's own loop supplies the "polling".
  ROLLUP=$(gh pr view "$PR_NUM" --repo stellar-experimental/henyey \
             --json statusCheckRollup --jq '.statusCheckRollup')
  TOTAL=$(echo "$ROLLUP" | jq 'length')
  PENDING=$(echo "$ROLLUP" | jq '[.[]|select((.status != null and (.status|ascii_upcase)!="COMPLETED") or (.status==null and (.state|ascii_upcase)=="PENDING"))]|length')
  if [ "$TOTAL" -gt 0 ] && [ "$PENDING" -eq 0 ]; then echo settled; else echo pending; fi
  ```

  When the check reports **settled**, clear the `CI_WAITERS` entry so the next Step D pick dispatches `/review-pr`: green CI → `/review-pr` merges; red CI → `/review-pr` bounces it. If you want a real delay between re-checks so you don't hammer `gh` while a PR's CI is slow, do **not** spawn a background job — either rely on the idle-backoff timing from Step G (a drained board naturally spaces the passes out), or, when the board is otherwise empty but a `CI_WAITER` is still pending, use a short bounded inline foreground `sleep` within the pass before the next re-check. A detached background bash job **cannot** re-invoke or wake this Claude session, so it can never trigger the `/review-pr` dispatch — only the orchestrator's own continued looping does.

- **Known flaky testnet check (#2916 — "horizon core up"):** if the **sole** failing check on a PR is the flaky testnet "horizon core up" job and everything else is green, you may **re-run that one failed job once** — **but only after the main-health pre-check below clears it as a genuine, PR-isolated flake.**

  **Main-health pre-check (before any re-run).** A re-run is only legitimate when the failure is isolated to the PR. First check whether the **same check is systematically red on `main`**. The "horizon core up" leg is a **check within the quickstart workflow**, not a standalone workflow — so query `main`'s recent check-runs and filter **by check name** (`HEAD` + the last ~3 commits), not by a 1:1 workflow:

  ```bash
  # Per-commit conclusion of the SAME check (by name) on recent main HEAD commits.
  gh run list --repo stellar-experimental/henyey --branch main --limit 5 \
    --json headSha,conclusion,name,workflowName,createdAt
  # (or `gh api` for per-check-run conclusions when the check is a job inside a workflow)
  ```

  - If the same check is **red across ≥2 recent `main` commits**, classify it as a **regression / infra-outage — NOT flaky**. Do **NOT** re-run. File (or escalate) an **`urgent`-labeled** issue capturing the **green→red commit boundary** (last green `main` sha → first red `main` sha) and record the escalation. The new `urgent` issue is then drained **first** by the urgent-override pick order (Step D), so a systematic CI regression is surfaced and prioritized instead of being masked by a blind re-run.
  - Only treat it as **flaky and re-run** when **`main` is green** and the failure is isolated to the PR:

  ```bash
  gh run rerun <RUN_ID> --repo stellar-experimental/henyey --failed
  ```

  This regression-escalation path is **distinct from** the existing `flaky-rerun` anomaly (below): `flaky-rerun` records a genuine isolated flake re-run ≥2× (flaky-quarantine candidate); the main-health guard instead escalates a *systematic* red as an `urgent` regression and does not re-run at all. Record an anomaly if you re-run the same check ≥2× on the flaky path. Do not re-run other failing checks automatically — those are real and go to `/review-pr` for a bounce.

- **Sibling nit consolidation:** when several follow-up issues are sibling **nits** touching the **same file/area** (typically self-improvement or post-review follow-ups filed against one PR), consolidate them into **one** `/do` PR rather than one PR each. Dispatch a single `/do` whose prompt lists all the sibling issue numbers and instructs it to close them together (`Closes #a #b #c` in the PR body). This avoids N near-identical PRs churning the same file and N review cycles.

- **Force-converge:** you do **not** implement force-converge — `/review-pr` owns the lifetime bounce-cap force-converge behavior (at the cap, green CI auto-merges and residual reviewer concerns become follow-up issues; only red/pending CI still blocks). Just let `/review-pr` handle it and record an anomaly when an issue hits the cap.

If actionable work remains and slots are free → go back to **Step C** immediately (same pass continues picking). Otherwise → **Step F2**.

### Step F2 — Trim the `done` section (keep ≤ `KEEP_RECENT_DONE` most recent)

Once per pass (after dispatch handling, before idle), keep the `done` column from growing without bound: archive every closed item except the **`KEEP_RECENT_DONE` (=10)** most-recently-closed. Best-effort — never block a pass; skip on error and record an anomaly:

```bash
[ "${NO_ARCHIVE:-0}" = "1" ] || \
  bash .github/skills/shared/scripts/archive-stale-done.sh --keep-recent "${KEEP_RECENT_DONE:-10}" \
  || anomaly_log_append "done-trim archive-stale-done.sh failed (non-fatal)"
```

Notes:
- Selection is by `closedAt` recency over CLOSED project items (which is what populates `done`); it uses GitHub's `archiveProjectV2Item` mutation, which is **reversible** (items can be unarchived). Idempotent — a no-op once ≤ `KEEP_RECENT_DONE` closed items remain, so the steady-state cost is one paginated query per pass.
- The same script's age-based mode (`[days]`, default 2) remains available for cron use; `/project-loop` uses the count-based `--keep-recent` mode so the visible `done` section stays bounded regardless of churn.
- Requires a token with org Projects scope (the script pre-flights and fails loudly otherwise — see its header); on a token-less run the step just logs the anomaly and the pass continues.

### Step G — Idle / backoff

When no actionable items remain (board drained for now):

1. Run the **self-reflection pass** (Step H) **if due** (always on an idle transition; at most ~once/hour while busy — track the last reflection time).
2. Schedule the next pass after a **backoff**: escalating intervals — start short (~30s), then grow (1m → 2m → 5m → 10m), **capped at ~15–30 min**. Reset the backoff to the short interval as soon as a pass does real work (picked/dispatched something) or a `CI_WAITER` fires.
3. Wake mechanism: the orchestrator is a **single long-running Claude session**, so the only way to sleep for the backoff and then continue the **same** session is a plain **bounded foreground `sleep <backoff>`** inline in the pass (bounded because the whole point is that this session stays alive and loops). After the `sleep` returns, fall through to the next pass. A detached background bash job **cannot** wake or re-invoke a Claude session — it runs in its own process and has no channel back into the session — so it must **not** be used here; likewise there is no in-session "self-schedule" that resumes a suspended session. There is nothing to poll while idle other than `CI_WAITERS`, which are re-checked at the top of the next pass (Step C) once the `sleep` returns. **Caveat (unchanged and correct):** the orchestrator only runs while its Claude session is alive; a foreground `sleep` keeps *this* session looping but cannot survive a session restart. For true 24/7 operation across restarts, pair `/project-loop` with an external scheduler (cron / the `schedule` skill) that relaunches it.

Then loop back to **Step A**.

## Self-reflection / self-improvement pass (Step H)

The key new capability: the orchestrator notices when the **pipeline itself** is buggy or improvable, and files issues to fix it — which then flow through the normal pipeline (dogfooding).

### Anomaly log

Maintain a per-session anomaly log via the helper `scripts/lib/pipeline-anomaly-log.sh` (authored separately). Assume it exposes:

- `anomaly_log_append "<signal>" "<evidence>"` — record one anomaly (signal = short tag; evidence = PR/issue/run links + detail).
- `anomaly_log_dump` — emit the accumulated log for the meta-reviewer.

Append an anomaly whenever you observe (these are the signals to watch — each was hit manually in prior sessions):

- **`merge-fallback`** — a merge fell back from the merge helper's `attempt_merge` to a direct `gh pr merge` (merge-helper bug).
- **`flaky-rerun`** — the same CI check was re-run ≥2× as "flaky" (flaky-test / quarantine candidate).
- **`refute-overturn`** — a reviewer/critic finding was overturned by the refute pass (false-positive-prone prompt).
- **`bounce-cap`** — an issue exceeded N bounces or hit `/review-pr`'s force-converge cap (skill-guidance gap).
- **`subagent-limit`** — a sub-agent stalled, orphaned, or reported a tool/model/skill limitation.
- **`disk-escape`** — a workspace escaped `~/data` (disk-leak-guard regression).
- **`cross-pr-conflict`** — repeated cross-PR conflicts (serialization gap).

### Cadence

Run reflection on **each idle transition** (board drained — you have the fullest picture and spare cycles) and **at most ~once/hour while busy**. Skip entirely if `--no-reflect` was passed.

### Meta-reviewer dispatch

Spawn **one** `sonnet` "meta-reviewer" Agent sub-agent (`subagent_type: general-purpose`, foreground) over `anomaly_log_dump` + recent `PASS_HISTORY`. `sonnet` is the right tier here: the meta-reviewer only dedupes, classifies, and files issues — no code changes and no deep code/plan reasoning — so it does not need `opus`. Its instructions:

1. **Dedupe** against open issues labeled `pipeline` or `self-improvement` (`gh issue list --state open --search 'label:self-improvement,pipeline'` — the comma is OR in gh search, so this matches issues with EITHER label; multiple `--label` flags would AND them and miss `self-improvement`-only issues). For a recurrence of an already-filed finding, **comment on the existing issue** (note the new occurrence + evidence) — do **NOT** refile.
2. **Classify** each genuine, non-duplicate finding as `trivial` (clear, low-risk fix — the pipeline can auto-implement it next pass) or `needs-design` (deeper change that warrants a full `/plan` pass). This is a *planning-depth* hint, not an operator-handoff — both classes are drained autonomously through the normal pipeline (see below).
3. **File** the issues via `gh issue create`, labeled `self-improvement` (plus `trivial` where apt), with reproduction steps and evidence (PR/run/issue links). For `needs-design`, write enough context for `/triage` and `/plan` to pick it up cold.
4. It must **NOT** edit any live skill or script — it only files issues. Self-changes are implemented by the normal pipeline (triage → plan → do → review-pr), so every self-edit is adversarially reviewed + CI-gated.
5. **Cap: ≤3 new self-issues per cycle.** If more than 3 genuine findings exist, file the top 3 by severity and note the remainder in the highest-priority issue's body.

The filed `self-improvement` issues are picked up by subsequent passes like any other work — the pipeline improves itself through its own machinery.

**Drain your own follow-ups — do not park them for the operator.** Self-improvement and follow-up issues the loop files (including the residual concerns `/review-pr` and `/plan` spin off at their force-converge caps) flow through the **normal pipeline like any other backlog item** — triage → plan → do → review-pr → merge. Being autonomous means *closing the loop on its own improvements*, not handing them off: the orchestrator picks them up by priority on subsequent passes and drives them all the way to merged, exactly as it would a repo bug. It does **not** defer, park, or wait on the operator to triage them. (`trivial` items the pipeline auto-implements next pass; `needs-design` items still run through `/triage` and `/plan` autonomously — the label informs planning depth, it is not an operator-handoff hold.) The ≤3-new-issues-per-cycle cap above is solely about not *over-filing* new findings; it never means deferring an already-filed issue.

After the meta-reviewer sub-agent returns and its issues are filed, **clear the anomaly log** so the next cycle starts fresh (matches the helper header's documented contract: "dumps the log, files `self-improvement` issues, then clears it"):

```bash
anomaly_log_clear
```

## When NOT to use

- **Do not** run two `/project-loop` orchestrators concurrently — there is **no locking by design**; a second orchestrator would double-dispatch and corrupt the in-memory in-flight set.
- **Do not** call `/project-loop` from inside a specialist (`/plan`, `/do`, `/triage`, `/review-pr`) — it dispatches *to* them.
- **Do not** use it for a single targeted pick or `--issue=<num>` recovery — that is `/project-tick`'s job.
- **Do not** use it to manage the validator monitor — that pipeline (`monitor-loop` / `monitor-tick`) is separate and out of scope.

## Operational notes

- **Single orchestrator only.** Concurrency is owned in-process (central pick + in-memory `IN_FLIGHT`/`AREA_LOCKS`). There is no flock, no sentinel comment, no cooldown file, and no assignee race anywhere in the pipeline. Running two loops at once is undefined behavior — there is nothing to mutually exclude them.
- **Disk hygiene.** All sub-agent build artifacts (worktrees, cargo-targets) must stay under `~/data/<session>/`; the worktree-contract helper (`scripts/lib/agent-worktree-contract.sh`, the #2843 disk-leak guard) enforces this. If anything escapes `~/data`, record a `disk-escape` anomaly and surface it to reflection.
- **Self-modifying gate.** PRs that modify the pipeline's own skills/scripts (`.claude/skills/{project-loop,project-tick,plan,do,review-pr,triage}/`, `scripts/lib/*`) are **detected and flagged** by `/review-pr`'s `self-modifying` gate, which subjects them to **extra reviewer scrutiny** (skill frontmatter/markdown still parses, scripts still `bash`-source cleanly, no dispatch-table/step-numbering breakage). They are **not** held for operator approval — on triple-green (2 reviewers APPROVE + refute + CI) they merge **autonomously** via `attempt_merge`, like any other PR. The gate is adversarial-review + CI, not a separate operator hold. If a reviewer genuinely doubts the change is safe for the live pipeline, that's a `CHANGES_REQUESTED` bounce, not an escalation. Self-improvement PRs do **not** wait for the operator.
- **Refresh skills after a self-modifying merge.** After a self-modifying PR that changes a pipeline skill or script merges, refresh the worktree/checkout the orchestrator reads its skills from, so the updated skill takes effect on the next pass — e.g. `git -C <skill-read-worktree> fetch && git -C <skill-read-worktree> reset --hard origin/main`. Without this, the loop keeps running the pre-merge skill text until its checkout is updated.
- **Conflict-avoidance is the orchestrator's job.** Never dispatch two code-modifying sub-agents whose changes touch the same crate/script/skill concurrently — serialize them via `AREA_LOCKS` and pick the next non-conflicting issue.
- **CI is pipelined, not blocking.** A pending PR never occupies a dispatch slot; it sits in `CI_WAITERS` and is re-checked with a cheap `gh` call at the top of each pass (Step C) — the continuous loop itself is the polling mechanism, with no separate background process — while the orchestrator keeps picking other work meanwhile.
- **Idle is productive.** The idle backoff is when reflection runs and when new repo issues are synced onto the board — the loop never truly sleeps without first checking for self-improvement work and untracked issues.
