# review-pr reference — Step 4 reviewer briefs (Correctness / Parity / Risk)

Full verbatim prompt text for the two reviewer lenses run in Step 4 of
`SKILL.md`. The Step 4 body keeps the load-bearing contract (run two lenses,
post two structured verdict comments with the exact headers/verdict shape, the
concern-class discipline, cycle-awareness); this file holds the detailed
per-lens brief text a reviewer applies when composing each verdict comment.
Zero information loss — this is a relocation of the Step 4 "Reviewer A" and
"Reviewer B" brief blocks, reproduced exactly.

### Reviewer A — Correctness (always)

> Invoke /review on PR #$PR_NUM in stellar-experimental/henyey. Focus on:
> correctness of the diff, test coverage, readability, error handling.
>
> **Cycle-awareness (kills whack-a-mole bouncing) — do this BEFORE writing
> your review:**
>
> 1. Fetch your own prior comments on this PR:
>    \`\`\`bash
>    gh pr view $PR_NUM --repo stellar-experimental/henyey --comments \\
>      --json comments --jq '.comments[] | select(.body | startswith("## 🔍 Reviewer: Correctness")) | .body'
>    \`\`\`
> 2. If empty → this is **cycle 1**. Produce a COMPLETE change-list. Every
>    concern must be labeled with a **concern class** (a 1–3 word category,
>    e.g. \`test-coverage\`, \`workspace-contract\`, \`regression-risk\`,
>    \`error-handling\`, \`api-shape\`, \`ci-failure\`). Group concerns by class
>    in the verdict body. Don't hold concerns back hoping the doer figures
>    them out — your cycle-1 list is the contract for the whole review arc.
> 3. If non-empty → this is **cycle N≥2**. Read your latest prior verdict.
>    Enumerate the classes you raised. This cycle, you must:
>    - Verify each prior concern is addressed (re-evaluating within the same
>      class is fine — e.g. if you raised \`test-coverage\` and the test was
>      added but is wrong, that's still \`test-coverage\`).
>    - Stick to those classes. New specific bullets within an existing class
>      are fine.
>    - If you genuinely identify a concern in a class you did NOT raise
>      previously, you MAY add it, but you MUST flag the section
>      \`**NEW CLASS DISCOVERED:** <class-name>\` and explain in 1–2 sentences
>      why cycle 1 missed it. (This still counts as a normal bounce; the
>      flag is audit-trail evidence, not a block.)
>
> **Test verification (REQUEST_CHANGES if any of these fails):**
>
> **Test verification (REQUEST_CHANGES if any of these fails):**
>
> 1. Find the linked issue's `kind:` from its `## Triage Report` comment.
> 2. For `kind: bug-fix`:
>    - The PR must include a regression test. Find it by reading the PR body's
>      `## Regression test` section (which /do should have populated).
>    - **Verify the regression test would have caught the bug.** Walk the PR
>      commit list (\`gh pr view $PR_NUM --json commits\`). The test should
>      have been committed BEFORE the fix. Check out the parent of the fix
>      commit and run the test:
>      \`\`\`bash
>      git fetch origin pull/$PR_NUM/head:pr-$PR_NUM
>      git checkout <test-commit-sha>
>      cargo test -p henyey-<crate> <test_fn> 2>&1 | tail -10
>      \`\`\`
>      Confirm the test FAILS at that point. If the test passes at the test-
>      commit, the regression test doesn't actually capture the bug → bounce.
>    - If the PR body has no \`## Regression test\` section, or the section's
>      claims don't match what's in the diff, → bounce.
> 3. For `kind: feature`:
>    - Every new public function in the diff (search for new \`pub fn\`,
>      \`pub struct\`, etc. lines) must have at least one test exercising it.
>      Use \`gh pr diff $PR_NUM\` and grep for new public surface. Cross-check
>      against the test files in the diff. Untested new public surface →
>      bounce.
> 4. For `kind: refactor` / `docs` / `test-only`: existing tests must still
>    pass (CI will catch this) and the plan's "Existing tests preserved" list
>    must all be green in CI.
>
> **Visibility-narrowing / dead-code build-verify gate (CHANGES_REQUESTED):**
>
> For ANY PR diff that narrows visibility (`pub`→`pub(crate)`/`pub(super)`/private)
> or removes "dead" code: green CI proves the change COMPILES under this repo's
> global `-Dwarnings`, but a green build alone is NOT enough — you MUST verify the
> diff's per-symbol classification rationale, not rubber-stamp a grep-only
> justification. For each affected symbol, confirm the diff's choice matches:
> **(a)** in-crate non-test caller exists → `pub(crate)` is correct; **(b)** only
> `#[cfg(test)]` callers or none → the symbol is DEAD and the diff must delete it
> or keep it `pub` / `#[cfg(test)]` (a crate-root `pub use` re-export can be the
> sole keep-alive; an integration-test-crate caller blocks `pub(crate)` with
> E0624) — a blind `pub(crate)` on a (b)-class symbol would not have compiled, so
> if CI is green the live question is whether the (a)/(b) classification the diff
> assumed is actually the reason it compiles. Return **CHANGES_REQUESTED** if the
> PR's narrowing/deletion justification rests on a caller grep with no evidence the
> (a)/(b) classification was reasoned through. (Full worked trap: the `/plan`
> SKILL "Examples (verdict patterns)" → "Visibility-narrowing dead-code trap",
> the #3365 scp cfg(test) case.)
>
> Then evaluate logic, error handling, readability per usual.
>
> Post your verdict as a single PR-level comment using \`gh pr comment\`,
> headed \`## 🔍 Reviewer: Correctness\`, with \`**Verdict:** APPROVE\` or
> \`**Verdict:** CHANGES_REQUESTED\` on its own line. Inline line comments
> via \`gh api\` are welcome for specific concerns.

### Reviewer B — Parity OR Risk (auto-detected)

**If parity-critical:**

> Invoke /spec-adhere style audit on PR #$PR_NUM in stellar-experimental/henyey.
> Focus on parity as defined in `docs/PARITY.md`: does the change preserve the
> **observable / interop surface** — ledger/bucket hashes, transaction result &
> meta XDR, SCP/overlay wire bytes, history archive format, HTTP/RPC/CLI
> contracts, crypto outputs? Consult the `stellar-core/` submodule for the
> matching C++ implementation. Raise a CHANGES_REQUESTED concern **only** for a
> divergence in bytes that cross the network, land in an archive, or appear in
> hashes/XDR/API responses — i.e. something a peer, Horizon, stellar-rpc, or an
> archive consumer could observe. Differences in internal architecture, helper
> utilities, metrics, logging, admin/debug endpoints, or performance are
> explicitly allowed — do NOT flag them as parity concerns. Post your verdict as
> a single PR-level comment via `gh pr comment`, headed `## 🔍 Reviewer: Parity`,
> with `**Verdict:**` on its own line. Reviewer A is doing correctness; you
> focus only on observable-surface parity.
>
> **Cycle-awareness (same discipline as Reviewer A):** Before writing your
> review, fetch your own prior comments via
> \`gh pr view $PR_NUM --comments --json comments --jq '.comments[] | select(.body | startswith("## 🔍 Reviewer: Parity")) | .body'\`.
> If empty → cycle 1: produce a complete change-list with every concern
> labeled by a concern class (e.g. \`parity-gap\`, \`spec-divergence\`,
> \`sequencing\`, \`edge-case\`). If non-empty → cycle N≥2: stick to the
> classes you raised before; only add a new class with an explicit
> \`**NEW CLASS DISCOVERED:**\` flag and rationale. This stops the
> parity-vs-correctness bounce-orbit that historically pushed PRs into
> the lifetime cap.

**If non-parity (risk lens):**

> Review PR #$PR_NUM in stellar-experimental/henyey for risk: regressions in
> existing behavior, performance impact, breaking changes to APIs or data
> formats, security implications, operational concerns (config, migrations).
> Reviewer A is doing correctness; you focus only on risk. Post your verdict
> as a single PR-level comment via `gh pr comment`, headed
> `## 🔍 Reviewer: Risk`, with `**Verdict:**` on its own line.
>
> **Cycle-awareness (same discipline as Reviewer A):** Before writing your
> review, fetch your own prior comments via
> \`gh pr view $PR_NUM --comments --json comments --jq '.comments[] | select(.body | startswith("## 🔍 Reviewer: Risk")) | .body'\`.
> Cycle 1 → complete change-list, concerns labeled by class
> (e.g. \`regression-risk\`, \`perf-regression\`, \`api-break\`,
> \`migration-risk\`, \`config-risk\`). Cycle N≥2 → stick to your
> previously-raised classes; only add a new class with an explicit
> \`**NEW CLASS DISCOVERED:**\` flag and rationale.
