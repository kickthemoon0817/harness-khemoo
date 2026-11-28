#!/usr/bin/env bash
# Closes each open pull request on a `<BRANCH_PREFIX><n>-…` branch (default prefix `ai/`)
# whose issue #<n> is closed. An issue that lands on another pull request, or that the operator
# closes, otherwise leaves its own draft open. The branch stays, so a closed pull request can be
# reopened. One line per close goes to $HARNESS_STATE/logs/pr-sweep.log.
#
#   pr-sweep.sh             close them
#   pr-sweep.sh --dry-run   name them, close nothing
set -u
# shellcheck source=/dev/null
. "$(dirname "${BASH_SOURCE[0]}")/../lib/instance.sh"
# Every key exported: gh reads GH_TOKEN from the config.
set -a; harness_load; set +a
: "${GH_REPO:?set GH_REPO in the instance harness.env}"
: "${BRANCH_PREFIX:=ai/}"
dry=""; [ "${1:-}" = --dry-run ] && dry=1
LOG="${PR_SWEEP_LOG:-$HARNESS_STATE/logs/pr-sweep.log}"; mkdir -p "$(dirname "$LOG")"
ts() { date -u +%FT%TZ; }

prs=$(gh api --paginate "repos/$GH_REPO/pulls?state=open&per_page=100" \
    --jq '.[] | "\(.number) \(.head.ref)"' < /dev/null) \
    || { echo "$(ts) pull request list failed" >> "$LOG"; exit 1; }
while read -r pr ref; do
  [ -n "$pr" ] && [[ "$ref" == "$BRANCH_PREFIX"* ]] || continue
  n=${ref#"$BRANCH_PREFIX"}
  [[ "$n" =~ ^[0-9]+- ]] || continue
  n=${n%%-*}
  issue=$(gh api "repos/$GH_REPO/issues/$n" --jq '"\(.state) \(.closed_at // "-")"' < /dev/null 2>/dev/null) || continue
  read -r state closed_at <<< "$issue"
  [ "$state" = closed ] || continue
  if [ -n "$dry" ]; then echo "would close PR #$pr ($ref): #$n closed $closed_at"; continue; fi
  body="Closing: #$n was closed on ${closed_at%%T*} and this pull request did not land it. The branch \`$ref\` stays, so this can be reopened."
  gh api -X POST "repos/$GH_REPO/issues/$pr/comments" -f body="$body" --jq .id < /dev/null > /dev/null 2>&1 \
    || { echo "$(ts) PR #$pr comment failed" >> "$LOG"; continue; }
  if gh api -X PATCH "repos/$GH_REPO/pulls/$pr" -f state=closed --jq .state < /dev/null > /dev/null 2>&1; then
    echo "$(ts) closed PR #$pr ($ref): #$n closed $closed_at" >> "$LOG"
  else
    echo "$(ts) PR #$pr close failed" >> "$LOG"
  fi
done <<< "$prs"
