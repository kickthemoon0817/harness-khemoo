#!/usr/bin/env bash
# Autonomous issue harness — tick launcher.
#
# Three modes:
#   tick.sh                       launcher: decide how many ticks to run, spawn them
#   tick.sh check                 print what the instance resolves to; exit 1 if it is incomplete
#   tick.sh runner <max> <delay>  internal: hold a slot and run exactly one tick
#   tick.sh runner <max> <delay> landing
#                                 internal: one tick that claims only a merge slot's ready issue
#   tick.sh supervisor <reason>   internal: one supervisor tick (SUPERVISOR=1)
#
# The launcher is invoked from cron every couple of minutes. It sizes the pool from
# each agent's usage windows AND the number of unclaimed issues, then spawns enough
# runners to fill it. Every step is ordered cheapest-first so the common case (nothing
# to do) costs no API calls.
set -u
# Started from inside a tick, the launcher is still no part of it: its runners and their ticks must
# not inherit that tick's marker.
unset HARNESS_TICK FLEET_SESSION FLEET_ADAPTER

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
: "${USAGE_CEILING_PCT:=90}"
# The supervisor (docs/ARCHITECTURE.md) runs only with SUPERVISOR=1. These defaults fill its prompt
# and its budget; `harness` reads the rails they name from harness.env itself, never from here.
: "${SUPERVISOR_BUDGET_MIN:=10}"
: "${SUPERVISOR_MAX_DIRECTIVE_S:=21600}"
: "${SUPERVISOR_STOP_GRACE_S:=1200}"
: "${SUPERVISOR_MAX_PARKS_PER_DAY:=2}"
supervisor=""; [ "${SUPERVISOR:-}" = 1 ] && supervisor=1
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
SUPERVISOR_CORE="$HARNESS_HOME/prompts/supervisor-core.md"
SUPERVISOR_PROMPT="$HARNESS_INSTANCE_DIR/prompts/supervisor.md"
SUP="$HARNESS_STATE/supervisor"

# The agent CLIs a tick runs on this host, each reading its prompt from stdin, never argv, which ps
# shows. AGENT_BIN_<agent> names the executable (default: the agent's name), AGENT_FLAGS_<agent> its
# flags (set but empty passes none) and AGENT_LIMIT_RE_<agent> the extended regex its usage-limit
# message matches. Through fleet any adapter runs, and the device's fleetd.toml fixes its argv.
LOCAL_AGENTS="claude codex gemini"
CLAUDE_LIMIT_RE="You've (reached|hit) your [^.]{0,40}limit"
CODEX_LIMIT_RE="You've hit your usage limit|rate_limit_reached"
GEMINI_LIMIT_RE="TerminalQuotaError|RESOURCE_EXHAUSTED|exhausted your daily quota|exhausted your capacity|Quota exceeded|status 429"
agent_setting() {  # $1 = setting, $2 = agent, $3 = default when the setting is unset
    local name="$1_$2"
    if [ -n "${!name+x}" ]; then printf '%s' "${!name}"; else printf '%s' "$3"; fi
}
agent_bin() { local name="AGENT_BIN_$1"; printf '%s' "${!name:-$1}"; }
agent_limit_re() {
    case $1 in
        claude) agent_setting AGENT_LIMIT_RE claude "$CLAUDE_LIMIT_RE" ;;
        codex) agent_setting AGENT_LIMIT_RE codex "$CODEX_LIMIT_RE" ;;
        gemini) agent_setting AGENT_LIMIT_RE gemini "$GEMINI_LIMIT_RE" ;;
        # A fleet adapter of another name runs one of these CLIs under it.
        *) agent_setting AGENT_LIMIT_RE "$1" "$CLAUDE_LIMIT_RE|$CODEX_LIMIT_RE|$GEMINI_LIMIT_RE" ;;
    esac
}
# Sets argv to the command line that runs agent $1 on model $2 here.
agent_argv() {
    local flags word skip=""
    local -a words
    case $1 in
        claude) flags=$(agent_setting AGENT_FLAGS claude --dangerously-skip-permissions) ;;
        codex) flags=$(agent_setting AGENT_FLAGS codex --dangerously-bypass-approvals-and-sandbox) ;;
        gemini) flags=$(agent_setting AGENT_FLAGS gemini --yolo) ;;
    esac
    # Split on whitespace, never globbed.
    read -ra words <<<"$flags"
    argv=("$(agent_bin "$1")")
    case $1 in
        claude) argv+=(-p --model "$2") ;;
        codex) argv+=(exec --model "$2") ;;
        # gemini runs headless because its stdin is not a terminal.
        gemini) argv+=(--model "$2") ;;
    esac
    # The entry names the model; a model flag among the flags would contradict it.
    for word in "${words[@]}"; do
        if [ -n "$skip" ]; then skip=""; continue; fi
        case $word in
            --model|-m) skip=1 ;;
            --model=*|-m?*) ;;
            *) argv+=("$word") ;;
        esac
    done
    [ "$1" = codex ] && argv+=(-)
    return 0
}

# The launcher fires every couple of minutes: a state that persists is logged once, not each time.
log_changed() {  # $1 = message
    tail -1 "$LOGS/allocator.log" 2>/dev/null | cut -d' ' -f2- | grep -qxF -- "$1" \
        || echo "$(date -u +%Y%m%dT%H%M%SZ) $1" >> "$LOGS/allocator.log"
}

# Model priority, best first: state/tick-model (whitespace-separated) overrides TICK_MODELS. Each
# entry is agent:model, agent@account:model for a tick on an account folder under
# AGENT_ACCOUNTS_DIR, or a model that TICK_AGENT runs. An entry that hit its usage limit is skipped
# until its cooldown passes.
: "${TICK_MODELS:=claude-opus-5-5}"
: "${MODEL_COOLDOWN_S:=1800}"
MODEL_COOL="$HARNESS_STATE/model-cooldown"
configured_models() {
    if [ -s "$HARNESS_STATE/tick-model" ]; then tr -s '[:space:]' ' ' < "$HARNESS_STATE/tick-model"
    else printf '%s' "$TICK_MODELS"; fi
}
# A directive's models, a subset or reordering of the configured list, replace it while it holds.
# directive_read sets these; empty, the configuration's own formulas hold.
d_quiet=""; d_until=""; d_why=""; d_woken=""; d_width=""; d_models=""; d_focus=""; d_avoid=""
model_list() {
    if [ -n "$d_models" ]; then printf '%s' "$d_models"; else configured_models; fi
}
entry_of() {  # $1 = a list entry; prints it as agent:model or agent@account:model
    case $1 in *:*) printf '%s' "$1" ;; *) printf '%s' "$TICK_AGENT:$1" ;; esac
}

# The model lane: an issue labelled LANE_LABEL (the operator sets it on difficult or important
# issues) is worked on LANE_MODEL. A tick runs on that lane while claimable lane issues outnumber
# the live lane ticks, and its prompt tells it to claim only those; other ticks leave them alone.
# While LANE_MODEL cools down on its usage limit, any tick may claim them, so none waits on it.
# The lane is off unless both are set.
: "${LANE_MODEL:=}"
: "${LANE_LABEL:=}"
lane_entry=""; [ -n "$LANE_MODEL" ] && lane_entry=$(entry_of "$LANE_MODEL")
lane_on() { [ -n "$LANE_MODEL" ] && [ -n "$LANE_LABEL" ]; }
entry_cooling() {
    local stamp="$MODEL_COOL/$1"
    [ -f "$stamp" ] && [ $(( $(date +%s) - $(date -r "$stamp" +%s) )) -lt "$MODEL_COOLDOWN_S" ]
}

# Each agent, or agent@account, of the model list and the lane, once.
list_seats() {
    local w
    for w in $(model_list) $(lane_on && printf '%s' "$lane_entry"); do
        w=$(entry_of "$w"); printf '%s\n' "${w%%:*}"
    done | awk '!seen[$0]++'
}
list_agents() { list_seats | sed 's/@.*//' | awk '!seen[$0]++'; }

# The accounts' root and the agents' executables, which `harness` reads to find an account folder
# and ask its CLI whether it is signed in. With no root, accounts are off and neither is exported.
accounts_export() {
    local v
    [ -n "${AGENT_ACCOUNTS_DIR:-}" ] || return 0
    export AGENT_ACCOUNTS_DIR
    for v in $(compgen -v AGENT_BIN_); do export "${v?}"; done
}
# Prints the env(1) arguments a tick on each agent@account of $@ runs with, one a line: an --unset of
# each of its agent's credential variables, so the default account's key never signs it in, then
# VARIABLE=folder. Fails, naming each refused account on stderr, when a folder is missing, a symlink
# or writable by anyone else.
account_env() {
    ( accounts_export; exec "$HARNESS_BIN" accounts env "$@" )
}

# Runs `harness usage`, or with `accounts` first `harness accounts`, with what it reads: harness.env
# is sourced here, not exported. The lane's entry counts toward the pool only while pool_lane names
# it.
pool_lane=""
harness_usage() {
    local sub=usage
    [ "${1:-}" = accounts ] && { sub=accounts; shift; }
    (
        vars=$(compgen -v USAGE_)
        # shellcheck disable=SC2086,SC2163
        [ -n "$vars" ] && export $vars
        [ -n "${TICK_WIDTH:-}" ] && export TICK_WIDTH
        accounts_export
        # The supervisor's account is fetched and checked with the lists' accounts. Its entry comes from
        # the configured list, which the list a directive narrows may leave it out of.
        if [ -n "$supervisor" ]; then
            s=$(supervisor_entry); s=${s%%:*}
            [[ "$s" == *@* ]] && { SUPERVISOR_MODEL=$(supervisor_entry); export SUPERVISOR SUPERVISOR_MODEL; }
        fi
        export MAX_SLOTS MODEL_COOLDOWN_S
        TICK_MODELS=$(model_list) LANE_MODEL=$pool_lane exec "$HARNESS_BIN" "$sub" "$@"
    )
}
# Runs each stale agent's fetch command and checks again each account's old sign-in, once per
# launcher firing and per tick a runner starts; the readings are only read in between. An account
# found signed out, or signed in again, is logged here, once.
usage_refresh() {
    local was=$pool_lane line
    lane_on && pool_lane=$lane_entry
    harness_usage refresh 2>/dev/null | while IFS= read -r line; do
        echo "$(date -u +%Y%m%dT%H%M%SZ) $line" >>"$LOGS/allocator.log"
    done
    pool_lane=$was
}

# Runs `harness <subcommand>` for the supervisor and the directive with what they read; --gh passes
# GH_TOKEN, as the queue count gets it, to `supervisor due`'s look for Operator comments. The model
# list is the configured one: a directive is checked against it.
harness_supervisor() {
    local gh=""
    [ "$1" = --gh ] && { gh=1; shift; }
    (
        vars=$(compgen -v USAGE_; compgen -v SUPERVISOR_)
        # shellcheck disable=SC2086,SC2163
        [ -n "$vars" ] && export $vars
        [ -n "${TICK_WIDTH:-}" ] && export TICK_WIDTH
        [ -n "$gh" ] && [ -n "${GH_TOKEN:-}" ] && export GH_TOKEN
        accounts_export
        export MAX_SLOTS MODEL_COOLDOWN_S TICK_MODELS TICK_AGENT LONG_TICK_BUDGET_MIN
        exec "$HARNESS_BIN" "$@"
    )
}

supervisor_entry() {
    local m=${SUPERVISOR_MODEL:-}
    [ -n "$m" ] || m=$(configured_models | awk '{print $NF}')
    entry_of "$m"
}

# Sets argv to the supervisor's command line, agent $1 on model $2 with no tools, or sets refusal
# to why it cannot run so and returns 1. SUPERVISOR_FLAGS_<agent> replaces the defaults (split on
# whitespace) and is held to an allowlist (supervisor_flags_ok): it may drop the optional flags,
# never add one. What the defaults guarantee (docs/INSTANCE.md): claude has no built-in tool
# (--tools ""), no MCP server (--strict-mcp-config) and no settings, hooks or plugins
# (--restricted); codex runs read-only and offline with its shell, exec, apps, plugins, browser and
# web search off. Gemini has no flag that takes its file and web tools away in headless mode, so it
# never supervises.
SUPERVISOR_CODEX_DISABLED="shell_tool unified_exec apps plugins browser_use computer_use in_app_browser image_generation"
supervisor_argv() {
    local name="SUPERVISOR_FLAGS_$1" word f
    local -a flags=() words
    refusal=""
    if [ -n "${!name+x}" ]; then
        # An empty value cannot be written in a whitespace-split setting: `--tools=` stands for `--tools ""`.
        read -ra words <<<"${!name}"
        for word in "${words[@]}"; do
            if [ "$word" = --tools= ]; then flags+=(--tools ""); else flags+=("$word"); fi
        done
    else
        case $1 in
            claude) flags=(--tools "" --restricted --strict-mcp-config --disable-slash-commands --no-session-persistence) ;;
            codex)
                flags=(--sandbox read-only --skip-git-repo-check --ephemeral --ignore-user-config)
                for f in $SUPERVISOR_CODEX_DISABLED; do flags+=(--disable "$f"); done
                flags+=(-c web_search=disabled) ;;
        esac
    fi
    case $1 in
        claude|codex) supervisor_flags_ok "$1" "${flags[@]}" || { refusal="$name: $refusal"; return 1; } ;;
        gemini) refusal="gemini cannot supervise: no flag of its CLI takes its file and web tools away in headless mode"; return 1 ;;
        *) refusal="$1 cannot supervise: only claude and codex run here without tools"; return 1 ;;
    esac
    case $1 in
        claude) argv=("$(agent_bin claude)" -p --model "$2" "${flags[@]}") ;;
        codex) argv=("$(agent_bin codex)" exec --model "$2" "${flags[@]}" -) ;;
    esac
}
# True when the flags $2... confine agent $1 and hold nothing else; else sets refusal. An
# allowlist: a flag outside it, such as one that names tools, loads settings or MCP servers, enables
# a feature or lifts the sandbox, is refused, not weighed.
supervisor_flags_ok() {
    local agent=$1 word seen=" " f
    shift
    while [ $# -gt 0 ]; do
        word=$1; shift
        case "$agent:$word" in
            claude:--tools)
                [ $# -gt 0 ] && [ -z "$1" ] || { refusal="--tools must be given empty"; return 1; }
                shift ;;
            claude:--restricted|claude:--strict-mcp-config|claude:--disable-slash-commands|claude:--no-session-persistence) ;;
            codex:--sandbox)
                [ "${1:-}" = read-only ] || { refusal="--sandbox must be read-only"; return 1; }
                shift ;;
            codex:--disable)
                [[ " $SUPERVISOR_CODEX_DISABLED " == *" ${1:-} "* ]] || { refusal="--disable ${1:-} is not one of the supervisor's"; return 1; }
                word="--disable $1"; shift ;;
            codex:-c)
                [ "${1:-}" = web_search=disabled ] || { refusal="-c ${1:-} is not the supervisor's"; return 1; }
                word="-c $1"; shift ;;
            codex:--skip-git-repo-check|codex:--ephemeral|codex:--ignore-user-config) ;;
            *) refusal="$word is not one of the supervisor's flags"; return 1 ;;
        esac
        [[ "$seen" == *" $word "* ]] && { refusal="$word is given twice"; return 1; }
        seen+="$word "
    done
    case $agent in
        claude)
            for f in --tools --restricted --strict-mcp-config; do
                [[ "$seen" == *" $f "* ]] || { refusal="$f is missing"; return 1; }
            done ;;
        codex)
            [[ "$seen" == *" --sandbox "* ]] || { refusal="--sandbox read-only is missing"; return 1; }
            for f in $SUPERVISOR_CODEX_DISABLED; do
                [[ "$seen" == *" --disable $f "* ]] || { refusal="--disable $f is missing"; return 1; }
            done
            [[ "$seen" == *" -c web_search=disabled "* ]] || { refusal="-c web_search=disabled is missing"; return 1; } ;;
    esac
}

# Sets d_* from the directive (`harness directive effective`): its quiet (a pause, or a wake_at not
# reached) unless a wake event ended it, its width, models, focus and avoid. Without SUPERVISOR=1,
# or with no directive, an expired one or one that no longer validates, none is set and the
# configuration's own formulas hold; `effective` logs an expired or invalid one once.
directive_read() {
    local out k v
    d_quiet=""; d_until=""; d_why=""; d_woken=""; d_width=""; d_models=""; d_focus=""; d_avoid=""
    [ -n "$supervisor" ] || return 0
    if ! out=$(harness_supervisor directive effective 2>/dev/null); then
        log_changed "ERROR harness directive effective failed; defaults in force"
        return 0
    fi
    while IFS='=' read -r k v; do
        case $k in
            quiet) d_quiet=$v ;;
            until) d_until=$v ;;
            quiet_reason) d_why=$v ;;
            woken) d_woken=$v ;;
            width) [[ "$v" =~ ^[0-9]+$ ]] && d_width=$v ;;
            models) d_models=$v ;;
            focus) d_focus=$v ;;
            avoid) d_avoid=$v ;;
        esac
    done <<<"$out"
}
directive_lane() {
    local n f="" a=""
    for n in $d_focus; do f+="${f:+, }#$n"; done
    for n in $d_avoid; do a+="${a:+, }#$n"; done
    [ -n "$f" ] && printf ' The supervisor asks: among the issues you may claim, prefer %s.' "$f"
    [ -n "$a" ] && printf ' The supervisor asks: do not claim %s.' "$a"
    return 0
}

# Prints one line per thing the instance lacks; a tick cannot run without any of them.
instance_missing() {
    local root argv a bin k name s own accounts
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
            for a in $(list_agents); do
                case " $LOCAL_AGENTS " in
                    *" $a "*)
                        bin=$(agent_bin "$a")
                        type -P -- "$bin" >/dev/null || echo "an executable AGENT_BIN_$a ($bin)" ;;
                    *) echo "$HARNESS_ENV_FILE: agent $a needs FLEET_DISPATCH=1; only $LOCAL_AGENTS run locally" ;;
                esac
            done
        fi
    fi
    for a in $(list_agents); do
        # Only claude's cache and codex's rollouts are found without being named. An account of
        # another agent may name its own, else it takes the agent's.
        name="USAGE_SOURCE_$a"
        case $a in claude|codex) ;; *)
            for s in $(list_seats | grep -E "^$a(@|\$)"); do
                own=""
                [ "$s" != "$a" ] && own="USAGE_SOURCE_${a}_$(printf '%s' "${s#*@}" | tr - _)"
                [ -n "${!name:-}" ] || { [ -n "$own" ] && [ -n "${!own:-}" ]; } \
                    || echo "$HARNESS_ENV_FILE: ${own:+$own or }$name unset: name $s's usage source, or none"
            done ;;
        esac
        # An empty pattern matches every line, so every attempt would read as a usage limit.
        name="AGENT_LIMIT_RE_$a"
        [ -n "${!name+x}" ] && [ -z "${!name}" ] && echo "$HARNESS_ENV_FILE: $name is empty"
    done
    # Keys this harness does not read, which an older harness.env may still set: a tick would run
    # without what they ask for, so each must move to the key named after it. Only the file is read
    # for them: an environment that happens to carry one is not this config.
    if [ -r "$HARNESS_ENV_FILE" ]; then
        for k in CLAUDE_BIN:AGENT_BIN_claude CLAUDE_FLAGS:AGENT_FLAGS_claude TICK_AGENT_EXES: \
            FIVE_HALF_AT:USAGE_STEPS_SHORT FIVE_ONE_AT:USAGE_STEPS_SHORT FIVE_NONE_AT:USAGE_STEPS_SHORT \
            WEEK_HALF_AT:USAGE_STEPS_LONG WEEK_ONE_AT:USAGE_STEPS_LONG WEEK_NONE_AT:USAGE_STEPS_LONG \
            USAGE_TICK_STOP_WEEK_PCT:USAGE_TICK_STOP_LONG_PCT; do
            name=${k%%:*}
            grep -qE "^[[:space:]]*(export[[:space:]]+)?$name=" "$HARNESS_ENV_FILE" || continue
            if [ -n "${k#*:}" ]; then echo "$HARNESS_ENV_FILE: $name is not read; set ${k#*:} instead"
            else echo "$HARNESS_ENV_FILE: $name is not read; remove it"; fi
        done
    fi
    # Every tick starts through it, with SIGINT and SIGQUIT restored (BusyBox's and coreutils' before
    # 8.31 have no such option).
    env --default-signal=INT,QUIT true >/dev/null 2>&1 \
        || echo "an env that takes --default-signal (GNU coreutils 8.31 or later), which starts each tick"
    # A tick runs the lint through it before every post, and the launcher counts ticks and usage with it.
    type -P -- "$HARNESS_BIN" >/dev/null || echo "an executable HARNESS_BIN ($HARNESS_BIN)"
    [ -r "$INSTANCE_PROMPT" ] || echo "$INSTANCE_PROMPT"
    [ -r "$CORE_PROMPT" ] || echo "$CORE_PROMPT"
    # An entry naming an account must name a model too: `claude@work` alone reads as a model. A bare
    # model may hold `@` otherwise, as a Vertex model id does.
    for s in $(model_list) $(lane_on && printf '%s' "$LANE_MODEL") $([ -n "$supervisor" ] && printf '%s' "${SUPERVISOR_MODEL:-}"); do
        [[ "$s" == *@* && "$s" != *:* && "${s%%@*}" =~ ^(claude|codex|gemini)$ && "${s#*@}" =~ ^[a-z0-9][a-z0-9-]{0,31}$ ]] \
            && echo "$HARNESS_ENV_FILE: '$s' names an account but no model: write agent@account:model"
    done
    # Each account named must be a folder a tick can run with; a fleet device keeps its own, and the
    # supervisor always runs here.
    accounts=$(list_seats | grep '@')
    if [ -n "$accounts" ] && [ -n "$fleet" ]; then
        for s in $accounts; do
            echo "$HARNESS_ENV_FILE: $s names an account; a FLEET_DISPATCH=1 tick runs on the device's own, so name none"
        done
        accounts=""
    fi
    if [ -n "$supervisor" ]; then
        s=$(supervisor_entry); s=${s%%:*}
        [[ "$s" == *@* ]] && accounts+="${accounts:+$'\n'}$s"
    fi
    if [ -n "$accounts" ]; then
        # shellcheck disable=SC2046
        account_env $(printf '%s\n' "$accounts" | awk '!seen[$0]++') 2>&1 >/dev/null | sed 's/^/an account folder for /'
    fi
    if [ -n "$supervisor" ]; then
        [ -r "$SUPERVISOR_CORE" ] || echo "$SUPERVISOR_CORE"
        a=$(supervisor_entry); a=${a%%:*}; a=${a%%@*}
        if supervisor_argv "$a" '<model>'; then
            type -P -- "${argv[0]}" >/dev/null || echo "an executable AGENT_BIN_$a (${argv[0]}) for the supervisor"
        else
            echo "$HARNESS_ENV_FILE: SUPERVISOR_MODEL: $refusal"
        fi
    fi
}

if [ "${1:-}" != check ]; then
    harness_hook env.sh; rc=$?
    [ "$rc" -eq 0 ] || [ "$rc" -eq 3 ] \
        || log_changed "hooks/env.sh failed (exit $rc); ticks run without it"
fi
# What the core prompt, the lint and a tick's gh calls read; none of it is secret. No GitHub token
# reaches a tick, whoever exported it: the config's is for the cron scripts and the dashboard.
export TARGET_REPO WORKTREE_ROOT GH_REPO PUBLIC_RECORD_RULES PUBLIC_RECORD_PR_SECTIONS PUBLIC_RECORD_ISSUE_SECTIONS \
    ISSUE_LABEL WIP_LABEL READY_LABEL SIGNOFF_LABEL OPERATOR_LABEL LONG_LABEL FIRST_LABEL LOW_LABEL LANE_LABEL \
    TICK_AGENT FLEET_DISPATCH FLEET_DEVICE FLEET_BIN FLEET_CONFIG HARNESS_BIN
export -n GH_TOKEN GITHUB_TOKEN
# What libexec/claim.sh needs to keep claims.log, the supervisor's loop signal; a tick sees it only
# while the supervisor is on.
[ -n "$supervisor" ] && export HARNESS_SUPERVISOR=1

# The width the usage steps scale from. Slots past it and up to MAX_SLOTS are left for the landing tick.
width=${TICK_WIDTH:-$MAX_SLOTS}

# Sets pool_cap, the ticks the agents' usage windows allow at once; room, how many more may start,
# each agent's cap less its live ticks summed; usage_line, each agent's cap and windows for the log;
# and unknown, the agents without a fresh reading. A failed reading allows nothing.
compute_pool() {
    local doc err
    pool_cap=0; room=0; usage_line="usage=unknown"; unknown=""
    # A lane agent with no lane tick to run would size the pool for runners that find no entry.
    pool_lane=""; lane_wanted && pool_lane=$lane_entry
    err=$(mktemp)
    if doc=$(harness_usage --json 2>"$err"); then
        { read -r pool_cap; read -r room; read -r usage_line; read -r unknown; } < <(jq -r '
            def windows: [.windows[] | "\(.name):\(.used_pct | if . == floor then floor else . end)%"];
            def key: .agent + (if .account then "@" + .account else "" end);
            .pool_cap, .room,
            ([.agents[] | "\(key)=\(.cap)[" + (.reading.state as $s
                | if $s == "fresh" or $s == "unmetered" then windows
                  elif $s == "holding" then ["holding"] + windows
                  else ["unknown"] end | join(",")) + "]"] | join(" ")),
            ([.agents[] | select(.reading.state == "stale" or .reading.state == "unreadable") | key] | join(" "))' <<<"$doc")
        [[ "$pool_cap" =~ ^[0-9]+$ ]] || pool_cap=0
        [[ "$room" =~ ^[0-9]+$ ]] || room=0
    else
        log_changed "ERROR harness usage failed ($(tail -n 1 "$err" | cut -c1-200)); starting nothing"
    fi
    rm -f "$err"
}

# Sets ticks to this state directory's live ticks, `pid age slot agent model kind` a line, from
# `harness ticks`. A failed read is logged and counts none.
read_ticks() {
    local err
    err=$(mktemp)
    if ! ticks=$("$HARNESS_BIN" ticks 2>"$err"); then
        log_changed "ERROR harness ticks failed ($(tail -n 1 "$err" | cut -c1-200)); counting no live tick"
        ticks=""
    fi
    rm -f "$err"
}
# How many of the live ticks of kind $1 (any when empty) are younger than CLAIM_GRACE_S.
young_ticks() {
    # The supervisor claims nothing, so it never stands for a queued issue.
    awk -v grace="${CLAIM_GRACE_S:-360}" -v kind="$1" 'NF && $2 < grace && (kind == "" ? $6 != "supervisor" : $6 == kind)' <<<"$ticks" | wc -l
}

# Sets claimable, young and want: queued issues not already about to be claimed.
compute_want() {
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
    read_ticks
    # The supervisor's queue event compares the open queue issues the count left with those at its
    # last run.
    [ -n "$supervisor" ] && [ -s "$SUP/queue.new" ] && mv -f "$SUP/queue.new" "$SUP/queue"
    young=$(young_ticks "")
    want=$(( claimable - young ))
    [ "$want" -lt 0 ] && want=0
}

# The best entry that is not cooling down and not among $1, the entries this tick tried, whose
# agent runs fewer ticks than its usage windows allow (harness usage pick). Further arguments go to
# the pick: --landing, --entry <entry>.
pick_model() {
    local tried=$1 out err rc; shift
    err=$(mktemp)
    out=$(harness_usage pick --tried "$tried" "$@" 2>"$err"); rc=$?
    [ "$rc" -le 1 ] || log_changed "ERROR harness usage pick failed ($(tail -n 1 "$err" | cut -c1-200)); nothing run"
    rm -f "$err"
    printf '%s' "$out"
    return "$rc"
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
    local out rc err total=""
    err=$(mktemp)
    # With the supervisor on, the queue's count also leaves the open queue issues, claimed ones
    # included, in $SUP/queue.new: its queue event weighs that number, which a claim never moves.
    if [ -n "$supervisor" ] && [ "$1" = "$ISSUE_LABEL" ]; then
        mkdir -p "$SUP" && total="$SUP/queue.new" && rm -f "$total"
    fi
    # gh reads the config's token, so the count gets it, through its environment only: a token in
    # argv shows in /proc/<pid>/cmdline, which ps and the dashboard read. A tick never gets it.
    out=$(
        exec 2>"$err"
        [ -n "${GH_TOKEN:-}" ] && export GH_TOKEN
        [ -n "$total" ] && export QUEUE_TOTAL_FILE="$total"
        if [ -n "${QUEUE_DEPTH_CMD:-}" ]; then
            # shellcheck disable=SC2086
            GH_REPO="${GH_REPO:-}" ISSUE_LABEL="$1" WIP_LABEL="$WIP_LABEL" SIGNOFF_LABEL="$SIGNOFF_LABEL" \
                OPERATOR_LABEL="$OPERATOR_LABEL" FIRST_LABEL="${FIRST_LABEL:-}" LOW_LABEL="${LOW_LABEL:-}" \
                LANE_LABEL="${LANE_LABEL:-}" exec $QUEUE_DEPTH_CMD
        fi
        cd "$TARGET_REPO" || exit 1
        if [ -n "$total" ]; then
            both=$(gh issue list --repo "${GH_REPO:-}" --label "$1" --state open --limit 100 --json labels \
                --jq "\"\([.[] | select(([.labels[].name] | index(\"$WIP_LABEL\")) | not)] | length) \(length)\"") || exit
            printf '%s\n' "${both#* }" > "$total"
            printf '%s\n' "${both%% *}"
            exit 0
        fi
        exec gh issue list --repo "${GH_REPO:-}" --label "$1" --state open --limit 100 \
            --json labels --jq "[.[] | select(([.labels[].name] | index(\"$WIP_LABEL\")) | not)] | length"
    )
    rc=$?
    queue_error="label $1: exit $rc, output '$(printf '%s' "$out" | tr '\n' ' ' | cut -c1-60)', stderr '$(tail -n 1 "$err" | cut -c1-160)'"
    rm -f "$err"
    [ "$rc" -eq 0 ] && [[ "$out" =~ ^[0-9]+$ ]] || return 1
    queue_n=$out
}

# True while the lane cannot run at all: its entry cools down, or its agent's cap is 0. --landing
# ignores how many ticks that agent runs, so a lane only full for now stays open, and work ticks
# keep off its issues.
lane_closed() {
    entry_cooling "$lane_entry" && return 0
    ! pick_model "" --entry "$lane_entry" --landing >/dev/null
}
# True when this tick should run on the model lane. Decided once per launcher firing and per tick a
# runner starts: clear lane_known to decide again.
lane_known=""
lane_wanted() {
    if [ -z "$lane_known" ]; then
        lane_known=1; lane_yes=""
        lane_decide && lane_yes=1
    fi
    [ -n "$lane_yes" ]
}
lane_decide() {
    lane_on && [ -n "${QUEUE_DEPTH_CMD:-}" ] || return 1
    lane_closed && return 1
    local want_l
    if ! queue_depth "$LANE_LABEL"; then
        log_changed "ERROR lane queue count failed ($queue_error); no lane tick"
        return 1
    fi
    want_l=$queue_n
    [ "$want_l" -gt 0 ] || return 1
    # The count excludes issues a live tick holds, so only a lane tick young enough
    # to be still choosing its issue stands for one of them (compute_want's rule).
    read_ticks
    [ "$(young_ticks lane)" -lt "$want_l" ] || return 1
    # The lane's agent needs room under its cap like any other.
    pick_model "" --entry "$lane_entry" >/dev/null
}

# A merge slot's landing tick: it may start past the width the usage windows allow, so it
# claims nothing but an issue waiting for the slot and never chains.
LANDING_MARK="This tick is the merge slot's landing tick"
LANDING_LANE="$LANDING_MARK: claim only an issue labelled \`$READY_LABEL\`. If none is claimable, print a one-line summary and end."

# {{KEY}} takes the key's value through a sed escape, never eval, so a value cannot run as code.
PROMPT_KEYS="GH_REPO ISSUE_LABEL WIP_LABEL READY_LABEL SIGNOFF_LABEL OPERATOR_LABEL LONG_LABEL LANE_LABEL TICK_BUDGET_MIN LONG_TICK_BUDGET_MIN USAGE_CEILING_PCT"
SUPERVISOR_PROMPT_KEYS="FIRST_LABEL LOW_LABEL SUPERVISOR_BUDGET_MIN SUPERVISOR_MAX_DIRECTIVE_S SUPERVISOR_STOP_GRACE_S SUPERVISOR_MAX_PARKS_PER_DAY"
prompt_subst() {  # sets subst to the sed expressions for the keys $@
    local k v
    subst=()
    for k in "$@"; do
        v=$(printf '%s' "${!k:-}" | tr '\n' ' ' | sed 's/[\\&|]/\\&/g')
        subst+=(-e "s|{{$k}}|$v|g")
    done
}
compose_prompt() {  # $1 = the lane line
    local subst
    # shellcheck disable=SC2086
    prompt_subst $PROMPT_KEYS
    printf '%s\n\n' "$1"
    sed "${subst[@]}" "$CORE_PROMPT"
    printf '\n## This installation\n\n'
    sed "${subst[@]}" "$INSTANCE_PROMPT"
}
# GH_TOKEN reaches `harness health`, as it reaches the queue count, never the agent.
compose_supervisor_prompt() {
    local subst doc
    # shellcheck disable=SC2086
    prompt_subst $PROMPT_KEYS $SUPERVISOR_PROMPT_KEYS
    sed "${subst[@]}" "$SUPERVISOR_CORE"
    if [ -r "$SUPERVISOR_PROMPT" ]; then
        printf '\n## This installation\n\n'
        sed "${subst[@]}" "$SUPERVISOR_PROMPT"
    fi
    printf '\n## Health snapshot\n\n'
    if ! doc=$( [ -n "${GH_TOKEN:-}" ] && export GH_TOKEN; "$HARNESS_BIN" health --json 2>&1 ); then
        doc=$(jq -nc --arg e "$(printf '%s' "$doc" | tail -n 1 | cut -c1-300)" '{error: ("harness health failed: " + $e)}')
    fi
    printf '%s\n' "$doc"
}

# Records process $1 as this slot's tick, running agent $2 (agent@account on an account) on model $3
# as a tick of kind $4: its pid, its start time, field 22 of /proc/<pid>/stat, which stays readable
# for a non-dumpable process and which a reused pid does not share, and the boot that start time
# counts from, so a file a crash left behind matches no process after a reboot. Written whole, by
# rename, and readable by this user only.
record_tick() {
    local stat start boot
    local -a fields
    stat=$(cat "/proc/$1/stat" 2>/dev/null) || return 0
    read -ra fields <<<"${stat##*) }"
    start=${fields[19]:-}
    boot=$(cat /proc/sys/kernel/random/boot_id 2>/dev/null)
    [[ "$start" =~ ^[0-9]+$ ]] && [ -n "$boot" ] || return 0
    (umask 077 && printf '%s %s %s %s %s %s\n' "$1" "$start" "$boot" "$2" "$3" "$4" >"$slot_file.$$" \
        && mv -f "$slot_file.$$" "$slot_file")
}

# Whether attempt status $2 of agent $1 can be a usage limit. Gemini exits non-zero on every error
# it ends on, a quota error included, so its message echoed by a tick that went on is not one.
# Claude's and Codex's status on a limit is not known, so for them the message alone decides.
limit_failed() {
    [ "$1" != gemini ] || [ "$2" -ne 0 ]
}

# Whether the attempt's output, from byte $1 of the tick log, ends on agent $2's usage-limit message.
# For claude and codex, one of its last 5 non-empty lines starts with it, after spaces and an error
# prefix such as `Error:` or `ERROR:`: an agent that quotes the sentence mid-output, from an issue
# body or a diff, writes more after it. Gemini ends a quota error with a stack trace, or wraps it in
# `[API Error: ...]`, so for it the message may be anywhere in the last 40 lines; limit_failed has
# already required the failure that makes it one.
limit_message() {
    local re
    re=$(agent_limit_re "$2")
    if [ "$2" = gemini ]; then
        tail -c +"$1" "$log" | tail -n 40 | grep -qE -- "$re"
    else
        tail -c +"$1" "$log" | grep -v '^[[:space:]]*$' | tail -n 5 \
            | grep -qE -- "^[[:space:]]*(([A-Za-z]*[Ee]rror|ERROR):[[:space:]]*)?($re)"
    fi
}

# Prints each device `fleet status --json` reports, then `@missing <agent>` for each agent of the
# model list that no reachable device offers for dispatch (or FLEET_DEVICE, when that names one).
# fleet exits 1 when any device failed, with the document complete, so only the document decides.
fleet_check() {
    # The document is read as text, so one that is not JSON is reported like one of the wrong shape.
    # shellcheck disable=SC2016
    timeout "${FLEET_CHECK_TIMEOUT_S:-60}" "$FLEET_BIN" status --json 2>/dev/null | jq -Rrs \
        --argjson agents "$(list_agents | jq -Rsc 'split("\n") | map(select(. != ""))')" \
        --arg wanted "${FLEET_DEVICE:-auto}" '
        def need($k): if type == "object" and has($k) then .[$k] else error("no \($k) in \(type)") end;
        def truthy: . != null and . != false and . != 0 and . != "" and . != [] and . != {};
        def str: if type == "string" then . else tojson end;
        def device:
            if need("ok") | truthy | not then
                need("error") as $e
                | {line: "fleet device:  \(need("name") | str) (\(need("transport") | str)) unreachable: \($e | need("code") | str): \($e | need("message") | str)",
                   usable: []}
            else
                . as $d
                | [need("status") | need("adapters") | if type == "array" then .[] else error("adapters is not a list") end] as $adapters
                | [$agents[] as $agent
                   | ([$adapters[] | select(need("name") == $agent)] | first) as $a
                   | {agent: $agent,
                      offer: (if $a then "\($agent) \($a | need("running") | str)/\($a | need("max_concurrent") | str) running" else "no \($agent) adapter" end),
                      usable: ($a != null and ($a.max_concurrent | if type == "number" then . > 0 else error("max_concurrent is not a number") end)
                               and ($wanted == "auto" or $wanted == $d.name))}] as $offers
                | {line: "fleet device:  \($d | need("name") | str) (\($d | need("transport") | str)) reachable, \([$offers[].offer] | join(", "))",
                   usable: [$offers[] | select(.usable) | .agent]}
            end;
        [try (
            (if test("^\\s*$") then error("empty output") else fromjson end)
            | if need("version") != 1 then error("version \(.version | str), not 1") else . end
            | need("devices") | if type == "array" then .[] else error("devices is not a list") end
            | device
        ) catch {error: (tostring | gsub("\\s+"; " ") | .[:160])}] as $items
        | ($items[] | if has("error") then "fleet:         fleet status --json gave no status document (\(.error))" else .line end),
          ([$items[] | .usable[]?] as $ok
           | $agents[] | select(. as $agent | any($items[]; has("error")) or ($ok | any(. == $agent) | not))
           | "@missing \(.)")'
}

if [ "${1:-}" = check ]; then
    echo "harness home:  $HARNESS_HOME"
    echo "instance:      $HARNESS_INSTANCE_DIR"
    echo "config:        $HARNESS_ENV_FILE"
    echo "state:         $HARNESS_STATE"
    echo "target repo:   ${TARGET_REPO:-}"
    echo "worktree root: ${WORKTREE_ROOT:-}"
    echo "repository:    ${GH_REPO:-(unset)}"
    echo "queue:         ${QUEUE_DEPTH_CMD:-gh issue list --label $ISSUE_LABEL, in the target repo}"
    echo "default agent: $TICK_AGENT"
    if [ -n "$fleet" ]; then
        echo "dispatch:      fleet: $FLEET_BIN dispatch --device ${FLEET_DEVICE:-auto}, config ${FLEET_CONFIG:-$HARNESS_INSTANCE_DIR/fleet.toml}"
    else
        echo "dispatch:      local"
    fi
    for a in $(list_agents); do
        if [ -n "$fleet" ]; then echo "agent $a: fleet adapter $a"
        elif [[ " $LOCAL_AGENTS " == *" $a "* ]]; then agent_argv "$a" '<model>'; echo "agent $a: ${argv[*]} < prompt"
        else echo "agent $a: no local profile"; fi
    done
    echo "models:        $(model_list)"
    if lane_on; then echo "model lane:    $LANE_MODEL for \`$LANE_LABEL\`"; pool_lane=$lane_entry; else echo "model lane:    off"; fi
    if [ -n "$supervisor" ]; then
        echo "supervisor:    $(supervisor_entry), budget $SUPERVISOR_BUDGET_MIN min, at least ${SUPERVISOR_MIN_INTERVAL_S:-600}s apart, at most ${SUPERVISOR_INTERVAL_S:-1800}s"
        e=$(supervisor_entry); a=${e%%:*}
        if supervisor_argv "${a%%@*}" "${e#*:}"; then echo "supervisor:    runs with no tools: ${argv[*]} < prompt"
        else echo "supervisor:    cannot run: $refusal"; fi
        echo "directive:     $(harness_supervisor directive show 2>&1 | head -1)"
    fi
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
    if type -P -- "$HARNESS_BIN" >/dev/null; then
        err=$(mktemp)
        if out=$(harness_usage 2>"$err"); then
            printf '%s\n' "$out" | sed 's/^/usage:         /'
        else
            missing+="${missing:+$'\n'}a usage configuration that harness usage reads ($(tail -n 1 "$err"))"
        fi
        rm -f "$err"
        # Each account folder found, its sign-in checked again when the last check is old.
        [ -n "${AGENT_ACCOUNTS_DIR:-}" ] && harness_usage accounts 2>&1 | sed 's/^/account:       /'
    fi
    if [ -n "$fleet" ] && type -P -- "$FLEET_BIN" >/dev/null; then
        out=$(fleet_check)
        printf '%s\n' "$out" | grep -v '^@missing '
        for a in $(printf '%s\n' "$out" | sed -n 's/^@missing //p'); do
            missing+="${missing:+$'\n'}a fleet device that can take a $a tick"
        done
    fi
    if [ -n "$missing" ]; then
        printf '%s\n' "$missing" | sed 's/^/missing: /'
        exit 1
    fi
    echo "instance complete"
    exit 0
fi

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
    directive_read
    [ -n "$d_quiet" ] && exit 0

    slot=""
    s=1
    while [ "$s" -le "$slotmax" ]; do
        exec {fd}>"$LOCKS/slot$s.lock"
        if flock -n "$fd"; then slot=$s; break; fi
        eval "exec $fd>&-"
        s=$(( s + 1 ))
    done
    # No free slot: another runner won the race.
    [ -z "$slot" ] && exit 0
    slot_file="$LOCKS/slot$slot.tick"
    usage_refresh

    # A slot does not wait for the next cron firing: when a tick ends early and work is
    # still queued, the slot starts the next tick at once, each with a fresh context.
    chain=0
    while :; do
        ts=$(date -u +%Y%m%dT%H%M%SZ)
        log="$LOGS/tick-$ts-slot$slot.log"
        cd "$TARGET_REPO" || exit 1
        start_s=$(date +%s)
        # Run on the best entry that is not cooling down and whose agent has room under its usage
        # windows. A tick that dies on its usage limit cools that entry and re-runs at once on the
        # next one, which may be another agent's.
        tried=""; entry=""; rc=0
        while :; do
            if [ -n "$landing" ]; then
                kind=landing
                entry=$(pick_model "$tried" --landing)
                lane="$LANDING_LANE"
            elif lane_wanted && [[ " $tried " != *" $lane_entry "* ]]; then
                kind=lane
                entry=$lane_entry
                lane="This tick runs on ${lane_entry#*:}, the model lane: claim only an issue labelled \`$LANE_LABEL\`. If none is claimable, print a one-line summary and end."
            else
                kind=work
                entry=$(pick_model "$tried")
                if ! lane_on; then
                    lane="This tick runs on ${entry#*:}."
                elif lane_closed; then
                    # A lane that cannot run must not leave its issues to no one.
                    log_changed "$lane_entry cannot run (its usage limit or windows); work ticks may claim $LANE_LABEL issues"
                    lane="This tick runs on ${entry#*:}. ${lane_entry#*:} cannot run now, on its usage limit or windows, so an issue labelled \`$LANE_LABEL\` may be claimed too."
                else
                    lane="This tick runs on ${entry#*:}: never claim an issue labelled \`$LANE_LABEL\`; those are worked on the model lane."
                fi
            fi
            lane+=$(directive_lane)
            if [ -z "$entry" ]; then
                # Logged once, not as a tick log of its own: those would push real ones out.
                log_changed "no entry of '$(model_list)' can run: each cools down or its agent is at its usage cap; nothing run"
                break
            fi
            seat=${entry%%:*}; agent=${seat%%@*}; model=${entry#*:}
            # A tick on an account runs with the agent's variable naming its folder; an entry whose
            # folder is refused is passed over, as one on its usage limit is.
            account_args=()
            if [ "$seat" != "$agent" ]; then
                if [ -n "$fleet" ]; then why="a FLEET_DISPATCH=1 tick runs on the device's own account"
                elif out=$(account_env "$seat" 2>&1); then why=""; mapfile -t account_args <<<"$out"
                else why=$out; fi
                if [ -n "$why" ]; then
                    log_changed "$entry not run: $why"
                    tried="$tried $entry"
                    continue
                fi
            fi
            # The tick's own description, for its tool shells (libexec/usage-ok.sh reads its agent).
            marker="state=$HARNESS_STATE slot=$slot agent=$seat model=$model kind=$kind"
            run_s=$(date +%s)
            # Only this attempt's output may decide its fate: the log also holds earlier attempts.
            log_from=$(( $(stat -c %s "$log" 2>/dev/null || echo 0) + 1 ))
            if [ -n "$fleet" ]; then
                # fleet exits with the agent's status and writes its output here, so what follows
                # reads it as a local run's. The timeout allows the longest budget and half an hour
                # to wrap up.
                dispatch=("$FLEET_BIN" dispatch --device "${FLEET_DEVICE:-auto}" --adapter "$agent"
                    --workdir "$TARGET_REPO" --model "$model" --wait --timeout $(( (LONG_TICK_BUDGET_MIN + 30) * 60 )))
                [ "$kind" = landing ] && dispatch+=(--label tick=landing)
                argv=("${dispatch[@]}")
            else
                agent_argv "$agent" "$model"
            fi
            # The tick is this shell's own child, so its pid is known, and its prompt comes on a
            # pipe, never on disk or in argv. The slot file is how `harness ticks` and
            # libexec/tick-pid.sh know it: they never read its environment, which a non-dumpable
            # agent keeps private.
            # In a session and process group of its own, so stopping the tick (TERM to its group)
            # reaches the agent and its tools but never this runner, which then writes the trailer.
            # setsid does not fork: a background job of a shell without job control is no group
            # leader, so $! stays the agent's pid. A background job also starts with SIGINT and
            # SIGQUIT ignored, which every tool would inherit, and the runner, started as one
            # itself, cannot reset them (bash keeps a signal ignored on entry ignored); env can, so
            # `kill -INT` and `timeout -s INT` reach the agent's tools.
            setsid env --default-signal=INT,QUIT ${account_args[@]+"${account_args[@]}"} HARNESS_TICK="$marker" "${argv[@]}" \
                < <(compose_prompt "$lane") >>"$log" 2>&1 &
            tick_pid=$!
            record_tick "$tick_pid" "$seat" "$model" "$kind"
            wait "$tick_pid"; rc=$?
            rm -f "$slot_file"
            limited=""
            [ $(( $(date +%s) - run_s )) -lt "${CHAIN_MIN_TICK_S:-180}" ] && limit_failed "$agent" "$rc" \
                && limit_message "$log_from" "$agent" && limited=1
            # The trailer makes a silently dead tick distinguishable from a quiet one.
            echo "[tick.sh] exit=$rc duration=$(( $(date +%s) - run_s ))s agent=$seat model=$model" >>"$log"
            if [ -n "$limited" ]; then
                mkdir -p "$MODEL_COOL"; touch "$MODEL_COOL/$entry"
                echo "$(date -u +%Y%m%dT%H%M%SZ) slot$slot $entry hit its usage limit; cooling ${MODEL_COOLDOWN_S}s" >>"$LOGS/allocator.log"
                tried="$tried $entry"
                continue
            fi
            break
        done

        # Only tick logs: the allocator's, the directive's and the supervisor's own logs live beside them.
        ls -1t "$LOGS"/tick-* 2>/dev/null | tail -n +$(( LOG_RETENTION + 1 )) | while read -r old; do rm -f "$old"; done
        chain=$(( chain + 1 ))
        dur=$(( $(date +%s) - start_s ))
        # A short tick found nothing to claim or failed: leave the retry to cron.
        [ "$dur" -lt "${CHAIN_MIN_TICK_S:-180}" ] && break
        [ -n "$landing" ] && break
        [ "$chain" -ge "${MAX_CHAIN:-24}" ] && break
        harness_paused && break
        directive_read
        [ -n "$d_quiet" ] && break
        # A directive's width only narrows the slots in use.
        [ -n "$d_width" ] && [ "$slot" -gt "$d_width" ] && break
        usage_refresh; lane_known=""
        compute_pool
        # The slot goes on only while the agent (or account) it ran can still run here: another's
        # room is the launcher's to fill.
        pick_model "" --agent "$seat" >/dev/null || break
        compute_want
        [ "$want" -lt 1 ] && break
        echo "$(date -u +%Y%m%dT%H%M%SZ) slot$slot chains tick $(( chain + 1 )) after ${dur}s: $usage_line pool=$pool_cap room=$room claimable=$claimable young=$young" \
            >>"$LOGS/allocator.log"
    done
    exit 0
fi

# One supervisor tick, outside the slots. The agent runs with no tools (supervisor_argv), in an
# empty directory, with an environment that holds only what its CLI needs to sign in: it can only
# answer. Its prompt and the health snapshot come on stdin; its answer, one JSON document at the
# end of its output, is what `harness supervisor answer` applies, the directive through its
# validation and each GitHub action through gh, every body linted. It holds supervisor.lock, so at
# most one runs, and its run is recorded under $HARNESS_STATE/supervisor/ for the next `due`.
if [ "${1:-}" = supervisor ]; then
    exec {sfd}>"$LOCKS/supervisor.lock"
    flock -n "$sfd" || exit 0
    entry=$(supervisor_entry); seat=${entry%%:*}; agent=${seat%%@*}; model=${entry#*:}
    # Recorded at once, as this shell, so the launcher that started it counts it toward its agent
    # before it sizes the pool; the agent's own record replaces it once it runs. libexec/tick-pid.sh
    # reads only the slots' files, so the record gives no claim.
    slot_file="$LOCKS/supervisor.tick"
    record_tick "$$" "$seat" "$model" supervisor
    trap 'rm -f "$slot_file"' EXIT
    harness_paused && exit 0
    if ! supervisor_argv "$agent" "$model"; then
        log_changed "supervisor not run: $refusal"
        exit 0
    fi
    sup_account=""
    if [ "$seat" != "$agent" ] && ! sup_account=$(account_env "$seat" 2>&1); then
        log_changed "supervisor not run: $sup_account"
        exit 0
    fi
    log="$LOGS/supervisor-$(date -u +%Y%m%dT%H%M%SZ).log"
    work=$(mktemp -d) && answer=$(mktemp) || exit 0
    trap 'rm -f "$slot_file" "$answer"; rm -rf "$work"' EXIT
    harness_supervisor supervisor begin --reason "${2:-}" >/dev/null 2>>"$log"
    run_s=$(date +%s)
    sup_env=("HOME=$HOME" "PATH=$PATH")
    # On an account, its folder is the only way it signs in: the variables `harness accounts env`
    # unsets or sets are not taken from here.
    for k in USER LOGNAME LANG LC_ALL TMPDIR TERM ANTHROPIC_API_KEY CLAUDE_CONFIG_DIR OPENAI_API_KEY CODEX_HOME XDG_CONFIG_HOME; do
        [ -n "$sup_account" ] && grep -qxE -- "(--unset=)?$k(=.*)?" <<<"$sup_account" && continue
        [ -n "${!k:-}" ] && sup_env+=("$k=${!k}")
    done
    [ -n "$sup_account" ] && sup_env+=("$(grep -v '^--unset=' <<<"$sup_account")")
    # In a session and process group of its own, as a tick runs; the lock stays this shell's alone.
    ( exec {sfd}>&-; cd "$work" && exec setsid env -i --default-signal=INT,QUIT "${sup_env[@]}" \
        timeout -k 30 "$(( SUPERVISOR_BUDGET_MIN * 60 ))" "${argv[@]}" ) \
        < <(compose_supervisor_prompt) >"$answer" 2>>"$log" &
    tick_pid=$!
    record_tick "$tick_pid" "$seat" "$model" supervisor
    wait "$tick_pid"; rc=$?
    dur=$(( $(date +%s) - run_s ))
    cat "$answer" >>"$log"
    echo "[tick.sh] exit=$rc duration=${dur}s agent=$seat model=$model" >>"$log"
    if [ "$dur" -lt "${CHAIN_MIN_TICK_S:-180}" ] && limit_failed "$agent" "$rc" && tail -n 40 "$log" | grep -qE -- "$(agent_limit_re "$agent")"; then
        mkdir -p "$MODEL_COOL"; touch "$MODEL_COOL/$entry"
        echo "$(date -u +%Y%m%dT%H%M%SZ) supervisor $entry hit its usage limit; cooling ${MODEL_COOLDOWN_S}s" >>"$LOGS/allocator.log"
    fi
    # The answer is applied only from a run that ended well; one that finds none counts as failed.
    if [ "$rc" -eq 0 ]; then
        harness_supervisor --gh supervisor answer "$answer" >>"$log" 2>&1; rc=$?
    fi
    harness_supervisor supervisor end --exit "$rc" --duration "$dur" >/dev/null 2>>"$log"
    ls -1t "$LOGS"/supervisor-* 2>/dev/null | tail -n +21 | while read -r old; do rm -f "$old"; done
    exit 0
fi

# -------------------------------------------------------------- launcher mode

if harness_paused; then
    # The supervisor's unpause event is the first firing that no longer finds this.
    [ -n "$supervisor" ] && mkdir -p "$SUP" && touch "$SUP/paused"
    exit 0
fi
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

# The supervisor runs outside the slots, so it is weighed before them: a stop whose grace ended is
# sent, a supervisor tick starts when one is due, and a directive's quiet ends the firing here.
if [ -n "$supervisor" ]; then
    harness_supervisor directive enforce >/dev/null 2>&1 || log_changed "ERROR harness directive enforce failed"
    err=$(mktemp)
    due=$(harness_supervisor --gh supervisor due 2>"$err"); rc=$?
    if [ "$rc" -eq 0 ]; then
        setsid "$0" supervisor "${due#due }" >/dev/null 2>&1 & sup_pid=$!
        # Counted before the pool is sized below: the supervisor records itself as it starts.
        n=0
        while [ ! -s "$LOCKS/supervisor.tick" ] && kill -0 "$sup_pid" 2>/dev/null && [ "$n" -lt 100 ]; do
            sleep 0.05; n=$(( n + 1 ))
        done
    elif [ "$rc" -ne 1 ]; then
        log_changed "ERROR harness supervisor due failed ($(tail -n 1 "$err" | cut -c1-200)); no supervisor tick"
    fi
    # On an account, held back for want of a reading, it would stay silent for ever: said once, until
    # it clears.
    if [ "$rc" -eq 1 ] && [[ "$due" == *"@"*" has no fresh usage reading"* ]]; then
        [ -e "$SUP/no-reading" ] || { touch "$SUP/no-reading"; echo "$(date -u +%Y%m%dT%H%M%SZ) supervisor $due" >>"$LOGS/allocator.log"; }
    else
        rm -f "$SUP/no-reading"
    fi
    rm -f "$err"
    directive_read
    if [ -n "$d_quiet" ]; then
        log_changed "directive: quiet ($d_quiet) until $d_until: $d_why"
        exit 0
    fi
    [ -n "$d_woken" ] && log_changed "directive: its quiet ended early on $d_woken"
    # A directive's width only narrows the slots the launcher fills; the windows still bound each agent.
    [ -n "$d_width" ] && [ "$width" -gt "$d_width" ] && width=$d_width
fi

# Cheapest gate: if every slot is held there is nothing to decide.
busy=0; free=0; s=1
while [ "$s" -le "$MAX_SLOTS" ]; do
    exec {fd}>"$LOCKS/slot$s.lock"
    if flock -n "$fd"; then flock -u "$fd"; free=$(( free + 1 )); else busy=$(( busy + 1 )); fi
    eval "exec $fd>&-"
    s=$(( s + 1 ))
done
[ "$free" -eq 0 ] && exit 0
# Every entry, the lane's too, cools down on its usage limit: a runner would only log that nothing
# ran. Anything less goes on, so a reading that allows nothing is logged below.
all_cooling=1
for e in $(model_list) $(lane_on && printf '%s' "$lane_entry"); do
    entry_cooling "$(entry_of "$e")" || { all_cooling=""; break; }
done
[ -n "$all_cooling" ] && exit 0

usage_refresh
compute_pool
if [ -n "$supervisor" ] && [ "$pool_cap" -eq 0 ]; then
    # Every cap at 0 with a reset ahead stays so until then: said once, not at each firing.
    at=$(harness_usage --json 2>/dev/null | jq -r '[.agents[]] as $a
        | if ($a | length) > 0 and all($a[]; .cap == 0)
          then [$a[].windows[] | select(.cap == 0) | .resets_at
                | if type == "number" then . elif type == "string"
                  then sub("\\.[0-9]+"; "") | sub("(\\+00:00|Z)$"; "Z") | (try fromdateiso8601 catch empty)
                  else empty end | select(. > now)] | min // empty
          else empty end' 2>/dev/null)
    [[ "$at" =~ ^[0-9]+$ ]] \
        && log_changed "every agent is at its usage cap; nothing starts before the earliest reset, $(date -u -d "@$at" +%Y%m%dT%H%M%SZ)"
fi
if [ -n "$unknown" ]; then
    # Say so at most hourly: the launcher fires every couple of minutes.
    stamp="$LOCKS/usage-unknown.stamp"
    if [ ! -f "$stamp" ] || [ $(( $(date +%s) - $(date -r "$stamp" +%s) )) -ge 3600 ]; then
        touch "$stamp"
        echo "$(date -u +%Y%m%dT%H%M%SZ) no fresh usage reading for $unknown; none of its ticks start" >>"$LOGS/allocator.log"
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

spawn=$(( width - busy ))
[ "$spawn" -gt "$room" ] && spawn=$room
[ "$spawn" -gt "$want" ] && spawn=$want
[ "$spawn" -lt 0 ] && spawn=0
# A landing needs little budget, but in a full pool it waits for a slot to free,
# which can outlast the merge slot. With the pool full (no slot inside the width, or no agent with
# room) and no young tick about to claim, one landing tick starts past the width for an issue
# waiting for the slot.
# A directive's width 0 starts nothing new, a landing tick included.
if [ "$spawn" -eq 0 ] && [ "$pool_cap" -gt 0 ] && [ "$d_width" != 0 ] && { [ "$busy" -ge "$width" ] || [ "$room" -eq 0 ]; } && [ "$free" -gt 0 ] \
    && [ "$young" -eq 0 ] && [ -n "${QUEUE_DEPTH_CMD:-}" ] \
    && ! awk '$6 == "landing" { found = 1 } END { exit !found }' <<<"$ticks"; then
    ready=0
    if queue_depth "$READY_LABEL"; then ready=$queue_n
    else log_changed "ERROR ready queue count failed ($queue_error); no landing tick"; fi
    if [ "$ready" -gt 0 ]; then
        slotmax=$(( busy + 1 )); [ "$slotmax" -gt "$MAX_SLOTS" ] && slotmax=$MAX_SLOTS
        echo "$(date -u +%Y%m%dT%H%M%SZ) $usage_line pool=$pool_cap room=$room busy=$busy ready=$ready -> spawning 1 landing tick" \
            >>"$LOGS/allocator.log"
        setsid "$0" runner "$slotmax" 0 landing >/dev/null 2>&1 &
        exit 0
    fi
fi
[ "$spawn" -eq 0 ] && exit 0   # quiet: no log spam on the frequent no-op firings

ts=$(date -u +%Y%m%dT%H%M%SZ)
echo "$ts $usage_line pool=$pool_cap room=$room busy=$busy claimable=$claimable -> spawning $spawn" \
    >>"$LOGS/allocator.log"

i=0
while [ "$i" -lt "$spawn" ]; do
    setsid "$0" runner "$width" $(( i * STAGGER_SECONDS )) >/dev/null 2>&1 &
    i=$(( i + 1 ))
done
exit 0
