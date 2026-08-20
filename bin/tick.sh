#!/usr/bin/env bash
# Autonomous issue harness — tick launcher.
#
# Two modes:
#   tick.sh                       launcher: decide how many ticks to run, spawn them
#   tick.sh runner <max> <delay>  internal: hold a slot and run exactly one tick
#
# The launcher is invoked from cron every couple of minutes. It sizes the pool from
# live API rate-limit headroom AND the number of unclaimed issues, then spawns enough
# runners to fill it. Every step is ordered cheapest-first so the common case (nothing
# to do) costs no API calls.
set -u

HARNESS_HOME="${HARNESS_HOME:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
# shellcheck source=/dev/null
[ -r "$HARNESS_HOME/config/harness.env" ] && . "$HARNESS_HOME/config/harness.env"

: "${HARNESS_STATE:=$HARNESS_HOME/state}"
: "${TARGET_REPO:?set TARGET_REPO in config/harness.env}"
: "${ISSUE_LABEL:=ai}"
: "${WIP_LABEL:=ai:wip}"
: "${MAX_SLOTS:=8}"
: "${STAGGER_SECONDS:=8}"
: "${HEARTBEAT_INTERVAL:=1800}"
: "${LOG_RETENTION:=200}"
: "${USAGE_CACHE:=$HOME/.claude/usage-cache.json}"
: "${USAGE_FETCH:=$HOME/.claude/scripts/usage-fetch.sh}"
: "${USAGE_MAX_AGE:=1800}"
: "${USAGE_REFRESH_AFTER:=120}"
: "${CLAUDE_BIN:=claude}"
# Set, but possibly empty: an explicit `CLAUDE_FLAGS=` means "pass no flags".
[ -z "${CLAUDE_FLAGS+x}" ] && CLAUDE_FLAGS="--dangerously-skip-permissions"
export PATH="${HARNESS_PATH:-$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin}"

LOGS="$HARNESS_STATE/logs"
LOCKS="$HARNESS_STATE/locks"
mkdir -p "$LOGS" "$LOCKS"

# ---------------------------------------------------------------- runner mode
if [ "${1:-}" = "runner" ]; then
    slotmax=${2:-1}
    stagger=${3:-0}
    # Stagger keeps concurrent runners from claiming the same issue in the same second.
    [ "$stagger" -gt 0 ] && sleep "$stagger"

    slot=""
    s=1
    while [ "$s" -le "$slotmax" ]; do
        exec {fd}>"$LOCKS/slot$s.lock"
        if flock -n "$fd"; then slot=$s; break; fi
        eval "exec $fd>&-"
        s=$(( s + 1 ))
    done
    # No free slot: another runner won the race. Exit quietly.
    [ -z "$slot" ] && exit 0

    ts=$(date -u +%Y%m%dT%H%M%SZ)
    log="$LOGS/tick-$ts-slot$slot.log"
    cd "$TARGET_REPO" || exit 1
    start_s=$(date +%s)
    "$CLAUDE_BIN" -p "$(cat "$HARNESS_HOME/prompts/tick-prompt.md")" $CLAUDE_FLAGS >"$log" 2>&1
    rc=$?
    # The trailer makes a silently dead tick distinguishable from a quiet one.
    echo "[tick.sh] exit=$rc duration=$(( $(date +%s) - start_s ))s" >>"$log"

    ls -1t "$LOGS" | tail -n +$(( LOG_RETENTION + 1 )) | while read -r old; do rm -f "$LOGS/$old"; done
    exit 0
fi

# -------------------------------------------------------------- launcher mode

# 1. Cheapest gate: if every slot is held there is nothing to decide.
busy=0; free=0; s=1
while [ "$s" -le "$MAX_SLOTS" ]; do
    exec {fd}>"$LOCKS/slot$s.lock"
    if flock -n "$fd"; then flock -u "$fd"; free=$(( free + 1 )); else busy=$(( busy + 1 )); fi
    eval "exec $fd>&-"
    s=$(( s + 1 ))
done
[ "$free" -eq 0 ] && exit 0

read_cache_field() {  # $1 = json field name
    [ -r "$USAGE_CACHE" ] || return 0
    grep -oE "\"$1\"[[:space:]]*:[[:space:]]*[0-9]+" "$USAGE_CACHE" | grep -oE '[0-9]+$' | head -1
}

# 2. Refresh usage only when the cache is stale, so most firings make no API call.
cache_ms=$(read_cache_field timestamp)
if [ $(( $(date +%s) - ${cache_ms:-0} / 1000 )) -ge "$USAGE_REFRESH_AFTER" ] && [ -x "$USAGE_FETCH" ]; then
    "$USAGE_FETCH" 2>/dev/null || true
fi

five=""; week=""
cache_ms=$(read_cache_field timestamp)
if [ $(( $(date +%s) - ${cache_ms:-0} / 1000 )) -le "$USAGE_MAX_AGE" ]; then
    five=$(read_cache_field fiveHourPercent)
    week=$(read_cache_field weeklyPercent)
fi

# 3. Map the worse of the two rate-limit windows to a slot cap.
if [ -n "$five" ] && [ -n "$week" ]; then
    pct=$(( five > week ? five : week ))
    if   [ "$pct" -lt 55 ]; then allowed=$MAX_SLOTS
    elif [ "$pct" -lt 70 ]; then allowed=6
    elif [ "$pct" -lt 80 ]; then allowed=4
    elif [ "$pct" -lt 85 ]; then allowed=3
    elif [ "$pct" -lt 90 ]; then allowed=2
    elif [ "$pct" -lt 95 ]; then allowed=1
    else allowed=0
    fi
    [ "$allowed" -gt "$MAX_SLOTS" ] && allowed=$MAX_SLOTS
else
    # No fresh usage data: run conservatively rather than blind at full width.
    pct="?"; allowed=$(( MAX_SLOTS < 3 ? MAX_SLOTS : 3 ))
fi

# 4. Queue depth: open issues carrying the work label but not the claim label.
claimable=$(gh issue list --repo "${GH_REPO:-}" --label "$ISSUE_LABEL" --state open --limit 100 \
    --json labels --jq "[.[] | select(([.labels[].name] | index(\"$WIP_LABEL\")) | not)] | length" 2>/dev/null)
[ -z "$claimable" ] && claimable=1   # gh failed: assume one, let the tick sort it out

want=$claimable
# With an empty queue keep one heartbeat/audit tick alive, but rate-limit it — the
# launcher itself fires every couple of minutes.
if [ "$busy" -eq 0 ] && [ "$want" -lt 1 ]; then
    hb="$LOCKS/heartbeat.stamp"
    hb_age=$(( $(date +%s) - $( [ -f "$hb" ] && date -r "$hb" +%s || echo 0 ) ))
    if [ "$hb_age" -ge "$HEARTBEAT_INTERVAL" ]; then want=1; touch "$hb"; fi
fi

spawn=$(( allowed - busy ))
[ "$spawn" -gt "$want" ] && spawn=$want
[ "$spawn" -lt 0 ] && spawn=0
[ "$spawn" -eq 0 ] && exit 0   # quiet: no log spam on the frequent no-op firings

ts=$(date -u +%Y%m%dT%H%M%SZ)
echo "$ts five=${five:-?}% week=${week:-?}% allowed=$allowed busy=$busy claimable=$claimable -> spawning $spawn" \
    >>"$LOGS/allocator.log"

i=0
while [ "$i" -lt "$spawn" ]; do
    setsid "$0" runner "$allowed" $(( i * STAGGER_SECONDS )) >/dev/null 2>&1 &
    i=$(( i + 1 ))
done
exit 0
