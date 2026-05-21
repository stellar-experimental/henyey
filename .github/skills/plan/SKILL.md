---
name: plan
description: |
  Draft an implementation plan for a triaged henyey issue, validated by three
  independent critics in parallel. Picks up issues in `ready-for-planning`,
  transitions them to `planning` while actively drafting, then to
  `ready-for-doing` on convergence (or `blocked` if critics still disagree
  after two rounds). Use when invoked by /project-tick with an issue in
  ready-for-planning, or manually as /plan <issue>.
model: gpt-5.4
---

# /plan <issue> — adversarial plan drafting

You produce a single, converged implementation plan for one issue. The plan is what `/do` will execute, so it must be specific: file paths, function names, test approach, parity considerations.

You are not alone — three independent critics evaluate every draft in parallel. The plan converges when all three approve (or downgrade to `REVISE-MINOR`).

**Hard cap: 2 rounds.** If you can't converge in 2, the issue goes to `blocked` and a human decides.

## Inputs

- `$ISSUE` — issue number.
- The `## Triage Report` comment on the issue (verify it exists before starting).
- The codebase (read-only for the planner; critics may also read).
- The `stellar-core/` git submodule (for parity-critical issues).

## Step 1 — Verify the handoff

Read the `## Triage Report` comment. Confirm:

- Verdict is `ACCEPT` (not `BLOCKED`).
- It's not a trivial short-circuit (those skip `/plan` entirely — if you see `## Implementation Notes`, something is wrong; bounce back to triage with a comment).
- The type / severity / crate labels match what triage recorded.

If triage looks wrong (e.g. issue is actually a duplicate, or actually trivial), post a `## Plan: Triage Disagreement` comment explaining, move the issue back to `backlog`, and unassign yourself. Exit. The handoff-verification pattern catches bad triage at this point — cheaper than letting it through.

## Step 1.5 — Transition to `planning`

Immediately after acquiring the issue (assignee race already won by the orchestrator), move the issue from `ready-for-planning` to `planning`. This signals on the board that a plan is actively being drafted with critics — important for transparency since `/plan` takes 5–10 minutes of parallel-critic work.

```bash
bash .github/skills/shared/scripts/move-issue-status.sh $ISSUE planning
```

Skip this if the issue is already in `planning` (e.g. a previous `/plan` attempt crashed and the operator manually unblocked it).

## Step 2 — Round 1: Draft

Then explore the codebase to ground your plan:

- Read the relevant crate's source files: at least the module-level docs, the function you'd be changing, and the surrounding context.
- Read the most relevant existing tests in `crates/<crate>/tests/` to understand the testing patterns.
- If the issue is parity-critical (touches `crates/scp/`, `crates/herder/`, `crates/ledger/`, `crates/tx/`, `crates/overlay/`), read the matching stellar-core code via the `stellar-core/` submodule.

Then post the draft:

```markdown
## 📝 Plan Draft (Round 1)

**Summary:** <one sentence: what changes and why>

**Files to modify:**
- `crates/<crate>/src/<file>.rs` — <what changes there>
- `crates/<crate>/tests/<test>.rs` — <what test changes / additions>

**Approach:**
<2–4 paragraphs explaining the design. Include the key data structures or
control flow changes. Reference specific functions by name where appropriate.>

**Test plan** — three explicit lists (omit a list if empty):

- **Regression tests** (required for `kind: bug-fix`): each test must (a) name the file path + test function it will live in, (b) describe what it asserts, (c) state explicitly why it would FAIL on `origin/main` today and PASS after this plan lands. If the kind is `bug-fix` and this list is empty, the plan is incomplete.
- **New coverage** (required for `kind: feature`): every new public function or new behavioral branch the plan introduces gets at least one test. Name the file path + test function + what it asserts.
- **Existing tests preserved**: explicit list of relevant existing tests that must keep passing (especially when refactoring touches shared code). These should be in `cargo test -p henyey-<crate>` and verified pre-/post- by `/do`.

**Parity considerations:**
<If parity-critical: which stellar-core function/file matches, what semantics
must be preserved. If not parity-critical: write "n/a — non-parity path".>

**Risks:**
<Known unknowns, edge cases, things that might bite at review time. Be honest.>
```

## Step 3 — Round 1: Spawn 3 critics in parallel

### Critic workspace contract

**All critic scratch work must live only under `$HOME/data`.** Critics should prefer the existing checkout for read-only inspection. If a critic needs a scratch checkout (e.g. to run a test or verify a claim), it must live under `$HOME/data`, never in the repo root, the repo parent, or anywhere outside `$HOME/data`. Each critic derives its workspace by sourcing the shared contract helper:

```bash
REPO_ROOT="$(git rev-parse --show-toplevel)"
source "$REPO_ROOT/scripts/lib/agent-worktree-contract.sh"

# plan_critic_bootstrap validates and exports WORKTREE_BASE, CARGO_TARGET_DIR,
# and CRITIC_WORKTREE. Pre-seeded values are accepted only if they resolve under
# $HOME/data; hostile values (outside $HOME/data or using ../ traversal) are
# rejected with a non-zero exit.
plan_critic_bootstrap "$ISSUE" "critic-a"  # or critic-b, critic-c
mkdir -p "$CRITIC_WORKTREE"
```

Include this bootstrap verbatim in each critic's prompt so the sub-agent knows where to place any checkout or build output. The bootstrap is self-seeding: if the parent runtime pre-sets `WORKTREE_BASE` or `CARGO_TARGET_DIR`, the critic respects those only if they resolve under `$HOME/data`; otherwise it falls back to `$HOME/data/$SESSION_ID/plan-$ISSUE/...`. Hostile overrides (paths outside `$HOME/data` or traversal like `$HOME/data/../escape`) are rejected before any `mkdir`, `git clone`, or cargo command runs.

Launch three `general-purpose` agents in parallel — do not wait between them. **Each critic must be spawned with `--model gpt-5.4`** (or equivalent model parameter) explicitly — do not inherit from the parent. Cross-model diversity is the whole point of the critic step. Each gets the issue number, the plan-draft comment ID, and a focused brief:

### Critic A — Correctness

> Read the plan draft on issue #$ISSUE. Independently evaluate: does this plan
> correctly solve the stated problem? Are there logical errors in the approach?
> Are there missing edge cases the plan does not address?
>
> **Test-plan verification (REVISE-MAJOR if any of the following fails):**
>
> - If triage classified `kind: bug-fix`, the Test plan MUST list a regression
>   test with (a) file path, (b) what it asserts, (c) an explicit claim that
>   it fails on `origin/main` today. Verify the claim by reading the named
>   test file's vicinity — if the test would actually pass on main (e.g. the
>   bug isn't reachable from that test), call it out.
> - If triage classified `kind: feature`, every new public function the plan
>   introduces must have at least one test covering its public contract.
>   Cross-check the file-list against the test-list.
> - If a refactor is touching parity-critical code, the "Existing tests
>   preserved" list must name the integration tests in that crate that exercise
>   the touched behavior.
>
> You may read the issue body, the plan, and any source files the plan references.
> Post your verdict as a PR-style comment with this exact structure:
>
> ```markdown
> ## 🔍 Critic A (correctness) — Round 1
>
> **Verdict:** APPROVE | REVISE-MINOR | REVISE-MAJOR
>
> **Key concerns:** <2–4 bullets, one line each. Be specific — name functions,
> lines, conditions.>
>
> <details>
> <summary>Full review</summary>
>
> <Detailed reasoning, file references, alternate approaches considered. Keep
> under 400 lines.>
> </details>
> ```
>
> Use REVISE-MAJOR only if a fundamental correctness issue invalidates the plan.
> Use REVISE-MINOR for fixable concerns that don't block forward progress.
> Use APPROVE if the plan is sound (small wording or style nits go in the
> details block, not the verdict).

### Critic B — Parity

> Same setup as Critic A, but evaluate ONLY: does the plan match stellar-core's
> behavior on this code path? Consult the stellar-core/ submodule. Identify the
> matching stellar-core function(s). Compare semantics. Flag any divergence as
> REVISE-MAJOR. If the plan is for a non-parity path (e.g. tooling, docs), say
> so and APPROVE. Post as `## 🔍 Critic B (parity) — Round 1` with the same
> structure as Critic A.

### Critic C — Scope

> Same setup, but evaluate ONLY: is the scope right? "Too narrow" means the
> plan band-aids a symptom without addressing the root cause. "Too broad" means
> the plan does multiple unrelated things in one PR, or its implementation will
> exceed ~500 lines of net change. "Just right" means one atomic concern,
> implementable in a single reviewable PR.
>
> Post as `## 🔍 Critic C (scope) — Round 1` with the same structure. If too
> broad, name the specific concerns and which ones should be split out into
> follow-up issues. If too narrow, name the root cause that the plan should
> address.

Wait for all three critics to post. Read their verdicts.

## Step 4 — Decide: converge or revise

**Convergence rule:** all three verdicts are `APPROVE` or `REVISE-MINOR`.

If converged in round 1 → skip to Step 6 (post Converged Plan).

If any critic returned `REVISE-MAJOR` → go to Step 5 (round 2).

## Step 5 — Round 2: Revise

Reconcile the feedback into a revised plan. Address every `REVISE-MAJOR` concern. You may also address `REVISE-MINOR` concerns at your discretion (note which you fixed, which you defer to `/do`).

### Special handling for scope `REVISE-MAJOR`

#### If too broad

Identify the most atomic sub-piece that fits one PR. For the cut content, file follow-up sub-issues:

```bash
gh issue create --repo stellar-experimental/henyey \
  --title "<descriptive title for sub-piece>" \
  --body "<scope, context, link back to parent #$ISSUE>" \
  --label "<labels>"
```

Set the parent-issue field on each new sub-issue (via the project board's parent-issue field) pointing back to `$ISSUE`. Then write the revised plan covering only the narrowed scope.

#### If too narrow / band-aid

Expand the plan to address the root cause. If expansion would now make the plan **too broad**, you have a structural problem — the issue itself is too small. Post a `## Plan: Scope Mismatch` comment, move the issue to `blocked` with that reason, unassign, and exit.

### Post the revised plan

```markdown
## 📝 Plan Revised (Round 2)

**Changes from Round 1:**
- <bullet per substantive change, referencing the critic that prompted it>

**Summary:** <one sentence>

<...same structure as Round 1...>

**Followup sub-issues filed:** #N1, #N2 (if scope was narrowed)
```

Spawn the same three critics again in parallel with the same briefs, but with "Round 2" in the comment heading. Wait for verdicts.

**Round 2 outcomes:**

- All approve or REVISE-MINOR → converge (Step 6).
- Any `REVISE-MAJOR` → move to `blocked` with `## Plan: Did Not Converge` comment summarizing the residual disagreement. Unassign. Exit.

## Step 6 — Converged Plan

Post the final plan as a clean, scannable comment. This is the single document `/do` will read — make it complete and self-contained.

```markdown
## ✅ Converged Plan

**Summary:** <one sentence>

**Files to modify:**
- `crates/<crate>/src/<file>.rs` — <what changes>
- ...

**Approach:**
<final, agreed-upon approach. 2–4 paragraphs. This is what /do reads.>

**Kind:** <bug-fix|feature|refactor|docs|test-only>  *(from triage)*

**Test plan:**
- **Regression tests:** <list each; for bug-fix, this section is non-empty>
- **New coverage:** <list each>
- **Existing tests preserved:** <list each>

**Parity considerations:**
<final form, after critic feedback>

**Minor items to consider during implementation:**
- <any REVISE-MINOR points the doer should keep in mind>

**Sub-issues filed (if scope was narrowed):** #N1, #N2

**Convergence:** Round <1|2>, verdicts: A=APPROVE, B=APPROVE, C=APPROVE
```

Then transition:

```bash
bash .github/skills/shared/scripts/move-issue-status.sh $ISSUE ready-for-doing
gh issue edit $ISSUE --repo stellar-experimental/henyey --remove-assignee @me
```

## What you do NOT do

- **Do not** write or commit code. `/do` does that.
- **Do not** run sequential critic rounds — critics run in parallel.
- **Do not** exceed 2 rounds. If round 2 fails, that's `blocked`. Period.
- **Do not** "argue back" with a critic by re-opening the same plan. If you genuinely disagree, post your reasoning in the revised plan and let the round-2 critic re-evaluate. If round 2 still REVISE-MAJOR, accept the block.
- **Do not** explore the codebase open-endedly. Each round, you read at most ~15 files of new context. Critics may read additional files independently.

## Failure handling

- **Critic agent failure:** if one of the three critics fails to post (timed out, errored), retry that critic once. If still failing, treat its verdict as REVISE-MAJOR and proceed to round 2; if round 2 also has a critic failure, `blocked` with that reason.
- **Triage Report missing:** route the issue back to `backlog` with a comment explaining; this is a `/triage` bug, not ours to fix.
- **GH API failure:** retry once after 5 seconds; if still failing, leave the issue assigned and exit non-zero.

## Examples (verdict patterns)

**Round 1 converges:**
- A: APPROVE, B: APPROVE, C: REVISE-MINOR ("consider testing the empty-input case")
- → Post Converged Plan noting the minor item; move to `ready-for-doing`.

**Round 2 converges:**
- Round 1: A: APPROVE, B: REVISE-MAJOR ("misses stellar-core's pre-protocol-26 fallback"), C: APPROVE.
- → Revise to add the fallback handling. Round 2: A: APPROVE, B: APPROVE, C: REVISE-MINOR. → Converge.

**Round 2 fails:**
- Round 1: C: REVISE-MAJOR ("too broad — splits across 3 crates")
- → File sub-issues, narrow plan. Round 2: A: REVISE-MAJOR ("narrowed plan no longer addresses original problem")
- → `blocked` with "scope-mismatch" reason. Humans decide.
