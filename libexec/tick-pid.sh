#!/usr/bin/env bash
# Print the PID of the enclosing tick. The runner records each tick it starts in
# $HARNESS_STATE/locks/slot<n>.tick as `pid starttime ...`; the tick is the first ancestor of the
# caller whose pid and start time, field 22 of /proc/<pid>/stat, a slot file records. On a fleet
# device, where the caller's own environment carries FLEET_SESSION, it is the ancestor that fleetd
# started. Only /proc/<pid>/stat is read: an agent CLI that marks itself non-dumpable, as the release
# Codex CLI does, keeps its environment and descriptors private. An operator's own agent CLI is
# recorded nowhere, so it takes no claim and no merge lock.
set -u
H="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
state="${HARNESS_STATE:-$H/state}"
# Sets ppid, comm and start from /proc/$1/stat; the name in parentheses may hold spaces and
# parentheses, so it runs to the last `)`.
read_stat() {
    local stat rest
    local -a fields
    ppid=1 comm="" start=""
    stat=$(cat "/proc/$1/stat" 2>/dev/null) || return 1
    rest=${stat##*) }
    comm=${stat#*(}; comm=${comm%) *}
    read -ra fields <<<"$rest"
    ppid=${fields[1]:-1} start=${fields[19]:-}
}
p=${1:-$PPID}
if [ -n "${FLEET_SESSION:-}" ]; then
    while [ "$p" -gt 1 ] && read_stat "$p"; do
        child=$p
        p=$ppid
        read_stat "$p" || break
        if [ "$comm" = fleetd ]; then echo "$child"; exit 0; fi
    done
    echo "ERROR: FLEET_SESSION is set, but no ancestor was started by fleetd" >&2; exit 1
fi
declare -A recorded=()
for f in "$state"/locks/slot*.tick; do
    [ -f "$f" ] && [ -O "$f" ] || continue
    read -r rpid rstart _ < "$f" 2>/dev/null || continue
    [[ "$rpid" =~ ^[0-9]+$ ]] && [[ "$rstart" =~ ^[0-9]+$ ]] && recorded[$rpid]=$rstart
done
while [ "$p" -gt 1 ] && read_stat "$p"; do
    if [ -n "${recorded[$p]:-}" ] && [ "${recorded[$p]}" = "$start" ]; then echo "$p"; exit 0; fi
    p=$ppid
done
echo "ERROR: no ancestor is a tick that $state/locks/slot*.tick records" >&2; exit 1
