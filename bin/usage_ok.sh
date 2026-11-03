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

# A ruling posted after a tick's claim binds that tick (prompts/runbook-plan.md, "Read the issue's
# comments again"). Name each Operator comment on the issue this tick claimed that is newer than the
# claim, once per tick. Silent, and never fatal, when no claim, no token or no answer is found.
operator_notes() {
    local state="${HARNESS_STATE:-$HARNESS_HOME/state}" pid=$PPID depth f iss cpid ctime seen since filter out
    [ -n "${GH_REPO:-}" ] || return 0
    for depth in 1 2 3 4 5 6; do
        for f in "$state/locks"/claim.*; do
            [ -s "$f" ] || continue
            read -r iss cpid ctime < "$f" || continue
            [ "$cpid" = "$pid" ] || continue
            seen="$state/operator-seen/$iss.$cpid"
            since=$ctime
            [ -s "$seen" ] && since=$(cat "$seen")
            [[ "$iss" =~ ^[0-9]+$ ]] && [[ "$since" =~ ^[0-9T:Z-]+$ ]] || return 0
            filter="[.[] | select((.body|startswith(\"Operator\")) and .created_at > \"$since\")] | sort_by(.created_at) | .[] | \"\(.created_at) \(.body|split(\"\n\")[0]|.[0:180])\""
            out=$(GH_TOKEN="${GH_TOKEN:-}" timeout 15 gh api "repos/$GH_REPO/issues/$iss/comments?since=$since&per_page=100" -q "$filter" 2>/dev/null < /dev/null) || return 0
            [ -n "$out" ] || return 0
            mkdir -p "$state/operator-seen" && printf '%s\n' "$out" | tail -1 | cut -d' ' -f1 > "$seen"
            printf 'OPERATOR: new on #%s since %s; read the issue comments before you start:\n' "$iss" "$since"
            printf '%s\n' "$out" | sed 's/^/  /'
            return 0
        done
        pid=$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d ' ')
        [ -n "$pid" ] && [ "$pid" -gt 1 ] || return 0
    done
}
operator_notes || true

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
