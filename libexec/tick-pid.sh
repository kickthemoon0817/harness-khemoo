#!/usr/bin/env bash
# Print the PID of the enclosing tick: the first ancestor of the caller whose pid and start time,
# field 22 of /proc/<pid>/stat, a tick file records. The runner records each tick it starts in
# $HARNESS_STATE/locks/slot<n>.tick; on a fleet device, fleetd records the agent it starts in the
# file FLEET_SESSION_TICK names. A file counts only when it is a regular file of this user, not a
# symlink. Only /proc/<pid>/stat is read: an agent CLI that marks itself non-dumpable, as the
# release Codex CLI does, keeps its environment and descriptors private. An operator's own agent
# CLI is recorded nowhere, so it takes no claim and no merge lock.
set -u
H="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
state="${HARNESS_STATE:-$H/state}"
declare -A recorded=()
record() {  # $1 = a tick file
    local pid start
    [ -f "$1" ] && [ ! -h "$1" ] && [ -O "$1" ] || return 0
    read -r pid start _ < "$1" 2>/dev/null || return 0
    [[ "$pid" =~ ^[0-9]+$ ]] && [[ "$start" =~ ^[0-9]+$ ]] && recorded[$pid]=$start
}
for f in "$state"/locks/slot*.tick; do record "$f"; done
[ -n "${FLEET_SESSION_TICK:-}" ] && record "$FLEET_SESSION_TICK"
# Sets ppid and start from /proc/$1/stat; the name in parentheses may hold spaces and parentheses,
# so the fields are counted after the last `)`.
read_stat() {
    local stat
    local -a fields
    stat=$(cat "/proc/$1/stat" 2>/dev/null) || return 1
    read -ra fields <<<"${stat##*) }"
    ppid=${fields[1]:-1} start=${fields[19]:-}
}
p=${1:-$PPID}
while [ "$p" -gt 1 ] && read_stat "$p"; do
    if [ -n "${recorded[$p]:-}" ] && [ "${recorded[$p]}" = "$start" ]; then echo "$p"; exit 0; fi
    p=$ppid
done
echo "ERROR: no ancestor is a tick that $state/locks/slot*.tick or FLEET_SESSION_TICK records" >&2; exit 1
