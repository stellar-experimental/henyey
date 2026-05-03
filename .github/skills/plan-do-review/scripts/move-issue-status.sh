#!/usr/bin/env bash
# Usage: move-issue-status.sh <issue_number> <status_name>
# status_name ∈ { Backlog, "in plan", "In progress", "In review", Done, Blocked }
#
# Idempotent: adds the issue to the henyey project if missing, then sets its
# Status field. Exits 0 on success, non-zero on any failure.
set -euo pipefail

ISSUE="${1:?issue number required}"
STATUS="${2:?status name required}"

OWNER="stellar-experimental"
REPO="henyey"
PROJECT_NUM=2
PROJECT_ID="PVT_kwDOD-vqsM4BWQnL"
STATUS_FIELD_ID="PVTSSF_lADOD-vqsM4BWQnLzhRmYgI"

case "$STATUS" in
  Backlog)        OPTION_ID="f75ad846" ;;
  "in plan")      OPTION_ID="61e4505c" ;;
  "In progress")  OPTION_ID="47fc9ee4" ;;
  "In review")    OPTION_ID="df73e18b" ;;
  Done)           OPTION_ID="98236657" ;;
  Blocked)        OPTION_ID="53ce269e" ;;
  *) echo "ERROR: unknown status: $STATUS" >&2; exit 2 ;;
esac

ISSUE_URL="https://github.com/$OWNER/$REPO/issues/$ISSUE"

# Find the issue's existing project item on this project (if any).
ITEM_ID=$(gh api graphql -f query='
  query($owner: String!, $repo: String!, $num: Int!) {
    repository(owner: $owner, name: $repo) {
      issue(number: $num) {
        projectItems(first: 20) {
          nodes { id project { id } }
        }
      }
    }
  }' -f owner="$OWNER" -f repo="$REPO" -F num="$ISSUE" \
  --jq ".data.repository.issue.projectItems.nodes[]
        | select(.project.id == \"$PROJECT_ID\") | .id" | head -n1)

# If not on the project, add it.
if [ -z "$ITEM_ID" ]; then
  ITEM_ID=$(gh project item-add "$PROJECT_NUM" \
    --owner "$OWNER" --url "$ISSUE_URL" --format json --jq '.id')
fi

# Update the Status field.
gh project item-edit \
  --project-id "$PROJECT_ID" \
  --id "$ITEM_ID" \
  --field-id "$STATUS_FIELD_ID" \
  --single-select-option-id "$OPTION_ID" >/dev/null

echo "Moved #$ISSUE → $STATUS"
