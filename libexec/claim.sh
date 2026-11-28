#!/usr/bin/env bash
# One tick per issue: a local claim taken atomically before the claim comment,
# so two ticks that start seconds apart cannot both claim an issue whose first
# comment has not reached GitHub yet. It belongs to the tick (libexec/tick-pid.sh
# finds it), and a dead holder frees it.
#   claim.sh acquire <issue>  -> ACQUIRED, or BUSY <issue> <pid> <since> (exit 1)
#   claim.sh status <issue>   -> free, HELD <issue> <pid> <since> or ORPHAN <...>
set -u
H="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
L="${HARNESS_STATE:-$H/state}/locks"
cmd="${1:-}"; issue="${2:-}"
[[ "$issue" =~ ^[0-9]+$ ]] || { echo "usage: claim.sh acquire|status <issue>" >&2; exit 2; }
F="$L/claim.$issue"
if [ "$cmd" = status ]; then
  pid=""
elif [ -x "$H/libexec/tick-pid.sh" ]; then
  pid=$("$H/libexec/tick-pid.sh" 2>/dev/null) || {
    echo "ERROR: no tick (an ancestor a tick file records) above this shell; call claim.sh from the tick's own shell" >&2
    exit 2
  }
else
  pid=$PPID
fi
mkdir -p "$L"
exec 8>"$L/claim.guard"; flock 8
case "$cmd" in
  acquire)
    if [ -s "$F" ]; then
      hp=$(awk '{print $2}' "$F")
      if [ "$hp" != "$pid" ] && kill -0 "$hp" 2>/dev/null; then echo "BUSY $(cat "$F")"; exit 1; fi
    fi
    echo "$issue $pid $(date -u +%FT%TZ)" > "$F"
    echo "ACQUIRED"
    ;;
  status)
    if [ -s "$F" ]; then
      hp=$(awk '{print $2}' "$F")
      if kill -0 "$hp" 2>/dev/null; then echo "HELD $(cat "$F")"; else echo "ORPHAN $(cat "$F")"; fi
    else
      echo "free"
    fi
    ;;
  *) echo "usage: claim.sh acquire|status <issue>" >&2; exit 2;;
esac
