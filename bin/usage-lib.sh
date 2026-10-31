# Sourced by tick.sh and usage_ok.sh: the rate-limit reading and the ceiling the harness keeps
# both windows under.

: "${USAGE_CACHE:=$HOME/.claude/usage-cache.json}"
: "${USAGE_FETCH:=$HOME/.claude/scripts/usage-fetch.sh}"
: "${USAGE_MAX_AGE:=1800}"
: "${USAGE_REFRESH_AFTER:=120}"

# Neither the 5-hour nor the weekly window may reach USAGE_CEILING_PCT. Ticks already running
# keep spending after spawning stops, so each window drops to half width, then to one slot, then
# to none well under the ceiling. The 5-hour window fills several times faster than the weekly
# one, so its steps start lower.
: "${USAGE_CEILING_PCT:=90}"
# A running tick stands down this far under the ceiling, so its wind-down stays under it too.
: "${USAGE_TICK_STOP_PCT:=$(( USAGE_CEILING_PCT - 2 ))}"
: "${FIVE_HALF_AT:=60}" "${FIVE_ONE_AT:=70}" "${FIVE_NONE_AT:=80}"
: "${WEEK_HALF_AT:=80}" "${WEEK_ONE_AT:=85}" "${WEEK_NONE_AT:=88}"

read_cache_field() {  # $1 = json field name
    [ -r "$USAGE_CACHE" ] || return 0
    grep -oE "\"$1\"[[:space:]]*:[[:space:]]*[0-9]+" "$USAGE_CACHE" | grep -oE '[0-9]+$' | head -1
}

# Sets five and week from a fresh reading; both stay empty when none is fresh.
read_usage() {
    local cache_ms
    # Refresh only when the cache is stale, so most calls make no API call.
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
}

# Slots one window permits: $1 percent; $2, $3, $4 where it drops to half, one, none; $5 full width.
window_slots() {
    if   [ "$1" -lt "$2" ]; then echo "$5"
    elif [ "$1" -lt "$3" ]; then echo $(( ($5 + 1) / 2 ))
    elif [ "$1" -lt "$4" ]; then echo 1
    else echo 0
    fi
}

# True while both windows are under $1 percent (default: the ceiling).
under_ceiling() {
    local at=${1:-$USAGE_CEILING_PCT}
    [ -n "$five" ] && [ -n "$week" ] && [ "$five" -lt "$at" ] && [ "$week" -lt "$at" ]
}

# Sets five, week, pct and allowed: the slot cap both rate-limit windows permit (MAX_SLOTS wide).
compute_allowed() {
    local a5 aw
    read_usage
    if [ -n "$five" ] && [ -n "$week" ]; then
        pct=$(( five > week ? five : week ))
        a5=$(window_slots "$five" "$FIVE_HALF_AT" "$FIVE_ONE_AT" "$FIVE_NONE_AT" "$MAX_SLOTS")
        aw=$(window_slots "$week" "$WEEK_HALF_AT" "$WEEK_ONE_AT" "$WEEK_NONE_AT" "$MAX_SLOTS")
        allowed=$(( a5 < aw ? a5 : aw ))
        under_ceiling || allowed=0
    else
        # No fresh reading: the ceiling cannot be kept blind, so start nothing.
        pct="?"; allowed=0
    fi
}
