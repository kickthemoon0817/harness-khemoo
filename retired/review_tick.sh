#!/usr/bin/env bash
# Best-model review tick: after REVIEW_EVERY_TICKS implementation ticks on one issue since its
# last [fable-review], run one read-only review that sets the direction and checks the PR.
# Cron-fired; at most one review at a time; never touches the GPU lease.
set -u
HARNESS_HOME="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
# shellcheck source=/dev/null
[ -r "$HARNESS_HOME/config/harness.env" ] && . "$HARNESS_HOME/config/harness.env"
: "${HARNESS_STATE:=$HARNESS_HOME/state}"; : "${GH_REPO:?}"; : "${ISSUE_LABEL:=ai}"; : "${WIP_LABEL:=ai:wip}"
: "${REVIEW_EVERY_TICKS:=4}"; : "${REVIEW_MODEL:=claude-fable-5-1}"; : "${CLAUDE_BIN:=claude}"
: "${REVIEW_FLAGS:=--dangerously-skip-permissions --model $REVIEW_MODEL}"
export PATH="${HARNESS_PATH:-$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin}"
LOGS="$HARNESS_STATE/logs"; LOCKS="$HARNESS_STATE/locks"; mkdir -p "$LOGS" "$LOCKS"
exec {fd}>"$LOCKS/review.lock"; flock -n "$fd" || exit 0
force="${1:-}"
issue=$(python3 - "$GH_REPO" "$ISSUE_LABEL" "$WIP_LABEL" "$REVIEW_EVERY_TICKS" "$force" <<'PY'
import json, re, subprocess, sys
repo, label, wip, every, force = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4]), sys.argv[5]
if force:
    print(force); sys.exit(0)
issues = json.loads(subprocess.run(["gh","issue","list","--repo",repo,"--label",label,"--state","open",
    "--limit","100","--json","number,labels,createdAt"],capture_output=True,text=True).stdout or "[]")
due = []
for i in sorted(issues, key=lambda x: x["createdAt"]):
    if not any(l["name"] == wip for l in i["labels"]): continue
    # One comment per JSON line; only a comment that BEGINS with the marker is a
    # review. Tick comments quote the marker in prose, which must not reset the count.
    out = subprocess.run(["gh","api","--paginate",f"repos/{repo}/issues/{i['number']}/comments?per_page=100",
        "--jq",".[] | {body: .body}"],capture_output=True,text=True).stdout or ""
    ticks = 0
    for line in out.splitlines():
        try: body = json.loads(line)["body"]
        except Exception: continue
        if re.match(r"\s*\[fable-review\] \d{4}-", body): ticks = 0; continue
        if re.match(r"\s*harness tick .*?pid \d+ (?:claiming|resuming)", body, re.S): ticks += 1
    if ticks >= every: due.append((i["number"], ticks))
if due: print(due[0][0])
PY
)
[ -z "$issue" ] && exit 0
ts=$(date -u +%Y%m%dT%H%M%SZ); log="$LOGS/review-$ts-$issue.log"; start_s=$(date +%s)
cd "${TARGET_REPO:?}" || exit 1
"$CLAUDE_BIN" -p "$(cat "$HARNESS_HOME/prompts/review-prompt.md")

ISSUE=$issue" $REVIEW_FLAGS >"$log" 2>&1
echo "[review_tick.sh] issue=$issue model=$REVIEW_MODEL exit=$? duration=$(( $(date +%s) - start_s ))s" >>"$log"
