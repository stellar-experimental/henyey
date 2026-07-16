# monitor-tick reference — Self-reflection: three tiers of action + boundaries

Detailed decision policy for the `## Self-reflection` step in `SKILL.md`,
extracted for progressive disclosure. The SKILL body keeps the common-path
trigger (what to look for, and `self_reflect: clean` when nothing is found);
consult this file only when a self-reflection finding needs acting on, to pick
a tier and follow its procedure. Zero information loss — verbatim relocation of
the "Three tiers of action" and "Boundaries" subsections.

### Three tiers of action

When an issue is found, choose a tier and act on it in the same tick.

**Tier 1 — Fix inline (trivial edit).**

Criteria (ALL must hold):
- Change is contained to `.claude/skills/monitor-tick/SKILL.md`
  (no other files touched)
- Diff < 50 lines
- No new runtime dependency (no new file path, env var, or metric name
  that the skill depends on)
- The edit is a clear text change with an obvious correct value:
  typo fix, threshold adjustment to match an observed live baseline,
  shell-portability fix, adding a metric to an existing exemption
  list, rendering-template tweak
- The need is demonstrated by THIS tick's observation — not speculative

Action sequence (same pattern used for every skill edit in this repo):

```bash
# 1. Edit .claude/skills/monitor-tick/SKILL.md
# 2. git add .claude/skills/monitor-tick/SKILL.md
# 3. git commit -m "Monitor-tick: <what + why>" with Co-authored-by: Claude Code trailer
# 4. git push origin main (on reject: git pull --rebase && git push)
```

Report: `self_reflect: fixed inline (<short-sha>: <short-desc>)`.

**Tier 2 — File GH issue (non-trivial but codeable).**

Apply the Bug filing workflow's Label policy: most monitor-tick self-reflection
issues are non-urgent (calibration / threshold / catalog tuning) and should
be filed without a label. Only use `urgent` if the skill's miscalibration is
silently masking a real validator-blocking signal.

The issue is real and actionable but any of:
- Touches multiple files or crates
- Requires a design choice (which metric? which threshold value?
  which algorithm?)
- Needs verification beyond the tick (new test cases, reproducing
  with a build)
- Affects runtime contracts (env schema, file format, section
  ordering that another skill depends on)

Before filing, search for an existing open issue:
`gh issue list --search "monitor-tick: <keywords>" --state open`.
Comment with new evidence if a match exists; otherwise file.
Board-route per `scripts/lib/monitor-label-policy.md`: Backlog for
actionable issues, Blocked for `not-ready` issues.

Issue body MUST include:
- **Symptom**: one-line description of the false positive / silent
  failure / contradiction
- **Evidence**: exact tick output and command results that demonstrated
  the issue
- **Suspected root cause**: which rule / threshold / code path
- **Concrete fix sketch**: file:line references and proposed diff
  direction
- **Related to #<prior>** if it's a recurrence of something already
  filed

Title format:
- `Non-critical: monitor-tick: <description>` — for observability /
  calibration issues (noise reduction, threshold tuning) — file with no label
- `monitor-tick: <description>` — for correctness bugs (silent
  failures, contradictory output) — file with no label unless the bug
  is silently masking a real validator-blocking signal (then `urgent`)

Report: `self_reflect: filed #<N> (urgent: <short-desc>)` or
`self_reflect: filed #<N> (no-label: <short-desc>)`.

**Tier 3 — File `not-ready` GH issue (human input required).**

Use this tier when any of:
- The fix has product/ops policy implications (e.g., "should we
  monitor a new metric class?", "should we change the restart
  philosophy?", "should we broaden the auto-deploy trigger?")
- Ambiguous scope — fix A, B, or C would all work and the right
  choice depends on operator intent
- Touches the node code, another skill, or config defaults — the
  downstream fixer should not auto-pick this up without explicit
  operator direction

Issue body includes everything from Tier 2 plus an explicit
**"Human input required"** section listing the specific
decisions / options that need an operator answer.

Label: `not-ready`. Title format:
`monitor-tick: [needs-decision] <description>`.

Report: `self_reflect: filed #<N> (not-ready: <short-desc>)`.

### Boundaries

- **Scope is single-tick.** Do not retrospectively review prior ticks
  for drift. Cross-tick pattern detection is a separate concern.
- **Never suppresses a real node-side filing.** If a check flagged a
  SYNC FAILURE on the node and self-reflection concludes the detection
  was over-eager, the SYNC FAILURE filing still goes out this tick
  (real-or-not is downstream's call). File a separate Tier 2 issue to
  tune the detection.
- **Never re-opens or argues with already-filed issues** from this
  or prior ticks. Those stand as-is.
- **Trivial-fix bias**: when in doubt between Tier 1 and Tier 2,
  prefer Tier 2 (filing) over an aggressive inline edit. Inline edits
  affect every subsequent tick immediately; better to write up the
  rationale and let the operator review.
