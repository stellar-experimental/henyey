---
name: audit-gesserit
description: Multi-stage adversarial security audit with hypothesis, review, PoC, publish
argument-hint: "[--crate <name>] [--max-rounds N] [--no-hypothesis] [--no-poc] [--no-publish] [--dry-run] [--resume] [--promote <path>]"
---

Parse `$ARGUMENTS`:
- `--crate <name>`: Restrict to a single crate. Default: all crates by priority.
- `--max-rounds N`: Maximum orchestrator iterations (default: 30).
- `--no-hypothesis`: Skip hypothesis generation; only process existing pipeline state.
- `--no-poc`: Skip PoC stage; promote reviewed hypotheses directly to final review.
- `--no-publish`: Skip publishing to GitHub issues.
- `--dry-run`: Run full pipeline but print issues instead of filing them.
- `--resume`: Resume from existing `ai-summary/` state instead of fresh start.
- `--promote <path>`: Re-promote a finding from `fail/` back to `hypothesis/`
  for re-evaluation. The path must be a file in `ai-summary/fail/{crate}/`
  (not `summary.md`). The file is moved to `ai-summary/hypothesis/{crate}/`
  with any prior Review/PoC/Final Review sections stripped. Exits after
  promotion without entering the main loop (combine with `--resume` to then
  process it).

# Gesserit Security Audit

Multi-stage adversarial security audit pipeline. Each finding passes
through independent agents at every stage — hypothesis, review, PoC, final
review, and publish — with fail/ as the rejection path at each gate.

This skill complements `/audit-ctf` (single-pass) with a thorough multi-stage
pipeline. Published issues are compatible with `/security-fix` and
`/security-fix-loop`.

---

## Configuration

### Crate-to-Upstream Mapping

| Crate | Upstream Directory |
|-------|--------------------|
| `tx` | `stellar-core/src/transactions/` |
| `scp` | `stellar-core/src/scp/` |
| `db` | `stellar-core/src/database/` |
| `common` | `stellar-core/src/util/` |
| `crypto` | `stellar-core/src/crypto/` |
| `ledger` | `stellar-core/src/ledger/` |
| `bucket` | `stellar-core/src/bucket/` |
| `herder` | `stellar-core/src/herder/` |
| `overlay` | `stellar-core/src/overlay/` |
| `history` | `stellar-core/src/history/` |
| `historywork` | `stellar-core/src/historywork/` |
| `work` | `stellar-core/src/work/` |
| `app` | `stellar-core/src/main/` |
| `henyey` | `stellar-core/src/main/` |
| `rpc` | *(no upstream)* |
| `simulation` | *(no upstream)* |

### Crate Risk Tiers

| Tier | Crates |
|------|--------|
| **Consensus-critical** | tx, ledger, scp, herder, bucket |
| **Network-facing** | overlay, rpc |
| **Infrastructure** | app, history, historywork, crypto, common, db, clock |
| **Test/development** | simulation, work |

### Priority Tiers (for hypothesis scheduling)

| Tier | Weight | Crates |
|------|--------|--------|
| **High** | 60% | tx, scp, overlay, herder |
| **Medium** | 30% | ledger, bucket |
| **Low** | 10% | crypto, rpc, app, history, historywork, db, common, work, simulation, clock, henyey |

When `--crate` is specified, only that crate is targeted regardless of tier.

---

## Step 1: Initialization

### 1a: Handle `--promote` (if set)

If `--promote <path>` is specified:
1. Validate the path is a file in `ai-summary/fail/{crate}/` and is not
   `summary.md`.
2. Read the file. Strip everything after the first `---` separator that begins
   a `## Review`, `## PoC`, or `## Final Review` section (keep only the
   original hypothesis content).
3. Move the stripped content to `ai-summary/hypothesis/{crate}/{filename}`.
4. Delete the original fail file.
5. Print: `Promoted: {path} → hypothesis/{crate}/{filename}`
6. Exit. (Use `--resume` in a subsequent invocation to process it.)

### 1b: Determine Target Crates

If `--crate` is specified, targets = just that crate. **Validate** that the
crate name exists in the Crate-to-Upstream Mapping table above and that
`crates/{crate}/src/` exists on disk. If not, print an error listing the valid
crate names and exit.

Otherwise, targets = all crates from the priority tiers above.

### 1c: Create Directory Structure

If `--resume` is NOT set, create the `ai-summary/` directory tree:

```bash
for stage in hypothesis reviewed poc fail success published; do
  for crate in $TARGETS; do
    mkdir -p ai-summary/$stage/$crate
  done
done
```

If `--resume` IS set, verify that `ai-summary/` already exists. Read its
current state.

### 1d: Initialize Tracking

Use TaskCreate to create a top-level task for the audit run, and sub-tasks
for tracking progress through the pipeline stages.

Initialize counters:
```
round = 0
hypotheses_generated = 0
hypotheses_promoted = 0
hypotheses_rejected = 0
pocs_attempted = 0
pocs_confirmed = 0
pocs_rejected = 0
findings_published = 0
```

---

## Step 2: Main Orchestrator Loop

Repeat until no more work is available or `round >= max_rounds`:

### 2a: Scan Pipeline State

Use Bash `ls` to check each stage directory for pending files:
```bash
# Count files in each stage for each target crate
for crate in $TARGETS; do
  ls ai-summary/hypothesis/$crate/*.md 2>/dev/null | wc -l
  ls ai-summary/reviewed/$crate/*.md 2>/dev/null | wc -l
  ls ai-summary/poc/$crate/*.md 2>/dev/null | wc -l
  ls ai-summary/success/$crate/*.md 2>/dev/null | wc -l
  ls ai-summary/fail/$crate/*.md 2>/dev/null | grep -v summary.md | wc -l
done
```

### 2b: Priority-Ordered Work Selection

Process in strict priority order. Pick the **first available work item**:

**Priority 1 — PUBLISH**: If any files exist in `success/{crate}/` (and
`--no-publish` is not set), pick the oldest one and spawn a **publish agent**.

**Priority 2 — CONDENSATION**: If any `fail/{crate}/` has ≥20 non-summary
`.md` files, spawn a **condensation agent** for that crate.

**Priority 3 — FINAL REVIEW**: If any files exist in `poc/{crate}/`, pick the
oldest one and spawn a **final-review agent**.

**Priority 4 — POC**: If any files exist in `reviewed/{crate}/` (and
`--no-poc` is not set), pick the oldest one and spawn a **PoC agent**.

If `--no-poc` IS set: Instead of spawning a PoC agent, copy the file from
`reviewed/{crate}/` to `poc/{crate}/` with a synthetic PoC-skipped section
appended:

```markdown

---

## PoC

**Result**: POC_SKIPPED (--no-poc flag)
**Date**: [today's date]

No PoC was attempted. The final reviewer must perform independent code
analysis to verify the finding instead of reproducing a test.
```

This ensures the final-review agent can detect the `--no-poc` path and
adjust its procedure accordingly (code analysis instead of test reproduction).

**Priority 5 — REVIEW**: If any files exist in `hypothesis/{crate}/`, pick the
oldest one. **Before spawning**, validate the hypothesis file has the required
sections (`## Expected Behavior`, `## Mechanism`, `## Attack Vector`,
`## Target Code`). If any are missing, move the file directly to
`fail/{crate}/` with an orchestrator note ("malformed hypothesis: missing
section X") and increment `hypotheses_rejected`. Do NOT spawn a reviewer agent
for malformed files. Then spawn a **reviewer agent** for the validated file.

**Priority 6 — HYPOTHESIS**: If `--no-hypothesis` is not set and target crates
have not been exhausted, spawn a **hypothesis agent** for the next target crate.

**Backpressure**: Skip hypothesis generation for a crate if it has 10 or more
unprocessed files across `hypothesis/{crate}/` + `reviewed/{crate}/` combined.
This prevents hypothesis buildup when downstream stages can't keep up. The
crate becomes eligible again once its backlog drops below 10.

**Target crate selection for hypothesis**: Use weighted random selection from
the priority tiers (60% high, 30% medium, 10% low). Within a tier, pick the
crate with the fewest existing hypothesis+reviewed+poc+success files (spread
coverage). If a crate has had 5+ hypothesis rounds with no promotions to
reviewed/, consider it temporarily exhausted and skip it.

**No work available**: If none of the above priorities have work, exit the loop.

### 2c: Assemble Prompt and Spawn Agent

For the selected work item, assemble the agent prompt:

1. **Read the stage template**: Use the Read tool to read
   `.claude/skills/audit-gesserit/stages/{stage}.md`

2. **Read context files** (for all stages):
   - `.claude/skills/audit-gesserit/context/security.md`
   - `.claude/skills/audit-gesserit/context/suppression-rules.md`
     (for reviewer, final-review, and hypothesis stages)

3. **Read crate context** (if exists):
   - `crates/{crate}/README.md`
   - `crates/{crate}/PARITY_STATUS.md`

4. **Read novelty context** (for hypothesis and reviewer stages):
   - `ai-summary/fail/{crate}/summary.md`

5. **Read the input file** (for review/poc/final-review/publish):
   - The specific hypothesis/reviewed/poc/success file being processed

6. **Substitute variables** in the stage template:
   - `{CRATE}` → crate name
   - `{UPSTREAM_DIR}` → upstream directory from mapping table
   - `{RISK_TIER}` → risk tier from table
   - `{NNN}` → next file number (for hypothesis stage)
   - `{HYPOTHESIS_FILE}` → path to file being processed
   - `{SUCCESS_FILE}` → path to success file (for publish)
   - `{DRY_RUN}` → "true" or "false"
   - `{SAME_FILENAME}` → filename of the input file (preserved across stages)

7. **Concatenate** the assembled prompt:
   ```
   [stage template with substitutions]

   === CONTEXT: Security ===
   [security.md contents]
   === END CONTEXT ===

   === CONTEXT: Suppression Rules ===
   [suppression-rules.md contents]
   === END CONTEXT ===

   === CONTEXT: Crate README ===
   [README.md contents, if exists]
   === END CONTEXT ===

   === CONTEXT: Parity Status ===
   [PARITY_STATUS.md contents, if exists]
   === END CONTEXT ===

   === CONTEXT: Prior Failures ===
   [fail summary.md contents, if exists]
   === END CONTEXT ===

   === INPUT: Hypothesis File ===
   [hypothesis file contents, if applicable]
   === END INPUT ===
   ```

8. **Spawn the Agent** using the Agent tool:
   - `prompt`: the assembled prompt
   - `model`: `"opus"` for hypothesis/reviewer/poc/final-review; `"sonnet"` for publish/condensation
   - `description`: e.g., "hypothesis for tx crate" or "review H-003"

### 2d: Process Agent Result

After the agent completes, detect the verdict by checking the filesystem:

**For hypothesis agents:**
- Check `ai-summary/hypothesis/{crate}/` for new files → hypotheses generated
- Check `ai-summary/fail/{crate}/` for new files → self-rejected
- If no new files in either → agent found nothing novel (crate may be exhausted)
- Update counters: `hypotheses_generated += new_hypothesis_count`

**For reviewer agents:**
- Check `ai-summary/reviewed/{crate}/` for new file → VIABLE
  - Move original from `hypothesis/{crate}/` to processed (delete it)
  - `hypotheses_promoted += 1`
- Check `ai-summary/fail/{crate}/` for new file → NOT_VIABLE
  - Delete original from `hypothesis/{crate}/`
  - `hypotheses_rejected += 1`
- If file is still in `hypothesis/` (agent failed to write output):
  - Move it to `fail/{crate}/` with orchestrator note appended
  - `hypotheses_rejected += 1`

**For PoC agents:**
- Check `ai-summary/poc/{crate}/` for new file → POC_PASS
  - Delete original from `reviewed/{crate}/`
  - `pocs_attempted += 1`
- Check `ai-summary/fail/{crate}/` for new file → POC_FAIL
  - Delete original from `reviewed/{crate}/`
  - `pocs_attempted += 1; pocs_rejected += 1`
- If file still in `reviewed/`: move to `fail/` with note
- **Cleanup**: Check for leftover test files:
  ```bash
  find crates/*/tests -name 'audit_poc_*' -delete 2>/dev/null
  ```

**For final-review agents:**
- Check `ai-summary/success/{crate}/` for new file → CONFIRMED
  - Delete original from `poc/{crate}/`
  - `pocs_confirmed += 1`
- Check `ai-summary/fail/{crate}/` for new file → REJECTED
  - Delete original from `poc/{crate}/`
  - `pocs_rejected += 1`
- If file still in `poc/`: move to `fail/` with note
- **Cleanup**: `find crates/*/tests -name 'audit_poc_*' -delete 2>/dev/null`

**For publish agents:**
- Check `ai-summary/published/{crate}/` for marker file → PUBLISHED
  - `findings_published += 1`
- If no marker: leave success file in place for retry next round

**For condensation agents:**
- Verify `ai-summary/fail/{crate}/summary.md` was updated
- Count remaining individual fail files

### 2e: Update Progress

Increment `round`. Print a one-line progress update:

```
Round N/M: [stage] [crate] [verdict] — H:X/Y/Z PoC:A/B/C Published:P
```

Where H = generated/promoted/rejected, PoC = attempted/confirmed/rejected.

Update tasks via TaskUpdate.

### 2f: Parallelism Opportunities

When multiple independent work items are available at different priority levels,
you MAY spawn up to 3 agents in parallel in a single message. Independent work:
- Publish + any other stage (publishing doesn't affect code analysis)
- Condensation + any other stage (condensation only touches fail/)
- Hypothesis agents for different crates (no shared state)

Do NOT parallelize:
- Two agents for the same crate (state conflicts)
- Review + PoC for the same hypothesis (sequential dependency)
- Any two agents that may both write to the same `fail/{crate}/` directory
  (e.g., a hypothesis agent for crate X that may self-reject + a reviewer for
  crate X that may reject — both write to `fail/X/`). The one-agent-per-crate
  rule covers this, but be especially careful: condensation for crate X counts
  as "an agent for crate X" since it reads and deletes from `fail/X/`.

---

## Step 3: Completion Summary

After the loop exits, print:

```
═══ Gesserit Audit Complete ═══
Rounds:           N / M
Target:           <crate name or "all crates">

Hypotheses:       X generated, Y promoted, Z rejected
PoC attempts:     A total, B confirmed, C rejected
Published:        P issues filed (or "P (dry run)")

Pipeline state:
  hypothesis/     [count] files pending review
  reviewed/       [count] files pending PoC
  poc/            [count] files pending final review
  success/        [count] files pending publish
  fail/           [count] total rejected

Issue URLs:       [list of published issue URLs]
═══════════════════════════════
```

If the pipeline still has files in intermediate stages (hypothesis, reviewed,
poc, success), note that `/audit-gesserit --resume` can continue processing.

---

## Guidelines

- **One agent at a time per crate.** Never spawn two agents that read/write
  the same crate's pipeline directories simultaneously.
- **Use subagents for all code analysis.** The orchestrator (this skill) only
  manages pipeline state. It never reads source code or makes security judgments.
- **Trust the pipeline.** Each stage is adversarial by design. If a finding
  survives all 4 gates (hypothesis → review → PoC → final review), it is
  high-confidence.
- **Quality over speed.** A single confirmed finding is worth more than 50
  hypotheses. Don't rush through the pipeline — let each agent do thorough work.
- **Resume is your friend.** The filesystem state machine means you can stop
  and resume at any time. Use `--resume` to continue where you left off.
- **Fail files are valuable.** They prevent future agents from repeating the
  same investigations. Never delete `ai-summary/` between runs unless you want
  a clean slate.
- **The suppression rules matter.** Prior audits had a 97.5% false-positive
  rate. The 10 suppression rules in `context/suppression-rules.md` are critical
  for avoiding the same mistakes.
