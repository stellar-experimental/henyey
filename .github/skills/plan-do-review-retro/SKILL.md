---
name: plan-do-review-retro
description: Evaluate a completed plan-do-review issue and propose reusable process improvements
argument-hint: "<issue-number-or-url> [--model <model>] [--apply] [--comment]"
---

Parse `$ARGUMENTS`:
- The first positional argument is required. It may be a GitHub issue number
  or an issue URL. Normalize URLs to their issue number.
- `--model <model>`: Model for the independent process-review agent
  (default: `"gpt-5.4"`).
- `--apply`: Apply vetted, reusable process improvements to relevant skill
  files. Default is report-only.
- `--comment`: Post the retrospective report to the analyzed GitHub issue.
  Default is terminal output only.

If no issue argument was provided, stop with:
"Usage: /plan-do-review-retro <issue-number-or-url> [--model <model>] [--apply] [--comment]".

# Plan-Do-Review Retrospective

Analyze a GitHub issue completed by `/plan-do-review`, determine why the run
needed the observed number of proposal and review-fix iterations, and identify
general process improvements that could reduce avoidable iterations in future
runs.

This skill evaluates the process, not just the code change. It should preserve
useful adversarial review while removing repeated, avoidable workflow failures.

Default mode is diagnostic only: produce a report and do not mutate GitHub or
repository files. Only `--apply` may edit skill files. Only `--comment` may post
to GitHub.

---

## Guiding Principles

- **Evidence first.** Every claim must cite issue comments, round numbers,
  commit hashes, or skill-file text.
- **Do not optimize away useful review.** Some iterations are desirable because
  an independent critic found real gaps. The target is avoidable iteration:
  missing standard checks, unclear prompts, incomplete implementation of an
  already accepted proposal, skipped audit-log steps, or repeated ambiguity.
- **Generalize or reject.** A recommendation must describe a reusable failure
  mode. Do not encode the specific bug, crate, file, issue, reviewer wording, or
  one-off implementation detail from the analyzed issue.
- **Prefer protocol improvements over memory.** Improve checklists, output
  schema, required evidence, side-effect gates, or prompt wording. Do not add
  "remember issue #N" examples.
- **Respect side-effect flags.** Without `--apply`, do not edit files. Without
  `--comment`, do not post comments. Never commit or push unless the caller
  explicitly asks after this skill completes.
- **Dependencies are in scope.** `/plan-do-review` depends on
  `.claude/skills/review-fix/SKILL.md`; process recommendations and `--apply`
  edits may target that file when the evidence points there.
- **Mirrors are informational.** `.opencode/skills/plan-do-review-oc/SKILL.md`
  and `.opencode/skills/review-fix-oc/SKILL.md` may be mentioned if drift is
  relevant, but do not edit them automatically in this skill.

---

## Step 1: Normalize Arguments and Fetch Issue Data

Normalize the issue argument:

```bash
RAW_ISSUE="<first positional argument>"

if [[ "$RAW_ISSUE" =~ /issues/([0-9]+) ]]; then
  ISSUE="${BASH_REMATCH[1]}"
else
  ISSUE="$RAW_ISSUE"
fi

if ! [[ "$ISSUE" =~ ^[0-9]+$ ]]; then
  echo "Invalid issue argument: $RAW_ISSUE"
  exit 1
fi
```

Create a temporary working directory for report fragments:

```bash
WORKDIR=$(mktemp -d -t "pdr-retro-$ISSUE.XXXXXX")
```

Fetch the issue:

```bash
gh issue view "$ISSUE" \
  --json number,title,body,state,stateReason,labels,assignees,author,comments,createdAt,updatedAt,closed,closedAt,url \
  > "$WORKDIR/issue.json"
```

Extract:
- Issue number, title, state, state reason, URL
- Labels and assignees
- Body
- Full comments with author, timestamp, URL if present, and body

Do not require the issue to be closed. `/plan-do-review` normally lands a commit
with `Closes #ISSUE`, but a retrospective can still analyze a completed audit
trail if the issue remains open because closing failed.

---

## Step 2: Validate That This Was a Plan-Do-Review Run

Look for completion evidence in the issue comments. Strong evidence includes:

- `## Implementation Complete`
- `*Implemented and reviewed using the \`plan-do-review\` skill.*`
- `## Converged Proposal (Round N/M)`
- `*This proposal was refined through N round(s) of adversarial review using the \`plan-do-review\` skill.*`
- at least one `## 📝 Proposal Draft (Round N/M)` comment
- at least one `## 🔍 Critic Response (Round N/M)` comment
- at least one `## 🔬 Review-Fix Report (Round N/M)` comment

Classify evidence:

| Evidence level | Requirements | Action |
|---|---|---|
| Complete | Completion marker plus proposal and review-fix comments | Continue normally |
| Partial | Proposal/critic comments exist, but completion or review-fix comments are missing | Continue only if the report clearly marks missing audit artifacts |
| Insufficient | No plan-do-review markers or round comments | Stop with `VERDICT: INSUFFICIENT_EVIDENCE` |

If blocker-ancestor resolution redirected the run, identify both:
- Original issue: the issue passed to this skill
- Worked issue: the issue that contains the actual plan-do-review audit trail

If the current issue contains a redirect comment like "Working on #N first" and
does not contain the audit trail, fetch issue `#N` and analyze that issue. The
final report must still mention the original issue.

---

## Step 3: Reconstruct the Audit Trail

Parse comments by stable headings. Match on the text label, not solely on emoji,
so older or manually edited comments remain parseable.

Required heading patterns:

| Artifact | Heading pattern |
|---|---|
| Proposal draft | `## ... Proposal Draft (Round N/M)` |
| Critic response | `## ... Critic Response (Round N/M)` |
| Converged proposal | `## Converged Proposal (Round N/M)` |
| Review-fix report | `## ... Review-Fix Report (Round N/M)` |
| Completion | `## Implementation Complete` |

For each proposal round, extract:
- Round number and max rounds
- Proposal body
- Comment author and timestamp
- Whether the proposal explicitly says it incorporated previous feedback

For each critic response, extract:
- Round number and max rounds
- Verdict: `APPROVED`, `REVISE`, or unclear
- Numbered feedback items, if any
- Whether the full critique was posted inside `<details>`

For the converged proposal, extract:
- Final round number and max rounds
- Whether it says forced convergence occurred
- Any unresolved critic concerns acknowledged in the final proposal

For each review-fix report, extract:
- Round number and max rounds
- Verdict from the report: `SOUND`, `CONCERNS`, `INCOMPLETE`, `WRONG`, or unclear
- Specific findings or recommendations
- Whether the report was posted inside `<details>`

For the completion comment, extract:
- Commit hashes or commit list
- Summary and "What was done"
- Review status and final review round count
- Deferred work and follow-up issue links

If commit hashes are missing from the completion comment, search locally:

```bash
git --no-pager log --all --grep="Closes #$ISSUE" --format='%H %s' --max-count=20
git --no-pager log --all --grep="#$ISSUE" --format='%H %s' --max-count=20
```

Read commit summaries only as needed to understand process causes:

```bash
git --no-pager show --stat <commit>
git --no-pager show --name-only --format=fuller <commit>
```

Do not perform a full correctness review of the code change unless a
review-fix finding or process recommendation depends on it.

---

## Step 4: Compute Run Metrics

Build a concise metrics table:

| Metric | Value |
|---|---|
| Original issue | `#N` |
| Worked issue | `#N` |
| Issue title | `<title>` |
| Completion evidence | Complete / Partial / Insufficient |
| Proposal rounds used | `used / max` |
| Critic verdict sequence | e.g. `REVISE -> APPROVED` |
| Forced convergence | Yes / No |
| Review-fix rounds used | `used / max` |
| Review verdict sequence | e.g. `CONCERNS -> SOUND` |
| Implementation commits | count and hashes |
| Deferred follow-ups | count and issue links |
| Missing audit artifacts | list or `None` |

Also compute:
- Number of critic feedback items per round
- Number of review-fix findings per round
- Whether any round repeated a previously raised issue
- Whether any issue was introduced by unclear skill instructions versus normal
  implementation uncertainty

---

## Step 5: Classify Iteration Causes

For each critic `REVISE` item and each non-`SOUND` review-fix finding, classify
the cause. Use one primary category and optional secondary categories.

### Proposal-stage cause categories

| Category | Use when |
|---|---|
| `missing-codebase-evidence` | The initial proposal made claims without reading or citing the relevant code |
| `missing-parity-evidence` | Protocol, consensus, ledger, or determinism behavior lacked stellar-core comparison |
| `scope-ambiguity` | The issue or proposal left key boundaries, defaults, or sequencing unclear |
| `missed-affected-path` | The proposal omitted a caller, crate, config path, test surface, or state transition |
| `weak-implementation-shape` | The proposal identified the goal but not a safe implementation structure |
| `under-scoped-design` | The proposal patched a symptom instead of the reusable abstraction or invariant |
| `over-scoped-design` | The proposal added unnecessary breadth likely to slow convergence |
| `critic-false-positive` | The critic was wrong after targeted verification |
| `audit-log-process-gap` | A required comment, verdict, body-file usage, or resume artifact was missing or malformed |
| `context-management-gap` | The run re-read too much, retained too much prior text, or lost essential state |

### Review-fix-stage cause categories

| Category | Use when |
|---|---|
| `proposal-not-fully-implemented` | The code did not implement an accepted proposal requirement |
| `test-coverage-gap` | Missing or weak regression/focused tests caused another round |
| `missed-code-path` | A relevant caller, branch, error path, or state transition was not changed |
| `similar-pattern-gap` | The implementation fixed one instance but missed the same category elsewhere |
| `parity-or-determinism-gap` | The implementation lacked or violated stellar-core parity/determinism expectations |
| `verification-gap` | The implementer skipped or chose the wrong validation command for the affected area |
| `dependency-skill-gap` | `review-fix` wording, output schema, or search strategy caused avoidable noise or ambiguity |
| `reviewer-noise` | The review finding was style-only, duplicate, false, or not behaviorally relevant |
| `process-side-effect-gap` | Worktree, commit, push, cleanup, or GitHub-comment handling caused avoidable iteration |

For each classified item, record:
- Source: comment heading, round number, and quote or concise paraphrase
- Category
- Was it avoidable by process? `Yes`, `No`, or `Unclear`
- Candidate process improvement, if any
- Affected skill file: `plan-do-review`, `review-fix`, both, or none
- Confidence: `High`, `Medium`, or `Low`

Treat an iteration as valuable, not avoidable, when it surfaced a real issue
that a reasonable general checklist would not have caught without making every
future run slower or noisier.

---

## Step 6: Generate Candidate Process Improvements

For every avoidable or recurring cause, propose at most one process improvement.
Prefer small changes such as:

- Add a required evidence field to a proposal, critic response, review report,
  or completion summary.
- Clarify when parity checks are mandatory.
- Add a checklist item for implementing every accepted proposal requirement.
- Require a brief "similar paths searched" note before review-fix.
- Tighten side-effect safeguards for comments, worktrees, commits, or cleanup.
- Clarify verdict extraction or missing-verdict handling.
- Add a context hygiene rule only if context loss contributed to iteration.

Do not propose:
- "Always check file X" or "always handle crate Y this way" unless it is framed
  as an existing general category like protocol/parity-critical code.
- Duplicating instructions that already exist, unless the evidence shows the
  existing instruction is ambiguous, too late in the workflow, or not tied to an
  output requirement.
- Large new process gates for a one-off problem.
- Changes designed to make reviewers less adversarial or less likely to report
  real concerns.

Each candidate must include:

```markdown
### Candidate: <short imperative process change>
- **Evidence**: <issue comment/round/commit evidence>
- **Avoidable iteration**: <which round(s) this could have reduced and why>
- **Affected skill**: plan-do-review / review-fix / both
- **Proposed edit**: <section and concise change>
- **Why this is not overfit**: <general failure mode that can recur>
- **Risk**: <possible downside or added overhead>
- **Confidence**: High / Medium / Low
```

---

## Step 7: Apply the Generalization Filter

Before recommending or applying any candidate, enforce this filter:

1. **Evidence test**: Is the candidate tied to a concrete issue artifact?
2. **Recurrence test**: Could this failure mode plausibly recur on unrelated
   issues?
3. **Process test**: Does the fix belong in skill protocol, prompt wording,
   output schema, or side-effect safeguards?
4. **Specificity test**: Does it avoid issue-, file-, crate-, and bug-specific
   instructions?
5. **Duplication test**: Is it not already clearly required by the skill? If it
   is already required, does the candidate improve enforcement or placement?
6. **Cost test**: Is the added overhead proportionate to the avoided iteration?
7. **Review-quality test**: Does it preserve useful adversarial review?

Classify each candidate:

| Decision | Meaning |
|---|---|
| `ACCEPT` | Recommend it, and apply it if `--apply` was set |
| `REWRITE` | Keep the idea but make it more general or less costly |
| `REJECT_OVERFIT` | Too specific to this issue |
| `REJECT_DUPLICATE` | Already covered clearly enough |
| `REJECT_LOW_VALUE` | Overhead exceeds likely benefit |
| `REJECT_HARMFUL` | Would reduce review quality or hide real issues |

Only `ACCEPT` candidates may become skill-file edits.

---

## Step 8: Run an Independent Process Review

Before finalizing recommendations, ask an independent agent to challenge the
retrospective. Launch a background Task agent:

- **agent_type**: `"general-purpose"`
- **model**: `$MODEL`
- **name**: `"pdr-retro-process-review"`
- **description**: `"Review PDR retrospective recommendations"`

Prompt:

```text
You are an independent process reviewer for the henyey project's
plan-do-review workflow.

Your job is to reject weak, overfit, redundant, harmful, or low-value process
recommendations. Do not review the underlying code fix except where necessary
to assess the process recommendation.

Inputs:

## Issue summary and reconstructed run
{metrics and timeline}

## Iteration-cause analysis
{classified proposal and review-fix findings}

## Candidate process improvements
{candidate list}

## Current relevant skill excerpts
{targeted excerpts from .github/skills/plan-do-review/SKILL.md and
.claude/skills/review-fix/SKILL.md}

Evaluate each candidate:
1. Is it supported by evidence from the issue?
2. Is it general enough for future plan-do-review runs?
3. Is it already covered by the current skills?
4. Would it reduce avoidable iteration without suppressing valuable adversarial review?
5. Is the proposed skill-file location appropriate?

Output a table:

| Candidate | Verdict | Reason | Safer rewrite if needed |
|---|---|---|---|

Verdict must be KEEP, MODIFY, or REJECT.

End with exactly one line:
VERDICT: ACCEPT_RECOMMENDATIONS
or
VERDICT: REVISE_RECOMMENDATIONS
```

Process the result:
- Keep `KEEP` recommendations.
- Rewrite `MODIFY` recommendations if the safer rewrite still passes the
  generalization filter.
- Drop `REJECT` recommendations unless there is clear contrary evidence; if you
  keep one, explain why in the final report.

---

## Step 9: Produce the Retrospective Report

Write a markdown report with this structure:

```markdown
# Plan-Do-Review Retrospective: #<issue>

## Summary
- **Issue**: #<issue> <title>
- **Worked issue**: #<issue> / same as original
- **Completion evidence**: Complete / Partial / Insufficient
- **Proposal rounds**: <used>/<max>
- **Review-fix rounds**: <used>/<max>
- **Final outcome**: <completed / partial / unclear>

## Reconstructed Timeline

| Stage | Round | Verdict | Key feedback/finding count | Evidence |
|---|---:|---|---:|---|

## Iteration Cause Analysis

| Stage | Round | Cause category | Avoidable? | Evidence | Process implication |
|---|---:|---|---|---|---|

## Valuable Iterations

List iterations that should not be optimized away because they caught real,
non-obvious issues.

## Avoidable Iterations

List iterations that likely could have been prevented by a general process
improvement.

## Accepted Process Improvements

For each accepted recommendation:
- **Change**:
- **Target skill file**:
- **Evidence**:
- **Why this is not overfit**:
- **Expected convergence impact**:
- **Risk/overhead**:

## Rejected Candidates

For each rejected candidate:
- **Candidate**:
- **Reason rejected**: overfit / duplicate / low-value / harmful / insufficient evidence

## Proposed Skill-File Edits

Summarize exact sections to edit. If `--apply` was used, list files changed.
If not, clearly state that no files were modified.

## Final Verdict

VERDICT: <NO_PROCESS_CHANGE | RECOMMEND_CHANGES | APPLIED_CHANGES | INSUFFICIENT_EVIDENCE>
```

Final verdicts:

- `VERDICT: NO_PROCESS_CHANGE` - no general process improvement survived review.
- `VERDICT: RECOMMEND_CHANGES` - recommendations survived review, but `--apply`
  was not set.
- `VERDICT: APPLIED_CHANGES` - `--apply` was set and edits were made.
- `VERDICT: INSUFFICIENT_EVIDENCE` - the issue did not contain enough
  plan-do-review audit trail to evaluate.

Write the report to `$WORKDIR/report.md` and print it to the terminal. If
`--apply` is set, Step 10 updates this report after edits and prints the final
version again.

---

## Step 10: Optional `--apply`

If `--apply` was not set, skip to Step 11.

If `--apply` was set:

1. Check the working tree:
   ```bash
   git --no-pager status --short -- .github/skills/plan-do-review/SKILL.md .claude/skills/review-fix/SKILL.md
   ```
   If either target file has unrelated uncommitted changes, stop and ask the
   caller how to proceed. Do not overwrite user work.

2. Edit only target skill files justified by accepted recommendations:
   - `.github/skills/plan-do-review/SKILL.md`
   - `.claude/skills/review-fix/SKILL.md`

3. Keep edits process-oriented:
   - Add or tighten checklists, required evidence, output fields, or side-effect
     safeguards.
   - Prefer a small insertion in the relevant step over broad rewrites.
   - Do not add issue-specific examples.
   - Do not weaken adversarial review.

4. After edits, verify:
   ```bash
   git --no-pager diff -- .github/skills/plan-do-review/SKILL.md .claude/skills/review-fix/SKILL.md
   rg -n '^---|^name:|^description:|^argument-hint:' .github/skills/plan-do-review/SKILL.md .claude/skills/review-fix/SKILL.md
   ```

5. Update `$WORKDIR/report.md`:
   - list the files and sections changed in "Proposed Skill-File Edits"
   - replace the final verdict with `VERDICT: APPLIED_CHANGES`
   - keep rejected candidates and overfitting rationale in the report

6. Print the updated final report.

Do not commit or push the edits unless the caller explicitly asks after the
skill completes.

---

## Step 11: Optional `--comment`

If `--comment` was set, post the final report currently stored at
`$WORKDIR/report.md` to the analyzed GitHub issue. If both `--apply` and
`--comment` were set, this step must happen after Step 10 so the comment reflects
the applied edits.

Use a file and `--body-file`; do not use heredoc command substitution.

```bash
tmpfile=$(mktemp)
{
  printf '## Plan-Do-Review Retrospective\n\n'
  cat "$WORKDIR/report.md"
} > "$tmpfile"
gh issue comment "$ISSUE" --body-file "$tmpfile"
rm -f "$tmpfile"
```

If `--comment` was not set, do not post to GitHub.

---

## Completion Output

Always finish with:

```text
=== Plan-Do-Review Retrospective Complete ===
Issue:                 #<issue>
Worked issue:          #<issue>
Proposal rounds:       <used>/<max>
Review-fix rounds:     <used>/<max>
Accepted improvements: <count>
Files changed:         <count, or 0 if --apply absent>
Final verdict:         <verdict>
=============================================
```

Clean up the temporary working directory unless it contains artifacts the caller
asked to keep:

```bash
rm -rf "$WORKDIR"
```
