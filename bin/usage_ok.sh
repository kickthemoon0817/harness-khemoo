#!/usr/bin/env bash
# Prints the rate-limit reading and exits 0 while both windows are under the point where a tick
# stands down (USAGE_TICK_STOP_PCT, two under the ceiling USAGE_CEILING_PCT), 1 at or above it,
# and 2 when no fresh reading exists. Ticks run it before starting any build, suite, kit, render
# queue or agent.
set -u
HARNESS_HOME="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
# shellcheck source=/dev/null
[ -r "$HARNESS_HOME/config/harness.env" ] && . "$HARNESS_HOME/config/harness.env"
# shellcheck source=/dev/null
. "$HARNESS_HOME/bin/usage-lib.sh"

read_usage
if [ -z "$five" ] || [ -z "$week" ]; then
    echo "usage unknown: no fresh reading in $USAGE_CACHE; start nothing"
    exit 2
fi
if under_ceiling "$USAGE_TICK_STOP_PCT"; then
    echo "five=${five}% week=${week}% stop=${USAGE_TICK_STOP_PCT}% ceiling=${USAGE_CEILING_PCT}%: go on"
    exit 0
fi
echo "five=${five}% week=${week}% stop=${USAGE_TICK_STOP_PCT}% ceiling=${USAGE_CEILING_PCT}%: start nothing, pause and end"
exit 1
