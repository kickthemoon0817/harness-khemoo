#!/usr/bin/env bash
# The merge slot's lock (runbook-plan override 3c): one landing at a time, so a
# ready PR merges on top of the batch-mates that landed before it in the slot.
# It belongs to the tick, found the way lease.sh finds it, and a dead holder
# frees it.
#   merge-lock.sh acquire <issue>  -> ACQUIRED, or BUSY <holder> (exit 1)
#   merge-lock.sh release <issue>  -> RELEASED, or NOT-HOLDER
#   merge-lock.sh status           -> free, HELD <holder> or ORPHAN <holder>
set -u
H="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
F="$H/state/locks/merge.lock"
if [ "${1:-}" = status ]; then
  pid=""
elif [ -x "$H/bin/tick-pid.sh" ]; then
  pid=$("$H/bin/tick-pid.sh" 2>/dev/null) || {
    echo "ERROR: no claude -p tick above this shell; call merge-lock.sh from the tick's own shell" >&2
    exit 2
  }
else
  pid=$PPID
fi
exec 8>"$H/state/locks/merge.guard"; flock 8
case "${1:-}" in
  acquire)
    issue="${2:?issue number}"
    if [ -s "$F" ]; then
      hp=$(awk '{print $2}' "$F")
      if [ "$hp" != "$pid" ] && kill -0 "$hp" 2>/dev/null; then echo "BUSY $(cat "$F")"; exit 1; fi
    fi
    echo "$issue $pid $(date -u +%FT%TZ)" > "$F"
    echo "ACQUIRED"
    ;;
  release)
    if [ -s "$F" ] && [ "$(awk '{print $2}' "$F")" = "$pid" ]; then rm -f "$F"; echo "RELEASED"; else echo "NOT-HOLDER"; fi
    ;;
  status)
    if [ -s "$F" ]; then
      hp=$(awk '{print $2}' "$F")
      if kill -0 "$hp" 2>/dev/null; then echo "HELD $(cat "$F")"; else echo "ORPHAN $(cat "$F")"; fi
    else
      echo "free"
    fi
    ;;
  *) echo "usage: merge-lock.sh acquire <issue> | release <issue> | status" >&2; exit 2;;
esac
