#!/usr/bin/env bash
# Dependency-aware queue depth: open ISSUE_LABEL issues without WIP_LABEL whose
# "Depends on:" issue numbers are all closed. Prints one integer.
set -u
: "${GH_REPO:?}"; : "${ISSUE_LABEL:=ai}"; : "${WIP_LABEL:=ai:wip}"
json=$(gh issue list --repo "$GH_REPO" --label "$ISSUE_LABEL" --state open --limit 100 \
    --json number,body,labels 2>/dev/null) || { echo 1; exit 0; }
python3 - "$json" "$WIP_LABEL" "$GH_REPO" <<'PY'
import json, os, re, subprocess, sys
issues = json.loads(sys.argv[1]); wip = sys.argv[2]; repo = sys.argv[3]
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
    if any(l["name"] == wip for l in i["labels"]) and claimant_alive(i["number"]): continue
    # Every "Depends on" line counts: ticks append blockers on new lines.
    deps = [int(x) for line in re.findall(r"Depends on[^:\n]*:\s*(.*)", i.get("body") or "")
            for x in re.findall(r"#(\d+)", line)]
    if all(closed(d) for d in deps): count += 1
print(count)
PY
