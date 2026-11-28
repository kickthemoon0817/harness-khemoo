#!/usr/bin/env bash
# Prints what a harness without accounts does, in a scratch home built from the harness checkout $1
# and run with $HARNESS_BIN: no AGENT_ACCOUNTS_DIR and no entry naming an account. A firing whose
# claude tick hits its usage limit and moves on to codex, then the chained firing; the variables,
# marker, slot record and usage check each tick ran with; what the fetch command ran with;
# `tick.sh check`, `harness usage` and `harness ticks`; the state files, the logs and the
# dashboard's and health snapshot's keys. Paths and times are taken out. test_accounts.sh compares
# it with off-state.txt, printed by the tree before accounts: with accounts off nothing may differ,
# so never regenerate it from this tree.
set -u
src=$1
H=$(mktemp -d)
trap 'rm -rf "$H"' EXIT
mkdir -p "$H/bin" "$H/lib" "$H/libexec" "$H/prompts" "$H/instance/prompts" "$H/state" "$H/target" "$H/fakebin" \
    "$H/seen" "$H/home/.codex/sessions/2026/11/28"
cp "$src/bin/tick.sh" "$H/bin/"; cp "$src/lib/instance.sh" "$H/lib/"; cp "$src"/libexec/*.sh "$H/libexec/"
echo "Core rules." > "$H/prompts/tick-core.md"
echo "Work the queue." > "$H/instance/prompts/tick.md"
# Each agent records the variables that pick an account, its marker, its slot record and its usage
# check; claude then ends on its usage-limit message while $H/limit exists.
cat > "$H/fakebin/agent" <<'FAKE'
#!/usr/bin/env bash
name=${0##*/}
cat >/dev/null
{
    echo "argv: $*"
    env | grep -E '^(AGENT_|USAGE_|CLAUDE_|CODEX_|GEMINI_|HARNESS_TICK=|TICK_)' | sed "s|$HARNESS_HOME|H|g" | LC_ALL=C sort
    cut -d' ' -f4- "$HARNESS_STATE/locks/slot1.tick"
    "$HARNESS_BIN" ticks | cut -d' ' -f3-
    "$HARNESS_HOME/libexec/usage-ok.sh" | sed "s|$HARNESS_HOME|H|g"
} > "$HARNESS_HOME/seen/$name"
if [ "$name" = claude ] && [ -e "$HARNESS_HOME/limit" ]; then
    rm -f "$HARNESS_HOME/limit"
    echo "You've hit your session limit · resets 5pm"
    exit 1
fi
"$HARNESS_HOME/libexec/claim.sh" acquire 7 >/dev/null
FAKE
chmod +x "$H/fakebin/agent"; ln -s agent "$H/fakebin/claude"; ln -s agent "$H/fakebin/codex"
cat > "$H/fakebin/fetch" <<'FAKE'
#!/usr/bin/env bash
env | grep -E '^(AGENT_|USAGE_|CLAUDE_|CODEX_|GEMINI_)' | sed "s|$HARNESS_HOME|H|g" | LC_ALL=C sort > "$HARNESS_HOME/seen/fetch"
FAKE
printf '#!/usr/bin/env bash\ncase "$ISSUE_LABEL" in ai) echo 1 ;; *) echo 0 ;; esac\n' > "$H/fakebin/queue"
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
TICK_MODELS="claude:model-a codex:model-x"
MAX_SLOTS=2
STAGGER_SECONDS=0
CLAIM_GRACE_S=0
USAGE_CACHE=$H/usage-cache.json
USAGE_FETCH=$H/fakebin/fetch
USAGE_REFRESH_AFTER=0
ENV
# shellcheck disable=SC2046 # one name a word
unset HARNESS_TICK FLEET_SESSION HARNESS_STATE GH_TOKEN SUPERVISOR CLAUDE_CONFIG_DIR CODEX_HOME GEMINI_CLI_HOME \
    AGENT_ACCOUNTS_DIR $(compgen -v USAGE_)
export HARNESS_HOME="$H" HARNESS_PATH="$H/fakebin:$PATH" PATH="$H/fakebin:$PATH" HOME="$H/home"
printf '{"timestamp": %s000, "data": {"fiveHourPercent": 10, "weeklyPercent": 20}}\n' "$(date +%s)" > "$H/usage-cache.json"
printf '{"timestamp":"2026-11-28T00:00:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","primary":{"used_percent":30,"window_minutes":10080,"resets_at":%s},"secondary":null}}}\n' \
    "$(( $(date +%s) + 3600 ))" > "$H/home/.codex/sessions/2026/11/28/rollout-a.jsonl"
settle() {
    local n=0 quiet=0
    while [ "$quiet" -lt 3 ] && [ "$n" -lt 400 ]; do
        if pgrep -f "$H/bin/tick.sh" >/dev/null; then quiet=0; else quiet=$((quiet + 1)); fi
        sleep 0.05; n=$((n + 1))
    done
}
norm() { sed -E -e "s|$H|H|g" -e 's/[0-9]{8}T[0-9]{6}Z/TS/g' -e 's/"(taken_ms|age_s|generated_epoch|resets_at|remaining_s)":[0-9.]+/"\1":N/g'; }
touch "$H/limit"
"$H/bin/tick.sh"; settle
for f in claude codex fetch; do echo "--- $f"; cat "$H/seen/$f" 2>/dev/null; done
echo "--- check"
"$H/bin/tick.sh" check 2>&1 | grep -v '^paused:' | norm
echo "--- usage"
(set -a; . "$H/instance/harness.env"; set +a; HARNESS_STATE="$H/state" "$HARNESS_BIN" usage --json) | norm
(set -a; . "$H/instance/harness.env"; set +a; HARNESS_STATE="$H/state" "$HARNESS_BIN" usage) | norm
echo "--- state"
(cd "$H" && find state -type f | norm | LC_ALL=C sort)
echo "--- logs"
norm < "$H/state/logs/allocator.log"
for f in "$H/state/logs"/tick-*.log; do grep '^\[tick.sh\]' "$f" | sed -E 's/duration=[0-9]+s/duration=Ns/'; done
echo "--- snapshots"
"$HARNESS_BIN" dashboard --once --harness-home "$H" --remote-interval 0 --claimable-interval 0 2>/dev/null \
    | jq -c 'keys, (.usage | del(.agents[]?.age_s, .agents[]?.taken_ms)), .harness.tick_models' | norm
"$HARNESS_BIN" health --json 2>/dev/null | jq -c 'keys, (.usage | del(.agents[]?.age_s, .agents[]?.taken_ms))' | norm
