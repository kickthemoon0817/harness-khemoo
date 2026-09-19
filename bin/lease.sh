#!/usr/bin/env bash
# Atomic GPU lease. The ONLY sanctioned way to take, check or drop a lease; a bare
# write to a lease file is a bug.
#
# The card holds up to GPU_SLOTS kit sessions at once. Each slot is its own lease
# file, warm container and DDS domain, so sibling sessions never share a
# container or a ROS graph:
#   slot 1: state/locks/resource.lease    container worv-iter    domain 77
#   slot K: state/locks/resource.lease.K  container worv-iter-K  domain 76+K
#
#   lease.sh acquire <issue> [domain]
#       -> ACQUIRED slot=K file=<lease> container=<name> domain=<id>
#          export MANURE_GATE_LEASE_FILE=<lease> WORV_ITER_CONTAINER=<name> ROS_DOMAIN_ID=<id>
#          or BUSY <holders>; exit 0/1. A stated domain applies to slot 1 only.
#       Run every iter.sh, gate and kit command of the tick under that export line.
#   lease.sh release <issue>   -> drops the slot this tick's pid holds
#   lease.sh status            -> every slot's holder and whether its pid is alive
set -u
H="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
: "${GPU_SLOTS:=3}"
LOCKS="$H/state/locks"; K="$LOCKS/resource.lock"; G=/tmp/isaac-cppmig-gpu-runtime.lock
pid=$("$H/bin/tick-pid.sh" 2>/dev/null || echo "$PPID")
slot_file() { [ "$1" -eq 1 ] && echo "$LOCKS/resource.lease" || echo "$LOCKS/resource.lease.$1"; }
slot_container() { [ "$1" -eq 1 ] && echo worv-iter || echo "worv-iter-$1"; }
case "${1:-}" in
  acquire)
    issue="${2:?issue number}"
    stated="${3:-}"
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
    echo "$issue $pid $(date -u +%FT%TZ) domain=$d" > "$f"
    echo "ACQUIRED slot=$held file=$f container=$c domain=$d"
    echo "export MANURE_GATE_LEASE_FILE=$f WORV_ITER_CONTAINER=$c ROS_DOMAIN_ID=$d"
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
  *) echo "usage: lease.sh acquire <issue> [domain] | release <issue> | status" >&2; exit 2;;
esac
