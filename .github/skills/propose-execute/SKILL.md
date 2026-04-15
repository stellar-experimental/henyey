---
name: propose-execute
description: Review a proposal issue with adversarial critics, converge on a plan, execute it, and iterate review-fix until clean
argument-hint: "<issue-number> [--model <model>] [--max-proposal-rounds N] [--max-review-rounds N] [--dry-run]"
---

Parse `$ARGUMENTS`:
- The first argument is a GitHub issue number. Replace `$ISSUE` with it.
- `--model <model>`: Model for critic and review agents (default: `"gpt-5.4"`).
- `--max-proposal-rounds N`: Max proposal↔critic iterations (default: 5).
- `--max-review-rounds N`: Max implement↔review-fix iterations (default: 3).
- `--dry-run`: Run through proposal convergence only; do not execute or commit.

# Propose-Execute

Adversarial proposal refinement → full implementation → iterative review-fix.

This skill automates the workflow: read a GitHub issue proposal, have an
independent agent critique it, rewrite incorporating feedback, repeat until
the proposal converges, then execute the plan, have it reviewed, fix issues,
and iterate until the review is clean.

The orchestrator (you) manages state, rewrites proposals, and implements code.
All reviews and critiques are delegated to independent sub-agents so that
review is adversarial and unbiased.

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
   Arc/Cow/lifetimes, redesigning enums — eliminate the root cause
   rather than patching symptoms? Prefer structural solutions that make
   the problem impossible over minimal fixes that address one instance.
8. **Rust idiom quality**: Does the proposal move the code toward
   idiomatic Rust? Ownership instead of cloning, references instead of
   copies, iterators instead of index loops, enums instead of booleans,
   newtypes for type safety?

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
gh issue comment $ISSUE --body "$(cat <<'PROPOSAL_EOF'
## Converged Proposal (Round {proposal_round}/{max_proposal_rounds})

{current_proposal}

---

*This proposal was refined through {proposal_round} round(s) of adversarial
review using the `propose-execute` skill.*
PROPOSAL_EOF
)"
```

If `--dry-run` is set, print the proposal to stdout instead of posting and
**stop here**.

---

## Step 3: Execute the Proposal

Implement the converged proposal in full. This is the core implementation
phase — you (the orchestrator) do the actual coding work.

**Prefer structural solutions over minimal patches.** If the proposal calls
for a large refactor — changing public API signatures, restructuring types,
introducing Arc/Cow/lifetimes, redesigning enums, or modifying cross-crate
interfaces — do it. The goal is clean, idiomatic, scalable Rust code, not
the smallest possible diff.

### 3a: Plan the Implementation

Break the proposal into concrete implementation steps. Use SQL todos for
tracking:

```sql
INSERT INTO todos (id, title, description, status) VALUES
  ('step-1', '...', '...', 'pending'),
  ('step-2', '...', '...', 'pending');
INSERT INTO todo_deps (todo_id, depends_on) VALUES ('step-2', 'step-1');
```

### 3b: Implement

For each step:
1. Update the todo to `in_progress`
2. Make the code changes
3. Run `cargo check --all` after each logical change
4. Run focused tests: `cargo test -p <crate>`
5. Update the todo to `done`

### 3c: Verify

After all steps are complete:
1. `cargo test --all` — full test suite passes
2. `cargo clippy --all` — no warnings
3. `cargo fmt --all -- --check` — formatting clean

Fix any issues before proceeding.

### 3d: Commit and Push

```bash
git add -A
git commit -m "<short imperative description>

<longer description of what was implemented>

Closes #$ISSUE

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"

git push
```

If push is rejected, `git pull --rebase && git push`.

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
You are reviewing commit {commit_hash} in /Users/tomer/dev/henyey.
This commit implements the proposal from GitHub issue #{issue_number}.

{brief summary of what was implemented}

Follow the review-fix skill instructions below exactly. Mode is review
(do NOT make changes). Produce the full structured report.

{contents of .claude/skills/review-fix/SKILL.md}
```

### 4b: Process Review Result

Read the agent result. Extract the verdict from the Fix Analysis section.

**If `SOUND`**:
- The implementation is clean. Proceed to Step 5.

**If `CONCERNS` or `INCOMPLETE`**:
- Extract the specific issues from the review report:
  - Missing per-op checks
  - Untested code paths
  - Similar issues found
  - Architectural gaps
  - Parity issues
- For each issue, determine if it is valid by reading the relevant code.
- Fix all valid issues:
  1. Make the code changes
  2. Add tests for untested paths
  3. Run `cargo test --all` and `cargo clippy --all`
  4. Commit with a descriptive message
  5. Push
- Loop back to 4a with the new commit.

**If `WRONG`**:
- The implementation has fundamental issues. Re-read the review carefully.
- Consider reverting and re-implementing from the proposal.
- If the review identifies a misunderstanding of the proposal, fix it.
- Commit, push, and loop back to 4a.

**If verdict unclear** (agent error):
- Treat as `CONCERNS` and manually review the agent output for actionable
  items.

---

## Step 5: Completion

Post a completion comment on the GitHub issue:

```bash
gh issue comment $ISSUE --body "$(cat <<'DONE_EOF'
## Implementation Complete

Implemented in commit(s):
{list of commit hashes with one-line descriptions}

### Summary
{brief description of what was implemented}

### Review Status
Passed review-fix in {review_round} round(s).
Final verdict: **SOUND**

### What was done
{bullet list of changes}

### What was deferred (if any)
{bullet list of follow-up items}

---

*Implemented and reviewed using the `propose-execute` skill.*
DONE_EOF
)"
```

Print a summary to the terminal:

```
═══ Propose-Execute Complete ═══
Issue:              #$ISSUE
Proposal rounds:    {proposal_round} / {max_proposal_rounds}
Review rounds:      {review_round} / {max_review_rounds}
Commits:            {count}
Final verdict:      SOUND
═════════════════════════════════
```

---

## Guidelines

- **The orchestrator implements; agents review.** You write code and make
  changes. Sub-agents only analyze and critique. This separation ensures
  reviews are independent.
- **Post everything to the issue.** The GitHub issue is the audit trail.
  Every proposal revision and review result should be visible there.
- **Respect max rounds.** If proposal convergence or review-fix hits the max,
  post whatever you have and note the remaining concerns. Do not loop forever.
- **Be honest about feedback.** If a critic's feedback is wrong, explain why
  in the rewrite. If it is right, fix it. Do not ignore valid feedback.
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
