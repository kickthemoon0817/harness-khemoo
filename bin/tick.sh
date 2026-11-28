#!/usr/bin/env bash
# Autonomous issue harness — tick launcher.
#
# Three modes:
#   tick.sh                       launcher: decide how many ticks to run, spawn them
#   tick.sh check                 print what the instance resolves to; exit 1 if it is incomplete
#   tick.sh runner <max> <delay>  internal: hold a slot and run exactly one tick
#   tick.sh runner <max> <delay> landing
#                                 internal: one tick that claims only a merge slot's ready issue
#
# The launcher is invoked from cron every couple of minutes. It sizes the pool from
# live API rate-limit headroom AND the number of unclaimed issues, then spawns enough
# runners to fill it. Every step is ordered cheapest-first so the common case (nothing
# to do) costs no API calls.
set -u

# shellcheck source=/dev/null
. "$(dirname "${BASH_SOURCE[0]}")/../lib/instance.sh"
harness_load
: "${ISSUE_LABEL:=ai}"
: "${WIP_LABEL:=ai:wip}"
: "${READY_LABEL:=ai:ready}"
: "${SIGNOFF_LABEL:=ai:signoff}"
: "${OPERATOR_LABEL:=ai:operator}"
: "${LONG_LABEL:=ai:long}"
: "${TICK_BUDGET_MIN:=120}"
: "${LONG_TICK_BUDGET_MIN:=240}"
: "${MAX_SLOTS:=8}"
: "${STAGGER_SECONDS:=8}"
: "${HEARTBEAT_INTERVAL:=1800}"
: "${LOG_RETENTION:=200}"
: "${HOLD_TIMEOUT_S:=5}"
# shellcheck source=/dev/null
. "$HARNESS_HOME/lib/usage.sh"
: "${CLAUDE_BIN:=claude}"
# Set, but possibly empty: an explicit `CLAUDE_FLAGS=` means "pass no flags".
[ -z "${CLAUDE_FLAGS+x}" ] && CLAUDE_FLAGS="--dangerously-skip-permissions"
# The agent a tick runs: an adapter name in fleet's sense. Only claude runs locally; any agent can
# run through `fleet dispatch` when FLEET_DISPATCH=1 (docs/FLEET.md).
: "${TICK_AGENT:=claude}"
: "${FLEET_BIN:=fleet}"
fleet=""; [ "${FLEET_DISPATCH:-}" = 1 ] && fleet=1
# fleet reads $FLEET_CONFIG before its default, so an empty one must not reach it as set.
[ -n "${FLEET_CONFIG:-}" ] || unset FLEET_CONFIG
# Cron gives a bare PATH; the instance's bin/ stays first, as harness_load put it.
export PATH="$HARNESS_INSTANCE_DIR/bin:${HARNESS_PATH:-$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin}"

LOGS="$HARNESS_STATE/logs"
LOCKS="$HARNESS_STATE/locks"
mkdir -p "$LOGS" "$LOCKS"
CORE_PROMPT="$HARNESS_HOME/prompts/tick-core.md"
INSTANCE_PROMPT="$HARNESS_INSTANCE_DIR/prompts/tick.md"

# The launcher fires every couple of minutes: a state that persists is logged once, not each time.
log_changed() {  # $1 = message
    tail -1 "$LOGS/allocator.log" 2>/dev/null | cut -d' ' -f2- | grep -qxF -- "$1" \
        || echo "$(date -u +%Y%m%dT%H%M%SZ) $1" >> "$LOGS/allocator.log"
}

# Prints one line per thing the instance lacks; a tick cannot run without any of them.
instance_missing() {
    local root argv
    if [ ! -r "$HARNESS_ENV_FILE" ]; then echo "$HARNESS_ENV_FILE"
    else
        if [ -z "${TARGET_REPO:-}" ]; then echo "$HARNESS_ENV_FILE: TARGET_REPO unset"
        elif ! git -C "$TARGET_REPO" rev-parse --git-dir >/dev/null 2>&1; then
            echo "a git checkout at TARGET_REPO ($TARGET_REPO)"
        fi
        if [ -z "${WORKTREE_ROOT:-}" ]; then echo "$HARNESS_ENV_FILE: WORKTREE_ROOT unset"
        else
            # A tick creates the root on its first worktree, so its nearest existing ancestor must allow that.
            root=$WORKTREE_ROOT
            while [ ! -e "$root" ] && [ "$root" != / ]; do root=$(dirname "$root"); done
            [ -d "$root" ] && [ -w "$root" ] || echo "a WORKTREE_ROOT that exists or can be created ($WORKTREE_ROOT)"
        fi
        [ -n "${GH_REPO:-}" ] || echo "$HARNESS_ENV_FILE: GH_REPO unset, and TARGET_REPO's origin names no GitHub repository: set GH_REPO in harness.env"
        if [ -n "${QUEUE_DEPTH_CMD:-}" ]; then
            read -r -a argv <<<"$QUEUE_DEPTH_CMD"
            type -P -- "${argv[0]}" >/dev/null || echo "an executable QUEUE_DEPTH_CMD (${argv[0]})"
        fi
        if [ -n "$fleet" ]; then
            type -P -- "$FLEET_BIN" >/dev/null || echo "an executable FLEET_BIN ($FLEET_BIN)"
        else
            [ "$TICK_AGENT" = claude ] \
                || echo "$HARNESS_ENV_FILE: TICK_AGENT=$TICK_AGENT needs FLEET_DISPATCH=1; only claude runs locally"
            type -P -- "$CLAUDE_BIN" >/dev/null || echo "an executable CLAUDE_BIN ($CLAUDE_BIN)"
        fi
    fi
    # A tick runs the lint through it before every post.
    type -P -- "$HARNESS_BIN" >/dev/null || echo "an executable HARNESS_BIN ($HARNESS_BIN)"
    [ -r "$INSTANCE_PROMPT" ] || echo "$INSTANCE_PROMPT"
    [ -r "$CORE_PROMPT" ] || echo "$CORE_PROMPT"
}

if [ "${1:-}" != check ]; then
    harness_hook env.sh; rc=$?
    [ "$rc" -eq 0 ] || [ "$rc" -eq 3 ] \
        || log_changed "hooks/env.sh failed (exit $rc); ticks run without it"
fi
# What the core prompt, the lint and a tick's gh calls read; none of it is secret. GH_TOKEN never
# reaches a tick, whoever exported it: the config's token is for the cron scripts and the dashboard.
export TARGET_REPO WORKTREE_ROOT GH_REPO PUBLIC_RECORD_RULES PUBLIC_RECORD_PR_SECTIONS PUBLIC_RECORD_ISSUE_SECTIONS \
    ISSUE_LABEL WIP_LABEL READY_LABEL SIGNOFF_LABEL OPERATOR_LABEL LONG_LABEL FIRST_LABEL LOW_LABEL LANE_LABEL \
    TICK_AGENT TICK_AGENT_EXES FLEET_DISPATCH FLEET_DEVICE FLEET_BIN FLEET_CONFIG HARNESS_BIN
export -n GH_TOKEN

# Sets claimable, young and want: queued issues not already about to be claimed.
compute_want() {
    local pid age
    if queue_depth "$ISSUE_LABEL"; then
        claimable=$queue_n
    else
        # A GitHub outage must not stop every slot: one tick still runs, and it ends early when it
        # finds nothing to claim. A broken count command looks the same, so it is logged as an error.
        log_changed "ERROR queue count failed ($queue_error); counting 1 claimable issue"
        claimable=1
    fi
    # A tick needs a few minutes to read the queue and write its claim; a tick younger
    # than that is assumed to be about to claim one of the counted issues.
    young=0
    for pid in $(pgrep -f "$tick_re" 2>/dev/null); do
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

cron_active() { crontab -l 2>/dev/null | grep -qE '^[[:space:]]*[^#[:space:]].*bin/tick\.sh'; }

# The harness is paused when its cron line is commented out or state/paused exists, and held
# while the instance's hooks/hold says so (exit 0; its first line of output is the reason): an
# installation holds while something every tick needs is down, so no tick spends its claim
# finding that out. The launcher starts nothing and a runner chains nothing while paused or held;
# a tick already running finishes its own claim. A hook that fails or hangs past HOLD_TIMEOUT_S
# holds nothing, so a broken hook cannot stop the harness.
harness_paused() {
    local out
    [ -e "$HARNESS_STATE/paused" ] && return 0
    if out=$(HOOK_TIMEOUT_S="$HOLD_TIMEOUT_S" harness_hook hold 2>/dev/null); then
        out=$(printf '%s\n' "$out" | head -1)
        log_changed "held: ${out:-hooks/hold}"
        return 0
    fi
    cron_active || return 0
    return 1
}

# Counts the claimable issues labelled $1 into queue_n, through QUEUE_DEPTH_CMD with the labels it
# gates on, else through gh in the target checkout. A count that fails or prints anything but a
# number sets queue_error instead and returns 1, so no caller mistakes it for a count.
# QUEUE_DEPTH_CMD is a command line split on whitespace, without quoting: no path in it may hold a space.
queue_depth() {  # $1 = the label to count
    local out rc err
    err=$(mktemp)
    # gh reads the config's token, so the count gets it, through its environment only: a token in
    # argv shows in /proc/<pid>/cmdline, which ps and the dashboard read. A tick never gets it.
    out=$(
        exec 2>"$err"
        [ -n "${GH_TOKEN:-}" ] && export GH_TOKEN
        if [ -n "${QUEUE_DEPTH_CMD:-}" ]; then
            # shellcheck disable=SC2086
            GH_REPO="${GH_REPO:-}" ISSUE_LABEL="$1" WIP_LABEL="$WIP_LABEL" SIGNOFF_LABEL="$SIGNOFF_LABEL" \
                OPERATOR_LABEL="$OPERATOR_LABEL" FIRST_LABEL="${FIRST_LABEL:-}" LOW_LABEL="${LOW_LABEL:-}" \
                LANE_LABEL="${LANE_LABEL:-}" exec $QUEUE_DEPTH_CMD
        fi
        cd "$TARGET_REPO" && exec gh issue list --repo "${GH_REPO:-}" --label "$1" --state open --limit 100 \
            --json labels --jq "[.[] | select(([.labels[].name] | index(\"$WIP_LABEL\")) | not)] | length"
    )
    rc=$?
    queue_error="label $1: exit $rc, output '$(printf '%s' "$out" | tr '\n' ' ' | cut -c1-60)', stderr '$(tail -n 1 "$err" | cut -c1-160)'"
    rm -f "$err"
    [ "$rc" -eq 0 ] && [[ "$out" =~ ^[0-9]+$ ]] || return 1
    queue_n=$out
}

# The model lane: an issue labelled LANE_LABEL (the operator sets it on difficult or important
# issues) is worked on LANE_MODEL. A tick runs on that lane while claimable lane issues outnumber
# the live lane ticks, and its prompt tells it to claim only those; other ticks leave them alone.
# While LANE_MODEL cools down on its usage limit, any tick may claim them, so none waits on it.
# The lane is off unless both are set.
: "${LANE_MODEL:=}"
: "${LANE_LABEL:=}"
lane_on() { [ -n "$LANE_MODEL" ] && [ -n "$LANE_LABEL" ]; }
lane_cooling() {
    local stamp="$MODEL_COOL/$LANE_MODEL"
    [ -f "$stamp" ] && [ $(( $(date +%s) - $(date -r "$stamp" +%s) )) -lt "$MODEL_COOLDOWN_S" ]
}
lane_wanted() {  # true when this tick should run on the model lane
    lane_on && [ -n "${QUEUE_DEPTH_CMD:-}" ] || return 1
    lane_cooling && return 1
    local want_l young_l=0 pid age
    if ! queue_depth "$LANE_LABEL"; then
        log_changed "ERROR lane queue count failed ($queue_error); no lane tick"
        return 1
    fi
    want_l=$queue_n
    [ "$want_l" -gt 0 ] || return 1
    # The count excludes issues a live tick holds, so only a lane tick young enough
    # to be still choosing its issue stands for one of them (compute_want's rule).
    for pid in $(pgrep -f -- "$lane_re$LANE_MODEL" 2>/dev/null); do
        age=$(ps -o etimes= -p "$pid" 2>/dev/null | tr -d ' ')
        [ -n "$age" ] && [ "$age" -lt "${CLAIM_GRACE_S:-360}" ] && young_l=$(( young_l + 1 ))
    done
    [ "$young_l" -lt "$want_l" ]
}

# A merge slot's landing tick: it may start past the width the usage windows allow, so it
# claims nothing but an issue waiting for the slot and never chains.
LANDING_MARK="This tick is the merge slot's landing tick"
LANDING_LANE="$LANDING_MARK: claim only an issue labelled \`$READY_LABEL\`. If none is claimable, print a one-line summary and end."
# How a live tick shows in the process table. Through fleet it is the `fleet dispatch` waiting on
# it, whose argv carries no prompt, so a landing tick is told by its label.
if [ -n "$fleet" ]; then
    tick_re="^[^ ]*${FLEET_BIN##*/} dispatch "
    lane_re="${FLEET_BIN##*/} dispatch .*--model "
    landing_re="${FLEET_BIN##*/} dispatch .*--label tick=landing"
else
    tick_re='^[^ ]*claude -p'
    lane_re='claude -p .*--model '
    landing_re="claude -p $LANDING_MARK"
fi

# The tick's prompt: the lane line, the core rules, then this installation's own. {{KEY}} takes
# the key's value through a sed escape, never eval, so a value cannot run as code.
PROMPT_KEYS="GH_REPO ISSUE_LABEL WIP_LABEL READY_LABEL SIGNOFF_LABEL OPERATOR_LABEL LONG_LABEL LANE_LABEL TICK_BUDGET_MIN LONG_TICK_BUDGET_MIN USAGE_CEILING_PCT"
compose_prompt() {  # $1 = the lane line
    local k v subst=()
    for k in $PROMPT_KEYS; do
        v=$(printf '%s' "${!k:-}" | tr '\n' ' ' | sed 's/[\\&|]/\\&/g')
        subst+=(-e "s|{{$k}}|$v|g")
    done
    printf '%s\n\n' "$1"
    sed "${subst[@]}" "$CORE_PROMPT"
    printf '\n## This installation\n\n'
    sed "${subst[@]}" "$INSTANCE_PROMPT"
}

# Prints each device `fleet status --json` reports; fails unless a reachable one offers TICK_AGENT
# for dispatch (and is FLEET_DEVICE, when that names one). fleet exits 1 when any device failed, with
# the document complete, so only the document decides.
fleet_check() {
    # The document is read as text, so one that is not JSON is reported like one of the wrong shape.
    # A device is down when its ok is false, null, 0, "", [] or {}.
    # shellcheck disable=SC2016
    timeout "${FLEET_CHECK_TIMEOUT_S:-60}" "$FLEET_BIN" status --json 2>/dev/null | jq -Rrs \
        --arg agent "$TICK_AGENT" --arg wanted "${FLEET_DEVICE:-auto}" '
        def need($k): if type == "object" and has($k) then .[$k] else error("no \($k) in \(type)") end;
        def truthy: . != null and . != false and . != 0 and . != "" and . != [] and . != {};
        def str: if type == "string" then . else tojson end;
        def device:
            if need("ok") | truthy | not then
                need("error") as $e
                | {line: "fleet device:  \(need("name") | str) (\(need("transport") | str)) unreachable: \($e | need("code") | str): \($e | need("message") | str)",
                   usable: false}
            else
                . as $d
                | ([need("status") | need("adapters") | if type == "array" then .[] else error("adapters is not a list") end
                    | select(need("name") == $agent)] | first) as $a
                | (if $a then "\($agent) \($a | need("running") | str)/\($a | need("max_concurrent") | str) running" else "no \($agent) adapter" end) as $offer
                | {line: "fleet device:  \($d | need("name") | str) (\($d | need("transport") | str)) reachable, \($offer)",
                   usable: ($a != null and ($a.max_concurrent | if type == "number" then . > 0 else error("max_concurrent is not a number") end)
                            and ($wanted == "auto" or $wanted == $d.name))}
            end;
        [try (
            (if test("^\\s*$") then error("empty output") else fromjson end)
            | if need("version") != 1 then error("version \(.version | str), not 1") else . end
            | need("devices") | if type == "array" then .[] else error("devices is not a list") end
            | device
        ) catch {error: (tostring | gsub("\\s+"; " ") | .[:160])}] as $items
        | ($items[] | if has("error") then "fleet:         fleet status --json gave no status document (\(.error))" else .line end),
          (if any($items[]; has("error")) or ([$items[] | select(.usable)] | length) == 0 then "" | halt_error(1) else empty end)'
}

# ----------------------------------------------------------------- check mode
if [ "${1:-}" = check ]; then
    echo "harness home:  $HARNESS_HOME"
    echo "instance:      $HARNESS_INSTANCE_DIR"
    echo "config:        $HARNESS_ENV_FILE"
    echo "state:         $HARNESS_STATE"
    echo "target repo:   ${TARGET_REPO:-}"
    echo "worktree root: ${WORKTREE_ROOT:-}"
    echo "repository:    ${GH_REPO:-(unset)}"
    echo "queue:         ${QUEUE_DEPTH_CMD:-gh issue list --label $ISSUE_LABEL, in the target repo}"
    echo "claude:        $CLAUDE_BIN"
    echo "agent:         $TICK_AGENT"
    if [ -n "$fleet" ]; then
        echo "dispatch:      fleet: $FLEET_BIN dispatch --device ${FLEET_DEVICE:-auto}, config ${FLEET_CONFIG:-$HARNESS_INSTANCE_DIR/fleet.toml}"
    else
        echo "dispatch:      local"
    fi
    echo "models:        $(model_list)"
    if lane_on; then echo "model lane:    $LANE_MODEL for \`$LANE_LABEL\`"; else echo "model lane:    off"; fi
    # harness_hook sources a *.sh hook and executes any other, so each is usable only in that form.
    for h in env.sh hold worktree-busy worktree-remove; do
        f="$HARNESS_INSTANCE_DIR/hooks/$h"
        if [ ! -e "$f" ]; then hs=none
        elif [[ "$h" == *.sh ]]; then if [ -r "$f" ]; then hs=present; else hs="present but not readable (ignored)"; fi
        elif [ -x "$f" ]; then hs=present
        else hs="present but not executable (ignored)"; fi
        echo "hook $h: $hs"
    done
    if out=$(HOOK_TIMEOUT_S="$HOLD_TIMEOUT_S" harness_hook hold 2>/dev/null); then
        echo "hold:          held: $(printf '%s\n' "$out" | head -1)"
    else
        echo "hold:          not held"
    fi
    if [ -e "$HARNESS_STATE/paused" ]; then echo "paused:        yes, $HARNESS_STATE/paused exists"
    elif ! cron_active; then echo "paused:        yes, no active crontab line runs bin/tick.sh"
    else echo "paused:        no"; fi
    missing=$(instance_missing)
    if [ -n "$fleet" ] && type -P -- "$FLEET_BIN" >/dev/null; then
        fleet_check || missing+="${missing:+$'\n'}a fleet device that can take a $TICK_AGENT tick"
    fi
    if [ -n "$missing" ]; then
        printf '%s\n' "$missing" | sed 's/^/missing: /'
        exit 1
    fi
    echo "instance complete"
    exit 0
fi

# ---------------------------------------------------------------- runner mode
# bash parses this whole block when a runner starts, so a running runner keeps
# chaining ticks on the code it started with. After changing the block, stop the
# runner shells (`kill` the `tick.sh runner` process, never its tick). A tick
# outlives its runner and keeps the slot lock through the descriptor it
# inherited, and cron starts runners on the new code.
if [ "${1:-}" = "runner" ]; then
    slotmax=${2:-1}
    stagger=${3:-0}
    landing=""; [ "${4:-}" = landing ] && landing=1
    # Stagger keeps concurrent runners from claiming the same issue in the same second.
    [ "$stagger" -gt 0 ] && sleep "$stagger"
    # A pause set while the launcher staggered its runners still stops the ones not yet started.
    harness_paused && exit 0

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
            if [ -n "$landing" ]; then
                model=$(pick_model "$tried")
                if [ -z "$model" ]; then
                    echo "[tick.sh] every model in '$(model_list)' is cooling down; nothing run" >>"$log"
                    break
                fi
                lane="$LANDING_LANE"
            elif lane_wanted && [[ " $tried " != *" $LANE_MODEL "* ]]; then
                model="$LANE_MODEL"
                lane="This tick runs on $LANE_MODEL, the model lane: claim only an issue labelled \`$LANE_LABEL\`. If none is claimable, print a one-line summary and end."
            else
                model=$(pick_model "$tried")
                if [ -z "$model" ]; then
                    echo "[tick.sh] every model in '$(model_list)' is cooling down; nothing run" >>"$log"
                    break
                fi
                if ! lane_on; then
                    lane="This tick runs on $model."
                elif lane_cooling; then
                    lane="This tick runs on $model. $LANE_MODEL is cooling down on its usage limit, so an issue labelled \`$LANE_LABEL\` may be claimed too."
                else
                    lane="This tick runs on $model: never claim an issue labelled \`$LANE_LABEL\`; those are worked on the model lane."
                fi
            fi
            flags="$(printf '%s' "$CLAUDE_FLAGS" | sed -E 's/--model[= ][^ ]+//') --model $model"
            run_s=$(date +%s)
            # Only this attempt's output may decide its fate: the log also holds earlier attempts.
            log_from=$(( $(stat -c %s "$log" 2>/dev/null || echo 0) + 1 ))
            if [ -n "$fleet" ]; then
                # The prompt goes on stdin, never into argv, which ps shows. fleet exits with the
                # agent's status and writes its output here, so what follows reads it as a local run's.
                # The timeout allows the longest budget and half an hour to wrap up.
                dispatch=("$FLEET_BIN" dispatch --device "${FLEET_DEVICE:-auto}" --adapter "$TICK_AGENT"
                    --workdir "$TARGET_REPO" --model "$model" --wait --timeout $(( (LONG_TICK_BUDGET_MIN + 30) * 60 )))
                [ -n "$landing" ] && dispatch+=(--label tick=landing)
                compose_prompt "$lane" | "${dispatch[@]}" >>"$log" 2>&1
                rc=$?
            else
                "$CLAUDE_BIN" -p "$(compose_prompt "$lane")" $flags >>"$log" 2>&1
                rc=$?
            fi
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
        [ -n "$landing" ] && break
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

harness_paused && exit 0
missing=$(instance_missing)
if [ -n "$missing" ]; then
    # Say so at most hourly; `tick.sh check` names everything that is missing.
    stamp="$LOCKS/instance-incomplete.stamp"
    if [ ! -f "$stamp" ] || [ $(( $(date +%s) - $(date -r "$stamp" +%s) )) -ge 3600 ]; then
        touch "$stamp"
        echo "$(date -u +%Y%m%dT%H%M%SZ) instance incomplete: $(printf '%s\n' "$missing" | head -1)" >>"$LOGS/allocator.log"
    fi
    exit 0
fi

# 1. Cheapest gate: if every slot is held there is nothing to decide.
busy=0; free=0; s=1
while [ "$s" -le "$MAX_SLOTS" ]; do
    exec {fd}>"$LOCKS/slot$s.lock"
    if flock -n "$fd"; then flock -u "$fd"; free=$(( free + 1 )); else busy=$(( busy + 1 )); fi
    eval "exec $fd>&-"
    s=$(( s + 1 ))
done
[ "$free" -eq 0 ] && exit 0
# Every model cooling down and no model lane to run: a runner would only log that nothing ran.
[ -z "$(pick_model "")" ] && ! lane_wanted && exit 0

compute_allowed
if [ "$pct" = "?" ]; then
    # Say so at most hourly: the launcher fires every couple of minutes.
    stamp="$LOCKS/usage-unknown.stamp"
    if [ ! -f "$stamp" ] || [ $(( $(date +%s) - $(date -r "$stamp" +%s) )) -ge 3600 ]; then
        touch "$stamp"
        echo "$(date -u +%Y%m%dT%H%M%SZ) no fresh usage reading in $USAGE_CACHE; starting nothing" >>"$LOGS/allocator.log"
    fi
fi
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
# A landing needs little budget, but in a full pool it waits for a slot to free,
# which can outlast the merge slot. With every allowed slot held and no young tick about to
# claim, one landing tick starts past the width for an issue waiting for the slot.
if [ "$spawn" -eq 0 ] && [ "$allowed" -gt 0 ] && [ "$busy" -ge "$allowed" ] && [ "$free" -gt 0 ] \
    && [ "$young" -eq 0 ] && [ -n "${QUEUE_DEPTH_CMD:-}" ] \
    && ! pgrep -f -- "$landing_re" >/dev/null 2>&1; then
    ready=0
    if queue_depth "$READY_LABEL"; then ready=$queue_n
    else log_changed "ERROR ready queue count failed ($queue_error); no landing tick"; fi
    if [ "$ready" -gt 0 ]; then
        slotmax=$(( busy + 1 )); [ "$slotmax" -gt "$MAX_SLOTS" ] && slotmax=$MAX_SLOTS
        echo "$(date -u +%Y%m%dT%H%M%SZ) five=${five:-?}% week=${week:-?}% allowed=$allowed busy=$busy ready=$ready -> spawning 1 landing tick" \
            >>"$LOGS/allocator.log"
        setsid "$0" runner "$slotmax" 0 landing >/dev/null 2>&1 &
        exit 0
    fi
fi
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
