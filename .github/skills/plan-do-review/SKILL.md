---
name: plan-do-review
description: Review a proposal issue with adversarial critics, converge on a plan, execute it, and iterate review-fix until clean
argument-hint: "<issue-number> [--model <model>] [--max-proposal-rounds N] [--max-review-rounds N]"
---

Parse `$ARGUMENTS`:
- The first argument is a GitHub issue number. Replace `$ISSUE` with it.
  Also set `$ORIGINAL_ISSUE` = `$ISSUE` (preserved for redirect comments if
  blocker-ancestor resolution changes the target).
- `--model <model>`: Model for critic and review agents (default: `"gpt-5.4"`).
- `--max-proposal-rounds N`: Max proposal↔critic iterations (default: 5).
- `--max-review-rounds N`: Max implement↔review-fix iterations (default: 3).

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

Run:
```bash
gh issue view $ISSUE --json title,body,labels,state,comments,number
```

Extract:
- **Title**: the issue title
- **Body**: the full proposal / description
- **Comments**: any existing discussion (prior reviews, context)
- **State**: must be open (if closed, stop and report)

### Blocker-Ancestor Resolution

Before triaging readiness, check whether the issue is **blocked by** another
open issue. If it is, walk up the dependency chain and switch to the first
unblocked ancestor so that useful work gets done instead of bailing with
`not-ready`.

**Procedure** (start with `visited = {}`, `depth = 0`):

1. Read the current issue's body and comments. Using your understanding of the
   text, identify any issue numbers that this issue is **blocked by** — look
   for patterns like "blocked by #N", "depends on #N", "requires #N first",
   tasklist items `- [ ] #N`, or similar contextual references that indicate
   a prerequisite relationship. **Only** extract issues that are genuine
   blockers; ignore issues that are merely referenced or related.

2. For each candidate blocker, fetch it:
   ```bash
   gh issue view <N> --json number,state,title,body,comments
   ```
   Filter to only **open** issues. If no open blockers remain, the current
   issue is not actually blocked — continue with it as the target.

3. If open blocker(s) exist, pick the **first** one (by order of mention in
   the body/comments). Add the current `$ISSUE` to `visited`, set
   `$ISSUE = <blocker number>`, increment `depth`, and go back to step 1.

4. **Cycle detection**: if the blocker is already in `visited`, skip it and
   try the next blocker. If all blockers are in `visited`, the current issue
   is in a dependency cycle — fall through to Readiness Triage as-is.

5. **Depth cap**: if `depth >= 10`, stop walking and fall through to
   Readiness Triage for the current issue.

**When the target changes** (i.e., `$ISSUE != $ORIGINAL_ISSUE` after
resolution):

1. Post a redirect comment on the **original** issue:
   ```bash
   gh issue comment $ORIGINAL_ISSUE --body "⏩ This issue is blocked by #$ISSUE. Working on #$ISSUE first."
   ```
2. Assign yourself to the new target issue:
   ```bash
   gh issue edit $ISSUE --add-assignee @me
   ```
3. Unassign yourself from the original issue (the loop script or caller
   assigned you):
   ```bash
   gh issue edit $ORIGINAL_ISSUE --remove-assignee @me
   ```

Now continue with `$ISSUE` (which may have changed) into Readiness Triage.

---

### Readiness Triage

Before proceeding, assess whether the issue is actionable. An issue is **not
ready** if any of these are true:

- The body is empty or contains only a vague one-liner with no concrete proposal
- It requires information or decisions that are not yet available
- It describes a problem but proposes no approach and the correct approach is
  unclear even after reading the referenced code

If the issue is **not ready**:

1. Add the `not-ready` label:
   ```bash
   gh issue edit $ISSUE --add-label "not-ready"
   ```
2. Post a comment explaining why the issue is not ready and what is needed:
   ```bash
   gh issue comment $ISSUE --body "Marking as not-ready: {reason}. This issue needs {what's missing} before it can be picked up."
   ```
3. Unassign yourself:
   ```bash
   gh issue edit $ISSUE --remove-assignee @me
   ```
4. Stop. Do not proceed to Step 2.

If the issue **is ready**, remove the `not-ready` label if present (it may have
been added previously and the issue has since been updated):
```bash
gh issue edit $ISSUE --remove-label "not-ready" 2>/dev/null || true
```

Read any files, crates, or stellar-core references mentioned in the issue body
to build context. You need enough understanding to critique and rewrite the
proposal intelligently.

Initialize tracking:
```
proposal_round = 0
review_round = 0
current_proposal = <issue body + any relevant context>
```

---

## Step 2: Proposal Convergence Loop

Repeat until `VERDICT: APPROVED` or `proposal_round >= max_proposal_rounds`:

### 2a: Spawn Critic Agent

Increment `proposal_round`.

**Post the proposal draft to the issue.**

> **CRITICAL — read before posting.** The pattern `gh issue comment ... --body "$(cat <<'EOF' ... EOF)"` is a template. The `{placeholder}` tokens must be replaced with **literal text** before the bash command runs. Do NOT substitute a placeholder with a shell expression like `$(cat /tmp/foo.md)` — the heredoc is single-quoted, so no shell expansion happens, and the literal string `$(cat /tmp/foo.md)` ends up in the GitHub comment body. To avoid this class of bug, use `--body-file` to point `gh` at a file on disk, which bypasses shell interpolation entirely. Write the full comment body (header + content + footer) to a temporary file, then post it as one unit.

```bash
# Preferred pattern — write to a file, then post via --body-file.
tmpfile=$(mktemp)
{
  printf '## 📝 Proposal Draft (Round %s/%s)\n\n' "$proposal_round" "$max_proposal_rounds"
  cat /tmp/pdr-$ISSUE/proposal_r$proposal_round.md
  printf '\n\n---\n\n*Submitting to adversarial critic for review…*\n'
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
3. **Feasibility**: Can this be implemented as described? Are there
   practical obstacles?
4. **Risk**: What could go wrong? What edge cases are not addressed?
5. **Scope**: Is the scope appropriate? Too broad? Too narrow?
6. **Stellar-core parity**: Will the proposed changes maintain or improve
   parity with stellar-core?
7. **Structural ambition**: Does the proposal go far enough? Could a
   bigger refactor — changing public APIs, restructuring types, using
   Arc/Cow/lifetimes, redesigning enums, splitting or merging modules —
   eliminate the *class* of bug rather than patching the one symptom?
   Prefer structural solutions that make incorrect states
   unrepresentable over minimal fixes that address one instance.
8. **Readability & sustainability**: Will the resulting code be easy to
   read, modify, and extend in six months? Does it push invariants into
   types (newtypes, enums, constructors) or shared helpers so future
   callers can't silently get it wrong? Does it use idiomatic Rust —
   ownership over cloning, iterators over index loops, `?` over match
   chains, `Result`/`Option` over sentinel values? Does it name things
   well and keep functions focused?
9. **Long-term vs. short-term tradeoff**: Is the proposed fix a durable
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

```bash
# Use --body-file (see the CRITICAL note in Step 2a).
tmpfile=$(mktemp)
{
  printf '## 🔍 Critic Response (Round %s/%s)\n\n' "$proposal_round" "$max_proposal_rounds"
  printf '<details>\n<summary>Full critique (click to expand)</summary>\n\n'
  cat /tmp/pdr-$ISSUE/critic_r$proposal_round.md
  printf '\n\n</details>\n\n'
  printf '**Verdict: %s**\n\n' "$verdict"
  # If REVISE, append the numbered feedback items outside the <details> block
  # here (either inline printf lines or cat a second file).
} > "$tmpfile"
gh issue comment $ISSUE --body-file "$tmpfile"
rm -f "$tmpfile"
```

**If `VERDICT: APPROVED`**:
- The proposal has converged. Proceed to Step 3.

**If `VERDICT: REVISE`**:
- Extract the numbered feedback items.
- Investigate each feedback item — read the relevant code, verify the
  critic's claims, determine which feedback is valid.
- Rewrite `current_proposal` incorporating valid feedback. Discard feedback
  that is incorrect (explain why in the rewrite).
- The rewrite should be a complete, self-contained proposal (not a diff).
- Loop back to 2a with the updated proposal.

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
  cat /tmp/pdr-$ISSUE/proposal_final.md
  printf '\n\n---\n\n*This proposal was refined through %s round(s) of adversarial review using the `plan-do-review` skill.*\n' "$proposal_round"
} > "$tmpfile"
gh issue comment $ISSUE --body-file "$tmpfile"
rm -f "$tmpfile"
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

If the push fails repeatedly (branch protection blocks direct pushes,
for example), open a PR from `$WORKTREE_BRANCH` with
`gh pr create --fill` and let the PR-level review gate handle the
landing. The worktree stays until the PR merges; do NOT `git worktree
remove` it in that case.

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
cat .claude/skills/review-fix/SKILL.md
```

Substitute `$COMMIT` with the actual commit hash and set `$MODE = review`.

Prepend context about what was implemented:
```
You are reviewing commit {commit_hash} in the current repository.
This commit implements the proposal from GitHub issue #{issue_number}.

{brief summary of what was implemented}

Follow the review-fix skill instructions below exactly. Mode is review
(do NOT make changes). Produce the full structured report.

{contents of .claude/skills/review-fix/SKILL.md}
```

### 4b: Process Review Result

Read the agent result. Extract the verdict from the Fix Analysis section.

**Post the review result to the issue**:

```bash
# Use --body-file (see the CRITICAL note in Step 2a).
tmpfile=$(mktemp)
{
  printf '## 🔬 Review-Fix Report (Round %s/%s)\n\n' "$review_round" "$max_review_rounds"
  printf '<details>\n<summary>Full review report (click to expand)</summary>\n\n'
  cat /tmp/pdr-$ISSUE/review_r$review_round.md
  printf '\n\n</details>\n\n'
  printf '**Verdict: %s**\n\n' "$verdict"
  # If not SOUND, append the key issues outside the <details> block.
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

Post a completion comment on the GitHub issue and unassign yourself:

```bash
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
} > "$tmpfile"
gh issue comment $ISSUE --body-file "$tmpfile"
rm -f "$tmpfile"
```

### 5b: File Issues for Deferred Work

If the completion summary includes deferred items (from the "What was deferred"
section, reviewer recommendations, or remaining concerns noted during
implementation), create a GitHub issue for **each** deferred item:

```bash
gh issue create \
  --title "<short imperative title>" \
  --body "Follow-up from #$ISSUE (<original issue title>).

## Context
<why this was deferred — e.g., out of scope, needs design, blocked on X>

## What needs to happen
<concrete description of the work>

## References
- Parent issue: #$ISSUE
- Implementation commit(s): <commit hashes>" \
  --label "follow-up"
```

Guidelines for deferred-work issues:
- One issue per distinct work item — do not bundle unrelated items.
- Title should be actionable and imperative (e.g., "Add integration tests for
  RPC semaphore rejection", not "Testing gaps").
- Include enough context that someone unfamiliar with the parent issue can
  understand and execute the work.
- Reference the parent issue and implementation commits.
- Add relevant labels beyond `follow-up` (e.g., `testing`, `refactor`,
  crate-specific labels).
- If a deferred item is trivial or speculative, skip it — only file issues for
  work that genuinely should be done.

Update the completion comment's "What was deferred" section to include the
newly created issue links (edit the comment or post a follow-up).

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
