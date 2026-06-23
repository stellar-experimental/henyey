---
name: triage
description: |
  Validate a freshly filed henyey issue, label it correctly, and route it to the
  next pipeline stage. Operates on issues in the `backlog` column of project #2.
  Routes well-formed actionable issues to `ready-for-planning`, trivial issues
  directly to `ready-for-doing`, and malformed/duplicate/oversized issues to
  `blocked` with a reason. Use when invoked by /project-tick with an issue in
  backlog, or manually as /triage <issue>.
argument-hint: <issue-number> [--force]
---

# /triage <issue> — backlog gate

You triage one issue. You do **not** explore the codebase. You do **not** write code. You read the issue, decide if it is well-formed and actionable, label it correctly, and advance its state on the project board.

## Inputs

- `$ISSUE` — issue number (positional arg).
- Issue body, comments, and labels (fetched via `gh issue view`).
- Open + recently-closed issue titles (for duplicate detection).

## Algorithm

### Step 1 — Read the issue

```bash
gh issue view $ISSUE --repo stellar-experimental/henyey --json title,body,labels,comments,createdAt,author
```

Skim the body and the last few comments. Note the existing labels. If the issue was filed by `/monitor-loop`, the body should already be structured (problem statement, evidence, suggested labels). If filed by a human, structure may be looser.

### Step 2 — Verify the handoff (no-op if not `backlog`)

Confirm the issue's current Status is `backlog`:

```bash
STATUS=$(gh api graphql -f query='{organization(login:"stellar-experimental"){projectV2(number:2){items(first:100){nodes{content{...on Issue{number}} fieldValueByName(name:"Status"){...on ProjectV2ItemFieldSingleSelectValue{name}}}}}}}' \
  --jq ".data.organization.projectV2.items.nodes[] | select(.content.number == $ISSUE) | .fieldValueByName.name")
```

Branching:

- **Status is `backlog`** (the expected case): proceed to Step 3.
- **Status is anything else AND `--force` was NOT passed**: exit 0 silently. Triage should not demote an issue that an operator or another agent has already moved forward. Post nothing, change nothing.
- **Status is anything else AND `--force` was passed**: re-triage as if `backlog` — the operator explicitly asked for a re-evaluation. Note in the resulting Triage Report which prior state we ran from (for audit trail).
- **A `## Triage Report` comment already exists** AND `--force` was NOT passed: exit 0 silently (also handles "monitor-loop refiled an issue we already triaged" cases).

This early-exit protects against the bug where /triage runs on an `in-review` issue and accidentally routes it to `blocked` (`needs-human-decision` etc.).

### Step 3 — Validate well-formed-ness

Check each of:

1. **Title is action-oriented.** Good: `Fix SCP message dedup loop`. Bad: `bug in scp`, `something is wrong`.
2. **Body has a clear problem statement.** Should answer: what's broken / missing / wanted, and where (file path, observable behavior, log excerpt).
3. **Labels are sane.** At minimum, exactly one type label (`bug`, `enhancement`, `documentation`, `question`). One severity label for bugs (`critical` / `high` / `medium` / `low`). Optionally one `crate:*` label.
4. **Not a duplicate.** Search open + recently-closed for matching titles or keywords:
   ```bash
   gh issue list --repo stellar-experimental/henyey --search "<key terms>" --state all --limit 10 --json number,title,state
   ```
   If a clear duplicate exists, route to `blocked` with a comment pointing at the duplicate.

### Step 3.5 — Classify the kind of change + capture test seed

Every accepted issue gets one of these `kind:` values in the triage report. Downstream skills key off this:

| `kind:` | Meaning | Test obligation |
|---|---|---|
| `bug-fix` | Repairs broken behavior. The issue body describes what's wrong | Must include a **regression test** that fails on current `main` and passes on the fix |
| `feature` | Adds new functionality, new public API, or new behavior | Must include **new tests** exercising every new public function and the new behavior |
| `refactor` | Restructures code without changing behavior | Existing tests must keep passing; net behavioral diff = 0; usually no new tests required |
| `docs` | Documentation, comments, READMEs only | No test obligation (but doc-tests must still pass if present) |
| `test-only` | Test infrastructure / hygiene change (e.g. TempDir leak fix) | The test change IS the deliverable |

**For `bug-fix` kind specifically**, capture a **failing-mode seed** in the triage report — the observable behavior of the bug that the regression test should reproduce. Examples:

- "`assert_eq!` on line 42 of `scp.rs` fires with `expected: 7, got: 9` when fed a 3-validator quorum"
- "`upload_history` panics with `unwrap` on `None` when `mkdir_cmd` is empty"
- "Test `test_xyz` hangs >60s; the bug is a deadlock in `tx_set_tracker`"

This seed is what `/plan` and `/do` will use to write the failing test. Without it, the bug-fix can't be TDD'd.

### Step 4 — Estimate size

Set the `Size` project field to one of `XS`, `S`, `M`, `L`, `XL` based on your reading:

- **XS** — single-line change, doc tweak, trivial.
- **S** — single function or small file, few lines of code, obvious tests.
- **M** — multiple functions in one crate, non-trivial logic, new tests needed.
- **L** — touches multiple crates, requires careful design, significant test surface.
- **XL** — spans subsystems, requires multi-step plan, will likely produce sub-issues.

If you'd estimate **XL**, the issue is too big for a single planning cycle. File sub-issues, set the parent-issue field, and route the original to `blocked` with a comment explaining the decomposition. Do not try to plan an XL.

### Step 5 — Decide the route

#### Route A — Trivial short-circuit (→ `ready-for-doing`)

If ALL of these hold:

- Size estimate is **XS**.
- Single-file change.
- No protocol / consensus / parity implications (does not touch `crates/scp/`, `crates/herder/`, `crates/ledger/`, `crates/tx/`, `crates/overlay/`).
- Test impact is obvious (no new test file needed, or one assertion in an existing test).

Then mark the issue trivial. Post a `## Triage Report` comment with the structure below, including an `## Implementation Notes` section that names the exact file path and the precise change. `/do` will use these notes as the plan and skip `/plan` entirely.

```markdown
## Triage Report

**Verdict:** ACCEPT (trivial — short-circuit to `ready-for-doing`)

**Type:** documentation
**Severity:** n/a
**Crate:** docs
**Size:** XS

**Summary:** <one sentence>

## Implementation Notes

- File: `crates/scp/README.md`
- Change: replace "5 seconds" with "5-second" on line 42.
- Test impact: none (docs only).
```

Advance state:

```bash
bash .github/skills/shared/scripts/move-issue-status.sh $ISSUE ready-for-doing
gh issue edit $ISSUE --repo stellar-experimental/henyey --remove-assignee @me
```

#### Route B — Normal accept (→ `ready-for-planning`)

If well-formed, actionable, and not trivial:

```markdown
## Triage Report

**Verdict:** ACCEPT

**Type:** <bug|enhancement|documentation>
**Kind:** <bug-fix|feature|refactor|docs|test-only>
**Severity:** <critical|high|medium|low|n/a>
**Crate:** <crate:scp | crate:herder | … | none>
**Size:** <XS|S|M|L>

**Summary:** <one or two sentences explaining what needs to happen and why>

**Test obligation:** <derived from Kind — see Step 3.5 table>

<For Kind=bug-fix, add this block:>

**Failing-mode seed (for the regression test):**
- Observable: <what the bug looks like: panic, wrong return, hang, etc.>
- Reproducer: <minimal scenario that triggers it — fixture, input, config>
- Where: <crate / file / function the bug lives in>

**Label adjustments:** <list any labels added/removed>
```

If you adjusted labels, do so via `gh issue edit --add-label / --remove-label`. Then:

```bash
bash .github/skills/shared/scripts/move-issue-status.sh $ISSUE ready-for-planning
gh issue edit $ISSUE --repo stellar-experimental/henyey --remove-assignee @me
```

#### Route C — Reject (→ `blocked`)

If the issue is malformed, a duplicate, or too big:

```markdown
## Triage Report

**Verdict:** BLOCKED

**Reason:** <one of: malformed | duplicate | out-of-scope | needs-human-decision | too-broad>

**Detail:** <specifics>

<For duplicates:> Duplicate of #<N>.

<For too-broad:> Filed sub-issues: #<N1>, #<N2>, … Setting parent. Recommend humans review the decomposition before advancing any sub-issue past backlog.
```

For too-broad, file the sub-issues first (use `gh issue create` with a clear scope each), set parent via the project board's parent-issue field, then:

```bash
bash .github/skills/shared/scripts/move-issue-status.sh $ISSUE blocked
gh issue edit $ISSUE --repo stellar-experimental/henyey --remove-assignee @me
```

## Examples

### Accept — normal
```
$ /triage 2698
Reading issue #2698 — "HERDER: Wire ScpPersistenceManager GC timer"
Type: enhancement, Crate: crate:herder, Size: S
Verdict: ACCEPT → ready-for-planning
```

### Accept — trivial short-circuit
```
$ /triage 2662
Reading issue #2662 — "Add prompts/ to deploy-gate"
Type: enhancement, Size: XS, single-file, no parity impact
Verdict: ACCEPT (trivial) → ready-for-doing
Implementation Notes: edit .github/workflows/deploy-gate.yml, add 'prompts/' to paths-ignore list.
```

### Reject — duplicate
```
$ /triage 2701
Reading issue #2701 — "bucket-list determinism mismatch"
Search reveals open issue #2503 with identical scope.
Verdict: BLOCKED (duplicate of #2503)
```

### Reject — too-broad
```
$ /triage 2660
"Spec adherence audit: suite overview (Protocol 26)"
This is an umbrella for 7 sub-audits (already filed: #2677-#2683).
Setting parent on those sub-issues. Routing #2660 to blocked as umbrella tracker.
```

## What you do NOT do

- **Do not** explore the codebase. If you can't tell what crate is involved from the issue body, that's a sign the issue is malformed — route to `blocked` (`needs-human-decision`) and ask the author to clarify.
- **Do not** propose a fix. That's `/plan`'s job. The only exception is the trivial short-circuit, where the "fix" is small enough to be one line in the triage report.
- **Do not** invoke critics. Triage is fast and one-shot. The next stage's worker verifies your output (handoff verification).
- **Do not** create the project item if missing — `move-issue-status.sh` does that.

## Failure handling

- **GH API failure:** retry once after 5 seconds. If still failing, leave the issue as-is (assigned to you) and exit non-zero — the operator will see the stuck assignment.
- **Duplicate of itself:** if your search returns the same issue you're triaging, ignore that match.
- **Unable to determine type or severity:** that means the body is too thin — route to `blocked` with `needs-human-decision`.
