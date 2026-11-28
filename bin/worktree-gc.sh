#!/usr/bin/env bash
# Removes harness worktrees whose work is done, so the disk does not fill with
# them. A worktree goes only when its issue is closed (or, for parent-/base-
# trees, its commit has not been WORK_BRANCH's head for KEEP_HOURS and is not one
# of its last KEEP_HEADS heads), it has no uncommitted change, every commit it
# holds is on the remote, and nothing uses it. A long run can read a parent tree
# for hours after its base moved, and a slot can land several heads in an hour,
# so a count of heads alone is too short. With WORK_BRANCH unset, no parent- or
# base-tree is removed. WORKTREE_KEEP names worktrees that are never removed.
# A worktree's issue is the <n> of its `<BRANCH_PREFIX><n>-<slug>` branch, else the
# trailing <n> of its directory name `<name>-<n>`; a worktree with neither is never removed.
# In use means a process runs inside it or the instance's hooks/worktree-busy
# names it (one directory per line, e.g. what containers mount); a busy hook that
# fails or outlasts WORKTREE_BUSY_TIMEOUT_S removes nothing, since what it would
# have named is unknown. What git cannot delete goes to hooks/worktree-remove.
#   worktree-gc.sh           dry run: prints what it would remove
#   worktree-gc.sh --apply   removes, and logs to $HARNESS_STATE/logs/worktree-gc.log
set -u
# shellcheck source=/dev/null
. "$(dirname "${BASH_SOURCE[0]}")/../lib/instance.sh"
# Cron gives no environment: every key exported, so gh reads GH_TOKEN from the config.
set -a; harness_load; set +a
export PATH="$HARNESS_INSTANCE_DIR/bin:${HARNESS_PATH:-$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin}"
WT="${WORKTREE_ROOT:?set WORKTREE_ROOT in the instance harness.env}"
REPO="${TARGET_REPO:?set TARGET_REPO in the instance harness.env}"
: "${KEEP_HEADS:=3}"; : "${KEEP_HOURS:=6}"; : "${WORK_BRANCH:=}"; : "${WORKTREE_KEEP:=}"
: "${BRANCH_PREFIX:=ai/}"; : "${WORKTREE_BUSY_TIMEOUT_S:=30}"
apply=0; [ "${1:-}" = "--apply" ] && apply=1
LOG="$HARNESS_STATE/logs/worktree-gc.log"; mkdir -p "$(dirname "$LOG")"
gh_repo=(); [ -n "${GH_REPO:-}" ] && gh_repo=(-R "$GH_REPO")
open=$(cd "$REPO" && gh issue list ${gh_repo[@]+"${gh_repo[@]}"} --state open --limit 500 --json number --jq '.[].number' 2>/dev/null) \
  || { echo "gh unavailable; nothing removed" >&2; exit 1; }
recent=""
if [ -n "$WORK_BRANCH" ]; then
  git -C "$REPO" fetch -q origin "$WORK_BRANCH" 2>/dev/null
  # The heads of the last KEEP_HOURS, the head the window opened on, and the last KEEP_HEADS.
  recent=$( { git -C "$REPO" rev-list --first-parent --since="$KEEP_HOURS hours ago" "origin/$WORK_BRANCH"
              git -C "$REPO" rev-list --first-parent -n 1 --until="$KEEP_HOURS hours ago" "origin/$WORK_BRANCH"
              git -C "$REPO" rev-list --first-parent -n "$KEEP_HEADS" "origin/$WORK_BRANCH"; } | cut -c1-8 | sort -u)
fi
hook_busy=$(HOOK_TIMEOUT_S="$WORKTREE_BUSY_TIMEOUT_S" harness_hook worktree-busy 2>/dev/null); rc=$?
[ "$rc" -eq 0 ] || [ "$rc" -eq 3 ] || { echo "hooks/worktree-busy failed (exit $rc); nothing removed" >&2; exit 1; }
busy=$( { for p in /proc/[0-9]*; do readlink "$p/cwd" 2>/dev/null; done
          printf '%s\n' "$hook_busy"
        } | grep "^$WT/" | sed "s|^$WT/||; s|/.*||" | sort -u)
removed=()
for path in "$WT"/*; do
  d=$(basename "$path"); [ -d "$path" ] && [ -e "$path/.git" ] || continue
  case " $WORKTREE_KEEP " in *" $d "*) continue;; esac
  if [[ "$d" =~ ^(parent|base)-([0-9a-f]{7,}) ]]; then
    [ -n "$WORK_BRANCH" ] || continue
    sha=${BASH_REMATCH[2]}; echo "$recent" | grep -q "^${sha:0:8}" && continue
    why="old base $sha"
  else
    ref=$(git -C "$path" symbolic-ref -q --short HEAD 2>/dev/null)
    if [[ "$ref" == "$BRANCH_PREFIX"* ]] && [[ "${ref#"$BRANCH_PREFIX"}" =~ ^([0-9]+)- ]]; then n=${BASH_REMATCH[1]}
    elif [[ "$d" =~ ^[A-Za-z][A-Za-z0-9-]*-([0-9]+)$ ]]; then n=${BASH_REMATCH[1]}
    else continue; fi
    echo "$open" | grep -qx "$n" && continue
    why="issue #$n closed"
  fi
  echo "$busy" | grep -qx "$d" && { echo "keep $d: a process runs inside it or the instance has it in use"; continue; }
  if [ -n "$(git -C "$path" status --porcelain --untracked-files=no 2>/dev/null)" ]; then echo "keep $d: uncommitted changes"; continue; fi
  if [ -z "$(git -C "$path" branch -r --contains HEAD 2>/dev/null)" ]; then echo "keep $d: HEAD is on no remote branch"; continue; fi
  echo "remove $d ($why)"; removed+=("$d")
done
[ "$apply" = 1 ] && [ "${#removed[@]}" -gt 0 ] || { [ "$apply" = 1 ] || echo "(dry run)"; exit 0; }
for d in "${removed[@]}"; do git -C "$REPO" worktree remove --force "$WT/$d" >/dev/null 2>&1; done
# The instance's hook removes what git could not (files a container left root-owned) and any copy elsewhere.
gone=(); for d in "${removed[@]}"; do gone+=("$WT/$d"); done
harness_hook worktree-remove "${gone[@]}" >/dev/null 2>&1
git -C "$REPO" worktree prune
echo "$(date -u +%FT%TZ) removed ${#removed[@]}: ${removed[*]}; $WT $(df -h "$WT" | awk 'NR==2{print $4}') free" >> "$LOG"
