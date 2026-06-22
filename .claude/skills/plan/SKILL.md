---
name: plan
description: |
  Draft an implementation plan for a triaged henyey issue, validated by three
  independent critics in parallel. Picks up issues in `ready-for-planning`,
  transitions them to `planning` while actively drafting, then to
  `ready-for-doing` on convergence. At the round-2 cap, instead of blocking,
  the plan **force-converges**: the round-2 plan ships as-is and each
  residual REVISE-MAJOR concern is filed as a follow-up issue for operator
  triage. Use when invoked by /project-tick with an issue in
  ready-for-planning, or manually as /plan <issue>.
argument-hint: <issue-number>
---

# /plan <issue> — adversarial plan drafting

You produce a single, converged implementation plan for one issue. The plan is what `/do` will execute, so it must be specific: file paths, function names, test approach, parity considerations.

You are not alone — three independent critic **lenses** (correctness / parity / scope) evaluate every draft. Under the pipeline's current single-level dispatch model (the orchestrator spawns one specialist per issue; that specialist cannot itself spawn nested sub-agents), **you run these three lenses yourself, as clearly-delimited in-agent passes** rather than as nested sub-agents — see the multi-lens discipline in Step 3. The plan converges when all three approve (or downgrade to `REVISE-MINOR`). Each REVISE-MAJOR concern is then subjected to an **adversarial refute pass** (an in-agent skeptic pass argues the concern is wrong or non-blocking) before it is allowed to cost a revision round — the distinct correctness/parity/scope lenses remain the primary defense against missed bugs, while the refute pass kills false-positive concerns.

**Hard cap: 2 rounds.** If round 2 still has REVISE-MAJOR verdicts, the plan **force-converges**: ship the round-2 plan as-is, file one follow-up issue per residual REVISE-MAJOR concern (so the disagreement is preserved as backlog rather than blocking the pipeline), and advance to `ready-for-doing`. The doer reads the converged plan and the follow-up list as "minor items to consider during implementation."

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

## Step 3 — Round 1: Run 3 critic lenses (in-agent multi-lens)

### Multi-lens discipline (how to run the three lenses yourself)

Under the current dispatch model you cannot spawn nested critic sub-agents, so **you run the three lenses (correctness / parity / scope) as three clearly-delimited passes within this agent**. The downstream contract is the **structured comments + verdict markers**, not the number of OS-level agents — so a disciplined in-agent multi-lens pass satisfies every parser as long as you hold to these four properties:

1. **Distinct framing per lens.** Before each pass, adopt ONLY that lens's brief (Critic A / B / C below is the per-pass framing). Do not carry forward the previous lens's conclusions into the next.
2. **No cross-lens peeking.** Form and **post each lens's verdict as its own separate comment** (a separate `gh pr comment` / issue-comment call) *before* starting the next lens. Never merge the three lenses into one block — the Step 4 / Step 6 parsers key on the `## 🔍 Critic X` header line and keep the latest comment per critic name, so each lens MUST be its own comment.
3. **Explicit reset between lenses.** When you move to the next lens, evaluate it as if you had not authored the prior one — re-read the plan fresh through the new lens, do not anchor on what you just concluded.
4. **Refute as a genuine adversary** (Step 4). The refute pass argues the *opposing* case as if it must prove a different reviewer wrong — see Step 4 for the stance-switch wording.

(If a future nested-spawn capability lands, these same three briefs can be dispatched as parallel sub-agents instead — the per-lens framing and comment shapes are unchanged. The workspace-contract / forbidden-scratch sections below still bind any shell-out a lens performs.)

### Critic workspace contract

**All critic scratch work must live only under `~/data` (the real home directory's `data/` subdirectory, derived from the passwd entry — immune to `$HOME` poisoning).** A lens should prefer the existing checkout for read-only inspection. If a lens needs a scratch checkout (e.g. to run a test or verify a claim), it must live under `~/data`, never in the repo root, the repo parent, or anywhere outside `~/data`. Derive the workspace by sourcing the shared contract helper:

```bash
REPO_ROOT="$(git rev-parse --show-toplevel)"
source "$REPO_ROOT/scripts/lib/agent-worktree-contract.sh"

# plan_critic_bootstrap validates and exports WORKTREE_BASE, CARGO_TARGET_DIR,
# and CRITIC_WORKTREE. Pre-seeded values are accepted only if they resolve under
# the real ~/data AND match the expected session/issue prefix
# ($HOME/data/$SESSION_ID/plan-$ISSUE/...). Hostile values (outside ~/data,
# using ../ traversal, HOME poisoning, or cross-session paths) are rejected
# with a non-zero exit.
plan_critic_bootstrap "$ISSUE" "critic-a" || exit 1  # or critic-b, critic-c
mkdir -p "$CRITIC_WORKTREE"
```

Use this bootstrap before any lens shell-out so you know where to place any checkout or build output. The bootstrap is self-seeding: if the runtime pre-sets `WORKTREE_BASE` or `CARGO_TARGET_DIR`, it respects those only if they resolve under the real `~/data` AND under the expected session/issue prefix (`~/data/$SESSION_ID/plan-$ISSUE/...`); otherwise the bootstrap rejects the value and exits non-zero — the `|| exit 1` guard ensures no subsequent commands run with stale env vars. Hostile overrides (paths outside `~/data`, traversal like `~/data/../escape`, HOME-poisoned paths, or cross-session shared directories) are rejected before any `mkdir`, `git clone`, or cargo command runs.

**Forbidden scratch patterns (issue #2843 — hard requirement).** Every lens shell-out is bound to `$CRITIC_WORKTREE` and must explicitly avoid the observed disk-leak patterns — not in the repo tree, not as a `<repo>-pr<N>` sibling, not under `/tmp`:

- `.review-data/`
- `.review-worktrees/`
- `.worktrees/`
- `.copilot-tmp/`
- `.opencode/worktrees/`
- any path under `/tmp`

These are the exact out-of-`~/data` scratch dirs that prior pipeline runs leaked and that repeatedly filled the root FS. A lens that needs a checkout uses `$CRITIC_WORKTREE` and nothing else.

Run three lens passes in sequence (correctness / parity / scope), each as its own delimited pass per the multi-lens discipline above. Three distinct lenses are the primary defense against missed bugs. For each pass, frame yourself with that lens's brief, evaluate against the issue number and the plan-draft comment, and **post that lens's verdict as its own separate comment before moving to the next lens**:

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
> **Visibility-narrowing / dead-code build-verify gate (REVISE-MAJOR):**
>
> For ANY plan that narrows visibility (`pub`→`pub(crate)`/`pub(super)`/private)
> or removes "dead" code: the plan MUST specify applying the narrowing/removal in
> a scratch worktree and verifying it with `cargo build -p <crate> --all-targets`
> (plus the workspace build) under `-Dwarnings`. A grep for callers is
> INSUFFICIENT — only a build determines reachability (this repo sets `-Dwarnings`
> globally in `.cargo/config.toml`, so an unused item is a hard error, and a
> `#[cfg(test)]`-only caller does NOT keep an item alive in a normal `cargo build`).
> The plan must classify each affected symbol:
>
> - **(a) in-crate non-test caller exists** → narrowing to `pub(crate)` is safe.
> - **(b) only `#[cfg(test)]` callers, or no callers** → the item is DEAD: delete
>   it, or keep it `pub` / `#[cfg(test)]` — never blind `pub(crate)` (that trips
>   `dead_code`). A crate-root `pub use` re-export can be the sole keep-alive. An
>   external integration-test-crate caller (`crates/<c>/tests/*.rs` is a SEPARATE
>   crate) blocks `pub(crate)` with E0624.
>
> Return **REVISE-MAJOR** if a plan asserts a symbol is "safe to narrow" / "dead"
> from a caller grep alone with no scratch build-verify step specified for `/do`,
> or if it blind-narrows a (b)-class symbol to `pub(crate)`.
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

## Step 4 — Adversarial refute pass (REVISE-MAJOR concerns only)

**Convergence rule:** all three verdicts are `APPROVE` or `REVISE-MINOR`.

If all three converged in round 1 (no `REVISE-MAJOR`) → skip to Step 6 (post Converged Plan).

If any critic returned `REVISE-MAJOR`, do **not** immediately revise. First subject each REVISE-MAJOR concern to an independent adversarial refute pass — this replaces the cross-model diversity the pipeline used to get from a second model, and directly attacks the false-positive concerns that otherwise cost a wasted round.

Enumerate every distinct `REVISE-MAJOR` concern across the three critic comments (one concern = one bullet from a critic's "Key concerns" / details block). For each such concern, run an independent **refute pass** in-agent — one delimited pass per concern. The refute pass does NOT inherit the critic's framing: switch stance and **argue the opposing case as if you must prove a different reviewer wrong**. Its job is to *refute* the concern. Apply the same multi-lens discipline (Step 3): distinct framing per refute, explicit reset from the critic pass that raised it, and **post each refute as its own separate comment**. Use this framing for each:

> A critic raised the following REVISE-MAJOR concern about the plan on issue
> #$ISSUE (plan-draft comment <comment-id>):
>
> <verbatim concern bullet + the critic lens it came from>
>
> Switch stance and REFUTE this concern as a genuine adversary — argue the
> opposing case as if you must prove a different reviewer wrong. Argue, with
> evidence, that it is one of:
> (a) **wrong** — the plan already handles this, or the critic misread the
> plan / the code; (b) **already-satisfied on current `origin/main`** — the
> behavior the critic wants already exists (read the relevant source to
> confirm); or (c) **non-blocking** — even if technically true, it does not
> invalidate the plan and is at most a REVISE-MINOR / follow-up item.
>
> You may read the issue, the plan draft, and any source files (read-only; if
> you need a scratch checkout use the workspace contract from Step 3 with a
> `skeptic-<n>` slot — never the repo tree, a sibling, or `/tmp`). Be honest:
> if you genuinely cannot refute it on any of the three grounds, say so
> explicitly. The concern only **stands** if it cannot be refuted.
>
> Post the refute as its own comment headed `## 🥊 Refute — <critic lens>: <short
> concern summary>` with a `**Outcome:** REFUTED | STANDS` line, followed by a
> 2–4 bullet justification (cite files/functions for ground (b)).

After every refute pass has posted, read the outcomes. Then:

- A concern is **dropped** if its refute pass returned `REFUTED`. Record it in the eventual plan comment under a "Refuted concerns" note as `refuted: <reason>` so the audit trail shows why a flagged concern did not drive a revision.
- A concern **stands** if its refute pass returned `STANDS` (or you could not produce an outcome after one retry — an un-refuted concern stands, fail-safe toward the critic).

**Decision after the refute pass:**

- If **no** REVISE-MAJOR concern survives refutation (all refuted) → treat the round as converged; skip to Step 6 (post Converged Plan, listing the refuted concerns).
- If **at least one** REVISE-MAJOR concern survives → go to Step 5 (round 2), addressing only the surviving concerns.

## Step 5 — Round 2: Revise

Reconcile the feedback into a revised plan. Address every `REVISE-MAJOR` concern **that survived the Step 4 refute pass** (refuted concerns are dropped — do not revise for them). You may also address `REVISE-MINOR` concerns at your discretion (note which you fixed, which you defer to `/do`).

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

Run the same three critic lenses again as in-agent passes (per the Step 3 multi-lens discipline), with the same briefs but with "Round 2" in the comment heading, posting each lens as its own separate comment. Then run the **same Step 4 adversarial refute pass** on any round-2 `REVISE-MAJOR` concern (one in-agent refute pass per concern); refuted concerns are dropped and only surviving concerns count below.

**Round 2 outcomes (after the refute pass):**

- All approve or REVISE-MINOR, or every round-2 REVISE-MAJOR was refuted → converge (Step 6); note the refuted concerns.
- Any `REVISE-MAJOR` **survives refutation** → **force-converge** (Step 5-bis below). The round-2 plan ships as-is; surviving REVISE-MAJOR concerns become follow-up issues for operator triage. The pipeline advances to `ready-for-doing` rather than stalling at `blocked`.

## Step 5-bis — Force-converge (round-2 REVISE-MAJOR)

Only entered when a round-2 `REVISE-MAJOR` concern **survived the refute pass**.

### 5-bis.1 File one follow-up issue per surviving REVISE-MAJOR concern

For each round-2 REVISE-MAJOR concern that survived refutation, file (do NOT file follow-ups for refuted concerns):

```bash
gh issue create --repo stellar-experimental/henyey \
  --title "<critic lens>: <short summary of concern, ≤80 chars>" \
  --body "$(cat <<EOF
Follow-up from issue #$ISSUE plan. Plan force-converged at the round-2 cap; this concern was raised by a critic but not resolved before the plan shipped to /do.

## Source

Critic: <Correctness | Parity | Scope>
Round: 2
Verdict: REVISE-MAJOR

Link: <round-2 critic comment URL>

## Concern detail

<full concern bullet body — quote the critic verbatim>

## Why this is a follow-up, not a plan block

The plan stage reached its hard cap of 2 rounds without consensus on this concern. Per /plan force-converge policy, the round-2 plan ships and unresolved concerns are preserved as backlog rather than stalling the pipeline indefinitely. Operator should triage this issue:
- close as won't-fix if the critic was over-strict or the concern is non-blocking,
- file a follow-up PR if the concern is real and needs addressing post-merge,
- or post \`## Review: Reset\` on the eventual PR and re-plan if the concern changes the implementation approach.
EOF
)" \
  --label "follow-up,force-converge,plan-residual"
```

If the issue has a `crate:<name>` label, propagate it.

Collect the new issue numbers.

### 5-bis.2 Ship the round-2 plan with force-converge marker

Post a single converged-plan comment matching Step 6's structure, but with a force-converge header instead of the clean `## ✅ Converged Plan`:

```markdown
## ⚠️ Plan: Force-Converged (Round 2 Cap)

**Summary:** <one sentence>

**Files to modify:** <as in normal converged plan>

**Approach:** <round-2 plan body, unchanged>

**Kind:** <bug-fix|feature|refactor|docs|test-only>

**Test plan:** <as in normal converged plan>

**Parity considerations:** <round-2 form>

**Residual disagreements (followed up as separate issues — `/do` MAY treat these as advisory):**
- <critic lens> `<concern class or short summary>`: #N1
- <critic lens> `<concern class or short summary>`: #N2

**Convergence:** Round 2, verdicts: A=<>, B=<>, C=<> *(force-converged on residual REVISE-MAJOR)*
```

Then advance to `ready-for-doing` (same as normal converge):

```bash
bash .github/skills/shared/scripts/move-issue-status.sh $ISSUE ready-for-doing
gh issue edit $ISSUE --repo stellar-experimental/henyey --remove-assignee @me
```

Exit.

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

**Refuted concerns (raised REVISE-MAJOR, dropped by the refute pass — omit if none):**
- <critic lens> `<short concern summary>` — refuted: <reason from the skeptic, e.g. "already satisfied on main: foo() already guards N==0">

**Sub-issues filed (if scope was narrowed):** #N1, #N2

**Convergence:** Round <1|2>, verdicts: A=APPROVE, B=APPROVE, C=APPROVE *(+ M REVISE-MAJOR concern(s) refuted, if any)*
```

Then transition:

```bash
bash .github/skills/shared/scripts/move-issue-status.sh $ISSUE ready-for-doing
gh issue edit $ISSUE --repo stellar-experimental/henyey --remove-assignee @me
```

## What you do NOT do

- **Do not** write or commit code. `/do` does that.
- **Do not** collapse the three lenses into one block — run them as three separate in-agent passes and post each as its own comment (the parsers key on the per-critic header). The per-concern refute passes are likewise each their own delimited pass with their own comment. (You do not run "rounds" of a single lens; each lens is evaluated once per plan round, with an explicit reset between lenses.)
- **Do not** revise for a REVISE-MAJOR concern before running the refute pass. Only concerns that survive refutation drive a round-2 revision; refuted concerns are dropped (recorded as `refuted: <reason>` in the plan comment).
- **Do not** exceed 2 rounds. If a round-2 REVISE-MAJOR concern survives refutation, force-converge (Step 5-bis) — file follow-ups for the surviving concerns and ship the round-2 plan to `ready-for-doing`. Do NOT spin a third round.
- **Do not** "argue back" with a critic by re-opening the same plan. If you genuinely disagree, post your reasoning in the revised plan and let the round-2 critic re-evaluate. If round 2 still has a surviving REVISE-MAJOR, force-converge with the disagreement preserved as follow-up issues — don't block the pipeline.
- **Do not** explore the codebase open-endedly. Each round, you read at most ~15 files of new context. Critics may read additional files independently.

## Failure handling

- **Critic lens failure:** if one of the three lens passes fails to produce its verdict comment (errored, interrupted), retry that lens pass once. If still failing, treat its verdict as REVISE-MAJOR and proceed to the refute pass / round 2; if round 2 also has a lens failure, `blocked` with that reason.
- **Refute pass failure:** if a refute pass fails to produce its outcome comment after one retry, the concern it was evaluating **stands** (fail-safe toward the critic) — it drives a revision as if un-refuted.
- **Triage Report missing:** route the issue back to `backlog` with a comment explaining; this is a `/triage` bug, not ours to fix.
- **GH API failure:** retry once after 5 seconds; if still failing, leave the issue assigned and exit non-zero.

## Examples (verdict patterns)

**Round 1 converges:**
- A: APPROVE, B: APPROVE, C: REVISE-MINOR ("consider testing the empty-input case")
- → Post Converged Plan noting the minor item; move to `ready-for-doing`.

**Round 1 REVISE-MAJOR refuted → converges:**
- Round 1: A: APPROVE, B: REVISE-MAJOR ("misses stellar-core's pre-protocol-26 fallback"), C: APPROVE.
- → Refute pass: the skeptic reads `stellar-core/` and the plan, finds protocol 24+ is the minimum supported (CLAUDE.md), so the pre-protocol-26 fallback is dead code and out of scope → `REFUTED`.
- → No surviving concern. Converge in round 1; note `refuted: parity — pre-p26 fallback is unreachable (protocol 24+ only)` in the plan comment.

**Round 2 converges:**
- Round 1: A: APPROVE, B: REVISE-MAJOR ("misses a real edge case at slot boundary"), C: APPROVE.
- → Refute pass: skeptic cannot refute (the edge case is genuinely unhandled) → `STANDS`.
- → Revise to handle the edge case. Round 2: A: APPROVE, B: APPROVE, C: REVISE-MINOR. → Converge.

**Round 2 force-converges:**
- Round 1: C: REVISE-MAJOR ("too broad — splits across 3 crates") → refute pass: `STANDS` (genuinely 3 crates).
- → File sub-issues, narrow plan. Round 2: A: REVISE-MAJOR ("narrowed plan no longer addresses original problem"), B: APPROVE, C: APPROVE → refute pass on A's concern: `STANDS`.
- → Force-converge (Step 5-bis): file follow-up issue capturing A's surviving "narrowed plan misses original problem" concern, post `## ⚠️ Plan: Force-Converged (Round 2 Cap)` with the round-2 plan body and a reference to the follow-up, advance to `ready-for-doing`. The operator triages the follow-up — close as won't-fix if the original problem is genuinely covered by the narrowed plan + sub-issues, or schedule a follow-up PR if A was right.

**Visibility-narrowing dead-code trap (Critic A REVISE-MAJOR):**
- Plan: "narrow `ScpDriver::foo`/`bar` from `pub` to `pub(crate)`; grep shows zero callers outside the crate."
- → Critic A: grep is insufficient under `-Dwarnings`. The symbols' only callers are `#[cfg(test)]` → they are DEAD, not "internal." `pub(crate)` would trip `dead_code` (this is exactly #3365 scp: 15 dead-code errors across 6 files). REVISE-MAJOR: the plan must specify a scratch build (`cargo build -p henyey-scp --all-targets` under `-Dwarnings`) and choose delete-or-keep-`pub`, not blind narrow.
