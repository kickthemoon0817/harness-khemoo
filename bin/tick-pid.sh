#!/usr/bin/env bash
# Print the PID of the enclosing long-lived `claude -p` tick process (walks up from the
# caller). Only a process whose argv[0] is the claude binary and argv[1] is -p qualifies —
# a tool shell whose command text merely contains "claude -p" does not.
p=${1:-$PPID}
while [ "$p" -gt 1 ]; do
    if [ -r "/proc/$p/cmdline" ]; then
        a0=$(tr '\0' '\n' < "/proc/$p/cmdline" | sed -n 1p); a1=$(tr '\0' '\n' < "/proc/$p/cmdline" | sed -n 2p)
        case "$(basename "$a0")" in claude|claude-code|node) [ "$a1" = "-p" ] && { echo "$p"; exit 0; } ;; esac
    fi
    p=$(awk '{print $4}' "/proc/$p/stat" 2>/dev/null || echo 1)
done
echo "ERROR: no claude -p ancestor" >&2; exit 1
