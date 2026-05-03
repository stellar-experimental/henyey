---
name: plan-do-review
description: Review a proposal issue with adversarial critics, converge on a plan, execute it, and iterate review-fix until clean. Drives the henyey project board (Backlog → in plan → In progress → In review → Done; Blocked on failure).
argument-hint: "[issue-number] [--model <model>] [--max-proposal-rounds N] [--max-review-rounds N]"
---

Parse `$ARGUMENTS`:
- The first positional argument, if present, is a GitHub issue number.
- `--model <model>`: Model for critic and review agents (default: `"gpt-5.4"`).
- `--max-proposal-rounds N`: Max proposal↔critic iterations (default: 5).
- `--max-review-rounds N`: Max implement↔review-fix iterations (default: 3).

**If no issue number was provided, auto-select one.**

Auto-select only **unassigned** issues. Never auto-select an issue that is
already assigned to anyone (including yourself) — an existing assignment
signals that another worker, or a prior session of this skill, is or was
working on it. Auto-claiming it races with that worker and silently
piggybacks on stale context. To resume an in-progress issue, the caller
must pass the issue number explicitly as an argument.

**Auto-selection is project-board-native.** Eligible issues are those on the
henyey project (`stellar-experimental/henyey`, project #2) currently in the
`Backlog` column. Issues not yet on the project are NOT auto-selectable —
the operator adds them to the board first. The `Blocked`, `in plan`,
`In progress`, `In review`, and `Done` columns are excluded automatically by
the Backlog filter; that is how the board replaces the legacy `not-ready` and
`plan-do-review-loop-failed` labels.

Fetch all open Backlog items once, then pick by priority label
(`urgent` → `high` → `medium` → `low`) with oldest-first as the tiebreaker:

```bash
# One paginated GraphQL query for all Backlog items on project #2.
# `--paginate` requires the cursor variable to be named exactly $endCursor;
# `jq -s` is required because gh outputs one JSON object per page.
backlog_json=$(gh api graphql --paginate -f query='
  query($org: String!, $proj: Int!, $endCursor: String) {
    organization(login: $org) {
      projectV2(number: $proj) {
        items(first: 100, after: $endCursor) {
          pageInfo { endCursor hasNextPage }
          nodes {
            status: fieldValueByName(name: "Status") {
              ... on ProjectV2ItemFieldSingleSelectValue { optionId }
            }
            content {
              ... on Issue {
                number title createdAt state
                assignees(first: 5) { nodes { login } }
                labels(first: 20)    { nodes { name } }
              }
            }
          }
        }
      }
    }
  }' -f org=stellar-experimental -F proj=2)

# Backlog option id is f75ad846 (verified). Pick the first eligible issue
# per priority tier; oldest first within a tier.
ISSUE=
for priority in urgent high medium low; do
  ISSUE=$(jq -rs --arg p "$priority" '
    [ .[].data.organization.projectV2.items.nodes[]
      | select(.status.optionId == "f75ad846")
      | select(.content.state == "OPEN")
      | select((.content.assignees.nodes | length) == 0)
      | select(.content.labels.nodes | map(.name) | index($p))
    ] | sort_by(.content.createdAt) | .[0].content.number // empty
  ' <<< "$backlog_json")
  [ -n "$ISSUE" ] && break
done

# Priority-5 fallback: any Backlog open unassigned, oldest first.
if [ -z "$ISSUE" ]; then
  ISSUE=$(jq -rs '
    [ .[].data.organization.projectV2.items.nodes[]
      | select(.status.optionId == "f75ad846")
      | select(.content.state == "OPEN")
      | select((.content.assignees.nodes | length) == 0)
    ] | sort_by(.content.createdAt) | .[0].content.number // empty
  ' <<< "$backlog_json")
fi
```

If `$ISSUE` is still empty, **stop** with a message: "No eligible unassigned
issues found in Backlog for auto-selection."

Otherwise, set `$ISSUE` to the selected issue number and announce:
"Auto-selected issue #$ISSUE: <title>".

**Set `$AUTO_SELECTED` = `true`** when the issue was auto-selected (no argument
provided), or `false` when an explicit issue number was given.

**Assign the issue to yourself as a concurrency lock** (applies to both
auto-selected and explicit issue numbers):
```bash
gh issue edit $ISSUE --add-assignee "@me"
```

**Verify we hold the lock alone.** `gh issue edit --add-assignee` does NOT
fail when someone else is already assigned — it silently appends. To detect
a race (another worker assigned themselves between our query and our
assignment), re-fetch the issue and confirm we are the *only* assignee:

```bash
ME=$(gh api user -q .login)
ASSIGNEES=$(gh issue view $ISSUE --json assignees --jq '[.assignees[].login] | join(",")')
if [ "$ASSIGNEES" != "$ME" ]; then
  # Someone else holds (or shares) the lock — back off and stop.
  gh issue edit $ISSUE --remove-assignee "@me"
  echo "Could not claim issue #$ISSUE exclusively — assignees: $ASSIGNEES"
  exit 0
fi
```

If verification fails, **stop** with the message above. Do NOT proceed to
Step 1.

**Failure handling:** If the skill fails at any point, before stopping:
1. Unassign yourself to release the concurrency lock:
   ```bash
   gh issue edit $ISSUE --remove-assignee "$(gh api user -q .login)"
   ```
2. Move the issue to the `Blocked` column. This applies whether the issue
   was auto-selected or passed explicitly — the operator can review the
   failure and re-triage by moving it back to `Backlog`. Auto-select
   never picks Blocked items, so this also prevents infinite retry loops:
   ```bash
   bash .github/skills/plan-do-review/scripts/move-issue-status.sh "$ISSUE" Blocked
   ```
This releases the lock so other workers can see the issue is no longer in
progress.

# Plan-Do-Review

Adversarial proposal refinement → full implementation → iterative review-fix.

This skill automates the workflow: read a GitHub issue proposal, have an
independent agent critique it, rewrite incorporating feedback, repeat until
the proposal converges, then execute the plan, have it reviewed, fix issues,
and iterate until the review is clean.

The orchestrator (you) manages state, rewrites proposals, and implements code.
All reviews and critiques are delegated to independent sub-agents so that
review is adversarial and unbiased.

---

## Guiding Principles

**Prefer long-term readable, sustainable, elegant, safe building blocks over
short-term patches — even at the cost of significant refactors.**

These principles apply at every stage of this skill — when rewriting the
proposal, when critiquing it, and when executing it:

- **Root causes over symptoms.** Address the underlying design flaw, not the
  surface manifestation. A fix that handles only the one caller that reported
  a bug is almost always the wrong answer when several callers share the same
  broken assumption.
- **Readable.** Favor code that is easy for a newcomer to read and reason
  about. Name things well. Break long functions. Let control flow mirror the
  problem domain.
- **Sustainable.** Favor designs that remain correct as the code evolves.
  Push invariants into types (newtypes, enums, const generics), constructors,
  and shared helpers so future changes cannot accidentally violate them.
- **Elegant.** Favor the minimum set of concepts that expresses the
  solution cleanly. Prefer standard Rust idioms — ownership over cloning,
  iterators over index loops, `?` over `match` chains, `Result`/`Option` over
  sentinel values.
- **Safe.** Favor designs that make incorrect states unrepresentable and
  error paths explicit. Never fail silently. Close races; don't paper over
  them with retries or sleeps.
- **Scope honestly.** If the best fix requires changing a public API,
  restructuring types, introducing `Arc`/`Cow`/lifetimes, splitting or
  merging modules, or touching several crates — propose that. The goal is
  long-term code health, not the smallest possible diff. Deferred
  refactors should be filed as follow-up issues, not papered over.
- **Parity-preserving.** In protocol/consensus/ledger code, long-term
  elegance never justifies diverging from stellar-core behavior. Match
  stellar-core semantics exactly; elegance applies to *how* we express
  those semantics in Rust.

When these principles conflict with proposal minimalism, the principles
win. A significant refactor that produces a sound, idiomatic foundation is
always preferable to a narrow patch that leaves the latent design problem
in place.

---

## Step 1: Fetch and Parse the Issue

Initialize context variables:

```bash
HARNESS="${HARNESS:-Copilot CLI}"
export HARNESS
```

Run:
```bash
gh issue view $ISSUE --json title,body,labels,state,comments,number
```

Extract:
- **Title**: the issue title
- **Body**: the full proposal / description
- **Comments**: any existing discussion (prior reviews, context)
- **State**: must be open (if closed, stop and report)

### Resume from Prior Run

Before the dependency check or readiness triage, check whether a previous
invocation already made progress on this issue. Scan the comments for
`## 📝 Proposal Draft (Round N/M)` and `## 🔍 Critic Response (Round N/M)`
headers.

**If prior proposal/critic comments exist:**

1. Find the **highest round number** across all `📝 Proposal Draft` comments.
   Call this `last_proposal_round`.
2. Check whether a `🔍 Critic Response (Round last_proposal_round/M)` comment
   exists for that round.
3. **If a critic response exists for the last round:**
   - Extract its verdict. If `APPROVED`, check for a `## Converged Proposal`
     comment — if present, skip straight to Step 3 (implementation). If no
     converged proposal, post one and proceed to Step 3.
   - If `REVISE`, extract the numbered feedback items from that critic
     response. Set `proposal_round = last_proposal_round`. Extract the last
     proposal text as `current_proposal`. Skip Step 1 exploration and
     readiness triage — proceed directly to Step 2's REVISE handler
     (investigate feedback, rewrite, loop to 2a).
4. **If no critic response exists for the last round:**
   - The previous run posted a proposal but crashed before the critic ran.
     Extract the last proposal text as `current_proposal`. Set
     `proposal_round = last_proposal_round - 1` (so the next increment
     brings it back to the same round number). Skip exploration — proceed
     directly to Step 2a (spawn critic for the existing proposal).
5. **If a `## Converged Proposal` comment exists:**
   - Skip to Step 3. Check for existing worktree/branch from prior run.

**If no prior comments exist**, proceed normally to Blocker-Ancestor
Resolution and Readiness Triage below.

> **Why this matters.** Without resume, a context-window crash causes the
> loop script to restart from scratch — re-exploring, re-proposing from
> Round 1, and wasting all prior convergence progress. With resume, each
> restart picks up where the last one left off, making forward progress
> even across multiple session crashes.

### Dependency Check

Before triaging readiness, check whether the issue has **unmet dependencies**
(is blocked by another open issue).

**Procedure:**

1. Read the current issue's body and comments. Using your understanding of the
   text, identify any issue numbers that this issue is **blocked by** — look
   for patterns like "blocked by #N", "depends on #N", "requires #N first",
   tasklist items `- [ ] #N`, or similar contextual references that indicate
   a prerequisite relationship. **Only** extract issues that are genuine
   blockers; ignore issues that are merely referenced or related.

2. For each candidate blocker, fetch it:
   ```bash
   gh issue view <N> --json number,state
   ```
   Filter to only **open** issues. If no open blockers remain, the current
   issue is not actually blocked — continue to Readiness Triage.

3. If any open blocker(s) exist, the issue has unmet dependencies. **Stop**:
   1. Move the issue to `Blocked`:
      ```bash
      bash .github/skills/plan-do-review/scripts/move-issue-status.sh "$ISSUE" Blocked
      ```
   2. Post a comment listing the blockers:
      ```bash
      gh issue comment $ISSUE --body "Moved to Blocked: depends on open issue(s) #X, #Y. Will be retried once dependencies are resolved."
      ```
   3. Unassign yourself:
      ```bash
      gh issue edit $ISSUE --remove-assignee @me
      ```
   4. **Stop.** Do not proceed to Readiness Triage or Step 2.

If no unmet dependencies, continue into Readiness Triage.

---

### Readiness Triage

Before proceeding, assess whether the issue is actionable. An issue is **not
ready** if any of these are true:

- The body is empty or contains only a vague one-liner with no concrete proposal
- It requires information or decisions that are not yet available
- It describes a problem but proposes no approach and the correct approach is
  unclear even after reading the referenced code

If the issue is **not ready**:

1. Move the issue to `Blocked`:
   ```bash
   bash .github/skills/plan-do-review/scripts/move-issue-status.sh "$ISSUE" Blocked
   ```
2. Post a comment explaining why the issue is not ready and what is needed:
   ```bash
   gh issue comment $ISSUE --body "Moved to Blocked: {reason}. This issue needs {what's missing} before it can be picked up."
   ```
3. Unassign yourself:
   ```bash
   gh issue edit $ISSUE --remove-assignee @me
   ```
4. Stop. Do not proceed to Step 2.

If the issue **is ready**, the Step 2 entry hook below will move it from
its current column (typically `Backlog`, but sometimes `Blocked` if a prior
run blocked it and the dependency has since been resolved) into `in plan`.
No explicit cleanup is needed here.

Build context for the issue — but **budget your context aggressively**.

> **Context budget rule.** You must survive the full skill lifecycle:
> up to 5 proposal↔critic rounds, then implementation, then up to 3
> review-fix rounds. If you exhaust your context window during
> exploration, the session will exit before the proposal even converges.
>
> **Hard limits for initial exploration (Round 0):**
> - Read at most **15 files** (via `view` or `view_range`).
> - Each read should be a targeted `view_range` of 20–50 lines, not a
>   full file.
> - Total exploration output should stay under ~3,000 lines. If you hit
>   this, stop and write your first draft with what you have.
>
> **Limits for subsequent rounds (Rounds 2+):**
> - Do NOT re-explore broadly. Only re-read specific lines that the
>   critic flagged — typically 1–3 targeted `view_range` calls per
>   feedback item.
> - If the critic claims a code path exists that you didn't see, verify
>   with a single `grep` + one `view_range`. Do not read surrounding
>   context "just in case."
>
> **General rules:**
> - **Prefer `grep`/`glob` over `view`.** Search for specific symbols,
>   function names, or config keys mentioned in the issue. Do not read
>   entire files when a 5-line match suffices.
> - **Prefer `view_range` over full-file reads.** When you need to read
>   code, read only the relevant function or block (20–50 lines), not
>   the entire file.
> - **Stop exploring once you can write a first draft.** Your first
>   proposal does not need to be perfect — the critic agent has its own
>   full context window and will verify claims against the codebase.
>   Trust the critic to catch what you missed; that is the whole point
>   of the adversarial loop.
> - **Do not pre-read code "just in case."** Only read code that
>   directly informs a specific claim in your proposal.

Initialize tracking:
```
proposal_round = 0
review_round = 0
current_proposal = <issue body + any relevant context>
```

### Proposal Output Requirements

Every proposal (initial and subsequent rewrites) should include:

- **## Problem** — clear statement of the root cause
- **## Proposed Fix** — concrete changes and their rationale
- **## Affected Paths** — files, functions, and code paths that will be modified
- **### Parity Verification** — evidence that this change maintains stellar-core parity:
  - Feature behavior vs stellar-core v25: [link to code reference or "N/A — new feature"]
  - Determinism/config assumptions: [file:line evidence or rationale]
  - Edge cases checked: [list or "n/a"]
- **### Test Strategy** — (optional but strongly recommended) outline of how the fix will be tested:
  - Unit tests: [areas to cover]
  - Integration/regression tests: [scenarios]
  - Existing tests affected: [changes needed or "none"]
- **### Affected/Similar Paths Searched** — list search terms, call graph paths checked, confirmed affected locations, and "not affected because…" notes. Allow `n/a` only with concrete justification (e.g., "new invariant local to this module").

---

## Step 2: Proposal Convergence Loop

**Move the issue to `in plan`** — proposal convergence is starting. This is
idempotent on resume: a prior run may have already moved it here, in which
case this is a no-op.

```bash
bash .github/skills/plan-do-review/scripts/move-issue-status.sh "$ISSUE" "in plan"
```

Repeat until `VERDICT: APPROVED` or `proposal_round >= max_proposal_rounds`:

> **Forced convergence rule.** If `proposal_round == max_proposal_rounds - 1`
> (the penultimate round) and the critic returns `REVISE`, do NOT loop back
> for another critic round. Instead, incorporate the feedback into a final
> rewrite, treat it as converged, and proceed to Step 2c. This prevents the
> common failure mode where the agent crashes from context exhaustion on the
> last round, wasting all prior progress. The converged proposal comment
> should note: "Converged after forced acceptance at round N (critic did not
> fully approve)."

### 2a: Spawn Critic Agent

Increment `proposal_round`.

**Post the proposal draft to the issue.**

> **CRITICAL — read before posting.** The pattern `gh issue comment ... --body "$(cat <<'EOF' ... EOF)"` is a template. The `{placeholder}` tokens must be replaced with **literal text** before the bash command runs. Do NOT substitute a placeholder with a shell expression like `$(cat /tmp/foo.md)` — the heredoc is single-quoted, so no shell expansion happens, and the literal string `$(cat /tmp/foo.md)` ends up in the GitHub comment body. To avoid this class of bug, use `--body-file` to point `gh` at a file on disk, which bypasses shell interpolation entirely. Write the full comment body (header + content + footer) to a temporary file, then post it as one unit.

```bash
# Preferred pattern — write to a file, then post via --body-file.
tmpfile=$(mktemp)
{
  printf '## 📝 Proposal Draft (Round %s/%s)\n\n' "$proposal_round" "$max_proposal_rounds"
  cat data/pdr-$ISSUE/proposal_r$proposal_round.md
  printf '\n\n---\n\n*Submitting to adversarial critic for review…*\n'
  printf '\n---\n\n*Created by `/plan-do-review` skill (%s, model: %s)*\n' "$HARNESS" "$MODEL"
} > "$tmpfile"
gh issue comment $ISSUE --body-file "$tmpfile"
rm -f "$tmpfile"
```

**Do NOT use this legacy heredoc pattern** — it is a footgun when sub-agents do the textual substitution, as evidenced by the 16-comment incident on #1759/#1768:

```bash
# ❌ DO NOT USE — substituting {current_proposal} with `$(cat ...)` leaves the literal string in the comment.
gh issue comment $ISSUE --body "$(cat <<'DRAFT_EOF'
## 📝 Proposal Draft (Round {proposal_round}/{max_proposal_rounds})

{current_proposal}

---

*Submitting to adversarial critic for review…*
DRAFT_EOF
)"
```

Launch a background agent using the Task tool:
- **agent_type**: `"general-purpose"`
- **model**: `$MODEL`
- **name**: `"critic-round-{proposal_round}"`
- **description**: `"Critique proposal round {proposal_round}"`

The critic agent prompt must include:

```
You are an independent technical reviewer for a software proposal on the
henyey project (a Rust implementation of stellar-core). Your job is to find
gaps, incorrect assumptions, missing edge cases, scope issues, and
impractical suggestions.

## The Proposal

{current_proposal}

## Codebase Context

This is the henyey project — a Rust port of stellar-core. Key conventions:
- Determinism and stellar-core parity are non-negotiable
- Tests use Rust's built-in #[test] framework
- Code lives in crates/ (e.g., crates/tx, crates/ledger, crates/herder)
- stellar-core reference is at stellar-core/ (git submodule, pinned to v25.x)

## Your Task

Evaluate this proposal thoroughly:

1. **Correctness**: Are the technical claims accurate? Do the referenced
   code paths exist and behave as described?
2. **Completeness**: Does the proposal cover all affected code paths? Are
   there cases it misses?
3. **Spec clarity**: Does the proposal define exact ledger state, protocol
   version limits, config flags, pre/post conditions, and determinism
   assumptions? Are boundary conditions explicit?
4. **Feasibility**: Can this be implemented as described? Are there
   practical obstacles?
5. **Risk**: What could go wrong? What edge cases are not addressed?
6. **Scope**: Is the scope appropriate? Too broad? Too narrow?
7. **Stellar-core parity**: Will the proposed changes maintain or improve
   parity with stellar-core? Are citations to stellar-core code provided?
8. **Structural ambition**: Does the proposal go far enough? Could a
   bigger refactor — changing public APIs, restructuring types, using
   Arc/Cow/lifetimes, redesigning enums, splitting or merging modules —
   eliminate the *class* of bug rather than patching the one symptom?
   Prefer structural solutions that make incorrect states
   unrepresentable over minimal fixes that address one instance.
9. **Readability & sustainability**: Will the resulting code be easy to
   read, modify, and extend in six months? Does it push invariants into
   types (newtypes, enums, constructors) or shared helpers so future
   callers can't silently get it wrong? Does it use idiomatic Rust —
   ownership over cloning, iterators over index loops, `?` over match
   chains, `Result`/`Option` over sentinel values? Does it name things
   well and keep functions focused?
10. **Long-term vs. short-term tradeoff**: Is the proposed fix a durable
    building block, or a short-term patch that leaves the underlying
    design problem in place? If the latter, flag it — this skill
    explicitly prefers significant refactors that produce sound
    foundations over narrow patches that will need to be undone.

## Output Format

You MUST end your response with exactly one of these verdicts:

VERDICT: APPROVED
(if the proposal is sound and ready for implementation)

VERDICT: REVISE
(if the proposal needs changes — list specific actionable items below)

If REVISE, list each required change as a numbered item:
1. [specific actionable feedback]
2. [specific actionable feedback]
...

Be concrete. "Needs more detail" is not actionable. "Add handling for the
case where X is None at file.rs:123" is actionable.
```

### 2b: Process Critic Result

Read the agent result. Extract the verdict line.

**You MUST post the critic response to the issue before processing the
verdict.** This is not optional — the issue comment trail is the audit log.
Do not skip this step, even if the verdict is APPROVED.

> **Context hygiene.** The critic's full response can be very large. After
> posting it to the issue (below), extract only the **verdict** and the
> **numbered feedback items** into your working state. Do not keep the
> full critique text in your conversational context — it is preserved in
> the issue comment for the audit trail. When investigating feedback
> items, do targeted `grep`/`view_range` lookups rather than re-reading
> everything the critic referenced.
>
> **Prior-round trimming.** After each round, discard all prior proposal
> drafts and critic responses from your working context. Your working
> state should contain only:
> 1. The **current (latest) proposal** text
> 2. The **numbered feedback items** from the most recent critic response
> 3. The issue title and body (for reference)
>
> Prior rounds are preserved in the issue comment trail — you do not need
> them in context. This is critical for surviving 5 rounds without
> context exhaustion.

```bash
# Use --body-file (see the CRITICAL note in Step 2a).
tmpfile=$(mktemp)
{
  printf '## 🔍 Critic Response (Round %s/%s)\n\n' "$proposal_round" "$max_proposal_rounds"
  printf '<details>\n<summary>Full critique (click to expand)</summary>\n\n'
  cat data/pdr-$ISSUE/critic_r$proposal_round.md
  printf '\n\n</details>\n\n'
  printf '**Verdict: %s**\n\n' "$verdict"
  # If REVISE, append the numbered feedback items outside the <details> block
  # here (either inline printf lines or cat a second file).
  printf '---\n\n*Created by `/plan-do-review` skill → `critic-round-%s` sub-agent (%s, model: %s)*\n' "$proposal_round" "$HARNESS" "$MODEL"
} > "$tmpfile"
gh issue comment $ISSUE --body-file "$tmpfile"
rm -f "$tmpfile"
```

**If `VERDICT: APPROVED`**:
- The proposal has converged. Proceed to Step 3.

**If `VERDICT: REVISE`**:
- Extract the numbered feedback items.
- **Drop all prior proposal/critic text from your working context** — you
  only need the current proposal and these feedback items going forward.
- Investigate each feedback item — but **respect the Round 2+ exploration
  budget**: only read specific lines the critic referenced (1–3 targeted
  `view_range` calls per item). Do not re-explore the codebase broadly.
- Verify the critic's claims, determine which feedback is valid.
- Rewrite `current_proposal` incorporating valid feedback. Discard feedback
  that is incorrect (explain why in the rewrite).
- The rewrite should be a complete, self-contained proposal (not a diff).
- **Check forced convergence**: if `proposal_round >= max_proposal_rounds - 1`,
  do not loop back to 2a. Instead, proceed to 2c with this rewrite as the
  final proposal. Note in the converged proposal that the critic did not
  fully approve.
- Otherwise, loop back to 2a with the updated proposal.

**If neither verdict found** (agent error):
- Treat as `VERDICT: REVISE` with feedback "Agent did not produce a clear
  verdict — review the proposal structure and clarity."

### 2c: Post Converged Proposal

After convergence (or max rounds), post the final proposal as a GitHub issue
comment:

```bash
# Use --body-file (see the CRITICAL note in Step 2a).
tmpfile=$(mktemp)
{
  printf '## Converged Proposal (Round %s/%s)\n\n' "$proposal_round" "$max_proposal_rounds"
  cat data/pdr-$ISSUE/proposal_final.md
  printf '\n\n---\n\n*This proposal was refined through %s round(s) of adversarial review using the `plan-do-review` skill.*\n' "$proposal_round"
  printf '\n---\n\n*Skill: `/plan-do-review` | Harness: %s | Model: %s*\n' "$HARNESS" "$MODEL"
} > "$tmpfile"
gh issue comment $ISSUE --body-file "$tmpfile"
rm -f "$tmpfile"
```

**Move the issue to `In progress`** — proposal converged, execution begins:

```bash
bash .github/skills/plan-do-review/scripts/move-issue-status.sh "$ISSUE" "In progress"
```

---

## Step 3: Execute the Proposal

Implement the converged proposal in full. This is the core implementation
phase — you (the orchestrator) do the actual coding work.

**Apply the Guiding Principles above.** Implement the proposal as a
durable building block, not a short-term patch. If carrying out the
proposal reveals that a larger refactor — changing public API
signatures, restructuring types, introducing Arc/Cow/lifetimes,
redesigning enums, splitting modules, or touching several crates —
would produce a significantly more readable, sustainable, elegant, or
safe result, do the refactor rather than working around it. File any
intentionally-deferred refactors as follow-up issues so they aren't
lost; don't paper them over with inline workarounds.

### 3a: Create an isolated worktree for implementation

All code edits in Step 3 happen in a git worktree rooted on a dedicated
branch, NOT in the caller's main checkout. Rationale: this skill may be
invoked from a running `/monitor-loop`, a CI-driving script, or another
long-lived session that owns the main checkout. Dirtying that checkout
mid-implementation blocks its git operations (pull, status checks,
other deploys) until we commit and push. A worktree costs ~30 s to
create and makes the isolation guarantee explicit.

```bash
# Create a dedicated worktree and branch off origin/main
WORKTREE_BRANCH="plan-do-review/issue-$ISSUE"
WORKTREE_PATH=".claude/worktrees/plan-do-review-$ISSUE"

git fetch origin main
git worktree add -B "$WORKTREE_BRANCH" "$WORKTREE_PATH" origin/main

cd "$WORKTREE_PATH"
```

For the remainder of Step 3 (Plan, Implement, Verify, Commit), all
`cargo`, `git`, and editor operations happen inside `$WORKTREE_PATH`.
Any tool or subagent you invoke for implementation work must be pointed
at this directory (via `cwd`, the agent `Plan` tool's worktree option,
or an explicit `cd`).

If the worktree or branch already exists from a prior failed run
(`$ISSUE` has been worked before), inspect it first rather than
clobbering — `git worktree list` + `git log -3 "$WORKTREE_BRANCH"` — and
decide whether to resume (checkout + rebase onto current origin/main)
or discard (`git worktree remove --force` then recreate). Do not
silently overwrite uncommitted work.

### 3b: Plan the Implementation

Break the proposal into concrete implementation steps. Use SQL todos for
tracking:

```sql
INSERT INTO todos (id, title, description, status) VALUES
  ('step-1', '...', '...', 'pending'),
  ('step-2', '...', '...', 'pending');
INSERT INTO todo_deps (todo_id, depends_on) VALUES ('step-2', 'step-1');
```

### 3c: Implement

For each step (inside `$WORKTREE_PATH`):
1. Update the todo to `in_progress`
2. Make the code changes
3. Run `cargo check --all` after each logical change
4. Run focused tests: `cargo test -p <crate>`
5. Update the todo to `done`

### 3d: Verify

After all steps are complete (inside `$WORKTREE_PATH`):
1. `cargo test --all` — full test suite passes
2. `cargo clippy --all` — no warnings
3. `cargo fmt --all -- --check` — formatting clean

Fix any issues before proceeding.

### 3e: Commit on the worktree branch

```bash
# Still inside $WORKTREE_PATH
git add -A
git commit -m "<short imperative description>

<longer description of what was implemented>

Closes #$ISSUE

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

This commit lives on `$WORKTREE_BRANCH`, NOT on main. We land it on
main in the next sub-step.

### 3f: Land the change on main and clean up

The commit needs to reach `origin/main` for the caller (e.g. the
monitoring loop) to pick it up on its next redeploy. From the main
checkout (NOT the worktree), fast-forward or rebase main onto the
worktree branch and push:

```bash
# Return to the main checkout root — wherever the caller invoked us
cd "$MAIN_CHECKOUT"   # typically the repo root, not $WORKTREE_PATH

git fetch origin main
git checkout main
git pull --rebase

# Fast-forward main to the worktree branch.
# If this fails, rebase the worktree branch onto main and retry.
if ! git merge --ff-only "$WORKTREE_BRANCH"; then
  git -C "$WORKTREE_PATH" rebase main
  git merge --ff-only "$WORKTREE_BRANCH"
fi

git push
```

If the push is rejected (upstream moved between `pull --rebase` and
`push`), `git pull --rebase && git push` and retry once.

After the push succeeds, clean up the worktree and its branch:

```bash
git worktree remove "$WORKTREE_PATH"
git branch -d "$WORKTREE_BRANCH"
```

**Move the issue to `In review`** — code has landed on `main` and the
review-fix loop is about to start:

```bash
bash .github/skills/plan-do-review/scripts/move-issue-status.sh "$ISSUE" "In review"
```

If the push fails repeatedly (branch protection blocks direct pushes,
for example), open a PR from `$WORKTREE_BRANCH` with
`gh pr create --fill` and let the PR-level review gate handle the
landing. The worktree stays until the PR merges; do NOT `git worktree
remove` it in that case. **Do NOT move the issue to `In review` in this
branch** — the column should advance only when the change has actually
reached `main`. Leave the issue in `In progress` until the PR merges,
then move it manually or rerun the skill.

If any verification step (3c–3e) failed and you could not converge on a
green state, do NOT clean up — leave the worktree in place so the
caller or a follow-up invocation can inspect the partial work.

---

## Step 4: Review-Fix Loop

Repeat until `VERDICT: SOUND` or `review_round >= max_review_rounds`:

### 4a: Spawn Review-Fix Agent

Increment `review_round`.

Get the commit hash:
```bash
git log -1 --format='%H'
```

Launch a background agent using the Task tool:
- **agent_type**: `"general-purpose"`
- **model**: `$MODEL`
- **name**: `"review-fix-round-{review_round}"`
- **description**: `"Review-fix round {review_round}"`

The review-fix agent prompt must include the full review-fix skill protocol.
Read the review-fix skill template to assemble the prompt:

```bash
cat .github/skills/review-fix/SKILL.md
```

Substitute `$COMMIT` with the actual commit hash and set `$MODE = review`.

Prepend context about what was implemented:
```
You are reviewing commit {commit_hash} in the current repository.
This commit implements the proposal from GitHub issue #{issue_number}.

{brief summary of what was implemented}

Follow the review-fix skill instructions below exactly. Mode is review
(do NOT make changes). Produce the full structured report.

{contents of .github/skills/review-fix/SKILL.md}
```

### 4b: Process Review Result

Read the agent result. Extract the verdict from the Fix Analysis section.

> **Context hygiene (same as 2b).** After posting the full review to the
> issue, keep only the verdict and the specific issue list in your working
> state. The full report is preserved in the issue comment.

**Post the review result to the issue**:

```bash
# Use --body-file (see the CRITICAL note in Step 2a).
tmpfile=$(mktemp)
{
  printf '## 🔬 Review-Fix Report (Round %s/%s)\n\n' "$review_round" "$max_review_rounds"
  printf '<details>\n<summary>Full review report (click to expand)</summary>\n\n'
  cat data/pdr-$ISSUE/review_r$review_round.md
  printf '\n\n</details>\n\n'
  printf '**Verdict: %s**\n\n' "$verdict"
  # If not SOUND, append the key issues outside the <details> block.
  printf '---\n\n*Created by `/plan-do-review` skill → `review-fix-round-%s` sub-agent (%s, model: %s)*\n' "$review_round" "$HARNESS" "$MODEL"
} > "$tmpfile"
gh issue comment $ISSUE --body-file "$tmpfile"
rm -f "$tmpfile"
```

**If `SOUND`**:
- The implementation is clean. Proceed to Step 5.

**If `CONCERNS`, `INCOMPLETE`, or `WRONG`**:
- Extract every specific issue from the review report:
  - Missing per-op checks
  - Untested code paths
  - Similar issues found
  - Architectural gaps
  - Parity issues
  - Fundamental design issues
- **Address every single issue.** Do not skip, defer, or dismiss any feedback.
  Read the relevant code to understand each issue, then fix it. If a reviewer
  raised it, it gets fixed — period.
- For each issue:
  1. Read the relevant code to understand the problem
  2. Make the code changes to fully resolve it — **inside the same
     `$WORKTREE_PATH` worktree used in Step 3**. If the worktree was
     already cleaned up (3f completed), recreate it with
     `git worktree add -B "$WORKTREE_BRANCH" "$WORKTREE_PATH" main`
     and rebase onto main before editing.
  3. Add or update tests to cover the fix
  4. Run `cargo test --all` and `cargo clippy --all` inside the worktree
- Once all issues are resolved, commit on the worktree branch, land on
  main via the Step 3f "fast-forward + push" recipe (re-clean the
  worktree after landing), and loop back to 4a.
- If `WRONG`, consider whether a revert and re-implementation is cleaner than
  incremental fixes. Either way, all feedback must be addressed before the
  next review round.

**If verdict unclear** (agent error):
- Treat as `CONCERNS` and extract all actionable items from the agent output.
  Address every item — do not skip any.

---

## Step 5: Completion

Move the issue to `Done`, post a completion comment, and unassign yourself:

```bash
bash .github/skills/plan-do-review/scripts/move-issue-status.sh "$ISSUE" Done
gh issue edit $ISSUE --remove-assignee @me
```

```bash
# Use --body-file (see the CRITICAL note in Step 2a).
tmpfile=$(mktemp)
# Compose the completion comment by writing each section in turn. Any
# variable-length sections (commit list, What-was-done, etc.) should be
# either printed inline with printf or cat'd from a pre-written file —
# never embedded as `$(cat ...)` inside a quoted heredoc.
{
  printf '## Implementation Complete\n\n'
  printf 'Implemented in commit(s):\n%s\n\n' "$commit_list"  # or cat a prebuilt file
  printf '### Summary\n%s\n\n' "$brief_summary"
  printf '### Review Status\nPassed review-fix in %s round(s).\nFinal verdict: **SOUND**\n\n' "$review_round"
  printf '### What was done\n%s\n\n' "$what_was_done"

### What was deferred (if any)
{bullet list of follow-up items with issue links}

  printf -- '---\n\n*Implemented and reviewed using the `plan-do-review` skill.*\n'
  printf '\n---\n\n*Skill: `/plan-do-review` | Harness: %s | Model: %s*\n' "$HARNESS" "$MODEL"
} > "$tmpfile"
gh issue comment $ISSUE --body-file "$tmpfile"
rm -f "$tmpfile"
```

### 5b: File Issues for Deferred Work

If the completion summary includes deferred items (from the "What was deferred"
section, reviewer recommendations, or remaining concerns noted during
implementation), create a GitHub issue for **each** deferred item.

**Classify each deferred item** to determine its priority label:

| Category | Priority label | Examples |
|---|---|---|
| Correctness bugs | `urgent` | Wrong result, data corruption, logic error, missing validation that causes incorrect behavior |
| Security issues | `high` | Unauthenticated access, input injection, credential exposure, DoS vectors |
| Performance issues | `medium` | Unnecessary allocations, O(n²) where O(n) suffices, missing caching, redundant I/O |
| Everything else | `low` | Refactors, testing gaps, documentation, code cleanup, naming improvements |

Apply the highest applicable category — e.g., a performance issue that also
causes incorrect results is `urgent` (correctness), not `medium`.

```bash
gh issue create \
  --title "<short imperative title>" \
  --body "Follow-up from #$ISSUE (<original issue title>).

## Context
<why this was deferred — e.g., out of scope, needs design, blocked on X>

## What needs to happen
<concrete description of the work>

## Dependencies
<list any issues that must be completed before this one can start, or state "None">
- Blocked by #NNN — <short reason why this depends on that issue>

## References
- Parent issue: #$ISSUE
- Implementation commit(s): <commit hashes>" \
  --label "follow-up" \
  --label "<priority>"
```

where `<priority>` is one of `urgent`, `high`, `medium`, or `low` based on the
classification above.

Guidelines for deferred-work issues:
- One issue per distinct work item — do not bundle unrelated items.
- Title should be actionable and imperative (e.g., "Add integration tests for
  RPC semaphore rejection", not "Testing gaps").
- Include enough context that someone unfamiliar with the parent issue can
  understand and execute the work.
- Reference the parent issue and implementation commits.
- Add relevant labels beyond `follow-up` and the priority label (e.g.,
  `testing`, `refactor`, crate-specific labels).
- If a deferred item is trivial or speculative, skip it — only file issues for
  work that genuinely should be done.
- **State dependencies explicitly.** If a deferred issue depends on another
  issue (including other deferred issues being filed in the same batch), say so
  in the Dependencies section with "Blocked by #NNN" and a short reason. If
  multiple deferred items form a sequence (e.g., Phase 1 → Phase 2 → Phase 3),
  each later phase must list the earlier one as a blocker. An issue with no
  prerequisites should say "None". This is critical for the dependency check
  in this skill to work correctly.

Update the completion comment's "What was deferred" section to include the
newly created issue links (edit the comment or post a follow-up).

### 5c: Clean Up Per-Issue Build Artifacts

Each invocation of this skill accumulates a per-issue `CARGO_TARGET_DIR`
at `~/data/pdr-$ISSUE/` (25–50 GB per dir for a full henyey workspace
build), plus optionally `~/data/pdr-$ISSUE-target/` when the caller used
that alternate naming. Once the fix has landed on `main` and the issue
is closed, these caches are stale — the next relevant build comes from
the monitor-loop rebuilding `main`, not from this target. Leaving them
behind is the dominant disk-pressure driver on the shared `~/data/`
volume (observed 68 such dirs totalling ~500 GB on 2026-04-22).

Remove the per-issue build targets:

```bash
N="$ISSUE"
rm -rf "$HOME/data/pdr-$N" 2>/dev/null || true
rm -rf "$HOME/data/pdr-$N-target" 2>/dev/null || true
```

**Do not run this cleanup before Step 5.** The target dir is still
needed across Step 3 (implementation), Step 4 (review-fix re-compiles),
and any review-round rebuilds that happen if the worktree was recreated
in 4b. Only remove the target once the completion comment has been
posted and the skill is truly done.

**Do not clean up if verification failed and the worktree was left
in place** (see Step 3f's failure clause). In that case the caller or a
follow-up invocation needs the cached build to resume, so preserve the
target dir along with the worktree. Check: if `.claude/worktrees/plan-do-review-$ISSUE/`
still exists, skip the `rm -rf` above.

Print a summary to the terminal:

```
═══ Plan-Do-Review Complete ═══
Issue:              #$ISSUE
Proposal rounds:    {proposal_round} / {max_proposal_rounds}
Review rounds:      {review_round} / {max_review_rounds}
Commits:            {count}
Deferred issues:    {count} filed
Final verdict:      SOUND
═════════════════════════════════
```

---

## Guidelines

- **The orchestrator implements; agents review.** You write code and make
  changes. Sub-agents only analyze and critique. This separation ensures
  reviews are independent.
- **Post everything to the issue — no exceptions.** The GitHub issue is the
  complete audit trail. Skipping any comment is a skill violation. Every
  step below MUST appear as an issue comment:
  - Each proposal draft (before critic review)
  - **Each critic response (with verdict)** — this is the most commonly
    skipped step. Post it immediately after the critic agent returns,
    before processing the verdict or rewriting the proposal.
  - The converged proposal (delimiter between planning and execution)
  - Each review-fix report (with verdict)
  - The completion summary (with commits, deferred work, and issue links)
- **Respect max rounds.** If proposal convergence or review-fix hits the max,
  post whatever you have and note the remaining concerns. Do not loop forever.
- **Be honest about feedback.** If a critic's feedback is wrong, explain why
  in the rewrite. If it is right, fix it. Do not ignore valid feedback.
- **Address all review-fix feedback.** Every issue raised in a review-fix
  round must be fully resolved before the next round. No feedback may be
  skipped, deferred, or dismissed. If the reviewer raised it, fix it.
- **Use the codebase, not assumptions.** Before rewriting a proposal or
  implementing code, read the actual files. Do not rely on memory or the
  issue description alone.
- **One commit per review round.** Each review-fix iteration should produce
  one commit addressing all feedback from that round.
- **Parity is non-negotiable.** For any change touching protocol, consensus,
  or ledger logic, verify against stellar-core.
- **Big swings are expected.** Large refactors, public API modifications,
  struct redesigns, and design-level changes are not only permitted — they
  are preferred when the result is cleaner, more idiomatic Rust, and more
  scalable. Do not artificially constrain scope to minimal patches.
  Examples of encouraged changes:
  - Changing function signatures to accept `&T` or `Arc<T>` instead of
    requiring callers to clone
  - Restructuring types (adding Cow, newtype wrappers, splitting enums)
  - Moving fields between structs to improve ownership semantics
  - Cross-crate API changes when a pattern spans module boundaries
  - Replacing C-style output parameters with return values
  - Introducing trait bounds or generics to eliminate repetition
  The bar is: does the code end up clearer, cleaner, and more maintainable?
  If yes, the refactor is worth it regardless of diff size.
