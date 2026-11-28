#!/usr/bin/env bash
# One tick per issue: a local claim taken atomically before the claim comment,
# so two ticks that start seconds apart cannot both claim an issue whose first
# comment has not reached GitHub yet. It belongs to the tick (libexec/tick-pid.sh
# finds it), and a dead holder frees it.
#   claim.sh acquire <issue>  -> ACQUIRED, or BUSY <issue> <pid> <since> (exit 1)
#   claim.sh status <issue>   -> free, HELD <issue> <pid> <since> or ORPHAN <...>
set -u
H="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
S="${HARNESS_STATE:-$H/state}"
L="$S/locks"
cmd="${1:-}"; issue="${2:-}"
[[ "$issue" =~ ^[0-9]+$ ]] || { echo "usage: claim.sh acquire|status <issue>" >&2; exit 2; }
F="$L/claim.$issue"
if [ "$cmd" = status ]; then
  pid=""
elif [ -x "$H/libexec/tick-pid.sh" ]; then
  pid=$("$H/libexec/tick-pid.sh" 2>/dev/null) || {
    echo "ERROR: no tick (an ancestor carrying HARNESS_TICK or FLEET_SESSION) above this shell; call claim.sh from the tick's own shell" >&2
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
    now=$(date -u +%FT%TZ)
    echo "$issue $pid $now" > "$F"
    # Every claim, for the supervisor's loop signal: an issue claimed by tick after tick. Cut to
    # its newest lines past a MiB.
    echo "$issue $pid $now" >> "$S/claims.log"
    if [ "$(stat -c %s "$S/claims.log" 2>/dev/null || echo 0)" -gt 1048576 ]; then
      tail -n 2000 "$S/claims.log" > "$S/claims.log.new" && mv "$S/claims.log.new" "$S/claims.log"
    fi
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
