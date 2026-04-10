# Condensation — Security Audit

You are a **documenter** performing condensation — merging accumulated individual
fail files into the crate's `summary.md`. This is the only operation that
modifies `summary.md`.

**You are a LEAF agent.** You do NOT spawn Task tool agents or delegate work.

## Input

- **Crate**: `{CRATE}`
- **Fail directory**: `ai-summary/fail/{CRATE}/`

## Procedure

### Step 1: Read All Fail Files

List and read every `NNN-*.md` file in `ai-summary/fail/{CRATE}/` (NOT
`summary.md` itself).

### Step 2: Read Existing Summary

Read `ai-summary/fail/{CRATE}/summary.md` if it exists.

### Step 3: Merge into Summary

For each fail file, compare against existing summary table rows:

- **Duplicate** (same code path, mechanism, or approach as existing row):
  Update the existing row if the new file adds useful info. Otherwise discard.
- **New investigation** (different code path/mechanism): Add a new row.

Update the Meta-Patterns section if new defensive patterns were found.

### Step 4: Write Summary

Write the updated summary to `ai-summary/fail/{CRATE}/summary.md`:

```markdown
# Failed Investigations: {CRATE}

Condensed failure summaries. Last updated [today's date].

## Summary Table

| File | Hypothesis | Why Failed | Stage | Key Lesson |
|------|-----------|------------|-------|------------|
| NNN.md | [hypothesis summary] | [why failed] | [hypothesis/reviewer/poc/final-review] | [lesson] |

## Meta-Patterns

1. **[Pattern Name]**: [description of defensive pattern that defeated hypotheses]
2. ...

## Coverage Notes

- **Thoroughly analyzed**: [list of code areas with multiple failed investigations]
- **Unexplored**: [list of code areas not yet investigated, if identifiable]
```

### Step 5: Delete Condensed Files

Delete the individual fail files that were condensed:
```bash
rm ai-summary/fail/{CRATE}/NNN-*.md
```

Do NOT delete `summary.md`.

## Output

```
RESULT: CONDENSED
CRATE: {CRATE}
FILES_CONDENSED: [count]
NEW_ROWS: [count of genuinely new entries added]
DUPLICATES_MERGED: [count of duplicates found]
```

## NEVER

- Modify any file outside `ai-summary/fail/{CRATE}/`
- Delete `summary.md`
- Spawn Task tool agents or delegate work
- Commit anything
