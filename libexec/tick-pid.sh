#!/usr/bin/env bash
# Print the PID of the enclosing tick. The runner starts every tick with HARNESS_TICK, whose state=
# names this harness's state directory, and fleetd starts every agent with FLEET_SESSION; whatever
# the agent starts inherits them. The tick is the nearest ancestor of the caller that carries one
# while its parent does not: the agent CLI the runner or fleetd started, or the node shim that runs
# it, never a tool shell under it. An operator's own agent CLI carries neither, so it takes no claim
# and no merge lock.
set -u
H="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
state="${HARNESS_STATE:-$H/state}"
marked() {  # $1 = pid
    local entry
    [ -r "/proc/$1/environ" ] || return 1
    # Bounded: Linux caps a process's argv and environment together at a few MiB.
    while IFS= read -r -d '' entry; do
        case $entry in
            "HARNESS_TICK=state=$state slot="*|FLEET_SESSION=?*) return 0 ;;
        esac
    done < <(head -c 4194304 "/proc/$1/environ" 2>/dev/null)
    return 1
}
parent_of() {  # $1 = pid; the name in /proc/<pid>/stat may hold spaces, so fields count from its `)`
    local stat ppid
    stat=$(cat "/proc/$1/stat" 2>/dev/null) || { echo 1; return; }
    read -r _ ppid _ <<<"${stat##*) }"
    echo "${ppid:-1}"
}
p=${1:-$PPID}
tick=""
while [ "$p" -gt 1 ]; do
    if marked "$p"; then tick=$p
    elif [ -n "$tick" ]; then break; fi
    p=$(parent_of "$p")
done
if [ -n "$tick" ]; then echo "$tick"; exit 0; fi
echo "ERROR: no ancestor carries HARNESS_TICK for $state or FLEET_SESSION" >&2; exit 1
