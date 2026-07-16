# review-pr reference — Step 7-bis: Force-converge merge (lifetime cap, CI green)

This is the full worked procedure for the **force-converge merge** edge case,
extracted from `SKILL.md` for progressive disclosure. It is only reached when
Step 6's force-converge override fires (lifetime bounce cap reached **and** CI
green). Consult it only on that branch; the common review flow never enters here.

## Step 7-bis — Force-converge merge (lifetime cap, CI green)

Only entered if Step 6's force-converge override fired. The flow is parallel to Step 7 but takes its follow-up source from the **top-level reviewer CHANGES_REQUESTED concern bullets** instead of (or in addition to) unaddressed inline comments. CI is green by precondition of this branch.

#### 7-bis.1 Collect unresolved concern bullets

Parse the latest `## 🔍 Reviewer: Correctness` and `## 🔍 Reviewer: <Parity|Risk>` PR comments. If a reviewer's verdict is `CHANGES_REQUESTED`, extract each bulleted concern from the `<details><summary>Full review</summary>` block. Group by the `class:` label on each bullet (or "(unlabeled)" if missing — older comments may predate the discipline). External reviewer CHANGES_REQUESTED bodies are processed the same way.

Also fetch unaddressed inline comments via the Step 7.1 mechanism — those still file follow-ups too.

#### 7-bis.2 File one follow-up issue per unresolved concern bullet

For each concern bullet:

```bash
gh issue create --repo stellar-experimental/henyey \
  --title "<class>: <short summary derived from concern bullet, ≤80 chars>" \
  --body "$(cat <<EOF
Follow-up from PR #$PR_NUM (issue #$ISSUE). PR was force-converged at the lifetime bounce cap; this concern was raised by a reviewer but not resolved before merge.

## Concern class

\`<class-name>\` (from reviewer cycle-1 change-list discipline)

## Source

[Reviewer comment](<link-to-reviewer-PR-comment>) on PR #$PR_NUM.

Reviewer: <Correctness|Parity|Risk|external-username>
Verdict: CHANGES_REQUESTED

## Detail

<full concern bullet body — quote the reviewer's text verbatim>

## Why this is a follow-up, not a merge-blocker

PR #$PR_NUM reached the lifetime bounce cap (6 cycles since last \`## Review: Reset\`) without converging. Per /review-pr force-converge policy, CI being green is sufficient to land the change and preserve unresolved concerns as backlog. Operator should triage this issue — close as won't-fix if the reviewer was over-strict, or schedule a follow-up PR if the concern is real.
EOF
)" \
  --label "follow-up,force-converge"
```

Also apply any `crate:<name>` label inferred from file paths in the concern text.

Collect the new issue numbers; they'll be referenced in the force-converge comment.

#### 7-bis.3 Merge

```bash
gh pr merge $PR_NUM --repo stellar-experimental/henyey --squash --admin
```

If `--admin` fails (no admin token), still file the follow-ups, but leave the PR open and post `## Review: Force-Converge Permission Gap` so the operator can land it manually. Do NOT degrade to a non-admin merge.

#### 7-bis.4 Clean up + post

Same cleanup as Step 7.4 (move issue to `done`, unassign, prune worktree + build cache). Then post:

```markdown
## ⚠️ Force-Converged (Lifetime Cap)

**Commit:** <merge-commit-sha>
**Lifetime bounce count at force-converge:** <N>
**CI at merge:** green

This PR cycled 6+ times across multiple code states without convergence between reviewers and `/do`. Rather than blocking indefinitely, the pipeline force-merged on the strength of green CI alone, preserving each unresolved reviewer concern as a follow-up issue for operator triage.

**Unresolved concern follow-ups (by class):**
- `<class-A>`: #N1, #N2
- `<class-B>`: #N3
- `(unlabeled)`: #N4 *(reviewer didn't follow cycle-1 class-labeling discipline)*

**Inline-comment follow-ups (if any):** #N5, #N6

To prevent this in future cycles: reviewers should produce a COMPLETE cycle-1 change-list grouped by concern class. New classes appearing in cycle N≥2 should be flagged `**NEW CLASS DISCOVERED:**` to surface incomplete cycle-1 reviews early.
```

Exit.
