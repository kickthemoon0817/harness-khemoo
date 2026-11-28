#!/usr/bin/env bash
# Prints the usage reading of this tick's agent and exits 0 while each of its windows is under the
# point where a tick stands down (USAGE_TICK_STOP_PCT, two under the ceiling USAGE_CEILING_PCT
# unless set, for a window under a day; USAGE_TICK_STOP_LONG_PCT, the same point unless set, for a
# longer one), 1 at or above one, and 2 when no fresh reading exists (`harness usage check`). The
# agent is the one HARNESS_TICK names, else the fleet adapter that started the tick, else
# TICK_AGENT. Ticks run it before starting any build, suite, long run or agent.
set -u
# shellcheck source=/dev/null
. "$(dirname "${BASH_SOURCE[0]}")/../lib/instance.sh"
harness_load

# A ruling posted after a tick's claim binds that tick, so name each Operator comment on the issue this tick claimed that is newer than the
# claim, once per tick. Silent, and never fatal, when no claim, no token or no answer is found.
operator_notes() {
    local state="$HARNESS_STATE" pid=$PPID depth f iss cpid ctime seen since filter out
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

agent=${FLEET_ADAPTER:-${TICK_AGENT:-claude}}
# The marker's fields after its state are fixed, and the state itself may hold spaces.
case ${HARNESS_TICK:-} in
    "state=$HARNESS_STATE slot="*) agent=${HARNESS_TICK% model=*}; agent=${agent##* agent=} ;;
esac
# harness.env is sourced, not exported, and harness usage reads its settings from the environment.
vars=$(compgen -v USAGE_)
# shellcheck disable=SC2086,SC2163
[ -n "$vars" ] && export $vars
exec "$HARNESS_BIN" usage check --agent "$agent"
