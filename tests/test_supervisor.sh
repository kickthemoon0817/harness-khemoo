#!/usr/bin/env bash
# Exercises the supervisor and its directive in a scratch harness home. A fake claude stands for the
# supervisor (it records the argv, environment and directory it was given, then answers with the
# JSON in $H/answer.json, or a directive from $H/directive-in.json, or crashes) and for every tick
# (it records its lane line and argv, and claims an issue on request). A fake queue counts five
# issues; a fake gh knows three issues and records every write; processes a slot file records stand
# for live ticks. Each launch waits for every runner and supervisor tick it started.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
: "${HARNESS_BIN:?tests/run.sh sets it}"
H=$(mktemp -d)
holders=()
cleanup() { for p in "${holders[@]}"; do kill "$p" 2>/dev/null; done; rm -rf "$H"; }
trap cleanup EXIT
mkdir -p "$H/bin" "$H/lib" "$H/libexec" "$H/prompts" "$H/instance/prompts" "$H/state/locks" "$H/state/logs" "$H/target" "$H/fakebin"
cp "$here/bin/tick.sh" "$H/bin/"; cp "$here/lib/instance.sh" "$H/lib/"
cp "$here/libexec/usage-ok.sh" "$here/libexec/tick-pid.sh" "$here/libexec/claim.sh" "$H/libexec/"
cp "$here/prompts/supervisor-core.md" "$here/prompts/public-record.md" "$H/prompts/"
echo "Core rules." > "$H/prompts/tick-core.md"
cp "$here/tests/fixtures/instance/prompts/tick.md" "$H/instance/prompts/tick.md"
# The supervisor gets no environment of the harness's, so its branch names its paths itself.
cat > "$H/fakebin/claude" <<EOF
#!/usr/bin/env bash
prompt=\$(cat)
case \$prompt in
    "You are the supervisor"*)
        printf '%s\n' "\$prompt" > "$H/sup-prompt.txt"
        printf '%s\n' "\$*" > "$H/sup-argv.txt"
        env > "$H/sup-env.txt"
        { pwd; ls -A; } > "$H/sup-cwd.txt"
        "$HARNESS_BIN" ticks --state "$H/state" > "$H/sup-ticks.txt"
        [ -e "$H/sup-crash" ] && exit 3
        echo "I read the snapshot."
        if [ -s "$H/answer.json" ]; then cat "$H/answer.json"
        elif [ -s "$H/directive-in.json" ]; then printf '{"directive": %s, "github": []}\n' "\$(cat "$H/directive-in.json")"
        else echo '{"directive": null, "github": []}'; fi
        exit 0 ;;
esac
printf '%s\n' "\${prompt%%\$'\n'*}" >> "\$FAKE_LANES"
printf '%s\n' "\$*" >> "\$FAKE_ARGS"
[ -e "\$FAKE_CLAIM" ] && rm -f "\$FAKE_CLAIM" && "\$HARNESS_HOME/libexec/claim.sh" acquire 7 >/dev/null
# One tick held open, for a stop to end.
[ -e "\$FAKE_LONG" ] && rm -f "\$FAKE_LONG" && sleep 30
exit 0
EOF
cat > "$H/fakebin/queue" <<'EOF'
#!/usr/bin/env bash
case "$ISSUE_LABEL" in ai) echo 5 ;; ai:ready) cat "$FAKE_READY" 2>/dev/null || echo 0 ;; *) echo 0 ;; esac
# Six open queue issues, one of them claimed, for the supervisor's queue event.
[ -n "${QUEUE_TOTAL_FILE:-}" ] && echo 6 > "$QUEUE_TOTAL_FILE"
exit 0
EOF
cat > "$H/fakebin/crontab" <<'EOF'
#!/usr/bin/env bash
echo "*/2 * * * * $HARNESS_HOME/bin/tick.sh"
EOF
# Issue 7 is open in the queue, 8 is closed and 9 is open without the queue's label; every write is
# recorded and succeeds, and anything else fails, as an outage would.
cat > "$H/fakebin/gh" <<'EOF'
#!/usr/bin/env bash
printf '%s ' "$@" | tr '\n' ' ' >> "$FAKE_GH_LOG"; echo >> "$FAKE_GH_LOG"
case " $* " in
    *" -X POST "*"/labels "*) [ -e "$FAKE_GH_LABEL_FAIL" ] && rm -f "$FAKE_GH_LABEL_FAIL" && exit 1; echo '{}' ;;
    *" -X POST "*|*" -X DELETE "*) echo '{}' ;;
    *" repos/example-org/example-repo/issues/7 ") echo '{"state": "open", "labels": [{"name": "ai"}]}' ;;
    *" repos/example-org/example-repo/issues/8 ") echo '{"state": "closed", "labels": [{"name": "ai"}]}' ;;
    *" repos/example-org/example-repo/issues/9 ") echo '{"state": "open", "labels": [{"name": "bug"}]}' ;;
    *) exit 1 ;;
esac
EOF
chmod +x "$H/fakebin/"*
git -C "$H/target" init -q
write_env() {  # $1 = SUPERVISOR's value; any further argument is one more line
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
FIRST_LABEL=ai:first
LOW_LABEL=ai:low
SUPERVISOR=$1
SUPERVISOR_MIN_INTERVAL_S=${MIN_INTERVAL:-3600}
SUPERVISOR_INTERVAL_S=7200
SUPERVISOR_STOP_GRACE_S=2
EOF
    [ $# -gt 1 ] && printf '%s\n' "${@:2}" >> "$H/instance/harness.env"
}
write_env 1
# shellcheck disable=SC2046 # one name a word
unset HARNESS_TICK FLEET_SESSION HARNESS_STATE GH_TOKEN $(compgen -v SUPERVISOR)
export HARNESS_HOME="$H" HARNESS_PATH="$H/fakebin:$PATH" PATH="$H/fakebin:$PATH" \
    FAKE_LANES="$H/lanes.txt" FAKE_ARGS="$H/args.txt" FAKE_READY="$H/ready.txt" FAKE_LONG="$H/long" \
    FAKE_CLAIM="$H/claim" FAKE_GH_LOG="$H/gh.txt" FAKE_GH_LABEL_FAIL="$H/label-fails"
S="$H/state"; L="$S/locks"; LOG="$S/logs/allocator.log"; DLOG="$S/logs/directive.log"; SUP="$S/supervisor"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
usage() {  # $1 = the 5-hour window's percent; $2, when given, its reset as Unix seconds
    local reset=""
    [ -n "${2:-}" ] && reset=", \"fiveHourResetsAt\": \"$(date -u -d "@$2" +%Y-%m-%dT%H:%M:%S.000000+00:00)\""
    printf '{"timestamp": %s000, "data": {"fiveHourPercent": %s, "weeklyPercent": 10%s}}\n' "$(date +%s)" "$1" "$reset" > "$H/usage-cache.json"
}
until_ok() { local n=0; until "$@"; do [ "$n" -lt 400 ] || return 1; sleep 0.05; n=$((n + 1)); done; }
# pgrep can miss a process mid-exec, so no runner means three empty looks in a row.
no_runner() { for _ in 1 2 3; do pgrep -f "$H/bin/tick.sh" >/dev/null && return 1; sleep 0.05; done; }
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
# Records process $1 as slot $2's tick of state directory $3, as the runner does.
record() {
    local stat; local -a f
    mkdir -p "$3/locks"
    stat=$(cat "/proc/$1/stat"); read -ra f <<<"${stat##*) }"
    printf '%s %s %s claude model-a work\n' "$1" "${f[19]}" "$(cat /proc/sys/kernel/random/boot_id)" > "$3/locks/slot$2.tick"
}
reset_supervisor() { rm -rf "$SUP" "$S/directive.json" "$DLOG" "$H/sup-prompt.txt" "$H/sup-crash" "$H/directive-in.json" "$H/answer.json"; }
usage 10

# --- with SUPERVISOR unset, a run leaves exactly what harness/agents's leaves --------------------
off=$(bash "$here/tests/fixtures/supervisor/off-run.sh" "$here" 2>&1)
check "SUPERVISOR unset: the state, the logs and the dashboard's keys a run leaves are harness/agents's" \
    '[ "$off" = "$(cat "$here/tests/fixtures/supervisor/off-state.txt")" ]' \
    "$(diff <(printf '%s\n' "$off") "$here/tests/fixtures/supervisor/off-state.txt")"

# --- the supervisor runs confined, and its answer is applied -------------------------------------
printf '{"version": 1, "reason": "one tick on model-b", "expires_at": "%s", "width": 1, "models": ["claude:model-b"], "focus": [12, 15], "avoid": [7]}\n' "$(at 3600)" > "$H/directive-in.json"
launch
check "the first firing starts a supervisor tick whose answer's directive is applied" \
    '[ -s "$S/directive.json" ] && grep -q "exit=0 .*reason=first" "$SUP/runs.log" && grep -q "apply: one tick on model-b" "$DLOG"' \
    "runs=$(cat "$SUP/runs.log" 2>/dev/null) dlog=$(cat "$DLOG" 2>/dev/null)"
check "the directive is owner-only" '[ "$(stat -c %a "$S/directive.json")" = 600 ]' "$(stat -c %a "$S/directive.json")"
check "the supervisor's prompt is its core rules, then the health snapshot" \
    'grep -q "^## Health snapshot" "$H/sup-prompt.txt" && tail -n 1 "$H/sup-prompt.txt" | jq -e ".version == 1 and .supervisor.enabled == true" >/dev/null' \
    "$(tail -c 300 "$H/sup-prompt.txt" 2>/dev/null)"
check "the supervisor runs with no tools: claude's tool, MCP and settings flags, no permission bypass" \
    'grep -q -- "-p --model model-b --tools  --restricted --strict-mcp-config" "$H/sup-argv.txt" && ! grep -q dangerously "$H/sup-argv.txt"' \
    "$(cat "$H/sup-argv.txt")"
check "its environment holds nothing of the harness's: no token, no config, no paths" \
    'grep -q "^HOME=" "$H/sup-env.txt" && ! grep -qE "^(GH_TOKEN|GITHUB_TOKEN|GH_REPO|TARGET_REPO|WORKTREE_ROOT|HARNESS_[A-Z_]*|FAKE_[A-Z_]*|SUPERVISOR[A-Z_]*)=" "$H/sup-env.txt" && ! grep -q ghp_FAKE "$H/sup-prompt.txt"' \
    "$(cut -d= -f1 "$H/sup-env.txt" | tr '\n' ' ')"
check "it runs in an empty directory of its own" \
    '[ "$(wc -l < "$H/sup-cwd.txt")" -eq 1 ] && [ "$(head -1 "$H/sup-cwd.txt")" != "$H/target" ] && [ ! -e "$(head -1 "$H/sup-cwd.txt")" ]' \
    "$(cat "$H/sup-cwd.txt")"
check "while it runs, it is a tick of its own kind in slot 0, and its record goes with it" \
    'grep -qE "^[0-9]+ [0-9]+ 0 claude model-b supervisor$" "$H/sup-ticks.txt" && [ ! -e "$L/supervisor.tick" ]' \
    "ticks=$(cat "$H/sup-ticks.txt" 2>/dev/null)"

touch "$FAKE_CLAIM"
launch
check "the minimum interval keeps a second supervisor tick from starting" '[ "$(wc -l < "$SUP/runs.log")" -eq 1 ]' "$(cat "$SUP/runs.log")"
check "width: one tick starts where the windows allow three" \
    '[ "$(ticks_run)" = 1 ] && grep -q "spawning 1$" "$LOG"' "lanes=$(cat "$FAKE_LANES") log=$(cat "$LOG")"
check "models: the tick runs on the directive's entry" 'grep -q -- "--model model-b" "$FAKE_ARGS" && ! grep -q model-a "$FAKE_ARGS"' "$(cat "$FAKE_ARGS")"
check "focus and avoid reach the lane line" \
    'grep -q "prefer #12, #15\." "$FAKE_LANES" && grep -q "do not claim #7\." "$FAKE_LANES"' "$(cat "$FAKE_LANES")"
check "with the supervisor on, a tick's claim is logged for the loop signal" 'grep -qE "^7 [0-9]+ " "$S/claims.log"' "$(cat "$S/claims.log" 2>/dev/null)"
check "the launcher keeps the open queue issues the count left, for the queue event" '[ "$(cat "$SUP/queue")" = 6 ] && [ ! -e "$SUP/queue.new" ]' "$(ls "$SUP")"

apply "{\"version\": 1, \"reason\": \"wide\", \"expires_at\": \"$(at 3600)\", \"width\": 3}"
usage 65
launch
check "width is clamped to the usage cap" '[ "$(ticks_run)" = 2 ] && grep -q "room=2 .*spawning 2$" "$LOG"' "lanes=$(cat "$FAKE_LANES") log=$(cat "$LOG")"
usage 72
launch
check "a cap of 1 on a 3-slot config holds a width of 3 to one tick" '[ "$(ticks_run)" = 1 ] && grep -q "room=1 .*spawning 1$" "$LOG"' "lanes=$(cat "$FAKE_LANES") log=$(cat "$LOG")"
usage 10

# The answer's GitHub actions: two the rails allow, three they refuse.
cat > "$H/answer.json" <<'EOF'
{"directive": null, "github": [
  {"comment": {"issue": 7, "body": "Ticks loop on this issue; the next one should read the review first."}},
  {"label": {"issue": 7, "add": ["ai:first"]}},
  {"label": {"issue": 7, "add": ["ai:ready"]}},
  {"comment": {"issue": 8, "body": "This one is closed."}},
  {"park": {"issue": 9, "question": "Is this in scope?"}}
]}
EOF
: > "$FAKE_GH_LOG"
"$H/bin/tick.sh" supervisor manual
check "a comment on a queued issue is posted, marked as the supervisor's" \
    'grep -q -- "-X POST -f body=\*\*From the harness.s supervisor\*\* .*repos/example-org/example-repo/issues/7/comments" "$FAKE_GH_LOG" && grep -q "github: commented on #7" "$DLOG"' \
    "$(cat "$FAKE_GH_LOG")"
check "a label the rails name is added" \
    'grep -q -- "-X POST -f labels\[\]=ai:first repos/example-org/example-repo/issues/7/labels" "$FAKE_GH_LOG" && grep -q "github: labelled #7 +ai:first" "$DLOG"' "$(cat "$FAKE_GH_LOG")"
check "a label the rails do not name, a closed issue and one outside the queue are refused, and nothing is written for them" \
    '! grep -q "ai:ready" "$FAKE_GH_LOG" && ! grep -qE "issues/(8|9)/" "$FAKE_GH_LOG" && grep -q "refused label on #7: the label ai:ready" "$DLOG" && grep -q "refused comment on #8: it is not open" "$DLOG" && grep -q "refused park on #9: it does not carry ai" "$DLOG"' \
    "gh=$(cat "$FAKE_GH_LOG") dlog=$(grep github: "$DLOG")"
check "a null directive leaves the directive in force" 'grep -q "answer: no directive" "$DLOG" && jq -e ".width == 3" "$S/directive.json" >/dev/null' "$(cat "$S/directive.json")"
jq '.github += [.github[0], .github[0]]' "$H/answer.json" > "$H/answer.new" && mv "$H/answer.new" "$H/answer.json"
: > "$FAKE_GH_LOG"
"$H/bin/tick.sh" supervisor manual
check "more than five actions are all refused" \
    '! grep -q -- "-X POST" "$FAKE_GH_LOG" && grep -q "github: refused all 7 actions" "$DLOG"' "$(cat "$FAKE_GH_LOG")"
echo 'I would rather not answer.' > "$H/answer.json"
"$H/bin/tick.sh" supervisor manual
check "an output with no answer applies nothing and counts as a failed run" \
    'grep -q "answer: none found" "$DLOG" && tail -n 1 "$SUP/runs.log" | grep -q "exit=1 "' "$(tail -n 2 "$SUP/runs.log")"
echo '{"directive": null, "github": [], "shell": "rm -rf /"}' > "$H/answer.json"
"$H/bin/tick.sh" supervisor manual
check "an answer with an unknown key is refused as such, nothing applied" \
    'grep -q "answer refused: unknown field \`shell\`" "$DLOG" && tail -n 1 "$SUP/runs.log" | grep -q "exit=1 "' "$(tail -n 2 "$DLOG")"
printf '{"directive": {"version": 1, "reason": "r", "expires_at": "%s", "models": ["claude:model-z"]}, "github": [{"comment": {"issue": 7, "body": "it goes with the directive"}}]}\n' "$(at 3600)" > "$H/answer.json"
: > "$FAKE_GH_LOG"
"$H/bin/tick.sh" supervisor manual
check "a refused directive refuses its answer's GitHub actions too" \
    '! grep -q -- "-X POST" "$FAKE_GH_LOG" && [ "$(grep -c "answer refused: its directive was refused; no GitHub action taken" "$DLOG")" -eq 1 ] && jq -e ".width == 3" "$S/directive.json" >/dev/null' \
    "gh=$(cat "$FAKE_GH_LOG") dlog=$(tail -n 3 "$DLOG")"
# Three parks: the first two go, the question before the label, and the third is past the day's cap.
# The first one's label fails: its question was asked, so it counts all the same.
cat > "$H/answer.json" <<'EOF'
{"directive": null, "github": [
  {"park": {"issue": 7, "question": "Should this wait for the new API?"}},
  {"park": {"issue": 7, "question": "Is the old behaviour still needed?"}},
  {"park": {"issue": 7, "question": "A third question."}}
]}
EOF
: > "$FAKE_GH_LOG"; touch "$FAKE_GH_LABEL_FAIL"
"$H/bin/tick.sh" supervisor manual
check "a park posts its question before it adds the operator's label" \
    '[ "$(grep -n -- "-X POST" "$FAKE_GH_LOG" | head -2 | cut -d: -f1 | tr "\n" " ")" = "$(grep -n -- "issues/7/comments" "$FAKE_GH_LOG" | head -1 | cut -d: -f1) $(grep -n -- "labels\[\]=ai:operator" "$FAKE_GH_LOG" | head -1 | cut -d: -f1) " ]' \
    "$(cat "$FAKE_GH_LOG")"
check "at most two parks a day (SUPERVISOR_MAX_PARKS_PER_DAY)" \
    '[ "$(grep -c "issues/7/comments" "$FAKE_GH_LOG")" -eq 2 ] && grep -q "refused park on #7: 2 issues parked in 24 h already" "$DLOG"' "$(grep github: "$DLOG" | tail -n 3)"
check "a park whose label failed is logged as such, and still counts toward the cap" \
    'grep -q "github: parked #7 for the operator, but its label failed" "$DLOG" && grep -q "github: parked #7 for the operator$" "$DLOG"' "$(grep github: "$DLOG" | tail -n 3)"
echo '{"directive": null, "github": [{"comment": {"issue": 7, "body": "ping @owner-a, see https://example.com"}}]}' > "$H/answer.json"
: > "$FAKE_GH_LOG"
"$H/bin/tick.sh" supervisor manual
check "a body with a mention or a link is refused" \
    '! grep -q -- "-X POST" "$FAKE_GH_LOG" && grep -q "refused comment on #7: body: holds an @-mention" "$DLOG"' "$(tail -n 1 "$DLOG")"
rm -f "$H/answer.json" "$SUP/failures"

flock -o "$L/supervisor.lock" sleep 30 >/dev/null 2>&1 & holders+=("$!")
until_ok bash -c "! flock -n '$L/supervisor.lock' true"
runs=$(wc -l < "$SUP/runs.log"); rm -f "$H/sup-prompt.txt"
"$H/bin/tick.sh" supervisor manual
check "a second supervisor is kept out by the lock" '[ "$(wc -l < "$SUP/runs.log")" = "$runs" ] && [ ! -e "$H/sup-prompt.txt" ]' "runs=$runs $(cat "$SUP/runs.log")"
kill "${holders[-1]}" 2>/dev/null; wait "${holders[-1]}" 2>/dev/null

# --- agents that cannot run confined are refused -------------------------------------------------
write_env 1 "SUPERVISOR_MODEL=gemini:pro"
out=$("$H/bin/tick.sh" check 2>&1); rc=$?
check "gemini is refused as the supervisor, saying why" '[ "$rc" -eq 1 ] && [[ "$out" == *"gemini cannot supervise"* ]]' "rc=$rc $(grep -i supervis <<<"$out")"
# The flags are held to an allowlist: each of these would give the supervisor a tool, a setting,
# an MCP server or a looser sandbox, and is refused, so the supervisor does not run.
confine="--tools= --restricted --strict-mcp-config"
codex_d="--sandbox read-only --disable shell_tool --disable unified_exec --disable apps --disable plugins --disable browser_use --disable computer_use --disable in_app_browser --disable image_generation"
while IFS='|' read -r agent flags why; do
    write_env 1 "SUPERVISOR_MODEL=$agent:model-s" "SUPERVISOR_FLAGS_$agent=\"$flags\""
    out=$("$H/bin/tick.sh" check 2>&1); rc=$?
    check "SUPERVISOR_FLAGS_$agent=$flags is refused" '[ "$rc" -eq 1 ] && [[ "$out" == *"SUPERVISOR_FLAGS_$agent: $why"* ]]' "rc=$rc want=$why $(grep -i supervis <<<"$out")"
done <<EOF
claude|--tools= --restricted --dangerously-skip-permissions|--dangerously-skip-permissions is not one of the supervisor's flags
claude|--tools Bash,Edit,Write --restricted --strict-mcp-config|--tools must be given empty
claude|$confine --tools=|--tools is given twice
claude|$confine --mcp-config servers.json|--mcp-config is not one of the supervisor's flags
claude|$confine --settings extra.json|--settings is not one of the supervisor's flags
claude|$confine --permission-mode bypassPermissions|--permission-mode is not one of the supervisor's flags
claude|--tools= --restricted|--strict-mcp-config is missing
codex|$codex_d --enable shell_tool|--enable is not one of the supervisor's flags
codex|$codex_d --full-auto|--full-auto is not one of the supervisor's flags
codex|$codex_d -c sandbox_mode=danger-full-access|-c sandbox_mode=danger-full-access is not the supervisor's
codex|$codex_d --dangerously-bypass-approvals-and-sandbox|--dangerously-bypass-approvals-and-sandbox is not one of the supervisor's flags
codex|--sandbox read-only --disable shell_tool|--disable unified_exec is missing
codex|$codex_d|-c web_search=disabled is missing
EOF
write_env 1 "SUPERVISOR_FLAGS_claude=\"--tools Bash --restricted --strict-mcp-config\""
rm -f "$H/sup-argv.txt"; : > "$LOG"
"$H/bin/tick.sh" supervisor manual
check "with refused flags the supervisor does not run, and the launcher says why" \
    '[ ! -e "$H/sup-argv.txt" ] && grep -q "supervisor not run: SUPERVISOR_FLAGS_claude: --tools must be given empty" "$LOG"' "$(cat "$LOG")"
write_env 1 'SUPERVISOR_FLAGS_claude="--tools= --restricted --strict-mcp-config"'
rm -f "$H/sup-argv.txt"
"$H/bin/tick.sh" supervisor manual
check "a flag override keeps the supervisor tool-free, --tools= passing an empty --tools" \
    'grep -qx -- "-p --model model-b --tools  --restricted --strict-mcp-config" "$H/sup-argv.txt"' "$(cat "$H/sup-argv.txt" 2>/dev/null)"
write_env 1

# --- the environment cannot loosen a rail --------------------------------------------------------
sleep 60 & loose=$!; holders+=("$loose")
record "$loose" 8 "$S"
SUPERVISOR_STOP_GRACE_S=0 SUPERVISOR_MAX_DIRECTIVE_S=999999 apply "{\"version\": 1, \"reason\": \"r\", \"expires_at\": \"$(at 90000)\", \"stop\": [{\"pid\": $loose, \"reason\": \"x\"}]}"
check "a rail set in the environment is not read: the grace and the longest expiry are harness.env's" \
    '[ "$rc" -eq 0 ] && jq -e "((.stop[0].due_at | fromdate) - (.applied_at | fromdate)) == 2 and ((.expires_at | fromdate) - (.applied_at | fromdate)) == 21600" "$S/directive.json" >/dev/null' \
    "rc=$rc $out $(cat "$S/directive.json")"
kill "$loose"; rm -f "$L/slot8.tick"

# --- pause, wake_at and the human's pause --------------------------------------------------------
# A full pool starts one landing tick past the width for an issue waiting for the merge slot; a
# width of 0 starts nothing new, that landing tick included.
echo 1 > "$FAKE_READY"
flock -o "$L/slot1.lock" sleep 60 >/dev/null 2>&1 & holders+=("$!")
until_ok bash -c "! flock -n '$L/slot1.lock' true"
apply "{\"version\": 1, \"reason\": \"one at a time\", \"expires_at\": \"$(at 3600)\", \"width\": 1}"
launch
check "width 1 with its slot held: the landing tick still starts" \
    'grep -q "landing tick: claim only" "$FAKE_LANES" && [ "$(ticks_run)" = 1 ]' "lanes=$(cat "$FAKE_LANES") log=$(cat "$LOG")"
apply "{\"version\": 1, \"reason\": \"nothing new\", \"expires_at\": \"$(at 3600)\", \"width\": 0}"
launch
check "width 0: nothing starts, not even a landing tick" '[ "$(ticks_run)" = 0 ]' "lanes=$(cat "$FAKE_LANES") log=$(cat "$LOG")"
kill "${holders[-1]}" 2>/dev/null; wait "${holders[-1]}" 2>/dev/null
until_ok flock -n "$L/slot1.lock" true
launch
check "width 0 with every slot free: nothing starts" '[ "$(ticks_run)" = 0 ]' "lanes=$(cat "$FAKE_LANES") log=$(cat "$LOG")"
echo 0 > "$FAKE_READY"

apply "{\"version\": 1, \"reason\": \"hold off\", \"expires_at\": \"$(at 3600)\", \"pause\": {\"until\": \"$(at 1800)\", \"reason\": \"the base branch is red\"}}"
launch
check "pause: no tick starts" '[ "$(ticks_run)" = 0 ] && grep -q "directive: quiet (pause) until .*: the base branch is red" "$LOG"' "lanes=$(cat "$FAKE_LANES") log=$(cat "$LOG")"
sleep 1.1
date +%s > "$SUP/next_reset"
launch
check "a window's reset does not end a pause" '[ "$(ticks_run)" = 0 ] && grep -q "directive: quiet (pause)" "$LOG"' "lanes=$(cat "$FAKE_LANES") log=$(cat "$LOG")"
rm -f "$SUP/next_reset"

apply "{\"version\": 1, \"reason\": \"windows are full\", \"expires_at\": \"$(at 3600)\", \"wake_at\": \"$(at 1800)\"}"
launch
check "wake_at: the launcher stays quiet" '[ "$(ticks_run)" = 0 ] && grep -q "directive: quiet (wake)" "$LOG"' "lanes=$(cat "$FAKE_LANES") log=$(cat "$LOG")"
sleep 1.1
date +%s > "$SUP/next_reset"
launch
check "a window's reset ends the quiet before wake_at" \
    '[ "$(ticks_run)" = 3 ] && grep -q "quiet ended early on reset" "$LOG"' "lanes=$(cat "$FAKE_LANES") log=$(cat "$LOG")"
rm -f "$SUP/next_reset"
launch
check "and the quiet stays ended for that directive once the reset's sign is gone" '[ "$(ticks_run)" = 3 ]' "lanes=$(cat "$FAKE_LANES") log=$(cat "$LOG")"

apply "{\"version\": 1, \"reason\": \"windows are full\", \"expires_at\": \"$(at 3600)\", \"wake_at\": \"$(at 1800)\"}"
sleep 1.1
touch "$S/paused"
launch
check "the human's pause holds whatever the directive says" '[ "$(ticks_run)" = 0 ] && [ -e "$SUP/paused" ]' "lanes=$(cat "$FAKE_LANES")"
rm -f "$S/paused"
launch
check "lifting the human's pause is a wake event that ends the quiet" \
    '[ "$(ticks_run)" = 3 ] && grep -q "quiet ended early on unpaused" "$LOG"' "lanes=$(cat "$FAKE_LANES") log=$(cat "$LOG")"

apply "{\"version\": 1, \"reason\": \"hold off\", \"expires_at\": \"$(at 3600)\", \"pause\": {\"until\": \"$(at 1800)\", \"reason\": \"x\"}}"
sleep 1.1
date +%s > "$SUP/operator"
launch
check "an operator's comment ends a pause" '[ "$(ticks_run)" = 3 ] && grep -q "quiet ended early on operator" "$LOG"' "lanes=$(cat "$FAKE_LANES") log=$(cat "$LOG")"
rm -f "$SUP/operator"

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

jq -n '{version: 1, reason: "by hand", applied_at: "2031-01-01T00:00:00Z", expires_at: "2031-01-01T05:00:00Z", pause: {until: "2031-01-01T04:00:00Z", reason: "x"}}' > "$S/directive.json"
launch
check "a directive written by hand for years ahead is ignored" '[ "$(ticks_run)" = 3 ] && grep -q "applied_at: in the future" "$DLOG"' "lanes=$(cat "$FAKE_LANES") dlog=$(tail -n 2 "$DLOG")"

apply "{\"version\": 1, \"reason\": \"r\", \"expires_at\": \"$(at 3600)\", \"width\": 1, \"speed\": 9}"
check "a directive with an unknown field is refused and logged" \
    '[ "$rc" -eq 1 ] && [[ "$out" == *"unknown field"* ]] && tail -n 1 "$DLOG" | grep -q "refused: .*unknown field"' "rc=$rc $out"
apply "{\"version\": 1, \"reason\": \"r\", \"expires_at\": \"$(at 3600)\", \"models\": [\"claude:model-z\"]}"
check "a model outside the configured list is refused" '[ "$rc" -eq 1 ] && [[ "$out" == *"not in the configured model list"* ]]' "rc=$rc $out"
"$HARNESS_BIN" directive clear >/dev/null

# --- with SUPERVISOR unset, nothing of it applies ------------------------------------------------
apply "{\"version\": 1, \"reason\": \"hold off\", \"expires_at\": \"$(at 3600)\", \"pause\": {\"until\": \"$(at 1800)\", \"reason\": \"x\"}}"
write_env ""
runs_before=$(wc -l < "$SUP/runs.log")
rm -f "$SUP/last_start" "$S/claims.log" "$L/claim.7"
touch "$FAKE_CLAIM"
launch
check "SUPERVISOR unset: the directive is not read and no supervisor tick starts" \
    '[ "$(ticks_run)" = 3 ] && [ "$(wc -l < "$SUP/runs.log")" = "$runs_before" ] && ! grep -q "supervisor\\|directive" "$LOG"' "lanes=$(cat "$FAKE_LANES") runs_before=$runs_before log=$(cat "$LOG")"
check "SUPERVISOR unset: a claim is not logged" '[ ! -e "$S/claims.log" ] && [ -e "$L/claim.7" ]' "$(cat "$S/claims.log" 2>/dev/null)"
rm -f "$L/claim.7" "$FAKE_CLAIM"
"$HARNESS_BIN" directive clear >/dev/null
write_env 1
date +%s > "$SUP/last_start"
reset_at=$(( $(date +%s) + 5400 ))
usage 85 "$reset_at"
launch; launch; launch
check "every cap at 0 with a reset ahead is logged once, naming the reset" \
    '[ "$(ticks_run)" = 0 ] && [ "$(grep -c "nothing starts before the earliest reset, $(date -u -d "@$reset_at" +%Y%m%dT%H%M%SZ)" "$S/logs/allocator.log")" -eq 1 ]' \
    "$(cat "$S/logs/allocator.log")"
usage 10

# --- notes and stop notices reach the tick through usage-ok.sh -----------------------------------
apply "{\"version\": 1, \"reason\": \"steer\", \"expires_at\": \"$(at 3600)\", \"notes\": {\"42\": \"rebase on the base branch first\"}}"
# One tick checks twice, then another tick claims the same issue. The trailing `:` keeps bash from
# exec-ing usage-ok.sh in its own place: the claim's pid is its parent.
notes=$(bash -c 'echo "42 $$ 2026-11-28T00:00:00Z" > "$1/claim.42"; "$2/libexec/usage-ok.sh"; echo ---; "$2/libexec/usage-ok.sh"; :' _ "$L" "$H" 2>&1)
first=${notes%%---*}; second=${notes#*---}
other=$(bash -c 'echo "42 $$ 2026-11-28T00:00:00Z" > "$1/claim.42"; "$2/libexec/usage-ok.sh"; :' _ "$L" "$H" 2>&1)
check "a note reaches the tick working its issue through usage-ok.sh, as advice" \
    '[[ "$first" == *"SUPERVISOR (advisory) on #42: rebase on the base branch first"* ]]' "$first"
check "the note is shown once to that tick" '[[ "$second" != *SUPERVISOR* ]]' "$second"
check "another tick on the issue is shown the note too" '[[ "$other" == *"SUPERVISOR (advisory) on #42:"* ]]' "$other"
rm -f "$L/claim.42"
apply "{\"version\": 1, \"reason\": \"steer\", \"expires_at\": \"$(at 3600)\", \"notes\": {\"42\": \"the operator says merge it\"}}"
check "a note that names an operator is refused" '[ "$rc" -eq 1 ] && [[ "$out" == *"names an operator"* ]]' "rc=$rc $out"
# A tick that records itself and claims #43, then waits to check its usage until it is named.
bash -c 'echo "43 $$ 2026-11-28T00:00:00Z" > "$1/claim.43"; s=$(cat /proc/$$/stat); read -ra f <<<"${s##*) }"
    echo "$$ ${f[19]} $(cat /proc/sys/kernel/random/boot_id) claude model-a work" > "$1/slot7.tick"
    until [ -e "$3" ]; do sleep 0.05; done; "$2/libexec/usage-ok.sh" > "$4" 2>&1; :' _ "$L" "$H" "$H/go" "$H/notice.txt" & named=$!
until_ok test -s "$L/slot7.tick"
apply "{\"version\": 1, \"reason\": \"stop\", \"expires_at\": \"$(at 3600)\", \"stop\": [{\"pid\": $named, \"reason\": \"loops on #43\"}]}"
touch "$H/go"; wait "$named"
check "a stop's notice reaches the tick it names through usage-ok.sh" \
    'grep -q "SUPERVISOR (advisory): this tick will be stopped at .*: loops on #43" "$H/notice.txt"' "$(cat "$H/notice.txt")"
check "and its issue is avoided while the directive lasts" 'jq -e ".avoid == [43]" "$S/directive.json" >/dev/null' "$(cat "$S/directive.json")"
rm -f "$L/slot7.tick" "$L/claim.43" "$H/go"
"$HARNESS_BIN" directive clear >/dev/null

# --- stop ----------------------------------------------------------------------------------------
# A process no slot file records; another installation's tick; and a tick in the test shell's own
# process group, which a stop refuses rather than signal.
sleep 60 & plain=$!; holders+=("$plain")
sleep 60 & foreign=$!; holders+=("$foreign")
record "$foreign" 9 "$H/other"
sleep 60 & shared=$!; holders+=("$shared")
record "$shared" 8 "$S"
apply "{\"version\": 1, \"reason\": \"stop\", \"expires_at\": \"$(at 3600)\", \"stop\": [{\"pid\": $plain, \"reason\": \"x\"}]}"
check "a pid that is no tick is refused" '[ "$rc" -eq 1 ] && [[ "$out" == *"not a live tick of this installation"* ]]' "rc=$rc $out"
apply "{\"version\": 1, \"reason\": \"stop\", \"expires_at\": \"$(at 3600)\", \"stop\": [{\"pid\": $foreign, \"reason\": \"x\"}]}"
check "another installation's tick is refused" '[ "$rc" -eq 1 ] && [[ "$out" == *"not a live tick of this installation"* ]]' "rc=$rc $out"
# A runner's own tick, held open. A stopped tick of a few seconds chains only under a
# CHAIN_MIN_TICK_S below that.
write_env 1 CHAIN_MIN_TICK_S=1
apply "{\"version\": 1, \"reason\": \"one\", \"expires_at\": \"$(at 3600)\", \"width\": 1}"
: > "$FAKE_LANES"; : > "$LOG"; touch "$FAKE_LONG"
"$H/bin/tick.sh"
until_ok test -s "$L/slot1.tick"
tick=$(cut -d' ' -f1 "$L/slot1.tick")
apply "{\"version\": 1, \"reason\": \"stop\", \"expires_at\": \"$(at 3600)\", \"width\": 1, \"stop\": [{\"pid\": $tick, \"reason\": \"loops on #7\"}, {\"pid\": $shared, \"reason\": \"shares a group\"}]}"
check "live ticks of this installation may be named" '[ "$rc" -eq 0 ]' "rc=$rc $out"
"$H/bin/tick.sh"
check "the tick is not stopped before its grace" 'kill -0 "$tick" 2>/dev/null' "gone early"
sleep 2.2
"$H/bin/tick.sh"
until_ok no_runner
check "after its grace the tick's own process group gets TERM, and the stop is logged" \
    '! kill -0 "$tick" 2>/dev/null && grep -q "stop: the supervisor stopped pid $tick, TERM to its own process group $tick after its grace: loops on #7" "$DLOG"' \
    "$(grep "stop:" "$DLOG")"
check "the runner survives the stop: it writes the tick's trailer" \
    'grep -q "^\[tick.sh\] exit=143 " "$S"/logs/tick-*-slot1.log' "$(tail -n 2 "$S"/logs/tick-*-slot1.log)"
check "and it chains the next tick" \
    'grep -q "slot1 chains tick 2" "$LOG" && [ "$(ticks_run)" = 2 ]' "lanes=$(cat "$FAKE_LANES") log=$(cat "$LOG")"
check "a tick outside a process group of its own is refused, not signalled" \
    'kill -0 "$shared" && grep -q "stop: pid $shared skipped: its process group ([0-9]*) is not its own" "$DLOG"' "$(grep "stop:" "$DLOG")"
check "nothing else was stopped" 'kill -0 "$plain" && kill -0 "$foreign"' "plain or foreign gone"
launch
check "a stop is sent once" '[ "$(grep -c "stop: .*pid $tick" "$DLOG")" -eq 1 ]' "$(grep "stop:" "$DLOG")"
kill "$shared"; rm -f "$L/slot8.tick"
write_env 1
"$HARNESS_BIN" directive clear >/dev/null

# --- a supervisor that crashes leaves the defaults in force --------------------------------------
reset_supervisor
touch "$H/sup-crash"
printf '{"version": 1, "reason": "never applied", "expires_at": "%s", "width": 1}\n' "$(at 3600)" > "$H/directive-in.json"
# A fresh event before each look, so only the backoff can explain a supervisor tick that does
# not start. Times count in whole seconds, so the minimum interval (3 s), the backoffs (6 s, then
# 12 s) and the sleeps between them keep a second of room either way.
MIN_INTERVAL=3 write_env 1
due() { USAGE_CACHE="$H/usage-cache.json" USAGE_FETCH="$H/no-fetch" "$HARNESS_BIN" supervisor due 2>&1; }
event() { mkdir -p "$S/model-cooldown"; touch "$S/model-cooldown/claude:elsewhere-$1"; }
spawned() { sed -n 's/.*-> spawning \([0-9]*\)$/\1/p' "$LOG" | tail -n 1; }
usage 10
launch
# The supervisor counts toward its agent's cap while it runs, so the pool is the room the launcher
# found, two or three ticks.
check "a crashed supervisor's run is recorded as a failure, and the defaults hold" \
    'grep -q "exit=3 " "$SUP/runs.log" && [ "$(cat "$SUP/failures")" = 1 ] && [ ! -e "$S/directive.json" ] && [ "$(ticks_run)" = "$(spawned)" ] && [ "$(spawned)" -ge 2 ]' \
    "runs=$(cat "$SUP/runs.log" 2>/dev/null) lanes=$(cat "$FAKE_LANES") log=$(cat "$LOG")"
sleep 3.5; event 1
check "one failure backs off twice the minimum interval" '[[ "$(due)" == "not due: backing off 6s after 1 failed runs" ]]' "$(due)"
launch
check "no supervisor tick starts within that backoff" '[ "$(wc -l < "$SUP/runs.log")" -eq 1 ]' "$(cat "$SUP/runs.log")"
sleep 4; event 2
launch
check "past the backoff it runs again, and fails again" '[ "$(wc -l < "$SUP/runs.log")" -eq 2 ] && [ "$(cat "$SUP/failures")" = 2 ]' "$(cat "$SUP/runs.log")"
sleep 3.5; event 3
check "two failures in a row double it" '[[ "$(due)" == "not due: backing off 12s after 2 failed runs" ]]' "$(due)"
rm -f "$H/sup-crash"
sleep 10; event 4
launch
check "a run that ends well resets the failures" '[ "$(cat "$SUP/failures")" = 0 ] && tail -n 1 "$SUP/runs.log" | grep -q "exit=0 "' "$(cat "$SUP/runs.log")"
sleep 3.5; event 5
check "and with them the backoff: the next event is due after the minimum interval alone" '[[ "$(due)" == "due "* ]]' "$(due)"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
