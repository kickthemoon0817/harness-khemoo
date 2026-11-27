#!/usr/bin/env bash
# Dependency-aware queue depth: open ISSUE_LABEL issues without WIP_LABEL whose
# "Depends on:" issue numbers are all closed, and which are not parked for the
# owner (SIGNOFF_LABEL). Prints one integer.
# Priority: an issue labelled LOW_LABEL counts only when no other
# issue of the list is claimable, and none labelled FIRST_LABEL is claimable anywhere; an
# issue labelled FABLE_LABEL without FIRST_LABEL counts only while no FIRST_LABEL issue is
# claimable anywhere. So low-priority work and new design rounds wait for the take-first queue.
set -u
: "${GH_REPO:?}"; : "${ISSUE_LABEL:=ai}"; : "${WIP_LABEL:=ai:wip}"; : "${SIGNOFF_LABEL:=ai:signoff}"
: "${OPERATOR_LABEL:=ai:operator}"; : "${READY_LABEL:=ai:ready}"
: "${FIRST_LABEL:=ai:first}"; : "${LOW_LABEL:=ai:low}"; : "${FABLE_LABEL:=ai:fable}"
# A tick takes a local claim (bin/claim.sh) before its claim comment reaches GitHub.
CLAIM_LOCKS="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}/state/locks"
json=$(gh issue list --repo "$GH_REPO" --label "$ISSUE_LABEL" --state open --limit 100 \
    --json number,body,labels 2>/dev/null) || { echo 1; exit 0; }
# The issue list goes through a file, never argv: forty issue bodies exceed ARG_MAX,
# and the heredoc below already owns stdin.
list_file=$(mktemp); trap 'rm -f "$list_file"' EXIT; printf '%s' "$json" > "$list_file"
python3 - "$list_file" "$WIP_LABEL" "$GH_REPO" "$SIGNOFF_LABEL" "$OPERATOR_LABEL" "$READY_LABEL" "$CLAIM_LOCKS" \
    "$ISSUE_LABEL" "$FIRST_LABEL" "$LOW_LABEL" "$FABLE_LABEL" <<'PY'
import json, os, re, subprocess, sys
issues = json.load(open(sys.argv[1])); wip = sys.argv[2]; repo = sys.argv[3]; signoff = sys.argv[4]
operator = sys.argv[5]; ready = sys.argv[6]; claim_locks = sys.argv[7]
issue_label, first, low, fable = sys.argv[8], sys.argv[9], sys.argv[10], sys.argv[11]
import datetime
# Merge slots (runbook-plan override 3c): the first hour of every third UTC hour.
slot_open = True  # every hour is a merge slot (owner, 2026-11-25)
open_nums = {i["number"] for i in issues}
def closed(n):
    if n in open_nums: return False
    out = subprocess.run(["gh", "issue", "view", str(n), "--repo", repo, "--json", "state", "--jq", ".state"],
                         capture_output=True, text=True).stdout.strip()
    # A dependency named by a PR number is met once the PR merges (its state reads MERGED).
    return out in ("CLOSED", "MERGED")
def claimant_alive(n):
    # Held while any PID that claimed and did not later withdraw is alive; withdrawn or dead
    # claimants make the issue resumable.
    out = subprocess.run(["gh", "issue", "view", str(n), "--repo", repo, "--json", "comments",
                          "--jq", ".comments[].body"], capture_output=True, text=True).stdout or ""
    claimed = set(re.findall(r"harness tick .*?pid (\d+) (?:claiming|resuming)", out))
    withdrawn = set(re.findall(r"pid (\d+) (?:withdrawing|pausing|releasing)", out))
    return any(os.path.exists("/proc/" + pid) for pid in claimed - withdrawn)
def local_claim_alive(n):
    # The local claim is taken before the comment, so it sees a claimant the comments do not show yet.
    try:
        pid = open(os.path.join(claim_locks, "claim.%d" % n)).read().split()[1]
    except (OSError, IndexError):
        return False
    return pid.isdigit() and os.path.exists("/proc/" + pid)
def names(i): return {l["name"] for l in i["labels"]}
def claimable(i):
    if local_claim_alive(i["number"]): return False
    # An issue waiting for its sign-off is never claimable, however its claimant ended.
    if signoff in names(i): return False
    # Nor is one paused until the operator answers it: a tick could only find it waiting.
    if operator in names(i): return False
    # A PR ready for a merge slot is work only while a slot is open.
    if ready in names(i) and not slot_open: return False
    if wip in names(i) and claimant_alive(i["number"]): return False
    # Every "Depends on" line counts: ticks append blockers on new lines.
    deps = [int(x) for line in re.findall(r"Depends on[^:\n]*:\s*(.*)", i.get("body") or "")
            for x in re.findall(r"#(\d+)", line)]
    return all(closed(d) for d in deps)
cands = [i for i in issues if claimable(i)]
def first_waiting():
    if any(first in names(i) for i in cands): return True
    if issue_label == first: return False
    out = subprocess.run(["gh", "issue", "list", "--repo", repo, "--label", first, "--state", "open",
                          "--limit", "100", "--json", "number,body,labels"], capture_output=True, text=True)
    if out.returncode != 0: return False
    others = [i for i in json.loads(out.stdout or "[]") if i["number"] not in open_nums]
    return any(claimable(i) for i in others)
gated = [i for i in cands if low in names(i) or (fable in names(i) and first not in names(i))]
fw = first_waiting() if gated else False
keep = [i for i in cands if low not in names(i) and not (fable in names(i) and first not in names(i) and fw)]
if not keep and not fw:
    keep = [i for i in cands if low in names(i)]
print(len(keep))
PY
