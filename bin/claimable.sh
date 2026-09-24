#!/usr/bin/env bash
# Dependency-aware queue depth: open ISSUE_LABEL issues without WIP_LABEL whose
# "Depends on:" issue numbers are all closed, and which are not parked for the
# owner (SIGNOFF_LABEL). Prints one integer.
set -u
: "${GH_REPO:?}"; : "${ISSUE_LABEL:=ai}"; : "${WIP_LABEL:=ai:wip}"; : "${SIGNOFF_LABEL:=ai:signoff}"
json=$(gh issue list --repo "$GH_REPO" --label "$ISSUE_LABEL" --state open --limit 100 \
    --json number,body,labels 2>/dev/null) || { echo 1; exit 0; }
# The issue list goes through a file, never argv: forty issue bodies exceed ARG_MAX,
# and the heredoc below already owns stdin.
list_file=$(mktemp); trap 'rm -f "$list_file"' EXIT; printf '%s' "$json" > "$list_file"
python3 - "$list_file" "$WIP_LABEL" "$GH_REPO" "$SIGNOFF_LABEL" <<'PY'
import json, os, re, subprocess, sys
issues = json.load(open(sys.argv[1])); wip = sys.argv[2]; repo = sys.argv[3]; signoff = sys.argv[4]
open_nums = {i["number"] for i in issues}
def closed(n):
    if n in open_nums: return False
    out = subprocess.run(["gh", "issue", "view", str(n), "--repo", repo, "--json", "state", "--jq", ".state"],
                         capture_output=True, text=True).stdout.strip()
    return out == "CLOSED"
def claimant_alive(n):
    # Held while any PID that claimed and did not later withdraw is alive; withdrawn or dead
    # claimants make the issue resumable.
    out = subprocess.run(["gh", "issue", "view", str(n), "--repo", repo, "--json", "comments",
                          "--jq", ".comments[].body"], capture_output=True, text=True).stdout or ""
    claimed = set(re.findall(r"harness tick .*?pid (\d+) (?:claiming|resuming)", out))
    withdrawn = set(re.findall(r"pid (\d+) (?:withdrawing|pausing|releasing)", out))
    return any(os.path.exists("/proc/" + pid) for pid in claimed - withdrawn)
count = 0
for i in issues:
    # An issue waiting for the owner is never claimable, however its claimant ended.
    if any(l["name"] == signoff for l in i["labels"]): continue
    if any(l["name"] == wip for l in i["labels"]) and claimant_alive(i["number"]): continue
    # Every "Depends on" line counts: ticks append blockers on new lines.
    deps = [int(x) for line in re.findall(r"Depends on[^:\n]*:\s*(.*)", i.get("body") or "")
            for x in re.findall(r"#(\d+)", line)]
    if all(closed(d) for d in deps): count += 1
print(count)
PY
