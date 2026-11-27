#!/usr/bin/env bash
# Exercises bin/tick.sh against an instance overlay in a scratch harness home: the instance's
# required files, its hooks, the composed prompt and `tick.sh check`. A fake claude records its
# prompt and environment; a fake queue command answers per label.
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
printf '%s\n' "$*" >> "$FAKE_ARGS"
EOF
cat > "$H/fakebin/queue" <<'EOF'
#!/usr/bin/env bash
case "$ISSUE_LABEL" in ai:lane) cat "$FAKE_LANE" 2>/dev/null || echo 0 ;; ai:ready) echo 0 ;; *) echo 1 ;; esac
EOF
cat > "$H/fakebin/crontab" <<'EOF'
#!/usr/bin/env bash
echo "*/2 * * * * $HARNESS_HOME/bin/tick.sh"
EOF
chmod +x "$H/fakebin/"*
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
    FAKE_ARGS="$H/args.txt" FAKE_LANE="$H/lane.txt"
LOG="$H/state/logs/allocator.log"
printf '{"timestamp": %s000, "data": {"fiveHourPercent": 10, "weeklyPercent": 10}}\n' "$(date +%s)" > "$H/usage-cache.json"
pass=0; fail=0
check() { if eval "$2"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
launch() {  # run the launcher once and wait for the runner it may start
    rm -f "$FAKE_PROMPT" "$FAKE_ENV" "$FAKE_ARGS"; : > "$LOG"
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
check "check exits 0 on a complete instance" '[ "$rc" -eq 0 ] && grep -q "instance complete" "$H/check.txt" && grep -q "model lane:    off" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"

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

# HARNESS_INSTANCE_DIR moves the whole instance.
mv "$I" "$H/elsewhere"
HARNESS_INSTANCE_DIR="$H/elsewhere" "$H/bin/tick.sh" check >"$H/check.txt" 2>&1; rc=$?
check "HARNESS_INSTANCE_DIR names the instance" '[ "$rc" -eq 0 ] && grep -q "config:        $H/elsewhere/harness.env" "$H/check.txt"' "rc=$rc $(cat "$H/check.txt")"
mv "$H/elsewhere" "$I"

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
