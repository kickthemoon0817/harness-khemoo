#!/usr/bin/env bash
# Exercises the supervisor and its directive in a scratch harness home: a fake claude stands for the
# supervisor (it applies the directive $FAKE_DIRECTIVE names, or crashes) and for every tick (it
# records its lane line and argv); a fake queue counts five issues; processes carrying the runner's
# marker stand for live ticks. Each launch waits for every runner and supervisor tick it started.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
H=$(mktemp -d)
holders=()
cleanup() { for p in "${holders[@]}"; do kill "$p" 2>/dev/null; done; rm -rf "$H"; }
trap cleanup EXIT
mkdir -p "$H/bin" "$H/lib" "$H/libexec" "$H/prompts" "$H/instance/prompts" "$H/state/locks" "$H/state/logs" "$H/target" "$H/fakebin"
cp "$here/bin/tick.sh" "$H/bin/"; cp "$here/lib/instance.sh" "$H/lib/"
cp "$here/libexec/usage-ok.sh" "$here/libexec/tick-pid.sh" "$here/libexec/claim.sh" "$H/libexec/"
cp "$here/prompts/supervisor-core.md" "$H/prompts/"
echo "Core rules." > "$H/prompts/tick-core.md"
cp "$here/tests/fixtures/instance/prompts/tick.md" "$H/instance/prompts/tick.md"
cat > "$H/fakebin/claude" <<'EOF'
#!/usr/bin/env bash
prompt=$(cat)
case $prompt in
    "You are the supervisor"*)
        printf '%s\n' "$prompt" > "$FAKE_SUP_PROMPT"
        printf '%s\n' "${HARNESS_TICK:-unset}" "token=${GH_TOKEN:-unset}" > "$FAKE_SUP_ENV"
        [ -e "$FAKE_SUP_CRASH" ] && exit 3
        [ -s "$FAKE_DIRECTIVE" ] && "$HARNESS_BIN" directive apply "$FAKE_DIRECTIVE"
        exit 0 ;;
esac
printf '%s\n' "${prompt%%$'\n'*}" >> "$FAKE_LANES"
printf '%s\n' "$*" >> "$FAKE_ARGS"
EOF
cat > "$H/fakebin/queue" <<'EOF'
#!/usr/bin/env bash
case "$ISSUE_LABEL" in ai) echo 5 ;; *) echo 0 ;; esac
EOF
cat > "$H/fakebin/crontab" <<'EOF'
#!/usr/bin/env bash
echo "*/2 * * * * $HARNESS_HOME/bin/tick.sh"
EOF
# GitHub is never reached: every gh call fails, as an outage would.
printf '#!/usr/bin/env bash\nexit 1\n' > "$H/fakebin/gh"
chmod +x "$H/fakebin/"*
git -C "$H/target" init -q
write_env() {  # $1 = SUPERVISOR's value
    cat > "$H/instance/harness.env" <<EOF
TARGET_REPO=$H/target
WORKTREE_ROOT=$H/worktrees
GH_REPO=example-org/example-repo
GH_TOKEN=ghp_FAKEtokenFAKEtokenFAKEtoken0123456789
QUEUE_DEPTH_CMD=$H/fakebin/queue
AGENT_BIN_claude=$H/fakebin/claude
AGENT_FLAGS_claude=
TICK_MODELS="claude:model-a claude:model-b"
MAX_SLOTS=3
STAGGER_SECONDS=0
USAGE_CACHE=$H/usage-cache.json
USAGE_FETCH=$H/no-fetch
CLAIM_GRACE_S=0
SUPERVISOR=$1
SUPERVISOR_MIN_INTERVAL_S=3600
SUPERVISOR_INTERVAL_S=7200
SUPERVISOR_STOP_GRACE_S=2
EOF
}
write_env 1
unset HARNESS_TICK FLEET_SESSION HARNESS_STATE GH_TOKEN
export HARNESS_HOME="$H" HARNESS_PATH="$H/fakebin:$PATH" PATH="$H/fakebin:$PATH" \
    FAKE_LANES="$H/lanes.txt" FAKE_ARGS="$H/args.txt" FAKE_SUP_PROMPT="$H/sup-prompt.txt" FAKE_SUP_ENV="$H/sup-env.txt" \
    FAKE_SUP_CRASH="$H/sup-crash" FAKE_DIRECTIVE="$H/directive-in.json"
S="$H/state"; L="$S/locks"; LOG="$S/logs/allocator.log"; DLOG="$S/logs/directive.log"; SUP="$S/supervisor"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
usage() {  # $1 = the 5-hour window's percent; $2, when given, its reset as Unix seconds
    local reset=""
    [ -n "${2:-}" ] && reset=", \"fiveHourResetsAt\": \"$(date -u -d "@$2" +%Y-%m-%dT%H:%M:%S.000000+00:00)\""
    printf '{"timestamp": %s000, "data": {"fiveHourPercent": %s, "weeklyPercent": 10%s}}\n' "$(date +%s)" "$1" "$reset" > "$H/usage-cache.json"
}
until_ok() { local n=0; until "$@"; do [ "$n" -lt 400 ] || return 1; sleep 0.05; n=$((n + 1)); done; }
no_runner() { ! pgrep -f "$H/bin/tick.sh" >/dev/null; }
launch() {
    : > "$FAKE_LANES"; : > "$FAKE_ARGS"; : > "$LOG"
    "$H/bin/tick.sh"
    until_ok no_runner
}
at() { date -u -d "@$(( $(date +%s) + $1 ))" +%Y-%m-%dT%H:%M:%SZ; }
apply() {  # $1 = the directive's JSON; leaves the output in out and the status in rc
    printf '%s\n' "$1" > "$H/apply.json"
    out=$("$HARNESS_BIN" directive apply "$H/apply.json" 2>&1); rc=$?
}
ticks_run() { wc -l < "$FAKE_LANES" | tr -d ' '; }
reset_supervisor() { rm -rf "$SUP" "$S/directive.json" "$DLOG" "$FAKE_SUP_PROMPT" "$FAKE_SUP_ENV" "$FAKE_SUP_CRASH" "$FAKE_DIRECTIVE"; }
usage 10

# --- a fake supervisor applies a directive -------------------------------------------------------
printf '{"version": 1, "reason": "one tick on model-b", "expires_at": "%s", "width": 1, "models": ["claude:model-b"], "focus": [12, 15], "avoid": [7]}\n' "$(at 3600)" > "$FAKE_DIRECTIVE"
launch
check "the first firing starts a supervisor tick that applies its directive" \
    '[ -s "$S/directive.json" ] && grep -q "exit=0 .*reason=first" "$SUP/runs.log" && grep -q "apply: one tick on model-b" "$DLOG"' \
    "runs=$(cat "$SUP/runs.log" 2>/dev/null) dlog=$(cat "$DLOG" 2>/dev/null)"
check "the directive is owner-only" '[ "$(stat -c %a "$S/directive.json")" = 600 ]' "$(stat -c %a "$S/directive.json")"
check "the supervisor's prompt is its core rules, then the health snapshot" \
    'grep -q "^## Health snapshot" "$FAKE_SUP_PROMPT" && tail -n 1 "$FAKE_SUP_PROMPT" | jq -e ".version == 1 and .supervisor.enabled == true" >/dev/null' \
    "$(tail -c 300 "$FAKE_SUP_PROMPT" 2>/dev/null)"
check "the supervisor tick carries its marker and never GH_TOKEN, nor does its prompt" \
    'grep -qx "state=$S slot=0 agent=claude model=model-b kind=supervisor" "$FAKE_SUP_ENV" && grep -qx "token=unset" "$FAKE_SUP_ENV" && ! grep -q ghp_FAKE "$FAKE_SUP_PROMPT"' \
    "$(cat "$FAKE_SUP_ENV" 2>/dev/null)"

launch
check "the minimum interval keeps a second supervisor tick from starting" '[ "$(wc -l < "$SUP/runs.log")" -eq 1 ]' "$(cat "$SUP/runs.log")"
check "width: one tick starts where the windows allow three" \
    '[ "$(ticks_run)" = 1 ] && grep -q "allowed=1 " "$LOG"' "lanes=$(cat "$FAKE_LANES") log=$(cat "$LOG")"
check "models: the tick runs on the directive's entry" 'grep -q -- "--model model-b" "$FAKE_ARGS" && ! grep -q model-a "$FAKE_ARGS"' "$(cat "$FAKE_ARGS")"
check "focus and avoid reach the lane line" \
    'grep -q "prefer #12, #15\." "$FAKE_LANES" && grep -q "do not claim #7\." "$FAKE_LANES"' "$(cat "$FAKE_LANES")"

apply "{\"version\": 1, \"reason\": \"wide\", \"expires_at\": \"$(at 3600)\", \"width\": 3}"
usage 65
launch
check "width is clamped to the usage cap" '[ "$(ticks_run)" = 2 ] && grep -q "allowed=2 " "$LOG"' "lanes=$(cat "$FAKE_LANES") log=$(cat "$LOG")"
usage 10

# --- pause, wake_at and the human's pause --------------------------------------------------------
apply "{\"version\": 1, \"reason\": \"hold off\", \"expires_at\": \"$(at 3600)\", \"pause\": {\"until\": \"$(at 1800)\", \"reason\": \"the base branch is red\"}}"
launch
check "pause: no tick starts" '[ "$(ticks_run)" = 0 ] && grep -q "directive: quiet (pause) until .*: the base branch is red" "$LOG"' "lanes=$(cat "$FAKE_LANES") log=$(cat "$LOG")"

apply "{\"version\": 1, \"reason\": \"windows are full\", \"expires_at\": \"$(at 3600)\", \"wake_at\": \"$(at 1800)\"}"
launch
check "wake_at: the launcher stays quiet" '[ "$(ticks_run)" = 0 ] && grep -q "directive: quiet (wake)" "$LOG"' "lanes=$(cat "$FAKE_LANES") log=$(cat "$LOG")"
sleep 1.1
touch "$S/paused"
launch
check "the human's pause holds whatever the directive says" '[ "$(ticks_run)" = 0 ] && [ -e "$SUP/paused" ]' "lanes=$(cat "$FAKE_LANES")"
rm -f "$S/paused"
launch
check "lifting the human's pause is a wake event that ends the quiet" \
    '[ "$(ticks_run)" = 3 ] && grep -q "quiet ended early on unpaused" "$LOG"' "lanes=$(cat "$FAKE_LANES") log=$(cat "$LOG")"

apply "{\"version\": 1, \"reason\": \"all yours\", \"expires_at\": \"$(at 3600)\", \"width\": 3}"
touch "$S/paused"
launch
check "the human's pause beats a directive that allows ticks" '[ "$(ticks_run)" = 0 ]' "lanes=$(cat "$FAKE_LANES")"
rm -f "$S/paused"

# --- fallback ------------------------------------------------------------------------------------
now=$(date +%s)
jq -n --arg a "$(date -u -d "@$((now - 7200))" +%FT%TZ)" --arg e "$(date -u -d "@$((now - 3600))" +%FT%TZ)" \
    '{version: 1, reason: "old", applied_at: $a, expires_at: $e, width: 1}' > "$S/directive.json"
launch; first=$(ticks_run); launch
check "an expired directive is ignored: the defaults hold" '[ "$first" = 3 ] && [ "$(ticks_run)" = 3 ]' "first=$first then=$(ticks_run)"
check "an expired directive is logged once" '[ "$(grep -c "expired at .*defaults in force" "$DLOG")" -eq 1 ]' "$(cat "$DLOG")"

echo 'not json' > "$S/directive.json"
launch; first=$(ticks_run); launch
check "a malformed directive is ignored: the defaults hold" '[ "$first" = 3 ] && [ "$(ticks_run)" = 3 ]' "first=$first then=$(ticks_run)"
check "a malformed directive is logged once" '[ "$(grep -c "ignored, defaults in force" "$DLOG")" -eq 1 ]' "$(cat "$DLOG")"

apply "{\"version\": 1, \"reason\": \"r\", \"expires_at\": \"$(at 3600)\", \"width\": 1, \"speed\": 9}"
check "a directive with an unknown field is refused and logged" \
    '[ "$rc" -eq 1 ] && [[ "$out" == *"unknown field"* ]] && tail -n 1 "$DLOG" | grep -q "refused: .*unknown field"' "rc=$rc $out"
apply "{\"version\": 1, \"reason\": \"r\", \"expires_at\": \"$(at 3600)\", \"models\": [\"claude:model-z\"]}"
check "a model outside the configured list is refused" '[ "$rc" -eq 1 ] && [[ "$out" == *"not in the configured model list"* ]]' "rc=$rc $out"

# --- with SUPERVISOR unset, nothing of it applies ------------------------------------------------
apply "{\"version\": 1, \"reason\": \"hold off\", \"expires_at\": \"$(at 3600)\", \"pause\": {\"until\": \"$(at 1800)\", \"reason\": \"x\"}}"
write_env ""
runs=$(wc -l < "$SUP/runs.log")
rm -f "$SUP/last_start"
launch
check "SUPERVISOR unset: the directive is not read and no supervisor tick starts" \
    '[ "$(ticks_run)" = 3 ] && [ "$(wc -l < "$SUP/runs.log")" = "$runs" ] && ! grep -q "supervisor\\|directive" "$LOG"' "lanes=$(cat "$FAKE_LANES") log=$(cat "$LOG")"
"$HARNESS_BIN" directive clear >/dev/null
reset_at=$(( $(date +%s) + 5400 ))
usage 85 "$reset_at"
launch; launch; launch
check "every cap at 0 with a reset ahead is logged once, naming the reset" \
    '[ "$(ticks_run)" = 0 ] && [ "$(grep -c "nothing starts before the earliest reset, $(date -u -d "@$reset_at" +%Y%m%dT%H%M%SZ)" "$S/logs/allocator.log")" -eq 1 ]' \
    "$(cat "$S/logs/allocator.log")"
usage 10
write_env 1
echo "$(date +%s)" > "$SUP/last_start"

# --- notes reach the tick through usage-ok.sh ----------------------------------------------------
apply "{\"version\": 1, \"reason\": \"steer\", \"expires_at\": \"$(at 3600)\", \"notes\": {\"42\": \"rebase on the base branch first\"}}"
# One tick checks twice, then another tick claims the same issue. The trailing `:` keeps bash from
# exec-ing usage-ok.sh in its own place: the claim's pid is its parent.
notes=$(bash -c 'echo "42 $$ 2026-11-28T00:00:00Z" > "$1/claim.42"; "$2/libexec/usage-ok.sh"; echo ---; "$2/libexec/usage-ok.sh"; :' _ "$L" "$H" 2>&1)
first=${notes%%---*}; second=${notes#*---}
other=$(bash -c 'echo "42 $$ 2026-11-28T00:00:00Z" > "$1/claim.42"; "$2/libexec/usage-ok.sh"; :' _ "$L" "$H" 2>&1)
check "a note reaches the tick working its issue through usage-ok.sh" \
    '[[ "$first" == *"SUPERVISOR: on #42: rebase on the base branch first"* ]]' "$first"
check "the note is shown once to that tick" '[[ "$second" != *SUPERVISOR* ]]' "$second"
check "another tick on the issue is shown the note too" '[[ "$other" == *"SUPERVISOR: on #42:"* ]]' "$other"
rm -f "$L/claim.42"

# --- stop ----------------------------------------------------------------------------------------
# A stand-in tick in a process group of its own, holding its slot's lock open as a tick does.
HARNESS_TICK="state=$S slot=9 agent=claude model=model-a kind=work" setsid sleep 60 9>"$L/slot9.lock" & tick=$!; holders+=("$tick")
until_ok grep -qa "^HARNESS_TICK=" <(tr '\0' '\n' < "/proc/$tick/environ" 2>/dev/null)
sleep 60 & plain=$!; holders+=("$plain")
mkdir -p "$H/other/locks"
HARNESS_TICK="state=$H/other slot=9 agent=claude model=model-a kind=work" setsid sleep 60 9>"$H/other/locks/slot9.lock" & foreign=$!; holders+=("$foreign")
until_ok grep -qa "^HARNESS_TICK=" <(tr '\0' '\n' < "/proc/$foreign/environ" 2>/dev/null)
apply "{\"version\": 1, \"reason\": \"stop\", \"expires_at\": \"$(at 3600)\", \"stop\": [{\"pid\": $plain, \"reason\": \"x\"}]}"
check "a pid that is no tick is refused" '[ "$rc" -eq 1 ] && [[ "$out" == *"not a live tick of this installation"* ]]' "rc=$rc $out"
apply "{\"version\": 1, \"reason\": \"stop\", \"expires_at\": \"$(at 3600)\", \"stop\": [{\"pid\": $foreign, \"reason\": \"x\"}]}"
check "another installation's tick is refused" '[ "$rc" -eq 1 ] && [[ "$out" == *"not a live tick of this installation"* ]]' "rc=$rc $out"
apply "{\"version\": 1, \"reason\": \"stop\", \"expires_at\": \"$(at 3600)\", \"stop\": [{\"pid\": $tick, \"reason\": \"loops on #7\"}]}"
check "a live tick of this installation may be named" '[ "$rc" -eq 0 ]' "rc=$rc $out"
launch
check "the tick is not stopped before its grace" 'kill -0 "$tick" 2>/dev/null' "gone early"
sleep 2.2
launch
until_ok bash -c "! kill -0 $tick 2>/dev/null"
check "after its grace the tick gets TERM, and the stop is logged" \
    '! kill -0 "$tick" 2>/dev/null && grep -q "stop: TERM to pid $tick (process group $tick) after its grace: loops on #7" "$DLOG"' "$(tail -n 3 "$DLOG")"
check "nothing else was stopped" 'kill -0 "$plain" && kill -0 "$foreign"' "plain or foreign gone"
launch
check "a stop is sent once" '[ "$(grep -c "stop: .*pid $tick" "$DLOG")" -eq 1 ]' "$(grep "stop:" "$DLOG")"

# --- a supervisor that crashes leaves the defaults in force --------------------------------------
reset_supervisor
touch "$FAKE_SUP_CRASH"
printf '{"version": 1, "reason": "never applied", "expires_at": "%s", "width": 1}\n' "$(at 3600)" > "$FAKE_DIRECTIVE"
launch
check "a crashed supervisor's run is recorded as a failure" \
    'grep -q "exit=3 " "$SUP/runs.log" && [ "$(cat "$SUP/failures")" = 1 ] && [ ! -e "$S/directive.json" ]' \
    "runs=$(cat "$SUP/runs.log" 2>/dev/null)"
launch
check "after a crash the defaults hold and no supervisor tick starts within its backoff" \
    '[ "$(ticks_run)" = 3 ] && [ "$(wc -l < "$SUP/runs.log")" -eq 1 ]' "lanes=$(cat "$FAKE_LANES") runs=$(cat "$SUP/runs.log")"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
