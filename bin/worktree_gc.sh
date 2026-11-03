#!/usr/bin/env bash
# Removes harness worktrees whose work is done, so the disk does not fill with
# them. A worktree goes only when its issue is closed (or, for parent-/base-
# trees, its commit has not been the branch's head for KEEP_HOURS and is not one
# of its last KEEP_HEADS heads), it has no uncommitted change, every commit it
# holds is on the remote, and no process runs inside it or container mounts it.
# A render queue reads a parent tree for hours after its base moved, and a slot
# can land several heads in an hour, so a count of heads alone is too short.
# Build containers leave root-owned files, so what git cannot delete is removed
# from inside the builder image.
#   worktree_gc.sh           dry run: prints what it would remove
#   worktree_gc.sh --apply   removes, and logs to state/logs/worktree_gc.log
set -u
H="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
# Cron gives no environment: take the harness token and PATH the way tick.sh does.
export HARNESS_HOME="$H"
[ -r "$H/config/harness.env" ] && set -a && . "$H/config/harness.env" && set +a
export PATH="${HARNESS_PATH:-$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin}"
WT="${WORKTREE_ROOT:-$HOME/tmp_workspace/ai-worktrees}"
REPO="${MAIN_REPO:-$HOME/tmp_workspace/isaac_sim}"
: "${GH_REPO:=MaumAI-Company/isaac_sim}"; : "${KEEP_HEADS:=3}"; : "${KEEP_HOURS:=6}"
apply=0; [ "${1:-}" = "--apply" ] && apply=1
LOG="$H/state/logs/worktree_gc.log"
open=$(gh issue list -R "$GH_REPO" --state open --limit 500 --json number --jq '.[].number' 2>/dev/null) || { echo "gh unavailable; nothing removed" >&2; exit 1; }
git -C "$REPO" fetch -q origin ai/manure-mpm 2>/dev/null
# The heads of the last KEEP_HOURS, the head the window opened on, and the last KEEP_HEADS.
recent=$( { git -C "$REPO" rev-list --first-parent --since="$KEEP_HOURS hours ago" origin/ai/manure-mpm
            git -C "$REPO" rev-list --first-parent -n 1 --until="$KEEP_HOURS hours ago" origin/ai/manure-mpm
            git -C "$REPO" rev-list --first-parent -n "$KEEP_HEADS" origin/ai/manure-mpm; } | cut -c1-8 | sort -u)
# The remote host holds a copy of every worktree a run there mounted; its containers count too.
remote=$(cat "$H/state/locks/remote-card" 2>/dev/null)
rdocker() { DOCKER_HOST="ssh://$remote" command docker "$@"; }
# This host's own daemon, whatever DOCKER_HOST the caller carries (bin/remote-env.sh points it at a missing socket).
ldocker() { DOCKER_HOST= command docker "$@"; }
busy=$( { for p in /proc/[0-9]*; do readlink "$p/cwd" 2>/dev/null; done
          for c in $(ldocker ps -q 2>/dev/null); do ldocker inspect --format '{{range .Mounts}}{{println .Source}}{{end}}' "$c" 2>/dev/null; done
          if [ -n "$remote" ]; then
            for c in $(rdocker ps -q 2>/dev/null); do rdocker inspect --format '{{range .Mounts}}{{println .Source}}{{end}}' "$c" 2>/dev/null; done
          fi
        } | grep "^$WT/" | sed "s|^$WT/||; s|/.*||" | sort -u)
removed=()
for path in "$WT"/*; do
  d=$(basename "$path"); [ -d "$path" ] && [ -e "$path/.git" ] || continue
  case "$d" in manure-plan-docs|manure-gui|slowset) continue;; esac
  if [[ "$d" =~ ^(parent|base)-([0-9a-f]{7,}) ]]; then
    sha=${BASH_REMATCH[2]}; echo "$recent" | grep -q "^${sha:0:8}" && continue
    why="old base $sha"
  else
    n=$(grep -o -E '[0-9]{3,5}' <<<"$d" | head -1); [ -n "$n" ] || continue
    echo "$open" | grep -qx "$n" && continue
    why="issue #$n closed"
  fi
  echo "$busy" | grep -qx "$d" && { echo "keep $d: a process runs inside it or a container mounts it"; continue; }
  if [ -n "$(git -C "$path" status --porcelain --untracked-files=no 2>/dev/null)" ]; then echo "keep $d: uncommitted changes"; continue; fi
  if [ -z "$(git -C "$path" branch -r --contains HEAD 2>/dev/null)" ]; then echo "keep $d: HEAD is on no remote branch"; continue; fi
  echo "remove $d ($why)"; removed+=("$d")
done
[ "$apply" = 1 ] && [ "${#removed[@]}" -gt 0 ] || { [ "$apply" = 1 ] || echo "(dry run)"; exit 0; }
for d in "${removed[@]}"; do git -C "$REPO" worktree remove --force "$WT/$d" >/dev/null 2>&1; done
left=(); for d in "${removed[@]}"; do [ -e "$WT/$d" ] && left+=("/w/$d"); done
[ "${#left[@]}" -gt 0 ] && ldocker run --rm --runtime=runc -v "$WT":/w --entrypoint bash "${WORV_BUILDER_IMAGE:-worv-builder:isaac6}" -lc "rm -rf ${left[*]}"
if [ -n "$remote" ]; then
  gone=(); for d in "${removed[@]}"; do gone+=("/w/$d"); done
  rdocker run --rm -v "$WT":/w --entrypoint bash "${WORV_BUILDER_IMAGE:-worv-builder:isaac6}" -lc "rm -rf ${gone[*]}" >/dev/null 2>&1
fi
git -C "$REPO" worktree prune
echo "$(date -u +%FT%TZ) removed ${#removed[@]}: ${removed[*]}; /home $(df -h /home | awk 'NR==2{print $4}') free" >> "$LOG"
