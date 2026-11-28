#!/usr/bin/env bash
# Prints the usage reading of this tick's agent and exits 0 while each of its windows is under its
# stand-down point (`harness usage check`, docs/INSTANCE.md), 1 at or above one, and 2 when no fresh
# reading exists. The agent is the one HARNESS_TICK names, else the fleet adapter that started the
# tick, else TICK_AGENT. Ticks run it before starting any build, suite, long run or agent.
set -u
# shellcheck source=/dev/null
. "$(dirname "${BASH_SOURCE[0]}")/../lib/instance.sh"
harness_load

# Sets iss, cpid and ctime from the claim of the tick this runs under: the first claim file whose
# pid is the nearest of this script's ancestors to hold one. Returns 1 when no claim is found.
find_claim() {
    local pid=$PPID f
    for _ in 1 2 3 4 5 6; do
        for f in "$HARNESS_STATE/locks"/claim.*; do
            [ -s "$f" ] || continue
            read -r iss cpid ctime < "$f" || continue
            [ "$cpid" = "$pid" ] && return 0
        done
        pid=$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d ' ')
        [ -n "$pid" ] && [ "$pid" -gt 1 ] || return 1
    done
    return 1
}

# A ruling posted after a tick's claim binds that tick, so name each operator's Operator comment on
# the issue this tick claimed that is newer than the claim, once per tick. Anyone can comment on a
# public repository: `harness operator notes` keeps only an operator's (docs/INSTANCE.md,
# OPERATOR_LOGINS). Never fatal; silent when no token or no answer is found.
operator_notes() {
    local state="$HARNESS_STATE" seen since page out
    [ -n "${GH_REPO:-}" ] || return 0
    seen="$state/operator-seen/$iss.$cpid"
    since=$ctime
    [ -s "$seen" ] && since=$(cat "$seen")
    [[ "$iss" =~ ^[0-9]+$ ]] && [[ "$since" =~ ^[0-9T:Z-]+$ ]] || return 0
    # Every page: a run of comments from anyone must not push a ruling off the first. Each comment is
    # cut to its first line here, so huge ones cannot crowd the others out of what is read.
    page=$(GH_TOKEN="${GH_TOKEN:-}" timeout 15 gh api --paginate "repos/$GH_REPO/issues/$iss/comments?since=$since&per_page=100" \
        -q '.[] | {created_at, author_association, issue_url, user: {login: .user.login}, body: ((.body // "") | split("\n")[0] | .[0:400])}' \
        2>/dev/null < /dev/null) || return 0
    out=$(printf '%s' "$page" | OPERATOR_LOGINS="${OPERATOR_LOGINS:-}" "$HARNESS_BIN" operator notes --since "$since") || return 0
    [ -n "$out" ] || return 0
    mkdir -p "$state/operator-seen" && printf '%s\n' "$out" | tail -1 | cut -d' ' -f1 > "$seen"
    printf 'OPERATOR: new on #%s since %s; read these comments on the issue before you start:\n' "$iss" "$since"
    printf '%s\n' "$out" | sed 's/^/  /'
}

# The supervisor's notes on this tick's issue, and a stop it named this tick in, each once per tick
# (`harness directive notes`), with SUPERVISOR=1 only. Never fatal.
supervisor_notes() {
    [ "${SUPERVISOR:-}" = 1 ] && [[ "$iss" =~ ^[0-9]+$ ]] && [[ "$cpid" =~ ^[0-9]+$ ]] || return 0
    (
        vars=$(compgen -v SUPERVISOR_)
        # shellcheck disable=SC2086,SC2163
        [ -n "$vars" ] && export $vars
        export TICK_MODELS TICK_AGENT MAX_SLOTS TICK_WIDTH
        exec "$HARNESS_BIN" directive notes --issue "$iss" --tick "$cpid"
    ) 2>/dev/null
}
if find_claim; then
    operator_notes || true
    supervisor_notes || true
fi

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
