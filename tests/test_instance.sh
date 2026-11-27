#!/usr/bin/env bash
# Exercises bin/tick.sh against an instance overlay in a scratch harness home: the instance's
# required files, its hooks, the composed prompt, the queue count, dispatch through fleet and
# `tick.sh check`. A fake claude records its prompt and environment; a fake fleet follows the CLI
# contract in docs/FLEET.md; a fake queue command answers per label.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
H=$(mktemp -d)
trap 'rm -rf "$H"' EXIT
I="$H/instance"
mkdir -p "$H/bin" "$H/prompts" "$I/prompts" "$I/hooks" "$H/state/locks" "$H/state/logs" "$H/target" "$H/fakebin"
cp "$here/bin/tick.sh" "$here/bin/usage-lib.sh" "$here/bin/instance-lib.sh" "$H/bin/"
cp "$here/tests/fixtures/instance/prompts/tick.md" "$I/prompts/tick.md"
cat > "$H/prompts/tick-core.md" <<'EOF'
Core rules for {{GH_REPO}}: budget {{TICK_BUDGET_MIN}} min, lane `{{LANE_LABEL}}`, {{UNKNOWN}} stays.
EOF
cat > "$H/fakebin/claude" <<'EOF'
#!/usr/bin/env bash
printf '%s' "$2" > "$FAKE_PROMPT"
printf '%s\n' "${FROM_HOOK:-unset} ${FROM_EXPORTS:-unset}" > "$FAKE_ENV"
printf '%s\n' "rules=${PUBLIC_RECORD_RULES:-unset} pr=${PUBLIC_RECORD_PR_SECTIONS:-unset}" \
    "issue=${PUBLIC_RECORD_ISSUE_SECTIONS:-unset} repo=${GH_REPO:-unset} wip=${WIP_LABEL:-unset}" \
    "low=${LOW_LABEL:-unset} token=${GH_TOKEN:-unset}" > "$FAKE_LINT_ENV"
printf '%s\n' "$*" >> "$FAKE_ARGS"
EOF
cat > "$H/fakebin/queue" <<'EOF'
#!/usr/bin/env bash
[ -e "$FAKE_QUEUE_FAILS" ] && { echo "queue service unreachable" >&2; exit 1; }
printf '%s\n' "argv=$(tr '\0' ' ' < /proc/$$/cmdline)" "token=${GH_TOKEN:-unset}" > "$FAKE_QUEUE_SEEN"
case "$ISSUE_LABEL" in ai:lane) cat "$FAKE_LANE" 2>/dev/null || echo 0 ;; ai:ready) echo 0 ;; *) echo 1 ;; esac
EOF
# docs/FLEET.md's CLI: status exits 1 when any device failed, its document complete; dispatch reads
# the prompt from stdin, refuses an empty one, and with --wait writes the output and exits with the
# agent's status.
cat > "$H/fakebin/fleet" <<'EOF'
#!/usr/bin/env bash
cmd=$1; shift
case "$cmd" in
status)
    [ "$*" = --json ] || exit 2
    [ -e "$FAKE_FLEET_DOWN" ] && { echo "fleet: fleet.toml: no such file" >&2; exit 2; }
    cat "$FAKE_FLEET_STATUS"; exit 1 ;;
dispatch)
    printf '%s\n' "$cmd $*" >> "$FAKE_FLEET_ARGV"
    model=""
    while [ $# -gt 0 ]; do
        case "$1" in
            --model) model=$2; shift 2 ;;
            --device|--adapter|--workdir|--timeout|--label) [ $# -ge 2 ] || exit 2; shift 2 ;;
            --wait|--json) shift ;;
            *) echo "fleet: unknown argument $1" >&2; exit 2 ;;
        esac
    done
    prompt=$(cat)
    [ -n "$prompt" ] || { echo "fleet: empty prompt" >&2; exit 125; }
    printf '%s' "$prompt" > "$FAKE_FLEET_STDIN"
    [ "$model" = "$(cat "$FAKE_FLEET_LIMIT" 2>/dev/null)" ] && { echo "You've hit your usage limit · resets 5pm"; exit 1; }
    echo "agent output on $model"
    exit "$(cat "$FAKE_FLEET_RC" 2>/dev/null || echo 0)" ;;
*) exit 2 ;;
esac
EOF
# ssh -G prints the host an alias resolves to, and an unknown name as itself.
cat > "$H/fakebin/ssh" <<'EOF'
#!/usr/bin/env bash
[ "$1" = -G ] || exit 255
case "$2" in github-work) echo "hostname github.com" ;; *) echo "hostname $2" ;; esac
EOF
cat > "$H/fakebin/crontab" <<'EOF'
#!/usr/bin/env bash
[ -e "$FAKE_NO_CRON" ] || echo "*/2 * * * * $HARNESS_HOME/bin/tick.sh"
EOF
chmod +x "$H/fakebin/"*
git -C "$H/target" init -q
# The fixture's values, with a repository name that a naive substitution would mangle.
write_env() {
    { grep -v '^GH_REPO=\|^TARGET_REPO=\|^WORKTREE_ROOT=' "$here/tests/fixtures/instance/harness.env"
      cat <<EOF
TARGET_REPO=$H/target
WORKTREE_ROOT=$H/worktrees
GH_REPO='o/r&x|y\\z'
QUEUE_DEPTH_CMD=$H/fakebin/queue
CLAUDE_BIN=$H/fakebin/claude
CLAUDE_FLAGS=
MAX_SLOTS=1
USAGE_CACHE=$H/usage-cache.json
USAGE_FETCH=$H/no-fetch
CLAIM_GRACE_S=0
FROM_EXPORTS=exported
TICK_EXPORTS="FROM_EXPORTS"
EOF
      printf '%s\n' "$@"; } > "$I/harness.env"
}
export HARNESS_HOME="$H" HARNESS_PATH="$H/fakebin:$PATH" FAKE_PROMPT="$H/prompt.txt" FAKE_ENV="$H/env.txt" \
    FAKE_ARGS="$H/args.txt" FAKE_LANE="$H/lane.txt" FAKE_LINT_ENV="$H/lint-env.txt" \
    PATH="$H/fakebin:$PATH" FAKE_QUEUE_FAILS="$H/queue-fails" FAKE_QUEUE_SEEN="$H/queue-seen.txt" FAKE_NO_CRON="$H/no-cron" \
    FAKE_FLEET_STATUS="$here/tests/fixtures/fleet/status.json" FAKE_FLEET_DOWN="$H/fleet-down" FAKE_FLEET_ARGV="$H/fleet-argv.txt" \
    FAKE_FLEET_STDIN="$H/fleet-stdin.txt" FAKE_FLEET_LIMIT="$H/fleet-limit" FAKE_FLEET_RC="$H/fleet-rc"
LOG="$H/state/logs/allocator.log"
printf '{"timestamp": %s000, "data": {"fiveHourPercent": 10, "weeklyPercent": 10}}\n' "$(date +%s)" > "$H/usage-cache.json"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
launch() {  # run the launcher once and wait for the runner it may start
    rm -f "$FAKE_PROMPT" "$FAKE_ENV" "$FAKE_ARGS" "$FAKE_FLEET_ARGV" "$FAKE_FLEET_STDIN" "$H/state/logs/"tick-*.log; : > "$LOG"
    rm -f "$H/state/locks/heartbeat.stamp"
    "$H/bin/tick.sh"
    sleep 2
}

# A missing instance prompt starts nothing and says so, once.
write_env; mv "$I/prompts/tick.md" "$H/tick.md.away"
launch
check "a missing tick.md starts nothing" '[ ! -e "$FAKE_PROMPT" ]' "$(cat "$FAKE_PROMPT" 2>/dev/null)"
check "a missing tick.md is logged" 'grep -q "instance incomplete: $I/prompts/tick.md" "$LOG"' "$(cat "$LOG")"
"$H/bin/tick.sh"; sleep 1
check "the incomplete instance is logged at most hourly" '[ "$(grep -c "instance incomplete" "$LOG")" -eq 1 ]' "$(cat "$LOG")"
"$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "check exits 1 on an incomplete instance and names what is missing" \
    '[ "$rc" -eq 1 ] && grep -q "missing: $I/prompts/tick.md" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
mv "$H/tick.md.away" "$I/prompts/tick.md"
mv "$I/harness.env" "$H/env.away"
"$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "check exits 1 without harness.env" '[ "$rc" -eq 1 ] && grep -q "missing: $I/harness.env" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
mv "$H/env.away" "$I/harness.env"
write_env "WORKTREE_ROOT="
"$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "check exits 1 when a required key is empty" '[ "$rc" -eq 1 ] && grep -q "WORKTREE_ROOT unset" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
write_env
"$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "check exits 0 on a complete instance" '[ "$rc" -eq 0 ] && grep -q "instance complete" "$H/check.txt" && grep -q "model lane:    off" "$H/check.txt" && grep -q "paused:        no" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"

# check names each way an instance is unusable, one line each.
check_with() {  # extra harness.env lines; leaves the output in $H/check.txt and the status in rc
    write_env "$@"; "$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
}
check_with "TARGET_REPO=$H"
check "check fails when TARGET_REPO is not a git checkout" '[ "$rc" -eq 1 ] && grep -q "missing: a git checkout at TARGET_REPO ($H)" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
: > "$H/plain-file"
check_with "WORKTREE_ROOT=$H/plain-file/trees"
check "check fails when WORKTREE_ROOT cannot be created" '[ "$rc" -eq 1 ] && grep -q "missing: a WORKTREE_ROOT that exists or can be created" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
check_with "WORKTREE_ROOT=$H/not-yet/trees"
check "a WORKTREE_ROOT that can be created passes" '[ "$rc" -eq 0 ] && [ ! -e "$H/not-yet" ]' "rc=$rc $(cat "$H/check.txt")"
check_with "QUEUE_DEPTH_CMD=\"$H/plain-file --flag\""
check "check fails when QUEUE_DEPTH_CMD is not executable" '[ "$rc" -eq 1 ] && grep -q "missing: an executable QUEUE_DEPTH_CMD ($H/plain-file)" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
check_with "CLAUDE_BIN=no-such-claude"
check "check fails when CLAUDE_BIN does not resolve" '[ "$rc" -eq 1 ] && grep -q "missing: an executable CLAUDE_BIN (no-such-claude)" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
check_with "GH_REPO="
check "check fails when GH_REPO is unset and the origin names none" '[ "$rc" -eq 1 ] && grep -q "GH_REPO unset" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
git -C "$H/target" remote add origin git@github.com:derived-org/derived-repo.git
check_with "GH_REPO="
check "an unset GH_REPO is derived from the origin remote" '[ "$rc" -eq 0 ] && grep -q "repository:    derived-org/derived-repo" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
launch
check "the derived GH_REPO reaches the prompt" 'sed -n 3p "$FAKE_PROMPT" | grep -q "^Core rules for derived-org/derived-repo:"' "$(cat "$FAKE_PROMPT" 2>/dev/null)"
derive() {  # origin URL, expected repository or "none"
    want_repo=$2; git -C "$H/target" remote set-url origin "$1"; check_with "GH_REPO="
    if [ "$2" = none ]; then
        check "no GH_REPO is derived from $1" '[ "$rc" -eq 1 ] && grep -q "set GH_REPO in harness.env" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
    else
        check "GH_REPO $2 is derived from $1" '[ "$rc" -eq 0 ] && grep -qx "repository:    $want_repo" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
    fi
}
derive git@github-work:alias-org/alias-repo.git alias-org/alias-repo
derive ssh://git@github-work/alias-org/alias-repo alias-org/alias-repo
derive git@no-such-alias:o/n.git none
derive file:///o/n none
derive https://github.com/https-org/https-repo.git https-org/https-repo
derive git@github.com:scp-org/scp-repo.git scp-org/scp-repo
derive https://ghe.example.com/ent-org/ent-repo ghe.example.com/ent-org/ent-repo
git -C "$H/target" remote remove origin
printf '#!/usr/bin/env bash\nexit 0\n' > "$I/hooks/worktree-busy"; chmod -x "$I/hooks/worktree-busy"
check_with
check "check reports a hook that is not executable as ignored" 'grep -q "hook worktree-busy: present but not executable (ignored)" "$H/check.txt" && grep -q "hook worktree-remove: none" "$H/check.txt"' "$(cat "$H/check.txt")"
rm -f "$I/hooks/worktree-busy"
touch "$H/state/paused"; check_with
check "check reports state/paused" 'grep -q "paused:        yes, $H/state/paused exists" "$H/check.txt"' "$(cat "$H/check.txt")"
rm -f "$H/state/paused"; touch "$FAKE_NO_CRON"; check_with
check "check reports a missing cron line as paused" 'grep -q "paused:        yes, no active crontab line" "$H/check.txt"' "$(cat "$H/check.txt")"
rm -f "$FAKE_NO_CRON"

# The template's placeholder paths and empty GH_REPO cannot pass.
T="$H/template-instance"; cp -r "$here/templates/instance" "$T"; mv "$T/harness.env.example" "$T/harness.env"
HARNESS_INSTANCE_DIR="$T" "$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "the template's placeholders fail check" \
    '[ "$rc" -eq 1 ] && grep -q "missing: a git checkout at TARGET_REPO (/path/to/checkout)" "$H/check.txt" && grep -q "missing: a WORKTREE_ROOT that exists or can be created (/path/to/worktrees)" "$H/check.txt" && grep -q "GH_REPO unset" "$H/check.txt"' \
    "rc=$rc $(cat "$H/check.txt")"
write_env

# The prompt: lane line, blank line, core rules, then the installation's own, with keys substituted.
cat > "$I/hooks/env.sh" <<'EOF'
export FROM_HOOK=sourced
EOF
launch
check "a complete instance starts a tick" '[ -s "$FAKE_PROMPT" ]' "log=$(cat "$LOG")"
check "the first line is the lane line, then a blank line" \
    '[ "$(sed -n 1p "$FAKE_PROMPT")" = "This tick runs on fake-model." ] && [ -z "$(sed -n 2p "$FAKE_PROMPT")" ]' "$(cat "$FAKE_PROMPT")"
check "the core rules follow, with keys substituted literally" \
    '[ "$(sed -n 3p "$FAKE_PROMPT")" = "Core rules for o/r&x|y\\z: budget 120 min, lane \`\`, {{UNKNOWN}} stays." ]' "$(sed -n 3p "$FAKE_PROMPT")"
check "the installation section comes last" \
    '[ "$(sed -n 5p "$FAKE_PROMPT")" = "## This installation" ] && tail -1 "$FAKE_PROMPT" | grep -qF "Fake installation rules for o/r&x|y\\z: work only issues labelled \`ai\`."' "$(cat "$FAKE_PROMPT")"
check "hooks/env.sh exports and TICK_EXPORTS reach the tick" '[ "$(cat "$FAKE_ENV")" = "sourced exported" ]' "$(cat "$FAKE_ENV" 2>/dev/null)"

# The lint's settings, GH_REPO and the labels reach every tick; GH_TOKEN never does.
write_env "PUBLIC_RECORD_RULES=$H/rules.json" "PUBLIC_RECORD_PR_SECTIONS='Summary,Why'" \
    "PUBLIC_RECORD_ISSUE_SECTIONS=Summary" "WIP_LABEL=bot:wip" "LOW_LABEL=bot:low" "GH_TOKEN=secret-token"
GH_TOKEN=env-token launch
check "the lint's settings and the labels reach the tick, GH_TOKEN does not" \
    '[ "$(cat "$FAKE_LINT_ENV")" = "$(printf "%s\n" "rules=$H/rules.json pr=Summary,Why" "issue=Summary repo=o/r&x|y\\z wip=bot:wip" "low=bot:low token=unset")" ]' \
    "$(cat "$FAKE_LINT_ENV" 2>/dev/null)"
check "the queue count gets the config's GH_TOKEN in its environment, never in its argv" \
    'grep -qx "token=secret-token" "$FAKE_QUEUE_SEEN" && grep -q "^argv=.*queue" "$FAKE_QUEUE_SEEN" && ! grep "^argv=" "$FAKE_QUEUE_SEEN" | grep -q secret-token' \
    "$(cat "$FAKE_QUEUE_SEEN" 2>/dev/null)"
write_env

# A failed queue count is an error in the log, and still counts one issue so a tick runs.
touch "$FAKE_QUEUE_FAILS"
launch
check "a failed queue count is logged as an error and a tick still runs" \
    'grep -q "ERROR queue count failed (label ai: exit 1, output .*, stderr .queue service unreachable.); counting 1 claimable issue" "$LOG" && [ -s "$FAKE_PROMPT" ]' "log=$(cat "$LOG")"
printf '#!/usr/bin/env bash\n' > "$H/fakebin/silent-queue"; chmod +x "$H/fakebin/silent-queue"
write_env "QUEUE_DEPTH_CMD=$H/fakebin/silent-queue"
launch
check "an empty queue count is logged as an error, not read as a count" \
    'grep -q "ERROR queue count failed (label ai: exit 0, output .., " "$LOG"' "log=$(cat "$LOG")"
rm -f "$FAKE_QUEUE_FAILS"
write_env

printf 'false\n' > "$I/hooks/env.sh"
launch
check "a failing env.sh is logged and the tick still runs" \
    'grep -q "hooks/env.sh failed (exit 1)" "$LOG" && [ "$(cat "$FAKE_ENV")" = "unset exported" ]' "log=$(cat "$LOG") env=$(cat "$FAKE_ENV" 2>/dev/null)"
rm -f "$I/hooks/env.sh"

# The model lane: off when empty, on with both keys set and a lane issue claimable.
write_env "LANE_MODEL=lane-model" "LANE_LABEL=ai:lane"; echo 1 > "$FAKE_LANE"
launch
check "a claimable lane issue runs on the lane model" \
    'head -1 "$FAKE_PROMPT" | grep -q "^This tick runs on lane-model, the model lane: claim only an issue labelled \`ai:lane\`" && grep -q -- "--model lane-model" "$FAKE_ARGS"' "$(head -1 "$FAKE_PROMPT" 2>/dev/null)"
echo 0 > "$FAKE_LANE"
launch
check "with no lane issue, the tick leaves lane issues alone" \
    'head -1 "$FAKE_PROMPT" | grep -q "^This tick runs on fake-model: never claim an issue labelled \`ai:lane\`"' "$(head -1 "$FAKE_PROMPT" 2>/dev/null)"
write_env

# hooks/hold: exit 0 holds and logs its reason once; exit 1 or a hang holds nothing.
printf '#!/usr/bin/env bash\necho "service down since noon"\necho second line\nexit 0\n' > "$I/hooks/hold"; chmod +x "$I/hooks/hold"
launch
"$H/bin/tick.sh"; sleep 1
check "a hold hook that exits 0 holds the harness" '[ ! -e "$FAKE_PROMPT" ]' "$(cat "$FAKE_PROMPT" 2>/dev/null)"
check "the hold reason is logged once" '[ "$(grep -c "held: service down since noon$" "$LOG")" -eq 1 ] && [ "$(wc -l < "$LOG")" -eq 1 ]' "$(cat "$LOG")"
"$H/bin/tick.sh" check >"$H/check.txt" 2>&1
check "check reports the hold" 'grep -q "held: service down since noon" "$H/check.txt"' "$(cat "$H/check.txt")"
printf '#!/usr/bin/env bash\necho "not today"\nexit 1\n' > "$I/hooks/hold"
launch
check "a hold hook that exits 1 holds nothing" '[ -s "$FAKE_PROMPT" ] && ! grep -q held "$LOG"' "log=$(cat "$LOG")"
printf '#!/usr/bin/env bash\necho "stuck"\nsleep 30\nexit 0\n' > "$I/hooks/hold"
write_env "HOLD_TIMEOUT_S=1"
start=$(date +%s); launch; sleep 2; took=$(( $(date +%s) - start ))
check "a hold hook that hangs past HOLD_TIMEOUT_S holds nothing" '[ -s "$FAKE_PROMPT" ] && ! grep -q held "$LOG" && [ "$took" -lt 10 ]' "took=${took}s log=$(cat "$LOG")"
rm -f "$I/hooks/hold"

# Through fleet: the runner dispatches with the prompt on stdin and reads the output as a local run's.
check "the default path runs claude, not fleet" '[ -s "$FAKE_PROMPT" ] && [ ! -e "$FAKE_FLEET_ARGV" ]' "prompt=$(cat "$FAKE_PROMPT" 2>/dev/null)"
fleet_env() { write_env "FLEET_DISPATCH=1" "FLEET_BIN=$H/fakebin/fleet" "LONG_TICK_BUDGET_MIN=90" "$@"; }
fleet_env
launch
check "FLEET_DISPATCH runs fleet dispatch with the argv the contract gives, and not claude" \
    '[ "$(cat "$FAKE_FLEET_ARGV")" = "dispatch --device auto --adapter claude --workdir $H/worktrees --model fake-model --wait --timeout 7200" ] && [ ! -e "$FAKE_PROMPT" ]' \
    "argv=$(cat "$FAKE_FLEET_ARGV" 2>/dev/null) log=$(cat "$LOG")"
check "the prompt goes on stdin, never into argv" \
    '[ "$(head -1 "$FAKE_FLEET_STDIN")" = "This tick runs on fake-model." ] && grep -q "^Core rules for" "$FAKE_FLEET_STDIN" && ! grep -q "Core rules" "$FAKE_FLEET_ARGV"' \
    "stdin=$(cat "$FAKE_FLEET_STDIN" 2>/dev/null)"
check "the dispatch creates the workdir it names" '[ -d "$H/worktrees" ]' ""
echo 7 > "$FAKE_FLEET_RC"
launch
tick_log=$(ls "$H/state/logs/"tick-*.log 2>/dev/null | head -1)
check "the dispatched agent's exit code and output reach the tick log" \
    'grep -q "^agent output on fake-model$" "$tick_log" && grep -q "^\[tick.sh\] exit=7 duration=.*model=fake-model$" "$tick_log"' \
    "$(cat "$tick_log" 2>/dev/null)"
rm -f "$FAKE_FLEET_RC"
fleet_env "TICK_MODELS='model-a model-b'"; echo model-a > "$FAKE_FLEET_LIMIT"
launch
tick_log=$(ls "$H/state/logs/"tick-*.log 2>/dev/null | head -1)
check "a usage limit in the dispatched output cools the model and the tick re-runs on the next" \
    '[ -e "$H/state/model-cooldown/model-a" ] && grep -q "model-a hit its usage limit" "$LOG" && [ "$(grep -o -- "--model [^ ]*" "$FAKE_FLEET_ARGV" | tr "\n" " ")" = "--model model-a --model model-b " ] && grep -q "^\[tick.sh\] exit=0 .*model=model-b$" "$tick_log"' \
    "argv=$(cat "$FAKE_FLEET_ARGV" 2>/dev/null) log=$(cat "$LOG")"
rm -rf "$FAKE_FLEET_LIMIT" "$H/state/model-cooldown"
fleet_env "TICK_AGENT=codex" "FLEET_DEVICE=dev-a"
launch
check "TICK_AGENT and FLEET_DEVICE reach the dispatch" 'grep -q -- "^dispatch --device dev-a --adapter codex " "$FAKE_FLEET_ARGV"' "$(cat "$FAKE_FLEET_ARGV" 2>/dev/null)"
fleet_env
"$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "check reports fleet's devices and passes with one that can take a tick" \
    '[ "$rc" -eq 0 ] && grep -q "^dispatch:      fleet: $H/fakebin/fleet dispatch --device auto" "$H/check.txt" && grep -qx "fleet device:  dev-a (tls) reachable, claude 1/2 running" "$H/check.txt" && grep -qx "fleet device:  dev-b (ssh) unreachable: unreachable: ssh exited 255" "$H/check.txt"' \
    "rc=$rc $(cat "$H/check.txt")"
touch "$FAKE_FLEET_DOWN"
"$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "check fails when fleet status fails" \
    '[ "$rc" -eq 1 ] && grep -q "gave no status document" "$H/check.txt" && grep -qx "missing: a fleet device that can take a claude tick" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
rm -f "$FAKE_FLEET_DOWN"
fleet_env "FLEET_DEVICE=dev-b"
"$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "check fails when FLEET_DEVICE names a device that cannot be reached" '[ "$rc" -eq 1 ]' "rc=$rc $(cat "$H/check.txt")"
fleet_env "FLEET_BIN=no-such-fleet"
"$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "check fails when FLEET_BIN does not resolve" '[ "$rc" -eq 1 ] && grep -q "missing: an executable FLEET_BIN (no-such-fleet)" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
write_env "TICK_AGENT=codex"
"$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "check fails when TICK_AGENT is not claude without fleet" \
    '[ "$rc" -eq 1 ] && grep -q "TICK_AGENT=codex needs FLEET_DISPATCH=1" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
rm -f "$H/state/locks/instance-incomplete.stamp"
launch
check "the launcher refuses TICK_AGENT=codex without fleet and logs why" \
    '[ ! -e "$FAKE_PROMPT" ] && [ ! -e "$FAKE_FLEET_ARGV" ] && grep -q "instance incomplete: .*TICK_AGENT=codex needs FLEET_DISPATCH=1" "$LOG"' "log=$(cat "$LOG")"
write_env

# HARNESS_INSTANCE_DIR moves the whole instance.
mv "$I" "$H/elsewhere"
HARNESS_INSTANCE_DIR="$H/elsewhere" "$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "HARNESS_INSTANCE_DIR names the instance" '[ "$rc" -eq 0 ] && grep -q "config:        $H/elsewhere/harness.env" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
mv "$H/elsewhere" "$I"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
