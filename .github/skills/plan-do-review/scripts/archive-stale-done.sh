#!/usr/bin/env bash
# archive-stale-done.sh — archive closed project items older than N days.
#
# Usage:
#   archive-stale-done.sh [--dry-run] [days]
#
# Default days = 2.
#
# Archives every project #2 item whose underlying Issue/PR is in state
# CLOSED and was closed at least N days ago, unless already archived.
# Idempotent and safe to run from cron / a scheduled GitHub Action /
# the plan-do-review-loop tick.
#
# Why not use the built-in "Auto-archive items" project workflow?
#   GitHub does not expose the workflow's filter/action via the public
#   GraphQL API — only its enabled flag. There is no
#   createProjectV2Workflow or updateProjectV2Workflow mutation. So a
#   from-scratch CLI implementation is the only "config-as-code" path.
set -euo pipefail

DRY_RUN=false
if [[ "${1:-}" == "--dry-run" ]]; then
  DRY_RUN=true
  shift
fi
DAYS="${1:-2}"

OWNER="stellar-experimental"
PROJECT_NUM=2
PROJECT_ID="PVT_kwDOD-vqsM4BWQnL"

# Fetch all project items + their issue/PR state and closedAt.
# `--paginate` requires the cursor variable to be named $endCursor; jq -s
# is required because gh emits one JSON object per page.
items_json=$(gh api graphql --paginate -f query='
  query($org: String!, $proj: Int!, $endCursor: String) {
    organization(login: $org) {
      projectV2(number: $proj) {
        items(first: 100, after: $endCursor) {
          pageInfo { endCursor hasNextPage }
          nodes {
            id
            isArchived
            content {
              ... on Issue       { number state closedAt }
              ... on PullRequest { number state closedAt }
            }
          }
        }
      }
    }
  }' -f org="$OWNER" -F proj="$PROJECT_NUM")

# Eligible: not yet archived, has issue/PR content (skip drafts), state CLOSED,
# closedAt older than the cutoff.
to_archive=$(jq -rs --argjson days "$DAYS" '
  .[].data.organization.projectV2.items.nodes[]
  | select(.isArchived == false)
  | select(.content != null)
  | select(.content.state == "CLOSED")
  | select(.content.closedAt != null)
  | select((.content.closedAt | fromdateiso8601) < (now - ($days * 86400)))
  | "\(.id) \(.content.number) \(.content.closedAt)"
' <<<"$items_json")

if [ -z "$to_archive" ]; then
  echo "No items to archive (closed > ${DAYS} day(s) ago)."
  exit 0
fi

count=0
while IFS=' ' read -r item_id issue_num closed_at; do
  [ -z "$item_id" ] && continue
  if $DRY_RUN; then
    echo "[dry-run] would archive #${issue_num} (closed ${closed_at})"
  else
    gh api graphql -f query='
      mutation($projectId: ID!, $itemId: ID!) {
        archiveProjectV2Item(input: { projectId: $projectId, itemId: $itemId }) {
          item { id }
        }
      }' -f projectId="$PROJECT_ID" -f itemId="$item_id" >/dev/null
    echo "Archived #${issue_num} (closed ${closed_at})"
  fi
  count=$((count + 1))
done <<<"$to_archive"

if $DRY_RUN; then
  echo "Dry-run complete: ${count} item(s) would be archived."
else
  echo "Archived ${count} item(s)."
fi
