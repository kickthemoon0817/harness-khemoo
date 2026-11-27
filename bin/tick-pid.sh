#!/usr/bin/env bash
# Print the PID of the enclosing long-lived tick process: the nearest ancestor of the caller whose
# argv[0] or executable has a base name in TICK_AGENT_EXES (space-separated, default claude), the
# agent CLIs a tick runs as, locally or under fleetd. Only the agent process itself qualifies: a
# tool shell whose command text merely names the agent is a shell.
# shellcheck disable=SC2086 # word splitting collapses the list's spacing, so an empty name never matches
exes=" $(echo ${TICK_AGENT_EXES:-claude}) "
p=${1:-$PPID}
while [ "$p" -gt 1 ]; do
    if [ -r "/proc/$p/cmdline" ]; then
        a0=$(tr '\0' '\n' < "/proc/$p/cmdline" | sed -n 1p)
        exe=$(readlink "/proc/$p/exe" 2>/dev/null)
        case "$exes" in *" ${a0##*/} "*|*" ${exe##*/} "*) echo "$p"; exit 0 ;; esac
    fi
    p=$(awk '{print $4}' "/proc/$p/stat" 2>/dev/null || echo 1)
done
echo "ERROR: no ancestor in TICK_AGENT_EXES ($exes)" >&2; exit 1
