---
name: security-fix
description: Triage and fix a GitHub security audit issue — validate, test, fix, commit, review
argument-hint: <issue-number-or-url>
---

Parse `$ARGUMENTS`:
- The first argument is a GitHub issue number or URL. Replace `$ISSUE` with it.

# Security Fix

Triage a security audit issue, validate whether it is a real finding, and if
so: write a failing regression test, fix the code, commit, push, close the
issue, and invoke `/review-fix` on the commit.

## Crate-to-Upstream Mapping

Use this table to find the upstream stellar-core directory for each crate.

| Crate | Upstream Directory |
|-------|--------------------|
| `crates/tx` | `stellar-core/src/transactions/` |
| `crates/scp` | `stellar-core/src/scp/` |
| `crates/db` | `stellar-core/src/database/` |
| `crates/common` | `stellar-core/src/util/` |
| `crates/crypto` | `stellar-core/src/crypto/` |
| `crates/ledger` | `stellar-core/src/ledger/` |
| `crates/bucket` | `stellar-core/src/bucket/` |
| `crates/herder` | `stellar-core/src/herder/` |
| `crates/overlay` | `stellar-core/src/overlay/` |
| `crates/history` | `stellar-core/src/history/` |
| `crates/historywork` | `stellar-core/src/historywork/` |
| `crates/work` | `stellar-core/src/work/` |
| `crates/app` | `stellar-core/src/main/` |
| `crates/henyey` | `stellar-core/src/main/` (CLI subset) |

## Step 1: Fetch and Parse the Issue

Run:
```
gh issue view $ISSUE --json title,body,labels,state,comments,number
```

Extract from the issue body:
- **Audit ID**: from the `[AUDIT-xxx]` prefix in the title
- **Severity**: CRITICAL / HIGH / MEDIUM / LOW
- **Category**: e.g., authentication bypass, determinism violation, integer overflow
- **Location**: files and functions mentioned
- **Description**: the detailed explanation of the issue
- **Suggested fix**: the proposed remediation
- **References**: audit report paths, stellar-core references

If the issue is already closed, stop and report: "Issue $ISSUE is already closed."

If the issue has no `[AUDIT-` prefix in the title, stop and report: "Issue
$ISSUE does not appear to be a security audit issue."

### Claim the issue (self-assign)

You are about to invest real time in this issue. Signal that it is in progress
by assigning yourself **now**, before validation and code work (Step 2 onward).

Run:
```
gh issue edit $ISSUE --add-assignee @me
```

- If you are already an assignee, this is a no-op; still run it for a uniform
  workflow.
- If the command fails (permissions, org policy, SSO), log the error and
  **continue** — do not block triage or fixes on assignee updates. Mention the
  failure when you post the final issue comment or PR description if relevant.

Read the linked audit report if one is referenced (e.g.,
`reports/audit/overlay__auth.rs.md`).

## Step 2: Validate the Finding

Read **all** files mentioned in the issue's Location section. Read enough
surrounding context to understand the code — callers, callees, types, and
related modules.

### Parity-Sensitive Code

If the affected code is in a parity-sensitive crate (any crate that touches
consensus, ledger state, protocol logic, SCP, herder, overlay auth, bucket,
or transaction execution), **always** read the corresponding stellar-core
code. Use the Crate-to-Upstream Mapping above to find the right directory.
Read the upstream `.h` file first for API surface, then the relevant `.cpp`
sections.

### Validation Criteria

Determine whether the finding is real by evaluating:

1. **Code matches description?** Does the code actually behave as the issue
   claims? Read the actual implementation, not just function signatures.
2. **Impact realistic?** Could the described impact actually occur in
   production? Consider: Is the affected code path reachable? Are there
   upstream guards that prevent exploitation?
3. **Already fixed?** Has this been addressed by a subsequent commit? Check
   `git log --oneline -- <affected-files>` for recent changes.
4. **Parity check**: For parity-sensitive code, does stellar-core actually
   handle this differently, as the issue claims? Read the upstream code to
   confirm.

### Classification

Classify the finding as one of:

- **CONFIRMED**: The code behaves as described and the issue is real.
- **FALSE_POSITIVE**: The code does not behave as described, or there are
  mitigating factors that make the issue non-exploitable. Common reasons:
  - The code already handles the case correctly (misread by auditor)
  - Upstream guards prevent the described attack
  - The described behavior is actually correct / matches stellar-core
  - The affected code path is unreachable
- **ALREADY_FIXED**: The issue was real but has been fixed since the audit.

## Step 3: Handle Non-Issues

### FALSE_POSITIVE

Close the issue with a comment explaining exactly why it is not a real issue.
The comment must:
- Cite specific code at `file:line` that contradicts the finding
- Explain the actual behavior vs the claimed behavior
- Reference stellar-core behavior if relevant
- Be respectful — the auditor may have missed context, not been wrong about
  the pattern

Format:
```
## Assessment: False Positive

This finding is not a real issue because:

<explanation with file:line references>

### Actual Behavior
<what the code actually does>

### Why This Is Safe
<specific guards, checks, or design properties that prevent the issue>

Closing as false positive.
```

Run:
```
gh issue close $ISSUE --comment "$(cat <<'EOF'
<comment body>
EOF
)"
```

**Stop here.** Do not proceed to Steps 4-6.

### ALREADY_FIXED

Close the issue with a comment citing the fix:

```
## Assessment: Already Fixed

This issue was addressed in commit <hash>:

<brief description of what the commit changed>

Closing as already fixed.
```

**Stop here.** Do not proceed to Steps 4-6.

## Step 4: Write Failing Test(s)

Per AGENTS.md: "When investigating a bug, always start by writing a narrow
unit test that reproduces the bug and fails. Then fix the code until the test
passes. Do not skip the failing-test-first step."

### Test Placement

- Unit tests: `#[cfg(test)] mod tests { }` at the bottom of the affected
  source file.
- Integration tests: `crates/<crate>/tests/` when the test requires
  multi-module setup.

### Test Design

Write the minimal test that demonstrates the vulnerability:

- **Name**: `test_<audit_id_lowercase>_<brief_description>`
  Example: `test_audit_c1_mac_bypass_rejected`
- **Setup**: Construct the minimal state that triggers the bug.
- **Action**: Perform the operation described in the issue.
- **Assert**: Assert the correct behavior (which currently fails because the
  bug exists).

For determinism issues: assert that the output is identical across multiple
runs or with different internal orderings.

For integer overflow: assert that the operation returns an error or saturates
instead of wrapping.

For authentication bypass: assert that the unauthorized operation is rejected.

For silent fallbacks: assert that the code panics or errors instead of
silently degrading.

### Verify the Test Fails

Run:
```
cargo test -p <crate> -- <test_name>
```

The test **must fail**. This proves the bug exists and the test is meaningful.

If the test passes:
- Re-read the issue and the code. You may have misunderstood the bug.
- If the test genuinely cannot be made to fail, the finding may not be
  reproducible. Reassess — consider reclassifying as FALSE_POSITIVE and
  returning to Step 3.

If writing a test is genuinely impossible (e.g., the issue is about a race
condition that cannot be deterministically triggered in a unit test), document
why in a code comment at the test site, write the best approximation you can,
and proceed.

## Step 5: Fix the Code

Implement the fix. Use these sources to guide the implementation:

1. **The issue's suggested fix** — start here, but verify it is correct and
   complete.
2. **stellar-core's behavior** — for parity-sensitive code, the fix MUST
   produce behavior identical to stellar-core. Read the upstream
   implementation and match it.
3. **AGENTS.md principles**:
   - Never fail silently. If assumptions are not met, error out.
   - Prefer types from `rs-stellar-xdr` over custom types.
   - Any observable behavior must be deterministic.
   - Keep modules small and focused.

### Fix Verification

After implementing the fix, run these commands in order:

1. `cargo test -p <crate> -- <test_name>` — regression test must now **pass**
2. `cargo test -p <crate>` — no other tests broken
3. `cargo clippy -p <crate>` — no warnings introduced
4. `cargo test --all` — full workspace passes

If any step fails, fix the issue before proceeding. If the fix causes
failures in other tests, those tests may have been relying on the buggy
behavior — fix them to expect the correct behavior.

## Step 6: Commit, Push, Close, and Review

### Commit

Stage and commit:
```
git add -A
git commit -m "Fix [AUDIT-<ID>]: <short description>" -m "" -m "Co-authored-by: GitHub Copilot <copilot@github.com>"
```

The commit message should be short, imperative, sentence case. Examples:
- `Fix [AUDIT-C1]: Gate MAC verification on receiver auth state`
- `Fix [AUDIT-M1]: Replace HashSet with BTreeSet for deterministic iteration`
- `Fix [AUDIT-H5]: Add overflow checks to fee computation`

### Push

Push immediately per AGENTS.md:
```
git push
```

If push is rejected, pull with rebase and retry:
```
git pull --rebase && git push
```

### Close the Issue

Close the issue with a comment linking the fix:

```
gh issue close $ISSUE --comment "$(cat <<'EOF'
## Fixed

Fixed in commit <full-hash>.

### Changes
- <bullet summary of what changed>

### Regression Test
- `<test_name>` in `<file_path>` — verifies <what the test checks>
EOF
)"
```

### Post-Fix Review

Invoke the `/review-fix` skill on the commit to check for similar issues and
apply any that are found:

```
/review-fix <commit-hash> --apply
```

This handles:
- Checking if the fix is correct and complete
- Scanning for similar issues elsewhere in the codebase
- Applying fixes for confirmed similar issues
- Suggesting refactoring opportunities to prevent this category of bug

## Guidelines

- **Claim early.** Self-assign as soon as the issue is confirmed open and
  audit-scoped (end of Step 1), so duplicate work is visible to the team.
- **Be precise.** Cite `file:line` for every claim in validation comments.
- **Do not speculate.** If you cannot determine whether a finding is real,
  read more code until you can. If you still cannot after thorough
  investigation, err on the side of CONFIRMED and write the test.
- **Read the actual code, not just the issue.** The audit may have been
  working from stale code or may have misread something.
- **One fix per issue.** Do not bundle fixes for multiple issues in one commit.
- **Test first, always.** Do not skip the failing-test step even if the fix
  seems obvious.
- **Parity is non-negotiable.** For parity-sensitive code, the fix must match
  stellar-core behavior exactly. Read the upstream code.
- **Never fail silently.** If the fix involves error handling, prefer loud
  failure (panic, error) over silent degradation.
- **Use subagents for exploration.** When you need to search the codebase
  (e.g., finding all callers of a function, or checking if a pattern exists
  elsewhere), use the Task tool with `explore` type.
