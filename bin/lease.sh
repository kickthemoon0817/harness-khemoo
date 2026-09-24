#!/usr/bin/env bash
# Atomic GPU lease. The ONLY sanctioned way to take, check or drop a lease; a bare
# write to a lease file is a bug.
#
# The card holds up to GPU_SLOTS kit sessions at once. Each slot is its own lease
# file, warm container and DDS domain, so sibling sessions never share a
# container or a ROS graph:
#   slot 1: state/locks/resource.lease    container worv-iter    domain 77
#   slot K: state/locks/resource.lease.K  container worv-iter-K  domain 76+K
# Kits booting together against one shader and cooking cache stall on it, so
# slot K>1 binds its own cache root /tmp/isaac-sim/cache-slotK, seeded once
# from slot 1's warm cache on the first acquire that finds it missing.
#
#   lease.sh acquire <issue> [domain]
#       -> ACQUIRED slot=K file=<lease> container=<name> domain=<id>
#          export MANURE_GATE_LEASE_FILE=<lease> WORV_ITER_CONTAINER=<name> ROS_DOMAIN_ID=<id>
#                 [WORV_ITER_CACHE_ROOT=<root>]   (slots 2+ only)
#          or BUSY <holders>; exit 0/1. A stated domain applies to slot 1 only.
#       Run every iter.sh, gate and kit command of the tick under that export line.
#   lease.sh acquire <issue> --all
#       -> the whole card, for a capture that cannot share it (a baseline). Each call reserves every
#          free slot for this tick, so siblings cannot take a slot it is waiting on, and prints
#          RESERVING held=<n>/<N> <holders> (exit 1) until the last sibling releases; then ACQUIRED
#          and the export line of the slot it runs in (the one it already held, else slot 1).
#   lease.sh release <issue>   -> drops every slot this tick's pid holds
#   lease.sh status            -> every slot's holder and whether its pid is alive
set -u
H="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
: "${GPU_SLOTS:=3}"
LOCKS="$H/state/locks"; K="$LOCKS/resource.lock"; G=/tmp/isaac-cppmig-gpu-runtime.lock
# Explicit task wrappers must be live ancestors, never arbitrary lease claimants.
if [[ -n "${CODEX_TASK_WRAPPER_PID:-}" ]]; then
  pid="$CODEX_TASK_WRAPPER_PID"
  [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null || {
    echo "ERROR: task wrapper PID is not live" >&2; exit 2;
  }
  ancestor=$PPID
  while [[ "$ancestor" =~ ^[0-9]+$ ]] && (( ancestor > 1 )) && [[ "$ancestor" != "$pid" ]]; do
    ancestor=$(sed -n 's/.*) [A-Za-z] \([0-9][0-9]*\) .*/\1/p' "/proc/$ancestor/stat" 2>/dev/null)
  done
  [[ "$ancestor" == "$pid" ]] || {
    echo "ERROR: task wrapper PID is not an ancestor" >&2; exit 2;
  }
else
  pid=$("$H/bin/tick-pid.sh" 2>/dev/null || echo "$PPID")
fi
slot_file() { [ "$1" -eq 1 ] && echo "$LOCKS/resource.lease" || echo "$LOCKS/resource.lease.$1"; }
slot_container() { [ "$1" -eq 1 ] && echo worv-iter || echo "worv-iter-$1"; }
CACHE_BASE=/tmp/isaac-sim
SEED_IMAGE="${WORV_BUILDER_IMAGE:-worv-builder:isaac6}"
slot_cache() { [ "$1" -eq 1 ] || echo "$CACHE_BASE/cache-slot$1"; }
# The caches are root-owned (docker creates the bind sources), so the copy runs
# in a container. A partial copy is staged under .seeding and renamed, so a
# killed seed is retried rather than taken for a warm root.
seed_cache() {
  local root="$1" name
  [ -d "$root" ] && return 0
  name=$(basename "$root")
  docker run --rm --runtime=runc -v "$CACHE_BASE":/c --entrypoint bash "$SEED_IMAGE" -lc \
    "rm -rf /c/$name.seeding && mkdir -p /c/cache/kit /c/cache/ov /c/cache/glcache /c/cache/computecache \
     && cp -a /c/cache /c/$name.seeding && mv /c/$name.seeding /c/$name" >&2
}
case "${1:-}" in
  acquire)
    issue="${2:?issue number}"
    stated="${3:-}"
    if [ "$stated" = "--all" ]; then
      exec 8>"$K"; flock 8; exec 9>"$G"; flock 9
      mine=0; primary=""; others=""
      for s in $(seq 1 "$GPU_SLOTS"); do
        f=$(slot_file $s); hp=""
        [ -s "$f" ] && hp=$(awk '{print $2}' "$f")
        if [ -n "$hp" ] && [ "$hp" != "$pid" ] && kill -0 "$hp" 2>/dev/null; then
          others="$others[$(cat "$f")] "; continue
        fi
        # Free, dead or already ours: reserve it for this tick.
        [ "$hp" = "$pid" ] && [ -z "$primary" ] && primary=$s
        [ "$hp" = "$pid" ] || echo "$issue $pid $(date -u +%FT%TZ) domain=$(( 76 + s )) reserved-for-all" > "$f"
        mine=$(( mine + 1 ))
      done
      if [ -n "$others" ]; then
        echo "RESERVING held=$mine/$GPU_SLOTS waiting for $others"; exit 1
      fi
      [ -z "$primary" ] && primary=1
      f=$(slot_file $primary); c=$(slot_container $primary); d=$(( 76 + primary ))
      cr=$(slot_cache $primary)
      if [ -n "$cr" ] && ! seed_cache "$cr"; then
        echo "ERROR: could not seed $cr from $CACHE_BASE/cache" >&2; exit 1
      fi
      echo "$issue $pid $(date -u +%FT%TZ) domain=$d exclusive" > "$f"
      echo "ACQUIRED slot=$primary file=$f container=$c domain=$d exclusive=all${cr:+ cache=$cr}"
      echo "export MANURE_GATE_LEASE_FILE=$f WORV_ITER_CONTAINER=$c ROS_DOMAIN_ID=$d${cr:+ WORV_ITER_CACHE_ROOT=$cr}"
      exit 0
    fi
    exec 8>"$K"; flock 8; exec 9>"$G"; flock 9
    held=""
    s=1
    while [ "$s" -le "$GPU_SLOTS" ]; do
      f=$(slot_file $s)
      if [ -s "$f" ]; then
        hp=$(awk '{print $2}' "$f")
        if kill -0 "$hp" 2>/dev/null; then
          if [ "$hp" = "$pid" ]; then held=$s; break; fi
          s=$(( s + 1 )); continue
        fi
      fi
      held=$s; break
    done
    if [ -z "$held" ]; then
      echo "BUSY $(for s in $(seq 1 "$GPU_SLOTS"); do printf '[%s] ' "$(cat "$(slot_file $s)" 2>/dev/null)"; done)"; exit 1
    fi
    f=$(slot_file $held); c=$(slot_container $held)
    if [ "$held" -eq 1 ] && [ -n "$stated" ]; then d=$stated; else d=$(( 76 + held )); fi
    cr=$(slot_cache $held)
    if [ -n "$cr" ] && ! seed_cache "$cr"; then
      echo "ERROR: could not seed $cr from $CACHE_BASE/cache" >&2; exit 1
    fi
    echo "$issue $pid $(date -u +%FT%TZ) domain=$d" > "$f"
    echo "ACQUIRED slot=$held file=$f container=$c domain=$d${cr:+ cache=$cr}"
    echo "export MANURE_GATE_LEASE_FILE=$f WORV_ITER_CONTAINER=$c ROS_DOMAIN_ID=$d${cr:+ WORV_ITER_CACHE_ROOT=$cr}"
    ;;
  release)
    exec 8>"$K"; flock 8
    out="NOT-HOLDER"
    for s in $(seq 1 "$GPU_SLOTS"); do
      f=$(slot_file $s)
      if [ -s "$f" ] && [ "$(awk '{print $2}' "$f")" = "$pid" ]; then rm -f "$f"; out="RELEASED slot=$s"; fi
    done
    echo "$out"
    ;;
  status)
    for s in $(seq 1 "$GPU_SLOTS"); do
      f=$(slot_file $s)
      if [ -s "$f" ]; then hp=$(awk '{print $2}' "$f"); kill -0 "$hp" 2>/dev/null && echo "slot $s HELD $(cat "$f") (alive)" || echo "slot $s ORPHAN $(cat "$f") (pid dead)"
      else echo "slot $s free"; fi
    done
    ;;
  *) echo "usage: lease.sh acquire <issue> [domain|--all] | release <issue> | status" >&2; exit 2;;
esac
