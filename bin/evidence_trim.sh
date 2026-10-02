#!/usr/bin/env bash
# Cron entry for bin/evidence_trim.py, which keeps the evidence folder from filling
# the disk with what a packet no longer needs (its docstring has the rules).
#   evidence_trim.sh           dry run
#   evidence_trim.sh --apply   acts, and logs to state/logs/evidence_trim.log
set -u
H="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
# Cron gives no environment: take the harness token and PATH the way tick.sh does.
export HARNESS_HOME="$H"
[ -r "$H/config/harness.env" ] && set -a && . "$H/config/harness.env" && set +a
export PATH="${HARNESS_PATH:-$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin}"
LOG="$H/state/logs/evidence_trim.log"
out=$(nice -n 19 ionice -c3 python3 "$H/bin/evidence_trim.py" "$@") || exit 1
echo "$out"
[ "${1:-}" = "--apply" ] && echo "$(date -u +%FT%TZ) $(echo "$out" | tr '\n' ';'); /home $(df -h /home | awk 'NR==2{print $4}') free" >> "$LOG"
exit 0
