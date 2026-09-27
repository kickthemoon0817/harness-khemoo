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

read_cache_field() {  # $1 = json field name
    [ -r "$USAGE_CACHE" ] || return 0
    grep -oE "\"$1\"[[:space:]]*:[[:space:]]*[0-9]+" "$USAGE_CACHE" | grep -oE '[0-9]+$' | head -1
}

# Sets five, week, pct and allowed: the slot cap the worse rate-limit window permits.
compute_allowed() {
    local cache_ms
    # Refresh usage only when the cache is stale, so most firings make no API call.
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
    if [ -n "$five" ] && [ -n "$week" ]; then
        pct=$(( five > week ? five : week ))
        # Full width until USAGE_FULL_BELOW (90 %), then half, then one; none past 98 %.
        if   [ "$pct" -lt "${USAGE_FULL_BELOW:-90}" ]; then allowed=$MAX_SLOTS
        elif [ "$pct" -lt 95 ]; then allowed=$(( (MAX_SLOTS + 1) / 2 ))
        elif [ "$pct" -lt 98 ]; then allowed=1
        else allowed=0
        fi
        [ "$allowed" -gt "$MAX_SLOTS" ] && allowed=$MAX_SLOTS
    else
        # No fresh usage data: run conservatively rather than blind at full width.
        pct="?"; allowed=$(( MAX_SLOTS < 3 ? MAX_SLOTS : 3 ))
    fi
}

# Sets claimable, young and want: queued issues not already about to be claimed.
compute_want() {
    local pid age
    # QUEUE_DEPTH_CMD (config) may replace the count with a project-aware one, e.g.
    # bin/claimable.sh, which also excludes issues whose dependencies are still open.
    if [ -n "${QUEUE_DEPTH_CMD:-}" ]; then
        claimable=$(GH_REPO="${GH_REPO:-}" ISSUE_LABEL="$ISSUE_LABEL" WIP_LABEL="$WIP_LABEL" $QUEUE_DEPTH_CMD 2>/dev/null)
    else
        claimable=$(gh issue list --repo "${GH_REPO:-}" --label "$ISSUE_LABEL" --state open --limit 100 \
            --json labels --jq "[.[] | select(([.labels[].name] | index(\"$WIP_LABEL\")) | not)] | length" 2>/dev/null)
    fi
    [ -z "$claimable" ] && claimable=1   # gh failed: assume one, let the tick sort it out
    # A tick needs a few minutes to read the queue and write its claim; a tick younger
    # than that is assumed to be about to claim one of the counted issues.
    young=0
    for pid in $(pgrep -f '^[^ ]*claude -p' 2>/dev/null); do
        age=$(ps -o etimes= -p "$pid" 2>/dev/null | tr -d ' ')
        [ -n "$age" ] && [ "$age" -lt "${CLAIM_GRACE_S:-360}" ] && young=$(( young + 1 ))
    done
    want=$(( claimable - young ))
    [ "$want" -lt 0 ] && want=0
}

# Model priority, best first: state/tick-model (whitespace-separated) overrides
# TICK_MODELS. A model that hit its usage limit is skipped until its cooldown passes.
: "${TICK_MODELS:=claude-opus-5-5}"
: "${MODEL_COOLDOWN_S:=1800}"
MODEL_COOL="$HARNESS_STATE/model-cooldown"
model_list() {
    if [ -s "$HARNESS_STATE/tick-model" ]; then tr -s '[:space:]' ' ' < "$HARNESS_STATE/tick-model"
    else printf '%s' "$TICK_MODELS"; fi
}
pick_model() {  # $1 = models already tried this tick
    local m stamp
    for m in $(model_list); do
        case " $1 " in *" $m "*) continue ;; esac
        stamp="$MODEL_COOL/$m"
        if [ -f "$stamp" ] && [ $(( $(date +%s) - $(date -r "$stamp" +%s) )) -lt "$MODEL_COOLDOWN_S" ]; then continue; fi
        echo "$m"; return 0
    done
}

# The harness is paused when its cron line is commented out or state/paused exists; a
# running chain honours both, so pausing the cron also stops ticks already in flight.
harness_paused() {
    [ -e "$HARNESS_STATE/paused" ] && return 0
    crontab -l 2>/dev/null | grep -qE '^[[:space:]]*[^#[:space:]].*bin/tick\.sh' || return 0
    return 1
}

# The fable lane: an issue labelled FABLE_LABEL (the operator sets it on difficult or important
# issues) is worked on FABLE_MODEL. A tick runs on that lane while claimable fable issues outnumber
# the live fable ticks, and its prompt tells it to claim only those; other ticks leave them alone.
# While FABLE_MODEL cools down on its usage limit, any tick may claim them, so none waits on it.
: "${FABLE_MODEL:=claude-fable-5-1}"
: "${FABLE_LABEL:=ai:fable}"
fable_cooling() {
    local stamp="$MODEL_COOL/$FABLE_MODEL"
    [ -f "$stamp" ] && [ $(( $(date +%s) - $(date -r "$stamp" +%s) )) -lt "$MODEL_COOLDOWN_S" ]
}
fable_lane() {  # true when this tick should run on the fable lane
    [ -n "$FABLE_MODEL" ] && [ -n "${QUEUE_DEPTH_CMD:-}" ] || return 1
    fable_cooling && return 1
    local want_f young_f=0 pid age
    want_f=$(GH_REPO="${GH_REPO:-}" ISSUE_LABEL="$FABLE_LABEL" WIP_LABEL="$WIP_LABEL" $QUEUE_DEPTH_CMD 2>/dev/null)
    [[ "$want_f" =~ ^[0-9]+$ ]] && [ "$want_f" -gt 0 ] || return 1
    # The count excludes issues a live tick holds, so only a fable tick young enough
    # to be still choosing its issue stands for one of them (compute_want's rule).
    for pid in $(pgrep -f -- "claude -p .*--model $FABLE_MODEL" 2>/dev/null); do
        age=$(ps -o etimes= -p "$pid" 2>/dev/null | tr -d ' ')
        [ -n "$age" ] && [ "$age" -lt "${CLAIM_GRACE_S:-360}" ] && young_f=$(( young_f + 1 ))
    done
    [ "$young_f" -lt "$want_f" ]
}

# ---------------------------------------------------------------- runner mode
# bash parses this whole block when a runner starts, so a running runner keeps
# chaining ticks on the code it started with. After changing the block, stop the
# runner shells (`kill` the `tick.sh runner` process, never its tick). A tick
# outlives its runner and keeps the slot lock through the descriptor it
# inherited, and cron starts runners on the new code.
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

    # A slot does not wait for the next cron firing: when a tick ends early and work is
    # still queued, the slot starts the next tick at once, each with a fresh context.
    chain=0
    while :; do
        ts=$(date -u +%Y%m%dT%H%M%SZ)
        log="$LOGS/tick-$ts-slot$slot.log"
        cd "$TARGET_REPO" || exit 1
        start_s=$(date +%s)
        # Run on the best model that is not cooling down. A tick that dies on a model's
        # usage limit cools that model and re-runs at once on the next one in the list.
        tried=""; model=""; rc=0
        while :; do
            if fable_lane && [[ " $tried " != *" $FABLE_MODEL "* ]]; then
                model="$FABLE_MODEL"
                lane="This tick runs on $FABLE_MODEL, the fable lane: claim only an issue labelled \`$FABLE_LABEL\`. If none is claimable, print a one-line summary and end."
            else
                model=$(pick_model "$tried")
                if [ -z "$model" ]; then
                    echo "[tick.sh] every model in '$(model_list)' is cooling down; nothing run" >>"$log"
                    break
                fi
                if fable_cooling; then
                    lane="This tick runs on $model. $FABLE_MODEL is cooling down on its usage limit, so an issue labelled \`$FABLE_LABEL\` may be claimed too."
                else
                    lane="This tick runs on $model: never claim an issue labelled \`$FABLE_LABEL\`; those are worked on the fable lane."
                fi
            fi
            flags="$(printf '%s' "$CLAUDE_FLAGS" | sed -E 's/--model[= ][^ ]+//') --model $model"
            run_s=$(date +%s)
            # Only this attempt's output may decide its fate: the log also holds earlier attempts.
            log_from=$(( $(stat -c %s "$log" 2>/dev/null || echo 0) + 1 ))
            "$CLAUDE_BIN" -p "$lane

$(cat "$HARNESS_HOME/prompts/tick-prompt.md")" $flags >>"$log" 2>&1
            rc=$?
            # The trailer makes a silently dead tick distinguishable from a quiet one.
            echo "[tick.sh] exit=$rc duration=$(( $(date +%s) - run_s ))s model=$model" >>"$log"
            if [ $(( $(date +%s) - run_s )) -lt "${CHAIN_MIN_TICK_S:-180}" ] \
                && tail -c +"$log_from" "$log" | tail -n 40 | grep -qE "You've (reached|hit) your [^.]{0,40}limit"; then
                mkdir -p "$MODEL_COOL"; touch "$MODEL_COOL/$model"
                echo "$(date -u +%Y%m%dT%H%M%SZ) slot$slot $model hit its usage limit; cooling ${MODEL_COOLDOWN_S}s" >>"$LOGS/allocator.log"
                tried="$tried $model"
                continue
            fi
            break
        done

        ls -1t "$LOGS" | tail -n +$(( LOG_RETENTION + 1 )) | while read -r old; do rm -f "$LOGS/$old"; done
        chain=$(( chain + 1 ))
        dur=$(( $(date +%s) - start_s ))
        # A short tick found nothing to claim or failed: leave the retry to cron.
        [ "$dur" -lt "${CHAIN_MIN_TICK_S:-180}" ] && break
        [ "$chain" -ge "${MAX_CHAIN:-24}" ] && break
        harness_paused && break
        compute_allowed
        [ "$slot" -gt "$allowed" ] && break
        compute_want
        [ "$want" -lt 1 ] && break
        echo "$(date -u +%Y%m%dT%H%M%SZ) slot$slot chains tick $(( chain + 1 )) after ${dur}s: pct=${pct}% allowed=$allowed claimable=$claimable young=$young" \
            >>"$LOGS/allocator.log"
    done
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
# Every model cooling down: a runner would only log that nothing ran.
[ -z "$(pick_model "")" ] && exit 0

compute_allowed
compute_want
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
