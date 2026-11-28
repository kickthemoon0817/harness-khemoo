#!/usr/bin/env bash
# A tick's compile: picks a lane with bin/build_host.sh, runs `tools/dev/iter.sh build <args>` from the
# caller's working directory on it, and exits with iter.sh's code. Equivalent to
# `eval "$(bin/build_host.sh)"; tools/dev/iter.sh build <args>`, except that a build still waiting for
# its lane's compile lock moves to another lane once one is free.
#
#   build.sh <iter.sh build args…>   (`--test`, `--fast`, `--suite` and the rest go to iter.sh as given)
#
# Every BUILD_REBALANCE_S (15) seconds while its iter.sh waits for the lane's lock (iter.sh blocks in
# `flock <fd>` on WORV_DEV_BUILD_LOCK), it asks `build_host.sh --free-only` for a lane whose lock is
# free with no waiter, among the hosts build_host.sh does not leave out. Given another lane, it stops
# its iter.sh's processes, checks while they are stopped that the lock is still not theirs, ends them,
# and runs again on that lane, saying `build.sh: moved from <lane> to <lane> after N s waiting` on
# stderr. A build that holds its lock, and so has begun compiling, is never stopped. A device build
# (`--device`) runs on the export line the caller holds from its lease, untouched and never moved.
#
# iter.sh runs in the caller's process group, so a caller's `timeout` reaches it as it would a bare
# iter.sh, and a timeout covers the time spent waiting as well as the compile; a signal sent to
# build.sh alone is passed on to its iter.sh.
#
# Each move, and each build when it ends, is a line in state/logs/build.log:
#   <time> build.sh: moved from <lane> to <lane> after N s waiting (<cwd>)
#   <time> build start=<time> lane=<lane> wait_s=N compile_s=N|- exit=N moves=N device=0|1 cwd=<cwd> args=<args>
# wait_s runs from the start until iter.sh holds its lane's lock (to the end when it never does), and
# compile_s from then until iter.sh lets the lock go; a lane is named from its lock file (`local` for
# worv-dev-build.lock, <name> for worv-dev-build-remote-<name>.lock).
set -u
H="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
S="${HARNESS_STATE:-$H/state}"
: "${BUILD_REBALANCE_S:=15}"
LOG="$S/logs/build.log"
ITER="tools/dev/iter.sh"
[ -x "$ITER" ] || { echo "build.sh: no $ITER in $PWD; run it from the worktree to build" >&2; exit 2; }
device=0
for a in "$@"; do [ "$a" = --device ] && device=1; done

lane_lock() { echo "${WORV_DEV_BUILD_LOCK:-/tmp/worv-dev-build.lock}"; }
lane_name() {
  local b=${1##*/}
  case "$b" in
    worv-dev-build.lock) echo local ;;
    worv-dev-build-remote-*.lock) b=${b#worv-dev-build-remote-}; echo "${b%.lock}" ;;
    *) echo "$b" ;;
  esac
}
log() { mkdir -p "${LOG%/*}" 2>/dev/null; echo "$(date -u +%FT%TZ) $*" >> "$LOG" 2>/dev/null; }
# The pids of process $1 and every process under it.
tree_pids() {
  ps -e -o pid=,ppid= 2>/dev/null | awk -v root="$1" '
    { parent[$1] = $2 }
    END {
      mine[root] = 1
      do { grown = 0; for (p in parent) if (!(p in mine) && (parent[p] in mine)) { mine[p] = 1; grown = 1 } } while (grown)
      for (p in mine) print p
    }'
}
# The pid of the `flock <fd>` under iter.sh that waits for the lane's lock, if one does.
waiting_flock() {
  local p
  for p in $(tree_pids "$1"); do
    [ "$p" = "$1" ] && continue
    [[ "$(tr '\0' ' ' < "/proc/$p/cmdline" 2>/dev/null)" =~ ^flock\ [0-9]+\ $ ]] && { echo "$p"; return 0; }
  done
  return 1
}
state_of() { sed 's/.*) //' "/proc/$1/stat" 2>/dev/null | cut -d' ' -f1; }
# The waiting flock's pid while the build has not yet taken its lane's lock; fails once it has.
still_waiting() { [ -z "$lock_t" ] && waiting_flock "$ipid"; }
gone() { ! kill -0 "$1" 2>/dev/null || [ "$(state_of "$1")" = Z ]; }
all_stopped() { local p s; for p in "$@"; do gone "$p" && continue; s=$(state_of "$p"); [ "$s" = T ] || [ "$s" = t ] || return 1; done; }
# iter.sh has the lock: it writes its pid into the lock file once it holds it.
holds_lock() { local w p; read -r w p _ < "$cur_lock" 2>/dev/null && [ "$w" = pid ] && [ "${p%,}" = "$ipid" ]; }
# iter.sh still has the lock file open: it closes it when its compile ends.
lock_open() { ls -l "/proc/$ipid/fd" 2>/dev/null | grep -qF -- "-> $(readlink -f "$cur_lock")"; }
# The kernel granted the lock to pid $1 (its line in /proc/locks names it and the file's inode).
granted_to() {
  local ino; ino=$(stat -c %i "$cur_lock" 2>/dev/null) || return 1
  awk -v p="$1" -v i="$ino" '$2 == "FLOCK" && $5 == p && $6 ~ (":" i "$") { found = 1 } END { exit !found }' /proc/locks
}
# Stops iter.sh's processes and, if its flock still waits for the lock while they are stopped, ends
# them and returns 0; otherwise lets them go on and returns 1. A stopped flock cannot take the lock.
end_if_waiting() {
  local fl=$1 pids i
  pids=$(tree_pids "$ipid")
  kill -STOP $pids 2>/dev/null
  for i in $(seq 1 20); do all_stopped $pids && break; sleep 0.1; done
  if ! all_stopped $pids || gone "$fl" || granted_to "$fl" || holds_lock; then kill -CONT $pids 2>/dev/null; return 1; fi
  kill -TERM $pids 2>/dev/null; kill -CONT $pids 2>/dev/null
  for i in $(seq 1 50); do gone "$ipid" && break; sleep 0.1; done
  gone "$ipid" || kill -KILL $(tree_pids "$ipid") 2>/dev/null
  wait "$ipid" 2>/dev/null
  return 0
}
start_t=$(date +%s); start_iso=$(date -u -d "@$start_t" +%FT%TZ)
lock_t=""; release_t=""; moves=0; ipid=""
finish() {
  local rc=$1 end_t; shift; end_t=$(date +%s)
  [ -n "$lock_t" ] && [ -z "$release_t" ] && release_t=$end_t
  log "build start=$start_iso lane=$(lane_name "$cur_lock") wait_s=$(( ${lock_t:-$end_t} - start_t ))" \
    "compile_s=$([ -n "$lock_t" ] && echo $(( release_t - lock_t )) || echo -) exit=$rc moves=$moves device=$device cwd=$PWD args=$*"
  exit "$rc"
}
on_signal() {
  trap - TERM INT HUP
  [ -n "$ipid" ] && ! gone "$ipid" && kill -TERM $(tree_pids "$ipid") 2>/dev/null
  [ -n "$ipid" ] && wait "$ipid" 2>/dev/null
  finish "$1" "${ARGS[@]}"
}
ARGS=("$@")
trap 'on_signal 143' TERM; trap 'on_signal 130' INT; trap 'on_signal 129' HUP

if [ "$device" = 0 ]; then
  line=$("$H/bin/build_host.sh") || { rc=$?; cur_lock=none; finish "$rc" "$@"; }
  eval "$line"
fi
cur_lock=$(lane_lock)
"$ITER" build "$@" & ipid=$!
lane_t=$start_t; checked_t=$start_t
while ! gone "$ipid"; do
  sleep 1
  now=$(date +%s)
  if [ -z "$lock_t" ]; then
    holds_lock && lock_t=$now
  elif [ -z "$release_t" ] && ! gone "$ipid" && ! lock_open; then
    release_t=$now
  fi
  [ "$device" = 0 ] && [ $(( now - checked_t )) -ge "$BUILD_REBALANCE_S" ] || continue
  checked_t=$now
  fl=$(still_waiting) || continue
  next=$("$H/bin/build_host.sh" --free-only) || continue
  next_lock=$(sed -n 's/.* WORV_DEV_BUILD_LOCK=\([^ ]*\).*/\1/p' <<< "$next")
  [ -n "$next_lock" ] && [ "$next_lock" != "$cur_lock" ] || continue
  end_if_waiting "$fl" || continue
  msg="build.sh: moved from $(lane_name "$cur_lock") to $(lane_name "$next_lock") after $(( now - lane_t )) s waiting"
  echo "$msg" >&2; log "$msg ($PWD)"
  moves=$(( moves + 1 ))
  eval "$next"
  cur_lock=$(lane_lock); lane_t=$(date +%s)
  "$ITER" build "$@" & ipid=$!
done
wait "$ipid"; rc=$?
finish "$rc" "$@"
