#!/usr/bin/env bash
# Prints the rate-limit reading and exits 0 while both windows are under the point where a tick
# stands down (USAGE_TICK_STOP_PCT, two under the ceiling USAGE_CEILING_PCT unless set; the
# weekly window reads USAGE_TICK_STOP_WEEK_PCT, the same point unless set), 1 at or above it,
# and 2 when no fresh reading exists. Ticks run it before starting any build, suite, long run
# or agent.
set -u
# shellcheck source=/dev/null
. "$(dirname "${BASH_SOURCE[0]}")/instance-lib.sh"
harness_load
# shellcheck source=/dev/null
. "$HARNESS_HOME/bin/usage-lib.sh"

# A ruling posted after a tick's claim binds that tick, so name each Operator comment on the issue this tick claimed that is newer than the
# claim, once per tick. Silent, and never fatal, when no claim, no token or no answer is found.
# Anyone can comment on a public repository, so only an operator's comment counts: one by a login in
# OPERATOR_LOGINS when that is set, else by the repository's owner, a member or a collaborator.
operator_notes() {
    local state="$HARNESS_STATE" pid=$PPID depth f iss cpid ctime seen since filter out who
    [ -n "${GH_REPO:-}" ] || return 0
    if [ -n "${OPERATOR_LOGINS:-}" ]; then
        # The logins go into the jq program, so each must be a GitHub login and nothing else.
        [[ "$OPERATOR_LOGINS" =~ ^[A-Za-z0-9-]+([[:space:]]+[A-Za-z0-9-]+)*$ ]] || {
            echo "OPERATOR_LOGINS is not a space-separated list of GitHub logins; no operator notes read" >&2
            return 0
        }
        # shellcheck disable=SC2086 # split on purpose: one quoted jq string per login
        who="(.user.login | IN($(printf '"%s",' $OPERATOR_LOGINS | sed 's/,$//')))"
    else
        who='(.author_association | IN("OWNER", "MEMBER", "COLLABORATOR"))'
    fi
    for depth in 1 2 3 4 5 6; do
        for f in "$state/locks"/claim.*; do
            [ -s "$f" ] || continue
            read -r iss cpid ctime < "$f" || continue
            [ "$cpid" = "$pid" ] || continue
            seen="$state/operator-seen/$iss.$cpid"
            since=$ctime
            [ -s "$seen" ] && since=$(cat "$seen")
            [[ "$iss" =~ ^[0-9]+$ ]] && [[ "$since" =~ ^[0-9T:Z-]+$ ]] || return 0
            filter="[.[] | select($who and (.body|startswith(\"Operator\")) and .created_at > \"$since\")] | sort_by(.created_at) | .[] | \"\(.created_at) @\(.user.login): \(.body|split(\"\n\")[0]|.[0:180])\""
            out=$(GH_TOKEN="${GH_TOKEN:-}" timeout 15 gh api "repos/$GH_REPO/issues/$iss/comments?since=$since&per_page=100" -q "$filter" 2>/dev/null < /dev/null) || return 0
            [ -n "$out" ] || return 0
            mkdir -p "$state/operator-seen" && printf '%s\n' "$out" | tail -1 | cut -d' ' -f1 > "$seen"
            printf 'OPERATOR: new on #%s since %s; read these comments on the issue before you start:\n' "$iss" "$since"
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
stops="stop=${USAGE_TICK_STOP_PCT}% week_stop=${USAGE_TICK_STOP_WEEK_PCT}% ceiling=${USAGE_CEILING_PCT}%"
if under_ceiling "$USAGE_TICK_STOP_PCT" "$USAGE_TICK_STOP_WEEK_PCT"; then
    echo "five=${five}% week=${week}% $stops: go on"
    exit 0
fi
echo "five=${five}% week=${week}% $stops: start nothing, pause and end"
exit 1
