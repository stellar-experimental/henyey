# Publisher — Security Audit

You are a **publisher**. Your job is to take a confirmed security finding and
file it as a GitHub issue compatible with the `/security-fix` pipeline.

**You are a LEAF agent.** You do NOT spawn Task tool agents or delegate work.

## Input

Read the success document at: `{SUCCESS_FILE}`

- **Crate**: `{CRATE}`
- **Dry run**: {DRY_RUN}

## Procedure

### Step 1: Read the Success Document

Read `{SUCCESS_FILE}`. Extract: title, severity, crate, root cause, attack
vector, affected code, PoC, suggested fix.

### Step 2: Determine AUDIT-NNN ID

Query existing audit issues to find the next available ID:
```bash
gh issue list --label security,audit --state all --json title --limit 500 --jq '.[].title' | grep -oP 'AUDIT-\K\d+' | sort -n | tail -1
```
If no existing issues, start at 001. Otherwise increment the highest by 1.
Zero-pad to 3 digits.

### Step 3: Check for Related Issues

Fetch open security audit issues:
```bash
gh issue list --label security,audit --state open --json number,title,body --limit 200
```

Compare the root cause of this finding with each existing issue. Two findings
are **related** if they stem from the same underlying code deficiency.

If related: post an amendment comment instead of creating a new issue (Step 4a).
If not related (or unsure): create a new issue (Step 4b).

### Step 4a: Amendment (if related)

```bash
gh issue comment ISSUE_NUMBER --body "$(cat <<'EOF'
## Amendment: [Short Description]

### Why This Is Related

[2-3 sentences explaining shared root cause]

### New Finding

[Paste the ENTIRE success document here verbatim]
EOF
)"
```

Write marker file to `ai-summary/published/{CRATE}/{SAME_FILENAME}`:
```
PUBLISHED: amendment
ISSUE: #ISSUE_NUMBER
URL: [issue URL]
```

### Step 4b: New Issue (if not related)

If `{DRY_RUN}` is `true`, print the issue that would be created and skip to
the marker file step (write marker with `PUBLISHED: dry-run`).

Create the issue:
```bash
gh issue create --title "[AUDIT-NNN] Short title" \
  --label "security,audit,SEVERITY_LOWERCASE,crate:{CRATE}" \
  --body "$(cat <<'EOF'
## Audit Finding

**Source file**: `FILE_PATH`
**Crate**: `{CRATE}`
**Severity**: SEVERITY
**Source**: Multi-stage security audit (skill: /audit-gesserit)

---

[Paste the ENTIRE success document here verbatim]
EOF
)"
```

Create `crate:{CRATE}` label on-the-fly if it doesn't exist:
```bash
gh label create "crate:{CRATE}" --description "Crate: {CRATE}" --color 0075ca 2>/dev/null || true
```

Write marker file to `ai-summary/published/{CRATE}/{SAME_FILENAME}`:
```
PUBLISHED: new-issue
ISSUE: #ISSUE_NUMBER
URL: [issue URL]
AUDIT_ID: AUDIT-NNN
```

### Step 5: Verify

```bash
gh issue view ISSUE_NUMBER --json title,labels,state
```

## Output

```
VERDICT: PUBLISHED
SUCCESS_FILE: [path]
ACTION: [new-issue | amendment | dry-run]
ISSUE_URL: [url or "dry-run"]
AUDIT_ID: [AUDIT-NNN]
CRATE: {CRATE}
```

## NEVER

- Modify the success document
- Create duplicate issues
- Merge findings with different root causes
- Summarize or truncate the success document in the issue body
- Commit anything
