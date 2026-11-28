#!/usr/bin/env bash
# Prints what a harness without accounts does across its other paths, in a scratch home built from
# the harness checkout $1 and run with $HARNESS_BIN: a supervisor tick, a lane tick that chains into
# a work tick on a bare model, and a landing tick, each one at a time. The lane's, the supervisor's
# and one listed model are bare models holding `@`, as a Vertex model id does, which no account names. For each it prints the
# variables that pick an account, the marker, the slot record and the usage check its agent ran
# with, and the lane line; then the supervisor's argv and environment, `tick.sh check`,
# `harness usage`, the state files and the logs. Paths and times are taken out. test_accounts.sh
# compares it with off-state-flow.txt, printed by main at 3236b8f, the tree before accounts: with
# accounts off nothing may differ, so never regenerate it from this tree.
set -u
src=$1
H=$(mktemp -d)
trap 'rm -rf "$H"' EXIT
mkdir -p "$H/bin" "$H/lib" "$H/libexec" "$H/prompts" "$H/instance/prompts" "$H/state" "$H/target" "$H/fakebin" \
    "$H/seen" "$H/home/.codex/sessions/2026/11/28"
cp "$src/bin/tick.sh" "$H/bin/"; cp "$src/lib/instance.sh" "$H/lib/"; cp "$src"/libexec/*.sh "$H/libexec/"
cp "$src/prompts/supervisor-core.md" "$src/prompts/public-record.md" "$H/prompts/"
echo "Core rules." > "$H/prompts/tick-core.md"
echo "Work the queue." > "$H/instance/prompts/tick.md"
# Each run records what picks its account, its marker and slot record, its usage check and its lane
# line, in the order the runs happen. The supervisor's environment holds nothing of the harness's,
# so its record's path comes from where this fake is.
cat > "$H/fakebin/agent" <<'FAKE'
#!/usr/bin/env bash
name=${0##*/}
prompt=$(cat)
home=${0%/fakebin/*}
case $prompt in
    "You are the supervisor"*)
        { echo "--- $name supervisor"; echo "argv: $*"
          env | grep -E '^(AGENT_|USAGE_|CLAUDE_|CODEX_|AGY_|ANTHROPIC_|OPENAI_|HARNESS_)' | LC_ALL=C sort
        } >> "$home/seen/runs"
        echo '{"directive": null, "github": []}'
        exit 0 ;;
esac
slot=${HARNESS_TICK#* slot=}; slot=${slot%% *}
{
    echo "--- $name"
    echo "argv: $*"
    echo "lane: ${prompt%%$'\n'*}"
    env | grep -E '^(AGENT_|USAGE_|CLAUDE_|CODEX_|AGY_|ANTHROPIC_|OPENAI_|HARNESS_TICK=|TICK_)' | sed "s|$HARNESS_HOME|H|g" | LC_ALL=C sort
    cut -d' ' -f4- "$HARNESS_STATE/locks/slot$slot.tick"
    "$HARNESS_HOME/libexec/usage-ok.sh" | sed "s|$HARNESS_HOME|H|g"
} >> "$HARNESS_HOME/seen/runs"
# The lane holds one issue until its tick has run.
rm -f "$HARNESS_HOME/lane-queued"
FAKE
chmod +x "$H/fakebin/agent"; ln -s agent "$H/fakebin/claude"; ln -s agent "$H/fakebin/codex"
cat > "$H/fakebin/queue" <<'FAKE'
#!/usr/bin/env bash
case "$ISSUE_LABEL" in
    ai) echo 2 ;;
    ai:lane) [ -e "$HARNESS_HOME/lane-queued" ] && echo 1 || echo 0 ;;
    ai:ready) echo 1 ;;
    *) echo 0 ;;
esac
FAKE
printf '#!/usr/bin/env bash\necho "*/2 * * * * $HARNESS_HOME/bin/tick.sh"\n' > "$H/fakebin/crontab"
printf '#!/usr/bin/env bash\nexit 1\n' > "$H/fakebin/gh"
chmod +x "$H/fakebin/"*
git -C "$H/target" init -q
cat > "$H/instance/harness.env" <<ENV
TARGET_REPO=$H/target
WORKTREE_ROOT=$H/worktrees
GH_REPO=example-org/example-repo
QUEUE_DEPTH_CMD=$H/fakebin/queue
AGENT_BIN_claude=$H/fakebin/claude
AGENT_BIN_codex=$H/fakebin/codex
AGENT_FLAGS_claude=
AGENT_FLAGS_codex=
TICK_AGENT=claude
TICK_MODELS="model-a claude-opus-4@20250514 codex:model-x"
LANE_MODEL=model-l@20250514
LANE_LABEL=ai:lane
MAX_SLOTS=1
STAGGER_SECONDS=0
CLAIM_GRACE_S=0
CHAIN_MIN_TICK_S=0
MAX_CHAIN=2
USAGE_CACHE=$H/usage-cache.json
USAGE_FETCH=$H/no-fetch
SUPERVISOR=1
SUPERVISOR_MODEL=model-s@20250514
SUPERVISOR_MIN_INTERVAL_S=86400
SUPERVISOR_INTERVAL_S=86400
ANTHROPIC_API_KEY=sk-ant-fixture
ENV
# Every inherited name under a prefix this prints goes, so a host's own (a CI runner's
# AGENT_TOOLSDIRECTORY) never reaches the output.
# shellcheck disable=SC2046 # one name a word
unset HARNESS_TICK FLEET_SESSION HARNESS_STATE GH_TOKEN SUPERVISOR \
    $(compgen -v AGENT_) $(compgen -v CLAUDE_) $(compgen -v CODEX_) $(compgen -v AGY_) $(compgen -v TICK_) \
    $(compgen -v USAGE_) $(compgen -v ANTHROPIC_) $(compgen -v OPENAI_) $(compgen -v SUPERVISOR_)
export HARNESS_HOME="$H" HARNESS_PATH="$H/fakebin:$PATH" PATH="$H/fakebin:$PATH" HOME="$H/home"
printf '{"timestamp": %s000, "data": {"fiveHourPercent": 10, "weeklyPercent": 20}}\n' "$(date +%s)" > "$H/usage-cache.json"
printf '{"timestamp":"2026-11-28T00:00:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","primary":{"used_percent":30,"window_minutes":10080,"resets_at":%s},"secondary":null}}}\n' \
    "$(( $(date +%s) + 3600 ))" > "$H/home/.codex/sessions/2026/11/28/rollout-a.jsonl"
settle() {
    local n=0 quiet=0
    while [ "$quiet" -lt 3 ] && [ "$n" -lt 600 ]; do
        if pgrep -f "$H/bin/tick.sh" >/dev/null; then quiet=0; else quiet=$((quiet + 1)); fi
        sleep 0.05; n=$((n + 1))
    done
}
norm() {
    sed -E -e "s|$H|H|g" -e 's/[0-9]{8}T[0-9]{6}Z/TS/g' -e 's/[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]{8}Z/ISO/g' \
        -e 's/"(taken_ms|age_s|generated_epoch|resets_at|remaining_s)":[0-9.]+/"\1":N/g' \
        -e 's/(duration=|after )[0-9]+s/\1Ns/g'
}
# The supervisor first, by hand; its interval then keeps the launcher's firing from starting another.
"$H/bin/tick.sh" supervisor manual
touch "$H/lane-queued"
"$H/bin/tick.sh"; settle
"$H/bin/tick.sh" runner 1 0 landing; settle
norm < "$H/seen/runs"
echo "--- check"
"$H/bin/tick.sh" check 2>&1 | grep -vE '^(paused|directive):' | norm
echo "--- usage"
(set -a; . "$H/instance/harness.env"; set +a; HARNESS_STATE="$H/state" "$HARNESS_BIN" usage --json) | norm
echo "--- state"
(cd "$H" && find state -type f | norm | LC_ALL=C sort -u)
echo "--- logs"
norm < "$H/state/logs/allocator.log"
for f in "$H/state/logs"/tick-*.log "$H/state/logs"/supervisor-*.log; do grep '^\[tick.sh\]' "$f" | norm; done
cut -d' ' -f2- "$H/state/supervisor/runs.log" | norm
