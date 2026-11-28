#!/usr/bin/env bash
# Print the PID of the enclosing long-lived tick process: the nearest ancestor of the caller that
# matches an entry of TICK_AGENT_EXES, the agent CLIs a tick runs as, locally or under fleetd.
# Entries are space-separated, each `exe` or `exe:arg`: the base name of the process's argv[0] or
# executable is `exe` and, with `:arg`, its argv[1] is exactly `arg`. The first argument is what
# tells an unattended tick (`claude -p`) from an operator's interactive `claude`, which must not
# take claims or the merge lock. Only the agent process itself qualifies: a tool shell whose
# command text merely names the agent is a shell.
read -ra agents <<< "${TICK_AGENT_EXES:-claude:-p claude-code:-p node:-p}"
p=${1:-$PPID}
while [ "$p" -gt 1 ]; do
    if [ -r "/proc/$p/cmdline" ]; then
        a0='' a1=''
        { IFS= read -rd '' a0; IFS= read -rd '' a1; } < "/proc/$p/cmdline"
        exe=$(readlink "/proc/$p/exe" 2>/dev/null)
        for agent in "${agents[@]}"; do
            name=${agent%%:*}
            [ -n "$name" ] || continue
            case "$agent" in *:*) [ "$a1" = "${agent#*:}" ] || continue ;; esac
            if [ "$name" = "${a0##*/}" ] || [ "$name" = "${exe##*/}" ]; then echo "$p"; exit 0; fi
        done
    fi
    p=$(awk '{print $4}' "/proc/$p/stat" 2>/dev/null || echo 1)
done
echo "ERROR: no ancestor in TICK_AGENT_EXES (${agents[*]})" >&2; exit 1
